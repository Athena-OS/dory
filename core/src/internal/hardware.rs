use log::{info, warn};
use regex::Regex;
use shared::args::{is_arch, is_nix};
use shared::files;
use shared::exec::exec_output;
use shared::returncode_eval::{exec_eval_result, files_eval};
use std::fs;
use std::process::{Command, Output};
use std::thread::available_parallelism;

type Packages = Vec<&'static str>;
type Services = Vec<&'static str>;
type SetParams = Vec<(String, Vec<String>)>;

pub fn virt_check() -> (Packages, Services, SetParams) {
    let output_result = Command::new("systemd-detect-virt")
        .output(); // Directly call command
        // in baremetal, when no virtualization is detected, systemd-detect-virt returns exit status 1.
        // So we use directly Command::new to prevent it panics the application

    let output: Output = match output_result {
        Ok(out) => out,
        Err(e) => {
            panic!("Failed to execute systemd-detect-virt: {e}");
        }
    };

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Virtualization detected: {result}");

    let mut packages = Vec::new();
    let mut services = Vec::new();
    let mut set_params = Vec::new(); // To store the commands for file changes by sed

    match result.as_str() {
        "oracle" => {
            if is_arch() {
                packages.push("virtualbox-guest-utils");
                services.push("vboxservice");
            } else if is_nix() {
                files_eval(
                    files::sed_file(
                        "/mnt/etc/nixos/modules/hardware/virtualization/guest.nix",
                        "virtualbox.guest.enable =.*",
                        "virtualbox.guest.enable = lib.mkDefault true;",
                    ),
                    "Enable virtualbox guest additions",
                );
            }
        }

        "vmware" => {
            if is_arch() {
                packages.extend(["open-vm-tools"]);
                services.extend(["vmware-vmblock-fuse", "vmtoolsd"]);

                // Add the mkinitcpio MODULES edits for VMware on Arch
                set_params.push((
                    "Set vmware modules".to_string(),
                    vec![
                        "-i".into(),
                        "-e".into(),
                        r"/^MODULES=()/ s/()/(vsock vmw_vsock_vmci_transport vmw_balloon vmw_vmci vmwgfx)/".into(),
                        "-e".into(),
                        r"/^MODULES=([^)]*)/ { /vsock vmw_vsock_vmci_transport vmw_balloon vmw_vmci vmwgfx/! s/)/ vsock vmw_vsock_vmci_transport vmw_balloon vmw_vmci vmwgfx)/ }".into(),
                        "/mnt/etc/mkinitcpio.conf".into(),
                    ],
                ));
            } else if is_nix() {
                files_eval(
                    files::sed_file(
                        "/mnt/etc/nixos/modules/hardware/virtualization/guest.nix",
                        "vmware.guest.enable =.*",
                        "vmware.guest.enable = lib.mkDefault true;",
                    ),
                    "Enable vmware guest additions",
                );                
            }
        }

        "qemu" | "kvm" => {
            if !is_nix() {
                packages.extend(["qemu-guest-agent", "spice-vdagent"]);
                services.push("qemu-guest-agent");
            } else {
                files_eval(
                    files::sed_file(
                        "/mnt/etc/nixos/modules/hardware/virtualization/guest.nix",
                        "spice-vdagentd.enable =.*",
                        "spice-vdagentd.enable = lib.mkDefault true;",
                    ),
                    "Enable spice vdagent",
                );
                files_eval(
                    files::sed_file(
                        "/mnt/etc/nixos/modules/hardware/virtualization/guest.nix",
                        "qemuGuest.enable =.*",
                        "qemuGuest.enable = lib.mkDefault true;",
                    ),
                    "Enable qemu guest additions",
                );                
            }
        }

        "microsoft" => {
            if !is_nix() {
                packages.extend(["hyperv", "xf86-video-fbdev"]);
                
                let unit_dir = "/mnt/etc/systemd/system";
                let unit_path = "/mnt/etc/systemd/system/hv_fcopy_uio_daemon.service";
                let unit_contents = r#"[Unit]
Description=Hyper-V file copy service (uio_hv_generic)
ConditionPathExists=/dev/vmbus/uio_hv_generic
                    
[Service]
ExecStart=/usr/bin/hv_fcopy_uio_daemon -n
                    
[Install]
WantedBy=multi-user.target
"#;
                    
                // Ensure the target directory exists, then write the unit
                fs::create_dir_all(unit_dir)
                    .unwrap_or_else(|e| panic!("Failed to create {unit_dir}: {e}"));
                fs::write(unit_path, unit_contents)
                    .unwrap_or_else(|e| panic!("Failed to write {unit_path}: {e}"));

                services.extend(["hv_fcopy_uio_daemon", "hv_kvp_daemon", "hv_vss_daemon"]);
            }
            else {
                files_eval(
                    files::sed_file(
                        "/mnt/etc/nixos/modules/hardware/virtualization/guest.nix",
                        "hypervGuest.enable =.*",
                        "hypervGuest.enable = lib.mkDefault true;",
                    ),
                    "Enable kvm guest additions",
                );                
            }
        }

        "none" => info!("Running on bare metal."),
        _ => info!("Unknown virtualization type: {result}"),
    }

    (packages, services, set_params) // Return packages, services and params
}

pub fn set_cores() {
    let default_parallelism_approx = available_parallelism().unwrap().get();
    info!("The system has {default_parallelism_approx} cores");
    if default_parallelism_approx > 1 {
        files_eval(
            files::sed_file(
                "/mnt/etc/makepkg.conf",
                "#MAKEFLAGS=.*",
                &(format!("MAKEFLAGS=\"-j{default_parallelism_approx}\"")),
            ),
            "Set available cores on MAKEFLAGS",
        );
        files_eval(
            files::sed_file(
                "/mnt/etc/makepkg.conf",
                "#BUILDDIR=.*",
                "BUILDDIR=/tmp/makepkg",
            ),
            "Improving compilation times",
        );
        files_eval(
            files::sed_file(
                "/mnt/etc/makepkg.conf",
                "COMPRESSXZ=\\(xz -c -z -\\)",
                "COMPRESSXZ=(xz -c -z - --threads=0)",
            ),
            "Changing the compression settings",
        );
        files_eval(
            files::sed_file(
                "/mnt/etc/makepkg.conf",
                "COMPRESSZST=\\(zstd -c -z -q -\\)",
                "COMPRESSZST=(zstd -c -z -q - --threads=0)",
            ),
            "Changing the compression settings",
        );
        files_eval(
            files::sed_file(
                "/mnt/etc/makepkg.conf",
                "PKGEXT='.pkg.tar.xz'",
                "PKGEXT='.pkg.tar.zst'",
            ),
            "Changing the compression settings",
        );
    }
}

pub fn cpu_check() -> Vec<&'static str> {
    let mut packages: Vec<&'static str> = Vec::new();
    // -------- CPU --------
    let cpu = cpu_detect();
    if cpu.contains("Intel") {
        info!("Intel CPU detected.");
        if is_arch() {
            packages.push("intel-ucode");
            packages.push("intel-compute-runtime");
        } else if is_nix() {
            files_eval(
                files::sed_file(
                    "/mnt/etc/nixos/modules/hardware/default.nix",
                    "cpu.intel.updateMicrocode =.*",
                    "cpu.intel.updateMicrocode = true;",
                ),
                "Enable Intel ucode",
            );            
        }
    } else if cpu.contains("AMD") {
        info!("AMD CPU detected.");
        if is_arch() {
            packages.push("amd-ucode");
        } else if is_nix() {
        info!("AMD CPU detected.");
            files_eval(
                files::sed_file(
                    "/mnt/etc/nixos/modules/hardware/default.nix",
                    "cpu.intel.updateMicrocode =.*",
                    "cpu.amd.updateMicrocode = true;",
                ),
                "Enable AMD ucode",
            );            
        }
    }
    packages
}

/// Vendor of a PCI display device, as far as we care for graphics configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Intel,
    Amd,
    Other,
}

/// A detected display-class PCI device together with its X.org/NixOS-style bus ID.
#[derive(Debug, Clone)]
struct GpuDevice {
    vendor: GpuVendor,
    /// Bus ID in the form expected by `hardware.nvidia.prime.*BusId`,
    /// e.g. `PCI:1@0:0:0` (bus@domain:device:function, all in decimal).
    bus_id: String,
    /// The descriptive part of the `lspci` line (everything after the PCI
    /// address), e.g. `3D controller [0302]: NVIDIA Corporation TU106M ...`.
    /// Kept so callers can match on chip codenames (TU106, GM204, ...).
    description: String,
}

/// Convert a PCI address as printed by `lspci -D` (hex `domain:bus:device.function`,
/// e.g. `0000:01:00.0`) into the `PCI:bus@domain:device:function` form (decimal)
/// used by the NixOS `hardware.nvidia.prime` bus-ID options.
fn pci_addr_to_busid(addr: &str) -> Option<String> {
    // Split off the function (after the dot).
    let (head, func_hex) = addr.rsplit_once('.')?;
    let segments: Vec<&str> = head.split(':').collect();

    // With `-D` we get domain:bus:device; without it, bus:device.
    let (domain_hex, bus_hex, device_hex) = match segments.as_slice() {
        [domain, bus, device] => (*domain, *bus, *device),
        [bus, device] => ("0", *bus, *device),
        _ => return None,
    };

    let domain = u32::from_str_radix(domain_hex.trim(), 16).ok()?;
    let bus = u32::from_str_radix(bus_hex.trim(), 16).ok()?;
    let device = u32::from_str_radix(device_hex.trim(), 16).ok()?;
    let function = u32::from_str_radix(func_hex.trim(), 16).ok()?;

    Some(format!("PCI:{bus}@{domain}:{device}:{function}"))
}

/// Detect every display-class PCI device on the system using `lspci -D -nn`,
/// returning each one's vendor and computed bus ID.
fn detect_gpus() -> Vec<GpuDevice> {
    let output = match exec_output("lspci", vec![String::from("-D"), String::from("-nn")]) {
        Ok(out) => out,
        Err(e) => {
            // Graphics detection is best-effort: never abort the install over it.
            warn!("Could not run `lspci -D -nn` to detect GPUs: {e}");
            return Vec::new();
        }
    };

    let listing = String::from_utf8_lossy(&output.stdout);

    // First single 4-hex-digit bracket on a line is the PCI class code, e.g. `[0300]`.
    // Vendor:device pairs look like `[10de:1f95]` and are skipped by this pattern.
    let class_re = Regex::new(r"\[([0-9a-fA-F]{4})\]").unwrap();
    // The vendor ID is the first half of a `[vendor:device]` pair.
    let vendor_re = Regex::new(r"\[([0-9a-fA-F]{4}):[0-9a-fA-F]{4}\]").unwrap();

    let mut gpus = Vec::new();

    for line in listing.lines() {
        let mut parts = line.splitn(2, char::is_whitespace);
        let addr = match parts.next() {
            Some(a) if !a.is_empty() => a,
            _ => continue,
        };
        let rest = parts.next().unwrap_or("");

        // Keep only display controllers (PCI base class 0x03: VGA/3D/Display).
        let class = match class_re.captures(rest) {
            Some(c) => c[1].to_string(),
            None => continue,
        };
        if !class.starts_with("03") {
            continue;
        }

        let vendor = match vendor_re.captures(rest) {
            Some(v) => match v[1].to_lowercase().as_str() {
                "10de" => GpuVendor::Nvidia,
                "8086" => GpuVendor::Intel,
                "1002" | "1022" => GpuVendor::Amd,
                _ => GpuVendor::Other,
            },
            None => GpuVendor::Other,
        };

        let bus_id = match pci_addr_to_busid(addr) {
            Some(id) => id,
            None => {
                warn!("Could not parse PCI bus ID from `{addr}`");
                continue;
            }
        };

        info!("Detected display device {addr} -> {vendor:?} (busID {bus_id})");
        gpus.push(GpuDevice {
            vendor,
            bus_id,
            description: rest.to_string(),
        });
    }

    gpus
}

pub fn gpu_check_nix() {
    let gpus = detect_gpus();

    let nvidia = gpus.iter().find(|g| g.vendor == GpuVendor::Nvidia);

    // NVIDIA
    if let Some(nvidia) = nvidia {
        info!("NVIDIA GPU detected.");
        if is_nix() {
            let graphics_nix = "/mnt/etc/nixos/modules/hardware/graphics/default.nix";

            files_eval(
                files::sed_file(
                    graphics_nix,
                    "modesetting.enable =.*",
                    "modesetting.enable = true;",
                ),
                "Enable NVIDIA modesetting",
            );
            files_eval(
                files::sed_file(
                    graphics_nix,
                    "powerManagement.enable =.*",
                    "powerManagement.enable = true;",
                ),
                "Enable NVIDIA power management",
            );
            // Use the open-source NVIDIA kernel modules.
            files_eval(
                files::sed_file(
                    graphics_nix,
                    r"\bopen\s*=.*",
                    "open = true;",
                ),
                "Enable NVIDIA open kernel modules",
            );
            files_eval(
                files::sed_file(
                    graphics_nix,
                    "nvidiaSettings =.*",
                    "nvidiaSettings = true;",
                ),
                "Enable nvidia-settings menu",
            );
            files_eval(
                files::sed_file(
                    graphics_nix,
                    r"#\s*package = config\.boot\.kernelPackages\.nvidiaPackages\.stable;",
                    "package = config.boot.kernelPackages.nvidiaPackages.stable;",
                ),
                "Uncomment NVIDIA driver package",
            );
            files_eval(
                files::sed_file(
                    graphics_nix,
                    r#"#\s*services\.xserver\.videoDrivers = \[ "modesetting" "nvidia" \];"#,
                    r#"services.xserver.videoDrivers = [ "modesetting" "nvidia" ];"#,
                ),
                "Uncomment NVIDIA video drivers",
            );

            // ---- Hybrid GPU (PRIME) handling ----
            // If an integrated GPU (Intel or AMD) sits alongside the NVIDIA dGPU,
            // enable PRIME sync and wire up the computed bus IDs.
            let igpu = gpus
                .iter()
                .find(|g| matches!(g.vendor, GpuVendor::Intel | GpuVendor::Amd));

            if let Some(igpu) = igpu {
                info!(
                    "Hybrid GPU setup detected (NVIDIA dGPU + {:?} iGPU). Configuring PRIME.",
                    igpu.vendor
                );

                files_eval(
                    files::sed_file(
                        graphics_nix,
                        r"sync\.enable\s*=.*",
                        "sync.enable = true;",
                    ),
                    "Enable NVIDIA PRIME sync",
                );

                // Compute and assign the NVIDIA bus ID.
                files_eval(
                    files::sed_file(
                        graphics_nix,
                        r"nvidiaBusId\s*=.*",
                        &format!("nvidiaBusId = \"{}\";", nvidia.bus_id),
                    ),
                    "Set NVIDIA PRIME bus ID",
                );

                // Uncomment and assign the integrated GPU's bus ID.
                match igpu.vendor {
                    GpuVendor::Intel => {
                        files_eval(
                            files::sed_file(
                                graphics_nix,
                                r"#\s*intelBusId\s*=.*",
                                &format!("intelBusId = \"{}\";", igpu.bus_id),
                            ),
                            "Set Intel PRIME bus ID",
                        );
                    }
                    GpuVendor::Amd => {
                        files_eval(
                            files::sed_file(
                                graphics_nix,
                                r"#\s*amdgpuBusId\s*=.*",
                                &format!("amdgpuBusId = \"{}\";", igpu.bus_id),
                            ),
                            "Set AMD PRIME bus ID",
                        );
                    }
                    _ => {}
                }
            } else {
                info!("Single NVIDIA GPU detected; skipping PRIME/hybrid configuration.");
            }
        }
    }
}

pub fn gpu_check(kernel: &str) -> Vec<&'static str> {
    let mut packages: Vec<&'static str> = Vec::new();

    // -------- GPU --------
    // Reuse the shared display-device detector: it only considers PCI
    // display-class devices, so substring checks below can't be fooled by
    // non-GPU hardware (e.g. an Intel NIC or an AMD-CPU host bridge).
    let gpus = detect_gpus();

    // AMD
    if gpus
        .iter()
        .any(|g| g.vendor == GpuVendor::Amd && g.description.contains("AMD"))
    {
        info!("AMD GPU detected.");
        packages.extend(["xf86-video-amdgpu", "opencl-amd"]);
    }

    // ATI (legacy, not reporting AMD)
    if gpus
        .iter()
        .any(|g| g.description.contains("ATI") && !g.description.contains("AMD"))
    {
        info!("ATI GPU detected.");
        packages.push("opencl-mesa");
    }

    // NVIDIA
    if gpus.iter().any(|g| g.vendor == GpuVendor::Nvidia) {
        info!("NVIDIA GPU detected.");

        // Combined descriptions of every NVIDIA display device, used for
        // chip-family (codename) matching.
        let gpudetect: String = gpus
            .iter()
            .filter(|g| g.vendor == GpuVendor::Nvidia)
            .map(|g| g.description.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        // Family-specific handling
        let mut matched_family = false;

        if gpudetect.contains("GM107") || gpudetect.contains("GM108") || gpudetect.contains("GM200")
            || gpudetect.contains("GM204") || gpudetect.contains("GM206") || gpudetect.contains("GM20B")
        {
            info!("NV110 family (Maxwell)");
            matched_family = true;
            match kernel {
                "linux" => packages.push("nvidia-open"),
                "linux-lts" => packages.push("nvidia-open-lts"),
                _ => packages.push("nvidia-open-dkms"),
            }
            packages.push("nvidia-settings");
        }

        if gpudetect.contains("TU102") || gpudetect.contains("TU104") || gpudetect.contains("TU106")
            || gpudetect.contains("TU116") || gpudetect.contains("TU117")
        {
            info!("NV160 family (Turing)");
            matched_family = true;
            match kernel {
                "linux" => packages.push("nvidia-open"),
                "linux-lts" => packages.push("nvidia-open-lts"),
                _ => packages.push("nvidia-open-dkms"),
            }
            packages.push("nvidia-settings");
        }

        if gpudetect.contains("GK104") || gpudetect.contains("GK107") || gpudetect.contains("GK106")
            || gpudetect.contains("GK110") || gpudetect.contains("GK110B") || gpudetect.contains("GK208B")
            || gpudetect.contains("GK208") || gpudetect.contains("GK20A") || gpudetect.contains("GK210")
        {
            info!("NVE0 family (Kepler)");
            matched_family = true;
            packages.extend(["nvidia-470xx-dkms", "nvidia-470xx-settings"]);
        }

        if gpudetect.contains("GF100") || gpudetect.contains("GF108") || gpudetect.contains("GF106")
            || gpudetect.contains("GF104") || gpudetect.contains("GF110") || gpudetect.contains("GF114")
            || gpudetect.contains("GF116") || gpudetect.contains("GF117") || gpudetect.contains("GF119")
        {
            info!("NVC0 family (Fermi)");
            matched_family = true;
            packages.extend(["nvidia-390xx-dkms", "nvidia-390xx-settings"]);
        }

        if gpudetect.contains("G80") || gpudetect.contains("G84") || gpudetect.contains("G86")
            || gpudetect.contains("G92") || gpudetect.contains("G94") || gpudetect.contains("G96")
            || gpudetect.contains("G98") || gpudetect.contains("GT200") || gpudetect.contains("GT215")
            || gpudetect.contains("GT216") || gpudetect.contains("GT218") || gpudetect.contains("MCP77")
            || gpudetect.contains("MCP78") || gpudetect.contains("MCP79") || gpudetect.contains("MCP7A")
            || gpudetect.contains("MCP89")
        {
            info!("NV50 family (Tesla)");
            matched_family = true;
            match kernel {
                "linux" => packages.push("nvidia-340xx"),
                "linux-lts" => packages.push("nvidia-340xx-lts"),
                _ => packages.push("nvidia-340xx-lts-dkms"),
            }
            packages.push("nvidia-340xx-settings");
        }

        if !matched_family {
            packages.extend(["nvidia-open-dkms", "nvidia-settings"]);
        }

        // Common extras on Arch
        packages.extend(["opencl-nvidia", "gwe", "nvtop"]);

        // Hybrid GPU setup? add envycontrol. Only a *display* device from
        // another vendor counts, so a discrete-only NVIDIA box with an Intel
        // NIC is not misdetected as hybrid.
        if gpus
            .iter()
            .any(|g| matches!(g.vendor, GpuVendor::Intel | GpuVendor::Amd))
        {
            packages.push("envycontrol");
        }
    }
    packages
}

pub fn cpu_detect() -> String {
    let lscpu_output = exec_eval_result(
        exec_output(
            "lscpu",
            vec![]
        ),
        "Detect the CPU",
    );

    let lscpu_str = std::str::from_utf8(&lscpu_output.stdout)
        .expect("Failed to parse lscpu output as UTF-8");

    let vendor_id_line = lscpu_str
        .lines()
        .find(|line| line.starts_with("Vendor ID:"))
        .expect("Vendor ID not found in lscpu output");

    let vendor_id = vendor_id_line
        .split(':')
        .nth(1)
        .expect("Invalid format for Vendor ID in lscpu output")
        .trim();

    vendor_id.to_string()
}

pub fn is_hyperv_guest() -> bool {
    let candidates = [
        "/sys/devices/virtual/dmi/id/sys_vendor",
        "/sys/devices/virtual/dmi/id/product_name",
        "/sys/class/dmi/id/sys_vendor",
        "/sys/class/dmi/id/product_name",
    ];

    for path in candidates {
        if let Ok(data) = fs::read_to_string(path) {
            let d = data.to_lowercase();
            // loose match: "microsoft" is enough
            if d.contains("microsoft") || d.contains("hyper-v") || d.contains("hyperv") {
                return true;
            }
        }
    }

    false
}
