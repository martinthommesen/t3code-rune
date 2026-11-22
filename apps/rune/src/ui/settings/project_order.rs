use rune::{Align, Color, El, Key, KeyEvent, Shadow, Side, Tx, col, el, icon, row, text};

use crate::client_settings::ProjectOrder;

use super::workspace::{CHEVRON, ITEM, keyboard, surface};
use super::{CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, icon_button};

pub(super) const ROW: &str = "project-order";
const TRIGGER: &str = "project-order-select";
const LIST: &str = "project-order-options";
const LABEL: &str = "Project order";
const WIDTH: f32 = 176.0;
const OPTIONS: [ProjectOrder; 3] = [
    ProjectOrder::Activity,
    ProjectOrder::Created,
    ProjectOrder::Manual,
];

#[derive(Default)]
pub(super) struct Panel {
    highlighted: Option<usize>,
    typeahead: keyboard::Typeahead,
}

impl Panel {
    pub(super) fn clear(&mut self) {
        self.highlighted = None;
        self.typeahead.clear();
    }
}

pub(super) fn section(state: &State, palette: Palette, width: f32) -> El {
    let current = state.client_settings.project_order();
    let active = state.settings.project_order.highlighted;
    let mut trigger = row()
        .key(TRIGGER)
        .role("combobox")
        .label(LABEL)
        .a11y_value(label(current))
        .expanded(active.is_some())
        .focusable(true)
        .size(WIDTH, 28.0)
        .min_w(WIDTH)
        .items_center()
        .gap(6.0)
        .px(9.0)
        .radius(8.0)
        .fill(surface(palette, "#fcfcfc", "#111111"))
        .border(1.0, surface(palette, "#d4d4d8", "#292929"))
        .focus_ring(2.0, 1.0, palette.accent)
        .on_press_tx(open)
        .on_key(trigger_input)
        .child(text(label(current)).font(ITEM).color(palette.ink).grow(1.0))
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden());
    if let Some(active) = active {
        trigger = trigger.active_descendant(option_key(OPTIONS[active]));
    }
    let mut title = row()
        .items_center()
        .gap(6.0)
        .child(text(LABEL).font(CONTROL).color(palette.ink));
    if current != ProjectOrder::default() {
        title = title.child(
            icon_button(
                "reset-project-order",
                "Reset project order to default",
                &RESET,
                12.0,
                20.0,
                palette,
                reset,
            )
            .help("Reset to default"),
        );
    }
    let details = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text("Order of projects in the sidebar project picker and command palette.")
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let setting = if width >= 560.0 {
        row().items_center().gap(32.0).child(details).child(trigger)
    } else {
        col().gap(12.0).child(details).child(trigger)
    };
    col()
        .w_full()
        .gap(12.0)
        .child(
            text("Sidebar")
                .font(SECTION)
                .color(palette.ink)
                .role("heading")
                .px(16.0),
        )
        .child(
            setting
                .key(ROW)
                .focusable(true)
                .tab_index(-1)
                .no_focus_ring()
                .w_full()
                .px(16.0)
                .py(12.0)
                .radius(12.0)
                .border(1.0, palette.line)
                .border_layout()
                .fill(surface(palette, "#fcfcfc", "#0a0a0a")),
        )
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let active = state.settings.project_order.highlighted?;
    let current = state.client_settings.project_order();
    let outside = el()
        .key("project-order-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    let options = col()
        .key(LIST)
        .role("listbox")
        .label(LABEL)
        .focusable(true)
        .active_descendant(option_key(OPTIONS[active]))
        .anchored_fit(TRIGGER, Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .w(WIDTH)
        .padding(4.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(surface(palette, "#ffffff", "#111111").alpha(if palette.dark { 0.92 } else { 0.97 }))
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
        .children(OPTIONS.into_iter().enumerate().map(|(index, mode)| {
            row()
                .key(option_key(mode))
                .role("option")
                .label(label(mode))
                .focus_selected(mode == current)
                .w_full()
                .h(28.0)
                .radius(4.0)
                .px(8.0)
                .items_center()
                .fill(if index == active {
                    palette.ink.alpha(0.08)
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    state.settings.project_order.highlighted = Some(index)
                })
                .on_press_tx(move |state, tx| choose(state, mode, tx))
                .child(text(label(mode)).font(ITEM).color(palette.ink))
        }));
    Some([outside, options])
}

fn open(state: &mut State, tx: &mut Tx) {
    state.settings.project_order.clear();
    state.settings.project_order.highlighted = Some(index(state.client_settings.project_order()));
    tx.focus_when_shown(LIST, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.project_order.clear();
    tx.return_focus(TRIGGER, tx.focus_visible());
}

fn choose(state: &mut State, mode: ProjectOrder, tx: &mut Tx) {
    close(state, tx);
    save(state, mode);
}

fn reset(state: &mut State, tx: &mut Tx) {
    save(state, ProjectOrder::default());
    tx.focus_when_shown(ROW, tx.focus_visible());
}

fn save(state: &mut State, mode: ProjectOrder) {
    if mode != state.client_settings.project_order() {
        state.settings.error = state
            .client_settings
            .set_project_order(mode)
            .err()
            .map(|error| format!("Could not save client settings. {error}"));
    }
}

fn trigger_input(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if matches!(event.key, Key::Up | Key::Down) {
        open(state, tx);
        return true;
    }
    if event.key == Key::Tab {
        state.settings.project_order.typeahead.clear();
    }
    typeahead(state, event, tx)
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(active) = state.settings.project_order.highlighted else {
        return false;
    };
    if typeahead(state, event, tx) {
        return true;
    }
    if let Some(index) = keyboard::arrow_index(&event.key, active, OPTIONS.len()) {
        state.settings.project_order.highlighted = Some(index);
        return true;
    }
    match &event.key {
        Key::Escape => close(state, tx),
        Key::Enter => choose(state, OPTIONS[active], tx),
        Key::Text(value) if value == " " => choose(state, OPTIONS[active], tx),
        Key::Tab => {
            close(state, tx);
            return false;
        }
        _ => return false,
    }
    true
}

fn typeahead(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let panel = &mut state.settings.project_order;
    if matches!(&event.key, Key::Text(value) if value == " ") && !panel.typeahead.typing() {
        return false;
    }
    let active = panel.highlighted;
    let anchor = active.unwrap_or_else(|| index(state.client_settings.project_order()));
    match panel
        .typeahead
        .input(event, anchor, &OPTIONS.map(|mode| Some(label(mode))))
    {
        keyboard::Input::Other => false,
        keyboard::Input::Typed(matched) => {
            if let Some(index) = matched {
                if active.is_some() {
                    panel.highlighted = Some(index);
                } else {
                    choose(state, OPTIONS[index], tx);
                }
            }
            true
        }
    }
}

fn index(mode: ProjectOrder) -> usize {
    OPTIONS
        .iter()
        .position(|&option| option == mode)
        .expect("project order option")
}

fn label(mode: ProjectOrder) -> &'static str {
    match mode {
        ProjectOrder::Activity => "Last user message",
        ProjectOrder::Created => "Created at",
        ProjectOrder::Manual => "Manual",
    }
}

fn option_key(mode: ProjectOrder) -> &'static str {
    match mode {
        ProjectOrder::Activity => "project-order-activity",
        ProjectOrder::Created => "project-order-created",
        ProjectOrder::Manual => "project-order-manual",
    }
}
