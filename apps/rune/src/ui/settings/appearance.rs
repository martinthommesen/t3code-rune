use rune::color::hex;
use rune::{Color, El, ScrollSpec, TextStyle, Tx, col, el, row, text};

use crate::client_settings::AppearanceMode;

use super::{Palette, SECTION, State, error_text};

const PREVIEW_HEIGHT: f32 = 140.0;
const MODE_LABEL: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);

impl State {
    pub(in crate::ui) fn set_appearance_mode(&mut self, mode: AppearanceMode) {
        self.settings.error = self
            .client_settings
            .set_appearance_mode(mode)
            .err()
            .map(|error| format!("Could not save the appearance mode. {error}"));
    }
}

pub(super) fn page(state: &State, palette: Palette, content_width: f32) -> El {
    let page_width = ((content_width - 14.0).min(896.0) - 48.0).max(1.0);
    let preview_width = ((page_width - 24.0) / 3.0 - 18.0).max(2.0);
    let modes = [
        (
            AppearanceMode::System,
            "System",
            "Follow the system appearance",
        ),
        (AppearanceMode::Light, "Light", "Use light mode"),
        (AppearanceMode::Dark, "Dark", "Use dark mode"),
    ];
    let mut choices = row()
        .w_full()
        .gap(12.0)
        .role("group")
        .label("Appearance mode");
    for (mode, title, label) in modes {
        let selected = state.client_settings.appearance_mode() == mode;
        let mut tile = col()
            .key(format!("appearance-{}", mode.as_str()))
            .role("button")
            .label(label)
            .checked(selected)
            .focusable(true)
            .focus_ring(2.0, 1.0, palette.muted)
            .grow(1.0)
            .min_w(0.0)
            .h(180.0)
            .items_center()
            .gap(6.0)
            .padding(8.0)
            .radius(12.0)
            .fill(if selected {
                palette.canvas.mix(palette.hover, 0.3)
            } else {
                palette.field
            })
            .border(
                1.0,
                if selected {
                    Color::TRANSPARENT
                } else {
                    palette.line
                },
            )
            .border_layout()
            .hover(palette.hover)
            .on_press_tx(move |state: &mut State, _: &mut Tx| state.set_appearance_mode(mode))
            .child(preview(mode, preview_width, palette))
            .child(text(title).font(MODE_LABEL).color(if selected {
                palette.ink
            } else {
                palette.muted
            }));
        if selected {
            tile = tile.child(
                el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .radius(12.0)
                    .border(2.0, palette.accent)
                    .a11y_hidden(),
            );
        }
        choices = choices.child(tile);
    }
    let mut section = col()
        .key("appearance")
        .focusable(true)
        .tab_index(-1)
        .no_focus_ring()
        .w(page_width)
        .gap(12.0)
        .child(
            row().h(28.0).items_center().px(16.0).child(
                text("Color scheme")
                    .font(SECTION)
                    .color(palette.ink)
                    .role("heading"),
            ),
        )
        .child(choices);
    if let Some(error) = &state.settings.error {
        section = section.child(error_text(error, palette));
    }
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(24.0)
        .items_center()
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(section)
}

fn preview(mode: AppearanceMode, width: f32, palette: Palette) -> El {
    let picture = row()
        .w_full()
        .h(PREVIEW_HEIGHT)
        .radius(8.0)
        .clip_rounded(8.0)
        .border(1.0, palette.line)
        .a11y_hidden();
    if mode == AppearanceMode::System {
        let half = (width - 2.0) * 0.5;
        picture
            .gap(2.0)
            .child(
                el().w(half)
                    .h_full()
                    .clip_rounded(0.0)
                    .child(wireframe(width, false).absolute(0.0, 0.0)),
            )
            .child(
                el().w(half)
                    .h_full()
                    .clip_rounded(0.0)
                    .child(wireframe(width, true).absolute(-half - 2.0, 0.0)),
            )
    } else {
        picture.child(wireframe(width, mode == AppearanceMode::Dark))
    }
}

fn wireframe(width: f32, dark: bool) -> El {
    let colors = if dark {
        [
            "#000000", "#000000", "#111111", "#141414", "#141414", "#346bf1",
        ]
    } else {
        [
            "#fcfcfc", "#fafafa", "#ffffff", "#f4f4f5", "#f4f4f5", "#1b4ed8",
        ]
    };
    let [canvas, sidebar, surface, accent, message, action] = colors.map(color);
    let line = color("#7f7f7f").alpha(0.25);
    let rect = |x, y, w, h, radius, fill| {
        el().absolute(x * width, y * PREVIEW_HEIGHT)
            .w(w * width)
            .h(h * PREVIEW_HEIGHT)
            .radius(radius)
            .fill(fill)
    };
    let status = rect(0.75, 0.08, 0.20, 0.46, 8.0, surface)
        .border(1.0, line)
        .child(status_line(width, 0.10, color("#34d399"), line))
        .child(status_line(width, 0.40, action, line))
        .child(status_line(width, 0.70, color("#fbbf24"), line));
    let composer = rect(0.26, 0.77, 0.68, 0.15, 6.0, surface)
        .border(1.0, line)
        .child(
            el().absolute(4.0, 8.0)
                .w(width * 0.22)
                .h(5.5)
                .radius(3.0)
                .fill(line),
        )
        .child(
            el().pin(None, Some(4.0), Some(4.0), None)
                .w(12.0)
                .h(12.0)
                .radius(6.0)
                .fill(action),
        );
    el().w(width)
        .h(PREVIEW_HEIGHT)
        .fill(canvas)
        .child(rect(0.0, 0.0, 0.22, 1.0, 0.0, sidebar).border_sides([0.0, 1.0, 0.0, 0.0], line))
        .child(rect(0.03, 0.08, 0.16, 0.08, 6.0, surface).border(1.0, line))
        .child(rect(0.03, 0.22, 0.16, 0.07, 6.0, accent))
        .child(rect(0.03, 0.32, 0.16, 0.07, 6.0, message.alpha(0.7)))
        .child(rect(0.03, 0.42, 0.16, 0.07, 6.0, message.alpha(0.5)))
        .child(rect(0.48, 0.11, 0.24, 0.09, 8.0, message))
        .child(rect(0.27, 0.28, 0.34, 0.05, 4.0, line))
        .child(rect(0.27, 0.38, 0.26, 0.05, 4.0, line))
        .child(composer)
        .child(status)
}

fn status_line(width: f32, y: f32, dot: Color, line: Color) -> El {
    let status_width = width * 0.20;
    row()
        .absolute(0.11 * status_width, y * 0.46 * PREVIEW_HEIGHT)
        .items_center()
        .w(0.78 * status_width)
        .h(0.20 * 0.46 * PREVIEW_HEIGHT)
        .gap(4.0)
        .child(el().w(3.4).h(3.4).radius(1.7).fill(dot.alpha(0.55)))
        .child(
            el().w(0.52 * 0.78 * status_width)
                .h(3.9)
                .radius(2.0)
                .fill(line),
        )
}

fn color(raw: &str) -> Color {
    hex(raw).expect("static appearance preview color")
}
