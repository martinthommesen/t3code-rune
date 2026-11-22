use icu_properties::props::{
    DefaultIgnorableCodePoint, EastAsianWidth, GeneralCategory, GeneralCategoryGroup,
};
use icu_properties::{CodePointMapData, CodePointSetData};

use super::pen::Pen;
use super::tail::{utf16_boundary, utf16_len};

const MAX_LINE_RUNS: usize = 2_048;

#[derive(Default)]
pub(super) struct Line {
    runs: Vec<Run>,
    pub(super) units: usize,
    pub(super) width: usize,
}

struct Run {
    pen: Pen,
    text: String,
}

pub(super) fn character_width(character: char) -> usize {
    if character.is_ascii() {
        return 1;
    }
    if GeneralCategoryGroup::Mark
        .contains(CodePointMapData::<GeneralCategory>::new().get(character))
        || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(character)
    {
        0
    } else if matches!(
        CodePointMapData::<EastAsianWidth>::new().get(character),
        EastAsianWidth::Wide | EastAsianWidth::Fullwidth
    ) {
        2
    } else {
        1
    }
}

fn text_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.chars().map(character_width).sum()
    }
}

impl Line {
    fn push(&mut self, pen: Pen, text: &str) {
        if text.is_empty() {
            return;
        }
        self.units += utf16_len(text);
        self.width += text_width(text);
        if let Some(last) = self.runs.last_mut()
            && last.pen == pen
        {
            last.text.push_str(text);
        } else {
            self.runs.push(Run {
                pen,
                text: text.into(),
            });
        }
    }

    fn spaces(&mut self, count: usize) {
        self.push(Pen::reset(), &" ".repeat(count));
    }

    fn slice_into(&self, out: &mut Self, start: usize, end: usize) {
        let mut col = 0;
        let mut base = 0;
        let mut base_end = 0;
        for run in &self.runs {
            if run.text.is_ascii() {
                let run_end = col + run.text.len();
                if run_end > start && col < end {
                    let from = start.saturating_sub(col);
                    let to = run.text.len().min(end - col);
                    out.push(run.pen, &run.text[from..to]);
                }
                base = run_end - 1;
                base_end = run_end;
                col = run_end;
                continue;
            }
            let mut text = String::new();
            for character in run.text.chars() {
                let width = character_width(character);
                if width == 0 {
                    if base >= start && base < end && base_end <= end {
                        text.push(character);
                    }
                } else {
                    base = col;
                    base_end = col + width;
                    if col < end && base_end > start {
                        if col >= start && base_end <= end {
                            text.push(character);
                        } else {
                            let cells = end.min(base_end) - start.max(col);
                            text.extend(std::iter::repeat_n(' ', cells));
                        }
                    }
                }
                col += width;
            }
            out.push(run.pen, &text);
        }
    }

    pub(super) fn write(&mut self, col: usize, text: &str, pen: Pen) -> usize {
        let width = text_width(text);
        if col == self.width {
            self.push(pen, text);
        } else {
            let mut replacement = Self::default();
            self.slice_into(&mut replacement, 0, col.min(self.width));
            replacement.spaces(col.saturating_sub(self.width));
            replacement.push(pen, text);
            self.slice_into(&mut replacement, col + width, self.width);
            *self = replacement;
        }
        if self.runs.len() > MAX_LINE_RUNS {
            let flatten = self.runs.len() - MAX_LINE_RUNS / 8;
            let text = self.runs.drain(..flatten).map(|run| run.text).collect();
            self.runs.insert(
                0,
                Run {
                    pen: Pen::reset(),
                    text,
                },
            );
        }
        width
    }

    pub(super) fn erase(&mut self, col: usize, mode: usize) {
        let mut replacement = Self::default();
        match mode {
            0 => self.slice_into(&mut replacement, 0, col),
            1 => {
                replacement.spaces((col + 1).min(self.width));
                self.slice_into(&mut replacement, col + 1, self.width);
            }
            2 => {}
            _ => return,
        }
        *self = replacement;
    }

    pub(super) fn trim_start(&mut self, count: usize) -> usize {
        let mut removed_width = 0;
        let mut removed_units = 0;
        let mut dropped = 0;
        let mut remaining = count;
        for run in &mut self.runs {
            let (mut cut, _) = utf16_boundary(&run.text, remaining);
            for character in run.text[cut..].chars() {
                if character_width(character) != 0 {
                    break;
                }
                cut += character.len_utf8();
            }
            let prefix = &run.text[..cut];
            let units = utf16_len(prefix);
            removed_width += text_width(prefix);
            removed_units += units;
            remaining = remaining.saturating_sub(units);
            if cut < run.text.len() {
                run.text.drain(..cut);
                break;
            }
            dropped += 1;
        }
        self.runs.drain(..dropped);
        self.units -= removed_units;
        self.width -= removed_width;
        removed_width
    }

    pub(super) fn serialize(&self, out: &mut String) {
        let mut current = Pen::reset();
        for run in &self.runs {
            if run.pen != current {
                run.pen.write_sgr(out);
                current = run.pen;
            }
            out.push_str(&run.text);
        }
        if current != Pen::reset() {
            Pen::reset().write_sgr(out);
        }
    }
}
