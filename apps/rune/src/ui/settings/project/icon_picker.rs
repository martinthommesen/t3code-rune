use rune::{
    Color, El, Key, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text, text_input,
};
use t3_rune::project_icon::{first_emoji, normalized_monogram};

use crate::connection::{Project, ProjectEdit, ProjectEditKind};
use crate::ui::{
    Palette, State,
    settings::{CONTROL, DESCRIPTION, SECTION},
    sidebar::project_badge,
};

use super::{PROJECT_FONT, find_project};

const DIALOG: &str = "project-icon-picker";
const OPENER: &str = "choose-project-icon";
const SEARCH: &str = "project-icon-search";
const POPULAR: &[&str] = &[
    "folder-code",
    "code-2",
    "terminal",
    "globe-2",
    "server",
    "database",
    "bot",
    "sparkles",
    "smartphone",
    "monitor",
    "cloud-cog",
    "package",
    "book-open",
    "flask-conical",
    "shield-check",
    "rocket",
    "gamepad-2",
    "music",
    "image",
    "shopping-bag",
    "git-branch",
    "workflow",
    "wrench",
    "layers-3",
];
const SWATCHES: [&str; 18] = [
    "#6a7282", "#fb2c36", "#ff6900", "#fe9a00", "#f0b100", "#7ccf00", "#00c950", "#00bc7d",
    "#00bba7", "#00b8db", "#00a6f4", "#2b7fff", "#615fff", "#8e51ff", "#ad46ff", "#e12afb",
    "#f6339a", "#ff2056",
];

const EMOJIS: &[(&str, &str)] = &[
    ("💻", "Computer"),
    ("🛠️", "Tools"),
    ("🚀", "Rocket"),
    ("🤖", "Robot"),
    ("✨", "Sparkles"),
    ("⚡", "Lightning"),
    ("🌐", "Web"),
    ("📱", "Mobile"),
    ("🖥️", "Desktop"),
    ("⌨️", "Keyboard"),
    ("⚙️", "Gear"),
    ("🗄️", "Database"),
    ("☁️", "Cloud"),
    ("📦", "Package"),
    ("📚", "Books"),
    ("🧪", "Test tube"),
    ("🔒", "Lock"),
    ("🎮", "Game"),
    ("🎵", "Music"),
    ("🎬", "Movie"),
    ("🖼️", "Picture"),
    ("🛍️", "Shopping"),
    ("🔥", "Fire"),
    ("💡", "Idea"),
    ("🧩", "Puzzle"),
    ("📊", "Chart"),
    ("🧠", "Brain"),
    ("🦄", "Unicorn"),
    ("🐙", "Octopus"),
    ("🌱", "Seedling"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Lucide,
    Emoji,
    Monogram,
}

pub(super) struct Picker {
    mode: Mode,
    name: String,
    emoji: String,
    color: String,
    automatic_monogram: String,
    search: TextEdit,
    letters: TextEdit,
    custom_emoji: TextEdit,
}

impl Picker {
    fn new(project: &Project) -> Self {
        let (automatic_monogram, color) = project_badge::project_identity(&project.name);
        let mut picker = Self {
            mode: Mode::Lucide,
            name: "folder-code".into(),
            color: project_badge::COLOR_NAMES[color].into(),
            emoji: "💻".into(),
            search: TextEdit::default(),
            letters: TextEdit::default(),
            custom_emoji: TextEdit::default(),
            automatic_monogram,
        };
        picker.letters.replace_text(&picker.automatic_monogram);
        match &project.icon {
            Some(ProjectIcon::Lucide { name, color }) => {
                picker.name.clone_from(name);
                picker.color.clone_from(color);
            }
            Some(ProjectIcon::Monogram { text, color }) => {
                picker.mode = Mode::Monogram;
                picker.letters.replace_text(text);
                picker.color.clone_from(color);
            }
            Some(ProjectIcon::Emoji { emoji }) => {
                picker.mode = Mode::Emoji;
                picker.emoji.clone_from(emoji);
            }
            _ => {}
        }
        picker
    }

    fn selection(&self) -> Option<ProjectIcon> {
        let color = self.color.clone();
        Some(match self.mode {
            Mode::Lucide => ProjectIcon::Lucide {
                name: self.name.clone(),
                color,
            },
            Mode::Emoji => ProjectIcon::Emoji {
                emoji: self.emoji.clone(),
            },
            Mode::Monogram => ProjectIcon::Monogram {
                text: normalized_monogram(&self.letters.text)?,
                color,
            },
        })
    }
}

use crate::connection::ProjectIcon;

fn active_picker_mut(state: &mut State) -> &mut Picker {
    state
        .settings
        .project
        .as_mut()
        .and_then(|form| form.icon_picker.as_mut())
        .expect("the project icon picker has an active draft")
}

impl State {
    fn open_project_icon(&mut self, tx: &mut Tx) {
        let (Some(workspace), Some(form)) = (&self.workspace, &mut self.settings.project) else {
            return;
        };
        if !workspace.project_writes
            || workspace.reconnecting
            || form.pending.contains(&ProjectEditKind::Icon)
        {
            return;
        }
        if let Some(project) = find_project(workspace, &form.project_id) {
            form.icon_picker = Some(Picker::new(project));
            tx.focus_when_shown(DIALOG, false);
        }
    }

    fn close_project_icon(&mut self, tx: &mut Tx) {
        if let Some(form) = &mut self.settings.project {
            form.icon_picker = None;
            tx.focus(OPENER, tx.focus_visible());
        }
    }

    fn save_project_icon(&mut self, tx: &mut Tx) {
        let Some(icon) = active_picker_mut(self).selection() else {
            return;
        };
        self.edit_project_setting(ProjectEdit::Icon(Some(icon)));
        self.close_project_icon(tx);
    }
}

pub(super) fn opener(palette: Palette, enabled: bool) -> El {
    button(OPENER, "Choose icon", 104.0, 28.0, palette)
        .label("Choose a project icon")
        .disabled(!enabled)
        .focusable(enabled)
        .on_press_tx(State::open_project_icon)
}

pub(in crate::ui) fn overlay(state: &State, palette: Palette, width: f32) -> Option<El> {
    let picker = state.settings.project.as_ref()?.icon_picker.as_ref()?;
    let outer = (width - 24.0).clamp(120.0, 512.0);
    let columns = if width >= 640.0 { 10 } else { 8 };
    let tab_list = mode_tabs(picker.mode, palette);
    let mut content = col().w_full().gap(19.0).child(tab_list);
    if picker.mode != Mode::Emoji {
        content = content.child(color_choices(picker, palette, outer));
    }
    content = match picker.mode {
        Mode::Lucide => content.child(lucide_choices(picker, palette, outer, columns)),
        Mode::Emoji => content.child(emoji_choices(picker, palette, outer, columns)),
        Mode::Monogram => content.child(monogram_editor(picker, palette)),
    };
    let height = match picker.mode {
        Mode::Lucide => 511.0,
        Mode::Emoji => 441.0,
        Mode::Monogram => 420.5,
    };
    let valid = picker.selection().is_some();
    let primary = rune::color::hex("#1b4ed8").expect("project icon primary color");
    let primary_palette = Palette {
        ink: Color::WHITE,
        line: primary,
        ..palette
    };
    let footer = row()
        .w_full()
        .h(64.0)
        .gap(8.0)
        .px(24.0)
        .items_center()
        .justify_end()
        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
        .child(
            button("cancel-project-icon", "Cancel", 71.0, 32.0, palette)
                .on_press_tx(State::close_project_icon),
        )
        .child(
            button(
                "save-project-icon",
                "Save icon",
                89.0,
                32.0,
                primary_palette,
            )
            .disabled(!valid)
            .focusable(valid)
            .fill(if valid { primary } else { primary.alpha(0.48) })
            .hover(primary)
            .on_press_tx(State::save_project_icon),
        );
    let popup = col()
        .key(DIALOG)
        .role("dialog")
        .label("Choose project icon")
        .hit()
        .focusable(true)
        .size(outer, height)
        .radius(12.0)
        .clip_rounded(12.0)
        .fill(palette.canvas)
        .border(1.0, palette.line)
        .border_layout()
        .on_key(|state: &mut State, event, tx| {
            if event.key == Key::Escape {
                state.close_project_icon(tx);
                return true;
            }
            false
        })
        .child(
            col()
                .w_full()
                .px(24.0)
                .pt(22.0)
                .pb(16.0)
                .gap(4.0)
                .child(
                    text("Choose project icon")
                        .font(SECTION)
                        .color(palette.ink)
                        .role("heading"),
                )
                .child(
                    text("Choose an icon, emoji, or monogram.")
                        .font(PROJECT_FONT)
                        .color(palette.muted),
                ),
        )
        .child(content.px(24.0).min_h(0.0).grow(1.0).scroll(ScrollSpec {
            stick_bottom: false,
        }))
        .child(footer);
    Some(
        col()
            .key("project-icon-picker-overlay")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .center()
            .child(
                el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .fill(Color::BLACK.alpha(0.54))
                    .on_press_tx(State::close_project_icon),
            )
            .child(popup),
    )
}

fn mode_tabs(active: Mode, palette: Palette) -> El {
    let mut tabs = row()
        .w_full()
        .h(30.0)
        .items_center()
        .padding(2.0)
        .gap(2.0)
        .radius(7.0)
        .fill(palette.hover)
        .role("group")
        .label("Icon type");
    for (mode, label, key) in [
        (Mode::Lucide, "Icons", "project-icon-tab-icons"),
        (Mode::Emoji, "Emoji", "project-icon-tab-emoji"),
        (Mode::Monogram, "Monogram", "project-icon-tab-monogram"),
    ] {
        let selected = mode == active;
        tabs = tabs.child(
            el().key(key)
                .role("button")
                .label(label)
                .checked(selected)
                .focusable(true)
                .focus_ring(1.0, 1.0, palette.accent)
                .grow(1.0)
                .h(26.0)
                .radius(5.0)
                .center()
                .fill(if selected {
                    palette.canvas
                } else {
                    Color::TRANSPARENT
                })
                .hover(palette.line)
                .on_press_tx(move |state: &mut State, _| active_picker_mut(state).mode = mode)
                .child(text(label).font(CONTROL).color(palette.ink)),
        );
    }
    tabs
}

fn color_choices(picker: &Picker, palette: Palette, outer: f32) -> El {
    let per_row = (((outer - 42.0) / 30.0) as usize).clamp(1, project_badge::COLOR_NAMES.len());
    let mut colors = col().w_full().gap(6.0).role("group").label("Icon color");
    for names in project_badge::COLOR_NAMES.chunks(per_row) {
        let mut swatches = row().w_full().gap(6.0);
        for &name in names {
            let index = project_badge::COLOR_NAMES
                .iter()
                .position(|color| *color == name)
                .expect("project icon swatch");
            let swatch = rune::color::hex(SWATCHES[index]).expect("static project icon color");
            let label = title_case(name);
            let selected = name == picker.color;
            swatches = swatches.child(
                el().key(format!("project-icon-color-{name}"))
                    .role("button")
                    .label(label)
                    .focusable(true)
                    .checked(selected)
                    .focus_ring(1.0, 1.0, palette.accent)
                    .size(24.0, 24.0)
                    .radius(12.0)
                    .center()
                    .border(
                        1.0,
                        if selected {
                            palette.ink.alpha(0.64)
                        } else {
                            Color::TRANSPARENT
                        },
                    )
                    .on_press_tx(move |state: &mut State, _| {
                        active_picker_mut(state).color = name.into()
                    })
                    .child(el().size(16.0, 16.0).radius(8.0).fill(swatch)),
            );
        }
        colors = colors.child(swatches);
    }
    col()
        .w_full()
        .gap(8.0)
        .child(text("Color").font(DESCRIPTION).color(palette.muted))
        .child(colors)
}

fn lucide_choices(picker: &Picker, palette: Palette, outer: f32, columns: usize) -> El {
    let query = picker
        .search
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let options: Vec<_> = if query.is_empty() {
        POPULAR.to_vec()
    } else {
        project_badge::icon_names()
            .iter()
            .map(String::as_str)
            .filter(|name| name.contains(&query))
            .take(60)
            .collect()
    };
    let mut choices = col().w_full().gap(17.5).child(
        el().w_full().px(1.0).child(
            field(&picker.search, SEARCH, "Search Lucide icons", palette)
                .role("searchbox")
                .placeholder("Search all Lucide icons", palette.muted)
                .on_edit(|state: &mut State| &mut active_picker_mut(state).search),
        ),
    );
    if options.is_empty() {
        choices = choices.child(
            row().w_full().h(64.0).center().child(
                text("No icons found.")
                    .font(PROJECT_FONT)
                    .color(palette.muted),
            ),
        );
    } else {
        let foreground = project_badge::named_color(&picker.color, palette.dark);
        choices = choices.child(grid(&options, outer, columns, |name, size| {
            let owned = (*name).to_owned();
            let mut tile = tile(
                format!("project-icon-choice-{name}"),
                &title_case(name),
                size,
                picker.name == *name,
                palette,
            )
            .on_press_tx(move |state: &mut State, _| {
                active_picker_mut(state).name.clone_from(&owned)
            });
            if let Some(glyph) = project_badge::lookup_icon(name) {
                tile = tile.child(icon(glyph, 20.0).color(foreground).a11y_hidden());
            }
            tile
        }));
    }
    choices
}

fn emoji_choices(picker: &Picker, palette: Palette, outer: f32, columns: usize) -> El {
    col()
        .w_full()
        .gap(12.0)
        .child(grid(EMOJIS, outer, columns, |&(value, label), size| {
            tile(
                format!("project-icon-emoji-{label}"),
                label,
                size,
                picker.emoji == value,
                palette,
            )
            .on_press_tx(move |state: &mut State, _| active_picker_mut(state).emoji = value.into())
            .child(
                text(value)
                    .font(TextStyle::new(20.0, 400.0).lh(24.0))
                    .color(palette.ink),
            )
        }))
        .child(
            col()
                .w_full()
                .gap(8.0)
                .child(
                    text("Or paste any emoji")
                        .font(DESCRIPTION)
                        .color(palette.muted),
                )
                .child(
                    field(
                        &picker.custom_emoji,
                        "project-icon-custom-emoji",
                        "Custom emoji",
                        palette,
                    )
                    .placeholder("Paste an emoji", palette.muted)
                    .on_edit(|state: &mut State| &mut active_picker_mut(state).custom_emoji)
                    .on_input_change(|state: &mut State, _| {
                        let picker = active_picker_mut(state);
                        if let Some(emoji) = first_emoji(&picker.custom_emoji.text) {
                            picker.emoji = emoji.to_owned();
                        }
                    }),
                ),
        )
}

fn monogram_editor(picker: &Picker, palette: Palette) -> El {
    let monogram = normalized_monogram(&picker.letters.text);
    row()
        .w_full()
        .items_center()
        .gap(16.0)
        .py(8.0)
        .child(project_badge::monogram(
            monogram.as_deref().unwrap_or(&picker.automatic_monogram),
            project_badge::named_color(&picker.color, palette.dark),
            48.0,
        ))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(8.0)
                .child(text("Letters").font(CONTROL).color(palette.ink))
                .child(
                    field(&picker.letters, "project-icon-letters", "Letters", palette)
                        .on_edit(|state: &mut State| &mut active_picker_mut(state).letters),
                )
                .child(
                    text("One or two letters or numbers.")
                        .font(DESCRIPTION)
                        .color(palette.muted),
                ),
        )
}

fn grid<T>(items: &[T], width: f32, columns: usize, cell: impl Fn(&T, f32) -> El) -> El {
    let size = ((width - 54.0 - 4.0 * (columns - 1) as f32) / columns as f32).max(12.0);
    let mut rows = col()
        .w_full()
        .gap(4.0)
        .padding(2.0)
        .max_h(256.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    for group in items.chunks(columns) {
        let mut tiles = row().w_full().gap(4.0);
        for entry in group {
            tiles = tiles.child(cell(entry, size));
        }
        rows = rows.child(tiles);
    }
    rows
}

fn tile(key: String, label: &str, size: f32, selected: bool, palette: Palette) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .checked(selected)
        .focusable(true)
        .focus_ring(1.0, 1.0, palette.accent)
        .size(size, size)
        .radius(6.0)
        .center()
        .fill(if selected {
            palette.hover
        } else {
            Color::TRANSPARENT
        })
        .border(
            1.0,
            if selected {
                palette.line
            } else {
                Color::TRANSPARENT
            },
        )
        .hover(palette.hover)
}

fn field(edit: &TextEdit, key: &str, label: &str, palette: Palette) -> El {
    text_input(edit, PROJECT_FONT)
        .key(key)
        .label(label)
        .single_line()
        .w_full()
        .h(30.0)
        .px(10.0)
        .radius(6.0)
        .color(palette.ink)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent)
}

fn button(key: &str, label: &str, width: f32, height: f32, palette: Palette) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .focus_ring(1.0, 1.0, palette.accent)
        .size(width, height)
        .radius(6.0)
        .center()
        .border(1.0, palette.line)
        .hover(palette.hover)
        .child(text(label).font(CONTROL).color(palette.ink))
}

fn title_case(name: &str) -> String {
    name.split('-')
        .filter_map(|word| {
            let mut chars = word.chars();
            Some(
                chars
                    .next()?
                    .to_uppercase()
                    .chain(chars)
                    .collect::<String>(),
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}
