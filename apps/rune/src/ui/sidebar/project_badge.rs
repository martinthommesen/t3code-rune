use rune::color::oklch;
use rune::{Color, El, Family, Icon, SurfaceFit, TextStyle, el, icon, text};
use unicode_normalization::UnicodeNormalization;

use crate::connection::{Project, ProjectIcon};
use crate::ui::project_favicons::ProjectFavicons;

mod lucide;

const COLOR_NAMES: [&str; 18] = [
    "gray", "red", "orange", "amber", "yellow", "lime", "green", "emerald", "teal", "cyan", "sky",
    "blue", "indigo", "violet", "purple", "fuchsia", "pink", "rose",
];

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

pub(super) fn view(project: &Project, favicons: &ProjectFavicons, dark: bool) -> El {
    view_at(project, favicons, dark, 16.0)
}

pub(in crate::ui) fn view_at(
    project: &Project,
    favicons: &ProjectFavicons,
    dark: bool,
    size: f32,
) -> El {
    match &project.icon {
        Some(ProjectIcon::Monogram { text, color }) => {
            monogram(text, named_color(color, dark), size)
        }
        Some(ProjectIcon::Emoji { emoji }) => el().size(size, size).center().a11y_hidden().child(
            text(emoji)
                .font(TextStyle::new(size * 0.8, 400.0).lh(size * 0.8))
                .color(if dark { Color::WHITE } else { Color::BLACK }),
        ),
        Some(ProjectIcon::Lucide { name, color }) => {
            let shape = lookup_icon(name)
                .or_else(|| lookup_icon("FolderCode"))
                .expect("folder code icon");
            icon(shape, size)
                .color(named_color(color, dark))
                .a11y_hidden()
        }
        None | Some(ProjectIcon::Unknown) => {
            if let Some(image) = favicons.image(project) {
                return el()
                    .size(size, size)
                    .radius(size * 0.25)
                    .pixels_fit(image.pixels(), SurfaceFit::Fit)
                    .a11y_hidden();
            }
            let (initials, index) = project_identity(&project.name);
            monogram(&initials, palette_color(index, dark), size)
        }
    }
}

fn lookup_icon(name: &str) -> Option<&'static Icon> {
    let mut pascal = String::with_capacity(name.len());
    for part in name.split('-') {
        let mut characters = part.chars();
        if let Some(first) = characters.next() {
            pascal.extend(first.to_uppercase());
            pascal.push_str(characters.as_str());
        }
    }
    let position = lucide::NAMES
        .binary_search_by_key(&pascal.as_str(), |(name, _)| *name)
        .ok()?;
    lucide::ICONS.get(lucide::NAMES[position].1)
}

fn named_color(name: &str, dark: bool) -> Color {
    let index = COLOR_NAMES
        .iter()
        .position(|color| *color == name)
        .unwrap_or(0);
    palette_color(index, dark)
}

fn palette_color(index: usize, dark: bool) -> Color {
    let (dark_color, light_color) = COLORS[index];
    let [lightness, chroma, hue] = if dark { dark_color } else { light_color };
    oklch(lightness, chroma, hue)
}

fn monogram(monogram: &str, foreground: Color, size: f32) -> El {
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
    use super::{lookup_icon, project_identity};

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

    #[test]
    fn saved_lucide_names_resolve_canonical_and_deprecated_exports() {
        assert_eq!(
            lookup_icon("git-pull-request-draft").unwrap().name,
            "GitPullRequestDraft"
        );
        assert_eq!(lookup_icon("folder-code-icon").unwrap().name, "FolderCode");
        assert_eq!(lookup_icon("alert-circle").unwrap().name, "CircleAlert");
        assert!(lookup_icon("not-a-project-icon").is_none());
    }
}
