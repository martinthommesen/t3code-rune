use rune::{
    Align, Color, El, Icon, Key, KeyEvent, ScrollSpec, Side, TextStyle, Tx, col, el, icon, row,
    text,
};

use crate::snooze::Preset;

use super::super::{Palette, State, surface};
use super::{Row, edit, writable};

const PICKER: &str = "snooze-preset-time-picker";
const CLOCK: Icon = Icon {
    name: "snooze-time-clock",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></g>"#,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Hour,
    Minute,
    Period,
}

impl Part {
    const ALL: [Self; 3] = [Self::Hour, Self::Minute, Self::Period];

    fn index(self) -> usize {
        match self {
            Self::Hour => 0,
            Self::Minute => 1,
            Self::Period => 2,
        }
    }

    fn length(self) -> u8 {
        match self {
            Self::Hour => 12,
            Self::Minute => 60,
            Self::Period => 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Hour => "Hour",
            Self::Minute => "Minute",
            Self::Period => "AM or PM",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Picker {
    row: u64,
    start: [u8; 3],
    focused: Part,
}

fn parts(row: &Row) -> Option<[u8; 3]> {
    let Preset::At { time, .. } = &row.preset else {
        return None;
    };
    let (hour, minute) = time.split_once(':')?;
    let hour = hour.parse::<u8>().ok()?;
    let minute = minute.parse::<u8>().ok()?;
    Some([(hour % 12 + 11) % 12, minute, hour / 12])
}

pub(super) fn trigger(value: &Row, palette: Palette, enabled: bool) -> El {
    let id = value.id;
    let trigger = el()
        .key(value.key("clock"))
        .size(22.0, 26.0)
        .center()
        .a11y_hidden()
        .child(icon(&CLOCK, 14.0).color(palette.ink).a11y_hidden());
    if enabled {
        trigger.on_background_press_tx(move |state, tx| open(state, id, tx))
    } else {
        trigger
    }
}

pub(super) fn open(state: &mut State, id: u64, tx: &mut Tx) {
    if !writable(state) {
        return;
    }
    edit(state, id, |row| row.dirty && row.commit());
    let Some(start) = state
        .settings
        .workspace
        .snooze
        .rows
        .iter()
        .find(|row| row.id == id)
        .and_then(parts)
    else {
        return;
    };
    state.settings.workspace.close_popups();
    state.settings.workspace.snooze.clock = Some(Picker {
        row: id,
        start,
        focused: Part::Hour,
    });
    tx.focus_when_shown(PICKER, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    let panel = &mut state.settings.workspace.snooze;
    if let Some(picker) = panel.clock.take()
        && let Some(row) = panel.row_mut(picker.row)
    {
        tx.return_focus(row.key("value"), tx.focus_visible());
    }
}

fn chosen(state: &mut State, part: Part, option: u8, tx: &mut Tx) {
    let Some(picker) = &mut state.settings.workspace.snooze.clock else {
        return;
    };
    picker.focused = part;
    let id = picker.row;
    edit(state, id, |row| {
        let Some(mut clock) = parts(row) else {
            return false;
        };
        let Preset::At { day, .. } = row.preset else {
            return false;
        };
        clock[part.index()] = option;
        let hour = (clock[0] + 1) % 12 + clock[2] * 12;
        let minute = clock[1];
        row.set_preset(Preset::At {
            day,
            time: format!("{hour:02}:{minute:02}"),
        })
    });
    tx.focus(PICKER, tx.focus_visible());
    tx.scroll_into_view(option_key(part, option));
}

fn option_key(part: Part, option: u8) -> String {
    format!("snooze-preset-clock-{}-{option}", part.index())
}

fn enforce_input(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let panel = &mut state.settings.workspace.snooze;
    let Some(picker) = &mut panel.clock else {
        return false;
    };
    let part = picker.focused;
    match event.key {
        Key::Escape | Key::Enter => {
            if !tx.key_repeat() {
                close(state, tx);
            }
        }
        Key::Left | Key::Right => {
            let index = if event.key == Key::Left {
                part.index().saturating_sub(1)
            } else {
                (part.index() + 1).min(2)
            };
            picker.focused = Part::ALL[index];
        }
        Key::Up | Key::Down => {
            let Some(current) = panel
                .rows
                .iter()
                .find(|row| row.id == picker.row)
                .and_then(parts)
            else {
                return false;
            };
            let length = part.length();
            let offset = if event.key == Key::Down {
                1
            } else {
                length - 1
            };
            let option = (current[part.index()] + offset) % length;
            chosen(state, part, option, tx);
        }
        Key::Tab => {
            let target = panel
                .rows
                .iter()
                .find(|row| row.id == picker.row)
                .map(|row| row.key(if event.mods.shift { "when" } else { "remove" }));
            close(state, tx);
            if let Some(target) = target {
                tx.focus(target, true);
            }
        }
        _ => return false,
    }
    true
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let panel = &state.settings.workspace.snooze;
    let picker = panel.clock?;
    let value = panel.rows.iter().find(|row| row.id == picker.row)?;
    let selected = parts(value)?;
    let background = surface(palette, "#ffffff", "#171717");
    let accent = rune::color::hex("#0075ff").expect("native time selection color");
    let outside = el()
        .key("snooze-time-picker-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    let content = row()
        .key(PICKER)
        .role("dialog")
        .label("Time picker")
        .focusable(true)
        .no_focus_ring()
        .anchored_fit(value.key("time-field"), Side::Bottom, Align::Start, 0.0)
        .anchored_round()
        .size(172.0, 260.0)
        .padding(3.0)
        .gap(4.0)
        .border(1.0, surface(palette, "#b7b7b7", "#565656"))
        .border_layout()
        .fill(background)
        .on_key(enforce_input)
        .children(Part::ALL.map(|part| {
            let choices = part.length();
            let index = part.index();
            col()
                .role("list")
                .label(part.label())
                .w(52.0)
                .h_full()
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .children((0..choices).map(|offset| {
                    let option = (picker.start[index] + offset) % choices;
                    let current = option == selected[index];
                    let focused = current && picker.focused == part;
                    let label = match part {
                        Part::Hour => format!("{:02}", option + 1),
                        Part::Minute => format!("{option:02}"),
                        Part::Period => {
                            if option == 0 {
                                "AM".into()
                            } else {
                                "PM".into()
                            }
                        }
                    };
                    let fill = if current { accent } else { background };
                    let color = if current { Color::WHITE } else { palette.ink };
                    let mut item = el()
                        .key(option_key(part, option))
                        .role("option")
                        .label(&label)
                        .focus_selected(current)
                        .size(52.0, 36.0)
                        .center()
                        .radius(2.0)
                        .fill(fill)
                        .child(
                            text(label)
                                .font(
                                    TextStyle::new(14.0, if current { 600.0 } else { 400.0 })
                                        .lh(20.0),
                                )
                                .color(color),
                        )
                        .on_press_tx(move |state, tx| chosen(state, part, option, tx));
                    if focused {
                        item = item.border(2.0, palette.ink);
                    }
                    item
                }))
        }));
    Some([outside, content])
}
