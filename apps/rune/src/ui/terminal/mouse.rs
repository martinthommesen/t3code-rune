use alacritty_terminal::{grid::Dimensions, term::TermMode};
use rune::{Drag, Mods, Phase, PointerButton, PointerEvent, Tx};

use super::{CELL_PADDING, CELL_WIDTH, LINE_HEIGHT, Pane, State};

type Report = (String, bool);

#[derive(Default)]
pub(super) struct Mouse {
    pressed: Option<u16>,
    motion: Option<Report>,
    any_motion: bool,
}

impl Mouse {
    pub(super) fn sync_mode(&mut self, modes: TermMode) {
        let any_motion = modes.contains(TermMode::MOUSE_MOTION);
        if std::mem::replace(&mut self.any_motion, any_motion) != any_motion {
            self.reset_motion();
        }
    }

    pub(super) fn reset_motion(&mut self) {
        self.motion = None;
    }
}

pub(super) fn capture(modes: TermMode, mods: Mods) -> bool {
    modes.intersects(TermMode::MOUSE_MODE) && !mods.shift && !mods.ctrl && !mods.logo && !mods.cmd
}

impl State {
    pub(super) fn terminal_pointer_drag(&mut self, attachment: u64, drag: &Drag, tx: &mut Tx) {
        let button = match tx.button() {
            PointerButton::Primary => 0,
            PointerButton::Middle => 1,
            _ => return,
        };
        let primary_paste = self.terminal_primary_paste;
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        let mods = tx.mods();
        let modes = *pane.screen.term.mode();
        let tracking = match drag.phase {
            Phase::Down => {
                pane.mouse.pressed = capture(modes, mods).then_some(button);
                pane.mouse.reset_motion();
                pane.mouse.pressed
            }
            Phase::Move => pane.mouse.pressed,
            Phase::Up => pane.mouse.pressed.take(),
        };
        let Some(button) = tracking else {
            if tx.button() == PointerButton::Primary {
                self.drag_terminal_selection(attachment, drag, tx);
            } else if drag.phase == Phase::Down {
                let selected = primary_paste
                    .then(|| super::selection::selected_text(pane))
                    .flatten();
                self.activate_terminal_attachment(attachment, tx);
                if let Some(text) = selected {
                    self.paste_terminal_at(attachment, &text);
                }
            }
            return;
        };
        let position = cell_at(pane, drag.x, drag.y);
        let report = match drag.phase {
            Phase::Down => encode(modes, button, false, position, mods),
            Phase::Move if modes.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) => {
                motion(pane, modes, 32 + button, position, mods)
            }
            Phase::Move => None,
            Phase::Up => {
                pane.mouse.reset_motion();
                encode(modes, button, true, position, mods)
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
        if pane.mouse.pressed.is_some() {
            return;
        }
        if !modes.contains(TermMode::MOUSE_MOTION) || !capture(modes, mods) {
            pane.mouse.reset_motion();
            return;
        }
        let position = cell_at(pane, event.x, event.y);
        if let Some((data, binary)) = motion(pane, modes, 35, position, mods) {
            self.write_terminal_input_at(attachment, data, binary);
        }
    }
}

#[cfg(target_os = "linux")]
pub(in crate::ui) fn primary_paste_enabled() -> bool {
    use std::{process::Stdio, time::Duration};
    use tokio::{process::Command, runtime::Builder, time::timeout};

    async fn read(program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|value| value.split(':').any(|name| name == "GNOME"));
    Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .and_then(|runtime| {
            runtime.block_on(async {
                timeout(Duration::from_secs(2), async {
                    if !gnome
                        && let Some(value) = read(
                            "dconf",
                            &[
                                "read",
                                "/org/gnome/desktop/interface/gtk-enable-primary-paste",
                            ],
                        )
                        .await
                    {
                        return Some(value);
                    }
                    read(
                        "gsettings",
                        &[
                            "get",
                            "org.gnome.desktop.interface",
                            "gtk-enable-primary-paste",
                        ],
                    )
                    .await
                })
                .await
                .ok()
                .flatten()
            })
        })
        .is_none_or(|output| output.trim() != "false")
}

#[cfg(not(target_os = "linux"))]
pub(in crate::ui) fn primary_paste_enabled() -> bool {
    false
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
