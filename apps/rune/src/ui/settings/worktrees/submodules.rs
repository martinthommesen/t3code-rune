use rune::{
    Align, Color, El, Icon, Key, KeyEvent, Shadow, Side, TextStyle, Tx, col, el, icon, row, text,
};

use crate::connection::SubmoduleMode;

use super::super::{CONTROL, DESCRIPTION, Palette, RESET, State, icon_button};
use super::workspace::{self, CHEVRON, ITEM, keyboard, surface};

const DISCLOSURE: &str = "worktree-creation-advanced";
const TRIGGER: &str = "worktree-submodules";
const LIST: &str = "worktree-submodules-options";
const LABEL: &str = "Worktree submodules";
const WIDTH: f32 = 160.0;
const OPTIONS: [SubmoduleMode; 3] = [
    SubmoduleMode::Recursive,
    SubmoduleMode::TopLevel,
    SubmoduleMode::Skip,
];

static DISCLOSURE_RIGHT: Icon = Icon {
    name: "worktree-advanced-chevron-right",
    view: 24.0,
    body: r#"<path d="m9 18 6-6-6-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static DISCLOSURE_DOWN: Icon = Icon {
    name: "worktree-advanced-chevron-down",
    view: 24.0,
    body: r#"<path d="m6 9 6 6 6-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(in crate::ui::settings) struct Panel {
    expanded: bool,
    highlighted: Option<SubmoduleMode>,
    typeahead: keyboard::Typeahead,
}

impl Panel {
    pub(in crate::ui::settings) fn dismiss(&mut self) {
        self.highlighted = None;
        self.typeahead.clear();
    }

    pub(in crate::ui::settings) fn collapse(&mut self) {
        self.expanded = false;
        self.dismiss();
    }
}

pub(super) fn advanced(state: &State, palette: Palette, width: f32) -> impl Iterator<Item = El> {
    let expanded = state.settings.workspace.submodules.expanded;
    let divider = || el().w_full().h(1.0).fill(palette.line);
    let disclosure = row()
        .key(DISCLOSURE)
        .role("button")
        .label("Advanced")
        .expanded(expanded)
        .w_full()
        .h(40.0)
        .px(16.0)
        .gap(6.0)
        .items_center()
        .focusable(true)
        .focus_ring(2.0, 1.0, palette.accent)
        .on_press_tx(toggle)
        .on_key(|state: &mut State, event, tx| {
            if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ") {
                toggle(state, tx);
                true
            } else {
                false
            }
        })
        .child(
            icon(
                if expanded {
                    &DISCLOSURE_DOWN
                } else {
                    &DISCLOSURE_RIGHT
                },
                14.0,
            )
            .color(palette.muted)
            .a11y_hidden(),
        )
        .child(
            text("Advanced")
                .font(TextStyle::new(12.0, 500.0).lh(16.0))
                .color(palette.muted),
        );
    [divider(), disclosure].into_iter().chain(
        expanded
            .then(|| [divider(), setting_row(state, palette, width)])
            .into_iter()
            .flatten(),
    )
}

fn setting_row(state: &State, palette: Palette, width: f32) -> El {
    let panel = &state.settings.workspace;
    let setting = panel.submodules_view(state);
    let writable = setting.as_ref().is_some_and(|setting| setting.writable);
    let value = setting
        .as_ref()
        .map_or("Unavailable", |setting| setting.mode.label());
    let expanded = panel.submodules.highlighted.is_some();
    let project = panel.project_selected();
    let may_reset = setting.as_ref().is_some_and(|setting| setting.may_reset);
    let description = if project {
        "How new worktrees in this project populate git submodules."
    } else {
        "How new worktrees populate git submodules. Projects and their t3.json can override it."
    };

    let mut trigger = row()
        .key(TRIGGER)
        .role("combobox")
        .label(LABEL)
        .a11y_value(value)
        .expanded(expanded)
        .size(WIDTH, 28.0)
        .min_w(WIDTH)
        .px(9.0)
        .gap(6.0)
        .items_center()
        .radius(8.0)
        .fill(surface(palette, "#fcfcfc", "#171717"))
        .border(1.0, surface(palette, "#d4d4d8", "#313131"))
        .focus_ring(2.0, 1.0, palette.accent)
        .focusable(writable)
        .disabled(!writable)
        .opacity(if writable { 1.0 } else { 0.64 })
        .child(
            text(value)
                .font(ITEM)
                .color(palette.ink)
                .truncate()
                .grow(1.0),
        )
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden());
    if let Some(active) = panel.submodules.highlighted {
        trigger = trigger.active_descendant(option_key(active));
    }
    if writable {
        trigger = trigger.on_press_tx(open).on_key(trigger_input);
    } else if let Some(organization) = setting.as_ref().and_then(|setting| setting.managed_by) {
        trigger = trigger.help(format!("Managed by {organization}."));
    }

    let mut title = row()
        .gap(6.0)
        .items_center()
        .child(text("Submodules").font(CONTROL).color(palette.ink))
        .child(workspace::submodules_inheritance_marker(state, palette));
    if writable && may_reset {
        title = title.child(
            icon_button(
                "reset-worktree-submodules",
                if project {
                    "Reset Submodules to default"
                } else {
                    "Reset worktree submodules to default"
                },
                &RESET,
                12.0,
                20.0,
                palette,
                reset,
            )
            .help(if project {
                "Reset to inherited value"
            } else {
                "Reset to default"
            }),
        );
    }
    let description = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text(description)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let layout = if width >= 560.0 {
        row()
            .items_center()
            .gap(32.0)
            .child(description)
            .child(trigger)
    } else {
        col().gap(12.0).child(description).child(trigger)
    };
    layout
        .key("worktree-submodules-row")
        .w_full()
        .px(16.0)
        .py(12.0)
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let panel = &state.settings.workspace;
    let active = panel.submodules.highlighted?;
    let current = panel
        .submodules_view(state)
        .filter(|setting| setting.writable)?;
    let outside = el()
        .key("worktree-submodules-dismiss")
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
        .active_descendant(option_key(active))
        .anchored_fit(TRIGGER, Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .w(WIDTH)
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
        .children(OPTIONS.map(|mode| {
            row()
                .key(option_key(mode))
                .role("option")
                .label(mode.label())
                .focus_selected(mode == current.mode)
                .w_full()
                .h(28.0)
                .radius(4.0)
                .px(8.0)
                .items_center()
                .fill(if mode == active {
                    palette.ink.alpha(0.08)
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    state.settings.workspace.submodules.highlighted = Some(mode);
                })
                .on_press_tx(move |state, tx| choose(state, mode, tx))
                .child(text(mode.label()).font(ITEM).color(palette.ink))
        }));
    Some([outside, options])
}

fn toggle(state: &mut State, tx: &mut Tx) {
    let expanded = state.settings.workspace.submodules.expanded;
    state.settings.workspace.close_popups();
    state.settings.workspace.submodules.expanded = !expanded;
    tx.focus(DISCLOSURE, tx.focus_visible());
}

fn open(state: &mut State, tx: &mut Tx) {
    let panel = &state.settings.workspace;
    let Some(current) = panel
        .submodules_view(state)
        .filter(|setting| setting.writable)
    else {
        return;
    };
    if panel.writes_pending() {
        return;
    }
    let active = current.mode;
    state.settings.workspace.close_popups();
    state.settings.workspace.submodules.highlighted = Some(active);
    tx.focus_when_shown(LIST, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.workspace.submodules.dismiss();
    tx.return_focus(TRIGGER, tx.focus_visible());
}

fn reset(state: &mut State, tx: &mut Tx) {
    workspace::save_submodules(state, None);
    tx.focus(TRIGGER, tx.focus_visible());
}

fn choose(state: &mut State, mode: SubmoduleMode, tx: &mut Tx) {
    close(state, tx);
    if state
        .settings
        .workspace
        .submodules_view(state)
        .is_some_and(|setting| setting.mode != mode)
    {
        workspace::save_submodules(state, Some(mode));
    }
}

fn trigger_input(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if matches!(event.key, Key::Up | Key::Down | Key::Enter) {
        open(state, tx);
        return true;
    }
    if event.key == Key::Tab {
        state.settings.workspace.submodules.typeahead.clear();
    }
    typeahead(state, event, tx)
}

fn typeahead(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let current = state
        .settings
        .workspace
        .submodules_view(state)
        .map_or(SubmoduleMode::Recursive, |setting| setting.mode);
    let panel = &mut state.settings.workspace.submodules;
    if matches!(&event.key, Key::Text(value) if value == " ") && !panel.typeahead.typing() {
        return false;
    }
    let active = panel.highlighted;
    match panel.typeahead.input(
        event,
        index(active.unwrap_or(current)),
        &OPTIONS.map(|mode| Some(mode.label())),
    ) {
        keyboard::Input::Other => false,
        keyboard::Input::Typed(option) => {
            if let Some(option) = option {
                if active.is_some() {
                    panel.highlighted = Some(OPTIONS[option]);
                } else {
                    choose(state, OPTIONS[option], tx);
                }
            }
            true
        }
    }
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(active) = state.settings.workspace.submodules.highlighted else {
        return false;
    };
    if typeahead(state, event, tx) {
        return true;
    }
    if let Some(option) = keyboard::arrow_index(&event.key, index(active), OPTIONS.len()) {
        state.settings.workspace.submodules.highlighted = Some(OPTIONS[option]);
        return true;
    }
    match &event.key {
        Key::Escape => close(state, tx),
        Key::Enter => choose(state, active, tx),
        Key::Text(value) if value == " " => choose(state, active, tx),
        Key::Tab => {
            close(state, tx);
            return false;
        }
        _ => return false,
    }
    true
}

fn index(mode: SubmoduleMode) -> usize {
    match mode {
        SubmoduleMode::Recursive => 0,
        SubmoduleMode::TopLevel => 1,
        SubmoduleMode::Skip => 2,
    }
}

fn option_key(mode: SubmoduleMode) -> &'static str {
    match mode {
        SubmoduleMode::Recursive => "worktree-submodules-recursive",
        SubmoduleMode::TopLevel => "worktree-submodules-top-level",
        SubmoduleMode::Skip => "worktree-submodules-skip",
    }
}
