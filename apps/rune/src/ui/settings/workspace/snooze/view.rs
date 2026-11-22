use std::collections::{HashMap, HashSet};

use rune::{
    Align, Color, El, Icon, Key, KeyEvent, Shadow, Side, TextStyle, Tx, col, el, icon, row, text,
    text_input,
};

use crate::snooze::{self, Day, MAX_MINUTES, MAX_OPTIONS, Preset, Unit, Wake};
use crate::ui::settings::archive::press;

use super::super::{
    CHEVRON, CONTROL, DESCRIPTION, ITEM, Palette, RESET, SECTION, State, inheritance, keyboard,
    surface,
};
use super::{Row, add, canonical_time, clock_minute, edit, remove, reset, setting, writable};

pub(in crate::ui::settings::workspace) const ADD: &str = "snooze-preset-add";
const LIST: &str = "snooze-preset-options";
const VALUE: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const CLOCK: Icon = Icon {
    name: "snooze-time-clock",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></g>"#,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    When,
    Unit,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::When => "When",
            Self::Unit => "Unit",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::When => "when",
            Self::Unit => "unit",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Menu {
    row: u64,
    field: Field,
    highlighted: usize,
}

pub(in crate::ui::settings::workspace) fn section(
    state: &State,
    palette: Palette,
    width: f32,
) -> Option<El> {
    if state.settings.workspace.project_selected() {
        return None;
    }
    let current = setting(state)?;
    let panel = &state.settings.workspace.snooze;
    let may_edit = writable(state);
    let resettable = can_reset(state);
    let mut title = row()
        .items_center()
        .gap(6.0)
        .child(text("Snooze options").font(CONTROL).color(palette.ink))
        .child(inheritance::marker(
            state,
            inheritance::Setting::Snooze,
            palette,
        ));
    if may_edit && resettable {
        title = title.child(
            press(
                el().key("reset-snooze-options")
                    .role("button")
                    .label("Reset snooze options to default")
                    .size(20.0, 20.0)
                    .center()
                    .radius(4.0)
                    .focusable(true)
                    .hover(palette.hover)
                    .child(icon(&RESET, 12.0).color(palette.muted).a11y_hidden()),
                reset,
            )
            .help("Reset to default"),
        );
    }
    let now = clock_minute();
    let previews = panel
        .rows
        .iter()
        .map(|row| snooze::resolve(&row.preset, now, current.twenty_four_hour))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let mut wake_times = HashSet::new();
    let rows = panel
        .rows
        .iter()
        .zip(&previews)
        .enumerate()
        .map(|(index, (value, wake))| {
            let preview = match wake {
                None => Preview::Note("Passed for today"),
                Some(_) if seen.contains(&value.preset) => {
                    Preview::Note("Duplicate of an option above")
                }
                Some(wake) if !wake_times.insert(wake.instant) => {
                    Preview::Note("Same time as an option above")
                }
                Some(wake) => {
                    seen.insert(&value.preset);
                    Preview::Wake(wake)
                }
            };
            option_row(state, value, index, preview, palette, width, may_edit)
        });
    let full = panel.rows.len() == MAX_OPTIONS;
    let add_button = press(
        row()
            .key(ADD)
            .role("button")
            .label("Add option")
            .focusable(may_edit)
            .disabled(!may_edit)
            .h(24.0)
            .px(6.0)
            .items_center()
            .gap(5.0)
            .radius(5.0)
            .hover(palette.hover)
            .child(text("+").font(CONTROL).color(palette.muted).a11y_hidden())
            .child(text("Add option").font(DESCRIPTION).color(palette.muted)),
        add,
    );
    let mut controls = row().items_center().gap(8.0).child(add_button);
    if full {
        controls = controls.child(
            text("Up to 8 options")
                .font(DESCRIPTION)
                .color(palette.muted),
        );
    }
    let editor = col()
        .w_full()
        .gap(6.0)
        .children(rows)
        .child(
            text(menu_preview(previews.iter().flatten()))
                .font(DESCRIPTION)
                .color(palette.muted)
                .wrap(),
        )
        .child(controls);
    let mut content = col()
        .key("snooze-options")
        .role("group")
        .label("Snooze options")
        .focusable(true)
        .tab_index(-1)
        .no_focus_ring()
        .w_full()
        .px(16.0)
        .pt(12.0)
        .pb(8.0)
        .gap(4.0)
        .hover_group()
        .child(title)
        .child(
            text("Choices in every snooze menu, listed soonest first. Weekdays mean the next one after today.")
                .font(DESCRIPTION)
                .color(palette.muted)
                .max_w(496.0)
                .wrap(),
        )
        .child(editor.pt(8.0));
    if let Some(organization) = current.managed_by.as_deref() {
        content = content.help(format!("Managed by {organization}."));
    }
    Some(
        col()
            .key("settings-snooze")
            .w_full()
            .max_w(800.0)
            .gap(10.0)
            .child(
                text("Snooze")
                    .role("heading")
                    .font(SECTION)
                    .color(palette.ink)
                    .px(16.0),
            )
            .child(
                col()
                    .w_full()
                    .radius(12.0)
                    .border(1.0, palette.line)
                    .border_layout()
                    .child(content),
            ),
    )
}

pub(in crate::ui::settings::workspace) fn can_reset(state: &State) -> bool {
    let presets = state
        .settings
        .workspace
        .snooze
        .rows
        .iter()
        .map(|row| &row.preset);
    writable(state) && !same_options(presets, snooze::defaults())
}

fn same_options<'a>(options: impl Iterator<Item = &'a Preset>, defaults: &[Preset]) -> bool {
    let mut values = options.collect::<Vec<_>>();
    for previous in defaults {
        let Some(index) = values.iter().position(|value| *value == previous) else {
            return false;
        };
        values.swap_remove(index);
    }
    values.is_empty()
}

enum Preview<'a> {
    Note(&'static str),
    Wake(&'a Wake),
}

fn menu_preview<'a>(wakes: impl Iterator<Item = &'a Wake>) -> String {
    let mut wakes = wakes.collect::<Vec<_>>();
    wakes.sort_by_key(|wake| wake.instant.unix_timestamp().div_euclid(60));
    wakes.dedup_by_key(|wake| wake.instant.unix_timestamp().div_euclid(60));
    if wakes.is_empty() {
        return "Menus offer only Custom…".into();
    }
    let counts = wakes
        .iter()
        .fold(HashMap::<&str, usize>::new(), |mut counts, wake| {
            *counts.entry(&wake.label).or_default() += 1;
            counts
        });
    let labels = wakes
        .iter()
        .map(|wake| {
            if counts[wake.label.as_str()] > 1 {
                format!("{} {}", wake.label, wake.when)
            } else {
                wake.label.clone()
            }
        })
        .collect::<Vec<_>>();
    format!("Menu now: {}", labels.join(" · "))
}

fn option_row(
    state: &State,
    value: &Row,
    index: usize,
    preview: Preview<'_>,
    palette: Palette,
    width: f32,
    may_edit: bool,
) -> El {
    let id = value.id;
    let duration = matches!(value.preset, Preset::After { .. });
    let when = match value.preset {
        Preset::At { day, .. } => day.label(),
        Preset::After { .. } => "In",
    };
    let editor = input(value, duration, may_edit, palette);
    let mut fields = row().gap(8.0).items_center().child(select(
        state,
        value,
        Field::When,
        when,
        144.0,
        may_edit,
        palette,
    ));
    if duration {
        let amount = value.duration / value.unit.minutes();
        let amount_input = row()
            .w(128.0)
            .h(28.0)
            .items_center()
            .radius(8.0)
            .border(1.0, palette.line)
            .border_layout()
            .fill(surface(palette, "#fcfcfc", "#171717"))
            .child(step(value, -1, "Decrease", may_edit && amount > 1, palette))
            .child(editor.grow(1.0).min_w(0.0))
            .child(step(
                value,
                1,
                "Increase",
                may_edit && amount < MAX_MINUTES / value.unit.minutes(),
                palette,
            ));
        fields = fields.child(amount_input).child(select(
            state,
            value,
            Field::Unit,
            value.unit.label(amount),
            144.0,
            may_edit,
            palette,
        ));
    } else {
        fields = fields
            .child(text("at").font(DESCRIPTION).color(palette.muted))
            .child(
                row()
                    .w(128.0)
                    .h(28.0)
                    .items_center()
                    .gap(6.0)
                    .px(9.0)
                    .radius(8.0)
                    .border(1.0, palette.line)
                    .border_layout()
                    .fill(surface(palette, "#fcfcfc", "#171717"))
                    .child(editor.grow(1.0).min_w(0.0))
                    .child(icon(&CLOCK, 14.0).color(palette.ink).a11y_hidden()),
            );
    }
    let preview = match preview {
        Preview::Note(note) => row().child(text(note).font(DESCRIPTION).color(palette.muted)),
        Preview::Wake(wake) => row()
            .gap(8.0)
            .items_center()
            .child(text(&wake.label).font(DESCRIPTION).color(palette.ink))
            .child(text(&wake.when).font(DESCRIPTION).color(palette.muted)),
    };
    let tail = row().gap(8.0).items_center().child(preview).child(press(
        el().key(value.key("remove"))
            .role("button")
            .label(format!("Remove snooze option {}", index + 1))
            .size(28.0, 28.0)
            .center()
            .radius(5.0)
            .focusable(may_edit)
            .disabled(!may_edit)
            .hover(palette.hover)
            .child(text("×").font(VALUE).color(palette.muted).a11y_hidden()),
        move |state, tx| remove(state, id, tx),
    ));
    let contents = if width >= if duration { 730.0 } else { 620.0 } {
        row()
            .items_center()
            .justify_between()
            .gap(8.0)
            .child(fields)
            .child(tail)
    } else {
        col().gap(4.0).child(fields).child(tail.self_end())
    };
    contents
        .key(value.key("row"))
        .role("group")
        .label(format!("Snooze option {}", index + 1))
        .w_full()
}

fn input(value: &Row, duration: bool, may_edit: bool, palette: Palette) -> El {
    let id = value.id;
    let element = text_input(&value.input, VALUE)
        .key(value.key("value"))
        .label(if duration { "Amount" } else { "Time" })
        .single_line()
        .align(if duration {
            Align::Center
        } else {
            Align::Start
        })
        .h(28.0)
        .color(palette.ink)
        .focus_ring(1.0, 1.0, palette.accent);
    if !may_edit {
        return element.disabled(true).read_only(true);
    }
    element
        .on_edit(move |state: &mut State| {
            &mut state
                .settings
                .workspace
                .snooze
                .row_mut(id)
                .expect("visible snooze option")
                .input
        })
        .on_input_change(move |state: &mut State, _| {
            if let Some(row) = state.settings.workspace.snooze.row_mut(id) {
                row.dirty = true;
            }
            if !duration {
                edit(state, id, |row| {
                    canonical_time(&row.input.text).is_some() && row.commit()
                });
            }
        })
        .on_submit(move |state, _| edit(state, id, Row::commit))
        .on_input_key(move |state, event, _| {
            if !duration {
                return false;
            }
            let increase = match event.key {
                Key::Up => true,
                Key::Down => false,
                _ => return false,
            };
            change_amount(state, id, if increase { 1 } else { -1 });
            true
        })
}

fn step(value: &Row, change: i16, label: &'static str, enabled: bool, palette: Palette) -> El {
    let id = value.id;
    press(
        el().key(value.key(if change < 0 { "decrease" } else { "increase" }))
            .role("button")
            .label(label)
            .size(26.0, 26.0)
            .center()
            .focusable(enabled)
            .disabled(!enabled)
            .hover(palette.hover)
            .child(
                text(if change < 0 { "−" } else { "+" })
                    .font(VALUE)
                    .color(palette.muted)
                    .a11y_hidden(),
            ),
        move |state, _| change_amount(state, id, change),
    )
}

fn change_amount(state: &mut State, id: u64, change: i16) {
    edit(state, id, |row| {
        let drafted = row.dirty && row.commit();
        let next = (row.duration / row.unit.minutes()).saturating_add_signed(change);
        row.adjust_amount(next) || drafted
    });
}

fn select(
    state: &State,
    value: &Row,
    field: Field,
    label: &str,
    width: f32,
    may_edit: bool,
    palette: Palette,
) -> El {
    let id = value.id;
    let expanded = state
        .settings
        .workspace
        .snooze
        .menu
        .is_some_and(|menu| menu.row == id && menu.field == field);
    row()
        .key(value.key(field.key()))
        .role("combobox")
        .label(field.name())
        .a11y_value(label)
        .expanded(expanded)
        .focusable(may_edit)
        .disabled(!may_edit)
        .size(width, 28.0)
        .px(8.0)
        .items_center()
        .gap(6.0)
        .radius(8.0)
        .fill(surface(palette, "#fcfcfc", "#171717"))
        .border(1.0, palette.line)
        .border_layout()
        .focus_ring(2.0, 1.0, palette.accent)
        .on_press_tx(move |state, tx| open(state, id, field, tx))
        .on_key(move |state, event, tx| {
            if matches!(event.key, Key::Up | Key::Down)
                || event.key == Key::Enter && event.mods == rune::Mods::default()
            {
                if !tx.key_repeat() {
                    open(state, id, field, tx);
                }
                return true;
            }
            false
        })
        .child(
            text(label)
                .font(ITEM)
                .color(palette.ink)
                .grow(1.0)
                .truncate(),
        )
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden())
}

fn labels(row: &Row, field: Field) -> Vec<&'static str> {
    match field {
        Field::When => std::iter::once("In")
            .chain(Day::ALL.map(Day::label))
            .collect(),
        Field::Unit => Unit::ALL
            .into_iter()
            .map(|unit| unit.label(row.duration / row.unit.minutes()))
            .collect(),
    }
}

fn selected(row: &Row, field: Field) -> usize {
    match field {
        Field::When => match row.preset {
            Preset::After { .. } => 0,
            Preset::At { day, .. } => {
                Day::ALL
                    .iter()
                    .position(|candidate| *candidate == day)
                    .unwrap_or_default()
                    + 1
            }
        },
        Field::Unit => Unit::ALL
            .iter()
            .position(|unit| *unit == row.unit)
            .unwrap_or_default(),
    }
}

fn open(state: &mut State, id: u64, field: Field, tx: &mut Tx) {
    if !writable(state) {
        return;
    }
    edit(state, id, |row| row.dirty && row.commit());
    let Some(row) = state
        .settings
        .workspace
        .snooze
        .rows
        .iter()
        .find(|row| row.id == id)
    else {
        return;
    };
    let highlighted = selected(row, field);
    state.settings.workspace.close_popups();
    state.settings.workspace.snooze.menu = Some(Menu {
        row: id,
        field,
        highlighted,
    });
    tx.focus_when_shown(LIST, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    let panel = &mut state.settings.workspace.snooze;
    let Some(menu) = panel.menu.take() else {
        return;
    };
    if let Some(row) = panel.row_mut(menu.row) {
        tx.return_focus(row.key(menu.field.key()), tx.focus_visible());
    }
}

fn choose(state: &mut State, index: usize, tx: &mut Tx) {
    let Some(menu) = state.settings.workspace.snooze.menu else {
        return;
    };
    close(state, tx);
    edit(state, menu.row, |row| match menu.field {
        Field::When => row.choose_day(
            index
                .checked_sub(1)
                .and_then(|index| Day::ALL.get(index))
                .copied(),
        ),
        Field::Unit => {
            let amount = row.duration / row.unit.minutes();
            row.unit = Unit::ALL[index];
            row.set_preset(Preset::After {
                minutes: (u32::from(amount) * u32::from(row.unit.minutes()))
                    .min(u32::from(MAX_MINUTES)) as u16,
            })
        }
    });
}

pub(in crate::ui::settings::workspace) fn overlay(
    state: &State,
    palette: Palette,
) -> Option<[El; 2]> {
    let panel = &state.settings.workspace.snooze;
    let menu = panel.menu?;
    let value = panel.rows.iter().find(|row| row.id == menu.row)?;
    let anchor = value.key(menu.field.key());
    let selected = selected(value, menu.field);
    let options = labels(value, menu.field);
    let outside = el()
        .key("snooze-select-dismiss")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .role("none")
        .on_background_press_tx(close);
    let list = col()
        .key(LIST)
        .role("listbox")
        .label(menu.field.name())
        .focusable(true)
        .active_descendant(option_key(menu.highlighted))
        .anchored_fit(anchor, Side::Bottom, Align::Start, 4.0)
        .anchored_round()
        .w(if menu.field == Field::When {
            128.0
        } else {
            90.0
        })
        .padding(4.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(surface(palette, "#ffffff", "#111111").alpha(if palette.dark { 0.84 } else { 0.97 }))
        .backdrop_blur(16.0)
        .box_shadow(Shadow {
            x: 0.0,
            y: 16.0,
            blur: 40.0,
            spread: -18.0,
            color: Color::BLACK.alpha(if palette.dark { 0.80 } else { 0.55 }),
            inset: false,
        })
        .on_key(navigate)
        .children(options.into_iter().enumerate().map(|(index, label)| {
            row()
                .key(option_key(index))
                .role("option")
                .label(label)
                .focus_selected(selected == index)
                .items_center()
                .px(8.0)
                .h(28.0)
                .radius(4.0)
                .fill(if index == menu.highlighted {
                    palette.ink.alpha(0.08)
                } else {
                    Color::TRANSPARENT
                })
                .child(text(label).font(ITEM).color(palette.ink))
                .on_hover(move |state: &mut State| {
                    if let Some(menu) = &mut state.settings.workspace.snooze.menu {
                        menu.highlighted = index;
                    }
                })
                .on_press_tx(move |state, tx| choose(state, index, tx))
        }));
    Some([outside, list])
}

fn option_key(index: usize) -> String {
    format!("snooze-preset-choice-{index}")
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let panel = &mut state.settings.workspace.snooze;
    let Some(menu) = panel.menu else {
        return false;
    };
    match event.key {
        Key::Escape => close(state, tx),
        Key::Enter => choose(state, menu.highlighted, tx),
        Key::Text(ref key) if key == " " && !panel.menu_typeahead.typing() => {
            choose(state, menu.highlighted, tx);
        }
        Key::Tab => {
            let next = panel.rows.iter().find(|row| row.id == menu.row).map(|row| {
                row.key(match (menu.field, event.mods.shift) {
                    (Field::When, false) | (Field::Unit, true) => "value",
                    (Field::When, true) => "when",
                    (Field::Unit, false) => "remove",
                })
            });
            close(state, tx);
            if let Some(key) = next {
                tx.focus(key, true);
            }
        }
        _ => {
            let Some(row) = panel.rows.iter().find(|row| row.id == menu.row) else {
                return false;
            };
            let options = labels(row, menu.field);
            let next =
                keyboard::arrow_index(&event.key, menu.highlighted, options.len()).or_else(|| {
                    let indexed = options.iter().copied().map(Some).collect::<Vec<_>>();
                    match panel
                        .menu_typeahead
                        .input(event, menu.highlighted, &indexed)
                    {
                        keyboard::Input::Typed(result) => result.or(Some(menu.highlighted)),
                        keyboard::Input::Other => None,
                    }
                });
            let Some(next) = next else {
                return false;
            };
            if let Some(menu) = &mut panel.menu {
                menu.highlighted = next;
            }
        }
    }
    true
}
