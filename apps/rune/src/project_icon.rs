use icu_properties::{
    CodePointMapData, CodePointSetData,
    props::{ExtendedPictographic, GeneralCategory, GeneralCategoryGroup, RegionalIndicator},
};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub fn monogram_characters(text: &str) -> bool {
    let categories = CodePointMapData::<GeneralCategory>::new();
    let is_letter_or_number = |character| {
        let category = categories.get(character);
        GeneralCategoryGroup::Letter.contains(category)
            || GeneralCategoryGroup::Number.contains(category)
    };
    let mut characters = text.chars();
    characters.next().is_some_and(is_letter_or_number)
        && characters.all(|character| {
            is_letter_or_number(character)
                || GeneralCategoryGroup::Mark.contains(categories.get(character))
                || matches!(character, '\u{200C}' | '\u{200D}')
        })
}

pub fn normalized_monogram(value: &str) -> Option<String> {
    let normalized: String = value.nfkc().collect();
    let text = trim(&normalized).to_uppercase();
    (text.graphemes(true).take(3).count() <= 2
        && text.encode_utf16().count() <= 32
        && monogram_characters(&text))
    .then_some(text)
}

pub fn first_emoji(value: &str) -> Option<&str> {
    let segment = trim(value).graphemes(true).next()?;
    let pictographs = CodePointSetData::new::<ExtendedPictographic>();
    let regional_indicators = CodePointSetData::new::<RegionalIndicator>();
    let flag = segment.chars().count() == 2
        && segment
            .chars()
            .all(|character| regional_indicators.contains(character));
    let keycap = segment.strip_suffix('\u{20E3}').is_some_and(|base| {
        matches!(
            base.strip_suffix('\u{FE0F}').unwrap_or(base).as_bytes(),
            [b'#' | b'*' | b'0'..=b'9']
        )
    });
    (flag
        || keycap
        || segment
            .chars()
            .any(|character| pictographs.contains(character)))
    .then_some(segment)
}

fn trim(text: &str) -> &str {
    text.trim_matches(|character: char| {
        (character.is_whitespace() && character != '\u{85}') || character == '\u{FEFF}'
    })
}
