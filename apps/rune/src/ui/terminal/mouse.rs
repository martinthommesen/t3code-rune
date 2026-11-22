use alacritty_terminal::grid::Dimensions;
use rune::{Drag, Mods, Phase, PointerButton, PointerEvent, Tx};

use super::{
    CELL_PADDING, CELL_WIDTH, LINE_HEIGHT, Pane, State,
    mouse_modes::{Encoding, Protocol, Tracking},
};

type Report = (String, bool);

#[derive(Default)]
pub(super) struct Mouse {
    pressed: Option<u16>,
    motion: Option<Report>,
    any_motion: bool,
}

impl Mouse {
    pub(super) fn sync_mode(&mut self, tracking: Tracking) {
        let any_motion = tracking == Tracking::Motion;
        if std::mem::replace(&mut self.any_motion, any_motion) != any_motion {
            self.reset_motion();
        }
    }

    pub(super) fn reset_motion(&mut self) {
        self.motion = None;
    }
}

pub(super) fn capture(modes: Protocol, mods: Mods) -> bool {
    modes.tracking != Tracking::Disabled && !mods.shift && !mods.ctrl && !mods.logo && !mods.cmd
}

impl State {
    pub(super) fn terminal_mouse_captured(&mut self, attachment: u64, mods: Mods) -> bool {
        self.terminal_attachment_mut(attachment)
            .is_some_and(|pane| capture(pane.screen.mouse_modes.protocol(), mods))
    }

    pub(super) fn terminal_pointer_drag(&mut self, attachment: u64, drag: &Drag, tx: &mut Tx) {
        let button = match tx.button() {
            PointerButton::Primary => 0,
            PointerButton::Middle => 1,
            PointerButton::Secondary => 2,
            PointerButton::Back => 64,
            PointerButton::Forward => 65,
            _ => return,
        };
        let primary_paste = self.terminal_primary_paste;
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        let mods = tx.mods();
        let modes = pane.screen.mouse_modes.protocol();
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
                self.terminal_link_drag(attachment, drag, tx);
            } else if tx.button() == PointerButton::Middle && drag.phase == Phase::Down {
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
        let position = position_at(pane, drag.x, drag.y);
        let report = match drag.phase {
            Phase::Down => encode(modes, button, false, position, mods),
            Phase::Move if matches!(modes.tracking, Tracking::Drag | Tracking::Motion) => {
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
        let modes = pane.screen.mouse_modes.protocol();
        let mods = tx.mods();
        if pane.mouse.pressed.is_some() {
            return;
        }
        if modes.tracking != Tracking::Motion || !capture(modes, mods) {
            pane.mouse.reset_motion();
            return;
        }
        let position = position_at(pane, event.x, event.y);
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
    modes: Protocol,
    button: u16,
    position: Position,
    mods: Mods,
) -> Option<Report> {
    let report = encode(modes, button, false, position, mods)?;
    if pane.mouse.motion.as_ref() == Some(&report) {
        return None;
    }
    pane.mouse.motion = Some(report.clone());
    Some(report)
}

pub(super) struct Position {
    cell: [u16; 2],
    pixels: [i32; 2],
}

pub(super) fn position_at(pane: &Pane, x: f32, y: f32) -> Position {
    let term = &pane.screen.term;
    let x = x.max(0.0) - CELL_PADDING;
    let y = y.max(0.0) - CELL_PADDING;
    let col = (x / CELL_WIDTH).clamp(0.0, (term.columns() - 1) as f32) as u16;
    let row = (y / LINE_HEIGHT).clamp(0.0, (term.screen_lines() - 1) as f32) as u16;
    Position {
        cell: [col + 1, row + 1],
        pixels: [x.round() as i32, y.round() as i32],
    }
}

pub(super) fn encode(
    modes: Protocol,
    mut button: u16,
    release: bool,
    position: Position,
    mods: Mods,
) -> Option<Report> {
    let x10 = modes.tracking == Tracking::X10;
    if modes.tracking == Tracking::Disabled || x10 && (release || button > 2) {
        return None;
    }
    let sgr = matches!(modes.encoding, Encoding::Sgr | Encoding::Pixels);
    if !sgr && release {
        button = 3;
    }
    if !x10 {
        button |=
            (u16::from(mods.shift) << 2) | (u16::from(mods.alt) << 3) | (u16::from(mods.ctrl) << 4);
    }
    let [col, row] = position.cell;
    if sgr {
        let [x, y] = if modes.encoding == Encoding::Pixels {
            position.pixels
        } else {
            [i32::from(col), i32::from(row)]
        };
        let suffix = if release { 'm' } else { 'M' };
        return Some((format!("\x1b[<{button};{x};{y}{suffix}"), false));
    }
    if modes.encoding == Encoding::Urxvt {
        return Some((format!("\x1b[{};{col};{row}M", 32 + button), false));
    }
    let utf8 = modes.encoding == Encoding::Utf8;
    if !utf8 && (col > 223 || row > 223) {
        return None;
    }
    let mut data = "\x1b[M".to_owned();
    data.extend(
        [button, col, row].map(|coordinate| char::from_u32(u32::from(32 + coordinate)).unwrap()),
    );
    Some((data, !utf8))
}
