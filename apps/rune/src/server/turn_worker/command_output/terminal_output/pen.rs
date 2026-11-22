use std::fmt::Write as _;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct Pen {
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) inverse: bool,
    pub(crate) strike: bool,
    pub(crate) foreground: Option<Color>,
    pub(crate) background: Option<Color>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Color {
    Standard(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Pen {
    pub(super) fn reset() -> Self {
        Self::default()
    }

    pub(crate) fn apply(&mut self, params: &str) {
        if params.is_empty() {
            *self = Self::reset();
            return;
        }
        let numbers = parameters(params);
        let mut index = 0;
        while let Some(&code) = numbers.get(index) {
            match code {
                0.0 => *self = Self::reset(),
                1.0 => self.bold = true,
                2.0 => self.dim = true,
                3.0 => self.italic = true,
                4.0 => self.underline = true,
                7.0 => self.inverse = true,
                9.0 => self.strike = true,
                22.0 => {
                    self.bold = false;
                    self.dim = false;
                }
                23.0 => self.italic = false,
                24.0 => self.underline = false,
                27.0 => self.inverse = false,
                29.0 => self.strike = false,
                39.0 => self.foreground = None,
                49.0 => self.background = None,
                38.0 | 48.0 | 58.0 => {
                    let (color, consumed) = extended_color(&numbers[index + 1..]);
                    if let Some(color) = color {
                        match code {
                            38.0 => self.foreground = Some(color),
                            48.0 => self.background = Some(color),
                            _ => {}
                        }
                    }
                    index += consumed;
                }
                _ if (30.0..=37.0).contains(&code) || (90.0..=97.0).contains(&code) => {
                    self.foreground = Some(Color::Standard(code as u8));
                }
                _ if (40.0..=47.0).contains(&code) || (100.0..=107.0).contains(&code) => {
                    self.background = Some(Color::Standard(code as u8));
                }
                _ => {}
            }
            index += 1;
        }
    }

    pub(super) fn write_sgr(self, out: &mut String) {
        out.push_str("\x1b[0");
        for (enabled, parameter) in [
            (self.bold, ";1"),
            (self.dim, ";2"),
            (self.italic, ";3"),
            (self.underline, ";4"),
            (self.inverse, ";7"),
            (self.strike, ";9"),
        ] {
            if enabled {
                out.push_str(parameter);
            }
        }
        for (color, code) in [(self.foreground, 38), (self.background, 48)] {
            let result = match color {
                Some(Color::Standard(code)) => write!(out, ";{code}"),
                Some(Color::Indexed(index)) => write!(out, ";{code};5;{index}"),
                Some(Color::Rgb(red, green, blue)) => write!(out, ";{code};2;{red};{green};{blue}"),
                None => continue,
            };
            result.expect("writing into a string succeeds");
        }
        out.push('m');
    }
}

fn extended_color(params: &[f64]) -> (Option<Color>, usize) {
    let channel = |value: f64| (value % 256.0) as u8;
    match params {
        [5.0, index, ..] if *index <= 255.0 => (Some(Color::Indexed(*index as u8)), 2),
        [5.0, ..] => (None, 2),
        [2.0, red, green, blue, ..] => (
            Some(Color::Rgb(channel(*red), channel(*green), channel(*blue))),
            4,
        ),
        [2.0, ..] => (None, 4),
        _ => (None, 1),
    }
}

#[derive(Clone, Copy)]
enum Parameter {
    Code,
    Mode,
    Color(usize),
}

pub(super) fn number(text: &str) -> f64 {
    if text.is_empty() {
        0.0
    } else {
        text.parse().unwrap_or(f64::INFINITY)
    }
}

fn parameters(params: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut pending = Parameter::Code;
    for part in params.split(';') {
        let mut sub: Vec<_> = part.split(':').map(number).collect();
        let head = sub[0];
        pending = match pending {
            Parameter::Color(count) => {
                out.push(head);
                if count > 1 {
                    Parameter::Color(count - 1)
                } else {
                    Parameter::Code
                }
            }
            Parameter::Mode if sub.len() == 1 => {
                out.push(head);
                match head {
                    5.0 => Parameter::Color(1),
                    2.0 => Parameter::Color(3),
                    _ => Parameter::Code,
                }
            }
            Parameter::Mode => {
                if head == 2.0 && sub.len() >= 5 {
                    sub.remove(1);
                }
                out.extend(sub.into_iter().take(match head {
                    5.0 => 2,
                    2.0 => 4,
                    _ => 1,
                }));
                Parameter::Code
            }
            Parameter::Code if sub.len() == 1 => {
                out.push(head);
                if matches!(head, 38.0 | 48.0 | 58.0) {
                    Parameter::Mode
                } else {
                    Parameter::Code
                }
            }
            Parameter::Code if !matches!(head, 38.0 | 48.0 | 58.0) => {
                out.push(if head == 4.0 && sub[1] == 0.0 {
                    24.0
                } else {
                    head
                });
                Parameter::Code
            }
            Parameter::Code => {
                let mode = sub[1];
                if mode == 2.0 && sub.len() >= 6 {
                    sub.remove(2);
                }
                out.extend(sub.into_iter().take(match mode {
                    5.0 => 3,
                    2.0 => 5,
                    _ => 2,
                }));
                Parameter::Code
            }
        };
    }
    out
}
