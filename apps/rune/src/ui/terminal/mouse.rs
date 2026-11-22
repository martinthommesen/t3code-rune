use alacritty_terminal::{grid::Dimensions, term::TermMode};
use rune::{Drag, Mods, Phase, PointerEvent, Tx};

use super::{CELL_PADDING, CELL_WIDTH, LINE_HEIGHT, Pane, State};

type Report = (String, bool);

#[derive(Default)]
pub(super) struct Mouse {
    dragging: bool,
    motion: Option<Report>,
}

fn capture(modes: TermMode, mods: Mods) -> bool {
    modes.intersects(TermMode::MOUSE_MODE) && !mods.shift && !mods.ctrl && !mods.logo && !mods.cmd
}

impl State {
    pub(super) fn terminal_pointer_drag(&mut self, attachment: u64, drag: &Drag, tx: &mut Tx) {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        let mods = tx.mods();
        let modes = *pane.screen.term.mode();
        let tracking = match drag.phase {
            Phase::Down => {
                pane.mouse.dragging = capture(modes, mods);
                pane.mouse.motion = None;
                pane.mouse.dragging
            }
            Phase::Move => pane.mouse.dragging,
            Phase::Up => std::mem::take(&mut pane.mouse.dragging),
        };
        if !tracking {
            self.drag_terminal_selection(attachment, drag, tx);
            return;
        }
        let position = cell_at(pane, drag.x, drag.y);
        let report = match drag.phase {
            Phase::Down => encode(modes, 0, false, position, mods),
            Phase::Move if modes.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) => {
                motion(pane, modes, 32, position, mods)
            }
            Phase::Move => None,
            Phase::Up => {
                pane.mouse.motion = None;
                encode(modes, 0, true, position, mods)
            }
        };
        if let Some((data, binary)) = report {
            self.write_terminal_input_at(attachment, data, binary);
        }
        if drag.phase == Phase::Up && !drag.cancelled() {
            self.activate_terminal_attachment(attachment, tx);
        }
    }

    pub(super) fn terminal_pointer_move(
        &mut self,
        attachment: u64,
        event: PointerEvent,
        tx: &mut Tx,
    ) {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        let modes = *pane.screen.term.mode();
        let mods = tx.mods();
        if pane.mouse.dragging {
            return;
        }
        if !modes.contains(TermMode::MOUSE_MOTION) || !capture(modes, mods) {
            pane.mouse.motion = None;
            return;
        }
        let position = cell_at(pane, event.x, event.y);
        if let Some((data, binary)) = motion(pane, modes, 35, position, mods) {
            self.write_terminal_input_at(attachment, data, binary);
        }
    }
}

fn motion(
    pane: &mut Pane,
    modes: TermMode,
    button: u16,
    position: [u16; 2],
    mods: Mods,
) -> Option<Report> {
    let report = encode(modes, button, false, position, mods)?;
    if pane.mouse.motion.as_ref() == Some(&report) {
        return None;
    }
    pane.mouse.motion = Some(report.clone());
    Some(report)
}

pub(super) fn cell_at(pane: &Pane, x: f32, y: f32) -> [u16; 2] {
    let term = &pane.screen.term;
    let col = ((x - CELL_PADDING) / CELL_WIDTH).clamp(0.0, (term.columns() - 1) as f32) as u16;
    let row =
        ((y - CELL_PADDING) / LINE_HEIGHT).clamp(0.0, (term.screen_lines() - 1) as f32) as u16;
    [col + 1, row + 1]
}

pub(super) fn encode(
    modes: TermMode,
    mut button: u16,
    release: bool,
    [col, row]: [u16; 2],
    mods: Mods,
) -> Option<Report> {
    if !modes.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let sgr = modes.contains(TermMode::SGR_MOUSE);
    if !sgr && release {
        button = 3;
    }
    button |=
        (u16::from(mods.shift) << 2) | (u16::from(mods.alt) << 3) | (u16::from(mods.ctrl) << 4);
    if sgr {
        let suffix = if release { 'm' } else { 'M' };
        return Some((format!("\x1b[<{button};{col};{row}{suffix}"), false));
    }
    let utf8 = modes.contains(TermMode::UTF8_MOUSE);
    if !utf8 && (col > 223 || row > 223) {
        return None;
    }
    let mut data = "\x1b[M".to_owned();
    data.extend(
        [button, col, row].map(|coordinate| char::from_u32(u32::from(32 + coordinate)).unwrap()),
    );
    Some((data, !utf8))
}
