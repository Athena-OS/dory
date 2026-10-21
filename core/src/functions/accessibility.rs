//! Speech on the installed system, when it was installed with speech
//! (Athena Arch).
//!
//! Two levels that never speak at the same time:
//!
//! * **Text consoles** (boot, the Ly login, TTYs): speakup + espeakup, a
//!   root system service started at boot, before any user session.
//! * **Desktop**: Orca, started at login (it ships in every install).
//!
//! The catch is sound. On the installed system ALSA's default device is
//! PipeWire (`/etc/alsa/conf.d/99-pipewire-default.conf`, pipewire-alsa),
//! and PipeWire only runs inside a user session: espeakup would run but
//! stay silent. So espeakup gets its own `HOME` with an `.asoundrc` that
//! sends it to the sound card through dmix. ALSA loads `~/.asoundrc` after
//! `/etc/alsa/conf.d`, so this overrides the PipeWire default for espeakup
//! alone. PipeWire releases the card a few seconds after the desktop goes
//! quiet, and speakup only speaks while a text console is in front, so the
//! two take turns.

use crate::internal::services::enable_service;
use log::{info, warn};
use shared::{
  args::{ExecMode, OnFail},
  exec::exec,
  files,
};
use std::io;

/// Packages added on top of the common set (which already has orca and
/// alsa-utils). brltty is deliberately not installed: its udev rules claim
/// common USB serial adapters (CH340/CH341), which pentesters rely on.
pub const PACKAGES: &[&str] = &["espeakup"];

const ESPEAKUP_HOME: &str = "/var/lib/espeakup";
const DROPIN_DIR: &str = "/mnt/etc/systemd/system/espeakup.service.d";
const SCHEMA_DIR: &str = "/usr/share/glib-2.0/schemas";

/// Desktops whose session starts Orca from Orca's own autostart entry once
/// `screen-reader-enabled` is on (its `OnlyShowIn`).
const ORCA_OWN_AUTOSTART: &str = "GNOME;MATE;Unity;Cinnamon;";

pub fn configure() {
  info!("Configuring speech for the installed system");
  console_speech();
  desktop_speech();
}

/// Log instead of aborting the install: speech setup must never stop an
/// otherwise good installation.
fn soft(result: io::Result<()>, what: &str) -> bool {
  match result {
    Ok(()) => {
      info!("{what}");
      true
    }
    Err(e) => {
      warn!("{what} failed: {e}");
      false
    }
  }
}

fn console_speech() {
  let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
  let live_choice = std::fs::read_to_string("/etc/asound.conf").ok();
  match speech_card(live_choice.as_deref(), &cards) {
    Some(card) => {
      let home = format!("/mnt{ESPEAKUP_HOME}");
      let _ = soft(files::create_directory(&home), "Create espeakup home");
      soft(
        files::write_file(&format!("{home}/.asoundrc"), &asoundrc(&card)),
        &format!("Route console speech to sound card {card}"),
      );
      let _ = soft(
        files::create_directory(DROPIN_DIR),
        "Create espeakup drop-in directory",
      );
      soft(
        files::write_file(&format!("{DROPIN_DIR}/athena-sound.conf"), DROPIN),
        "Give espeakup its own ALSA configuration",
      );
    }
    None => warn!("No sound card found: console speech will use the default device"),
  }

  // Keep the levels the live system had (unmuted by livecd-sound), restored
  // at boot by alsa-restore; a muted card would make console speech silent.
  let _ = soft(
    files::create_directory("/mnt/var/lib/alsa"),
    "Create ALSA state directory",
  );
  soft(
    exec(
      ExecMode::Direct,
      "alsactl",
      ["--file", "/mnt/var/lib/alsa/asound.state", "store"],
      OnFail::Error,
    ),
    "Save sound card levels",
  );

  enable_service("espeakup");
}

fn desktop_speech() {
  // GNOME, MATE, Cinnamon: Orca's own autostart entry starts it when this
  // setting is on. A schema override only changes the default, so users can
  // still turn it off (Super+Alt+S on GNOME).
  let override_path = format!("/mnt{SCHEMA_DIR}/90_athena-screen-reader.gschema.override");
  if soft(
    files::write_file(&override_path, SCHEMA_OVERRIDE),
    "Turn the screen reader on by default",
  ) {
    soft(
      exec(
        ExecMode::Chroot { root: "/mnt" },
        "glib-compile-schemas",
        [SCHEMA_DIR],
        OnFail::Error,
      ),
      "Compile GSettings schemas",
    );
  }

  // KDE Plasma, XFCE and others that run XDG autostart entries.
  let _ = soft(
    files::create_directory("/mnt/etc/xdg/autostart"),
    "Create XDG autostart directory",
  );
  soft(
    files::write_file(
      "/mnt/etc/xdg/autostart/athena-orca.desktop",
      &orca_autostart(),
    ),
    "Start Orca at login on other desktops",
  );
}

const DROPIN: &str = "\
# Written by the Athena OS installer (installed with speech).
# espeakup runs as root outside any PipeWire session; its own HOME holds an
# .asoundrc that routes it to the sound card instead of PipeWire.
[Service]
Environment=HOME=/var/lib/espeakup
";

const SCHEMA_OVERRIDE: &str = "\
# Written by the Athena OS installer (installed with speech).
[org.gnome.desktop.a11y.applications]
screen-reader-enabled=true
";

fn orca_autostart() -> String {
  format!(
    "[Desktop Entry]
Type=Application
Name=Orca Screen Reader
Comment=Athena OS was installed with speech
Exec=orca --replace
NoDisplay=true
NotShowIn={ORCA_OWN_AUTOSTART}
X-GNOME-AutoRestart=true
"
  )
}

/// espeakup's ALSA configuration: the chosen card, shared through dmix.
fn asoundrc(card: &str) -> String {
  format!(
    "# Written by the Athena OS installer for espeakup (console speech).
# Loaded after /etc/alsa/conf.d, so it replaces the PipeWire default for
# espeakup only. dmix lets it share the card.
pcm.!default {{
  type plug
  slave.pcm \"dmix:CARD={card},DEV=0\"
}}
ctl.!default {{
  type hw
  card \"{card}\"
}}
"
  )
}

/// The card speech used on the live system, by ALSA id (stable across
/// reboots, unlike the index): the one picked at boot by livecd-sound
/// (`defaults.pcm.card N` in /etc/asound.conf), else the first card that is
/// not the PC speaker.
fn speech_card(live_asound_conf: Option<&str>, proc_cards: &str) -> Option<String> {
  let cards = parse_cards(proc_cards);
  let picked = live_asound_conf.and_then(|conf| {
    conf.lines().find_map(|l| {
      let n = l.trim().strip_prefix("defaults.pcm.card")?;
      n.trim().trim_end_matches(';').trim().parse::<u32>().ok()
    })
  });
  let by_pick = picked.and_then(|n| cards.iter().find(|(i, _)| *i == n));
  by_pick
    .or_else(|| cards.iter().find(|(_, id)| id != "pcsp"))
    .map(|(_, id)| id.clone())
}

/// `(index, id)` pairs from /proc/asound/cards:
/// ` 0 [PCH            ]: HDA-Intel - HDA Intel PCH`
fn parse_cards(proc_cards: &str) -> Vec<(u32, String)> {
  proc_cards
    .lines()
    .filter_map(|line| {
      let (index, rest) = line.trim_start().split_once(' ')?;
      let index = index.parse().ok()?;
      let id = rest
        .trim_start()
        .strip_prefix('[')?
        .split(']')
        .next()?
        .trim();
      let valid = !id.is_empty()
        && id
          .bytes()
          .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
      valid.then(|| (index, id.to_string()))
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  const CARDS: &str = " 0 [pcsp           ]: PC-Speaker - pcsp
                      Internal PC-Speaker at port 0x61
 1 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7f10000 irq 32
 2 [Headset        ]: USB-Audio - USB Headset
                      Logitech USB Headset at usb-0000:00:14.0-1, full speed
";

  #[test]
  fn cards_are_parsed_by_id() {
    assert_eq!(
      parse_cards(CARDS),
      vec![(0, "pcsp".into()), (1, "PCH".into()), (2, "Headset".into())]
    );
    assert!(parse_cards("--- no soundcards ---").is_empty());
  }

  #[test]
  fn the_card_picked_on_the_live_system_wins() {
    // livecd-sound's template after picking card 2
    let conf = "Defaults node\ndefaults.ctl.card 2;\ndefaults.pcm.card 2;\n";
    assert_eq!(speech_card(Some(conf), CARDS).as_deref(), Some("Headset"));
    // No pick (single card): first card that is not the PC speaker.
    assert_eq!(speech_card(None, CARDS).as_deref(), Some("PCH"));
    // Picked card gone: fall back.
    let gone = "defaults.pcm.card 7;";
    assert_eq!(speech_card(Some(gone), CARDS).as_deref(), Some("PCH"));
    assert_eq!(speech_card(None, ""), None);
  }

  #[test]
  fn espeakup_config_names_the_card() {
    let rc = asoundrc("PCH");
    assert!(rc.contains("pcm.!default"));
    assert!(rc.contains("\"dmix:CARD=PCH,DEV=0\""));
    assert!(rc.contains("card \"PCH\""));
    assert!(DROPIN.contains("Environment=HOME=/var/lib/espeakup"));
    assert!(orca_autostart().contains("NotShowIn=GNOME;MATE;Unity;Cinnamon;"));
  }
}
