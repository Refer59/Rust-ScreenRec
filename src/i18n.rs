//! Messages in English, Spanish or Japanese: the one picked in the settings,
//! else the locale's (LANGUAGE, LC_ALL, LC_MESSAGES, LANG, the first one set),
//! else English.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    En,
    Es,
    Ja,
}

pub const LANGS: [Lang; 3] = [Lang::En, Lang::Es, Lang::Ja];

/// 0: none picked, else index into LANGS + 1.
static CHOSEN: AtomicU8 = AtomicU8::new(0);

pub fn set(l: Lang) {
    CHOSEN.store(LANGS.iter().position(|&x| x == l).unwrap() as u8 + 1, Relaxed);
}

/// The language picked in the settings, if any.
pub fn chosen() -> Option<Lang> {
    LANGS.get((CHOSEN.load(Relaxed) as usize).checked_sub(1)?).copied()
}

pub fn lang() -> Lang {
    static LOCALE: OnceLock<Lang> = OnceLock::new();
    chosen().unwrap_or_else(|| *LOCALE.get_or_init(|| {
        let var = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()));
        from_locale(&var.unwrap_or_default())
    }))
}

/// "es_MX.UTF-8" -> Es; LANGUAGE lists ("ja:en") go by the first entry.
fn from_locale(s: &str) -> Lang {
    match s.get(..2) {
        Some("es") => Lang::Es,
        Some("ja") => Lang::Ja,
        _ => Lang::En,
    }
}

/// `tr!("English", "Español", "日本語", args...)`: the message, as a String, in the user's language.
macro_rules! tr {
    ($en:literal, $es:literal, $ja:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => format!($en $(, $arg)*),
            $crate::i18n::Lang::Es => format!($es $(, $arg)*),
            $crate::i18n::Lang::Ja => format!($ja $(, $arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale() {
        assert_eq!(from_locale("es_MX.UTF-8"), Lang::Es);
        assert_eq!(from_locale("ja_JP.UTF-8"), Lang::Ja);
        assert_eq!(from_locale("ja:en"), Lang::Ja);
        assert_eq!(from_locale("en_US.UTF-8"), Lang::En);
        assert_eq!(from_locale("C"), Lang::En);
        assert_eq!(from_locale(""), Lang::En);
    }
}
