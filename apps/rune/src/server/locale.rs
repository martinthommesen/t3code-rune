use icu_collator::{Collator, CollatorBorrowed, options::CollatorOptions};
use icu_locale_core::{Locale, locale};

pub(super) fn path_collator() -> Option<CollatorBorrowed<'static>> {
    let locale = current_locale().unwrap_or(locale!("en-US"));
    collator_for(locale)
}

fn collator_for(locale: Locale) -> Option<CollatorBorrowed<'static>> {
    Collator::try_new(locale.into(), CollatorOptions::default()).ok()
}

#[cfg(any(target_os = "macos", windows))]
fn current_locale() -> Option<Locale> {
    sys_locale::get_locale().and_then(|locale| locale.parse().ok())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn current_locale() -> Option<Locale> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|locale| !locale.is_empty())?;
    let name = locale.split(['.', '@']).next()?.replace('_', "-");
    match name.as_str() {
        "C" | "POSIX" => None,
        _ => name.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_paths_with_the_desktop_runtime_unicode_version() {
        let collator = collator_for(locale!("en-US")).unwrap();
        let mut paths = [
            "森.txt",
            "ä.txt",
            "Z.cpp",
            "_x",
            "a.txt",
            "\u{1fa8b}.txt",
            "-x",
            "👻.txt",
            "A.txt",
            ".x",
        ];
        paths.sort_by(|left, right| collator.compare(left, right));
        assert_eq!(
            paths,
            [
                "_x",
                "-x",
                ".x",
                "👻.txt",
                "a.txt",
                "A.txt",
                "ä.txt",
                "Z.cpp",
                "森.txt",
                "\u{1fa8b}.txt"
            ]
        );
    }

    #[test]
    fn sorts_accents_by_the_locale() {
        let collator = collator_for(locale!("sv-SE")).unwrap();
        let mut paths = ["ä.txt", "Z.cpp", "A.txt"];
        paths.sort_by(|left, right| collator.compare(left, right));
        assert_eq!(paths, ["A.txt", "Z.cpp", "ä.txt"]);
    }
}
