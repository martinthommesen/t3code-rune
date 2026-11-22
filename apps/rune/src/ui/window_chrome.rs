use std::time::Duration;

use rune::color::hex;
use rune::{App, Color, Drag, El, Icon, Phase, Tx, WindowHandle, el, icon};

use super::State;

#[cfg(target_os = "linux")]
mod wayland;

#[cfg(target_os = "linux")]
mod x11;

const HEIGHT: f32 = 40.0;
const BUTTON_SIZE: f32 = 24.0;
const BUTTON_PITCH: f32 = 37.0;

static MINIMIZE: Icon = Icon {
    name: "window-minimize",
    view: 8.0,
    body: r#"<path fill="currentColor" d="M0 6h8v2H0z"/>"#,
};
static MAXIMIZE: Icon = Icon {
    name: "window-maximize",
    view: 8.0,
    body: r#"<path fill="currentColor" fill-rule="evenodd" d="M0 0h8v8H0zM2 2h4v4H2z"/>"#,
};
static RESTORE: Icon = Icon {
    name: "window-restore",
    view: 8.0,
    body: r#"<path fill="currentColor" fill-rule="evenodd" d="M1 1h6v6H1zM3 3h2v2H3z"/>"#,
};
static CLOSE: Icon = Icon {
    name: "window-close",
    view: 8.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.75" d="M.8 .8l6.4 6.4m0-6.4L.8 7.2"/>"#,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptionButton {
    Minimize,
    Maximize,
    Close,
}

#[derive(PartialEq, Eq)]
struct CaptionLayout {
    leading: Vec<CaptionButton>,
    trailing: Vec<CaptionButton>,
}

impl Default for CaptionLayout {
    fn default() -> Self {
        Self {
            leading: Vec::new(),
            trailing: vec![
                CaptionButton::Minimize,
                CaptionButton::Maximize,
                CaptionButton::Close,
            ],
        }
    }
}

impl CaptionLayout {
    #[cfg(target_os = "linux")]
    fn from_desktop_order(value: &str) -> Self {
        let (left, right) = value.split_once(':').unwrap_or((value, ""));
        let buttons = |side: &str| {
            side.split(',')
                .filter_map(|value| match value.trim() {
                    "minimize" => Some(CaptionButton::Minimize),
                    "maximize" => Some(CaptionButton::Maximize),
                    "close" => Some(CaptionButton::Close),
                    _ => None,
                })
                .collect()
        };
        Self {
            leading: buttons(left),
            trailing: buttons(right),
        }
    }

    fn width(buttons: &[CaptionButton]) -> f32 {
        if buttons.is_empty() {
            0.0
        } else {
            buttons.len() as f32 * BUTTON_PITCH + 2.0
        }
    }
}

#[derive(Default)]
pub(super) struct ChromeState {
    window: Option<WindowHandle>,
    maximized: bool,
    full_screen: bool,
    drag_origin: Option<DragOrigin>,
    caption_layout: CaptionLayout,
    #[cfg(target_os = "linux")]
    resize_frame: Option<x11::ResizeFrame>,
    #[cfg(target_os = "linux")]
    wayland: Option<wayland::Frame>,
}

#[derive(Clone, Copy)]
struct DragOrigin {
    frame: (i32, i32),
    pointer: (i32, i32),
    scale: f64,
    moving: bool,
}

impl ChromeState {
    pub(super) fn poll(&mut self, tx: &mut Tx) -> bool {
        let Some(window) = &self.window else {
            return false;
        };
        tx.wake_after(Duration::from_millis(33));
        #[cfg(target_os = "linux")]
        if let Some(frame) = &mut self.wayland {
            let maximized = window.is_maximized().unwrap_or(self.maximized);
            let full_screen = window.is_full_screen().unwrap_or(self.full_screen);
            let decorated = window.is_decorated().unwrap_or(frame.decorated);
            let changed = (self.maximized, self.full_screen, frame.decorated)
                != (maximized, full_screen, decorated);
            self.maximized = maximized;
            self.full_screen = full_screen;
            frame.decorated = decorated;
            return frame.poll_layout(&mut self.caption_layout) || changed;
        }
        let Some(placement) = window.placement() else {
            return false;
        };
        let changed =
            self.maximized != placement.maximized || self.full_screen != placement.full_screen;
        self.maximized = placement.maximized;
        self.full_screen = placement.full_screen;
        #[cfg(target_os = "linux")]
        if let Some(frame) = &mut self.resize_frame {
            return frame
                .poll(placement, &mut self.caption_layout)
                .unwrap_or(false)
                || changed;
        }
        changed
    }

    fn has_caption(&self) -> bool {
        if self.window.is_none() || self.full_screen {
            return false;
        }
        #[cfg(target_os = "linux")]
        if let Some(frame) = &self.wayland {
            return !frame.decorated;
        }
        true
    }

    pub(super) fn leading_inset(&self) -> f32 {
        if self.has_caption() {
            CaptionLayout::width(&self.caption_layout.leading)
        } else {
            0.0
        }
    }

    pub(super) fn trailing_inset(&self) -> f32 {
        if self.has_caption() {
            CaptionLayout::width(&self.caption_layout.trailing)
        } else {
            0.0
        }
    }

    pub(super) fn drag_band(&self, left: f32) -> Option<El> {
        self.has_caption().then(|| {
            let left = left.max(self.leading_inset());
            self.drag_region(
                el().key("window-drag-band")
                    .pin(Some(left), Some(0.0), Some(self.trailing_inset()), None)
                    .h(HEIGHT),
            )
        })
    }

    pub(super) fn drag_region(&self, element: El) -> El {
        #[cfg(target_os = "linux")]
        if let Some(frame) = &self.wayland {
            return if frame.decorated {
                element.window_drag()
            } else {
                wayland::drag_region(element)
            };
        }
        if self.window.is_none() {
            return element.window_drag();
        }
        // An X11 WM grabs the first native move immediately, so use the click run of
        // a Rune pointer drag. Header controls are added above this hit area.
        element.child(
            el().key("window-drag-area")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .on_drag(|state: &mut State, drag, tx| state.window_chrome.drag(drag, tx)),
        )
    }

    fn drag(&mut self, drag: &mut Drag, tx: &mut Tx) {
        let Some(window) = &self.window else {
            return;
        };
        match drag.phase {
            Phase::Down if drag.clicks.is_multiple_of(2) => {
                self.drag_origin = None;
                tx.toggle_maximized();
            }
            Phase::Down => {
                self.drag_origin = window
                    .placement()
                    .filter(|placement| !placement.maximized && !placement.full_screen)
                    .zip(drag.screen_point().or_else(|| self.pointer()))
                    .map(|(placement, pointer)| DragOrigin {
                        frame: placement.position,
                        pointer,
                        scale: placement.scale,
                        moving: false,
                    });
            }
            Phase::Move => self.move_window(drag),
            Phase::Up => {
                if !drag.cancelled() {
                    self.move_window(drag);
                }
                self.drag_origin = None;
            }
        }
    }

    fn move_window(&mut self, drag: &Drag) {
        let Some(window) = &self.window else { return };
        let pointer = if self.drag_origin.is_some_and(|origin| origin.moving) {
            self.pointer()
        } else {
            drag.screen_point().or_else(|| self.pointer())
        };
        let Some((x, y)) = pointer else { return };
        let Some(origin) = &mut self.drag_origin else {
            return;
        };
        let dx = x - origin.pointer.0;
        let dy = y - origin.pointer.1;
        if origin.moving {
            window.set_position((origin.frame.0 + dx, origin.frame.1 + dy));
        } else if f64::from(dx.abs().max(dy.abs())) >= 4.0 * origin.scale {
            origin.pointer = (x, y);
            origin.moving = true;
        }
    }

    fn pointer(&self) -> Option<(i32, i32)> {
        #[cfg(target_os = "linux")]
        {
            self.resize_frame.as_ref()?.pointer()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn minimize(&self) {
        #[cfg(target_os = "linux")]
        if let Some(frame) = &self.resize_frame {
            let _ = frame.minimize();
        } else if self.wayland.is_some()
            && let Some(window) = &self.window
        {
            let _ = window.minimize();
        }
    }

    pub(super) fn resize_overlay(&self) -> Option<El> {
        #[cfg(target_os = "linux")]
        if self.wayland.is_some() && self.has_caption() && !self.maximized {
            return Some(wayland::edges());
        }
        None
    }

    pub(super) fn overlay(&self, dark: bool) -> Option<El> {
        let leading = self.leading_inset();
        let trailing = self.trailing_inset();
        if leading == 0.0 && trailing == 0.0 {
            return None;
        }
        let colors = Colors::new(dark);
        let buttons = &self.caption_layout;
        Some(
            el().key("window-caption")
                .pin(Some(0.0), Some(0.0), Some(0.0), None)
                .h(HEIGHT)
                .child(
                    el().key("window-caption-leading")
                        .pin(Some(0.0), Some(0.0), None, None)
                        .size(leading, HEIGHT)
                        .no_window_drag()
                        .children(
                            buttons
                                .leading
                                .iter()
                                .enumerate()
                                .filter(|&(index, kind)| {
                                    !buttons.leading[index + 1..].contains(kind)
                                        && !buttons.trailing.contains(kind)
                                })
                                .map(|(index, &kind)| {
                                    self.caption_button(kind, 10.0, index, colors)
                                }),
                        ),
                )
                .child(
                    el().key("window-caption-trailing")
                        .pin(None, Some(0.0), Some(0.0), None)
                        .size(trailing, HEIGHT)
                        .no_window_drag()
                        .children(
                            buttons
                                .trailing
                                .iter()
                                .enumerate()
                                .filter(|&(index, kind)| !buttons.trailing[..index].contains(kind))
                                .map(|(index, &kind)| {
                                    self.caption_button(kind, 5.0, index, colors)
                                }),
                        ),
                ),
        )
    }

    fn caption_button(&self, kind: CaptionButton, offset: f32, index: usize, colors: Colors) -> El {
        use CaptionButton::*;
        let (label, glyph, action): (_, _, fn(&mut State, &mut Tx)) = match kind {
            Minimize => ("Minimize", &MINIMIZE, |state, _| {
                state.window_chrome.minimize()
            }),
            Maximize if self.maximized => ("Restore", &RESTORE, |_, tx| tx.toggle_maximized()),
            Maximize => ("Maximize", &MAXIMIZE, |_, tx| tx.toggle_maximized()),
            Close => ("Close", &CLOSE, |_, tx| tx.close_window()),
        };
        button(
            offset + index as f32 * BUTTON_PITCH,
            label,
            glyph,
            colors,
            action,
        )
    }
}

#[derive(Clone, Copy)]
struct Colors {
    symbol: Color,
    idle: Color,
    hover: Color,
    pressed: Color,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let symbol = hex(if dark { "#f8fafc" } else { "#1f2937" }).expect("static caption color");
        // These layers add up to 15% opacity on hover and 30% while pressed.
        Self {
            symbol,
            idle: symbol.alpha(0.10),
            hover: symbol.alpha(1.0 / 18.0),
            pressed: symbol.alpha(3.0 / 17.0),
        }
    }
}

fn button(
    x: f32,
    label: &'static str,
    glyph: &'static Icon,
    colors: Colors,
    action: fn(&mut State, &mut Tx),
) -> El {
    el().key(glyph.name)
        .absolute(x, (HEIGHT - BUTTON_SIZE) / 2.0)
        .size(BUTTON_SIZE, BUTTON_SIZE)
        .center()
        .radius(BUTTON_SIZE / 2.0)
        .fill(colors.idle)
        .hover(colors.hover)
        .press(colors.pressed)
        .role("button")
        .label(label)
        .help(label)
        .focusable(false)
        .on_press_tx(action)
        .child(icon(glyph, 8.0).color(colors.symbol).a11y_hidden())
}

pub(super) fn configure(app: App<State>) -> App<State> {
    #[cfg(target_os = "linux")]
    {
        let handle = app.handle();
        app.on_window(move |window| {
            let mut chrome =
                if let Some((frame, caption_layout)) = x11::ResizeFrame::install(window) {
                    ChromeState {
                        resize_frame: Some(frame),
                        caption_layout,
                        ..ChromeState::default()
                    }
                } else if let Some((frame, caption_layout)) = wayland::Frame::install(window) {
                    ChromeState {
                        wayland: Some(frame),
                        caption_layout,
                        ..ChromeState::default()
                    }
                } else {
                    return;
                };
            chrome.window = Some(window.clone());
            handle.update(move |state, tx| {
                state.window_chrome = chrome;
                state.window_chrome.poll(tx);
            });
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        app
    }
}
