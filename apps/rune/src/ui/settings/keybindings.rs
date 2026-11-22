use rune::{
    Color, El, Icon, Key, KeyEvent, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text,
    text_input,
};
use uuid::Uuid;

use crate::connection::{KeybindingEdit, KeybindingRule};

use super::super::{Palette, State, error_text, sidebar_header::activate_on_return};
use super::{CONTROL, DESCRIPTION, SECTION, keybinding_catalog as catalog};

mod command_picker;
mod when_editor;

pub(super) use when_editor::overlays as when_overlays;

const SEARCH: &str = "search-keybindings";
const ADD: &str = "add-keybinding";
const CAPTURE: &str = "record-keybinding";
const SAVE: &str = "save-keybinding";
const LABEL: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const RECORDER: TextStyle = TextStyle::new(14.0, 400.0).mono().lh(20.0);
const MONO: TextStyle = TextStyle::new(11.0, 400.0).mono().lh(16.0);
const CAPTION: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);

static MAGNIFIER: Icon = glyph!(
    "search",
    r#"<circle cx="11" cy="11" r="7"/><path d="m16 16 5 5"/>"#
);
static PLUS: Icon = glyph!("plus", r#"<path d="M12 5v14M5 12h14"/>"#);
static ELLIPSIS: Icon = glyph!(
    "ellipsis",
    r#"<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>"#
);

#[derive(Default)]
pub(super) struct Panel {
    query: TextEdit,
    draft: Option<Draft>,
    picker: Option<command_picker::Picker>,
    actions: Option<KeybindingRule>,
    pending: Option<Uuid>,
    status: CatalogStatus,
    error: Option<String>,
}

impl Panel {
    pub(super) fn clear_view(&mut self) {
        self.query = TextEdit::default();
        self.draft = None;
        self.picker = None;
        self.actions = None;
        self.error = None;
    }
}

#[derive(Default, PartialEq, Eq)]
enum CatalogStatus {
    #[default]
    Loading,
    Available,
    Unavailable,
}

struct Draft {
    original: Option<KeybindingRule>,
    command: Option<&'static str>,
    recorded: Option<String>,
    recording: bool,
    when: Option<when_editor::Editor>,
}

impl Draft {
    fn command(&self) -> Option<&str> {
        self.original
            .as_ref()
            .map(|rule| rule.command.as_str())
            .or(self.command)
    }

    fn key(&self) -> Option<&str> {
        self.recorded
            .as_deref()
            .or_else(|| self.original.as_ref().map(|rule| rule.key.as_str()))
    }

    fn condition(&self) -> Option<String> {
        if let Some(when) = &self.when {
            return when.expression();
        }
        self.original.as_ref().and_then(|original| {
            if self.recorded.is_some()
                && original.when.as_deref() == Some("false")
                && let Some(default) =
                    catalog::stock_rule(original).filter(|rule| rule.key == original.key)
            {
                return default.when.clone();
            }
            original.when.clone()
        })
    }

    fn changed(&self) -> bool {
        self.original.as_ref().map_or_else(
            || self.recorded.is_some(),
            |original| {
                self.key().is_some_and(|key| key != original.key)
                    || self.condition() != original.when
            },
        )
    }

    fn needs_editor(&self) -> bool {
        self.original.is_none()
            || self.recording
            || self.changed()
            || self.when.as_ref().is_some_and(|when| !when.valid())
    }

    fn can_save(&self) -> bool {
        !self.recording
            && self.command().is_some()
            && self.key().is_some()
            && self.when.as_ref().is_none_or(when_editor::Editor::valid)
            && self.changed()
    }
}

impl State {
    pub(in crate::ui) fn keybindings_catalog_available(&mut self, available: bool) {
        self.settings.keybindings.status = if available {
            CatalogStatus::Available
        } else {
            CatalogStatus::Unavailable
        };
    }

    pub(in crate::ui) fn reset_keybindings_panel(&mut self) {
        self.settings.keybindings = Panel::default();
    }

    pub(in crate::ui) fn settle_keybinding(
        &mut self,
        operation: Uuid,
        result: Result<(), String>,
        tx: &mut Tx,
    ) {
        let panel = &mut self.settings.keybindings;
        if panel.pending != Some(operation) {
            return;
        }
        panel.pending = None;
        match result {
            Ok(()) => {
                panel.draft = None;
                panel.picker = None;
                panel.actions = None;
                panel.error = None;
                if self.settings.page == super::Page::Keybindings && self.settings.is_open() {
                    tx.focus_when_shown(SEARCH, tx.focus_visible());
                }
            }
            Err(message) => panel.error = Some(message),
        }
    }

    fn can_edit_keybindings(&self) -> bool {
        self.settings.keybindings.status == CatalogStatus::Available
            && self
                .keybindings
                .as_ref()
                .is_some_and(|bindings| bindings.writable)
            && self.settings.keybindings.pending.is_none()
    }

    fn add_keybinding(&mut self, tx: &mut Tx) {
        if !self.can_edit_keybindings() {
            return;
        }
        let panel = &mut self.settings.keybindings;
        panel.actions = None;
        panel.error = None;
        panel.draft = Some(Draft {
            original: None,
            command: None,
            recorded: None,
            recording: false,
            when: None,
        });
        command_picker::open(self, tx);
    }

    fn start_keybinding_recording(&mut self, original: KeybindingRule, tx: &mut Tx) {
        if !self.can_edit_keybindings() {
            return;
        }
        let panel = &mut self.settings.keybindings;
        panel.actions = None;
        panel.error = None;
        if let Some(draft) = panel
            .draft
            .as_mut()
            .filter(|draft| draft.original.as_ref() == Some(&original))
        {
            draft.recording = true;
        } else {
            panel.draft = Some(Draft {
                original: Some(original),
                command: None,
                recorded: None,
                recording: true,
                when: None,
            });
        }
        tx.focus_when_shown(CAPTURE, true);
    }

    fn cancel_keybinding_recording(&mut self, tx: &mut Tx) {
        if let Some(draft) = self.settings.keybindings.draft.take() {
            let key = draft
                .original
                .as_ref()
                .map_or_else(|| ADD.to_owned(), row_key);
            tx.focus_when_shown(key, tx.focus_visible());
        }
        self.settings.keybindings.picker = None;
        self.settings.keybindings.error = None;
    }

    fn handle_keybinding_recording(&mut self, event: &KeyEvent, tx: &mut Tx) -> bool {
        let Some(draft) = self.settings.keybindings.draft.as_mut() else {
            return false;
        };
        if event.key == Key::Escape {
            if draft.original.is_none() && draft.recording {
                draft.recording = false;
                draft.recorded = None;
                tx.focus_when_shown(command_picker::TRIGGER, true);
            } else {
                self.cancel_keybinding_recording(tx);
            }
            return true;
        }
        if event.key == Key::Tab && !draft.recording {
            return false;
        }
        if !tx.key_repeat()
            && let Some(shortcut) = recorded_key(event, tx.key_base())
        {
            draft.recorded = Some(shortcut);
            draft.recording = false;
            if draft.can_save() {
                tx.focus_when_shown(SAVE, true);
            }
        }
        true
    }

    fn save_keybinding_recording(&mut self, _: &mut Tx) {
        let Some(draft) = self
            .settings
            .keybindings
            .draft
            .as_ref()
            .filter(|draft| draft.can_save())
        else {
            return;
        };
        let (Some(command), Some(key)) = (draft.command(), draft.key()) else {
            return;
        };
        let rule = KeybindingRule {
            command: command.to_owned(),
            key: key.to_owned(),
            when: draft.condition(),
        };
        self.send_keybinding_edit(KeybindingEdit::Upsert {
            rule,
            replace: draft.original.clone(),
        });
    }

    fn send_keybinding_edit(&mut self, edit: KeybindingEdit) {
        if !self.can_edit_keybindings() {
            return;
        }
        let operation = self
            .connection
            .as_ref()
            .and_then(|connection| connection.edit_keybinding(edit));
        let panel = &mut self.settings.keybindings;
        if let Some(operation) = operation {
            panel.pending = Some(operation);
            panel.error = None;
        } else {
            panel.error = Some("The environment connection ended.".into());
        }
    }
}

fn recorded_key(event: &KeyEvent, base: Option<char>) -> Option<String> {
    let key = match &event.key {
        Key::Text(value) if value.chars().count() == 1 => {
            let character = base.or_else(|| value.chars().next())?;
            if character == ' ' {
                "space".to_owned()
            } else {
                character.to_lowercase().to_string()
            }
        }
        Key::Command(character) => base.unwrap_or(*character).to_lowercase().to_string(),
        Key::Enter => "enter".into(),
        Key::Tab => "tab".into(),
        Key::Backspace => "backspace".into(),
        Key::Delete => "delete".into(),
        Key::Up => "arrowup".into(),
        Key::Down => "arrowdown".into(),
        Key::Left => "arrowleft".into(),
        Key::Right => "arrowright".into(),
        Key::Home => "home".into(),
        Key::End => "end".into(),
        Key::PageUp => "pageup".into(),
        Key::PageDown => "pagedown".into(),
        Key::Function(number) => format!("f{number}"),
        _ => return None,
    };
    let mac = cfg!(target_os = "macos");
    let flags = [
        (
            event.mods.primary() || (mac && event.mods.logo) || (!mac && event.mods.ctrl),
            "mod+",
        ),
        (mac && event.mods.ctrl, "ctrl+"),
        (!mac && event.mods.logo, "meta+"),
        (event.mods.alt, "alt+"),
        (event.mods.shift, "shift+"),
    ];
    let mut shortcut = String::new();
    for (held, modifier) in flags {
        if held {
            shortcut.push_str(modifier);
        }
    }
    shortcut.push_str(&key);
    Some(shortcut)
}

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let panel = &state.settings.keybindings;
    let enabled = state.can_edit_keybindings();
    let compact = width < 600.0;
    let mut groups = col().w_full().gap(32.0);
    if let Some(bindings) = state
        .keybindings
        .as_ref()
        .filter(|_| panel.status == CatalogStatus::Available)
    {
        let mut shown = false;
        for ((group, _), entries) in catalog::GROUPS
            .iter()
            .zip(catalog::groups(bindings, &panel.query.text))
        {
            if entries.is_empty() {
                continue;
            }
            shown = true;
            let mut card = card(palette);
            for (index, entry) in entries.iter().enumerate() {
                card = card.child(binding_row(
                    entry,
                    state,
                    palette,
                    enabled,
                    compact,
                    index > 0,
                ));
            }
            groups = groups.child(section(group, palette, card));
        }
        if !shown {
            groups = groups.child(notice("No keybindings match your search.", palette));
        }
    } else {
        groups = groups.child(notice(
            if panel.status == CatalogStatus::Unavailable {
                "The environment’s keybindings are unavailable."
            } else {
                "Loading keybindings…"
            },
            palette,
        ));
    }
    let mut controls = section(
        "Keybindings",
        palette,
        toolbar(state, palette, enabled, compact),
    );
    if let Some(error) = &panel.error {
        controls = controls.child(error_text(error, palette).px(16.0));
    } else if panel.status == CatalogStatus::Available
        && state
            .keybindings
            .as_ref()
            .is_some_and(|bindings| !bindings.writable)
    {
        controls = controls.child(
            text("This connection can view keybindings but cannot change them.")
                .font(DESCRIPTION)
                .color(palette.muted)
                .px(16.0),
        );
    }
    let mut body = col()
        .w_full()
        .max_w(800.0)
        .gap(32.0)
        .child(scope_description(palette))
        .child(controls);
    if let Some(draft) = panel
        .draft
        .as_ref()
        .filter(|draft| draft.original.is_none())
    {
        let description = col()
            .min_w(0.0)
            .grow(1.0)
            .gap(4.0)
            .child(text("New keybinding").font(CONTROL).color(palette.ink))
            .child(when_editor::control(
                None,
                draft.condition().as_deref(),
                draft
                    .when
                    .as_ref()
                    .is_some_and(when_editor::Editor::is_open),
                "new keybinding",
                panel.pending.is_none(),
                palette,
            ));
        let command = command_picker::trigger(state, palette, compact);
        let capture = editor(draft, panel.pending.is_some(), palette, compact);
        let content = if compact {
            col()
                .gap(8.0)
                .child(description)
                .child(command)
                .child(capture)
        } else {
            row()
                .min_h(48.0)
                .gap(8.0)
                .items_center()
                .child(description)
                .child(command)
                .child(capture)
        };
        body = body.child(card(palette).child(content.px(16.0).py(12.0)));
    }
    let scroll = col()
        .key("keybindings-settings-scroll")
        .w_full()
        .min_h(0.0)
        .grow(1.0)
        .items_center()
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .padding(if compact { 16.0 } else { 30.0 })
        .pt(24.0)
        .child(body.child(groups));
    let mut content = col().w_full().min_h(0.0).grow(1.0).child(scroll);
    if let Some([dismiss, popup]) = command_picker::overlay(state, palette, width) {
        content = content.child(dismiss).child(popup);
    }
    content
}

fn scope_description(palette: Palette) -> El {
    row()
        .h(28.0)
        .px(16.0)
        .gap(7.0)
        .items_center()
        .child(
            text("Applying settings for")
                .font(LABEL)
                .color(palette.muted),
        )
        .child(scope_value("All projects", palette))
        .child(text("across").font(LABEL).color(palette.muted))
        .child(scope_value("All environments", palette))
}

fn scope_value(label: &'static str, palette: Palette) -> El {
    row()
        .h(28.0)
        .px(10.0)
        .items_center()
        .radius(8.0)
        .border(1.0, palette.line)
        .child(text(label).font(CONTROL).color(palette.ink))
}

fn section(title: &str, palette: Palette, content: El) -> El {
    col()
        .gap(12.0)
        .child(
            text(title)
                .role("heading")
                .font(SECTION)
                .color(palette.ink)
                .px(16.0),
        )
        .child(content)
}

fn card(palette: Palette) -> El {
    col()
        .w_full()
        .fill(Color::WHITE.alpha(if palette.dark { 0.027 } else { 0.3 }))
        .radius(12.0)
        .border(1.0, palette.line)
        .border_layout()
}

fn notice(message: &str, palette: Palette) -> El {
    card(palette).items_center().child(
        text(message)
            .font(LABEL)
            .color(palette.muted)
            .px(16.0)
            .py(48.0),
    )
}

fn toolbar(state: &State, palette: Palette, enabled: bool, compact: bool) -> El {
    let panel = &state.settings.keybindings;
    let field = row()
        .min_w(0.0)
        .grow(1.0)
        .h(32.0)
        .items_center()
        .gap(5.0)
        .px(9.0)
        .fill(input_fill(palette))
        .border(
            1.0,
            if panel.query.focused {
                palette.accent
            } else {
                palette.line
            },
        )
        .radius(8.0)
        .child(icon(&MAGNIFIER, 13.0).color(palette.muted).a11y_hidden())
        .child(
            text_input(&panel.query, LABEL)
                .key(SEARCH)
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .color(palette.ink)
                .placeholder("Search keybindings", palette.muted)
                .label("Search keybindings")
                .single_line()
                .on_edit(|state: &mut State| &mut state.settings.keybindings.query)
                .on_input_change(|state: &mut State, _| {
                    let panel = &mut state.settings.keybindings;
                    if panel.pending.is_none() {
                        if panel
                            .draft
                            .as_ref()
                            .is_some_and(|draft| draft.original.is_some())
                        {
                            panel.draft = None;
                        }
                        panel.actions = None;
                    }
                })
                .on_input_key(|state: &mut State, event, tx| {
                    if event.key == Key::Escape {
                        let panel = &mut state.settings.keybindings;
                        if panel.query.text.is_empty() {
                            tx.focus(super::DIALOG, false);
                        } else {
                            panel.query.replace_text("");
                        }
                        return true;
                    }
                    false
                }),
        );
    let add = action(
        button_surface(palette)
            .child(icon(&PLUS, 14.0).color(palette.muted).a11y_hidden())
            .child(text("Add keybinding").font(CONTROL).color(if enabled {
                palette.ink
            } else {
                palette.muted
            })),
        ADD,
        "Add keybinding",
        palette,
        enabled && panel.draft.is_none(),
        |state, tx| state.add_keybinding(tx),
    );
    let controls = if compact {
        col().gap(8.0).child(field).child(add)
    } else {
        row().gap(8.0).items_center().child(field).child(add)
    };
    card(palette).child(controls.px(16.0).py(12.0))
}

fn binding_row(
    entry: &catalog::Row<'_>,
    state: &State,
    palette: Palette,
    enabled: bool,
    compact: bool,
    divider: bool,
) -> El {
    let panel = &state.settings.keybindings;
    let rule = &entry.binding.rule;
    let editable = catalog::EDITABLE_COMMANDS.contains(&rule.command.as_str());
    let editing = panel
        .draft
        .as_ref()
        .filter(|draft| draft.original.as_ref() == Some(rule));
    let menu_open = panel.actions.as_ref() == Some(rule);
    let label = row()
        .gap(6.0)
        .items_center()
        .child(text(&entry.label).font(CONTROL).color(palette.ink).wrap());
    let label = if entry.source != catalog::Source::Default {
        label.child(
            text(entry.source.label())
                .font(CAPTION)
                .color(palette.muted),
        )
    } else {
        label
    };
    let condition = if entry.unassigned() && editing.is_none() {
        text("No keyboard shortcut assigned. Click Unassigned to set one.")
            .font(CAPTION)
            .color(palette.muted)
            .wrap()
    } else {
        let condition = editing.map_or_else(|| rule.when.clone(), Draft::condition);
        when_editor::control(
            Some(rule),
            condition.as_deref(),
            editing
                .and_then(|draft| draft.when.as_ref())
                .is_some_and(when_editor::Editor::is_open),
            &entry.label,
            editable && enabled,
            palette,
        )
    };
    let info = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(4.0)
        .child(label)
        .child(condition);
    let pills = if entry.unassigned() {
        row().child(text("Unassigned").font(LABEL).color(palette.muted))
    } else {
        entry
            .binding
            .display_keys
            .iter()
            .fold(row().gap(4.0), |pills, key| {
                pills.child(
                    text(key)
                        .font(CAPTION)
                        .color(palette.muted)
                        .px(4.0)
                        .py(2.0)
                        .radius(4.0)
                        .fill(palette.ink.alpha(0.07)),
                )
            })
    };
    let controls = if let Some(draft) = editing.filter(|draft| draft.needs_editor()) {
        editor(draft, panel.pending.is_some(), palette, compact)
    } else if editable && enabled {
        let edit = rule.clone();
        let label = format!(
            "Edit shortcut for {}: {}",
            entry.label,
            if entry.unassigned() {
                "Unassigned".to_owned()
            } else {
                entry.binding.display_keys.join("+")
            }
        );
        let shortcut = action(
            pills.h(28.0).px(4.0).items_center(),
            row_key(rule),
            label,
            palette,
            true,
            move |state, tx| state.start_keybinding_recording(edit.clone(), tx),
        );
        let menu = rule.clone();
        let actions = action(
            el().size(26.0, 26.0)
                .center()
                .child(icon(&ELLIPSIS, 14.0).color(palette.muted).a11y_hidden()),
            action_key(rule),
            format!("Actions for {}", entry.label),
            palette,
            true,
            move |state, tx| {
                let panel = &mut state.settings.keybindings;
                let next = panel.actions.as_ref() != Some(&menu);
                panel.actions = next.then(|| menu.clone());
                if next {
                    tx.focus_when_shown("keybinding-first-action", tx.focus_visible());
                }
            },
        );
        row().items_center().gap(3.0).child(actions).child(shortcut)
    } else {
        pills
    };
    let body = if editing.is_some() && compact {
        col().gap(8.0).child(info).child(controls)
    } else {
        row().items_center().gap(8.0).child(info).child(controls)
    };
    let mut node = col()
        .key(search_row_key(rule))
        .focusable(true)
        .tab_index(-1)
        .no_focus_ring()
        .role("listitem")
        .label(&entry.label)
        .min_h(72.0)
        .w_full()
        .px(16.0)
        .py(12.0)
        .justify_center()
        .child(body);
    if divider {
        node = node.border_sides([1.0, 0.0, 0.0, 0.0], palette.line);
    }
    if menu_open {
        node = node.child(row_actions(entry, palette, enabled));
    }
    node
}

fn row_actions(entry: &catalog::Row<'_>, palette: Palette, enabled: bool) -> El {
    let rule = &entry.binding.rule;
    let return_focus = action_key(rule);
    let mut actions =
        row()
            .gap(8.0)
            .pt(8.0)
            .justify_end()
            .on_key(move |state: &mut State, event, tx| {
                if event.key != Key::Escape {
                    return false;
                }
                state.settings.keybindings.actions = None;
                tx.focus_when_shown(return_focus.clone(), tx.focus_visible());
                true
            });
    if entry.source == catalog::Source::Default {
        let original = rule.clone();
        return actions.child(button(
            "keybinding-first-action",
            "Unassign",
            palette,
            false,
            enabled,
            move |state, _| {
                let rule = KeybindingRule {
                    when: Some("false".into()),
                    ..original.clone()
                };
                state.send_keybinding_edit(KeybindingEdit::Upsert {
                    rule,
                    replace: Some(original.clone()),
                });
            },
        ));
    }
    if let Some(default) = catalog::stock_rule(rule) {
        let original = rule.clone();
        actions = actions.child(button(
            "keybinding-first-action",
            "Reset to default",
            palette,
            false,
            enabled,
            move |state, _| {
                state.send_keybinding_edit(KeybindingEdit::Upsert {
                    rule: default.clone(),
                    replace: Some(original.clone()),
                })
            },
        ));
    }
    if !entry.unassigned() {
        let original = rule.clone();
        actions = actions.child(button(
            "remove-keybinding",
            "Remove",
            palette,
            false,
            enabled,
            move |state, _| state.send_keybinding_edit(KeybindingEdit::Remove(original.clone())),
        ));
    }
    actions
}

fn editor(draft: &Draft, pending: bool, palette: Palette, compact: bool) -> El {
    let value = if draft.recording {
        "Press shortcut"
    } else {
        draft.key().unwrap_or("Unassigned")
    };
    let field = el()
        .key(CAPTURE)
        .role("textbox")
        .label(draft.command().map_or_else(
            || "Keybinding for new keybinding".into(),
            |command| format!("Keybinding for {}", catalog::command_label(command)),
        ))
        .focusable(!pending)
        .focus_ring(1.0, 1.0, palette.accent)
        .h(28.0)
        .w(if compact { 132.0 } else { 176.0 })
        .px(9.0)
        .items_center()
        .radius(8.0)
        .fill(input_fill(palette))
        .border(1.0, palette.line)
        .on_press_tx(|state: &mut State, tx| {
            if let Some(draft) = state.settings.keybindings.draft.as_mut() {
                draft.recording = true;
                tx.focus(CAPTURE, tx.focus_visible());
            }
        })
        .on_key(State::handle_keybinding_recording)
        .child(text(value).font(RECORDER).color(if draft.recording {
            palette.muted
        } else {
            palette.ink
        }));
    let add = draft.original.is_none();
    let save = || {
        button(
            SAVE,
            if pending { "Saving" } else { "Save" },
            palette,
            true,
            !pending && draft.can_save(),
            State::save_keybinding_recording,
        )
    };
    let mut controls = row().gap(8.0).items_center();
    if !add && (draft.changed() || draft.when.as_ref().is_some_and(|when| !when.valid())) {
        controls = controls.child(save());
    }
    controls = controls.child(field);
    if add {
        controls = controls.child(save()).child(action(
            el().size(26.0, 26.0)
                .center()
                .child(icon(&super::CLOSE, 14.0).color(palette.muted).a11y_hidden()),
            "cancel-keybinding",
            "Cancel",
            palette,
            !pending,
            State::cancel_keybinding_recording,
        ));
    }
    controls.on_key(|state: &mut State, event, tx| {
        if event.key != Key::Escape || state.settings.keybindings.pending.is_some() {
            return false;
        }
        state.cancel_keybinding_recording(tx);
        true
    })
}

fn action_key(rule: &KeybindingRule) -> String {
    format!("actions-{}", row_key(rule))
}

pub(super) fn search_row_key(rule: &KeybindingRule) -> String {
    format!("keybinding-row-{}", row_key(rule))
}

fn row_key(rule: &KeybindingRule) -> String {
    format!(
        "edit-keybinding-{}-{}-{}",
        rule.command,
        rule.key,
        rule.when.as_deref().unwrap_or_default()
    )
}

fn input_fill(palette: Palette) -> Color {
    if palette.dark {
        Color::WHITE.alpha(0.04)
    } else {
        palette.canvas
    }
}

fn button_surface(palette: Palette) -> El {
    row()
        .h(32.0)
        .px(11.0)
        .gap(7.0)
        .items_center()
        .border(1.0, palette.line)
        .fill(palette.canvas)
}

fn button(
    key: &'static str,
    label: &'static str,
    palette: Palette,
    primary: bool,
    enabled: bool,
    press: impl Fn(&mut State, &mut Tx) + Clone + 'static,
) -> El {
    let ink = if primary {
        palette.on_accent
    } else {
        palette.ink
    };
    action(
        button_surface(palette)
            .fill(if primary {
                palette.accent.alpha(if enabled { 1.0 } else { 0.54 })
            } else {
                palette.canvas
            })
            .child(
                text(label)
                    .font(CONTROL)
                    .color(if enabled { ink } else { palette.muted }),
            ),
        key,
        label,
        palette,
        enabled,
        press,
    )
}

fn action(
    element: El,
    key: impl Into<String>,
    label: impl Into<String>,
    palette: Palette,
    enabled: bool,
    press: impl Fn(&mut State, &mut Tx) + Clone + 'static,
) -> El {
    let element = element.key(key).label(label).role("button").radius(7.0);
    if !enabled {
        return element.disabled(true);
    }
    let keyboard = press.clone();
    element
        .focusable(true)
        .focus_ring(1.0, 1.0, palette.accent)
        .hover(palette.hover)
        .on_press_tx(press)
        .on_key(move |state, event, tx| activate_on_return(state, event, tx, &keyboard))
}
