use shared::args::{ExecMode, OnFail, is_nix};
use shared::exec::{exec, exec_output};
use shared::files;
use shared::keyboard;
use shared::returncode_eval::exec_eval;
use shared::returncode_eval::files_eval;

pub fn set_timezone(timezone: &str) {
    if !is_nix() {
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "ln",
                vec![
                    "-sf".to_string(),
                    format!("/usr/share/zoneinfo/{}", timezone),
                    "/etc/localtime".to_string(),
                ],
                OnFail::Error,
            ),
            "Set timezone",
        );
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "hwclock",
                vec![
                    "--systohc".to_string(),
                ],
                OnFail::Error
            ),
            "Set system clock",
        );
    } else {
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/hosts/locale/default.nix",
                "Europe/Zurich",
                timezone,
            ),
            "Set Timezone",
        );        
    }
}

pub fn set_locale(locale: String) {
    if !is_nix() {
        files::create_file("/mnt/etc/locale.conf");
        files_eval(
            files::append_file("/mnt/etc/locale.conf", "LANG=en_US.UTF-8"),
            "Edit locale.conf",
        );
        files::create_file("/mnt/etc/locale.gen");
        for i in (0..locale.split(' ').count()).step_by(2) {
            files_eval(
                files::append_file(
                    "/mnt/etc/locale.gen",
                    &format!(
                        "{} {}\n",
                        locale.split(' ').collect::<Vec<&str>>()[i],
                        locale.split(' ').collect::<Vec<&str>>()[i + 1]
                    ),
                ),
                "Add locales to locale.gen",
            );
            if locale.split(' ').collect::<Vec<&str>>()[i] != "en_US.UTF-8" {
                files_eval(
                    files::sed_file(
                        "/mnt/etc/locale.conf",
                        "en_US.UTF-8",
                        locale.split(' ').collect::<Vec<&str>>()[i],
                    ),
                    format!(
                        "Set locale {} in /etc/locale.conf",
                        locale.split(' ').collect::<Vec<&str>>()[i]
                    )
                    .as_str(),
                );
            }
        }
        exec_eval(
            exec(
                ExecMode::Chroot { root: "/mnt" },
                "locale-gen",
                vec![],
                OnFail::Error,
            ),
            "Generate locales.");
    } else {
        // Split the string into words using whitespace as delimiters and take only the first part
        let locale_part = locale.split_whitespace().next().unwrap_or("en_US.UTF-8");
        
        // Use only the extracted part of the locale in the sed_file call. Nix needs only the extracted part (i.e., en_US.UTF-8)
        files_eval(
            files::sed_file(
                "/mnt/etc/nixos/hosts/locale/default.nix",
                "en_US.UTF-8",
                locale_part,
            ),
            "Set Locale",
        );        
    }
}

pub fn set_keyboard(user_choice_or_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let km = keyboard::resolve(user_choice_or_id)
        .unwrap_or_else(|| keyboard::BY_ID["us"]); // safe fallback

    if !is_nix() {
        // Console
        files::create_file("/mnt/etc/vconsole.conf");
        files_eval(
            files::append_file("/mnt/etc/vconsole.conf", &format!("KEYMAP={}\n", km.console)),
            "Set keyboard layout for virtual console",
        );
        files_eval(
            files::append_file("/mnt/etc/vconsole.conf", "FONT=ter-u24n\n"),
            "Set console font",
        );

        // X11
        files_eval(files::create_directory("/mnt/etc/X11/xorg.conf.d"), "Create /mnt/etc/X11/xorg.conf.d directory");
        let mut conf = String::new();
        conf.push_str(r#"
Section "InputClass"
    Identifier "system-keyboard"
    MatchIsKeyboard "on"
"#);
        conf.push_str(&format!("    Option \"XkbLayout\" \"{}\"\n", km.xkb_layout));
        if let Some(var) = km.xkb_variant {
            conf.push_str(&format!("    Option \"XkbVariant\" \"{var}\"\n"));
        }
        conf.push_str(r#"    Option "XkbModel" "pc105+inet"
    Option "XkbOptions" "terminate:ctrl_alt_bksp"
EndSection
"#);
        let mut file = std::fs::File::create("/mnt/etc/X11/xorg.conf.d/00-keyboard.conf")?;
        use std::io::Write;
        file.write_all(conf.as_bytes())?;
    } else {
        // NixOS branch (adjust to your files)
        files_eval(
            files::sed_file("/mnt/etc/nixos/hosts/locale/default.nix", "keyMap = \"us\";", &format!("keyMap = \"{}\";", km.console)),
            "Set Console Keyboard Layout (NixOS)",
        );
        files_eval(
            files::sed_file("/mnt/etc/nixos/hosts/locale/default.nix", "layout = \"us\";", &format!("layout = \"{}\";", km.xkb_layout)),
            "Set X11 Keyboard Layout (NixOS)",
        );
        if let Some(var) = km.xkb_variant {
            files_eval(
                files::sed_file("/mnt/etc/nixos/hosts/locale/default.nix", "variant = \"\";", &format!("variant = \"{var}\";")),
                "Set X11 Keyboard Variant (NixOS)",
            );
        }
    }

    Ok(())
}
/// Apply a keymap to the currently running live environment.
///
/// Unlike `set_keyboard`, which targets the install root at `/mnt`, this
/// function configures the *running* system. It performs three actions:
///   1. Runs `loadkeys` so the change takes effect on the current TTY
///      immediately.
///   2. Writes `/etc/vconsole.conf` so new shells and TTYs spawned in this
///      live session inherit the setting.
///   3. Best-effort: invokes `setxkbmap` if an X11 session happens to be
///      attached (silently ignored if not present or not running under X).
///
/// Returns the resolved Keymap so callers can confirm what was applied
/// (useful for the non-interactive CLI path's success message).
pub fn set_keyboard_live(
    user_choice_or_id: &str,
) -> Result<&'static keyboard::Keymap, Box<dyn std::error::Error + Send + Sync>> {
    let km = keyboard::resolve(user_choice_or_id)
        .unwrap_or_else(|| keyboard::BY_ID["us"]);

    // 1. Apply to current console session immediately
    exec_eval(
        exec(
            ExecMode::Direct,
            "loadkeys",
            vec![km.console.to_string()],
            OnFail::Continue,
        ),
        &format!("Apply keymap {} to current TTY", km.console),
    );

    // 2. Persist for new shells/TTYs in this live session.
    //    Overwrite rather than append so re-runs do not accumulate stale lines.
    std::fs::write(
        "/etc/vconsole.conf",
        format!("KEYMAP={}\nFONT=ter-u24n\n", km.console),
    )?;

    // 3. Best-effort X11 update (no-op outside a graphical session).
    let mut xkb_args = vec![km.xkb_layout.to_string()];
    if let Some(variant) = km.xkb_variant {
        xkb_args.push("-variant".to_string());
        xkb_args.push(variant.to_string());
    }
    let _ = exec_output("setxkbmap", xkb_args);

    Ok(km)
}
