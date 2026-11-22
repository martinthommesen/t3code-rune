use rune::{Color, El, Icon, Key, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};

use super::{Palette, State, error_text, sidebar_header};

mod appearance;
mod project;

pub(super) use project::{
    gear as project_settings_gear, poll as poll_project, settle as settle_project_edit,
};
pub(super) use project::{poll_files as poll_project_files, settle_files as settle_project_files};

const CONTROL: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const SECTION: TextStyle = TextStyle::new(16.0, 600.0).lh(24.0);
const DESCRIPTION: TextStyle = TextStyle::new(12.0, 400.0).lh(18.0);
const DIALOG: &str = "client-settings-dialog";

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $path, "</g>"),
        }
    };
}

static GEAR: Icon = glyph!(
    "settings",
    r#"<path d="M12.2 2h-.4a2 2 0 0 0-2 2v.2a2 2 0 0 1-1 1.7l-.4.2a2 2 0 0 1-2 0l-.1-.1a2 2 0 0 0-2.7.7l-.2.4a2 2 0 0 0 .7 2.7l.2.1a2 2 0 0 1 1 1.7v.4a2 2 0 0 1-1 1.8l-.2.1a2 2 0 0 0-.7 2.7l.2.4a2 2 0 0 0 2.7.7l.1-.1a2 2 0 0 1 2 0l.4.2a2 2 0 0 1 1 1.7v.2a2 2 0 0 0 2 2h.4a2 2 0 0 0 2-2v-.2a2 2 0 0 1 1-1.7l.4-.2a2 2 0 0 1 2 0l.1.1a2 2 0 0 0 2.7-.7l.2-.4a2 2 0 0 0-.7-2.7l-.2-.1a2 2 0 0 1-1-1.8v-.4a2 2 0 0 1 1-1.7l.2-.1a2 2 0 0 0 .7-2.7l-.2-.4a2 2 0 0 0-2.7-.7l-.1.1a2 2 0 0 1-2 0l-.4-.2a2 2 0 0 1-1-1.7V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>"#
);
static GENERAL: Icon = glyph!(
    "settings-2",
    r#"<path d="M20 7h-9M14 17H5"/><circle cx="7" cy="7" r="3"/><circle cx="17" cy="17" r="3"/>"#
);
static APPEARANCE: Icon = glyph!(
    "palette",
    r#"<path d="M12 3a9 9 0 1 0 0 18h1.5a1.5 1.5 0 0 0 0-3H13a1 1 0 0 1 0-2h2a6 6 0 0 0 0-12z"/><circle cx="7.5" cy="12" r=".5"/><circle cx="9" cy="8" r=".5"/><circle cx="13" cy="7.5" r=".5"/><circle cx="16.5" cy="10" r=".5"/>"#
);
static CLOSE: Icon = glyph!("x", r#"<path d="m18 6-12 12M6 6l12 12"/>"#);
static CHEVRON: Icon = glyph!("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static RESET: Icon = glyph!(
    "undo-2",
    r#"<path d="m9 14-5-5 5-5M4 9h10.5a5.5 5.5 0 1 1 0 11H11"/>"#
);

pub(super) struct Panel {
    open: bool,
    error: Option<String>,
    page: Page,
    project: Option<project::Form>,
    return_focus: &'static str,
}

impl Panel {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            open: false,
            error: None,
            page: Page::General,
            project: None,
            return_focus: "open-client-settings",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    General,
    Appearance,
    Projects,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Projects => "Projects",
        }
    }
}

impl State {
    fn open_client_settings(&mut self, tx: &mut Tx) {
        self.settings.open = true;
        self.settings.page = Page::General;
        self.settings.return_focus = "open-client-settings";
        tx.focus(DIALOG, false);
    }

    fn close_client_settings(&mut self, tx: &mut Tx) {
        self.save_project_name();
        self.settings.open = false;
        self.settings.error = None;
        tx.focus(self.settings.return_focus, tx.focus_visible());
    }

    fn toggle_terminal_close_confirmation(&mut self, _: &mut Tx) {
        self.set_terminal_close_confirmation(!self.client_settings.confirm_terminal_close());
    }

    fn reset_terminal_close_confirmation(&mut self, tx: &mut Tx) {
        self.set_terminal_close_confirmation(true);
        tx.focus("confirm-terminal-closing", tx.focus_visible());
    }

    fn set_terminal_close_confirmation(&mut self, enabled: bool) {
        self.settings.error = self
            .client_settings
            .set_confirm_terminal_close(enabled)
            .err()
            .map(|error| format!("Could not save client settings. {error}"));
    }

    fn select_settings_page(&mut self, page: Page) {
        self.save_project_name();
        self.settings.page = page;
        self.settings.error = None;
    }
}

pub(super) fn opener(palette: Palette) -> El {
    icon_button(
        "open-client-settings",
        "Settings",
        &GEAR,
        16.0,
        32.0,
        palette,
        State::open_client_settings,
    )
    .on_key(|state: &mut State, event, tx| {
        sidebar_header::activate_on_return(state, event, tx, State::open_client_settings)
    })
}

pub(super) fn overlay(state: &State, palette: Palette, width: f32, height: f32) -> Option<El> {
    if !state.settings.open {
        return None;
    }
    let canvas = if palette.dark {
        Color::BLACK
    } else {
        rune::color::hex("#fcfcfc").expect("static settings background")
    };
    let backdrop = canvas.alpha(if palette.dark { 0.64 } else { 0.60 });
    let palette = Palette { canvas, ..palette };
    let inset = if width >= 1024.0 {
        40.0
    } else if width >= 640.0 {
        24.0
    } else {
        0.0
    };
    let navigation_width = if width >= 768.0 { 240.0 } else { 56.0 };
    let content_width = (width - 2.0 * inset).min(1280.0) - navigation_width;
    let radius = if width >= 640.0 { 16.0 } else { 0.0 };
    let color = sidebar_header::Colors::new(palette.dark);
    let page = state.settings.page;
    let content = match page {
        Page::General => general(state, palette, content_width, width < 640.0),
        Page::Appearance => appearance::page(state, palette, content_width),
        Page::Projects => project::page(state, palette, content_width),
    };
    let picker = project::icon_overlay(state, palette, width)
        .or_else(|| project::file_overlay(state, palette, width, height));
    let dialog = row()
        .key(DIALOG)
        .hit()
        .role("dialog")
        .label("Settings")
        .focusable(true)
        .w_full()
        .h_full()
        .max_w(1280.0)
        .max_h(960.0)
        .radius(radius)
        .clip_rounded(radius)
        .fill(palette.canvas)
        .border(1.0, color.border)
        .border_layout()
        .on_key(|state: &mut State, event, tx| {
            if event.key == Key::Escape {
                state.close_client_settings(tx);
                return true;
            }
            false
        })
        .child(navigation(page, navigation_width, color))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .child(header(page, palette, color.border))
                .child(content),
        );
    let dialog = if picker.is_some() {
        dialog.inert()
    } else {
        dialog
    };
    let mut overlay = col()
        .key("client-settings-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .padding(inset)
        .center()
        .child(
            el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(backdrop)
                .on_press_tx(State::close_client_settings),
        )
        .child(dialog);
    if let Some(picker) = picker {
        overlay = overlay.child(picker);
    }
    Some(overlay)
}

fn navigation(active: Page, width: f32, colors: sidebar_header::Colors) -> El {
    let mut navigation = col()
        .role("navigation")
        .label("Settings navigation")
        .w(width)
        .min_w(width)
        .h_full()
        .padding(8.0)
        .gap(4.0)
        .fill(colors.background)
        .border_sides([0.0, 1.0, 0.0, 0.0], colors.border);
    for (page, glyph, key) in [
        (Page::General, &GENERAL, "settings-general"),
        (Page::Appearance, &APPEARANCE, "settings-appearance"),
        (Page::Projects, &GEAR, "settings-projects"),
    ] {
        let selected = active == page;
        let ink = if selected {
            colors.foreground
        } else {
            colors.muted
        };
        let mut button = row()
            .key(key)
            .role("button")
            .label(page.label())
            .checked(selected)
            .focusable(true)
            .focus_ring(1.0, 1.0, colors.ring)
            .h(32.0)
            .w_full()
            .items_center()
            .px(8.0)
            .gap(8.0)
            .radius(6.0)
            .fill(if selected {
                colors.hover
            } else {
                Color::TRANSPARENT
            })
            .hover(colors.hover)
            .child(icon(glyph, 16.0).color(ink).a11y_hidden())
            .on_press_tx(move |state: &mut State, _| state.select_settings_page(page));
        if width > 56.0 {
            button = button.child(text(page.label()).font(CONTROL).color(ink));
        }
        navigation = navigation.child(button);
    }
    navigation
}

fn header(page: Page, palette: Palette, border: Color) -> El {
    row()
        .w_full()
        .h(48.0)
        .items_center()
        .gap(8.0)
        .pl(20.0)
        .pr(8.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], border)
        .child(text("Settings").font(CONTROL).color(palette.muted))
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden())
        .child(
            el().grow(1.0)
                .child(text(page.label()).font(CONTROL).color(palette.ink)),
        )
        .child(icon_button(
            "close-client-settings",
            "Close",
            &CLOSE,
            16.0,
            32.0,
            palette,
            State::close_client_settings,
        ))
}

fn general(state: &State, palette: Palette, width: f32, large_touch_switch: bool) -> El {
    let enabled = state.client_settings.confirm_terminal_close();
    let switch = confirmation_switch(enabled, large_touch_switch, palette);
    let mut title = row().items_center().gap(6.0).child(
        text("Terminal close confirmation")
            .font(CONTROL)
            .color(palette.ink),
    );
    if !enabled {
        title = title.child(icon_button(
            "reset-terminal-close-confirmation",
            "Reset terminal close confirmation to default",
            &RESET,
            12.0,
            20.0,
            palette,
            State::reset_terminal_close_confirmation,
        ));
    }
    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text("Ask before closing terminals and stopping their running processes.")
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let control = if width >= 560.0 {
        row().items_center().gap(32.0).child(label).child(switch)
    } else {
        col().gap(12.0).child(label).child(switch)
    };
    let mut group = col()
        .w_full()
        .gap(10.0)
        .child(
            text("Confirmations")
                .font(SECTION)
                .color(palette.ink)
                .role("heading")
                .px(16.0),
        )
        .child(
            control
                .key("terminal-close-confirmation")
                .w_full()
                .px(16.0)
                .py(12.0)
                .radius(12.0)
                .border(1.0, palette.line),
        );
    if let Some(error) = &state.settings.error {
        group = group.child(error_text(error, palette));
    }
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .padding(24.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(group.max_w(800.0))
}

fn confirmation_switch(enabled: bool, large: bool, palette: Palette) -> El {
    let (width, height, thumb) = if large {
        (38.0, 22.0, 18.0)
    } else {
        (30.0, 18.0, 14.0)
    };
    let neutral = rune::color::hex(if palette.dark { "#2e2e2e" } else { "#d4d4d8" })
        .expect("static switch palette");
    el().key("confirm-terminal-closing")
        .role("switch")
        .label("Confirm terminal closing")
        .checked(enabled)
        .focusable(true)
        .focus_ring(2.0, 1.0, palette.accent)
        .w(width)
        .h(height)
        .radius(height * 0.5)
        .fill(if enabled { palette.accent } else { neutral })
        .on_press_tx(State::toggle_terminal_close_confirmation)
        .child(
            el().absolute(if enabled { width - thumb - 2.0 } else { 2.0 }, 2.0)
                .w(thumb)
                .h(thumb)
                .radius(thumb * 0.5)
                .fill(if !enabled && palette.dark {
                    palette.muted
                } else {
                    palette.canvas
                }),
        )
}

fn icon_button(
    key: &str,
    label: &str,
    glyph: &'static Icon,
    icon_size: f32,
    size: f32,
    palette: Palette,
    press: fn(&mut State, &mut Tx),
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .w(size)
        .h(size)
        .radius(6.0)
        .center()
        .focusable(true)
        .focus_ring(1.0, 2.0, palette.accent)
        .hover(palette.hover)
        .on_press_tx(press)
        .child(icon(glyph, icon_size).color(palette.muted).a11y_hidden())
}
