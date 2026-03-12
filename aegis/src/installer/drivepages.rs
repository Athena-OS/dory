use log::{debug, error};
use ratatui::{
  Frame,
  crossterm::event::{KeyCode, KeyEvent},
  layout::{Constraint, Direction, Layout, Rect},
  style::{Color, Modifier},
};
use serde_json::Value;
use shared::partition::is_uefi;
use std::{fs::{OpenOptions, set_permissions, Permissions}, io::Write, os::unix::fs::PermissionsExt, process::Command};

use crate::{
  drives::{
    DiskItem, PartStatus, Partition, bytes_readable_floor, disk_table, lsblk, parse_sectors, part_table,
  },
  installer::{Installer, Page, Signal},
  split_hor, split_vert, styled_block, ui_back, ui_close, ui_down, ui_enter, ui_up,
  widget::{
    Button, CheckBox, ConfigWidget, HelpModal, InfoBox, LineEditor, TableWidget, WidgetBox,
  },
};

const HIGHLIGHT: Option<(Color, Modifier)> = Some((Color::Yellow, Modifier::BOLD));

fn write_luks_key_to_tmpfs(pass: &str, path: &str) -> anyhow::Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;

    set_permissions(path, Permissions::from_mode(0o400))?;

    f.write_all(pass.as_bytes())?;
    f.flush()?;
    Ok(())
}

pub struct Drives<'a> {
  pub buttons: WidgetBox,
  pub info_box: InfoBox<'a>,
  help_modal: HelpModal<'static>,
}

impl<'a> Drives<'a> {
  pub fn new() -> Self {
    let buttons = vec![
      Box::new(Button::new("Use a best-effort default partition layout")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Configure partitions manually")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Back")) as Box<dyn ConfigWidget>,
    ];
    let mut button_row = WidgetBox::button_menu(buttons);
    button_row.focus();
    let info_box = InfoBox::new(
      "Drive Configuration",
      styled_block(vec![
        vec![(
          None,
          "Select how you would like to configure your drives for the NixOS installation.",
        )],
        vec![
          (None, "- "),
          (
            Some((Color::Green, Modifier::BOLD)),
            "'Use a best-effort default partition layout'",
          ),
          (
            None,
            " will attempt to automatically partition and format your selected drive with sensible defaults. ",
          ),
          (None, "This is recommended for most users."),
        ],
        vec![
          (None, "- "),
          (
            Some((Color::Green, Modifier::BOLD)),
            "'Configure partitions manually'",
          ),
          (
            None,
            " will allow you to specify exactly how your drive should be partitioned and formatted. ",
          ),
          (
            None,
            "This is recommended for advanced users who have specific requirements.",
          ),
        ],
        vec![
          (Some((Color::Red, Modifier::BOLD)), "NOTE: "),
          (None, "When the installer is run, "),
          (
            Some((Color::Red, Modifier::BOLD | Modifier::ITALIC)),
            " any and all",
          ),
          (
            None,
            " data on the selected drive will be wiped. Make sure you've backed up any important data.",
          ),
        ],
      ]),
    );

    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select drive configuration method"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to main menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Choose how to configure your drive for NixOS installation:",
      )],
      vec![(
        None,
        "• Best-effort default - Automatic partitioning (recommended)",
      )],
      vec![(None, "• Manual configuration - Advanced users only")],
      vec![(None, "")],
      vec![
        (Some((Color::Red, Modifier::BOLD)), "WARNING: "),
        (None, "All data on the selected drive will be erased!"),
      ],
    ]);
    let help_modal = HelpModal::new("Drive Configuration", help_content);
    Self {
      buttons: button_row,
      info_box,
      help_modal,
    }
  }
}

impl<'a> Default for Drives<'a> {
  fn default() -> Self {
    Self::new()
  }
}

impl<'a> Page for Drives<'a> {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [Constraint::Percentage(70), Constraint::Percentage(30)]
    );

    self.info_box.render(f, chunks[0]);
    self.buttons.render(f, chunks[1]);

    // Render help modal on top
    self.help_modal.render(f, area);
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      ui_up!() => {
        self.buttons.prev_child();
        Signal::Wait
      }
      ui_down!() => {
        self.buttons.next_child();
        Signal::Wait
      }
      ui_enter!() => {
        let Some(idx) = self.buttons.selected_child() else {
          return Signal::Wait;
        };
        let disks = match lsblk() {
          Ok(disks) => disks,
          Err(e) => return Signal::Error(anyhow::anyhow!("Failed to list block devices: {e}")),
        };
        let table = disk_table(&disks);
        installer.drives = disks;
        match idx {
          0 => {
            installer.use_auto_drive_config = true;
            Signal::Push(Box::new(SelectDrive::new(table)))
          }
          1 => {
            installer.use_auto_drive_config = false;
            Signal::Push(Box::new(SelectDrive::new(table)))
          }
          2 => Signal::Pop,
          _ => Signal::Wait,
        }
      }
      _ => Signal::Wait,
    }
  }

  fn get_help_content(&self) -> (String, Vec<ratatui::text::Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select drive configuration method"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to main menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Choose how to configure your drive for NixOS installation:",
      )],
      vec![(
        None,
        "• Best-effort default - Automatic partitioning (recommended)",
      )],
      vec![(None, "• Manual configuration - Advanced users only")],
      vec![(None, "")],
      vec![
        (Some((Color::Red, Modifier::BOLD)), "WARNING: "),
        (None, "All data on the selected drive will be erased!"),
      ],
    ]);
    ("Drive Configuration".to_string(), help_content)
  }
}

pub struct SelectDrive {
  table: TableWidget,
  help_modal: HelpModal<'static>,
}

impl SelectDrive {
  pub fn new(mut table: TableWidget) -> Self {
    table.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate drive list"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select drive for installation"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to previous menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the drive you want to use for your NixOS installation.",
      )],
      vec![(
        None,
        "The selected drive will be used for partitioning and formatting.",
      )],
      vec![
        (Some((Color::Red, Modifier::BOLD)), "WARNING: "),
        (None, "All data on the selected drive will be erased!"),
      ],
    ]);
    let help_modal = HelpModal::new("Select Drive", help_content);
    Self { table, help_modal }
  }
}

impl Page for SelectDrive {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    self.table.render(f, area);

    // Render help modal on top
    self.help_modal.render(f, area);
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      ui_up!() => {
        self.table.previous_row();
        Signal::Wait
      }
      ui_down!() => {
        self.table.next_row();
        Signal::Wait
      }
      ui_enter!() => {
        if let Some(row) = self.table.selected_row() {
          // Copy out the selected disk name to avoid borrowing `installer.drives`
          let selected_name = match installer.drives.get(row) {
            Some(d) => d.name().to_string(),
            None => {
              return Signal::Error(anyhow::anyhow!("Failed to find drive info"));
            }
          };

          // Run partprobe/settle on that device node
          let devnode = format!("/dev/{}", selected_name);
          let _ = Command::new("partprobe").arg(&devnode).status(); // Not using exec() because these commands are run on TUI layout, otherwise they print an info!() and the TUI layout will be screwed up
          let _ = Command::new("udevadm").arg("settle").status();

          // Re-scan devices so our in-memory model matches the kernel
          let disks = match lsblk() {
            Ok(disks) => disks,
            Err(e) => return Signal::Error(anyhow::anyhow!("Failed to list block devices: {e}")),
          };
          installer.drives = disks;

          // Find the same disk again by name after refresh
          let Some(idx_refreshed) = installer
            .drives
            .iter()
            .position(|d| d.name() == selected_name)
          else {
            return Signal::Error(anyhow::anyhow!("Selected drive disappeared after partprobe"));
          };

          // Set drive_config from the refreshed entry
          let disk = installer.drives[idx_refreshed].clone();
          installer.drive_config = Some(disk.clone());

          if installer.use_auto_drive_config {
            Signal::Push(Box::new(SelectSwap::new()))
          } else {
            let table = part_table(disk.layout(), disk.sector_size());
            Signal::Push(Box::new(ManualPartition::new(table)))
          }
        } else {
          Signal::Wait
        }
      }
      _ => Signal::Wait,
    }
  }

  fn get_help_content(&self) -> (String, Vec<ratatui::text::Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate drive list"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select drive for installation"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to previous menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the drive you want to use for your NixOS installation.",
      )],
      vec![(
        None,
        "The selected drive will be used for partitioning and formatting.",
      )],
      vec![
        (Some((Color::Red, Modifier::BOLD)), "WARNING: "),
        (None, "All data on the selected drive will be erased!"),
      ],
    ]);
    ("Select Drive".to_string(), help_content)
  }
}

pub struct SelectFilesystem {
  pub buttons: WidgetBox,
  pub dev_id: Option<u64>,
  help_modal: HelpModal<'static>,
}

impl SelectFilesystem {
  pub fn new(dev_id: Option<u64>) -> Self {
    let buttons = vec![
      Box::new(Button::new("ext4")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("ext3")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("ext2")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("btrfs")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("xfs")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("fat32")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("ntfs")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("swap")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("don't format")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Back")) as Box<dyn ConfigWidget>,
    ];
    let mut button_row = WidgetBox::button_menu(buttons);
    button_row.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate filesystem options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select filesystem type"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to previous menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Choose the filesystem type for your partition.")],
      vec![(
        None,
        "Different filesystems have different features and performance",
      )],
      vec![(None, "characteristics. ext4 is recommended for most users.")],
    ]);
    let help_modal = HelpModal::new("Select Filesystem", help_content);
    Self {
      buttons: button_row,
      dev_id,
      help_modal,
    }
  }
  pub fn get_fs_info<'a>(idx: usize) -> InfoBox<'a> {
    match idx {
      0 => InfoBox::new(
        "ext4",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "ext4"),
            (None, " is a"),
            (HIGHLIGHT, " widely used and stable filesystem"),
            (None, " known for its "),
            (HIGHLIGHT, "reliability and performance."),
          ],
          vec![
            (None, "It supports "),
            (HIGHLIGHT, "journaling"),
            (None, ", which helps "),
            (HIGHLIGHT, "protect against data corruption "),
            (None, "in case of crashes."),
          ],
          vec![
            (None, "It's a good choice for"),
            (HIGHLIGHT, " general-purpose"),
            (None, " use and is"),
            (
              HIGHLIGHT,
              " well-supported across various Linux distributions.",
            ),
          ],
        ]),
      ),
      1 => InfoBox::new(
        "ext3",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "ext3"),
            (
              None,
              " is an older journaling filesystem that builds upon ext2.",
            ),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "journaling"),
            (
              None,
              " capabilities to improve data integrity and recovery after crashes.",
            ),
          ],
          vec![
            (None, "While it is "),
            (HIGHLIGHT, "reliable and stable"),
            (
              None,
              ", it lacks some of the performance and features of ext4.",
            ),
          ],
        ]),
      ),
      2 => InfoBox::new(
        "ext2",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "ext2"),
            (
              None,
              " is a non-journaling filesystem that is simple and efficient.",
            ),
          ],
          vec![
            (None, "It is suitable for use cases where "),
            (HIGHLIGHT, "journaling is not required"),
            (None, ", such as "),
            (HIGHLIGHT, "flash drives"),
            (None, " or "),
            (HIGHLIGHT, "small partitions"),
            (None, "."),
          ],
          vec![
            (None, "However, it is more "),
            (HIGHLIGHT, "prone to data corruption "),
            (None, "in case of crashes compared to ext3 and ext4."),
          ],
        ]),
      ),
      3 => InfoBox::new(
        "btrfs",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "btrfs"),
            (None, " ("),
            (Some((Color::Reset, Modifier::ITALIC)), "B-tree filesystem"),
            (None, ") is a "),
            (HIGHLIGHT, "modern filesystem"),
            (None, " that offers advanced features like "),
            (HIGHLIGHT, "snapshots"),
            (None, ", "),
            (HIGHLIGHT, "subvolumes"),
            (None, ", and "),
            (HIGHLIGHT, "built-in RAID support"),
            (None, "."),
          ],
          vec![
            (None, "It is designed for "),
            (HIGHLIGHT, "scalability"),
            (None, " and "),
            (HIGHLIGHT, "flexibility"),
            (None, ", making it suitable for systems that require "),
            (HIGHLIGHT, "complex storage solutions."),
          ],
          vec![
            (None, "However, it may not be as mature as "),
            (HIGHLIGHT, "ext4"),
            (None, " in terms of "),
            (HIGHLIGHT, "stability"),
            (None, " for all use cases."),
          ],
        ]),
      ),
      4 => InfoBox::new(
        "xfs",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "XFS"),
            (None, " is a "),
            (HIGHLIGHT, "high-performance journaling filesystem"),
            (None, " that excels in handling "),
            (HIGHLIGHT, "large files"),
            (None, " and "),
            (HIGHLIGHT, "high I/O workloads"),
            (None, "."),
          ],
          vec![
            (None, "It is known for its "),
            (HIGHLIGHT, "scalability"),
            (None, " and "),
            (HIGHLIGHT, "robustness"),
            (None, ", making it a popular choice for "),
            (HIGHLIGHT, "enterprise environments"),
            (None, "."),
          ],
          vec![
            (HIGHLIGHT, "XFS"),
            (
              None,
              " is particularly well-suited for systems that require efficient handling of ",
            ),
            (HIGHLIGHT, "large datasets"),
            (None, "."),
          ],
        ]),
      ),
      5 => InfoBox::new(
        "fat32",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "FAT32"),
            (None, " is a widely supported filesystem that can handle"),
            (HIGHLIGHT, " larger partitions and files than FAT16"),
            (None, "."),
          ],
          vec![
            (
              None,
              "It is commonly used for USB drives and memory cards due to its broad ",
            ),
            (HIGHLIGHT, "cross-platform compatibility"),
            (None, "."),
          ],
          vec![
            (None, "FAT32 is also commonly used for "),
            (HIGHLIGHT, "EFI System Partitions (ESP)"),
            (
              None,
              " on UEFI systems, allowing the firmware to load the bootloader.",
            ),
          ],
          vec![
            (None, "However, it has limitations such as a "),
            (HIGHLIGHT, "maximum file size of 4GB"),
            (None, " and"),
            (HIGHLIGHT, " lack of modern journaling features."),
          ],
        ]),
      ),
      6 => InfoBox::new(
        "ntfs",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "NTFS"),
            (None, " is a"),
            (HIGHLIGHT, " robust"),
            (None, " and"),
            (HIGHLIGHT, " feature-rich"),
            (None, " filesystem developed by Microsoft."),
          ],
          vec![
            (None, "It supports "),
            (HIGHLIGHT, "large files"),
            (None, ", "),
            (HIGHLIGHT, "advanced permissions"),
            (None, ", "),
            (HIGHLIGHT, "encryption"),
            (None, ", and "),
            (HIGHLIGHT, "journaling"),
            (None, "."),
          ],
          vec![
            (None, "While it is"),
            (HIGHLIGHT, " primarily used in Windows environments"),
            (None, ", Linux has good support for NTFS through the "),
            (HIGHLIGHT, "ntfs-3g"),
            (None, " driver."),
          ],
          vec![
            (None, "NTFS is a good choice if you need to "),
            (HIGHLIGHT, "share data between Windows and Linux systems "),
            (None, "or if you require features like "),
            (HIGHLIGHT, "file compression and encryption"),
            (None, "."),
          ],
        ]),
      ),
      7 => InfoBox::new(
        "swap",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "swap"),
            (None, " is used as virtual memory backing.")
          ],
          vec![
            (None, "No mount point. Can be "),
            (HIGHLIGHT, "encrypted"),
            (None, ".")
          ],
          vec![
            (None, "Size should fit your needs (RAM/2..RAM, etc).")
          ],
        ]),
      ),
      8 => InfoBox::new(
        "don't format",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "'don't format'"),
            (None, " is used for those partitions you don't want to wipe out.")
          ],
          vec![
            (None, "For example a "),
            (HIGHLIGHT, "boot partition"),
            (None, " to not delete current bootloader on it.")
          ],
        ]),
      ),
      _ => InfoBox::new(
        "Unknown Filesystem",
        styled_block(vec![vec![(
          None,
          "No information available for this filesystem.",
        )]]),
      ),
    }
  }
}

impl Page for SelectFilesystem {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(40),
        Constraint::Percentage(20),
        Constraint::Percentage(40),
      ]
    );

    let idx = self.buttons.selected_child().unwrap_or(11);
    let info_box = Self::get_fs_info(self.buttons.selected_child().unwrap_or(11));
    self.buttons.render(f, hor_chunks[1]);
    if idx < 11 {
      info_box.render(f, vert_chunks[1]);
    }

    // Render help modal on top
    self.help_modal.render(f, area);
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        return Signal::Wait;
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        return Signal::Wait;
      }
      _ if self.help_modal.visible => {
        return Signal::Wait;
      }
      ui_back!() => {
        return Signal::Pop;
      }
      ui_up!() => {
        self.buttons.prev_child();
        return Signal::Wait;
      }
      ui_down!() => {
        self.buttons.next_child();
        return Signal::Wait;
      }
      ui_enter!() => {
        let Some(idx) = self.buttons.selected_child() else {
          return Signal::Wait;
        };
        let fs = match idx {
          0 => "ext4",
          1 => "ext3",
          2 => "ext2",
          3 => "btrfs",
          4 => "xfs",
          5 => "fat32",
          6 => "ntfs",
          7 => "swap",
          8 => "don't format",
          9 => return Signal::Pop,
          _ => return Signal::Wait,
        }
        .to_string();

        if installer.use_auto_drive_config {
          if let Some(cfg) = installer.drive_config.as_mut() {
            let swap_gb = installer.swap.take();
            // build layout now (no encryption yet)
            cfg.use_default_layout_with_swap(Some(fs.clone()), swap_gb);
            cfg.assign_device_numbers();
          }
          installer.make_drive_config_display();
        
          // find ROOT id
          let root_id = installer
            .drive_config
            .as_ref()
            .and_then(|cfg| cfg.partitions().find(|p| p.mount_point() == Some("/")).map(|p| p.id()));
        
          if let Some(root_id) = root_id {
            // ask if user wants encryption on ROOT; that page will push PromptLuksPassword if needed
            return Signal::Push(Box::new(AskEncryptRoot::new(root_id)));
          } else {
            // fallback: no root? just return to summary
            return Signal::PopCount(4);
          }
        }

        let Some(config) = installer.drive_config.as_mut() else {
          return Signal::Error(anyhow::anyhow!("No drive config available"));
        };
        let Some(id) = self.dev_id else {
          return Signal::Error(anyhow::anyhow!(
            "No device id specified for filesystem selection"
          ));
        };
        let Some(partition) = config.partition_by_id_mut(id) else {
          return Signal::Error(anyhow::anyhow!("No partition found with id {id:?}"));
        };

        // Set filesystem
        partition.set_fs_type(&fs);

        // Normalize when switching TO swap
        if fs == "swap" {
          // optional but recommended: drop flags that don't apply to swap
          partition.remove_flags(["boot", "esp", "bls_boot"].into_iter());
          partition.clear_mount_point();
          partition.set_label("SWAP");
        }

        return Signal::Pop;
      }
      _ => {}
    }
    
    Signal::Wait
  }

  fn get_help_content(&self) -> (String, Vec<ratatui::text::Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate filesystem options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select filesystem type"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to previous menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Choose the filesystem type for your partition.")],
      vec![(
        None,
        "Different filesystems have different features and performance",
      )],
      vec![(None, "characteristics. ext4 is recommended for most users.")],
    ]);
    ("Select Filesystem".to_string(), help_content)
  }
}

pub struct SelectSwap {
  buttons: WidgetBox,
  help_modal: HelpModal<'static>,
}

impl Default for SelectSwap {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectSwap {
  pub fn new() -> Self {
    let buttons = vec![
      Box::new(Button::new("No swap")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("1 GB")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("2 GB")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("4 GB")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("8 GB")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Back")) as Box<dyn ConfigWidget>,
    ];
    let mut button_row = WidgetBox::button_menu(buttons);
    button_row.focus();

    let help_content = styled_block(vec![
      vec![(Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"), (None, " - Navigate")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Select swap size")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc"), (None, " - Go back")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "?"), (None, " - Help")],
      vec![(None, "")],
      vec![(None, "Choose a swap size (or disable swap).")],
    ]);
    let help_modal = HelpModal::new("Select Swap", help_content);
    Self { buttons: button_row, help_modal }
  }
}

impl Page for SelectSwap {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(70), Constraint::Percentage(30)]);
    let info = InfoBox::new(
      "Swap",
      styled_block(vec![
        vec![(None, "Enable swap and choose a size. ")],
        vec![(None, "Swap can help prevent OOM on low-memory systems.")],
      ]),
    );
    info.render(f, chunks[0]);
    self.buttons.render(f, chunks[1]);

    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => { self.help_modal.toggle(); Signal::Wait }
      ui_close!() if self.help_modal.visible => { self.help_modal.hide(); Signal::Wait }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      ui_up!() => { self.buttons.prev_child(); Signal::Wait }
      ui_down!() => { self.buttons.next_child(); Signal::Wait }
      ui_enter!() => {
        let Some(idx) = self.buttons.selected_child() else { return Signal::Wait; };
        installer.swap = match idx {
          0 => None,                // No swap
          1 => Some(1),
          2 => Some(2),
          3 => Some(4),
          4 => Some(8),
          5 => return Signal::Pop,  // Back
          _ => return Signal::Wait,
        };
        Signal::Push(Box::new(SelectFilesystem::new(None)))
      }
      _ => Signal::Wait,
    }
  }

  fn get_help_content(&self) -> (String, Vec<ratatui::text::Line<'_>>) {
    let help = styled_block(vec![
      vec![(Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"), (None, " - Navigate")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Select swap size")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc"), (None, " - Go back")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "?"), (None, " - Help")],
    ]);
    ("Select Swap".to_string(), help)
  }
}

pub struct ManualPartition {
  disk_config: TableWidget,
  buttons: WidgetBox,
  confirming_reset: bool,
  help_modal: HelpModal<'static>,
}

impl ManualPartition {
  pub fn new(mut disk_config: TableWidget) -> Self {
    let buttons = vec![
      Box::new(Button::new("Confirm and Exit")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Reset Partition Layout")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Abort")) as Box<dyn ConfigWidget>,
    ];
    let buttons = WidgetBox::button_menu(buttons);
    disk_config.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate partitions and buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch between partition table and buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select partition or button action"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to previous menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Manually configure drive partitions. Select partitions to",
      )],
      vec![(
        None,
        "modify them or select free space to create new partitions.",
      )],
      vec![(None, "Use buttons at bottom for additional actions.")],
    ]);
    let help_modal = HelpModal::new("Manual Partitioning", help_content);
    Self {
      disk_config,
      buttons,
      confirming_reset: false,
      help_modal,
    }
  }
}

impl Page for ManualPartition {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect) {
    let Some(config) = installer.drive_config.as_mut() else {
      error!("No drive config available for manual partitioning");
      return;
    };
    //config.assign_device_numbers();
    let rows = part_table(config.layout(), config.sector_size())
      .rows()
      .to_vec();
    self.disk_config.set_rows(rows);
    let len = self.disk_config.len() as u16;
    let table_pct = (20 + 5 * len).min(70); // cap at 70%
    let padding  = 70 - table_pct;

    let chunks = split_vert!(
        area,
        1,
        [
            Constraint::Percentage(table_pct),
            Constraint::Percentage(30),
            Constraint::Percentage(padding),
        ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      0,
      [
        Constraint::Percentage(33),
        Constraint::Percentage(34),
        Constraint::Percentage(33),
      ]
    );

    self.disk_config.render(f, chunks[0]);
    self.buttons.render(f, hor_chunks[1]);

    // Render help modal on top
    self.help_modal.render(f, area);
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        return Signal::Wait;
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        return Signal::Wait;
      }
      _ if self.help_modal.visible => {
        return Signal::Wait;
      }
      _ => {}
    }

    if self.confirming_reset && event.code != KeyCode::Enter {
      self.confirming_reset = false;
      self.buttons.set_children_inplace(vec![
        Box::new(Button::new("Confirm and Exit")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("Reset Partition Layout")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("Abort")) as Box<dyn ConfigWidget>,
      ]);
    }
    if self.disk_config.is_focused() {
      match event.code {
        ui_back!() => Signal::PopCount(2),
        ui_up!() => {
          if !self.disk_config.previous_row() {
            self.disk_config.unfocus();
            self.buttons.last_child();
            self.buttons.focus();
          }
          Signal::Wait
        }
        ui_down!() => {
          if !self.disk_config.next_row() {
            self.disk_config.unfocus();
            self.buttons.first_child();
            self.buttons.focus();
          }
          Signal::Wait
        }
        KeyCode::Enter => {
          debug!("Disk config is focused, handling row selection");
          // we have now selected a row in the table
          // now we need to figure out if we are editing a partition or creating one
          let Some(row) = self.disk_config.get_selected_row_info() else {
            return Signal::Error(anyhow::anyhow!("No row selected in disk config table"));
          };
          let Some(start) = row.get_field("start").and_then(|s| s.parse::<u64>().ok()) else {
            return Signal::Error(anyhow::anyhow!(
              "Failed to parse start sector from row: {row:?}"
            ));
          };
          let Some(ref drive) = installer.drive_config else {
            return Signal::Error(anyhow::anyhow!("No drive config available"));
          };
          let layout = drive.layout();
          let Some(item) = layout.iter().rfind(|i| i.start() == start) else {
            return Signal::Error(anyhow::anyhow!(
              "No partition or free space found at start sector {start}"
            ));
          };
          debug!("Selected item: {item:?}");
          match item {
            DiskItem::Partition(part) => Signal::Push(Box::new(AlterPartition::new(part.clone()))),
            DiskItem::FreeSpace { id, start, size } => Signal::Push(Box::new(NewPartition::new(
              *id,
              *start,
              drive.sector_size(),
              *size,
            ))),
          }
        }
        _ => Signal::Wait,
      }
    } else if self.buttons.is_focused() {
      match event.code {
        ui_back!() => Signal::PopCount(2),
        ui_up!() => {
          if !self.buttons.prev_child() {
            self.buttons.unfocus();
            self.disk_config.last_row();
            self.disk_config.focus();
          }
          Signal::Wait
        }
        ui_down!() => {
          if !self.buttons.next_child() {
            self.buttons.unfocus();
            self.disk_config.first_row();
            self.disk_config.focus();
          }
          Signal::Wait
        }
        KeyCode::Enter => {
          let Some(idx) = self.buttons.selected_child() else {
            return Signal::Wait;
          };
          match idx {
            0 => {
              // Confirm and Exit
              installer.make_drive_config_display();
              Signal::Unwind
            }
            1 => {
              if !self.confirming_reset {
                self.confirming_reset = true;
                let new_buttons = vec![
                  Box::new(Button::new("Confirm and Exit")) as Box<dyn ConfigWidget>,
                  Box::new(Button::new("Really?")) as Box<dyn ConfigWidget>,
                  Box::new(Button::new("Abort")) as Box<dyn ConfigWidget>,
                ];
                self.buttons.set_children_inplace(new_buttons);
                Signal::Wait
              } else {
                let Some(ref mut device) = installer.drive_config else {
                  return Signal::Wait;
                };
                device.reset_layout();
                device.assign_device_numbers();
                self.buttons.unfocus();
                self.disk_config.first_row();
                self.disk_config.focus();
                self.confirming_reset = false;
                self.buttons.set_children_inplace(vec![
                  Box::new(Button::new("Confirm and Exit")) as Box<dyn ConfigWidget>,
                  Box::new(Button::new("Reset Partition Layout")) as Box<dyn ConfigWidget>,
                  Box::new(Button::new("Abort")) as Box<dyn ConfigWidget>,
                ]);
                Signal::Wait
              }
            }
            2 => {
              // Abort
              Signal::PopCount(2)
            }
            _ => Signal::Wait,
          }
        }
        _ => Signal::Wait,
      }
    } else {
      self.disk_config.focus();
      self.handle_input(installer, event)
    }
  }

  fn get_help_content(&self) -> (String, Vec<ratatui::text::Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate partitions and buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch between partition table and buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select partition or button action"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Return to previous menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Manually configure drive partitions. Select partitions to",
      )],
      vec![(
        None,
        "modify them or select free space to create new partitions.",
      )],
      vec![(None, "Use buttons at bottom for additional actions.")],
    ]);
    ("Manual Partitioning".to_string(), help_content)
  }
}

pub struct NewPartition {
  pub fs_id: u64,
  pub part_start: u64,
  pub part_end: u64,
  pub sector_size: u64,
  pub total_size: u64, // sectors
  pub max_fit: Option<u64>, // sectors

  pub new_part_size: Option<u64>, // sectors
  pub size_input: LineEditor,

  pub new_part_fs: Option<String>,
  pub fs_buttons: WidgetBox,

  pub new_part_mount_point: Option<String>,
  pub mount_input: LineEditor,

  pub enc_widgets: WidgetBox,
  pub new_part_encrypt: Option<bool>,
  pub enc_selected: bool,
  collecting_pass: bool,
  pass1: LineEditor,
  pass2: LineEditor,
  new_part_luks_password: Option<String>,
}

impl NewPartition {
  pub fn new(fs_id: u64, part_start: u64, sector_size: u64, total_size: u64) -> Self {
    let part_end = part_start + total_size - 1;
    let fs_buttons = {
      let buttons = vec![
        Box::new(Button::new("ext4")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("ext3")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("ext2")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("btrfs")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("xfs")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("fat32")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("ntfs")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("swap")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("don't format")) as Box<dyn ConfigWidget>,
      ];
      let mut button_row = WidgetBox::button_menu(buttons);
      button_row.focus();
      button_row
    };
    let enc_widgets = {
      let buttons = vec![
        Box::new(CheckBox::new("Encryption (LUKS)", false)) as Box<dyn ConfigWidget>,
        Box::new(Button::new("Continue")) as Box<dyn ConfigWidget>,
        Box::new(Button::new("Back")) as Box<dyn ConfigWidget>,
      ];
      let mut row = WidgetBox::button_menu(buttons);
      // di default metto il focus sul checkbox, così Space/Enter toggla subito
      row.focus();
      row
    };
    let mount_input = LineEditor::new("New Partition Mount Point", None::<&str>);
    let mut size_input = LineEditor::new(
      "New Partition Size",
      Some("Empty input uses rest of free space"),
    );
    size_input.focus();
    Self {
      fs_id,
      part_start,
      part_end,
      sector_size,
      total_size,
      max_fit: None,

      new_part_size: None,
      size_input,

      new_part_fs: None,
      fs_buttons,

      new_part_mount_point: None,
      mount_input,

      enc_widgets,
      new_part_encrypt: None,
      enc_selected: false,
      collecting_pass: false,
      pass1: LineEditor::new("LUKS Password", Some("Enter passphrase...")).secret(true),
      pass2: LineEditor::new("Confirm LUKS Password", Some("Re-enter passphrase...")).secret(true),
      new_part_luks_password: None,
    }
  }
  fn dry_run_accepts(&self, installer: &Installer, size_sectors: u64) -> bool {
      let Some(ref dev) = installer.drive_config else { return false; };
      // clone so we don't mutate the real config
      let mut tmp = dev.clone();
      let part = Partition::new(
          self.part_start,
          size_sectors,
          self.sector_size,
          PartStatus::Create,
          None,                 // type
          self.new_part_fs.clone(),
          None,                 // mount point not needed for geometry
          None,                 // label
          false,                // bootable
          vec![],               // flags (geometry only)
      );
      tmp.new_partition(part).is_ok()
  }
  fn compute_max_fit(&mut self, installer: &Installer) -> u64 {
      if let Some(m) = self.max_fit { return m; }
      let mut lo = 1;
      let mut hi = self.total_size;
      let mut best = 0;
      while lo <= hi {
          let mid = (lo + hi) / 2;
          if self.dry_run_accepts(installer, mid) {
              best = mid;
              lo = mid + 1;
          } else {
              hi = mid - 1;
          }
      }
      self.max_fit = Some(best);
      best
  }
  fn finalize_new_partition(&mut self, installer: &mut Installer) -> Signal {
      let Some(ref mut device) = installer.drive_config else {
          // still bail here — this is a real internal error
          return Signal::Error(anyhow::anyhow!(
              "No drive config available when finalizing new partition"
          ));
      };

      let is_swap = self.new_part_fs.as_deref() == Some("swap");
      let luks_path = "/run/luks";

      let mut flags = if !is_swap && self.new_part_mount_point.as_deref() == Some("/efi") {
          vec!["boot".to_string(), "esp".to_string()]
      } else if !is_swap && self.new_part_mount_point.as_deref() == Some("/boot") {
          vec!["bls_boot".to_string()]
      } else {
          Vec::new() // Default case to ensure `flags` is always a Vec<String>
      };
      if self.new_part_encrypt.unwrap_or(false) {
          flags.push("encrypt".to_string());
      }
      let Some(size) = self.new_part_size else {
          // user-facing validation should have set this already
          return Signal::Error(anyhow::anyhow!(
              "No new partition size specified when finalizing new partition"
          ));
      };

      let auto_label = if is_swap {
        Some("SWAP".to_string())
      } else {
        match self.new_part_mount_point.as_deref() {
          Some("/efi") => Some("ESP".to_string()),
          Some("/boot") => Some("XBOOTLDR".to_string()),
          Some("/")     => Some("ROOT".to_string()),
          _             => None,
        }
      };

      // swap must not have a mount point
      let mp = if is_swap { None } else { self.new_part_mount_point.clone() };

      let new_part = Partition::new(
          self.part_start,
          size,
          self.sector_size,
          PartStatus::Create,
          None,
          self.new_part_fs.clone(),
          mp,
          auto_label,
          false,
          flags,
      );

      if self.new_part_encrypt == Some(true)
        && let Some(pw) = &self.new_part_luks_password
          && let Err(e) = write_luks_key_to_tmpfs(pw, luks_path) {
            return Signal::Error(anyhow::anyhow!("Failed to write {luks_path}: {e}"));
          }

      match device.new_partition(new_part) {
        Ok(_) => { device.assign_device_numbers(); Signal::Pop }
        Err(e) => {
          self.size_input.error(format!("Failed to create partition: {e}"));
          self.new_part_size = None;           // return to size screen
          self.fs_buttons.unfocus();
          self.mount_input.unfocus();
          self.enc_widgets.unfocus();
          self.size_input.focus();
          Signal::Wait
        }
      }
  }
  pub fn total_size_bytes(&self) -> u64 {
    self.total_size * self.sector_size
  }
  fn live_max_size_sectors(&self, installer: &Installer) -> Option<u64> {
    let drive = installer.drive_config.as_ref()?;
    // find the free-space item we were created from (by id & start)
    drive
      .layout()
      .iter()
      .find_map(|it| match it {
        DiskItem::FreeSpace { id, start, size } if *id == self.fs_id && *start == self.part_start => Some(*size),
        _ => None,
      })
      .or(Some(self.total_size)) // fallback to initial snapshot
  }
  pub fn render_size_input(&mut self, f: &mut Frame, area: Rect, max_fit: u64) {
    let chunks = split_vert!(area, 1, [
        Constraint::Percentage(40),
        Constraint::Length(7),
        Constraint::Percentage(40),
    ]);
    let hor_chunks = split_hor!(chunks[1], 1, [
        Constraint::Percentage(33),
        Constraint::Percentage(34),
        Constraint::Percentage(33),
    ]);

    let max_bytes = (max_fit as u128) * (self.sector_size as u128);
    let max_human  = bytes_readable_floor(max_bytes);

    let info_box = InfoBox::new(
        "Free Space Info",
        styled_block(vec![
            vec![(HIGHLIGHT, "Sector Size: "), (None, &format!("{}", self.sector_size))],
            vec![(HIGHLIGHT, "Partition Start Sector: "), (None, &format!("{}", self.part_start))],
            vec![(HIGHLIGHT, "Partition End Sector: "), (None, &format!("{}", self.part_end))],
            vec![(HIGHLIGHT, "Max allocatable now: "),
                 (None, &format!("{max_human} ({max_fit} sectors)"))],
            vec![(None, "")],
            vec![(None, "Enter the desired size...")],
            vec![(None, "Examples: "), (Some((Color::Green, Modifier::BOLD)), "10GiB"),
                 (None, ", "), (Some((Color::Green, Modifier::BOLD)), "500MiB"),
                 (None, ", "), (Some((Color::Green, Modifier::BOLD)), "100%")],
        ]),
    );
    info_box.render(f, chunks[0]);
    self.size_input.render(f, hor_chunks[1]);
  }
  pub fn handle_input_encryption_select(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      ui_up!() => { self.enc_widgets.prev_child(); Signal::Wait }
      ui_down!() => { self.enc_widgets.next_child(); Signal::Wait }
      KeyCode::Esc => {
        if self.new_part_fs.as_deref() == Some("swap") {
          // go back to filesystem selection when editing swap
          self.new_part_encrypt = None;
          self.enc_widgets.unfocus();
          self.new_part_fs = None;
          self.fs_buttons.first_child();
          self.fs_buttons.focus();
          return Signal::Wait;
        }
        self.enc_widgets.unfocus();
        self.new_part_mount_point = None;
        self.mount_input = LineEditor::new("New Partition Mount Point", None::<&str>);
        self.mount_input.focus();
        Signal::Wait
      }
      KeyCode::Char(' ') => {
          if let Some(child) = self.enc_widgets.focused_child_mut() {
              child.interact();
              if let Some(Value::Bool(checked)) = child.get_value() {
                  self.enc_selected = checked;
              }
          }
          Signal::Wait
      }
      KeyCode::Enter => {
        let Some(idx) = self.enc_widgets.selected_child() else { return Signal::Wait; };
        match idx {
          0 => {
            // Toggle checkbox
            if let Some(child) = self.enc_widgets.focused_child_mut() {
              child.interact();
              if let Some(Value::Bool(checked)) = child.get_value() {
                self.enc_selected = checked;
              }
            }
            Signal::Wait
          }
          1 => { // Continue
            self.new_part_encrypt = Some(self.enc_selected);
            if self.enc_selected {
              // ask for password inline
              self.collecting_pass = true;
              self.pass1.focus();
              return Signal::Wait;
            }
            // no encryption -> finalize right away
            self.finalize_new_partition(installer)
          }
          2 => {
            // Back
            self.enc_widgets.unfocus();
            self.new_part_mount_point = None;
            self.mount_input = LineEditor::new("New Partition Mount Point", None::<&str>);
            self.mount_input.focus();
            Signal::Wait
          }
          _ => Signal::Wait,
        }
      }
      _ => Signal::Wait,
    }
  }
  pub fn handle_input_size(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        let input = self.size_input.get_value().unwrap();
        let mut input = input.as_str().unwrap().trim();
        if input.is_empty() { input = "100%"; }

        let Some(ref device) = installer.drive_config else {
          return Signal::Error(anyhow::anyhow!("No drive config available for new partition size input"));
        };

        let max_fit = self.compute_max_fit(installer);

        match parse_sectors(input, device.sector_size(), self.total_size) {
          Some(0) => {
            self.size_input.error("Invalid size (must be > 0)");
            Signal::Wait
          }
          Some(size) if size > max_fit => {
            self.size_input.error(format!(
              "Too large. Max allocatable here is {} ({} sectors).",
              bytes_readable_floor(max_fit as u128 * self.sector_size as u128),
              max_fit
            ));
            Signal::Wait
          }
          Some(size) => {
            // one more belt-and-suspenders check using the allocator
            if !self.dry_run_accepts(installer, size) {
              self.size_input.error("Invalid size at this location: New partition overlaps with existing");
              return Signal::Wait;
            }
            self.new_part_size = Some(size);
            self.size_input.unfocus();
            self.fs_buttons.focus();
            Signal::Wait
          }
          None => {
            self.size_input.error("Invalid size input");
            Signal::Wait
          }
        }
      }
      _ => self.size_input.handle_input(event),
    }
  }
  pub fn render_encryption_select(&mut self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [Constraint::Percentage(70), Constraint::Percentage(30)]
    );

    let info_box = InfoBox::new(
      "Encryption",
      styled_block(vec![
        vec![(None, "Choose whether to encrypt this partition with "), (HIGHLIGHT, "LUKS"), (None, ".")],
        vec![(None, "If enabled, the installer will create a "), (HIGHLIGHT, "LUKS container"), (None, " and put the filesystem inside it.")],
        vec![(None, "You can use this for "), (HIGHLIGHT, "root (/), /home, /var"), (None, " ecc. ")],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.enc_widgets.render(f, chunks[1]);
  }

  fn render_password_input(&mut self, f: &mut Frame, area: Rect) {
    // Match PromptLuksPassword look: centered 60% column, two rows of height 5
    let hor = split_hor!(area, 1, [
      Constraint::Percentage(20),
      Constraint::Percentage(60),
      Constraint::Percentage(20),
    ]);
    let col = split_vert!(hor[1], 1, [
      Constraint::Length(5), // pass
      Constraint::Length(5), // confirm
      Constraint::Min(0),
    ]);

    self.pass1.render(f, col[0]);
    self.pass2.render(f, col[1]);
  }

  fn handle_input_password(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    use KeyCode::*;
    match event.code {
      Esc => {
        // back to the encryption checkbox instead of discarding encrypt choice
        self.collecting_pass = false;
        self.enc_widgets.focus();
        Signal::Wait
      }
      Tab => {
        // forward cycle: pass1 -> pass2 -> pass1
        if self.pass1.is_focused() {
          let v = self.pass1
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          if let Err(msg) = validate_passphrase(&v) {
            self.pass1.error(msg);
            return Signal::Wait;
          }
          self.pass1.clear_error();
          self.pass1.unfocus();
          self.pass2.focus();
        } else if self.pass2.is_focused() {
          self.pass2.unfocus();
          self.pass1.focus();
        } else {
          self.pass1.focus();
        }
        Signal::Wait
      }
      BackTab => {
        // backward cycle: pass2 -> pass1 -> pass2
        if self.pass2.is_focused() {
          self.pass2.unfocus();
          self.pass1.focus();
        } else if self.pass1.is_focused() {
          self.pass1.unfocus();
          self.pass2.focus();
        } else {
          self.pass1.focus();
        }
        Signal::Wait
      }
      Enter => {
        // Enter should behave like "Next" when on first field and "Confirm" when on second
        if self.pass1.is_focused() {
          let v = self.pass1
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          let v = v.trim();
          if v.len() < 8 {
            self.pass1.error("Passphrase must be at least 8 characters.");
            return Signal::Wait;
          }
          self.pass1.clear_error();
          self.pass1.unfocus();
          self.pass2.focus();
          return Signal::Wait;
        }

        if self.pass2.is_focused() {
          let p1 = self.pass1
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          let p2 = self.pass2
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          let p1 = p1.trim();
          let p2 = p2.trim();

          if p2.is_empty() {
            self.pass2.error("Confirmation cannot be empty.");
            return Signal::Wait;
          }
          if p1.len() < 8 {
            // belt and suspenders in case user tabbed back
            self.pass1.error("Passphrase must be at least 8 characters.");
            self.pass2.unfocus();
            self.pass1.focus();
            return Signal::Wait;
          }
          if p1 != p2 {
            self.pass2.clear();
            self.pass2.error("Passphrases do not match.");
            return Signal::Wait;
          }

          self.new_part_luks_password = Some(p1.to_string());
          self.collecting_pass = false;

          // proceed to finalization
          return self.finalize_new_partition(installer);
        }

        // If neither has focus yet, give it to the first editor
        self.pass1.focus();
        Signal::Wait
      }
      _ => {
        // Delegate other keys to the currently focused editor
        if self.pass1.is_focused() {
          self.pass1.handle_input(event)
        } else if self.pass2.is_focused() {
          self.pass2.handle_input(event)
        } else {
          self.pass1.focus();
          Signal::Wait
        }
      }
    }
  }

  pub fn render_fs_select(&mut self, f: &mut Frame, area: Rect) {
    let vert_chunks = split_vert!(
      area,
      1,
      [Constraint::Percentage(50), Constraint::Percentage(50)]
    );
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(40),
        Constraint::Percentage(20),
        Constraint::Percentage(40),
      ]
    );

    let idx = self.fs_buttons.selected_child().unwrap_or(11);
    let info_box = SelectFilesystem::get_fs_info(self.fs_buttons.selected_child().unwrap_or(11));
    self.fs_buttons.render(f, hor_chunks[1]);
    if idx < 11 {
      info_box.render(f, vert_chunks[1]);
    }
  }
  pub fn handle_input_fs_select(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      ui_back!() => {
        self.new_part_size = None;
        self.fs_buttons.unfocus();
        self.size_input = LineEditor::new("New Partition Size", Some("Empty input uses rest of free space"));
        self.size_input.focus();
        Signal::Wait
      }
      ui_up!() => {
        self.fs_buttons.prev_child();
        Signal::Wait
      }
      ui_down!() => {
        self.fs_buttons.next_child();
        Signal::Wait
      }
      KeyCode::Enter => {
        let Some(idx) = self.fs_buttons.selected_child() else {
          return Signal::Wait;
        };
        let fs = match idx {
          0 => "ext4",
          1 => "ext3",
          2 => "ext2",
          3 => "btrfs",
          4 => "xfs",
          5 => "fat32",
          6 => "ntfs",
          7 => "swap",
          8 => "don't format",
          _ => return Signal::Wait,
        }
        .to_string();

        self.new_part_fs = Some(fs);
        self.fs_buttons.unfocus();

        // If swap, go straight to encryption (or finalize if you prefer to skip encryption UI too)
        if self.new_part_fs.as_deref() == Some("swap") {
          self.enc_widgets.first_child();
          self.enc_widgets.focus();
          return Signal::Wait;
        }        
        self.mount_input.focus();
        Signal::Wait
      }
      _ => Signal::Wait,
    }
  }
  pub fn render_mount_point_input(&mut self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(70),
        Constraint::Length(7),
        Constraint::Percentage(25),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(33),
        Constraint::Percentage(34),
        Constraint::Percentage(33),
      ]
    );

    let info_box = InfoBox::new(
      "Mount Point Info",
      styled_block(vec![
        vec![(
          None,
          "Enter the mount point for the new partition. This is the directory where the partition will be mounted in the filesystem.",
        )],
        vec![
          (None, "Common mount points include "),
          (Some((Color::Green, Modifier::BOLD)), "/"),
          (None, " for root, "),
          (Some((Color::Green, Modifier::BOLD)), "/home"),
          (None, " for user data, "),
          (Some((Color::Green, Modifier::BOLD)), "/boot"),
          (None, " for boot files, "),
          (Some((Color::Green, Modifier::BOLD)), "/efi"),
          (None, " for EFI boot files, and "),
          (Some((Color::Green, Modifier::BOLD)), "/var"),
          (None, " for variable data."),
        ],
        vec![(None, "You can also specify other mount points as needed.")],
        vec![(None, "")],
        vec![
          (None, "Examples: "),
          (Some((Color::Green, Modifier::BOLD)), "/"),
          (None, ", "),
          (Some((Color::Green, Modifier::BOLD)), "/home"),
          (None, ", "),
          (Some((Color::Green, Modifier::BOLD)), "/mnt/data"),
        ],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.mount_input.render(f, hor_chunks[1]);
  }
  pub fn handle_input_mount_point(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      ui_back!() => {
        self.new_part_mount_point = None;
        self.mount_input.unfocus();
        self.new_part_fs = None;            // <- chiave per far renderizzare la schermata FS
        self.fs_buttons.first_child();
        self.fs_buttons.focus();
        Signal::Wait
      }
      KeyCode::Enter => {
        let input = self.mount_input.get_value().unwrap();
        let input = input.as_str().unwrap().trim();
        let Some(ref mut device) = installer.drive_config else {
          return Signal::Error(anyhow::anyhow!(
            "No drive config available for new partition mount point input"
          ));
        };
        let taken_mounts: Vec<String> = device
            .partitions()
            .filter(|p| *p.status() != PartStatus::Delete)
            .filter_map(|p| p.mount_point().map(|s| s.to_string()))
            .collect();

        if let Err(err) = SetMountPoint::validate_mount_point(input, &taken_mounts) {
          self.mount_input.error(&err);
          return Signal::Wait;
        }

        // Store the mount point
        self.new_part_mount_point = Some(input.to_string());
        self.mount_input.unfocus();

        // If /boot or /efi, don't ask for encryption
        if self.new_part_mount_point.as_deref() == Some("/boot") || self.new_part_mount_point.as_deref() == Some("/efi") {
          self.new_part_encrypt = Some(false);
          return self.finalize_new_partition(installer);
        }

        // Otherwise ask the encryption
        self.enc_widgets.first_child();
        self.enc_widgets.focus();
        Signal::Wait
      }
      _ => self.mount_input.handle_input(event),
    }
  }
}

impl Page for NewPartition {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect) {
    // compute max allocatable size right now
    let max_fit = self
      .live_max_size_sectors(installer)
      .unwrap_or_else(|| self.compute_max_fit(installer));

    if self.new_part_size.is_none() {
      self.render_size_input(f, area, max_fit);
    } else if self.new_part_fs.is_none() {
      self.render_fs_select(f, area);
    } else if self.new_part_fs.as_deref() == Some("swap") && self.new_part_encrypt.is_none() {
      self.render_encryption_select(f, area);
    } else if self.collecting_pass {
      self.render_password_input(f, area);
    } else if self.new_part_mount_point.is_none() {
      self.render_mount_point_input(f, area);
    } else if self.new_part_encrypt.is_none() && (self.new_part_mount_point.as_deref() != Some("/boot") && self.new_part_mount_point.as_deref() != Some("/efi")) {
      self.render_encryption_select(f, area);
    }
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    if self.new_part_size.is_none() {
      self.handle_input_size(installer, event)
    } else if self.new_part_fs.is_none() {
      self.handle_input_fs_select(installer, event)
    } else if self.new_part_fs.as_deref() == Some("swap") && self.new_part_encrypt.is_none() {
      self.handle_input_encryption_select(installer, event)
    } else if self.collecting_pass {
      self.handle_input_password(installer, event)
    } else if self.new_part_mount_point.is_none() {
      self.handle_input_mount_point(installer, event)
    } else if self.new_part_encrypt.is_none() && (self.new_part_mount_point.as_deref() != Some("/boot") && self.new_part_mount_point.as_deref() != Some("/efi")) {
      self.handle_input_encryption_select(installer, event)
    } else {
      Signal::Pop
    }
  }
}

pub struct AlterPartition {
  pub buttons: WidgetBox,
  pub part_id: u64,
  pub part_status: PartStatus,
  pub is_swap: bool,
}

impl AlterPartition {
  pub fn new(part: Partition) -> Self {
    let part_status = part.status();
    let is_swap = matches!(part.fs_type(), Some("swap"));
    let buttons = Self::buttons_by_status(*part_status, part.flags(), is_swap);
    let mut button_row = WidgetBox::button_menu(buttons);
    button_row.focus();
    Self {
      buttons: button_row,
      part_id: part.id(),
      part_status: *part_status,
      is_swap,
    }
  }
  /// Checkbox index map (non-swap, Modify/Create):
  ///   1 = boot, 2 = esp, 3 = bls_boot, 4 = encrypt
  /// Checkbox index map (swap, Modify/Create):
  ///   0 = encrypt
  fn toggle_current_checkbox(&mut self, device: &mut crate::drives::Disk) {
    // works for both Enter & Space when a checkbox item is focused
    if let Some(child) = self.buttons.focused_child_mut() {
      child.interact();
      if let Some(Value::Bool(checked)) = child.get_value()
        && let Some(part) = device.partition_by_id_mut(self.part_id) {
          // which checkbox are we on?
          let idx = self.buttons.selected_child().unwrap_or(usize::MAX);
          if self.is_swap {
            // Modify/Create (swap): only one checkbox at index 0 = encrypt
            if idx == 0 {
              if checked { part.add_flag("encrypt"); } else { part.remove_flag("encrypt"); }
            }
          } else {
            // Modify/Create (non-swap): boot(1) esp(2) bls_boot(3) encrypt(4)
            match idx {
              1 => if checked { part.add_flag("boot");     } else { part.remove_flag("boot");     },
              2 => if checked { part.add_flag("esp");      } else { part.remove_flag("esp");      },
              3 => if checked { part.add_flag("bls_boot"); } else { part.remove_flag("bls_boot"); },
              4 => if checked { part.add_flag("encrypt");  } else { part.remove_flag("encrypt");  },
              _ => {}
            }
          }
        }
    }
  }
  fn buttons_by_status(
    status: PartStatus,
    flags: &[String],
    is_swap: bool,
  ) -> Vec<Box<dyn ConfigWidget>> {
    if is_swap {
      match status {
        PartStatus::Exists => vec![
          Box::new(Button::new(
            "Mark For Modification (data will be wiped on install)",
          )),
          Box::new(Button::new("Back")),
        ],
        PartStatus::Modify => vec![
          Box::new(CheckBox::new("Encryption (LUKS)", flags.contains(&"encrypt".into()))),
          Box::new(Button::new("Change Filesystem")),
          Box::new(Button::new("Set Label")),
          Box::new(Button::new("Unmark for modification")),
          Box::new(Button::new("Delete Partition")),
          Box::new(Button::new("Back")),
        ],
        PartStatus::Create => vec![
          Box::new(CheckBox::new("Encryption (LUKS)", flags.contains(&"encrypt".into()))),
          Box::new(Button::new("Change Filesystem")),
          Box::new(Button::new("Set Label")),
          Box::new(Button::new("Delete Partition")),
          Box::new(Button::new("Back")),
        ],
        _ => vec![Box::new(Button::new("Back"))],
      }
    } else {
      match status {
        PartStatus::Exists => vec![
          Box::new(Button::new(
            "Mark For Modification (data will be wiped on install)",
          )),
          Box::new(Button::new("Back")),
        ],
        PartStatus::Modify => vec![
          Box::new(Button::new("Set Mount Point")),
          Box::new(CheckBox::new("Mark as bootable partition", flags.contains(&"boot".into()))),
          Box::new(CheckBox::new("Mark as ESP partition",      flags.contains(&"esp".into()))),
          Box::new(CheckBox::new("Mark as XBOOTLDR partition", flags.contains(&"bls_boot".into()))),
          Box::new(CheckBox::new("Encryption (LUKS)",          flags.contains(&"encrypt".into()))),
          Box::new(Button::new("Change Filesystem")),
          Box::new(Button::new("Set Label")),
          Box::new(Button::new("Unmark for modification")),
          Box::new(Button::new("Delete Partition")),
          Box::new(Button::new("Back")),
        ],
        PartStatus::Create => vec![
          Box::new(Button::new("Set Mount Point")),
          Box::new(CheckBox::new("Mark as bootable partition", flags.contains(&"boot".into()))),
          Box::new(CheckBox::new("Mark as ESP partition",      flags.contains(&"esp".into()))),
          Box::new(CheckBox::new("Mark as XBOOTLDR partition", flags.contains(&"bls_boot".into()))),
          Box::new(CheckBox::new("Encryption (LUKS)",          flags.contains(&"encrypt".into()))),
          Box::new(Button::new("Change Filesystem")),
          Box::new(Button::new("Set Label")),
          Box::new(Button::new("Delete Partition")),
          Box::new(Button::new("Back")),
        ],
        _ => vec![Box::new(Button::new("Back"))],
      }
    }
  }
  fn render_existing_part(&self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(70), Constraint::Percentage(30)]);
    let info_box = if self.is_swap {
      InfoBox::new(
        "Alter Existing Swap Partition",
        styled_block(vec![
          vec![(None, "Choose an action to perform on the selected swap partition.")],
          vec![(None, "- "),
              (Some((Color::Green, Modifier::BOLD)), "'Mark For Modification'"),
              (None, " will flag this partition to be reformatted during installation.")],
        ]),
      )
    } else {
      InfoBox::new(
        "Alter Existing Partition",
        styled_block(vec![
          vec![(None, "Choose an action to perform on the selected partition.")],
          vec![(None, "- "), (Some((Color::Green, Modifier::BOLD)), "'Mark For Modification'"),
              (None, " will flag this partition to be reformatted during installation.")],
        ]),
      )
    };
    info_box.render(f, chunks[0]);
    self.buttons.render(f, chunks[1]);
  }

  fn render_modify_part(&self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(70), Constraint::Percentage(30)]);
    let info_box = if self.is_swap {
      InfoBox::new(
        "Alter Swap (Marked for Modification)",
        styled_block(vec![
          vec![(None, "This swap partition is marked for modification. You can enable relabel, or delete it.")],
        ]),
      )
    } else {
      InfoBox::new(
        "Alter Partition (Marked for Modification)",
        styled_block(vec![
          vec![(None, "This partition is marked for modification. You can change its mount point, flags, filesystem, or delete it.")],
        ]),
      )
    };
    info_box.render(f, chunks[0]);
    self.buttons.render(f, chunks[1]);
  }

  fn render_delete_part(&self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(70), Constraint::Percentage(30)]);
    let info_box = InfoBox::new(
      "Deleted Partition",
      styled_block(vec![
        vec![(None, "This partition has been marked for deletion.")],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.buttons.render(f, chunks[1]);
  }

  fn refresh_menu_from_device(&mut self, device: &mut crate::drives::Disk) {
    if let Some(part) = device.partition_by_id(self.part_id) {
      self.part_status = *part.status();
      self.is_swap = matches!(part.fs_type(), Some("swap"));
      let buttons = Self::buttons_by_status(self.part_status, part.flags(), self.is_swap);
      let mut new_box = WidgetBox::button_menu(buttons);
      new_box.first_child();
      new_box.focus();
      self.buttons = new_box;
    }
  }
}

impl Page for AlterPartition {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    match &self.part_status {
      PartStatus::Exists => self.render_existing_part(f, area),
      PartStatus::Modify | PartStatus::Create => self.render_modify_part(f, area),
      PartStatus::Delete => self.render_delete_part(f, area),
      _ => {
        let info_box = InfoBox::new(
          "Alter Partition",
          styled_block(vec![vec![(None, "Unknown status for this partition.")]]),
        );
        info_box.render(f, area);
      }
    }
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    let is_checkbox_toggle_key = matches!(event.code, KeyCode::Char(' '));

    match event.code {
      ui_back!() => Signal::Pop,
      ui_up!() => { self.buttons.prev_child(); Signal::Wait}
      ui_down!() => { self.buttons.next_child(); Signal::Wait}

      // Space toggles checkboxes
      _ if is_checkbox_toggle_key => {
        if let Some(ref mut device) = installer.drive_config {
          self.toggle_current_checkbox(device);
          if let Some(p) = device.partition_by_id_mut(self.part_id) {
            let now_enc = p.flags().iter().any(|f| f == "encrypt");
            if now_enc {
              return Signal::Push(Box::new(PromptLuksPassword::new(self.part_id).with_pop_count(1)));
            }
          }
        }
        Signal::Wait
      }

      ui_enter!() => {
        if self.part_status == PartStatus::Delete {
          return Signal::Pop;
        }
        let Some(idx) = self.buttons.selected_child() else { return Signal::Wait; };
        let Some(ref mut device) = installer.drive_config else {
          return Signal::Error(anyhow::anyhow!("No drive config available for altering partition"));
        };

        match self.part_status {
          PartStatus::Exists => {
              let Some(part) = device.partition_by_id_mut(self.part_id) else {
                  return Signal::Error(anyhow::anyhow!("No partition found with id {}", self.part_id));
              };
              match idx {
                  0 => { // Mark for modification
                      part.set_status(PartStatus::Modify);
                      self.refresh_menu_from_device(device);
                      Signal::Wait
                  }
                  1 => Signal::Pop, // Back
                  _ => Signal::Wait,
              }
          }

          PartStatus::Modify => {
            if self.is_swap {
              match idx {
                0 => { // Encrypt toggle
                  self.toggle_current_checkbox(device);
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    let now_enc = p.flags().iter().any(|f| f == "encrypt");
                    if now_enc {
                      return Signal::Push(Box::new(PromptLuksPassword::new(self.part_id).with_pop_count(1)));
                    }
                  }
                  Signal::Wait
                }
                1 => Signal::Push(Box::new(SelectFilesystem::new(Some(self.part_id)))), // Change FS
                2 => Signal::Push(Box::new(SetLabel::new(self.part_id))),
                3 => { // Unmark
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    p.set_status(PartStatus::Exists);
                  }
                  self.refresh_menu_from_device(device);
                  Signal::Wait
                }
                4 => { // Delete
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    p.set_status(PartStatus::Delete);
                    device.calculate_free_space();
                  }
                  Signal::Pop
                }
                5 => Signal::Pop, // Back
                _ => Signal::Wait,
              }
            } else {
              match idx {
                0 => Signal::Push(Box::new(SetMountPoint::new(self.part_id))),
                1..=4 => { // checkboxes
                  self.toggle_current_checkbox(device);
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    let now_enc = p.flags().iter().any(|f| f == "encrypt");
                    if now_enc {
                      return Signal::Push(Box::new(PromptLuksPassword::new(self.part_id).with_pop_count(1)));
                    }
                  }
                  Signal::Wait
                }
                5 => Signal::Push(Box::new(SelectFilesystem::new(Some(self.part_id)))), // Change FS
                6 => Signal::Push(Box::new(SetLabel::new(self.part_id))),
                7 => { // Unmark
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    p.set_status(PartStatus::Exists);
                  }
                  self.refresh_menu_from_device(device);
                  Signal::Wait
                }
                8 => { // Delete
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    p.set_status(PartStatus::Delete);
                    device.calculate_free_space();
                  }
                  Signal::Pop
                }
                9 => Signal::Pop, // Back
                _ => Signal::Wait,
              }
            }
          }

          PartStatus::Create => {
            if self.is_swap {
              match idx {
                0 => { // Encrypt
                  self.toggle_current_checkbox(device);
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    let now_enc = p.flags().iter().any(|f| f == "encrypt");
                    if now_enc {
                      return Signal::Push(Box::new(PromptLuksPassword::new(self.part_id).with_pop_count(1)));
                    }
                  }
                  Signal::Wait
                }
                1 => Signal::Push(Box::new(SelectFilesystem::new(Some(self.part_id)))), // Change FS
                2 => Signal::Push(Box::new(SetLabel::new(self.part_id))),
                3 => { // Delete (remove from layout)
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    p.set_status(PartStatus::Delete);
                  }
                  if let Err(e) = device.remove_partition(self.part_id) {
                    return Signal::Error(anyhow::anyhow!("{e}"));
                  }
                  Signal::Pop
                }
                4 => Signal::Pop, // Back
                _ => Signal::Wait,
              }
            } else {
              match idx {
                0 => Signal::Push(Box::new(SetMountPoint::new(self.part_id))),
                1..=4 => { // checkboxes
                  self.toggle_current_checkbox(device);
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    let now_enc = p.flags().iter().any(|f| f == "encrypt");
                    if now_enc {
                      return Signal::Push(Box::new(PromptLuksPassword::new(self.part_id).with_pop_count(1)));
                    }
                  }
                  Signal::Wait
                }
                5 => Signal::Push(Box::new(SelectFilesystem::new(Some(self.part_id)))), // Change FS
                6 => Signal::Push(Box::new(SetLabel::new(self.part_id))),
                7 => { // Delete (remove from layout)
                  if let Some(p) = device.partition_by_id_mut(self.part_id) {
                    p.set_status(PartStatus::Delete);
                  }
                  if let Err(e) = device.remove_partition(self.part_id) {
                    return Signal::Error(anyhow::anyhow!("{e}"));
                  }
                  Signal::Pop
                }
                8 => Signal::Pop, // Back
                _ => Signal::Wait,
              }
            }
          }

          _ => Signal::Wait,
        }
      }
      _ => Signal::Wait,
    }
  }
}

fn validate_passphrase(p: &str) -> Result<(), &'static str> {
    if p.trim().len() < 8 {
        Err("Passphrase must be at least 8 characters.")
    } else {
        Ok(())
    }
}

pub struct PromptLuksPassword {
  dev_id: u64,
  pass_input: LineEditor,
  pass_confirm: LineEditor,
  help_modal: HelpModal<'static>,
  pop_count_on_success: usize,
}

impl PromptLuksPassword {
  pub fn new(dev_id: u64) -> Self {
    let mut pass_input = LineEditor::new("LUKS Passphrase", None::<&str>).secret(true);
    pass_input.focus();
    let pass_confirm = LineEditor::new("Confirm LUKS Passphrase", None::<&str>).secret(true);

    let help_content = styled_block(vec![
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Tab"), (None, " - Next field")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Shift+Tab"), (None, " - Previous field")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Enter"), (None, " - Continue / Confirm")],
      vec![(Some((Color::Yellow, Modifier::BOLD)), "Esc"), (None, " - Cancel")],
      vec![(None, "")],
      vec![(None, "Enter and confirm the LUKS passphrase.")],
      vec![(None, "It will be serialized into the installer JSON (not hashed).")],
    ]);
    let help_modal = HelpModal::new("LUKS Passphrase", help_content);

    Self { dev_id, pass_input, pass_confirm, help_modal, pop_count_on_success: 1 }
  }

  pub fn with_pop_count(mut self, n: usize) -> Self {
    self.pop_count_on_success = n;
    self
  }

  fn cycle_forward(&mut self) {
    if self.pass_input.is_focused() {
      // use the shared validator before moving on
      let v = self.pass_input.get_value()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
      if let Err(msg) = validate_passphrase(&v) {
        self.pass_input.error(msg);
        return;
      }
      self.pass_input.clear_error();
      self.pass_input.unfocus();
      self.pass_confirm.focus();
    } else if self.pass_confirm.is_focused() {
      // just wrap
      self.pass_confirm.unfocus();
      self.pass_input.focus();
    } else {
      self.pass_input.focus();
    }
  }

  fn cycle_backward(&mut self) {
    if self.pass_input.is_focused() {
      self.pass_input.unfocus();
      self.pass_confirm.focus();
    } else if self.pass_confirm.is_focused() {
      self.pass_confirm.unfocus();
      self.pass_input.focus();
    } else {
      self.pass_input.focus();
    }
  }
}

impl Page for PromptLuksPassword {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let hor = split_hor!(area, 1, [
      Constraint::Percentage(20),
      Constraint::Percentage(60),
      Constraint::Percentage(20),
    ]);
    let col = split_vert!(hor[1], 1, [
      Constraint::Length(5), // pass
      Constraint::Length(5), // confirm
      Constraint::Min(0),
    ]);

    self.pass_input.render(f, col[0]);
    self.pass_confirm.render(f, col[1]);

    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => { self.help_modal.toggle(); return Signal::Wait; }
      ui_close!() if self.help_modal.visible => { self.help_modal.hide(); return Signal::Wait; }
      _ if self.help_modal.visible => return Signal::Wait,
      KeyCode::Esc => return Signal::Pop,
      KeyCode::Tab => { self.cycle_forward(); return Signal::Wait; }
      KeyCode::BackTab => { self.cycle_backward(); return Signal::Wait; }
      _ => {}
    }

    if self.pass_input.is_focused() {
      match event.code {
        KeyCode::Enter => {
          let v = self.pass_input.get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          if let Err(msg) = validate_passphrase(&v) {
            self.pass_input.error(msg);
            return Signal::Wait;
          }
          self.pass_input.clear_error();
          self.pass_input.unfocus();
          self.pass_confirm.focus();
          Signal::Wait
        }
        _ => self.pass_input.handle_input(event),
      }
    } else if self.pass_confirm.is_focused() {
      match event.code {
        KeyCode::Enter => {
          let pass = self.pass_input.get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
          let confirm = self.pass_confirm.get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

          if let Err(msg) = validate_passphrase(&pass) {
            self.pass_input.error(msg);
            self.pass_confirm.unfocus();
            self.pass_input.focus();
            return Signal::Wait;
          }
          if confirm.trim().is_empty() {
            self.pass_confirm.error("Confirmation cannot be empty");
            return Signal::Wait;
          }
          if pass != confirm {
            self.pass_confirm.clear();
            self.pass_confirm.error("Passphrases do not match");
            return Signal::Wait;
          }

          // success: store encrypt flag and write key
          if let Some(dev) = installer.drive_config.as_mut()
            && let Some(p) = dev.partition_by_id_mut(self.dev_id)
          {
            p.add_flag("encrypt");
          }
          let luks_path = "/run/luks";
          if let Err(e) = write_luks_key_to_tmpfs(&pass, luks_path) {
            return Signal::Error(anyhow::anyhow!("Failed to write {luks_path}: {e}"));
          }
          installer.make_drive_config_display();
          Signal::PopCount(self.pop_count_on_success)
        }
        _ => self.pass_confirm.handle_input(event),
      }
    } else {
      self.pass_input.focus();
      Signal::Wait
    }
  }

  fn get_help_content(&self) -> (String, Vec<ratatui::text::Line<'_>>) {
    ("LUKS Passphrase".to_string(),
     styled_block(vec![
       vec![(None, "Enter passphrase, press Enter, confirm, press Enter.")],
       vec![(None, "Esc to cancel.")],
     ]))
  }
}

struct AskEncryptRoot {
  root_id: u64,
  buttons: WidgetBox, // [☐ Encrypt root, Continue, Back]
  help: HelpModal<'static>,
}

impl AskEncryptRoot {
  fn new(root_id: u64) -> Self {
    let mut buttons = WidgetBox::button_menu(vec![
      Box::new(CheckBox::new("Encrypt ROOT (LUKS)", false)) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Continue")) as Box<dyn ConfigWidget>,
      Box::new(Button::new("Back")) as Box<dyn ConfigWidget>,
    ]);
    buttons.focus();
    let help = HelpModal::new("Encrypt ROOT", crate::styled_block(vec![
      vec![(Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)), "↑/↓"), (None, " navigate")],
      vec![(Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)), "Space"), (None, " toggle")],
      vec![(Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)), "Enter"), (None, " continue")],
    ]));
    Self { root_id, buttons, help }
  }
}

impl Page for AskEncryptRoot {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(65), Constraint::Percentage(35)]);
    let info = InfoBox::new(
      "Encrypt ROOT?",
      styled_block(vec![
        vec![(None, "Optionally enable LUKS on the "), (Some((Color::Green, Modifier::BOLD)), "root"), (None, " partition only.")],
        vec![(None, "Boot/ESP remain unencrypted.")],
      ]),
    );
    info.render(f, chunks[0]);
    self.buttons.render(f, chunks[1]);
    self.help.render(f, area);
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    use KeyCode::*;
    match event.code {
      KeyCode::Char('?') => { self.help.toggle(); Signal::Wait}
      ui_up!() => { self.buttons.prev_child(); Signal::Wait}
      ui_down!() => { self.buttons.next_child(); Signal::Wait}
      Char(' ') => { if let Some(w) = self.buttons.focused_child_mut() { w.interact(); } Signal::Wait}
      Esc => Signal::Pop,
      Enter => {
        match self.buttons.selected_child() {
          Some(0) => { // toggle via Enter
            if let Some(w) = self.buttons.focused_child_mut() { w.interact(); }
            Signal::Wait
          }
          Some(1) => {
            let encrypt = self.buttons
              .get_value()
              .and_then(|v| v.get("widget_0").and_then(|b| b.as_bool()))
              .unwrap_or(false);

            if !encrypt { 
              // finalize summary now (no encryption) and go back to Installer
              installer.make_drive_config_display();
              return Signal::PopCount(5);
            }

            // Only handle the requested scenario: UEFI + encrypted root
            if !is_uefi() {
              // Fallback: keep existing flow if not UEFI
              if let Some(cfg) = installer.drive_config.as_mut()
                && let Some(p) = cfg.partition_by_id_mut(self.root_id) {
                  p.add_flag("encrypt");
                    }
                    return Signal::Push(Box::new(
                      PromptLuksPassword::new(self.root_id).with_pop_count(6)
                    ));
                  }
              
                  // We’re UEFI: rebuild layout to include ESP + /boot + encrypted ROOT (+ optional swap)
                  let (root_fs, swap_gb_opt) = {
                    let cfg = match installer.drive_config.as_ref() {
                      Some(c) => c,
                      None => return Signal::Error(anyhow::anyhow!("No drive config available")),
                    };
                
                    // Get FS from current ROOT (chosen earlier in SelectFilesystem)
                    let root_fs = cfg
                      .partitions()
                      .find(|p| p.mount_point() == Some("/"))
                      .and_then(|p| p.fs_type().map(|s| s.to_string()));
                
                    // Derive swap size (GiB) from any swap partition that exists in the temp layout
                    let swap_gb_opt = cfg
                      .partitions()
                      .find(|p| p.fs_type() == Some("swap"))
                      .map(|p| {
                        let bytes = p.size_bytes(cfg.sector_size());
                        // floor to GiB so it matches the “1/2/4/8 GB” choices
                        (bytes / (1024 * 1024 * 1024)) as u64
                      })
                      .filter(|&g| g > 0);
                  
              (root_fs, swap_gb_opt)
            };

            // Rebuild the auto layout for UEFI + encrypted root
            if let Some(cfg) = installer.drive_config.as_mut() {
              cfg.use_uefi_encrypted_root_layout_with_swap(
                root_fs.or(Some("ext4".into())), // fallback if somehow missing
                swap_gb_opt,
                None,  // /boot default 1 GiB
                None,  // ESP  default 512 MiB
              );
              cfg.assign_device_numbers();
            }
          
            // Find the new ROOT id (layout changed)
            let new_root_id = installer
              .drive_config
              .as_ref()
              .and_then(|cfg| cfg.partitions().find(|p| p.mount_point() == Some("/")).map(|p| p.id()));
          
            match new_root_id {
              Some(id) => {
                // continue to passphrase prompt
                Signal::Push(Box::new(
                  PromptLuksPassword::new(id).with_pop_count(6)
                ))
              }
              None => Signal::Error(anyhow::anyhow!("Rebuilt layout has no ROOT (/) partition")),
            }
          }
          Some(2) => Signal::Pop,
          _ => Signal::Wait,
        }
      }
      _ => Signal::Wait,
    }
  }
}

pub struct SetMountPoint {
  editor: LineEditor,
  dev_id: u64,
}

impl SetMountPoint {
  pub fn new(dev_id: u64) -> Self {
    let mut editor = LineEditor::new("Mount Point", Some("Enter a mount point..."));
    editor.focus();
    Self { editor, dev_id }
  }
  fn validate_mount_point(mount_point: &str, taken: &[String]) -> Result<(), String> {
    if mount_point.is_empty() {
      return Err("Mount point cannot be empty.".to_string());
    }
    if !mount_point.starts_with('/') {
      return Err("Mount point must be an absolute path starting with '/'.".to_string());
    }
    if mount_point != "/" && mount_point.ends_with('/') {
      return Err("Mount point cannot end with '/' unless it is root '/'.".to_string());
    }
    if taken.contains(&mount_point.to_string()) {
      return Err(format!(
        "Mount point '{mount_point}' is already taken by another partition."
      ));
    }
    Ok(())
  }
}

impl Page for SetMountPoint {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints(
        [
          Constraint::Percentage(40),
          Constraint::Length(7),
          Constraint::Percentage(40),
        ]
        .as_ref(),
      )
      .split(area);
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(15),
        Constraint::Percentage(70),
        Constraint::Percentage(15),
      ]
    );

    let info_box = InfoBox::new(
      "Set Mount Point",
      styled_block(vec![
        vec![(None, "Specify the mount point for the selected partition.")],
        vec![(None, "Examples of valid mount points include:")],
        vec![(None, "- "), (HIGHLIGHT, "/")],
        vec![(None, "- "), (HIGHLIGHT, "/home")],
        vec![(None, "- "), (HIGHLIGHT, "/boot")],
        vec![(None, "- "), (HIGHLIGHT, "/efi")],
        vec![(None, "Mount points must be absolute paths.")],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.editor.render(f, hor_chunks[1]);
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Esc => Signal::Pop,
      KeyCode::Enter => {
        let mount_point = self
          .editor
          .get_value()
          .unwrap()
          .as_str()
          .unwrap()
          .trim()
          .to_string();
        let Some(device) = installer.drive_config.as_mut() else {
          return Signal::Error(anyhow::anyhow!(
            "No drive config available for setting mount point"
          ));
        };
        let current_mount = device
          .partitions()
          .find(|p| p.id() == self.dev_id)
          .and_then(|p| p.mount_point());

        let mut taken_mounts: Vec<String> = device
          .partitions()
          .filter(|p| *p.status() != PartStatus::Delete && p.id() != self.dev_id)
          .filter_map(|p| p.mount_point().map(|mp| mp.to_string()))
          .collect();

        if let Some(current_mount) = current_mount {
          taken_mounts.retain(|mp| mp != current_mount);
        }
        if let Err(err) = Self::validate_mount_point(&mount_point, &taken_mounts) {
          self.editor.error(&err);
          return Signal::Wait;
        }

        if let Some(part) = device.partition_by_id_mut(self.dev_id) {
          part.set_mount_point(&mount_point);
          // ⬇️ auto-label always
          match mount_point.as_str() {
            "/efi" => part.set_label("ESP"),
            "/boot" => part.set_label("XBOOTLDR"),
            "/"     => part.set_label("ROOT"),
            _       => {}
          }
        }
        Signal::Pop
      }
      _ => self.editor.handle_input(event),
    }
  }
}

pub struct SetLabel {
  editor: LineEditor,
  dev_id: u64,
}

impl SetLabel {
  pub fn new(dev_id: u64) -> Self {
    let mut editor = LineEditor::new("Partition Label", Some("Enter a label..."));
    editor.focus();
    Self { editor, dev_id }
  }
}

impl Page for SetLabel {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(40),
        Constraint::Length(7),
        Constraint::Percentage(40),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(15),
        Constraint::Percentage(70),
        Constraint::Percentage(15),
      ]
    );

    let info_box = InfoBox::new(
      "Set Partition Label",
      styled_block(vec![
        vec![(None, "Specify a label for the selected partition.")],
        vec![(
          None,
          "Partition labels can help identify partitions in the system.",
        )],
        vec![(None, "")],
        vec![(
          HIGHLIGHT,
          "NOTE: If possible, you should make sure that your labels are all uppercase letters.",
        )],
        vec![(
          None,
          "Labels with lowercase letters may break certain tools, and they also cannot be used with vfat filesystems.",
        )],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.editor.render(f, hor_chunks[1]);
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Esc => Signal::Pop,
      KeyCode::Enter => {
        let label = self
          .editor
          .get_value()
          .unwrap()
          .as_str()
          .unwrap()
          .trim()
          .to_string();
        if label.is_empty() {
          self.editor.error("Label cannot be empty.");
          return Signal::Wait;
        }
        if label.len() > 36 {
          self.editor.error("Label cannot exceed 36 characters.");
          return Signal::Wait;
        }
        if label.contains(' ') {
          self.editor.error("Label cannot contain spaces.");
          return Signal::Wait;
        }
        let Some(drive_config) = installer.drive_config.as_mut() else {
          return Signal::Error(anyhow::anyhow!(
            "No drive config available for setting partition label"
          ));
        };
        let Some(part) = drive_config.partition_by_id_mut(self.dev_id) else {
          return Signal::Error(anyhow::anyhow!(
            "No partition found with id {}",
            self.dev_id
          ));
        };

        part.set_label(&label);
        Signal::Pop
      }
      _ => self.editor.handle_input(event),
    }
  }
}
