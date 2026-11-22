use alacritty_terminal::{
    grid::{Dimensions, Scroll},
    term::TermMode,
};
use rune::{Color, Drag, El, Key, KeyEvent, Phase, WheelEvent, el};

use super::{CELL_PADDING, LINE_HEIGHT, Palette, Pane, State, mouse, terminal_rgb};

const TRACK_WIDTH: f32 = 6.0;
const MIN_THUMB_HEIGHT: f32 = 18.0;

pub(super) fn input_key(event: &KeyEvent) -> Option<Scroll> {
    let mods = event.mods;
    if !mods.shift || mods.alt {
        return None;
    }
    if !mods.cmd && !mods.ctrl && !mods.logo {
        return match event.key {
            Key::PageUp => Some(Scroll::PageUp),
            Key::PageDown => Some(Scroll::PageDown),
            _ => None,
        };
    }
    let primary_only = mods.cmd
        && if cfg!(target_os = "macos") {
            !mods.ctrl
        } else {
            !mods.logo
        };
    if primary_only {
        match event.key {
            Key::Home => Some(Scroll::Top),
            Key::End => Some(Scroll::Bottom),
            _ => None,
        }
    } else {
        None
    }
}

impl State {
    pub(super) fn terminal_wheel(&mut self, attachment: u64, event: WheelEvent) -> bool {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return false;
        };
        if !event.dy.is_finite() || event.dy == 0.0 {
            return false;
        }
        let total = pane.wheel_remainder + event.dy / LINE_HEIGHT;
        let rows = total.trunc() as i32;
        pane.wheel_remainder = total.fract();
        if rows == 0 {
            return true;
        }
        let term = &mut pane.screen.term;
        let modes = *term.mode();
        if modes.intersects(TermMode::MOUSE_MODE)
            && !event.mods.shift
            && !event.mods.ctrl
            && !event.mods.logo
            && !event.mods.cmd
        {
            let button = if rows > 0 { 64 } else { 65 };
            let position = mouse::cell_at(pane, event.x, event.y);
            if let Some((data, binary)) = mouse::encode(modes, button, false, position, event.mods)
            {
                self.write_terminal_input_at(attachment, data.repeat(wheel_steps(rows)), binary);
            }
        } else if modes.contains(TermMode::ALT_SCREEN) {
            let arrow = match (rows > 0, modes.contains(TermMode::APP_CURSOR)) {
                (true, true) => "\x1bOA",
                (false, true) => "\x1bOB",
                (true, false) => "\x1b[A",
                (false, false) => "\x1b[B",
            };
            self.write_terminal_input_at(attachment, arrow.repeat(wheel_steps(rows)), false);
        } else {
            term.scroll_display(Scroll::Delta(rows));
        }
        true
    }

    pub(super) fn scroll_history(&mut self, attachment: u64, action: Scroll) -> bool {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return false;
        };
        let term = &mut pane.screen.term;
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        term.scroll_display(action);
        true
    }

    fn drag_terminal_scrollbar(&mut self, attachment: u64, drag: &mut Drag) {
        if drag.cancelled() {
            return;
        }
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        let Some(geometry) = scroll_geometry(pane, drag.h) else {
            return;
        };
        if drag.phase == Phase::Down {
            let origin = drag.y - geometry.top;
            drag.data = if (0.0..=geometry.height).contains(&origin) {
                origin
            } else {
                geometry.height / 2.0
            };
        }
        let ratio = ((drag.y - drag.data) / geometry.travel).clamp(0.0, 1.0);
        let target = (ratio * geometry.history as f32).round() as i32;
        pane.screen
            .term
            .scroll_display(Scroll::Delta(geometry.from_top as i32 - target));
    }
}

fn wheel_steps(rows: i32) -> usize {
    rows.unsigned_abs().min(128) as usize
}

struct Geometry {
    history: usize,
    from_top: usize,
    height: f32,
    top: f32,
    travel: f32,
}

fn scroll_geometry(pane: &Pane, track_height: f32) -> Option<Geometry> {
    let grid = pane.screen.term.grid();
    let history = grid.history_size();
    if history == 0 || track_height <= MIN_THUMB_HEIGHT {
        return None;
    }
    let from_top = history.saturating_sub(grid.display_offset());
    let height = (track_height * grid.screen_lines() as f32 / grid.total_lines() as f32)
        .clamp(MIN_THUMB_HEIGHT, track_height);
    let travel = track_height - height;
    Some(Geometry {
        history,
        from_top,
        height,
        travel,
        top: travel * from_top as f32 / history as f32,
    })
}

pub(super) fn scrollbar(pane: &Pane, palette: Palette) -> Option<El> {
    let attachment = pane.attachment;
    let geometry = scroll_geometry(pane, pane.viewport_height - CELL_PADDING * 2.0)?;
    let (thumb, hover) = if palette.dark {
        (Color::WHITE.alpha(0.36), Color::WHITE.alpha(0.45))
    } else {
        (terminal_rgb(217, 217, 217), terminal_rgb(191, 191, 191))
    };
    Some(
        el().key(format!("terminal-scrollback-{attachment}"))
            .role("scrollbar")
            .label("Terminal scrollback")
            .range_value(geometry.from_top as f64, 0.0, geometry.history as f64)
            .w(TRACK_WIDTH)
            .pin(None, Some(CELL_PADDING), Some(1.0), Some(CELL_PADDING))
            .focusable(true)
            .focus_ring(1.0, 0.0, palette.accent)
            .on_drag(move |state: &mut State, drag, _| {
                state.drag_terminal_scrollbar(attachment, drag)
            })
            .on_key(move |state: &mut State, event, _| {
                let action = match event.key {
                    Key::Up => Scroll::Delta(1),
                    Key::Down => Scroll::Delta(-1),
                    Key::PageUp => Scroll::PageUp,
                    Key::PageDown => Scroll::PageDown,
                    Key::Home => Scroll::Top,
                    Key::End => Scroll::Bottom,
                    _ => return false,
                };
                state.scroll_history(attachment, action)
            })
            .child(
                el().key(format!("terminal-scrollback-thumb-{attachment}"))
                    .pin(Some(1.0), Some(geometry.top), Some(1.0), None)
                    .h(geometry.height)
                    .radius(3.0)
                    .fill(thumb)
                    .hover(hover)
                    .a11y_hidden(),
            ),
    )
}
