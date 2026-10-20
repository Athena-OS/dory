//! Single source of truth for the kernels Athena installs.
//!
//! Adding or removing a kernel here updates: the package list (kernel +
//! headers), the NVIDIA driver selection, the UKIs that get built and signed,
//! the systemd-boot loader entries and the default entry.

pub struct Kernel {
  /// Package name, also the suffix of `/boot/vmlinuz-*` and `initramfs-*.img`.
  pub pkg: &'static str,
  /// Human title used in the boot menu: "Athena OS (<title>)".
  pub title: &'static str,
  /// Package providing the NVIDIA open kernel module for this kernel.
  pub nvidia_open: &'static str,
}

/// First entry is the default boot entry.
pub const KERNELS: &[Kernel] = &[
  Kernel {
    pkg: "linux-aegis-offensive",
    title: "Offensive",
    nvidia_open: "linux-aegis-offensive-nvidia-open",
  },
  Kernel {
    pkg: "linux-aegis-hardened",
    title: "Hardened",
    nvidia_open: "linux-aegis-hardened-nvidia-open",
  },
];

impl Kernel {
  pub fn headers(&self) -> String {
    format!("{}-headers", self.pkg)
  }
  pub fn uki_name(&self) -> String {
    format!("{}.efi", self.pkg)
  }
  pub fn entry_name(&self) -> String {
    format!("athena-{}.conf", self.pkg)
  }
}

/// The kernel whose loader entry is the default.
pub fn default_kernel() -> &'static Kernel {
  &KERNELS[0]
}
