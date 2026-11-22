use std::collections::VecDeque;

use rune::{TextEdit, Tx};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::connection::SnoozeSetting;
use crate::snooze::{self, Day, MAX_MINUTES, MAX_OPTIONS, Preset, Unit};

use super::{State, start_write};

mod clock;
mod view;

pub(super) use view::{ADD, can_reset, overlay, section};

#[derive(Default)]
pub(in crate::ui::settings) struct Panel {
    saved: Option<Vec<Preset>>,
    rows: Vec<Row>,
    next_id: u64,
    queue: VecDeque<Vec<Preset>>,
    awaiting_echo: VecDeque<Vec<Preset>>,
    sent: Option<Uuid>,
    menu: Option<view::Menu>,
    clock: Option<clock::Picker>,
    menu_typeahead: super::keyboard::Typeahead,
    list_had_focus: bool,
}

struct Row {
    id: u64,
    preset: Preset,
    unit: Unit,
    duration: u16,
    time: String,
    input: TextEdit,
    dirty: bool,
}

impl Row {
    fn new(id: u64, preset: Preset) -> Self {
        let unit = match preset {
            Preset::After { minutes } => Unit::for_duration(minutes),
            Preset::At { .. } => Unit::Hours,
        };
        let mut row = Self {
            id,
            preset,
            unit,
            duration: 60,
            time: "09:00".into(),
            input: TextEdit::default(),
            dirty: false,
        };
        row.refresh_input();
        row
    }

    fn key(&self, field: &str) -> String {
        format!("snooze-preset-{}-{field}", self.id)
    }

    fn refresh_input(&mut self) {
        match &self.preset {
            Preset::After { minutes } => {
                self.duration = *minutes;
                if !minutes.is_multiple_of(self.unit.minutes()) {
                    self.unit = Unit::for_duration(*minutes);
                }
                self.input
                    .replace_text((*minutes / self.unit.minutes()).to_string());
            }
            Preset::At { time, .. } => {
                self.time.clone_from(time);
                self.input.replace_text(display_time(time));
            }
        }
        self.dirty = false;
    }

    fn set_preset(&mut self, next: Preset) -> bool {
        let changed = self.preset != next;
        self.preset = next;
        self.refresh_input();
        changed
    }

    fn adjust_amount(&mut self, amount: u16) -> bool {
        self.set_preset(Preset::After {
            minutes: amount.clamp(1, MAX_MINUTES / self.unit.minutes()) * self.unit.minutes(),
        })
    }

    fn choose_day(&mut self, day: Option<Day>) -> bool {
        let (next_day, time) = (day, self.time.clone());
        let next = match next_day {
            Some(day) => Preset::At { day, time },
            None => {
                if matches!(self.preset, Preset::At { .. }) {
                    self.unit = Unit::for_duration(self.duration);
                }
                Preset::After {
                    minutes: self.duration,
                }
            }
        };
        self.set_preset(next)
    }

    fn commit(&mut self) -> bool {
        match self.preset {
            Preset::After { .. } => {
                if let Ok(value) = self.input.text.trim().parse::<f64>()
                    && value.is_finite()
                {
                    return self.adjust_amount(value.round() as u16);
                }
            }
            Preset::At { day, .. } => {
                if let Some(time) = canonical_time(&self.input.text) {
                    return self.set_preset(Preset::At { day, time });
                }
            }
        }
        self.refresh_input();
        false
    }
}

fn display_time(time: &str) -> String {
    let (hour, minute) = time.split_once(':').expect("valid snooze time");
    let hour = hour.parse::<u8>().expect("valid snooze hour");
    let period = if hour < 12 { "AM" } else { "PM" };
    let hour = if hour.is_multiple_of(12) {
        12
    } else {
        hour % 12
    };
    format!("{hour:02}:{minute} {period}")
}

fn canonical_time(value: &str) -> Option<String> {
    let value = value.trim();
    if snooze::valid_time(value) {
        return Some(value.to_owned());
    }
    let (time, period) = value.split_once(' ')?;
    let (hour, minute) = time.split_once(':')?;
    let hour = hour
        .parse::<u8>()
        .ok()
        .filter(|hour| (1..=12).contains(hour))?;
    let offset = if period.eq_ignore_ascii_case("AM") {
        0
    } else if period.eq_ignore_ascii_case("PM") {
        12
    } else {
        return None;
    };
    let hour = hour % 12 + offset;
    let time = format!("{hour:02}:{minute}");
    snooze::valid_time(&time).then_some(time)
}

impl Panel {
    pub(in crate::ui::settings) fn apply(&mut self, setting: Option<&SnoozeSetting>) {
        let saved = setting.map(|setting| setting.presets.as_slice());
        if self.saved.as_deref() == saved {
            return;
        }
        self.saved = saved.map(ToOwned::to_owned);
        if let Some(index) = self
            .awaiting_echo
            .iter()
            .position(|written| Some(written.as_slice()) == saved)
        {
            self.awaiting_echo.drain(..=index);
            return;
        }
        self.queue.clear();
        self.awaiting_echo.clear();
        self.replace_all(saved.unwrap_or_default());
    }

    pub(in crate::ui::settings) fn settle(&mut self, operation: Uuid, success: bool) {
        if self.sent != Some(operation) {
            return;
        }
        self.sent = None;
        if !success {
            self.queue.clear();
            self.awaiting_echo.clear();
            self.replace_all(&self.saved.clone().unwrap_or_default());
        }
    }

    pub(super) fn close_menu(&mut self) {
        self.menu = None;
        self.clock = None;
        self.menu_typeahead.clear();
    }

    fn replace_all(&mut self, presets: &[Preset]) {
        self.rows.clear();
        for preset in presets {
            self.add(preset.clone());
        }
        self.sort();
        self.close_menu();
        self.list_had_focus = false;
    }

    fn add(&mut self, preset: Preset) {
        self.rows.push(Row::new(self.next_id, preset));
        self.next_id += 1;
    }

    fn new_candidate(&self) -> Preset {
        let extras = [
            Preset::After { minutes: 30 },
            Preset::At {
                day: Day::Saturday,
                time: "09:00".into(),
            },
            Preset::After { minutes: 24 * 60 },
        ];
        snooze::defaults()
            .iter()
            .chain(extras.iter())
            .find(|candidate| !self.rows.iter().any(|row| row.preset == **candidate))
            .cloned()
            .unwrap_or(Preset::After { minutes: 60 })
    }

    fn sort(&mut self) {
        let now = clock_minute();
        self.rows.sort_by_cached_key(|row| {
            let instant = snooze::resolve(&row.preset, now, false).map(|wake| wake.instant);
            (instant.is_none(), instant)
        });
    }

    fn presets(&self) -> Vec<Preset> {
        self.rows.iter().map(|row| row.preset.clone()).collect()
    }

    fn row_mut(&mut self, id: u64) -> Option<&mut Row> {
        self.rows.iter_mut().find(|row| row.id == id)
    }
}

fn clock_minute() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    OffsetDateTime::from_unix_timestamp(now.unix_timestamp().div_euclid(60) * 60).unwrap_or(now)
}

pub(super) fn focus_editor(state: &State, tx: &mut Tx) {
    let key = state
        .settings
        .workspace
        .snooze
        .rows
        .first()
        .map_or_else(|| ADD.to_owned(), |row| row.key("when"));
    tx.focus(key, true);
}

pub(super) fn setting(state: &State) -> Option<&SnoozeSetting> {
    state.settings.workspace.current.as_ref()?.snooze.as_ref()
}

fn writable(state: &State) -> bool {
    state.settings.workspace.scoped_writable(
        state,
        setting(state).is_some_and(|setting| setting.writable),
    )
}

fn send_next(state: &mut State) -> bool {
    let panel = &state.settings.workspace;
    if panel.writes_pending() || panel.snooze.sent.is_some() || !writable(state) {
        return false;
    }
    let Some(presets) = state.settings.workspace.snooze.queue.pop_front() else {
        return false;
    };
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_snooze_presets(presets.clone()));
    if operation.is_some() {
        state
            .settings
            .workspace
            .snooze
            .awaiting_echo
            .push_back(presets);
        state.settings.workspace.snooze.sent = operation;
    }
    start_write(&mut state.settings.workspace, operation);
    true
}

fn save(state: &mut State) {
    let panel = &mut state.settings.workspace.snooze;
    let presets = panel.presets();
    let latest = panel
        .queue
        .back()
        .or_else(|| panel.awaiting_echo.back())
        .or(panel.saved.as_ref());
    if latest != Some(&presets) {
        panel.queue.push_back(presets);
        send_next(state);
    }
}

fn edit(state: &mut State, id: u64, action: impl FnOnce(&mut Row) -> bool) {
    if writable(state)
        && let Some(row) = state.settings.workspace.snooze.row_mut(id)
        && action(row)
    {
        save(state);
    }
}

pub(in crate::ui) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let focused = tx.focused();
    let in_list = focused.is_some_and(|key| key.starts_with("snooze-preset-"));
    let panel = &mut state.settings.workspace.snooze;
    let mut changed = false;
    let mut refreshed = false;
    for row in &mut panel.rows {
        if row.dirty && focused != Some(row.key("value").as_str()) {
            changed |= row.commit();
            refreshed = true;
        }
    }
    if changed {
        save(state);
    }
    let panel = &mut state.settings.workspace.snooze;
    let leaving = panel.list_had_focus && !in_list;
    if leaving {
        panel.sort();
    }
    panel.list_had_focus = in_list;
    send_next(state) || refreshed || leaving
}

fn reset(state: &mut State, tx: &mut Tx) {
    let panel = &mut state.settings.workspace.snooze;
    panel.replace_all(snooze::defaults());
    tx.focus("snooze-options", tx.focus_visible());
    save(state);
}

fn add(state: &mut State, tx: &mut Tx) {
    let panel = &mut state.settings.workspace.snooze;
    if panel.rows.len() < MAX_OPTIONS {
        panel.add(panel.new_candidate());
        tx.focus_when_shown(ADD, tx.focus_visible());
        save(state);
    }
}

fn remove(state: &mut State, id: u64, tx: &mut Tx) {
    let panel = &mut state.settings.workspace.snooze;
    if let Some(index) = panel.rows.iter().position(|row| row.id == id) {
        panel.rows.remove(index);
        let target = panel
            .rows
            .get(index.min(panel.rows.len().saturating_sub(1)))
            .map_or_else(|| ADD.to_owned(), |row| row.key("remove"));
        tx.focus_when_shown(target, tx.focus_visible());
        save(state);
    }
}
