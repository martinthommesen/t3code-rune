use icu_properties::{
    CodePointMapData,
    props::{GeneralCategory, GeneralCategoryGroup},
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum ProjectIcon {
    Lucide {
        #[serde(deserialize_with = "lucide_name")]
        name: String,
        color: Color,
    },
    Emoji {
        #[serde(deserialize_with = "emoji")]
        emoji: String,
    },
    Monogram {
        #[serde(deserialize_with = "monogram")]
        text: String,
        color: Color,
    },
}

impl ProjectIcon {
    pub(super) fn exceeds_monogram_limit(&self) -> bool {
        matches!(self, Self::Monogram { text, .. } if text.graphemes(true).take(3).count() > 2)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum Color {
    Gray,
    Red,
    Orange,
    Amber,
    Yellow,
    Lime,
    Green,
    Emerald,
    Teal,
    Cyan,
    Sky,
    Blue,
    Indigo,
    Violet,
    Purple,
    Fuchsia,
    Pink,
    Rose,
}

fn lucide_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = trimmed(deserializer, 64)?;
    if text.split('-').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) {
        Ok(text)
    } else {
        Err(D::Error::custom("Invalid Lucide project icon name."))
    }
}

fn emoji<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    trimmed(deserializer, 32)
}

fn monogram<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = trimmed(deserializer, 32)?;
    let categories = CodePointMapData::<GeneralCategory>::new();
    let is_letter_or_number = |character| {
        let category = categories.get(character);
        GeneralCategoryGroup::Letter.contains(category)
            || GeneralCategoryGroup::Number.contains(category)
    };
    let mut characters = text.chars();
    if characters.next().is_some_and(is_letter_or_number)
        && characters.all(|character| {
            is_letter_or_number(character)
                || GeneralCategoryGroup::Mark.contains(categories.get(character))
                || matches!(character, '‌' | '‍')
        })
    {
        Ok(text)
    } else {
        Err(D::Error::custom("Invalid project monogram text."))
    }
}

fn trimmed<'de, D: Deserializer<'de>>(
    deserializer: D,
    max_length: usize,
) -> Result<String, D::Error> {
    let original = String::deserialize(deserializer)?;
    let text = super::super::project_favicon::trim(&original);
    if text.is_empty() || text.encode_utf16().count() > max_length {
        return Err(D::Error::custom("Invalid project icon text length."));
    }
    Ok(text.to_owned())
}
