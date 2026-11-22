use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

pub(super) struct Query {
    text: String,
    actions_only: bool,
}

impl Query {
    pub(super) fn new(raw: &str) -> Self {
        let (actions_only, text) = raw
            .strip_prefix('>')
            .map_or((false, raw), |text| (true, text));
        let text = text
            .trim()
            .trim_matches(['"', '\'', '`', '“', '”', '‘', '’']);
        let text = fold(text).split_whitespace().collect::<Vec<_>>().join(" ");
        Self { text, actions_only }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(super) fn actions_only(&self) -> bool {
        self.actions_only
    }

    pub(super) fn rank(&self, title: &str, keywords: &[&str], typos: bool) -> Option<Rank> {
        let title_folded = fold(title);
        let tokens: Vec<_> = self.text.split_whitespace().collect();
        let optional_fillers = tokens.iter().any(|word| !filler(word));
        let meaningful_len = tokens
            .iter()
            .filter(|word| !filler(word))
            .map(|word| word.len())
            .sum::<usize>();
        let short = meaningful_len < 4;
        let title_words = words(title);
        let keywords: Vec<_> = keywords.iter().flat_map(|keyword| words(keyword)).collect();
        let mut count = 0;
        let mut score = 0;
        let mut strong = true;
        let mut title_match = false;
        for token in tokens {
            let primary = token_rank(token, &title_words, typos);
            let keyword = keywords
                .iter()
                .filter_map(|word| {
                    if word == token {
                        Some(if short { 80 } else { 60 })
                    } else if !short && word.starts_with(token) {
                        Some(45)
                    } else {
                        None
                    }
                })
                .max();
            let Some(best) = primary.max(keyword) else {
                if optional_fillers && filler(token) {
                    continue;
                }
                return None;
            };
            count += 1;
            score += best;
            strong &= primary.is_some_and(|score| score >= 70) || keyword.is_some();
            title_match |= primary.is_some();
        }
        if count == 0 {
            return None;
        }
        let bonus = if title_folded == self.text {
            50
        } else if count > 1 && title_folded.contains(&self.text) {
            25
        } else {
            0
        };
        let score = score / count + bonus;
        Some(Rank {
            score,
            group_class: class(if title_match { score } else { score.min(60) }),
            strong,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Rank {
    pub(super) score: u16,
    pub(super) group_class: u8,
    pub(super) strong: bool,
}

fn class(score: u16) -> u8 {
    match score {
        150.. => 3,
        70.. => 2,
        40.. => 1,
        _ => 0,
    }
}

fn fold(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .map(|character| match character {
            'ς' => 'σ',
            '‘' | '’' => '\'',
            character => character,
        })
        .collect()
}

fn words(value: &str) -> Vec<String> {
    let characters: Vec<_> = value.chars().collect();
    let mut result = Vec::new();
    let mut word = String::new();
    for (index, &character) in characters.iter().enumerate() {
        let previous = index.checked_sub(1).map_or(' ', |index| characters[index]);
        let next = characters.get(index + 1).copied().unwrap_or(' ');
        let apostrophe = matches!(character, '\'' | '‘' | '’')
            && previous.is_alphanumeric()
            && next.is_alphanumeric();
        let character_in_word = character.is_alphanumeric() || apostrophe;
        let word_boundary = character.is_uppercase()
            && (previous.is_lowercase() || (previous.is_uppercase() && next.is_lowercase()));
        let digit_boundary = previous.is_alphanumeric()
            && character.is_alphanumeric()
            && previous.is_numeric() != character.is_numeric();
        if (!character_in_word || word_boundary || digit_boundary) && !word.is_empty() {
            result.push(fold(&word));
            word.clear();
        }
        if character_in_word {
            word.push(character);
        }
    }
    if !word.is_empty() {
        result.push(fold(&word));
    }
    result
}

fn filler(word: &str) -> bool {
    matches!(
        word,
        "a" | "an" | "the" | "of" | "to" | "for" | "in" | "on" | "with" | "and" | "my"
    )
}

fn token_rank(token: &str, words: &[String], typos: bool) -> Option<u16> {
    if let Some(strong) = words
        .iter()
        .filter_map(|word| {
            if word == token {
                Some(100)
            } else if word.starts_with(token) {
                let ratio = 20 * token.chars().count() / word.chars().count();
                Some(70 + ratio as u16)
            } else {
                None
            }
        })
        .max()
    {
        return Some(strong);
    }
    if words.windows(2).any(|pair| pair.concat() == token) {
        return Some(60);
    }
    if typos && token.chars().count() >= 3 && token.chars().any(char::is_alphabetic) {
        let token: Vec<_> = token.chars().collect();
        if words.iter().any(|word| typo(&token, word)) {
            return Some(45);
        }
    }
    None
}

fn typo(token: &[char], word: &str) -> bool {
    let word: Vec<_> = word.chars().collect();
    let n = token.len();
    let short = n <= 4;
    if token.first() != word.first()
        && !(n > 3 && token.first() == word.get(1) && token.get(1) == word.first())
    {
        return false;
    }
    [n, n + 1, n - 1].into_iter().any(|length| {
        (!short || length >= n)
            && word
                .get(..length)
                .is_some_and(|prefix| one_edit(token, prefix, short && length == n))
    })
}

fn one_edit(left: &[char], right: &[char], swap_only: bool) -> bool {
    let matched = left
        .iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count();
    let left = &left[matched..];
    let right = &right[matched..];
    let swapped = left.len() >= 2
        && right.len() >= 2
        && left[0] == right[1]
        && left[1] == right[0]
        && left[2..] == right[2..];
    swapped
        || (!swap_only
            && (left == right
                || (left.len() == right.len() && left.get(1..) == right.get(1..))
                || left.get(1..) == Some(right)
                || right.get(1..) == Some(left)))
}
