//! Descriptive info boxes shown next to the selection lists, plus the
//! (cached) decoding of the embedded ASCII-art logos.

use base64::Engine as _;
use flate2::read::GzDecoder;
use ratatui::style::{Color, Modifier};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{LazyLock, Mutex};

use crate::{styled_block, widget::InfoBox};

const HIGHLIGHT: Option<(Color, Modifier)> = Some((Color::Yellow, Modifier::BOLD));

/// Decode base64+gzip into a String, converting literal "\x1b" sequences into
/// real escapes. Results are cached: pages render these on every redraw.
pub fn decode_logo(encoded: &'static str) -> String {
  static CACHE: LazyLock<Mutex<HashMap<&'static str, String>>> = LazyLock::new(Default::default);
  if let Some(hit) = CACHE.lock().expect("logo cache").get(encoded) {
    return hit.clone();
  }
  let bytes = base64::engine::general_purpose::STANDARD
    .decode(encoded)
    .expect("Invalid base64");
  let mut out = String::new();
  GzDecoder::new(&bytes[..])
    .read_to_string(&mut out)
    .expect("Decompression failed");
  let out = out.replace("\\x1b", "\x1b");
  CACHE
    .lock()
    .expect("logo cache")
    .insert(encoded, out.clone());
  out
}

/// Locales offered by the Locale page (locale.gen lines).
pub const LOCALES: &[&str] = &[
  "aa_DJ.UTF-8 UTF-8",
  "af_ZA.UTF-8 UTF-8",
  "an_ES.UTF-8 UTF-8",
  "ar_AE.UTF-8 UTF-8",
  "ar_BH.UTF-8 UTF-8",
  "ar_DZ.UTF-8 UTF-8",
  "ar_EG.UTF-8 UTF-8",
  "ar_IQ.UTF-8 UTF-8",
  "ar_JO.UTF-8 UTF-8",
  "ar_KW.UTF-8 UTF-8",
  "ar_LB.UTF-8 UTF-8",
  "ar_LY.UTF-8 UTF-8",
  "ar_MA.UTF-8 UTF-8",
  "ar_OM.UTF-8 UTF-8",
  "ar_QA.UTF-8 UTF-8",
  "ar_SA.UTF-8 UTF-8",
  "ar_SD.UTF-8 UTF-8",
  "ar_SY.UTF-8 UTF-8",
  "ar_TN.UTF-8 UTF-8",
  "ar_YE.UTF-8 UTF-8",
  "ast_ES.UTF-8 UTF-8",
  "be_BY.UTF-8 UTF-8",
  "bhb_IN.UTF-8 UTF-8",
  "bg_BG.UTF-8 UTF-8",
  "br_FR.UTF-8 UTF-8",
  "bs_BA.UTF-8 UTF-8",
  "ca_AD.UTF-8 UTF-8",
  "ca_ES.UTF-8 UTF-8",
  "ca_FR.UTF-8 UTF-8",
  "ca_IT.UTF-8 UTF-8",
  "cs_CZ.UTF-8 UTF-8",
  "cy_GB.UTF-8 UTF-8",
  "da_DK.UTF-8 UTF-8",
  "de_AT.UTF-8 UTF-8",
  "de_BE.UTF-8 UTF-8",
  "de_CH.UTF-8 UTF-8",
  "de_DE.UTF-8 UTF-8",
  "de_IT.UTF-8 UTF-8",
  "de_LI.UTF-8 UTF-8",
  "de_LU.UTF-8 UTF-8",
  "el_GR.UTF-8 UTF-8",
  "el_CY.UTF-8 UTF-8",
  "en_AU.UTF-8 UTF-8",
  "en_BW.UTF-8 UTF-8",
  "en_CA.UTF-8 UTF-8",
  "en_DK.UTF-8 UTF-8",
  "en_GB.UTF-8 UTF-8",
  "en_HK.UTF-8 UTF-8",
  "en_IE.UTF-8 UTF-8",
  "en_NZ.UTF-8 UTF-8",
  "en_PH.UTF-8 UTF-8",
  "en_SC.UTF-8 UTF-8",
  "en_SG.UTF-8 UTF-8",
  "en_US.UTF-8 UTF-8",
  "en_ZA.UTF-8 UTF-8",
  "en_ZW.UTF-8 UTF-8",
  "es_AR.UTF-8 UTF-8",
  "es_BO.UTF-8 UTF-8",
  "es_CL.UTF-8 UTF-8",
  "es_CO.UTF-8 UTF-8",
  "es_CR.UTF-8 UTF-8",
  "es_DO.UTF-8 UTF-8",
  "es_EC.UTF-8 UTF-8",
  "es_ES.UTF-8 UTF-8",
  "es_GT.UTF-8 UTF-8",
  "es_HN.UTF-8 UTF-8",
  "es_MX.UTF-8 UTF-8",
  "es_NI.UTF-8 UTF-8",
  "es_PA.UTF-8 UTF-8",
  "es_PE.UTF-8 UTF-8",
  "es_PR.UTF-8 UTF-8",
  "es_PY.UTF-8 UTF-8",
  "es_SV.UTF-8 UTF-8",
  "es_US.UTF-8 UTF-8",
  "es_UY.UTF-8 UTF-8",
  "es_VE.UTF-8 UTF-8",
  "et_EE.UTF-8 UTF-8",
  "eu_ES.UTF-8 UTF-8",
  "fo_FO.UTF-8 UTF-8",
  "fr_BE.UTF-8 UTF-8",
  "fr_CA.UTF-8 UTF-8",
  "fr_CH.UTF-8 UTF-8",
  "fr_FR.UTF-8 UTF-8",
  "fr_LU.UTF-8 UTF-8",
  "ga_IE.UTF-8 UTF-8",
  "gd_GB.UTF-8 UTF-8",
  "gl_ES.UTF-8 UTF-8",
  "gv_GB.UTF-8 UTF-8",
  "he_IL.UTF-8 UTF-8",
  "hr_HR.UTF-8 UTF-8",
  "hsb_DE.UTF-8 UTF-8",
  "hu_HU.UTF-8 UTF-8",
  "id_ID.UTF-8 UTF-8",
  "is_IS.UTF-8 UTF-8",
  "it_CH.UTF-8 UTF-8",
  "it_IT.UTF-8 UTF-8",
  "ja_JP.UTF-8 UTF-8",
  "ka_GE.UTF-8 UTF-8",
  "kk_KZ.UTF-8 UTF-8",
  "kl_GL.UTF-8 UTF-8",
  "ko_KR.UTF-8 UTF-8",
  "kw_GB.UTF-8 UTF-8",
  "lg_UG.UTF-8 UTF-8",
  "lt_LT.UTF-8 UTF-8",
  "ltg_LV.UTF-8 UTF-8",
  "lv_LV.UTF-8 UTF-8",
  "mg_MG.UTF-8 UTF-8",
  "mi_NZ.UTF-8 UTF-8",
  "mk_MK.UTF-8 UTF-8",
  "ms_MY.UTF-8 UTF-8",
  "mt_MT.UTF-8 UTF-8",
  "nb_NO.UTF-8 UTF-8",
  "nl_BE.UTF-8 UTF-8",
  "nl_NL.UTF-8 UTF-8",
  "nn_NO.UTF-8 UTF-8",
  "oc_FR.UTF-8 UTF-8",
  "om_KE.UTF-8 UTF-8",
  "pl_PL.UTF-8 UTF-8",
  "pt_BR.UTF-8 UTF-8",
  "pt_PT.UTF-8 UTF-8",
  "ro_RO.UTF-8 UTF-8",
  "ru_RU.UTF-8 UTF-8",
  "ru_UA.UTF-8 UTF-8",
  "sk_SK.UTF-8 UTF-8",
  "sl_SI.UTF-8 UTF-8",
  "so_DJ.UTF-8 UTF-8",
  "so_KE.UTF-8 UTF-8",
  "so_SO.UTF-8 UTF-8",
  "sq_AL.UTF-8 UTF-8",
  "st_ZA.UTF-8 UTF-8",
  "sv_FI.UTF-8 UTF-8",
  "sv_SE.UTF-8 UTF-8",
  "tcy_IN.UTF-8 UTF-8",
  "tg_TJ.UTF-8 UTF-8",
  "th_TH.UTF-8 UTF-8",
  "tl_PH.UTF-8 UTF-8",
  "tr_CY.UTF-8 UTF-8",
  "tr_TR.UTF-8 UTF-8",
  "uk_UA.UTF-8 UTF-8",
  "uz_UZ.UTF-8 UTF-8",
  "wa_BE.UTF-8 UTF-8",
  "xh_ZA.UTF-8 UTF-8",
  "yi_US.UTF-8 UTF-8",
  "zh_CN.UTF-8 UTF-8",
  "zh_HK.UTF-8 UTF-8",
  "zh_SG.UTF-8 UTF-8",
  "zh_TW.UTF-8 UTF-8",
  "zu_ZA.UTF-8 UTF-8",
];

pub fn base_system_info(name: &str) -> InfoBox<'static> {
  match name {
    "Athena Arch" => {
      let logo = decode_logo(
        "H4sIAAAAAAAAA52TUQ6EIAxE/z0FRyWuGg+waTRovFtPIilxV2Va0H7SmddCi3MwmLpGy2xMI9OM80d6NERXjaFEQl3O1DJ9mTzTUuk5FejFviJvbtQbqyj7xIwRzwmQ8xaDYCVWkLloowFEExgOjUY7o+zW/qqKtoovptIG2bCO6RP3rKnm/a5qXfbWpgJNz+/vm5SOddyVFqWTwyGfx+cEAYSsMvCn7Yi2HWcEzBuVBAAA",
      );
      let mut blocks = vec![
        vec![
          (HIGHLIGHT, "Athena Arch"),
          (None, " is based on "),
          (HIGHLIGHT, "Arch Linux"),
          (None, ", offering a "),
          (HIGHLIGHT, "rolling-release, lightweight environment"),
          (None, " with access to the "),
          (HIGHLIGHT, "AUR and bleeding-edge packages"),
          (None, "."),
        ],
        vec![
          (None, "It ships with the "),
          (HIGHLIGHT, "Pacman package manager"),
          (None, " and integrates a fork of the "),
          (HIGHLIGHT, "BlackArch repository"),
          (None, ", providing "),
          (HIGHLIGHT, "2800+ pentesting tools"),
          (None, " that are "),
          (HIGHLIGHT, "GPG-signed for trust"),
          (None, "."),
        ],
        vec![
          (None, "Athena Arch is "),
          (HIGHLIGHT, "highly customizable and fast to update"),
          (None, " but requires "),
          (HIGHLIGHT, "manual setup and maintenance"),
          (None, "; ideal for those who prefer "),
          (HIGHLIGHT, "DIY configuration and bleeding-edge software"),
          (None, "."),
        ],
      ];

      // append the logo *line by line*
      for line in logo.lines() {
        blocks.push(vec![(None, line)]);
      }

      InfoBox::new("Athena Arch", styled_block(blocks))
    }
    "Athena Nix" => {
      let logo = decode_logo(
        "H4sIAAAAAAAAA6WUzQ3DIAyF70zBqDnkkAncWjTqbkzSSlCw8U+tJPIFw/sQz45znl+FR4W9hZLqWbKGlJn8VeH4BVrZCm+SKAwRY7iEEEKKOTMeKMllAjHdo+t34ILE1cOd1sizxnNy2bM8Lh7EY3SKpo/ZteJiPmMym+E+nL1/fXzrCBdqtCPzCAN64bv+U7SjZ4VtxACQAmepDzj416BeftXaizeIaaJWzBgrXveH5pLb+RcIszBtvZF4jlMz9VV/AOenEZ3PBQAA",
      );

      let mut blocks = vec![
        vec![
          (HIGHLIGHT, "Athena Nix"),
          (None, " leverages "),
          (HIGHLIGHT, "NixOS"),
          (None, " to provide a "),
          (HIGHLIGHT, "declarative, reproducible Linux system"),
          (None, " built on the "),
          (HIGHLIGHT, "Nix package manager"),
          (None, "."),
        ],
        vec![
          (None, "It enables "),
          (HIGHLIGHT, "atomic upgrades and rollbacks"),
          (None, ", "),
          (HIGHLIGHT, "isolated dependencies"),
          (None, ", "),
          (HIGHLIGHT, "per-user profiles"),
          (None, ", and "),
          (HIGHLIGHT, "secure package management"),
          (None, " with automatic CVE checks."),
        ],
        vec![
          (None, "Athena Nix offers "),
          (
            HIGHLIGHT,
            "consistency across systems and CI-friendly workflows",
          ),
          (None, " but comes with a "),
          (HIGHLIGHT, "steeper learning curve and unique paradigms"),
          (None, "; perfect for users who value "),
          (
            HIGHLIGHT,
            "determinism, reproducibility, and rollback safety",
          ),
          (None, "."),
        ],
      ];

      // append the logo *line by line*
      for line in logo.lines() {
        blocks.push(vec![(None, line)]);
      }

      InfoBox::new("Athena Nix", styled_block(blocks))
    }
    "Athena Fedora" => {
      let logo = decode_logo(
        "H4sIAAAAAAAAA+NSwAMeTevARDBJLhL1YTUGpyHYNeAwhihTcInRRDOOMFMgwtG4tMIApj5U5cQEO6ZBBHQRGZ0Y5hLSRIah+IOWmHDDrmzo6MehlJSEQzA+cRuDlgBJ1Y0lAaPp4eICAA0CX3KVBAAA",
      );
      let mut blocks = vec![
        vec![
          (HIGHLIGHT, "Athena Fedora"),
          (None, " builds on "),
          (HIGHLIGHT, "Fedora Linux"),
          (None, " to deliver a "),
          (HIGHLIGHT, "stable yet modern environment"),
          (None, " with a focus on "),
          (HIGHLIGHT, "security and upstream collaboration"),
          (None, "."),
        ],
        vec![
          (None, "It uses "),
          (HIGHLIGHT, "DNF and RPM packaging"),
          (None, ", includes "),
          (HIGHLIGHT, "SELinux enabled by default"),
          (None, ", and benefits from "),
          (HIGHLIGHT, "Fedora's 6-month release cadence"),
          (None, " ensuring both stability and freshness."),
        ],
        vec![
          (None, "Athena Fedora is "),
          (HIGHLIGHT, "secure and polished out of the box"),
          (None, " with "),
          (HIGHLIGHT, "containerization and Secure Boot support"),
          (None, ", though it is "),
          (HIGHLIGHT, "less customizable than Arch"),
          (None, "; excellent for those wanting "),
          (HIGHLIGHT, "a dependable and security-hardened workflow"),
          (None, "."),
        ],
      ];

      // append the logo *line by line*
      for line in logo.lines() {
        blocks.push(vec![(None, line)]);
      }

      InfoBox::new("Athena Fedora", styled_block(blocks))
    }
    _ => InfoBox::new(
      "Unknown Base System",
      styled_block(vec![vec![(
        None,
        "No information available for this base system.",
      )]]),
    ),
  }
}

pub fn desktop_info(name: &str) -> InfoBox<'static> {
  match name {
    "GNOME" => InfoBox::new(
      "GNOME",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "GNOME"),
          (None, " is a "),
          (HIGHLIGHT, "modern and popular desktop environment"),
          (None, " that provides a "),
          (HIGHLIGHT, "user-friendly experience"),
          (None, " with a focus on "),
          (HIGHLIGHT, "simplicity and elegance"),
          (None, "."),
        ],
        vec![
          (None, "It features a "),
          (HIGHLIGHT, "clean interface"),
          (None, " with "),
          (HIGHLIGHT, "activities overview"),
          (None, ", "),
          (HIGHLIGHT, "workspaces"),
          (None, ", and extensive "),
          (HIGHLIGHT, "customization options"),
          (None, " through extensions."),
        ],
        vec![
          (None, "GNOME is "),
          (HIGHLIGHT, "resource-intensive"),
          (None, " but offers "),
          (HIGHLIGHT, "excellent accessibility"),
          (None, " and "),
          (HIGHLIGHT, "touch support"),
          (None, "."),
        ],
      ]),
    ),
    "KDE Plasma" => InfoBox::new(
      "KDE Plasma",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "KDE Plasma"),
          (None, " is a "),
          (HIGHLIGHT, "highly customizable desktop environment"),
          (None, " that offers "),
          (HIGHLIGHT, "extensive configuration options"),
          (None, " and a "),
          (HIGHLIGHT, "traditional desktop experience"),
          (None, "."),
        ],
        vec![
          (None, "It provides "),
          (HIGHLIGHT, "powerful widgets"),
          (None, ", "),
          (HIGHLIGHT, "multiple panel layouts"),
          (None, ", and "),
          (HIGHLIGHT, "advanced system settings"),
          (None, " with a familiar Windows-like interface."),
        ],
        vec![
          (None, "KDE Plasma is "),
          (HIGHLIGHT, "feature-rich"),
          (None, " and "),
          (HIGHLIGHT, "resource-efficient"),
          (
            None,
            ", making it suitable for both power users and beginners.",
          ),
        ],
      ]),
    ),
    "Hyprland" => InfoBox::new(
      "Hyprland",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Hyprland"),
          (None, " is a "),
          (HIGHLIGHT, "dynamic tiling Wayland compositor"),
          (None, " that focuses on "),
          (HIGHLIGHT, "eye candy and customization"),
          (None, "."),
        ],
        vec![
          (None, "It features "),
          (HIGHLIGHT, "beautiful animations"),
          (None, ", "),
          (HIGHLIGHT, "automatic window tiling"),
          (None, ", and "),
          (HIGHLIGHT, "extensive configuration"),
          (None, " through text files."),
        ],
        vec![
          (None, "Hyprland is "),
          (HIGHLIGHT, "highly efficient"),
          (None, " and perfect for users who prefer "),
          (HIGHLIGHT, "keyboard-driven workflows"),
          (None, " and "),
          (HIGHLIGHT, "minimal resource usage"),
          (None, "."),
        ],
      ]),
    ),
    "XFCE" => InfoBox::new(
      "XFCE",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "XFCE"),
          (None, " is a "),
          (HIGHLIGHT, "lightweight and fast desktop environment"),
          (None, " that aims to be "),
          (HIGHLIGHT, "visually appealing and user-friendly"),
          (None, " while being "),
          (HIGHLIGHT, "resource-efficient"),
          (None, "."),
        ],
        vec![
          (None, "It provides a "),
          (HIGHLIGHT, "traditional desktop experience"),
          (None, " with "),
          (HIGHLIGHT, "customizable panels"),
          (None, ", "),
          (HIGHLIGHT, "file manager"),
          (None, ", and "),
          (HIGHLIGHT, "application menu"),
          (None, "."),
        ],
        vec![
          (None, "XFCE is "),
          (HIGHLIGHT, "perfect for older hardware"),
          (None, " or users who want a "),
          (HIGHLIGHT, "simple, stable"),
          (None, " desktop without sacrificing functionality."),
        ],
      ]),
    ),
    "Cinnamon" => InfoBox::new(
      "Cinnamon",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Cinnamon"),
          (None, " is a "),
          (HIGHLIGHT, "modern desktop environment"),
          (None, " that provides a "),
          (HIGHLIGHT, "familiar and intuitive experience"),
          (None, " similar to traditional desktops."),
        ],
        vec![
          (None, "It features a "),
          (HIGHLIGHT, "taskbar-style panel"),
          (None, ", "),
          (HIGHLIGHT, "system tray"),
          (None, ", and "),
          (HIGHLIGHT, "start menu"),
          (None, " with "),
          (HIGHLIGHT, "smooth animations"),
          (None, " and effects."),
        ],
        vec![
          (None, "Cinnamon balances "),
          (HIGHLIGHT, "modern features"),
          (None, " with "),
          (HIGHLIGHT, "traditional usability"),
          (
            None,
            ", making it great for users transitioning from other operating systems.",
          ),
        ],
      ]),
    ),
    "MATE" => InfoBox::new(
      "MATE",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "MATE"),
          (None, " is a "),
          (HIGHLIGHT, "traditional desktop environment"),
          (None, " that continues the legacy of "),
          (HIGHLIGHT, "GNOME 2"),
          (None, " with a "),
          (HIGHLIGHT, "classic interface"),
          (None, "."),
        ],
        vec![
          (None, "It provides "),
          (HIGHLIGHT, "stability"),
          (None, ", "),
          (HIGHLIGHT, "reliability"),
          (None, ", and "),
          (HIGHLIGHT, "low resource usage"),
          (None, " while maintaining "),
          (HIGHLIGHT, "familiar desktop metaphors"),
          (None, "."),
        ],
        vec![
          (None, "MATE is "),
          (HIGHLIGHT, "ideal for users"),
          (None, " who prefer "),
          (HIGHLIGHT, "conventional desktop layouts"),
          (None, " and "),
          (HIGHLIGHT, "proven workflows"),
          (None, "."),
        ],
      ]),
    ),
    "Bspwm" => InfoBox::new(
      "Bspwm",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Bspwm"),
          (None, " is a "),
          (HIGHLIGHT, "tiling window manager"),
          (None, " that represents "),
          (HIGHLIGHT, "windows as the leaves of a full binary tree"),
          (None, "."),
        ],
        vec![
          (None, "It offers "),
          (HIGHLIGHT, "manual tiling"),
          (None, " controlled entirely via "),
          (HIGHLIGHT, "external programs"),
          (None, "."),
        ],
        vec![
          (None, "Bspwm emphasizes "),
          (HIGHLIGHT, "simplicity"),
          (None, " and "),
          (HIGHLIGHT, "minimalism"),
          (None, "."),
        ],
      ]),
    ),
    "None" => InfoBox::new(
      "None",
      styled_block(vec![vec![(
        None,
        "No desktop environment will be installed.",
      )]]),
    ),
    _ => InfoBox::new(
      "Unknown Desktop Environment",
      styled_block(vec![vec![(
        None,
        "No information available for this desktop environment.",
      )]]),
    ),
  }
}

pub fn design_info(name: &str) -> InfoBox<'static> {
  match name {
    "Cyborg" => InfoBox::new(
      "Cyborg",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Cyborg design"),
          (None, " features a "),
          (HIGHLIGHT, "dark cyberpunk aesthetic"),
          (None, " with a striking "),
          (HIGHLIGHT, "monochrome illustration"),
          (None, " of a woman’s face."),
        ],
        vec![
          (None, "Her hair is rendered in a "),
          (HIGHLIGHT, "bright glowing tone"),
          (None, " that contrasts with the "),
          (HIGHLIGHT, "shadowed facial features"),
          (None, " and mechanical details."),
        ],
        vec![
          (None, "A "),
          (HIGHLIGHT, "geometric wireframe cube"),
          (None, " surrounds the figure, giving the environment a "),
          (HIGHLIGHT, "futuristic and surreal atmosphere"),
          (None, "."),
        ],
        vec![
          (None, "Above the figure, there is "),
          (HIGHLIGHT, "stylized Japanese text"),
          (None, " that enhances the "),
          (HIGHLIGHT, "sci-fi and anime-inspired vibe"),
          (None, "."),
        ],
      ]),
    ),
    "Frost" => InfoBox::new(
      "Frost",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Frost design"),
          (None, " evokes a sense of "),
          (HIGHLIGHT, "quiet elegance"),
          (None, " through its "),
          (HIGHLIGHT, "monochrome palette"),
          (None, " of "),
          (HIGHLIGHT, "icy grays and silver tones"),
          (None, "."),
        ],
        vec![
          (None, "The centerpiece features a "),
          (HIGHLIGHT, "melancholic anime-style portrait"),
          (None, " rendered with delicate highlights that resemble "),
          (HIGHLIGHT, "frozen light reflections"),
          (None, " against a vast, starry backdrop."),
        ],
        vec![
          (None, "Its atmosphere combines "),
          (HIGHLIGHT, "cosmic stillness"),
          (None, " and "),
          (HIGHLIGHT, "fragile beauty"),
          (None, ", giving the impression of "),
          (HIGHLIGHT, "ice meeting starlight"),
          (None, " in a timeless void."),
        ],
        vec![
          (None, "The theme captures both "),
          (HIGHLIGHT, "emotional depth and minimalist design"),
          (None, ", creating an environment that feels "),
          (HIGHLIGHT, "cold yet introspective"),
          (None, ", perfect for those drawn to serene darkness."),
        ],
      ]),
    ),
    "Graphite" => InfoBox::new(
      "Graphite",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Graphite design"),
          (None, " embraces a "),
          (HIGHLIGHT, "terminal-inspired aesthetic"),
          (None, " with "),
          (HIGHLIGHT, "ASCII art and code fragments"),
          (None, " integrated into the environment."),
        ],
        vec![
          (None, "It combines a "),
          (HIGHLIGHT, "dark background"),
          (None, " with text in "),
          (HIGHLIGHT, "contrasting bright colors"),
          (None, " evoking a "),
          (HIGHLIGHT, "programming and hacker culture vibe"),
          (None, "."),
        ],
        vec![
          (None, "The design conveys a sense of "),
          (HIGHLIGHT, "minimalism"),
          (None, " while celebrating "),
          (HIGHLIGHT, "open-source and coding culture"),
          (None, "."),
        ],
      ]),
    ),
    "HackTheBox" => InfoBox::new(
      "HackTheBox",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "HackTheBox design"),
          (None, " presents a "),
          (HIGHLIGHT, "minimalist approach"),
          (None, " with a "),
          (HIGHLIGHT, "dark solid background"),
          (None, " and a "),
          (HIGHLIGHT, " Hack The Box logo"),
          (None, " at the center."),
        ],
        vec![
          (None, "The cube is rendered in a "),
          (HIGHLIGHT, "bright green tone"),
          (None, " which stands out sharply against the "),
          (HIGHLIGHT, "dark backdrop"),
          (None, ", creating a strong sense of "),
          (HIGHLIGHT, "contrast and focus"),
          (None, "."),
        ],
        vec![
          (None, "This design emphasizes "),
          (HIGHLIGHT, "simplicity, clarity, and modern aesthetics"),
          (None, " while avoiding visual clutter."),
        ],
      ]),
    ),
    "RedMoon" => InfoBox::new(
      "RedMoon",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "RedMoon design"),
          (None, " embody a "),
          (HIGHLIGHT, "dark gothic style"),
          (None, " with "),
          (HIGHLIGHT, "anime-inspired characters"),
          (None, " as the central focus."),
        ],
        vec![
          (None, "They feature a palette dominated by "),
          (HIGHLIGHT, "deep blacks and vivid reds"),
          (None, " which convey a sense of "),
          (HIGHLIGHT, "intensity, danger, and allure"),
          (None, "."),
        ],
        vec![
          (None, "These designs create a "),
          (HIGHLIGHT, "dark yet captivating aesthetic"),
          (None, " that balances "),
          (HIGHLIGHT, "elegance and menace"),
          (None, "."),
        ],
      ]),
    ),
    "Samurai" => InfoBox::new(
      "Samurai",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Samurai design"),
          (None, " combines a "),
          (HIGHLIGHT, "stylized anime character"),
          (None, " holding a "),
          (HIGHLIGHT, "katana"),
          (None, " with a "),
          (HIGHLIGHT, "dreamlike mountain background"),
          (None, "."),
        ],
        vec![
          (None, "The character is framed inside a "),
          (HIGHLIGHT, "soft circular outline"),
          (None, " that enhances focus and creates a sense of "),
          (HIGHLIGHT, "contrast against the blurred scenery"),
          (None, "."),
        ],
        vec![
          (None, "The overall palette features "),
          (HIGHLIGHT, "pastel gradients of purple, blue, and pink"),
          (None, " which evoke a feeling of "),
          (HIGHLIGHT, "calmness mixed with subtle intensity"),
          (None, "."),
        ],
        vec![
          (None, "This design merges "),
          (HIGHLIGHT, "urban streetwear details"),
          (None, " with a "),
          (HIGHLIGHT, "samurai-inspired aesthetic"),
          (None, " for a unique modern-meets-traditional style."),
        ],
      ]),
    ),
    "Sweet" => InfoBox::new(
      "Sweet",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Sweet design"),
          (None, " showcases a "),
          (HIGHLIGHT, "futuristic neon environment"),
          (None, " with a glowing "),
          (HIGHLIGHT, "circular portal"),
          (None, " at the center, radiating vivid light."),
        ],
        vec![
          (None, "The surrounding scene is filled with "),
          (HIGHLIGHT, "angular crystalline structures"),
          (None, " and a reflective "),
          (HIGHLIGHT, "gridded floor"),
          (None, " that enhances the sense of depth."),
        ],
        vec![
          (None, "The palette blends "),
          (HIGHLIGHT, "purple, pink, and electric blue hues"),
          (None, " to create a striking "),
          (HIGHLIGHT, "sci-fi and synthwave atmosphere"),
          (None, "."),
        ],
        vec![
          (None, "This design conveys a feeling of "),
          (HIGHLIGHT, "mystery and technological wonder"),
          (None, " while remaining visually bold and immersive."),
        ],
      ]),
    ),
    "Temple" => InfoBox::new(
      "Temple",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Temple design"),
          (None, " depicts a "),
          (HIGHLIGHT, "mythological theme"),
          (None, " featuring "),
          (HIGHLIGHT, "Athena"),
          (None, " as a central figure in a setting that blends "),
          (HIGHLIGHT, "ancient architecture"),
          (None, " with "),
          (HIGHLIGHT, "futuristic interfaces"),
          (None, "."),
        ],
        vec![
          (None, "The atmosphere combines "),
          (HIGHLIGHT, "Greek-inspired columns and pottery"),
          (None, " with glowing "),
          (HIGHLIGHT, "digital displays and floating code"),
          (None, ", merging tradition with modern technology."),
        ],
        vec![
          (None, "Athena is portrayed in "),
          (HIGHLIGHT, "golden armor and blue attire"),
          (None, " embodying both "),
          (HIGHLIGHT, "wisdom and strength"),
          (None, " while surrounded by an aura of "),
          (HIGHLIGHT, "strategic intelligence"),
          (None, "."),
        ],
        vec![
          (None, "This design creates a unique "),
          (HIGHLIGHT, "fusion of mythology and cyber aesthetics"),
          (None, " symbolizing a balance between the "),
          (HIGHLIGHT, "ancient and the futuristic"),
          (None, "."),
        ],
      ]),
    ),
    "None" => InfoBox::new(
      "None",
      styled_block(vec![vec![(None, "No design will be installed.")]]),
    ),
    _ => InfoBox::new(
      "Unknown Design",
      styled_block(vec![vec![(
        None,
        "No information available for this design.",
      )]]),
    ),
  }
}

pub fn display_manager_info(name: &str) -> InfoBox<'static> {
  match name {
    "Ly" => InfoBox::new(
      "Ly",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "Ly"),
          (None, " is a "),
          (HIGHLIGHT, "lightweight, terminal-based display manager"),
          (None, " designed for "),
          (HIGHLIGHT, "speed, simplicity, and minimal resource usage"),
          (None, "."),
        ],
        vec![
          (None, "It runs "),
          (HIGHLIGHT, "directly on a TTY"),
          (None, " and provides a "),
          (HIGHLIGHT, "keyboard-driven login interface"),
          (None, " without any graphical overhead."),
        ],
        vec![
          (None, "Ly is "),
          (HIGHLIGHT, "ideal for tiling window managers"),
          (None, ", "),
          (HIGHLIGHT, "Wayland compositors"),
          (None, ", and users who prefer a "),
          (HIGHLIGHT, "clean, distraction-free workflow"),
          (None, "."),
        ],
        vec![
          (None, "Its minimal design makes it "),
          (HIGHLIGHT, "fast to start"),
          (None, ", "),
          (HIGHLIGHT, "easy to configure"),
          (None, ", and "),
          (HIGHLIGHT, "reliable on low-end or headless systems"),
          (None, "."),
        ],
      ]),
    ),
    "Astronaut" => InfoBox::new(
      "Astronaut",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Astronaut"),
          (None, " presents a "),
          (HIGHLIGHT, "space-themed illustration"),
          (None, " featuring a "),
          (HIGHLIGHT, "vast planet"),
          (None, " set against a "),
          (HIGHLIGHT, "star-filled sky"),
          (None, "."),
        ],
        vec![
          (None, "A nearby "),
          (HIGHLIGHT, "spacecraft"),
          (None, " and a "),
          (HIGHLIGHT, "lone astronaut"),
          (None, " introduce a sense of "),
          (HIGHLIGHT, "exploration and scale"),
          (None, "."),
        ],
        vec![
          (None, "The palette leans on "),
          (HIGHLIGHT, "cool blues and muted tones"),
          (None, " with "),
          (HIGHLIGHT, "clean, graphic shapes"),
          (None, " that create a "),
          (HIGHLIGHT, "calm, futuristic atmosphere"),
          (None, "."),
        ],
      ]),
    ),
    "Black Hole" => InfoBox::new(
      "Black Hole",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Black Hole"),
          (None, " depicts a "),
          (HIGHLIGHT, "deep-space scene"),
          (None, " dominated by a "),
          (HIGHLIGHT, "massive black hole"),
          (None, " encircled by a bright "),
          (HIGHLIGHT, "accretion ring"),
          (None, " of swirling light and dust."),
        ],
        vec![
          (None, "Floating nearby are "),
          (HIGHLIGHT, "asteroids and debris"),
          (None, " with a "),
          (HIGHLIGHT, "solitary astronaut"),
          (None, " adding a sense of "),
          (HIGHLIGHT, "scale and motion"),
          (None, "."),
        ],
        vec![
          (None, "The color palette blends "),
          (HIGHLIGHT, "deep purples and blues"),
          (None, " with "),
          (HIGHLIGHT, "fiery orange highlights"),
          (None, ", creating a "),
          (HIGHLIGHT, "cinematic cosmic atmosphere"),
          (None, "."),
        ],
        vec![
          (None, "Overall, it conveys "),
          (HIGHLIGHT, "mystery, gravity, and vastness"),
          (None, " while remaining visually "),
          (HIGHLIGHT, "striking and immersive"),
          (None, "."),
        ],
      ]),
    ),
    "Cyberpunk" => InfoBox::new(
      "Cyberpunk",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Cyberpunk"),
          (None, " features a "),
          (HIGHLIGHT, "cyberpunk portrait"),
          (None, " of a "),
          (HIGHLIGHT, "hooded figure with a masked/visored face"),
          (None, ", rendered against a "),
          (HIGHLIGHT, "dark background"),
          (None, "."),
        ],
        vec![
          (None, "Surrounding the figure are "),
          (HIGHLIGHT, "glitch effects and HUD-like geometric lines"),
          (None, " that suggest "),
          (HIGHLIGHT, "digital interference and motion"),
          (None, "."),
        ],
        vec![
          (None, "The palette blends "),
          (HIGHLIGHT, "neon magenta and electric cyan"),
          (None, " accents with "),
          (HIGHLIGHT, "high-contrast blacks"),
          (None, " for a bold, tech-noir look."),
        ],
        vec![
          (None, "Overall, it conveys "),
          (HIGHLIGHT, "anonymity, hacking culture, and futuristic grit"),
          (None, " with a distinctly "),
          (HIGHLIGHT, "synthwave aesthetic"),
          (None, "."),
        ],
      ]),
    ),
    "Cyborg" => InfoBox::new(
      "Cyborg",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Cyborg"),
          (None, " showcases a "),
          (HIGHLIGHT, "minimalist retro-futuristic illustration"),
          (None, " of a "),
          (HIGHLIGHT, "woman in profile"),
          (None, " with long dark hair and subtle "),
          (HIGHLIGHT, "cybernetic accents"),
          (None, " across the face."),
        ],
        vec![
          (None, "She is framed by a "),
          (HIGHLIGHT, "geometric wireframe cube"),
          (None, " in perspective, adding a sense of "),
          (HIGHLIGHT, "depth and containment"),
          (None, "."),
        ],
        vec![
          (None, "The palette uses "),
          (HIGHLIGHT, "warm monochrome tones"),
          (None, ", cream, beige, and charcoal, creating a "),
          (HIGHLIGHT, "calm yet otherworldly atmosphere"),
          (None, " with generous negative space."),
        ],
      ]),
    ),
    "Kath" => InfoBox::new(
      "Kath",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Kath"),
          (None, " is an "),
          (HIGHLIGHT, "animated pixel-art scene"),
          (None, " set in a "),
          (HIGHLIGHT, "futuristic industrial room"),
          (None, " where a "),
          (HIGHLIGHT, "blue-haired girl"),
          (None, " sits under "),
          (HIGHLIGHT, "soft neon lighting"),
          (None, "."),
        ],
        vec![
          (None, "The palette blends "),
          (HIGHLIGHT, "cool blues and violets"),
          (None, " with "),
          (HIGHLIGHT, "electric cyan highlights"),
          (None, ", evoking a "),
          (HIGHLIGHT, "retro sci-fi arcade vibe"),
          (None, "."),
        ],
        vec![
          (None, "Overall, it conveys "),
          (HIGHLIGHT, "calm, atmospheric energy"),
          (None, " with a mix of "),
          (HIGHLIGHT, "nostalgia and futurism"),
          (None, "."),
        ],
      ]),
    ),
    "Jake The Dog" => InfoBox::new(
      "Jake The Dog",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Jake The Dog"),
          (None, " is an "),
          (HIGHLIGHT, "animated, cartoon-style scene"),
          (None, " featuring a "),
          (HIGHLIGHT, "relaxed dog wearing headphones"),
          (None, " sitting on a rooftop and enjoying music."),
        ],
        vec![
          (None, "The background shows a "),
          (HIGHLIGHT, "dusk forest landscape"),
          (None, " with "),
          (HIGHLIGHT, "soft purples and muted greens"),
          (None, " rendered in "),
          (HIGHLIGHT, "simple shapes and thick outlines"),
          (None, "."),
        ],
        vec![
          (None, "Overall, it conveys a "),
          (HIGHLIGHT, "cozy, playful, and nostalgic vibe"),
          (None, " ideal for a laid-back login mood."),
        ],
      ]),
    ),
    "Pixel Sakura" => InfoBox::new(
      "Pixel Sakura",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Pixel Sakura"),
          (None, " is a "),
          (HIGHLIGHT, "pixel-art cityscape"),
          (None, " with a "),
          (HIGHLIGHT, "train crossing a bridge"),
          (None, " over calm water, a distant "),
          (HIGHLIGHT, "skyline"),
          (None, ", and a foreground "),
          (HIGHLIGHT, "cherry-blossom branch"),
          (None, "."),
        ],
        vec![
          (None, "A "),
          (HIGHLIGHT, "looping animation"),
          (None, " sends "),
          (HIGHLIGHT, "petals/leaves drifting"),
          (None, " through the air, creating a sense of "),
          (HIGHLIGHT, "breeze and quiet motion"),
          (None, "."),
        ],
        vec![
          (None, "The palette favors "),
          (HIGHLIGHT, "soft grays and off-white"),
          (None, " with "),
          (HIGHLIGHT, "delicate pink accents"),
          (None, " and a "),
          (HIGHLIGHT, "muted sun"),
          (None, ", with gentle "),
          (HIGHLIGHT, "reflections in the water"),
          (None, "."),
        ],
        vec![
          (None, "Overall, it conveys "),
          (HIGHLIGHT, "serene, nostalgic vibes"),
          (None, " blending "),
          (HIGHLIGHT, "urban slice-of-life ambience"),
          (None, " with "),
          (HIGHLIGHT, "seasonal calm"),
          (None, "."),
        ],
      ]),
    ),
    "Post-Apocalypse" => InfoBox::new(
      "Post-Apocalypse",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Post-Apocalypse"),
          (None, " presents a "),
          (HIGHLIGHT, "gritty, graphic-novel illustration"),
          (None, " of a "),
          (HIGHLIGHT, "hooded figure with a skull-like face"),
          (None, " and "),
          (HIGHLIGHT, "glowing pink goggles"),
          (None, "."),
        ],
        vec![
          (
            None,
            "Cables and tubes trail from the mask as the character ",
          ),
          (HIGHLIGHT, "hunches over a keyboard"),
          (None, " beside a "),
          (HIGHLIGHT, "well-worn laptop"),
          (None, ", adding a sense of "),
          (HIGHLIGHT, "tension and focus"),
          (None, "."),
        ],
        vec![
          (None, "The palette leans on "),
          (HIGHLIGHT, "muted olive, maroon, and charcoal"),
          (None, " accented by "),
          (HIGHLIGHT, "neon magenta highlights"),
          (None, ", with "),
          (HIGHLIGHT, "rough inks and grunge textures"),
          (None, " that emphasize a "),
          (HIGHLIGHT, "post-apocalyptic hacker vibe"),
          (None, "."),
        ],
      ]),
    ),
    "Purple Leaves" => InfoBox::new(
      "Purple Leaves",
      styled_block(vec![
        vec![
          (HIGHLIGHT, "SDDM Purple Leaves"),
          (None, " features a "),
          (HIGHLIGHT, "stylized botanical pattern"),
          (None, " of "),
          (HIGHLIGHT, "overlapping leaves"),
          (None, " rendered with bold outlines and layered shading."),
        ],
        vec![
          (None, "The palette leans on "),
          (HIGHLIGHT, "deep indigos, violets, and navy tones"),
          (None, " creating a "),
          (HIGHLIGHT, "moody, nocturnal atmosphere"),
          (None, "."),
        ],
        vec![
          (None, "Overall, it provides a "),
          (HIGHLIGHT, "calm, elegant backdrop"),
          (None, " with "),
          (HIGHLIGHT, "soft contrast"),
          (None, " that keeps the scene cohesive and unobtrusive."),
        ],
      ]),
    ),
    "None" => InfoBox::new(
      "None",
      styled_block(vec![vec![(None, "No display manager will be installed.")]]),
    ),
    _ => InfoBox::new(
      "Unknown Display Manager",
      styled_block(vec![vec![(
        None,
        "No information available for this display manager.",
      )]]),
    ),
  }
}
