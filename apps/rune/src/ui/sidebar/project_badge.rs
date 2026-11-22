use rune::color::oklch;
use rune::{El, Family, TextStyle, el, text};
use unicode_normalization::UnicodeNormalization;

const COLORS: [([f32; 3], [f32; 3]); 18] = [
    ([0.707, 0.022, 261.325], [0.446, 0.030, 256.802]),
    ([0.704, 0.191, 22.216], [0.577, 0.245, 27.325]),
    ([0.750, 0.183, 55.934], [0.646, 0.222, 41.116]),
    ([0.828, 0.189, 84.429], [0.666, 0.179, 58.318]),
    ([0.852, 0.199, 91.936], [0.681, 0.162, 75.834]),
    ([0.841, 0.238, 128.850], [0.648, 0.200, 131.684]),
    ([0.792, 0.209, 151.711], [0.627, 0.194, 149.214]),
    ([0.765, 0.177, 163.223], [0.596, 0.145, 163.225]),
    ([0.777, 0.152, 181.912], [0.600, 0.118, 184.704]),
    ([0.789, 0.154, 211.530], [0.609, 0.126, 221.723]),
    ([0.746, 0.160, 232.661], [0.588, 0.158, 241.966]),
    ([0.707, 0.165, 254.624], [0.546, 0.245, 262.881]),
    ([0.673, 0.182, 276.935], [0.511, 0.262, 276.966]),
    ([0.702, 0.183, 293.541], [0.541, 0.281, 293.009]),
    ([0.714, 0.203, 305.504], [0.558, 0.288, 302.321]),
    ([0.740, 0.238, 322.160], [0.591, 0.293, 322.896]),
    ([0.718, 0.202, 349.761], [0.592, 0.249, 0.584]),
    ([0.712, 0.194, 13.428], [0.586, 0.253, 17.585]),
];

fn project_identity(title: &str) -> (String, usize) {
    let normalized: String = title.nfkc().collect();
    let normalized = normalized.trim();
    let mut words = normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let monogram = if let Some(first_word) = words.next() {
        let mut glyphs = first_word.chars();
        let first = glyphs.next().expect("nonempty first word");
        let second = glyphs
            .clone()
            .find(|glyph| glyph.is_numeric())
            .or_else(|| {
                words
                    .next_back()
                    .and_then(|last_word| last_word.chars().next())
            })
            .or_else(|| glyphs.next_back())
            .unwrap_or(first);
        [first, second]
            .into_iter()
            .flat_map(char::to_uppercase)
            .take(2)
            .collect()
    } else {
        "PR".to_owned()
    };
    let seed = if normalized.is_empty() {
        "project"
    } else {
        normalized
    };
    let color = seed.to_lowercase().chars().fold(0, |index, glyph| {
        (index * 31 + glyph as usize) % COLORS.len()
    });
    (monogram, color)
}

pub(super) fn view(title: &str, dark: bool) -> El {
    view_at(title, dark, 16.0)
}

pub(in crate::ui) fn view_at(title: &str, dark: bool, size: f32) -> El {
    let (monogram, index) = project_identity(title);
    let (dark_color, light_color) = COLORS[index];
    let [lightness, chroma, hue] = if dark { dark_color } else { light_color };
    let foreground = oklch(lightness, chroma, hue);
    el().size(size, size)
        .center()
        .radius(size * 0.25)
        .fill(foreground.alpha(0.14))
        .a11y_hidden()
        .child(
            text(monogram)
                .font(
                    TextStyle::new(size * (8.25 / 16.0), 700.0)
                        .lh(size * (10.0 / 16.0))
                        .family(Family::SystemMono),
                )
                .color(foreground),
        )
}

#[cfg(test)]
mod tests {
    use super::project_identity;

    #[test]
    fn project_badges_normalize_names_and_use_the_same_deterministic_palette() {
        assert_eq!(project_identity("Rune migration check"), ("RC".into(), 12));
        assert_eq!(project_identity("server"), ("SR".into(), 3));
        assert_eq!(
            project_identity("Ｒｕｎｅ migration check"),
            project_identity("Rune migration check")
        );
        assert_eq!(project_identity("t3code desktop").0, "T3");
        assert_eq!(project_identity(" — ").0, "PR");
    }
}
