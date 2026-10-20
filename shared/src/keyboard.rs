// keyboard.rs
use std::sync::LazyLock;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub struct Keymap {
    /// Stable ID you store/return from the picker (lowercase, no spaces).
    pub id: &'static str,
    /// Human-friendly label you show in the UI.
    pub label: &'static str,
    /// Console keymap (vconsole KEYMAP=)
    pub console: &'static str,
    /// XKB layout
    pub xkb_layout: &'static str,
    /// XKB variant (optional)
    pub xkb_variant: Option<&'static str>,
    /// Optional extra spellings that map to this ID
    pub aliases: &'static [&'static str],
}

// Add/extend here - this is your single list.
pub static KEYMAPS: &[Keymap] = &[
    // US
    Keymap { id: "us", label: "English (US) - qwerty", console: "us", xkb_layout: "us", xkb_variant: None, aliases: &["en", "us(qwerty)"] },
    Keymap { id: "us-dvorak", label: "English (US) - Dvorak", console: "dvorak", xkb_layout: "us", xkb_variant: Some("dvorak"), aliases: &["us(dvorak)","dvorak"] },
    Keymap { id: "us-colemak", label: "English (US) - Colemak", console: "colemak", xkb_layout: "us", xkb_variant: Some("colemak"), aliases: &["us(colemak)","colemak"] },
    Keymap { id: "us-intl", label: "English (US) - International", console: "us-acentos", xkb_layout: "us", xkb_variant: Some("intl"), aliases: &["intl","en-intl"] },

    // UK
    Keymap { id: "gb", label: "English (UK)", console: "uk", xkb_layout: "gb", xkb_variant: None, aliases: &["uk"] },

    // DE / FR / ES / IT
    Keymap { id: "de", label: "German", console: "de", xkb_layout: "de", xkb_variant: None, aliases: &[] },
    Keymap { id: "de-nodeadkeys", label: "German - nodeadkeys", console: "de-latin1-nodeadkeys", xkb_layout: "de", xkb_variant: Some("nodeadkeys"), aliases: &[] },
    Keymap { id: "fr", label: "French", console: "fr", xkb_layout: "fr", xkb_variant: None, aliases: &[] },
    Keymap { id: "fr-oss", label: "French - OSS", console: "fr", xkb_layout: "fr", xkb_variant: Some("oss"), aliases: &[] },
    Keymap { id: "es", label: "Spanish (Spain)", console: "es", xkb_layout: "es", xkb_variant: None, aliases: &[] },
    Keymap { id: "it", label: "Italian", console: "it", xkb_layout: "it", xkb_variant: None, aliases: &[] },

    // Nordics / NL
    Keymap { id: "se", label: "Swedish", console: "se", xkb_layout: "se", xkb_variant: None, aliases: &[] },
    Keymap { id: "no", label: "Norwegian", console: "no", xkb_layout: "no", xkb_variant: None, aliases: &[] },
    Keymap { id: "fi", label: "Finnish", console: "fi", xkb_layout: "fi", xkb_variant: None, aliases: &[] },
    Keymap { id: "dk", label: "Danish", console: "dk", xkb_layout: "dk", xkb_variant: None, aliases: &[] },
    Keymap { id: "nl", label: "Dutch", console: "nl", xkb_layout: "nl", xkb_variant: None, aliases: &[] },

    // Swiss
    Keymap { id: "ch", label: "Swiss (Mixed)", console: "de_CH-latin1", xkb_layout: "ch", xkb_variant: None, aliases: &[] },
    Keymap { id: "ch-de", label: "Swiss (German)", console: "de_CH-latin1", xkb_layout: "ch", xkb_variant: Some("de"), aliases: &[] },
    Keymap { id: "ch-fr", label: "Swiss (French)", console: "fr_CH-latin1", xkb_layout: "ch", xkb_variant: Some("fr"), aliases: &[] },


    // Eastern/Central EU
    Keymap { id: "pl", label: "Polish", console: "pl2", xkb_layout: "pl", xkb_variant: None, aliases: &[] },
    Keymap { id: "cz", label: "Czech", console: "cz", xkb_layout: "cz", xkb_variant: None, aliases: &[] },
    Keymap { id: "cz-qwerty", label: "Czech - qwerty", console: "cz-qwerty", xkb_layout: "cz", xkb_variant: Some("qwerty"), aliases: &[] },
    Keymap { id: "sk", label: "Slovak", console: "sk-qwerty", xkb_layout: "sk", xkb_variant: None, aliases: &[] },
    Keymap { id: "hu", label: "Hungarian", console: "hu", xkb_layout: "hu", xkb_variant: None, aliases: &[] },
    Keymap { id: "ro", label: "Romanian", console: "ro", xkb_layout: "ro", xkb_variant: None, aliases: &[] },

    // Balkans / Cyrillic
    Keymap { id: "ru", label: "Russian", console: "ru", xkb_layout: "ru", xkb_variant: None, aliases: &[] },
    Keymap { id: "ru-phonetic", label: "Russian - phonetic", console: "ru", xkb_layout: "ru", xkb_variant: Some("phonetic"), aliases: &[] },
    Keymap { id: "ua", label: "Ukrainian", console: "ua", xkb_layout: "ua", xkb_variant: None, aliases: &[] },
    Keymap { id: "bg", label: "Bulgarian", console: "bg", xkb_layout: "bg", xkb_variant: None, aliases: &[] },
    Keymap { id: "rs", label: "Serbian", console: "srp", xkb_layout: "rs", xkb_variant: None, aliases: &[] },
    Keymap { id: "hr", label: "Croatian", console: "croat", xkb_layout: "hr", xkb_variant: None, aliases: &[] },

    // Middle East
    Keymap { id: "ara", label: "Arabic", console: "ar", xkb_layout: "ar", xkb_variant: None, aliases: &["ar"] },
    Keymap { id: "il", label: "Hebrew", console: "il", xkb_layout: "il", xkb_variant: None, aliases: &["he","hebrew"] },
    Keymap { id: "ir", label: "Persian (Iran)", console: "ir", xkb_layout: "ir", xkb_variant: None, aliases: &["fa"] },
    Keymap { id: "tr", label: "Turkish - Q", console: "trq", xkb_layout: "tr", xkb_variant: None, aliases: &[] },
    Keymap { id: "tr-f", label: "Turkish - F", console: "trf", xkb_layout: "tr", xkb_variant: Some("f"), aliases: &[] },

    // Americas
    Keymap { id: "latam", label: "Spanish (Latin American)", console: "latam", xkb_layout: "latam", xkb_variant: None, aliases: &["mx","ar-latam","cl"] },
    Keymap { id: "br-abnt2", label: "Portuguese (Brazil) - ABNT2", console: "br-abnt2", xkb_layout: "br", xkb_variant: Some("abnt2"), aliases: &["br"] },
    Keymap { id: "ca", label: "Canadian (French)", console: "cf", xkb_layout: "ca", xkb_variant: None, aliases: &[] },
    Keymap { id: "ca-multix", label: "Canadian (Multi-CSA)", console: "cf", xkb_layout: "ca", xkb_variant: Some("multix"), aliases: &[] },

    // Asia
    Keymap { id: "jp", label: "Japanese", console: "jp106", xkb_layout: "jp", xkb_variant: None, aliases: &["jp106"] },
    Keymap { id: "kr", label: "Korean", console: "kr", xkb_layout: "kr", xkb_variant: None, aliases: &[] },
    Keymap { id: "cn", label: "Chinese", console: "cn", xkb_layout: "cn", xkb_variant: None, aliases: &[] },
    Keymap { id: "in", label: "Indian", console: "us", xkb_layout: "us", xkb_variant: None, aliases: &[] },

    // Greeeeeeek
    Keymap { id: "gr", label: "Greek", console: "gr", xkb_layout: "gr", xkb_variant: None, aliases: &[] },
];

// Fast lookup by id/alias (built once)
pub static BY_ID: LazyLock<HashMap<&'static str, &'static Keymap>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for km in KEYMAPS {
        m.insert(km.id, km);
        for &a in km.aliases {
            m.insert(a, km);
        }
    }
    m
});

#[inline]
pub fn normalize(s: &str) -> String {
    s.trim().to_lowercase().replace('_', "-")
}

/// Main resolver used by your installer.
/// Input can be an ID (preferred) or any alias like "us(dvorak)".
pub fn resolve(choice: &str) -> Option<&'static Keymap> {
    let key = normalize(choice);
    BY_ID.get(key.as_str()).copied()
}