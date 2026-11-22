use rune::{Align, Color, El, Key, KeyEvent, Shadow, Side, Tx, col, el, icon, row, text};

use crate::connection::{BranchNamingMode, BranchTextField};

use super::super::{CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, icon_button};
use super::workspace::{self, CHEVRON, ITEM, keyboard, surface};

mod fields;

pub(in crate::ui::settings) use fields::FieldsPanel;
pub(in crate::ui) use fields::poll;

const ROW: &str = "worktree-branch-naming-row";
const TRIGGER: &str = "worktree-branch-naming";
const LIST: &str = "worktree-branch-naming-options";
const LABEL: &str = "Worktree branch naming";
const WIDTH: f32 = 160.0;
const OPTIONS: [BranchNamingMode; 3] = [
    BranchNamingMode::Static,
    BranchNamingMode::Semantic,
    BranchNamingMode::Custom,
];
const DESCRIPTION_COPY: &str =
    "Choose how new worktree branches are named from your first message.";
const SEMANTIC_STATUS: &str = concat!(
    "The model chooses a prefix that describes the work, such as ",
    "feat/add-search, fix/login-timeout, or refactor/auth.",
);

pub(super) fn section(state: &State, palette: Palette, width: f32) -> El {
    let panel = &state.settings.workspace;
    let setting = panel.branch_naming_view(state);
    let writable = setting.as_ref().is_some_and(|setting| setting.writable);
    let mode = setting.as_ref().map(|setting| setting.mode);
    let value = mode.map_or("Unavailable", label);
    let mut trigger = row()
        .key(TRIGGER)
        .role("combobox")
        .label(LABEL)
        .a11y_value(value)
        .expanded(panel.branch_naming_highlighted.is_some())
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
    if let Some(active) = panel.branch_naming_highlighted {
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
        .child(text(LABEL).font(CONTROL).color(palette.ink));
    let may_reset = setting.as_ref().is_some_and(|setting| setting.may_reset);
    if panel.project_selected() {
        let source = if may_reset {
            "Overridden for this project".to_owned()
        } else {
            state.workspace.as_ref().map_or_else(
                || "Inherited from environment".to_owned(),
                |workspace| format!("Inherited from {}", workspace.label),
            )
        };
        title = title.child(workspace::inheritance_marker(
            source,
            Palette {
                muted: if may_reset {
                    palette.accent
                } else {
                    palette.muted
                },
                ..palette
            },
        ));
    } else {
        title = title.child(el().size(20.0, 20.0).a11y_hidden());
    }
    if writable && may_reset {
        let reset_label = if panel.project_selected() {
            "Reset Worktree branch naming to default"
        } else {
            "Reset branch naming to default"
        };
        title = title.child(
            icon_button(
                "reset-branch-naming",
                reset_label,
                &RESET,
                12.0,
                20.0,
                palette,
                reset,
            )
            .help(if panel.project_selected() {
                "Reset to inherited value"
            } else {
                "Reset to default"
            }),
        );
    }
    let mut description = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text(DESCRIPTION_COPY)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    if mode == Some(BranchNamingMode::Semantic) {
        description = description.child(
            text(SEMANTIC_STATUS)
                .font(DESCRIPTION)
                .color(palette.muted)
                .wrap(),
        );
    }
    let layout = if width >= 560.0 {
        row()
            .items_center()
            .gap(32.0)
            .child(description)
            .child(trigger)
    } else {
        col().gap(12.0).child(description).child(trigger)
    };
    let mut rows = col()
        .w_full()
        .radius(12.0)
        .border(1.0, palette.line)
        .border_layout()
        .child(
            layout
                .key(ROW)
                .focusable(true)
                .tab_index(-1)
                .no_focus_ring()
                .w_full()
                .px(16.0)
                .py(12.0),
        );
    if let Some(field) = BranchTextField::ALL
        .into_iter()
        .find(|field| Some(field.mode()) == mode)
        && let Some(field) = fields::view(state, palette, width, field)
    {
        rows = rows
            .child(el().w_full().h(1.0).fill(palette.line))
            .child(field);
    }

    col()
        .key("worktree-branches")
        .w_full()
        .gap(10.0)
        .child(
            text("Branches")
                .font(SECTION)
                .color(palette.ink)
                .role("heading")
                .px(16.0),
        )
        .child(rows)
}

pub(in crate::ui::settings) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let panel = &state.settings.workspace;
    let setting = panel
        .branch_naming_view(state)
        .filter(|setting| setting.writable)?;
    let active = panel.branch_naming_highlighted?;
    let outside = el()
        .key("worktree-branch-naming-dismiss")
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
            let selected = mode == setting.mode;
            row()
                .key(option_key(mode))
                .role("option")
                .label(label(mode))
                .focus_selected(selected)
                .w_full()
                .h(28.0)
                .radius(4.0)
                .px(8.0)
                .items_center()
                .fill(if selected || mode == active {
                    palette.ink.alpha(0.08)
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    state.settings.workspace.branch_naming_highlighted = Some(mode)
                })
                .on_press_tx(move |state, tx| choose(state, Some(mode), tx))
                .child(text(label(mode)).font(ITEM).color(palette.ink))
        }));
    Some([outside, options])
}

fn open(state: &mut State, tx: &mut Tx) {
    let panel = &state.settings.workspace;
    let Some(current) = panel
        .branch_naming_view(state)
        .filter(|setting| setting.writable)
    else {
        return;
    };
    if panel.writes_pending() {
        return;
    }
    let active = current.mode;
    state.settings.workspace.close_popups();
    state.settings.workspace.branch_naming_highlighted = Some(active);
    tx.focus_when_shown(LIST, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.workspace.branch_naming_highlighted = None;
    state.settings.workspace.branch_naming_typeahead.clear();
    tx.return_focus(TRIGGER, tx.focus_visible());
}

fn reset(state: &mut State, tx: &mut Tx) {
    choose(state, None, tx);
    tx.focus(ROW, tx.focus_visible());
}

fn choose(state: &mut State, mode: Option<BranchNamingMode>, tx: &mut Tx) {
    close(state, tx);
    let panel = &state.settings.workspace;
    let Some(setting) = panel.branch_naming_view(state) else {
        return;
    };
    if mode != Some(setting.mode) && (mode.is_some() || setting.may_reset) {
        workspace::save_branch_naming(state, mode);
    }
}

fn trigger_input(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if matches!(event.key, Key::Up | Key::Down) {
        open(state, tx);
        return true;
    }
    if event.key == Key::Tab {
        state.settings.workspace.branch_naming_typeahead.clear();
    }
    typeahead(state, event, tx)
}

fn typeahead(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let current = state
        .settings
        .workspace
        .branch_naming_view(state)
        .map_or(BranchNamingMode::Static, |setting| setting.mode);
    let panel = &mut state.settings.workspace;
    if matches!(&event.key, Key::Text(value) if value == " ")
        && !panel.branch_naming_typeahead.typing()
    {
        return false;
    }
    let active = panel.branch_naming_highlighted;
    let index = index(active.unwrap_or(current));
    match panel
        .branch_naming_typeahead
        .input(event, index, &OPTIONS.map(|mode| Some(label(mode))))
    {
        keyboard::Input::Other => false,
        keyboard::Input::Typed(option) => {
            if let Some(option) = option {
                if active.is_some() {
                    panel.branch_naming_highlighted = Some(OPTIONS[option]);
                } else {
                    choose(state, Some(OPTIONS[option]), tx);
                }
            }
            true
        }
    }
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(active) = state.settings.workspace.branch_naming_highlighted else {
        return false;
    };
    if typeahead(state, event, tx) {
        return true;
    }
    if let Some(option) = keyboard::arrow_index(&event.key, index(active), OPTIONS.len()) {
        state.settings.workspace.branch_naming_highlighted = Some(OPTIONS[option]);
        return true;
    }
    match &event.key {
        Key::Escape => close(state, tx),
        Key::Enter => choose(state, Some(active), tx),
        Key::Text(value) if value == " " => choose(state, Some(active), tx),
        Key::Tab => {
            close(state, tx);
            return false;
        }
        _ => return false,
    }
    true
}

fn index(mode: BranchNamingMode) -> usize {
    OPTIONS
        .iter()
        .position(|&item| item == mode)
        .expect("branch naming option")
}

fn label(mode: BranchNamingMode) -> &'static str {
    match mode {
        BranchNamingMode::Static => "Static prefix",
        BranchNamingMode::Semantic => "Semantic prefix",
        BranchNamingMode::Custom => "Custom instructions",
    }
}

fn option_key(mode: BranchNamingMode) -> &'static str {
    match mode {
        BranchNamingMode::Static => "worktree-branch-naming-static",
        BranchNamingMode::Semantic => "worktree-branch-naming-semantic",
        BranchNamingMode::Custom => "worktree-branch-naming-custom",
    }
}
