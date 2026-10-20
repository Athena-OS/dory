use crate::internal::kernels::KERNELS;
use log::{info, warn};
use regex::Regex;
use shared::args::{is_arch, is_nix};
use shared::exec::exec_output;
use shared::files;
use shared::returncode_eval::files_eval;
use std::fs;
use std::process::Command;
use std::sync::{LazyLock, OnceLock};
use std::thread::available_parallelism;

type Packages = Vec<&'static str>;
type Services = Vec<&'static str>;

const NIX_GUEST: &str = "/mnt/etc/nixos/modules/hardware/virtualization/guest.nix";
const NIX_HARDWARE: &str = "/mnt/etc/nixos/modules/hardware/default.nix";
const MKINITCPIO_CONF: &str = "/mnt/etc/mkinitcpio.conf";

/// Kernel modules to force into the initramfs for VMware guests.
pub const VMWARE_MODULES: &[&str] = &["vsock", "vmw_vsock_vmci_transport", "vmw_balloon", "vmw_vmci", "vmwgfx"];
/// Kernel modules to force into the initramfs on Apple hardware.
pub const APPLE_MODULES: &[&str] = &[
    "usb_storage", "uas", "xhci_hcd", "ahci", "libahci", "sd_mod", "usbhid", "hid_apple",
    "xhci_pci", "ehci_pci", "ohci_hcd", "uhci_hcd",
];

/// Add `modules` to the `MODULES=(...)` line of the target mkinitcpio.conf,
/// skipping any that are already present. Replaces the former external `sed`.
pub fn add_mkinitcpio_modules(modules: &[&str]) -> std::io::Result<()> {
    static MODULES_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^MODULES=\(([^)]*)\)").expect("valid regex"));
    let contents = fs::read_to_string(MKINITCPIO_CONF)?;
    let Some(caps) = MODULES_RE.captures(&contents) else {
        return Err(std::io::Error::other("MODULES=(...) line not found in mkinitcpio.conf"));
    };
    let existing: Vec<&str> = caps[1].split_whitespace().collect();
    let mut merged: Vec<&str> = existing.clone();
    merged.extend(modules.iter().copied().filter(|m| !existing.contains(m)));
    if merged.len() == existing.len() {
        return Ok(());
    }
    let new_line = format!("MODULES=({})", merged.join(" "));
    let updated = MODULES_RE.replace(&contents, new_line.as_str());
    info!("Set mkinitcpio {new_line}");
    fs::write(MKINITCPIO_CONF, updated.as_bytes())
}

fn nix_enable(what: &str, key: &str) {
    files_eval(
        files::sed_file(NIX_GUEST, &format!("{key} =.*"), &format!("{key} = lib.mkDefault true;")),
        &format!("Enable {what}"),
    );
}

/// Detect the hypervisor and return (packages, services, extra initramfs modules).
pub fn virt_check() -> (Packages, Services, Vec<&'static [&'static str]>) {
    // In bare metal systemd-detect-virt returns exit status 1 ("none"), so we
    // do not treat a non-zero exit as an error.
    let output = Command::new("systemd-detect-virt")
        .output()
        .unwrap_or_else(|e| panic!("Failed to execute systemd-detect-virt: {e}"));
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Virtualization detected: {result}");

    let mut packages = Vec::new();
    let mut services = Vec::new();
    let mut modules: Vec<&'static [&'static str]> = Vec::new();

    match result.as_str() {
        "oracle" => {
            if is_arch() {
                packages.push("virtualbox-guest-utils");
                services.push("vboxservice");
            } else if is_nix() {
                nix_enable("virtualbox guest additions", "virtualbox.guest.enable");
            }
        }
        "vmware" => {
            if is_arch() {
                packages.push("open-vm-tools");
                services.extend(["vmware-vmblock-fuse", "vmtoolsd"]);
                modules.push(VMWARE_MODULES);
            } else if is_nix() {
                nix_enable("vmware guest additions", "vmware.guest.enable");
            }
        }
        "qemu" | "kvm" => {
            if !is_nix() {
                packages.extend(["qemu-guest-agent", "spice-vdagent"]);
                services.push("qemu-guest-agent");
            } else {
                nix_enable("spice vdagent", "spice-vdagentd.enable");
                nix_enable("qemu guest additions", "qemuGuest.enable");
            }
        }
        "microsoft" => {
            if !is_nix() {
                packages.extend(["hyperv", "xf86-video-fbdev"]);

                let unit_dir = "/mnt/etc/systemd/system";
                let unit_path = "/mnt/etc/systemd/system/hv_fcopy_uio_daemon.service";
                let unit_contents = "[Unit]\n\
Description=Hyper-V file copy service (uio_hv_generic)\n\
ConditionPathExists=/dev/vmbus/uio_hv_generic\n\
\n\
[Service]\n\
ExecStart=/usr/bin/hv_fcopy_uio_daemon -n\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n";
                fs::create_dir_all(unit_dir).unwrap_or_else(|e| panic!("Failed to create {unit_dir}: {e}"));
                fs::write(unit_path, unit_contents).unwrap_or_else(|e| panic!("Failed to write {unit_path}: {e}"));

                services.extend(["hv_fcopy_uio_daemon", "hv_kvp_daemon", "hv_vss_daemon"]);
            } else {
                nix_enable("hyperv guest additions", "hypervGuest.enable");
            }
        }
        "none" => info!("Running on bare metal."),
        _ => info!("Unknown virtualization type: {result}"),
    }

    (packages, services, modules)
}

pub fn set_cores() {
    let cores = available_parallelism().map(|n| n.get()).unwrap_or(1);
    info!("The system has {cores} cores");
    if cores <= 1 {
        return;
    }
    let makepkg = "/mnt/etc/makepkg.conf";
    let edits: [(&str, String, &str); 5] = [
        ("#MAKEFLAGS=.*", format!("MAKEFLAGS=\"-j{cores}\""), "Set available cores on MAKEFLAGS"),
        ("#BUILDDIR=.*", "BUILDDIR=/tmp/makepkg".into(), "Improving compilation times"),
        ("COMPRESSXZ=\\(xz -c -z -\\)", "COMPRESSXZ=(xz -c -z - --threads=0)".into(), "Changing xz compression settings"),
        ("COMPRESSZST=\\(zstd -c -z -q -\\)", "COMPRESSZST=(zstd -c -z -q - --threads=0)".into(), "Changing zstd compression settings"),
        ("PKGEXT='.pkg.tar.xz'", "PKGEXT='.pkg.tar.zst'".into(), "Changing package extension"),
    ];
    for (find, replace, what) in edits {
        files_eval(files::sed_file(makepkg, find, &replace), what);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuVendor {
    Intel,
    Amd,
    Other,
}

impl CpuVendor {
    /// Microcode initrd shipped by the matching *-ucode package, if any.
    pub fn ucode_initrd(self) -> Option<&'static str> {
        match self {
            CpuVendor::Intel => Some("/boot/intel-ucode.img"),
            CpuVendor::Amd => Some("/boot/amd-ucode.img"),
            CpuVendor::Other => None,
        }
    }
}

/// CPU vendor, read once from /proc/cpuinfo (no process spawn) and cached.
pub fn cpu_vendor() -> CpuVendor {
    static VENDOR: OnceLock<CpuVendor> = OnceLock::new();
    *VENDOR.get_or_init(|| {
        let vendor_id = fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("vendor_id"))
                    .and_then(|l| l.split(':').nth(1))
                    .map(|v| v.trim().to_string())
            })
            .unwrap_or_default();
        let v = match vendor_id.as_str() {
            "GenuineIntel" => CpuVendor::Intel,
            "AuthenticAMD" | "HygonGenuine" => CpuVendor::Amd,
            _ => CpuVendor::Other,
        };
        info!("CPU vendor: {vendor_id} -> {v:?}");
        v
    })
}

pub fn cpu_check() -> Vec<&'static str> {
    let mut packages: Vec<&'static str> = Vec::new();
    match cpu_vendor() {
        CpuVendor::Intel => {
            if is_arch() {
                packages.extend(["intel-ucode", "intel-compute-runtime"]);
            } else if is_nix() {
                files_eval(
                    files::sed_file(NIX_HARDWARE, "cpu.intel.updateMicrocode =.*", "cpu.intel.updateMicrocode = true;"),
                    "Enable Intel ucode",
                );
            }
        }
        CpuVendor::Amd => {
            if is_arch() {
                packages.push("amd-ucode");
            } else if is_nix() {
                files_eval(
                    files::sed_file(NIX_HARDWARE, "cpu.amd.updateMicrocode =.*", "cpu.amd.updateMicrocode = true;"),
                    "Enable AMD ucode",
                );
            }
        }
        CpuVendor::Other => info!("Unknown CPU vendor; no microcode package selected."),
    }
    packages
}

/// Vendor of a PCI display device, as far as we care for graphics configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Intel,
    Amd,
    Other,
}

/// A detected display-class PCI device together with its X.org/NixOS-style bus ID.
#[derive(Debug, Clone)]
pub struct GpuDevice {
    pub vendor: GpuVendor,
    /// Bus ID in the form expected by `hardware.nvidia.prime.*BusId`,
    /// e.g. `PCI:1@0:0:0` (bus@domain:device:function, all in decimal).
    pub bus_id: String,
    /// The descriptive part of the `lspci` line (everything after the PCI
    /// address). Kept so callers can match on chip codenames (TU106, GM204, ...).
    pub description: String,
}

/// Convert a PCI address as printed by `lspci -D` (hex `domain:bus:device.function`)
/// into the `PCI:bus@domain:device:function` form (decimal) used by NixOS.
fn pci_addr_to_busid(addr: &str) -> Option<String> {
    let (head, func_hex) = addr.rsplit_once('.')?;
    let segments: Vec<&str> = head.split(':').collect();
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

fn parse_lspci(listing: &str) -> Vec<GpuDevice> {
    // First single 4-hex-digit bracket on a line is the PCI class code, e.g. `[0300]`.
    static CLASS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([0-9a-fA-F]{4})\]").expect("regex"));
    // The vendor ID is the first half of a `[vendor:device]` pair.
    static VENDOR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[([0-9a-fA-F]{4}):[0-9a-fA-F]{4}\]").expect("regex"));

    let mut gpus = Vec::new();
    for line in listing.lines() {
        let mut parts = line.splitn(2, char::is_whitespace);
        let addr = match parts.next() {
            Some(a) if !a.is_empty() => a,
            _ => continue,
        };
        let rest = parts.next().unwrap_or("");

        // Keep only display controllers (PCI base class 0x03: VGA/3D/Display).
        let Some(class) = CLASS_RE.captures(rest) else { continue };
        if !class[1].starts_with("03") {
            continue;
        }

        let vendor = match VENDOR_RE.captures(rest) {
            Some(v) => match v[1].to_lowercase().as_str() {
                "10de" => GpuVendor::Nvidia,
                "8086" => GpuVendor::Intel,
                "1002" | "1022" => GpuVendor::Amd,
                _ => GpuVendor::Other,
            },
            None => GpuVendor::Other,
        };

        let Some(bus_id) = pci_addr_to_busid(addr) else {
            warn!("Could not parse PCI bus ID from `{addr}`");
            continue;
        };

        info!("Detected display device {addr} -> {vendor:?} (busID {bus_id})");
        gpus.push(GpuDevice { vendor, bus_id, description: rest.to_string() });
    }
    gpus
}

/// Detect every display-class PCI device using `lspci -D -nn`. Runs once; cached.
pub fn detect_gpus() -> &'static [GpuDevice] {
    static GPUS: OnceLock<Vec<GpuDevice>> = OnceLock::new();
    GPUS.get_or_init(|| match exec_output("lspci", ["-D", "-nn"]) {
        Ok(out) => parse_lspci(&String::from_utf8_lossy(&out.stdout)),
        Err(e) => {
            // Graphics detection is best-effort: never abort the install over it.
            warn!("Could not run `lspci -D -nn` to detect GPUs: {e}");
            Vec::new()
        }
    })
}

pub fn gpu_check_nix() {
    let gpus = detect_gpus();
    let Some(nvidia) = gpus.iter().find(|g| g.vendor == GpuVendor::Nvidia) else {
        return;
    };
    info!("NVIDIA GPU detected.");
    if !is_nix() {
        return;
    }
    let graphics_nix = "/mnt/etc/nixos/modules/hardware/graphics/default.nix";
    let edits: [(&str, &str, &str); 6] = [
        ("modesetting.enable =.*", "modesetting.enable = true;", "Enable NVIDIA modesetting"),
        ("powerManagement.enable =.*", "powerManagement.enable = true;", "Enable NVIDIA power management"),
        (r"\bopen\s*=.*", "open = true;", "Enable NVIDIA open kernel modules"),
        ("nvidiaSettings =.*", "nvidiaSettings = true;", "Enable nvidia-settings menu"),
        (
            r"#\s*package = config\.boot\.kernelPackages\.nvidiaPackages\.stable;",
            "package = config.boot.kernelPackages.nvidiaPackages.stable;",
            "Uncomment NVIDIA driver package",
        ),
        (
            r#"#\s*services\.xserver\.videoDrivers = \[ "modesetting" "nvidia" \];"#,
            r#"services.xserver.videoDrivers = [ "modesetting" "nvidia" ];"#,
            "Uncomment NVIDIA video drivers",
        ),
    ];
    for (find, replace, what) in edits {
        files_eval(files::sed_file(graphics_nix, find, replace), what);
    }

    // ---- Hybrid GPU (PRIME) handling ----
    let Some(igpu) = gpus.iter().find(|g| matches!(g.vendor, GpuVendor::Intel | GpuVendor::Amd)) else {
        info!("Single NVIDIA GPU detected; skipping PRIME/hybrid configuration.");
        return;
    };
    info!("Hybrid GPU setup detected (NVIDIA dGPU + {:?} iGPU). Configuring PRIME.", igpu.vendor);
    files_eval(files::sed_file(graphics_nix, r"sync\.enable\s*=.*", "sync.enable = true;"), "Enable NVIDIA PRIME sync");
    files_eval(
        files::sed_file(graphics_nix, r"nvidiaBusId\s*=.*", &format!("nvidiaBusId = \"{}\";", nvidia.bus_id)),
        "Set NVIDIA PRIME bus ID",
    );
    let (find, key, what) = match igpu.vendor {
        GpuVendor::Intel => (r"#\s*intelBusId\s*=.*", "intelBusId", "Set Intel PRIME bus ID"),
        _ => (r"#\s*amdgpuBusId\s*=.*", "amdgpuBusId", "Set AMD PRIME bus ID"),
    };
    files_eval(files::sed_file(graphics_nix, find, &format!("{key} = \"{}\";", igpu.bus_id)), what);
}

/// NVIDIA chip families and the driver generation they need.
enum NvidiaFamily {
    /// Maxwell/Turing and newer: open kernel modules, one package per kernel.
    Open,
    Kepler,
    Fermi,
    Tesla,
}

const NVIDIA_FAMILIES: &[(NvidiaFamily, &[&str])] = &[
    (NvidiaFamily::Open, &["GM107", "GM108", "GM200", "GM204", "GM206", "GM20B"]),
    (NvidiaFamily::Open, &["TU102", "TU104", "TU106", "TU116", "TU117"]),
    (NvidiaFamily::Kepler, &["GK104", "GK107", "GK106", "GK110", "GK110B", "GK208B", "GK208", "GK20A", "GK210"]),
    (NvidiaFamily::Fermi, &["GF100", "GF108", "GF106", "GF104", "GF110", "GF114", "GF116", "GF117", "GF119"]),
    (
        NvidiaFamily::Tesla,
        &[
            "G80", "G84", "G86", "G92", "G94", "G96", "G98", "GT200", "GT215", "GT216", "GT218", "MCP77",
            "MCP78", "MCP79", "MCP7A", "MCP89",
        ],
    ),
];

/// Graphics packages for Arch. NVIDIA open modules are selected for every
/// kernel in [`KERNELS`], so each installed kernel gets a matching driver.
pub fn gpu_check() -> Vec<&'static str> {
    let mut packages: Vec<&'static str> = Vec::new();
    let gpus = detect_gpus();

    if gpus.iter().any(|g| g.vendor == GpuVendor::Amd && g.description.contains("AMD")) {
        info!("AMD GPU detected.");
        packages.extend(["xf86-video-amdgpu", "opencl-amd"]);
    }
    if gpus.iter().any(|g| g.description.contains("ATI") && !g.description.contains("AMD")) {
        info!("ATI GPU detected.");
        packages.push("opencl-mesa");
    }

    if gpus.iter().any(|g| g.vendor == GpuVendor::Nvidia) {
        info!("NVIDIA GPU detected.");
        let gpudetect: String = gpus
            .iter()
            .filter(|g| g.vendor == GpuVendor::Nvidia)
            .map(|g| g.description.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        let mut matched_family = false;
        for (family, chips) in NVIDIA_FAMILIES {
            if !chips.iter().any(|c| gpudetect.contains(c)) {
                continue;
            }
            matched_family = true;
            match family {
                NvidiaFamily::Open => {
                    info!("NVIDIA Maxwell/Turing family");
                    packages.extend(KERNELS.iter().map(|k| k.nvidia_open));
                    packages.push("nvidia-settings");
                }
                NvidiaFamily::Kepler => {
                    info!("NVE0 family (Kepler)");
                    packages.extend(["nvidia-470xx-dkms", "nvidia-470xx-settings"]);
                }
                NvidiaFamily::Fermi => {
                    info!("NVC0 family (Fermi)");
                    packages.extend(["nvidia-390xx-dkms", "nvidia-390xx-settings"]);
                }
                NvidiaFamily::Tesla => {
                    info!("NV50 family (Tesla)");
                    packages.extend(["nvidia-340xx-lts-dkms", "nvidia-340xx-settings"]);
                }
            }
        }
        if !matched_family {
            packages.extend(["nvidia-open-dkms", "nvidia-settings"]);
        }

        packages.extend(["opencl-nvidia", "gwe", "nvtop"]);

        // Hybrid GPU setup? add envycontrol. Only a *display* device from
        // another vendor counts.
        if gpus.iter().any(|g| matches!(g.vendor, GpuVendor::Intel | GpuVendor::Amd)) {
            packages.push("envycontrol");
        }
    }
    packages
}

pub fn is_hyperv_guest() -> bool {
    [
        "/sys/devices/virtual/dmi/id/sys_vendor",
        "/sys/devices/virtual/dmi/id/product_name",
        "/sys/class/dmi/id/sys_vendor",
        "/sys/class/dmi/id/product_name",
    ]
    .iter()
    .filter_map(|p| fs::read_to_string(p).ok())
    .any(|d| {
        let d = d.to_lowercase();
        d.contains("microsoft") || d.contains("hyper-v") || d.contains("hyperv")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busid_conversion() {
        assert_eq!(pci_addr_to_busid("0000:01:00.0").as_deref(), Some("PCI:1@0:0:0"));
        assert_eq!(pci_addr_to_busid("0a:00.1").as_deref(), Some("PCI:10@0:0:1"));
        assert!(pci_addr_to_busid("garbage").is_none());
    }

    #[test]
    fn lspci_parsing_filters_display_devices() {
        let listing = "0000:00:02.0 VGA compatible controller [0300]: Intel Corporation UHD Graphics [8086:9a49]\n\
                       0000:00:14.3 Network controller [0280]: Intel Corporation Wi-Fi 6 [8086:a0f0]\n\
                       0000:01:00.0 3D controller [0302]: NVIDIA Corporation TU117M [10de:1f9d]\n";
        let gpus = parse_lspci(listing);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].vendor, GpuVendor::Intel);
        assert_eq!(gpus[1].vendor, GpuVendor::Nvidia);
        assert_eq!(gpus[1].bus_id, "PCI:1@0:0:0");
    }
}
