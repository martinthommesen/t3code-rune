use std::sync::OnceLock;

use rune::color::hex;
use rune::{
    ChromeInsets, Color, El, Icon, Key, KeyEvent, Mods, Pixels, TextStyle, Tx, el, icon, row, text,
};

use super::State;
use super::navigation_history::{Destination, Direction};

pub(super) const WIDTH: f32 = 256.0;
pub(super) const HEIGHT: f32 = if cfg!(target_os = "macos") {
    52.0
} else {
    40.0
};
const CONTROL_SIZE: f32 = 28.0;
const WORDMARK_SIZE: f32 = 16.84375;
const MIN_CHROME_WIDTH: f32 = 181.0;

#[derive(Clone, Copy)]
pub(super) struct Layout {
    leading: f32,
    height: f32,
    width: f32,
}

impl Layout {
    pub(super) fn new(insets: ChromeInsets, caption_leading: f32) -> Self {
        Self {
            leading: if cfg!(target_os = "macos") && insets.leading > 0.0 {
                insets.leading + 14.0
            } else {
                12.0 + caption_leading
            },
            height: HEIGHT.max(insets.height),
            width: WIDTH.max(MIN_CHROME_WIDTH + caption_leading),
        }
    }

    pub(super) fn content_left(self) -> f32 {
        self.leading + CONTROL_SIZE + 12.0
    }

    pub(super) fn height(self) -> f32 {
        self.height
    }

    pub(super) fn width(self) -> f32 {
        self.width
    }
}

#[derive(Clone, Copy)]
pub(super) struct Colors {
    pub(super) background: Color,
    pub(super) border: Color,
    pub(super) foreground: Color,
    pub(super) muted: Color,
    pub(super) hover: Color,
    pub(super) ring: Color,
}

impl Colors {
    pub(super) fn new(dark: bool) -> Self {
        let [background, border, foreground, muted, hover, ring] = if dark {
            [
                "#000000", "#141414", "#f1f3f7", "#a3a3a3", "#191a1d", "#526cca",
            ]
        } else {
            [
                "#fafafa", "#e4e4e7", "#18181b", "#71717a", "#f4f4f5", "#4765c2",
            ]
        }
        .map(|value| hex(value).expect("static sidebar palette"));
        Self {
            background,
            border,
            foreground,
            muted,
            hover,
            ring,
        }
    }
}

static WORDMARK: Icon = Icon {
    name: "t3-wordmark",
    view: 94.3941,
    body: r#"<g transform="translate(-15.5309 -18.28295)"><path d="M33.4509 93V47.56H15.5309V37H64.3309V47.56H46.4109V93H33.4509ZM86.7253 93.96C82.832 93.96 78.9653 93.4533 75.1253 92.44C71.2853 91.3733 68.032 89.88 65.3653 87.96L70.4053 78.04C72.5386 79.5867 75.0186 80.8133 77.8453 81.72C80.672 82.6267 83.5253 83.08 86.4053 83.08C89.6586 83.08 92.2186 82.44 94.0853 81.16C95.952 79.88 96.8853 78.12 96.8853 75.88C96.8853 73.7467 96.0586 72.0667 94.4053 70.84C92.752 69.6133 90.0853 69 86.4053 69H80.4853V60.44L96.0853 42.76L97.5253 47.4H68.1653V37H107.365V45.4L91.8453 63.08L85.2853 59.32H89.0453C95.9253 59.32 101.125 60.8667 104.645 63.96C108.165 67.0533 109.925 71.0267 109.925 75.88C109.925 79.0267 109.099 81.9867 107.445 84.76C105.792 87.48 103.259 89.6933 99.8453 91.4C96.432 93.1067 92.0586 93.96 86.7253 93.96Z" fill="currentColor"/></g>"#,
};

static BACK: Icon = Icon {
    name: "arrow-left",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m12 19-7-7 7-7"/><path d="M19 12H5"/></g>"#,
};

static FORWARD: Icon = Icon {
    name: "arrow-right",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="m12 5 7 7-7 7"/></g>"#,
};

static CLOSE_SIDEBAR: Icon = Icon {
    name: "panel-left-close",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/><path d="m16 15-3-3 3-3"/></g>"#,
};

static OPEN_SIDEBAR: Icon = Icon {
    name: "panel-left",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/></g>"#,
};

pub(super) fn surface_grain() -> Option<&'static Pixels> {
    static GRAIN: OnceLock<Option<Pixels>> = OnceLock::new();
    GRAIN.get_or_init(|| {
        let svg = br#"<svg viewBox="0 0 256 256" xmlns="http://www.w3.org/2000/svg"><filter id="n"><feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="4" stitchTiles="stitch"/></filter><rect width="100%" height="100%" filter="url(#n)" opacity="0.035"/></svg>"#;
        let (width, height, mut rgba) = rune::image::svg_rgba(svg, 256).ok()?;
        // Chromium starts the SVG turbulence tile one sample later on each axis than resvg.
        let stride = width as usize * 4;
        for row in rgba.chunks_exact_mut(stride) {
            row.rotate_left(4);
        }
        rgba.rotate_left(stride);
        let pixels = Pixels::new();
        pixels.set(width, height, rgba);
        Some(pixels)
    }).as_ref()
}

pub(super) fn view(state: &State, layout: Layout, colors: Colors) -> El {
    let mut header = state.window_chrome.drag_region(
        row()
            .key("sidebar-titlebar")
            .w_full()
            .h(layout.height)
            .clip_rounded(0.0)
            .center_y()
            .pl(layout.content_left()),
    );
    if let Some(grain) = surface_grain() {
        header = header.child(
            el().absolute(0.0, 0.0)
                .size(WIDTH, WIDTH)
                .pixels(grain)
                .a11y_hidden(),
        );
    }
    header
        .child(
            row()
                .key("home")
                .h(CONTROL_SIZE)
                .center_y()
                .gap(4.0)
                .radius(6.0)
                .role("link")
                .label("T3 Code home")
                .focusable(true)
                .focus_ring(2.0, -2.0, colors.ring)
                .on_press(|state: &mut State| state.navigate(Destination::Home))
                .on_key(|state: &mut State, event, tx| {
                    activate_on_return(state, event, tx, |state, _| {
                        state.navigate(Destination::Home)
                    })
                })
                .child(
                    icon(&WORDMARK, WORDMARK_SIZE)
                        .color(colors.foreground)
                        .a11y_hidden(),
                )
                .child(
                    text("Code")
                        .font(TextStyle::new(14.0, 500.0).lh(20.0).tracking(-0.35))
                        .color(colors.muted),
                ),
        )
        .child(
            row()
                .ml(4.0)
                .child(history_button(state, Direction::Back, colors))
                .child(history_button(state, Direction::Forward, colors)),
        )
        .child(toggle(state, layout, colors))
}

pub(super) fn toggle(state: &State, layout: Layout, colors: Colors) -> El {
    let glyph = if state.sidebar_open {
        &CLOSE_SIDEBAR
    } else {
        &OPEN_SIDEBAR
    };
    control("toggle-sidebar", "Toggle main sidebar", colors)
        .absolute(layout.leading + 1.0, (layout.height - CONTROL_SIZE) / 2.0)
        .expanded(state.sidebar_open)
        .focusable(true)
        .on_press_tx(toggle_from_button)
        .on_key(|state: &mut State, event, tx| {
            activate_on_return(state, event, tx, toggle_from_button)
        })
        .child(icon(glyph, 16.0).color(colors.muted).a11y_hidden())
}

fn history_button(state: &State, direction: Direction, colors: Colors) -> El {
    let (key, label, arrow) = match direction {
        Direction::Back => ("go-back", "Go back", &BACK),
        Direction::Forward => ("go-forward", "Go forward", &FORWARD),
    };
    let enabled = state.can_go(direction);
    let button = control(key, label, colors).child(
        icon(arrow, 14.0)
            .color(if enabled {
                colors.muted
            } else {
                colors.muted.alpha(0.64)
            })
            .a11y_hidden(),
    );
    if enabled {
        button
            .focusable(true)
            .on_press(move |state: &mut State| state.go(direction))
            .on_key(move |state: &mut State, event, tx| {
                activate_on_return(state, event, tx, |state, _| state.go(direction))
            })
    } else {
        button.disabled(true)
    }
}

fn toggle_from_button(state: &mut State, tx: &mut Tx) {
    state.toggle_sidebar();
    tx.focus_when_shown("toggle-sidebar", tx.focus_visible());
}

impl State {
    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        let _ = self.client_settings.set_sidebar_open(self.sidebar_open);
    }
}

pub(super) fn activate_on_return(
    state: &mut State,
    event: &KeyEvent,
    tx: &mut Tx,
    press: impl FnOnce(&mut State, &mut Tx),
) -> bool {
    if event.key != Key::Enter || event.mods != Mods::default() {
        return false;
    }
    if !tx.key_repeat() {
        press(state, tx);
    }
    true
}

pub(super) fn shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let mods = event.mods;
    if state.workspace.is_none()
        || state.onboarding.is_active()
        || state.settings.is_open()
        || state.add_project.is_some()
        || state.new_thread_picker.is_some()
        || state.terminal_close_confirmation_open()
    {
        return false;
    }
    let terminal_focus = tx.focused() == Some(super::terminal::INPUT_KEY);
    let control = mods.ctrl || (!cfg!(target_os = "macos") && mods.cmd);
    match state.resolve_shortcut(event, tx) {
        Some("sidebar.toggle") if !(control && terminal_focus) => {
            if !tx.key_repeat() {
                state.toggle_sidebar();
            }
            return true;
        }
        Some("chat.new") if !terminal_focus => {
            if !tx.key_repeat() {
                state.start_new_thread_shortcut(tx);
            }
            return true;
        }
        _ => {}
    }
    if terminal_focus
        || mods.shift
        || mods.alt
        || (cfg!(target_os = "macos") && mods.ctrl)
        || (!cfg!(target_os = "macos") && mods.logo)
    {
        return false;
    }
    let direction = match event.key {
        Key::Command('[') => Direction::Back,
        Key::Command(']') => Direction::Forward,
        _ => return false,
    };
    if !tx.key_repeat() {
        state.go(direction);
    }
    true
}

fn control(key: &str, label: &str, colors: Colors) -> El {
    el().key(key)
        .size(CONTROL_SIZE, CONTROL_SIZE)
        .center()
        .role("button")
        .label(label)
        .radius(8.0)
        .hover(colors.hover)
        .focus_ring(2.0, -1.0, colors.ring)
}
