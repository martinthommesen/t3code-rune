use std::time::Duration;

use rune::color::hex;
use rune::{App, Color, Drag, El, Icon, Phase, Tx, WindowHandle, el, icon};

use super::State;

#[cfg(target_os = "linux")]
mod x11;

const HEIGHT: f32 = 40.0;
const TRAILING: f32 = 113.0;
const BUTTON_SIZE: f32 = 24.0;
const BUTTON_START: f32 = 5.0;
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

#[derive(Default)]
pub(super) struct ChromeState {
    window: Option<WindowHandle>,
    maximized: bool,
    full_screen: bool,
    drag_origin: Option<DragOrigin>,
    #[cfg(target_os = "linux")]
    resize_frame: Option<x11::ResizeFrame>,
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
        let Some(placement) = window.placement() else {
            return false;
        };
        #[cfg(target_os = "linux")]
        if let Some(frame) = &mut self.resize_frame {
            let _ = frame.poll(placement);
        }
        let changed =
            self.maximized != placement.maximized || self.full_screen != placement.full_screen;
        self.maximized = placement.maximized;
        self.full_screen = placement.full_screen;
        changed
    }

    pub(super) fn trailing_inset(&self) -> f32 {
        if self.window.is_some() && !self.full_screen {
            TRAILING
        } else {
            0.0
        }
    }

    pub(super) fn drag_band(&self, left: f32) -> Option<El> {
        let trailing = self.trailing_inset();
        (trailing > 0.0).then(|| {
            self.drag_region(
                el().key("window-drag-band")
                    .pin(Some(left), Some(0.0), Some(trailing), None)
                    .h(HEIGHT),
            )
        })
    }

    pub(super) fn drag_region(&self, element: El) -> El {
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
        }
    }

    pub(super) fn overlay(&self, dark: bool) -> Option<El> {
        if self.trailing_inset() == 0.0 {
            return None;
        }
        let colors = Colors::new(dark);
        let (maximize_label, maximize_icon) = if self.maximized {
            ("Restore", &RESTORE)
        } else {
            ("Maximize", &MAXIMIZE)
        };
        Some(
            el().key("window-caption")
                .pin(None, Some(0.0), Some(0.0), None)
                .size(TRAILING, HEIGHT)
                .no_window_drag()
                .child(button(0.0, "Minimize", &MINIMIZE, colors, |state, _| {
                    state.window_chrome.minimize();
                }))
                .child(button(
                    1.0,
                    maximize_label,
                    maximize_icon,
                    colors,
                    |_, tx| tx.toggle_maximized(),
                ))
                .child(button(2.0, "Close", &CLOSE, colors, |_, tx| {
                    tx.close_window()
                })),
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
    index: f32,
    label: &'static str,
    glyph: &'static Icon,
    colors: Colors,
    action: fn(&mut State, &mut Tx),
) -> El {
    el().key(glyph.name)
        .absolute(
            BUTTON_START + index * BUTTON_PITCH,
            (HEIGHT - BUTTON_SIZE) / 2.0,
        )
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
            let Some(resize_frame) = x11::ResizeFrame::install(window) else {
                return;
            };
            let window = window.clone();
            handle.update(move |state, tx| {
                state.window_chrome = ChromeState {
                    window: Some(window),
                    resize_frame: Some(resize_frame),
                    ..ChromeState::default()
                };
                state.window_chrome.poll(tx);
            });
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        app
    }
}
