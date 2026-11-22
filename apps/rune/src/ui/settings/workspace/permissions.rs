use rune::{
    Align, Color, El, Icon, Key, KeyEvent, Shadow, Side, TextStyle, Tx, col, el, icon, row, text,
};

use crate::connection::PermissionSetting;
use t3_rune::providers::codex::RuntimeMode;

use super::{
    CHEVRON, CONTROL, DESCRIPTION, ITEM, Palette, RESET, State, inheritance_marker, keyboard,
    start_write, surface,
};

const TRIGGER: &str = "default-permissions";
const LIST: &str = "default-permissions-options";
const MODES: [RuntimeMode; 4] = [
    RuntimeMode::ApprovalRequired,
    RuntimeMode::AutoAcceptEdits,
    RuntimeMode::Auto,
    RuntimeMode::FullAccess,
];
const OPTION_LABEL: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const OPTION_DETAIL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $path, "</g>"),
        }
    };
}

static LOCK: Icon = glyph!(
    "permission-lock",
    r#"<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>"#
);
static PEN: Icon = glyph!(
    "permission-pen",
    r#"<path d="M13 21h8"/><path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/>"#
);
static SHIELD: Icon = glyph!(
    "permission-shield",
    r#"<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>"#
);
static SHIELD_OFF: Icon = glyph!(
    "permission-shield-off",
    r#"<path d="m2 2 20 20"/><path d="M5 5a1 1 0 0 0-1 1v7c0 5 3.5 7.5 7.67 8.94a1 1 0 0 0 .67.01c2.35-.82 4.48-1.97 5.9-3.71"/><path d="M9.309 3.652A12.252 12.252 0 0 0 11.24 2.28a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1v7a9.784 9.784 0 0 1-.08 1.264"/>"#
);

fn style(mode: RuntimeMode) -> (&'static str, &'static Icon, &'static str) {
    match mode {
        RuntimeMode::ApprovalRequired => (
            "permissions-supervised",
            &LOCK,
            "Ask before commands and file changes.",
        ),
        RuntimeMode::AutoAcceptEdits => (
            "permissions-auto-accept-edits",
            &PEN,
            "Auto-approve edits, ask before other actions.",
        ),
        RuntimeMode::Auto => (
            "permissions-auto",
            &SHIELD,
            "Supported providers approve routine actions; others still ask.",
        ),
        RuntimeMode::FullAccess => (
            "permissions-full-access",
            &SHIELD_OFF,
            "Allow commands and edits without prompts.",
        ),
    }
}

struct Selection<'a> {
    settings: &'a PermissionSetting,
    project_mode: Option<RuntimeMode>,
    writable: bool,
}

impl Selection<'_> {
    fn mode(&self) -> RuntimeMode {
        self.project_mode.unwrap_or(self.settings.environment)
    }

    fn may_reset(&self, project_scope: bool) -> bool {
        if project_scope {
            self.project_mode.is_some()
        } else {
            self.mode() != RuntimeMode::FullAccess
        }
    }
}

fn selection(state: &State) -> Option<Selection<'_>> {
    let panel = &state.settings.workspace;
    let settings = &panel.current.as_ref()?.permission;
    let project_mode = panel
        .scope
        .id
        .as_ref()
        .and_then(|id| settings.projects.get(id))
        .copied();
    Some(Selection {
        settings,
        project_mode,
        writable: panel.scoped_writable(state, settings.writable),
    })
}

pub(super) fn setting_row(state: &State, palette: Palette, width: f32) -> El {
    let current = selection(state);
    let panel = &state.settings.workspace;
    let project_scope = panel.project_selected();
    let mode = current.as_ref().map(Selection::mode);
    let writable = current.as_ref().is_some_and(|current| current.writable);
    let label = mode.map_or("Unavailable", PermissionSetting::label);
    let trigger_width = if mode == Some(RuntimeMode::AutoAcceptEdits) {
        174.0
    } else {
        160.0
    };
    let mut trigger = row()
        .key(TRIGGER)
        .role("combobox")
        .label("Default permissions")
        .a11y_value(label)
        .expanded(panel.permission_highlighted.is_some())
        .disabled(!writable)
        .focusable(writable)
        .focus_ring(2.0, 1.0, palette.accent)
        .size(trigger_width, 28.0)
        .min_w(trigger_width)
        .px(9.0)
        .gap(6.0)
        .items_center()
        .radius(8.0)
        .fill(surface(palette, "#fcfcfc", "#171717"))
        .border(1.0, surface(palette, "#d4d4d8", "#313131"))
        .opacity(if writable { 1.0 } else { 0.64 });
    if let Some(mode) = mode {
        trigger = trigger.child(icon(style(mode).1, 14.0).color(palette.muted).a11y_hidden());
    }
    trigger = trigger
        .child(
            text(label)
                .font(ITEM)
                .color(palette.ink)
                .grow(1.0)
                .truncate(),
        )
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden());
    if let Some(active) = panel.permission_highlighted {
        trigger = trigger.active_descendant(style(active).0);
    }
    if writable {
        trigger = trigger.on_press_tx(open).on_key(trigger_input);
    } else if let Some(organization) = current
        .as_ref()
        .and_then(|current| current.settings.managed_by())
    {
        trigger = trigger.help(format!("Managed by {organization}."));
    } else if current
        .as_ref()
        .is_some_and(|current| !current.settings.writable)
    {
        trigger = trigger.help("This connection lacks permission to change environment settings.");
    }
    let mut title = row()
        .items_center()
        .gap(6.0)
        .child(text("Permissions").font(CONTROL).color(palette.ink));
    if let Some(current) = &current {
        if project_scope {
            let selected = current.project_mode.is_some();
            let inherited = if selected {
                "Overridden for this project".to_owned()
            } else {
                state.workspace.as_ref().map_or_else(
                    || "Inherited from environment".to_owned(),
                    |workspace| format!("Inherited from {}", workspace.label),
                )
            };
            let marker = Palette {
                muted: if selected {
                    palette.accent
                } else {
                    palette.muted
                },
                ..palette
            };
            title = title.child(inheritance_marker(inherited, marker));
        } else {
            title = title.child(el().size(20.0, 20.0).a11y_hidden());
        }
        if writable && current.may_reset(project_scope) {
            let reset = super::super::icon_button(
                "reset-default-permissions",
                if project_scope {
                    "Reset Permissions to default"
                } else {
                    "Reset default permissions to default"
                },
                &RESET,
                12.0,
                20.0,
                palette,
                |state, tx| choose(state, None, tx),
            );
            title = title.child(if project_scope {
                reset.help("Reset to inherited value")
            } else {
                reset
            });
        }
    }
    let content = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text(if project_scope {
            "Permissions for new threads in this project."
        } else {
            "Default permissions for new threads. Projects can override them."
        })
        .font(DESCRIPTION)
        .color(palette.muted)
        .wrap(),
    );
    let layout = if width >= 560.0 {
        row().items_center().gap(32.0).child(content).child(trigger)
    } else {
        col().gap(12.0).child(content).child(trigger)
    };
    layout
        .key("new-threads-permissions")
        .w_full()
        .px(16.0)
        .py(12.0)
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let current = selection(state)?;
    let active = state
        .settings
        .workspace
        .permission_highlighted
        .filter(|_| current.writable)?;
    let outside = el()
        .key("default-permissions-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    let options = col()
        .key(LIST)
        .role("listbox")
        .label("Default permissions")
        .focusable(true)
        .active_descendant(style(active).0)
        .anchored_fit(TRIGGER, Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .w(370.0)
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
        .children(MODES.into_iter().map(|mode| {
            let (key, graphic, note) = style(mode);
            let denial = current.settings.denial(mode);
            let allowed = denial.is_none();
            let selected = current.mode() == mode;
            let label = PermissionSetting::label(mode);
            let description = denial.as_deref().unwrap_or(note);
            let mut entry = col()
                .key(key)
                .role("option")
                .label(format!("{label} {description}"))
                .focus_selected(selected)
                .disabled(!allowed)
                .w_full()
                .h(46.0)
                .radius(4.0)
                .px(8.0)
                .py(4.0)
                .gap(2.0)
                .fill(if mode == active || selected {
                    palette.ink.alpha(0.08)
                } else {
                    Color::TRANSPARENT
                })
                .opacity(if allowed { 1.0 } else { 0.64 })
                .child(
                    row()
                        .gap(5.0)
                        .items_center()
                        .child(icon(graphic, 14.0).color(palette.muted).a11y_hidden())
                        .child(text(label).font(OPTION_LABEL).color(palette.ink)),
                )
                .child(text(description).font(OPTION_DETAIL).color(palette.muted));
            if allowed {
                entry = entry
                    .on_hover(move |state: &mut State| {
                        state.settings.workspace.permission_highlighted = Some(mode)
                    })
                    .on_press_tx(move |state, tx| choose(state, Some(mode), tx));
            }
            entry
        }));
    Some([outside, options])
}

fn open(state: &mut State, tx: &mut Tx) {
    let Some(current) = selection(state).filter(|current| current.writable) else {
        return;
    };
    if state.settings.workspace.pending.is_some() {
        return;
    }
    let mode = current.mode();
    state.settings.workspace.highlighted = None;
    state.settings.workspace.permission_typeahead.clear();
    state.settings.workspace.permission_highlighted = Some(mode);
    tx.focus_when_shown(LIST, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.workspace.permission_highlighted = None;
    tx.return_focus(TRIGGER, tx.focus_visible());
}

fn choose(state: &mut State, mode: Option<RuntimeMode>, tx: &mut Tx) {
    close(state, tx);
    let panel = &state.settings.workspace;
    let Some(current) = selection(state).filter(|current| current.writable) else {
        return;
    };
    if panel.pending.is_some()
        || mode
            .is_some_and(|mode| mode == current.mode() || current.settings.denial(mode).is_some())
    {
        return;
    }
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_default_permissions(panel.scope.id.clone(), mode));
    start_write(&mut state.settings.workspace, operation);
}

pub(super) fn writable(state: &State) -> bool {
    selection(state).is_some_and(|current| current.writable)
}

fn trigger_input(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if matches!(event.key, Key::Up | Key::Down) {
        open(state, tx);
        return true;
    }
    if event.key == Key::Tab {
        state.settings.workspace.permission_typeahead.clear();
    }
    typeahead(state, event, tx)
}

fn typeahead(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(current) = selection(state) else {
        return false;
    };
    let options = MODES.map(|mode| {
        current
            .settings
            .denial(mode)
            .is_none()
            .then(|| PermissionSetting::label(mode))
    });
    let index = MODES
        .iter()
        .position(|&mode| current.mode() == mode)
        .unwrap_or_default();
    let panel = &mut state.settings.workspace;
    if matches!(&event.key, Key::Text(value) if value == " ")
        && !panel.permission_typeahead.typing()
    {
        return false;
    }
    match panel.permission_typeahead.input(event, index, &options) {
        keyboard::Input::Other => false,
        keyboard::Input::Typed(mode) => {
            if let Some(index) = mode {
                if panel.permission_highlighted.is_some() {
                    panel.permission_highlighted = Some(MODES[index]);
                } else {
                    choose(state, Some(MODES[index]), tx);
                }
            }
            true
        }
    }
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(active) = state.settings.workspace.permission_highlighted else {
        return false;
    };
    if typeahead(state, event, tx) {
        return true;
    }
    match event.key {
        Key::Escape => close(state, tx),
        Key::Tab => {
            let panel = &state.settings.workspace;
            let target = if event.mods.shift {
                if selection(state)
                    .is_some_and(|current| current.may_reset(panel.project_selected()))
                {
                    "reset-default-permissions"
                } else {
                    "threads-project-scope"
                }
            } else if !panel.writable(state) {
                "close-client-settings"
            } else if panel.configured().is_some() {
                "reset-default-workspace"
            } else {
                "default-workspace"
            };
            state.settings.workspace.permission_highlighted = None;
            state.settings.workspace.permission_typeahead.clear();
            tx.focus(target, true);
        }
        Key::Enter => choose(state, Some(active), tx),
        Key::Text(ref input) if input == " " => choose(state, Some(active), tx),
        _ => {
            let Some(current) = selection(state) else {
                return false;
            };
            let allowed: Vec<_> = MODES
                .into_iter()
                .filter(|&mode| current.settings.denial(mode).is_none())
                .collect();
            let index = allowed
                .iter()
                .position(|&mode| mode == active)
                .unwrap_or_default();
            let Some(next) = keyboard::arrow_index(&event.key, index, allowed.len()) else {
                return false;
            };
            state.settings.workspace.permission_highlighted = Some(allowed[next]);
        }
    }
    true
}
