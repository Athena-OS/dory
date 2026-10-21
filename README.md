<h1 align="center">Dory</h1>
<h6 align="center">
  A modern terminal-based Athena OS installer, inspired by
  <a href="https://github.com/km-clay/nixos-wizard">nixos-wizard</a>.
</h6>

<img width="2338" height="1264" alt="image" src="https://github.com/user-attachments/assets/1f60f632-cf7a-4399-954c-9aeb7c557d01" />

---

## Features

* **Terminal UI** built with [Ratatui](https://github.com/ratatui/ratatui)
* Partition disks and create filesystems easily
* Configure users, groups, passwords and system environment
* Automatically generate system and drives configuration


## Requirements & Recommendations

* Must be run **as root**.
* A terminal emulator with proper color and Unicode support is recommended for best experience.
* May cause failures if necessary commands are not found in your environment.


## Getting Started

### Running Dory

If running inside the included installer ISO:

```bash
sudo dory
```

## Screen readers

Dory works with the speakup console screen reader (the ISO's speech boot
entry) and with braille displays through brltty.

* **Focus cursor.** The terminal cursor, which stays hidden, always sits on
  the focused item: menu entry, list row, table row, button or text-field
  caret. Readers that follow the cursor land on the right line.
* **Speech.** When speakup is speaking on the console Dory runs on, Dory
  takes over speech for its session. A full-screen interface otherwise makes
  speakup read every redraw: fragments, spinners and the scrolling log. Dory
  speaks:
  * the description shown on the page, then the focused item and its
    position ("Timezone, 2 of 12"), only when they change;
  * typed and deleted characters in text fields. Password fields say "star"
    and never reveal their content;
  * help popups when they open, and why an action was refused;
  * during the installation, only the Install Steps box as each step changes
    state, then the result: success, or failure with the last errors. The
    installation log is shown on screen but never spoken; F2 repeats the
    current step.

  The voice is RHVoice's "slt" when speech-dispatcher, `rhvoice` and
  `rhvoice-voice-slt` are installed (the ISO ships them): Dory starts its own
  speech-dispatcher instance on ALSA. Otherwise it uses speakup's espeak-ng.
  speakup itself (the shell, other TTYs) always uses espeak-ng.

  | Key | Action |
  |---|---|
  | `F2` | Repeat the focused item and its description |
  | `F3` | Read the whole screen |
  | `?` | Open the help of the current page |

  speakup's own screen reading is switched off for Dory's console only, and
  restored when Dory exits, panics or is killed by `SIGTERM`, `SIGHUP`,
  `SIGINT` or `SIGQUIT`. If that happens while you are on another console,
  it is restored as soon as you switch back. Run `dory --no-speech` to leave
  speakup alone; the focus cursor still works.

  Two cases cannot be caught: `SIGKILL`, and a hard crash such as an abort.
  If speech stays silent after one of them, run:

  ```bash
  echo 6 | sudo tee /sys/accessibility/speakup/silent
  ```

  Turning speakup back on with its own speech-kill key while Dory runs
  lasts only until Dory's next announcement.

### After installation

When Dory runs with speech on, the installed system speaks too (Athena Arch;
Athena Nix does not yet):

* **Text consoles** (boot, the Ly login, TTYs): `espeakup` is installed and
  enabled. It runs as root before any user session, where ALSA's default
  device is PipeWire, which is not running yet. So it gets its own `HOME`
  (`/var/lib/espeakup`) whose `.asoundrc` sends it to the sound card used
  during the installation, through dmix. Nothing else is affected. The card
  levels from the live system are saved and restored at boot.
* **Desktop**: Orca starts automatically after login, speaking English with
  RHVoice's "slt" voice and the system's language with espeak-ng. On GNOME, MATE and
  Cinnamon through the `screen-reader-enabled` setting (default changed to on,
  so Super+Alt+S still toggles it on GNOME); on KDE Plasma and XFCE through
  `/etc/xdg/autostart/athena-orca.desktop`. Hyprland and Bspwm do not start it.
* **Login screen**: Ly is a text login and is read by the console screen
  reader. The graphical login screens are silent.

Console and desktop speech never talk at the same time: speakup only speaks
while a text console is in front. They share the sound card, which PipeWire
releases a few seconds after the desktop goes quiet; while the desktop is
playing sound, the console cannot speak.

brltty (braille) is not installed: its udev rules take over common USB serial
adapters (CH340/CH341).

## Roadmap

Propose your ideas.
