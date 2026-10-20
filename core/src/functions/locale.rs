use shared::args::{ExecMode, OnFail, is_nix};
use shared::exec::{exec, exec_output};
use shared::files;
use shared::keyboard;
use shared::returncode_eval::{exec_eval, files_eval};

const CHROOT: ExecMode<'static> = ExecMode::Chroot { root: "/mnt" };
const NIX_LOCALE: &str = "/mnt/etc/nixos/hosts/locale/default.nix";

pub fn set_timezone(timezone: &str) {
  if !is_nix() {
    exec_eval(
      exec(
        CHROOT,
        "ln",
        [
          "-sf",
          &format!("/usr/share/zoneinfo/{timezone}"),
          "/etc/localtime",
        ],
        OnFail::Error,
      ),
      "Set timezone",
    );
    exec_eval(
      exec(CHROOT, "hwclock", ["--systohc"], OnFail::Error),
      "Set system clock",
    );
  } else {
    files_eval(
      files::sed_file(NIX_LOCALE, "Europe/Zurich", timezone),
      "Set Timezone",
    );
  }
}

/// `locale` is one or more locale.gen lines joined by spaces, e.g.
/// "en_US.UTF-8 UTF-8" or "en_US.UTF-8 UTF-8 it_IT.UTF-8 UTF-8".
/// The first locale becomes LANG.
pub fn set_locale(locale: &str) {
  let tokens: Vec<&str> = locale.split_whitespace().collect();
  let pairs: Vec<(&str, &str)> = tokens
    .chunks(2)
    .map(|c| (c[0], c.get(1).copied().unwrap_or("UTF-8")))
    .collect();
  let lang = pairs
    .first()
    .map(|(name, _)| *name)
    .unwrap_or("en_US.UTF-8");

  if !is_nix() {
    files_eval(
      files::write_file("/mnt/etc/locale.conf", &format!("LANG={lang}\n")),
      "Write locale.conf",
    );
    let locale_gen: String = pairs
      .iter()
      .map(|(name, charset)| format!("{name} {charset}\n"))
      .collect();
    files_eval(
      files::write_file("/mnt/etc/locale.gen", &locale_gen),
      "Write locale.gen",
    );
    exec_eval(
      exec(CHROOT, "locale-gen", Vec::<&str>::new(), OnFail::Error),
      "Generate locales.",
    );
  } else {
    // Nix needs only the locale name (i.e., en_US.UTF-8)
    files_eval(
      files::sed_file(NIX_LOCALE, "en_US.UTF-8", lang),
      "Set Locale",
    );
  }
}

fn x11_keyboard_conf(km: &keyboard::Keymap) -> String {
  let mut conf = String::from(
    "\nSection \"InputClass\"\n    Identifier \"system-keyboard\"\n    MatchIsKeyboard \"on\"\n",
  );
  conf.push_str(&format!("    Option \"XkbLayout\" \"{}\"\n", km.xkb_layout));
  if let Some(var) = km.xkb_variant {
    conf.push_str(&format!("    Option \"XkbVariant\" \"{var}\"\n"));
  }
  conf.push_str("    Option \"XkbModel\" \"pc105+inet\"\n    Option \"XkbOptions\" \"terminate:ctrl_alt_bksp\"\nEndSection\n");
  conf
}

pub fn set_keyboard(user_choice_or_id: &str) -> std::io::Result<()> {
  let km = keyboard::resolve(user_choice_or_id).unwrap_or_else(|| keyboard::BY_ID["us"]); // safe fallback

  if !is_nix() {
    files_eval(
      files::write_file(
        "/mnt/etc/vconsole.conf",
        &format!("KEYMAP={}\nFONT=ter-u24n\n", km.console),
      ),
      "Set keyboard layout and font for virtual console",
    );
    files_eval(
      files::create_directory("/mnt/etc/X11/xorg.conf.d"),
      "Create /mnt/etc/X11/xorg.conf.d directory",
    );
    files::write_file(
      "/mnt/etc/X11/xorg.conf.d/00-keyboard.conf",
      &x11_keyboard_conf(km),
    )?;
  } else {
    files_eval(
      files::sed_file(
        NIX_LOCALE,
        "keyMap = \"us\";",
        &format!("keyMap = \"{}\";", km.console),
      ),
      "Set Console Keyboard Layout (NixOS)",
    );
    files_eval(
      files::sed_file(
        NIX_LOCALE,
        "layout = \"us\";",
        &format!("layout = \"{}\";", km.xkb_layout),
      ),
      "Set X11 Keyboard Layout (NixOS)",
    );
    if let Some(var) = km.xkb_variant {
      files_eval(
        files::sed_file(
          NIX_LOCALE,
          "variant = \"\";",
          &format!("variant = \"{var}\";"),
        ),
        "Set X11 Keyboard Variant (NixOS)",
      );
    }
  }
  Ok(())
}

/// Apply a keymap to the currently running live environment.
///
/// Unlike `set_keyboard`, which targets the install root at `/mnt`, this
/// configures the *running* system: `loadkeys` for the current TTY,
/// `/etc/vconsole.conf` for new TTYs, and best-effort `setxkbmap` for X11.
pub fn set_keyboard_live(user_choice_or_id: &str) -> std::io::Result<&'static keyboard::Keymap> {
  let km = keyboard::resolve(user_choice_or_id).unwrap_or_else(|| keyboard::BY_ID["us"]);

  exec_eval(
    exec(ExecMode::Direct, "loadkeys", [km.console], OnFail::Continue),
    &format!("Apply keymap {} to current TTY", km.console),
  );
  // Overwrite rather than append so re-runs do not accumulate stale lines.
  std::fs::write(
    "/etc/vconsole.conf",
    format!("KEYMAP={}\nFONT=ter-u24n\n", km.console),
  )?;

  let mut xkb_args = vec![km.xkb_layout];
  if let Some(variant) = km.xkb_variant {
    xkb_args.extend(["-variant", variant]);
  }
  let _ = exec_output("setxkbmap", xkb_args);

  Ok(km)
}
