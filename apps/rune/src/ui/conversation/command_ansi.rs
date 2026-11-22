use rune::color::{hex, srgb_decode};
use rune::{Color, Span};

use crate::server::{CommandColor, CommandPen};

const DARK: [&str; 16] = [
    "#3b4048", "#f47067", "#57ab5a", "#c69026", "#539bf5", "#b083f0", "#39c5cf", "#adbac7",
    "#768390", "#ff938a", "#6bc46d", "#daaa3f", "#6cb6ff", "#dcbdfb", "#56d4dd", "#cdd9e5",
];
const LIGHT: [&str; 16] = [
    "#24292f", "#cf222e", "#116329", "#7d4e00", "#0550ae", "#8250df", "#1b7c83", "#6e7781",
    "#57606a", "#a40e26", "#1a7f37", "#633c01", "#0969da", "#6639ba", "#3192aa", "#8c959f",
];

pub(super) fn spans(
    output: &str,
    dark: bool,
    foreground: Color,
    background: Color,
) -> (String, Vec<Span>) {
    let mut text = String::with_capacity(output.len());
    let mut spans = Vec::new();
    let mut pen = CommandPen::default();
    let mut parts = output.split('\x1b');
    text.push_str(parts.next().unwrap_or_default());
    for part in parts {
        let Some((sgr, body)) = part.strip_prefix('[').and_then(|part| part.split_once('m')) else {
            continue;
        };
        pen.apply(sgr);
        let start = text.len();
        text.push_str(body);
        if pen != CommandPen::default() && text.len() != start {
            let fg = pen
                .foreground
                .map_or(foreground, |value| color(value, dark));
            let bg = pen.background.map(|value| color(value, dark));
            let (fg, bg) = if pen.inverse {
                (bg.unwrap_or(background), Some(fg))
            } else {
                (fg, bg)
            };
            let fg = if pen.dim { fg.alpha(0.65) } else { fg };
            let mut span = Span::new(start, text.len()).color(fg);
            if pen.bold {
                span = span.bold();
            }
            if pen.italic {
                span = span.italic();
            }
            if pen.underline {
                span = span.underline(fg);
            }
            if let Some(bg) = bg {
                span = span.bg(bg, 0.0);
            }
            spans.push(span);
        }
    }
    (text, spans)
}

fn color(color: CommandColor, dark: bool) -> Color {
    let palette = if dark { &DARK } else { &LIGHT };
    let channels = match color {
        CommandColor::Standard(value) => {
            let index = match value {
                30..=37 => value - 30,
                40..=47 => value - 40,
                90..=97 => value - 82,
                100..=107 => value - 92,
                _ => 0,
            };
            return hex(palette[usize::from(index)]).expect("standard terminal color");
        }
        CommandColor::Indexed(index @ 0..=15) => {
            return hex(palette[usize::from(index)]).expect("indexed terminal color");
        }
        CommandColor::Indexed(index @ 16..=231) => {
            let n = index - 16;
            let level = |n| if n == 0 { 0 } else { 55 + n * 40 };
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        CommandColor::Indexed(index) => {
            let channel = 8 + (index - 232) * 10;
            (channel, channel, channel)
        }
        CommandColor::Rgb(r, g, b) => (r, g, b),
    };
    let decode = |channel| srgb_decode(f32::from(channel) / 255.0);
    Color {
        r: decode(channels.0),
        g: decode(channels.1),
        b: decode(channels.2),
        a: 1.0,
    }
}
