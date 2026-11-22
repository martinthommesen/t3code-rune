use std::time::{Duration, Instant};

use rune::{Key, KeyEvent};

const RESET_AFTER: Duration = Duration::from_millis(750);

#[derive(Default)]
pub(super) struct Typeahead {
    query: String,
    last_input: Option<Instant>,
    anchor: usize,
    last_match: Option<usize>,
}

pub(super) enum Input {
    Other,
    Typed(Option<usize>),
}

impl Typeahead {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn input(
        &mut self,
        event: &KeyEvent,
        selected: usize,
        labels: &[Option<&str>],
    ) -> Input {
        if event.mods.ctrl || event.mods.cmd || event.mods.alt || event.mods.logo {
            return Input::Other;
        }
        let Key::Text(character) = &event.key else {
            return Input::Other;
        };
        if character.encode_utf16().count() != 1 || labels.is_empty() {
            return Input::Other;
        }
        Input::Typed(self.find(character, selected, labels, Instant::now()))
    }

    pub(super) fn typing(&self) -> bool {
        !self.query.is_empty()
            && self
                .last_input
                .is_some_and(|last| last.elapsed() < RESET_AFTER)
    }

    fn find(
        &mut self,
        character: &str,
        selected: usize,
        labels: &[Option<&str>],
        now: Instant,
    ) -> Option<usize> {
        if self
            .last_input
            .is_some_and(|last| now.duration_since(last) >= RESET_AFTER)
        {
            self.clear();
        }
        if self.query.is_empty() {
            self.anchor = selected;
        } else if self.query == character {
            self.anchor = self.last_match.unwrap_or(selected);
            self.query.clear();
        }
        self.query.push_str(character);
        self.last_input = Some(now);
        let query = self.query.to_lowercase();
        let matched = (1..=labels.len())
            .map(|offset| (self.anchor + offset) % labels.len())
            .find(|&index| {
                labels[index].is_some_and(|label| label.to_lowercase().starts_with(&query))
            });
        if let Some(index) = matched {
            self.last_match = Some(index);
        } else if character != " " {
            self.clear();
        }
        matched
    }
}

pub(super) fn arrow_index(key: &Key, index: usize, count: usize) -> Option<usize> {
    let end = count.checked_sub(1)?;
    match key {
        Key::Home => Some(0),
        Key::End => Some(end),
        Key::Up => Some(index.saturating_sub(1)),
        Key::Down => Some((index + 1).min(end)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_cycle_skip_disabled_choices_and_expire() {
        let labels = [
            Some("Supervised"),
            Some("Auto-accept edits"),
            Some("Auto"),
            Some("Full access"),
        ];
        let begin = Instant::now();
        let mut active = Typeahead::default();
        assert_eq!(active.find("a", 3, &labels, begin), Some(1));
        assert_eq!(
            active.find("a", 3, &labels, begin + Duration::from_millis(40)),
            Some(2)
        );
        let renewed = begin + Duration::from_millis(900);
        for key in ["a", "u", "t", "o"] {
            assert_eq!(active.find(key, 3, &labels, renewed), Some(1));
        }
        assert_eq!(active.find("-", 3, &labels, renewed), Some(1));
        active.clear();
        assert_eq!(
            active.find("a", 3, &[labels[0], None, labels[2], labels[3]], renewed),
            Some(2)
        );
        assert_eq!(arrow_index(&Key::Up, 0, 4), Some(0));
        assert_eq!(arrow_index(&Key::Down, 3, 4), Some(3));
    }
}
