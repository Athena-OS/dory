use crate::args::{self, ExecMode, MountSpec, OnFail};
use crate::exec::{exec, exec_output, exec_workdir};
use crate::returncode_eval::exec_eval;
use crate::strings::crash;
use log::{debug, info};
use std::collections::HashSet;
use std::fs::{self, create_dir_all};
use std::path::{Path, PathBuf};

/*mkfs.bfs mkfs.cramfs mkfs.ext3  mkfs.fat mkfs.msdos  mkfs.xfs
mkfs.btrfs mkfs.ext2  mkfs.ext4  mkfs.minix mkfs.vfat mkfs.f2fs */

fn extract_partition_number(bdevice: &str) -> String {
    bdevice
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|pos| &bdevice[pos + 1..]) // Extract the digits
        .unwrap_or("") // Return empty string if no match
        .to_string() // Convert &str to String
}

fn encrypt_blockdevice(blockdevice: &str, cryptlabel: &str) {
    let luks_k = String::from("/run/luks");
    exec_eval(
        exec(
            ExecMode::Direct,
            "cryptsetup",
            vec![
                String::from("luksFormat"),
                String::from("-q"),
                String::from(blockdevice),
                String::from("-d"),
                luks_k.clone(),
            ],
            OnFail::Error,
        ),
        "Format LUKS partition",
    );
    exec_eval(
        exec(
            ExecMode::Direct,
            "cryptsetup",
            vec![
                String::from("luksOpen"),
                String::from(blockdevice),
                String::from(cryptlabel),
                String::from("-d"),
                luks_k.clone(),
            ],
            OnFail::Error,
        ),
        "Open LUKS format",
    );
    exec_eval(
        exec(
            ExecMode::Direct,
            "rm",
            vec![
                String::from("-rf"),
                luks_k.clone(),
            ],
            OnFail::Error,
        ),
        "Remove luks key",
    );
}

pub fn partition(
    device: PathBuf,
    table_type: &str,
    mode: &str,
    partitions: &mut [args::Partition],
) {
    let mut plan: Vec<MountSpec> = Vec::new();
    let is_none_label = table_type.eq_ignore_ascii_case("none");
    let is_msdos_label = table_type.eq_ignore_ascii_case("msdos");
    let is_auto_mode = mode.eq_ignore_ascii_case("auto");

    if !device.exists() {
        crash(format!("The device {device:?} doesn't exist"), 1);
    }
    debug!("Partitioning process");

    if is_none_label || (is_msdos_label && is_auto_mode) { // If the disk has no partition table or it is msdos and auto partitioning mode, I will create a GPT one
        if is_uefi() {
            exec_eval(
                exec(
                    ExecMode::Direct,
                    "parted",
                    vec![
                        "-s".into(),
                        device.to_string_lossy().to_string(),
                        "--".into(),
                        "mklabel".into(),
                        "gpt".into(),
                    ],
                    OnFail::Error,
                ),
                &format!("Create a GPT partition table on {}", device.display()),
            );
        }
        else {
            exec_eval(
                exec(
                    ExecMode::Direct,
                    "parted",
                    vec![
                        "-s".into(),
                        device.to_string_lossy().to_string(),
                        "--".into(),
                        "mklabel".into(),
                        "msdos".into(),
                    ],
                    OnFail::Error,
                ),
                &format!("Create an MSDOS (MBR) partition table on {}", device.display()),
            );
        }
    }

    // --- Phase A: deletes first ---
    for p in partitions.iter() {
        if p.action == "delete" {
            info!("Deleting {}", &p.blockdevice);
            delete_partition(&device, &p.blockdevice, p.mountpoint.as_deref().unwrap_or(""));
        }
    }

    // --- Phase B: creates / modifies ---
    for p in partitions.iter_mut() {
        if p.action == "create" {
            info!("Creating {}", &p.blockdevice);
            create_partition(
                &device,
                &p.blockdevice,
                &p.start,
                &p.end,
                p.filesystem.as_deref().unwrap_or(""),
                &p.flags,
                table_type,
            );
        }
        
        if p.action == "create" || p.action == "modify" {
            // Format + mount (or just mount if "don't format")
            let specs = fmt_mount(
                p.mountpoint.as_deref().unwrap_or(""),
                p.filesystem.as_deref().unwrap_or(""),
                &p.blockdevice,
                &p.flags,
            );
            plan.extend(specs);
        }
    }

    mount_queue(plan);
}

fn fmt_mount(mountpoint: &str, filesystem: &str, blockdevice: &str, flags: &[String]) -> Vec<MountSpec> {
    let mut plan = Vec::new();
    let mut bdevice = String::from(blockdevice);
    // Extract the block device name (i.e., sda3)
    let cryptlabel = format!("{}crypted",bdevice.trim_start_matches("/dev/")); // i.e., sda3crypted
    let encryption = flags.iter().any(|f| f.eq_ignore_ascii_case("encrypt"));
    if encryption {
        encrypt_blockdevice(&bdevice, &cryptlabel);
        bdevice = format!("/dev/mapper/{cryptlabel}");
    }

    match filesystem {
        "vfat" | "fat32" => exec_eval(
            exec(ExecMode::Direct, "mkfs.vfat", vec![String::from("-F32"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as vfat").as_str(),
        ),
        "bfs" => exec_eval(
            exec(ExecMode::Direct, "mkfs.bfs", vec![String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as bfs").as_str(),
        ),
        "cramfs" => exec_eval(
            exec(ExecMode::Direct, "mkfs.cramfs", vec![String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as cramfs").as_str(),
        ),
        "ext3" => exec_eval(
            exec(ExecMode::Direct, "mkfs.ext3", vec![String::from("-F"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as ext3").as_str(),
        ),
        "fat" => exec_eval(
            exec(ExecMode::Direct, "mkfs.fat", vec![String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as fat").as_str(),
        ),
        "msdos" => exec_eval(
            exec(ExecMode::Direct, "mkfs.msdos", vec![String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as msdos").as_str(),
        ),
        "xfs" => exec_eval(
            exec(ExecMode::Direct, "mkfs.xfs", vec![String::from("-f"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as xfs").as_str(),
        ),
        "btrfs" => {
            exec_eval(exec(ExecMode::Direct, "mkfs.btrfs", vec!["-f".into(), bdevice.clone()], OnFail::Error),
                      &format!("Formatting {bdevice} as btrfs"));

            // Create subvolumes in a temporary staging mount (not /mnt)
            // Use a device-specific staging dir to avoid collisions
            let stage = format!("/mnt/.staging-btrfs-{}", bdevice.trim_start_matches("/dev/").replace('/', "_"));
            exec_eval(exec(ExecMode::Direct, "mkdir", vec!["-p".into(), stage.clone()], OnFail::Error),
                      &format!("Create staging dir {stage}"));

            // Temporary mount only for subvolume creation
            mount(&bdevice, &stage, "", "btrfs");
            exec_eval(
                exec_workdir("btrfs", &stage, vec!["subvolume".into(), "create".into(), "@".into()]),
                "Create btrfs subvolume @",
            );
            exec_eval(
                exec_workdir("btrfs", &stage, vec!["subvolume".into(), "create".into(), "@home".into()]),
                "Create btrfs subvolume @home",
            );
            umount(&stage);

            // Final btrfs mounts are QUEUED (mounted later in correct order):
            // Root gets subvol=@; /home gets subvol=@home if root was requested.
            if mountpoint == "/" {
                plan.push(MountSpec {
                    device: bdevice.clone(),
                    mountpoint: "/".into(),
                    options: "subvol=@".into(),
                    fstype: "btrfs".into(),
                    is_swap: false,
                });
                plan.push(MountSpec {
                    device: bdevice.clone(),
                    mountpoint: "/home".into(),
                    options: "subvol=@home".into(),
                    fstype: "btrfs".into(),
                    is_swap: false,
                });
            } else if !mountpoint.is_empty() {
                // Non-root btrfs mount without subvols (rare, but support it)
                plan.push(MountSpec {
                    device: bdevice.clone(),
                    mountpoint: mountpoint.to_string(),
                    options: String::new(),
                    fstype: "btrfs".into(),
                    is_swap: false,
                });
            }

            // btrfs path handled fully; return the plan now
            return plan;
        }
        "ext2" => exec_eval(
            exec(ExecMode::Direct, "mkfs.ext2", vec![String::from("-F"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as ext2").as_str(),
        ),
        "ext4" => exec_eval(
            exec(ExecMode::Direct, "mkfs.ext4", vec![String::from("-F"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as ext4").as_str(),
        ),
        "minix" => exec_eval(
            exec(ExecMode::Direct, "mkfs.minix", vec![String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as minix").as_str(),
        ),
        "f2fs" => exec_eval(
            exec(ExecMode::Direct, "mkfs.f2fs", vec![String::from("-f"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as f2fs").as_str(),
        ),
        "ntfs" => exec_eval(
            exec(ExecMode::Direct, "mkfs.ntfs", vec![String::from("-F"), String::from(&bdevice)], OnFail::Error),
            format!("Formatting {bdevice} as ntfs").as_str(),
        ),
        "linux-swap" | "swap" => {
            exec_eval(exec(ExecMode::Direct, "mkswap", vec!["-L".into(), "swap".into(), bdevice.clone()], OnFail::Error),
                      &format!("Formatting {bdevice} as linux-swap"));
            // Queue swap activation (no mountpoint)
            plan.push(MountSpec {
                device: bdevice.clone(),
                mountpoint: String::new(),
                options: String::new(),
                fstype: String::new(),
                is_swap: true,
            });
            return plan; // nothing else to do
        }
        "don't format" => {
            info!("Not formatting {bdevice}");
        }
        _ => {
            crash(
                format!("Unknown filesystem {filesystem}, used in partition {bdevice}"),
                1,
            );
        }
    }

    if !mountpoint.is_empty() {
        plan.push(MountSpec {
            device: bdevice.clone(),
            mountpoint: mountpoint.to_string(),
            options: String::new(),
            fstype: fs_to_mount_type(filesystem).to_string(),
            is_swap: false,
        });
    }

    plan
}

/// Map a requested filesystem to the type string the kernel's `mount -t`
/// expects. Returns "" when we can't/shouldn't specify it (e.g. "don't format",
/// where the on-disk fs is pre-existing and safe to auto-detect).
fn fs_to_mount_type(filesystem: &str) -> &'static str {
    match filesystem {
        "vfat" | "fat32" | "fat" | "msdos" => "vfat",
        "ext2" => "ext2",
        "ext3" => "ext3",
        "ext4" => "ext4",
        "btrfs" => "btrfs",
        "xfs" => "xfs",
        "f2fs" => "f2fs",
        "ntfs" => "ntfs3",
        "minix" => "minix",
        "bfs" => "bfs",
        "cramfs" => "cramfs",
        _ => "", // "don't format" / unknown => auto-detect
    }
}

pub fn mount(partition: &str, mountpoint: &str, options: &str, fstype: &str) {
    // Always pass an explicit `-t <fstype>` when we know it. Right after mkfs,
    // libblkid auto-detection can return a stale/wrong type (a just-deleted
    // partition's old signature still in cache), which makes a bare `mount`
    // fail with "wrong fs type, bad superblock" even though the fs is valid.
    // An explicit type bypasses detection entirely.
    let mut args = vec![String::from(partition), String::from(mountpoint)];
    if !fstype.is_empty() {
        args.push(String::from("-t"));
        args.push(String::from(fstype));
    }
    if !options.is_empty() {
        args.push(String::from("-o"));
        args.push(String::from(options));
    }
    exec_eval(
        exec(ExecMode::Direct, "mount", args, OnFail::Error),
        format!("Mount {partition} ({}) at {mountpoint}", if fstype.is_empty() { "auto" } else { fstype }).as_str(),
    );
}

pub fn umount(mountpoint: &str) {
    let mounts = fs::read_to_string("/proc/mounts")
        .expect("Failed to read /proc/mounts");

    // Only umount if it appears as a mounted path
    if mounts.lines().any(|line| line.split_whitespace().nth(1) == Some(mountpoint)) {
        exec_eval(
            exec(ExecMode::Direct, "umount", vec!["-R".to_string(), mountpoint.to_string()], OnFail::Error),
            &format!("Unmount command processed on {mountpoint}"),
        );
    } else {
        println!("Skipping umount: {mountpoint} is not mounted");
    }
}

pub fn is_uefi() -> bool {
    Path::new("/sys/firmware/efi").is_dir()
}

pub fn partition_info() {
    exec_eval(
        exec(
            ExecMode::Direct,
            "lsblk",
            vec![
                String::from("-o"),
                String::from("NAME,SIZE,FSTYPE,UUID,MOUNTPOINT"),
            ],
            OnFail::Error,
        ),
        "Show lsblk",
    );
}

fn fs_to_parted_fs(filesystem: &str) -> Option<&'static str> {
    match filesystem {
        // parted wants "fat32" (you still mkfs.vfat later)
        "vfat" | "fat32" => Some("fat32"),

        // return literals, not `filesystem`
        "ext4" => Some("ext4"),
        "ext3" => Some("ext3"),
        "ext2" => Some("ext2"),
        "btrfs" => Some("btrfs"),
        "xfs"   => Some("xfs"),
        "ntfs"  => Some("ntfs"),

        // swap GUID/type
        "linux-swap" | "swap" => Some("linux-swap"),

        "don't format" => Some("don't format"),

        _ => None,
    }
}

/// Create a partition in sectors and set flags appropriately:
/// - if flags contain "esp": create ESP (mkpart with fat32 + `set esp on`)
/// - else if flags contain "boot" (but NOT "esp"): create BIOS/GRUB partition (`set bios_grub on`)
/// - else if filesystem is swap: create Linux swap (mkpart linux-swap)
/// - else: create a regular data/root partition (mkpart <fs> or no fs-type)
fn create_partition(
    device: &Path,
    blockdevice: &str,      // e.g. "/dev/sda2" (used to extract the number)
    start_sector: &str,
    end_sector: &str,
    filesystem: &str,       // e.g. "ext4" | "btrfs" | "vfat" | "swap" | "don't format"
    flags: &[String],
    disklabel: &str,
) {

    info!("Filesystem: {filesystem}");
    info!("Block device: {blockdevice}");
    info!("Start Sector: {start_sector}");
    info!("End Sector: {end_sector}");
    info!("Flags: {flags:?}");

    let dev = device.to_string_lossy().to_string();
    let partnum = extract_partition_number(blockdevice);
    if partnum.is_empty() {
        crash(
            format!("Cannot derive partition number from {blockdevice}. It must be like /dev/sda3."),
            1,
        );
    }

    let is_none_label = disklabel.eq_ignore_ascii_case("none");
    let is_gpt = disklabel.eq_ignore_ascii_case("gpt") || (is_none_label && is_uefi());
    let is_mbr = disklabel.eq_ignore_ascii_case("msdos") || (is_none_label && !is_uefi());

    let has_esp  = flags.iter().any(|f| f.eq_ignore_ascii_case("esp"));
    let has_xbld  = flags.iter().any(|f| f.eq_ignore_ascii_case("bls_boot"));
    let has_boot = flags.iter().any(|f| f.eq_ignore_ascii_case("boot"));
    let is_swap  = filesystem.eq_ignore_ascii_case("swap") || filesystem.eq_ignore_ascii_case("linux-swap");

    // Decide a human label (optional). We set it after mkpart with `name N <LABEL>`.
    let label = if has_esp {
        "EFI"
    } else if has_xbld {
        "XBOOTLDR"
    } else if has_boot && !has_esp {
        "BIOS_GRUB"
    } else if is_swap {
        "SWAP"
    } else {
        "" // no label
    };

    // Build mkpart command in sectors.
    let mut args = vec![
        String::from("-s"),
        dev.clone(),
        String::from("unit"),
        String::from("s"),
        String::from("--"),
        String::from("mkpart"),
    ];

    // Special-cases for GUID/type selection
    if is_gpt {
        if has_esp { // Only in GPT I can use EFI boot partition
            // ESP is FAT; parted flag will mark it on GPT
            args.push(String::from("ESP"));
            args.push(String::from("fat32"));
        } else if has_xbld{
            args.push(String::from("XBOOTLDR"));
            args.push(String::from("fat32"));
        } else if is_swap {
            // Use linux-swap so GUID becomes 8200
            args.push(String::from("swap"));
            args.push(String::from("linux-swap"));
        } else if let Some(pfs) = fs_to_parted_fs(filesystem) {
            // Regular data partition with a known fs-type (gives 8300 on GPT)
            args.push(String::from(pfs));
        }
    } else if is_mbr {
        args.push("primary".into());
        if is_swap {
            args.push(String::from("linux-swap"));
        } else if let Some(pfs) = fs_to_parted_fs(filesystem) {
            // Regular data partition with a known fs-type (gives 8300 on GPT)
            args.push(String::from(pfs));
        }
    }

    args.push(start_sector.to_string());
    args.push(end_sector.to_string());

    info!("Running: parted {args:?}");
    exec_eval(
        exec(ExecMode::Direct, "parted", args, OnFail::Error),
        &format!("Create partition {blockdevice} from {start_sector} to {end_sector}"),
    );

    // Name it by setting a label
    if is_gpt && !label.is_empty() {
        exec_eval(
            exec(
                ExecMode::Direct,
                "parted",
                vec![
                    "-s".into(),
                    dev.clone(),
                    "--".into(),
                    "name".into(),
                    partnum.clone(),
                    label.into(),
                ],
                OnFail::Error,
            ),
            &format!("Name partition #{partnum} as {label}"),
        );
    }

    // Only set flags on the current created partition if applicable
    if is_gpt && has_esp {
        exec_eval(exec(ExecMode::Direct, "parted", vec!["-s".into(), dev.clone(), "--".into(), "set".into(), partnum.clone(), "esp".into(), "on".into()], OnFail::Error),
            &format!("Enable ESP on partition #{partnum}"));
    } else if is_gpt && has_xbld{
        exec_eval(exec(ExecMode::Direct, "parted", vec!["-s".into(), dev.clone(), "--".into(), "set".into(), partnum.clone(), "bls_boot".into(), "on".into()], OnFail::Error),
            &format!("Enable ESP on partition #{partnum}"));
    } else if is_gpt && has_boot && !has_esp {
        // For real BIOS-on-GPT you'd typically have a tiny bios_grub partition; this flag is for that case.
        exec_eval(exec(ExecMode::Direct, "parted", vec!["-s".into(), dev.clone(), "--".into(), "set".into(), partnum.clone(), "bios_grub".into(), "on".into()], OnFail::Error),
            &format!("Enable BIOS_GRUB on partition #{partnum}"));
    } else if is_mbr && has_boot {
        exec_eval(exec(ExecMode::Direct, "parted", vec!["-s".into(), dev.clone(), "--".into(), "set".into(), partnum.clone(), "boot".into(), "on".into()], OnFail::Error),
            &format!("Enable boot flag on partition #{partnum}"));
    }

    // Inform the kernel of the new table and wait for udev to recreate the /dev
    // nodes BEFORE we mkfs/mount. Without this, a stale partition node (from a
    // previous table) can still map to the old offsets: mkfs writes a valid
    // filesystem at the old location, then the later mount sees the new geometry
    // and fails with "bad superblock". These are best-effort (Continue), since
    // partprobe can return EBUSY and must not abort the install.
    exec_eval(
        exec(ExecMode::Direct, "partprobe", vec![dev.clone()], OnFail::Continue),
        &format!("Inform kernel of new partition on {}", device.display()),
    );
    exec_eval(
        exec(ExecMode::Direct, "udevadm", vec!["settle".into()], OnFail::Continue),
        "Wait for udev to settle",
    );
    // Block until THIS specific partition node is present with current attributes,
    // so mkfs and the later mount agree on where the partition lives on disk.
    exec_eval(
        exec(
            ExecMode::Direct,
            "udevadm",
            vec!["wait".into(), "--timeout=10".into(), blockdevice.to_string()],
            OnFail::Continue,
        ),
        &format!("Wait for {blockdevice} to appear"),
    );
}

fn delete_partition(device: &Path, blockdevice: &str, mountpoint: &str) {
    // --- inline helpers ------------------------------------------------------
    // NOTE: these are *inline* and not defined as separate functions.
    info!("Mount point: {mountpoint}");
    info!("Block device: {blockdevice}");

    // Best-effort swapoff (non-fatal if not active)
    exec_eval(
        exec(
            ExecMode::Direct,
            "sh",
            vec![
                String::from("-c"),
                format!("swapoff '{}' || true", blockdevice),
            ],
            OnFail::Error,
        ),
        format!("Swapoff {blockdevice} (if active)").as_str(),
    );

    // Best-effort recursive unmount by mountpoint (non-fatal if not mounted)
    exec_eval(
        exec(
            ExecMode::Direct,
            "sh",
            vec![
                String::from("-c"),
                format!("umount -R '{}' || true", mountpoint),
            ],
            OnFail::Error,
        ),
        format!("Umount {mountpoint} (best-effort)").as_str(),
    );

    // If it's a mapper path, best-effort close it (non-fatal if not open)
    if blockdevice.starts_with("/dev/mapper/") {
        let name = blockdevice.trim_start_matches("/dev/mapper/").to_string();
        exec_eval(
            exec(
                ExecMode::Direct,
                "sh",
                vec![
                    String::from("-c"),
                    format!("cryptsetup luksClose '{}' || true", name),
                ],
                OnFail::Error,
            ),
            format!("Close LUKS mapper for {blockdevice} (if open)").as_str(),
        );
    }

    // --- figure out partition number on the physical disk --------------------
    let partnum = extract_partition_number(blockdevice);
    if partnum.is_empty() {
        crash(
            format!(
                "Cannot extract partition number from {blockdevice}. \
                 Provide a real partition path like /dev/sda3 (not a /dev/mapper/* node)."
            ),
            1,
        );
    }

    // --- delete the partition from the label (via parted) --------------------
    exec_eval(
        exec(
            ExecMode::Direct,
            "parted",
            vec![
                String::from("-s"),
                device.to_string_lossy().to_string(),
                String::from("--"),
                String::from("rm"),
                partnum.clone(),
            ],
            OnFail::Error,
        ),
        format!("Delete partition #{partnum} on {}", device.display()).as_str(),
    );

    // --- inform the kernel about the table change ----------------------------
    // Best-effort: re-read the table and let udev tear down the deleted node so a
    // stale partition node doesn't linger into the create/format phase.
    exec_eval(
        exec(ExecMode::Direct, "partprobe", vec![device.to_string_lossy().to_string()], OnFail::Continue),
        format!(
            "Inform kernel of partition table changes on {}",
            device.display()
        )
        .as_str(),
    );
    exec_eval(
        exec(ExecMode::Direct, "udevadm", vec!["settle".into()], OnFail::Continue),
        "Wait for udev to settle after delete",
    );
}

fn path_depth(mp: &str) -> usize {
    if mp == "/" || mp.is_empty() {
        return 0;
    }
    mp.trim_matches('/').split('/').filter(|s| !s.is_empty()).count()
}

fn sort_mount_queue(plan: &mut [MountSpec]) {
    plan.sort_by_key(|m| {
        // order: swap first, then root, then shallower paths
        let swap_key = if m.is_swap { 0 } else { 1 };
        let root_key = if m.mountpoint == "/" { 0 } else { 1 };
        (swap_key, root_key, path_depth(&m.mountpoint))
    });
}

fn mount_queue(mut plan: Vec<MountSpec>) {
    sort_mount_queue(&mut plan);

    // to prevent repeated mkdir on the same target
    let mut created: HashSet<String> = HashSet::new();
    created.insert("/mnt".to_string()); // l’abbiamo appena creato

    for m in plan {
        if m.is_swap {
            exec_eval(exec(ExecMode::Direct, "swapon", vec![m.device.clone()], OnFail::Error),
                      &format!("Activate swap {}", m.device));
            continue;
        }

        let target = if m.mountpoint == "/" {
            "/mnt".to_string()
        } else {
            // prevent double / at the end
            format!("/mnt{}", m.mountpoint.trim_end_matches('/'))
        };

        // create the mountpoint dir if not already created
        if created.insert(target.clone()) {
            create_dir_all(&target).unwrap_or_else(|e| panic!("Failed to create {target}: {e}"));
            info!("{target} directory created.");
        }

        // The filesystem was just created by mkfs a moment ago. The kernel's
        // block-device page/buffer cache for this node can still hold pre-mkfs
        // content (the old/empty signature from when udev probed the freshly
        // partitioned device). Mounting then reads a stale superblock and fails
        // with "wrong fs type, bad superblock" even though the on-disk fs is
        // perfectly valid (this is why a manual mount after reboot succeeds).
        // Force coherence before mounting:
        //   sync                -> flush mkfs's dirty writes to the device
        //   blockdev --flushbufs-> BLKFLSBUF: sync + INVALIDATE the buffer cache
        //   udevadm settle      -> let udev finish its re-probe so the blkid db
        //                          (used later by genfstab for UUIDs) is current
        exec_eval(
            exec(ExecMode::Direct, "sync", vec![], OnFail::Continue),
            "Flush pending writes before mount",
        );
        exec_eval(
            exec(ExecMode::Direct, "blockdev", vec!["--flushbufs".into(), m.device.clone()], OnFail::Continue),
            &format!("Invalidate stale buffer cache on {}", m.device),
        );
        exec_eval(
            exec(ExecMode::Direct, "udevadm", vec!["settle".into()], OnFail::Continue),
            "Wait for udev to settle before mount",
        );

        mount(&m.device, &target, &m.options, &m.fstype);
    }
}

pub fn close_luks_best_effort(blockdevice: &str) {
    // 1) Try the conventional name first: /dev/mapper/<basename>crypted
    let conventional = format!(
        "/dev/mapper/{}crypted",
        blockdevice.trim_start_matches("/dev/")
    );
    if let Some(name) = conventional.strip_prefix("/dev/mapper/") {
        exec_eval(
            exec(ExecMode::Direct, "sh", vec!["-c".into(), format!("cryptsetup luksClose '{}' || true", name)], OnFail::Error),
            &format!("Close LUKS mapper (conventional) {}", name),
        );
    }

    // 2) Also scan lsblk for any crypt children that reference this blockdevice
    let out = exec_output("lsblk", vec!["-rno".into(), "NAME,TYPE,PKNAME".into(), "-p".into()]);
    if let Ok(out) = out {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            // Example line: "/dev/mapper/vda2crypt crypt /dev/vda2"
            let mut it = line.split_whitespace();
            let name = it.next().unwrap_or("");
            let typ  = it.next().unwrap_or("");
            let pk   = it.next().unwrap_or("");
            if typ == "crypt" && pk == blockdevice
                && let Some(mapper) = name.strip_prefix("/dev/mapper/") {
                    exec_eval(
                        exec(ExecMode::Direct, "sh", vec!["-c".into(), format!("cryptsetup luksClose '{}' || true", mapper)], OnFail::Error),
                        &format!("Close LUKS mapper {}", mapper),
                    );
                }
        }
    }
}