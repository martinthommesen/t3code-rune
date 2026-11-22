use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
};
use rune::{
    Color, Drag, El, KeyEvent, Phase, Tx,
    color::{srgb_decode, srgb_encode},
    el,
};

use super::{CELL_PADDING, CELL_WIDTH, LINE_HEIGHT, Palette, Pane, State, terminal_rgb};

impl State {
    pub(super) fn drag_terminal_selection(&mut self, attachment: u64, drag: &Drag, tx: &mut Tx) {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        let term = &mut pane.screen.term;
        let col = ((drag.x - CELL_PADDING) / CELL_WIDTH).max(0.0);
        let row = ((drag.y - CELL_PADDING) / LINE_HEIGHT).max(0.0);
        let col = col.min(term.columns() as f32);
        let point = Point::new(
            Line(
                (row as usize).min(term.screen_lines() - 1) as i32
                    - term.grid().display_offset() as i32,
            ),
            Column((col as usize).min(term.columns() - 1)),
        );
        let side = if col.fract() < 0.5 && col < term.columns() as f32 {
            Side::Left
        } else {
            Side::Right
        };
        match drag.phase {
            Phase::Down if !tx.mods().shift || term.selection.is_none() => {
                let kind = match drag.clicks {
                    2 => SelectionType::Semantic,
                    3 => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                term.selection = Some(Selection::new(kind, point, side));
            }
            Phase::Up if drag.cancelled() => term.selection = None,
            _ => {
                if let Some(selection) = &mut term.selection {
                    selection.update(point, side);
                }
            }
        }
        if drag.phase == Phase::Up && !drag.cancelled() {
            self.activate_terminal_attachment(attachment, tx);
        }
    }
}

pub(super) fn clipboard_key(pane: &mut Pane, event: &KeyEvent, tx: &mut Tx) -> bool {
    let mac = cfg!(target_os = "macos");
    let insert = !mac && matches!(event.key, rune::Key::Insert) && !event.mods.logo;
    if (event.is_command('v') && (event.mods.shift || mac))
        || (insert && event.mods.shift && !event.mods.ctrl)
    {
        tx.request_paste();
        return true;
    }
    let alternate_primary = if mac {
        event.mods.ctrl
    } else {
        event.mods.logo
    };
    if event.is_command('a') && !alternate_primary && !event.mods.alt && event.mods.shift != mac {
        select_all(pane);
        return true;
    }
    if !event.is_command('c') && !(insert && event.mods.ctrl && !event.mods.shift) {
        return false;
    }
    let Some(text) = selected_text(pane) else {
        return false;
    };
    tx.copy(text);
    if !mac && !event.mods.shift && !insert {
        pane.screen.term.selection = None;
    }
    true
}

pub(super) fn selected_text(pane: &Pane) -> Option<String> {
    pane.screen
        .term
        .selection_to_string()
        .filter(|text| !text.is_empty())
}

pub(super) fn select_all(pane: &mut Pane) {
    let term = &mut pane.screen.term;
    let grid = term.grid();
    let end = (term.topmost_line().0..=term.bottommost_line().0)
        .rev()
        .map(Line)
        .find_map(|line| {
            (0..term.columns())
                .rev()
                .map(Column)
                .find(|&column| {
                    let cell = &grid[line][column];
                    cell.c != ' ' || cell.zerowidth().is_some()
                })
                .map(|column| Point::new(line, column))
        });
    term.selection = end.map(|end| {
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(term.topmost_line(), Column(0)),
            Side::Left,
        );
        selection.update(end, Side::Right);
        selection
    });
}

pub(super) fn highlights(pane: &Pane, palette: Palette) -> Vec<El> {
    let term = &pane.screen.term;
    let screen = term.renderable_content();
    let Some(selection) = screen.selection else {
        return Vec::new();
    };
    let fill = if palette.dark {
        terminal_rgb(45, 51, 64)
    } else {
        terminal_rgb(208, 214, 221)
    };
    (0..term.screen_lines())
        .filter_map(|row| {
            let line = Line(row as i32 - screen.display_offset as i32);
            if line < selection.start.line || line > selection.end.line {
                return None;
            }
            let first = if line == selection.start.line {
                selection.start.column.0
            } else {
                0
            };
            let after_last = if line == selection.end.line {
                selection.end.column.0 + 1
            } else {
                term.columns()
            };
            Some(
                el().absolute(first as f32 * CELL_WIDTH, 1.0 + row as f32 * LINE_HEIGHT)
                    .size((after_last - first) as f32 * CELL_WIDTH, LINE_HEIGHT)
                    .fill(fill),
            )
        })
        .collect()
}

pub(super) fn background(color: Color, dark: bool) -> Color {
    let (overlay, alpha) = if dark {
        ([180.0, 203.0, 255.0], 0.25)
    } else {
        ([37.0, 63.0, 99.0], 0.2)
    };
    let blend = |channel: f32, overlay: f32| {
        srgb_decode(srgb_encode(channel) * (1.0 - alpha) + overlay / 255.0 * alpha)
    };
    Color {
        r: blend(color.r, overlay[0]),
        g: blend(color.g, overlay[1]),
        b: blend(color.b, overlay[2]),
        a: color.a,
    }
}
