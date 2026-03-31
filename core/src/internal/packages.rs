pub const COMMON: &[&str] = &[
    "linux-firmware", "network-manager-applet", "man-db", "man-pages", "nano", "sudo", "curl",
    "accountsservice", "alacritty", "alsa-utils", "apparmor", "audit", "bind", "bluez", "dhcpcd",
    "dialog", "docker", "dosfstools", "firejail", "irqbalance", "lvm2", "memtest86+", "most", "mtools", "nbd",
    "net-tools", "nfs-utils", "nss-mdns", "ntfsprogs", "pavucontrol", "pv", "rsync", "scx-scheds", "scx-tools",
    "squashfs-tools", "testdisk", "usbutils", "wpa_supplicant", "xfsprogs",
    "pipewire", "pipewire-alsa", "wireplumber", "ntfs-3g", "zram-generator",
    "pocl", "asciinema", "bat", "bc", "cmatrix", "cowsay", "fastfetch", "file-roller",
    "fortune-mod", "git", "gparted", "gvfs-gphoto2", "gvfs-mtp", "hexedit", "jq", "keepassxc",
    "lolcat", "lsd", "nano-syntax-highlighting", "nautilus", "ncdu", "onionshare", "openvpn",
    "orca", "p7zip", "podman", "polkit", "sl", "torbrowser-launcher", "tree",
    "unzip", "usbguard", "vim", "vnstat", "which", "xclip", "xmlstarlet", "zoxide",
    "athena-bash", "athena-kitty-config", "athena-nexus", "athena-settings",
    "athena-tmux-config", "athena-tweak-tool", "athena-vscodium-themes", "athena-welcome",
    "htb-toolkit", "nist-feed",
];

pub const ARCH_ONLY: &[&str] = &[
    "systemd-sysvcompat","networkmanager","arch-install-scripts","edk2-shell",
    "inetutils", "iptables-nft", "mesa","mesa-utils","mkinitcpio-nfs-utils","mkinitcpio-openswap",
    "netctl","ntp","profile-sync-daemon","sof-firmware","wireless_tools",
    "pipewire-pulse","pipewire-jack","ananicy","bashtop","imagemagick","lib32-glibc","mtpfs",
    "networkmanager-openvpn","noto-fonts-cjk", "octopi","openbsd-netcat","paru","reflector",
    "toilet-fonts","wget","athena-firefox-config","athena-powershell-config",
    "athena-vim-config","kando-bin","cai"
];

// Small helpers
pub fn to_strings(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| (*s).to_string()).collect()
}
pub fn extend(dst: &mut Vec<String>, slice: &[&str]) {
    dst.extend(slice.iter().copied().map(String::from));
}
