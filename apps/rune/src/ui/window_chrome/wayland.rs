use rune::raw_window_handle::RawWindowHandle;
use rune::{Cursor, El, Phase, WindowHandle, WindowResizeDirection, el};

use super::{CaptionLayout, State};

mod settings;

pub(super) struct Frame {
    pub(super) decorated: bool,
    settings: Option<settings::Watch>,
}

impl Frame {
    pub(super) fn install(window: &WindowHandle) -> Option<(Self, CaptionLayout)> {
        if !window
            .with_raw_handle(|handle| matches!(handle.as_raw(), RawWindowHandle::Wayland(_)))
            .ok()?
        {
            return None;
        }
        let decorated = window.is_decorated().unwrap_or(true);
        let (settings, layout) = settings::open();
        Some((
            Self {
                decorated,
                settings,
            },
            layout,
        ))
    }

    pub(super) fn poll_layout(&self, layout: &mut CaptionLayout) -> bool {
        if let Some(next) = self.settings.as_ref().and_then(settings::Watch::latest)
            && next != *layout
        {
            *layout = next;
            return true;
        }
        false
    }
}

pub(super) fn drag_region(element: El) -> El {
    element.child(
        el().key("window-drag-area")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .on_drag(|state: &mut State, drag, tx| {
                if state.window_chrome.full_screen {
                    return;
                }
                if drag.phase == Phase::Down && drag.clicks.is_multiple_of(2) {
                    drag.data = 1.0;
                    tx.toggle_maximized();
                } else if drag.phase == Phase::Move
                    && drag.data == 0.0
                    && drag.dx.abs().max(drag.dy.abs()) >= 4.0
                    && let Some(window) = &state.window_chrome.window
                {
                    // Preserve the button-up between stationary clicks. The compositor grabs
                    // the pointer after the move request, so send it only after actual travel.
                    drag.data = 1.0;
                    let _ = window.drag_window();
                }
            }),
    )
}

pub(super) fn edges() -> El {
    use WindowResizeDirection::*;
    const EDGE: f32 = 6.0;
    let edge = Some(EDGE);
    let at = Some(0.0);
    let grip = |key, area: El, direction, cursor| {
        area.key(key)
            .cursor(cursor)
            .on_drag(move |state: &mut State, drag, _| {
                if drag.phase == Phase::Down
                    && let Some(window) = &state.window_chrome.window
                {
                    let _ = window.drag_resize(direction);
                }
            })
    };
    let horizontal = Cursor::ResizeColumn;
    let vertical = Cursor::ResizeRow;
    el().key("window-resize-edges")
        .pin(at, at, at, at)
        .child(grip(
            "resize-top",
            el().pin(edge, at, edge, None).h(EDGE),
            North,
            vertical,
        ))
        .child(grip(
            "resize-bottom",
            el().pin(edge, None, edge, at).h(EDGE),
            South,
            vertical,
        ))
        .child(grip(
            "resize-left",
            el().pin(at, edge, None, edge).w(EDGE),
            West,
            horizontal,
        ))
        .child(grip(
            "resize-right",
            el().pin(None, edge, at, edge).w(EDGE),
            East,
            horizontal,
        ))
        .child(grip(
            "resize-top-left",
            el().pin(at, at, None, None).size(EDGE, EDGE),
            NorthWest,
            horizontal,
        ))
        .child(grip(
            "resize-top-right",
            el().pin(None, at, at, None).size(EDGE, EDGE),
            NorthEast,
            horizontal,
        ))
        .child(grip(
            "resize-bottom-left",
            el().pin(at, None, None, at).size(EDGE, EDGE),
            SouthWest,
            horizontal,
        ))
        .child(grip(
            "resize-bottom-right",
            el().pin(None, None, at, at).size(EDGE, EDGE),
            SouthEast,
            horizontal,
        ))
}
