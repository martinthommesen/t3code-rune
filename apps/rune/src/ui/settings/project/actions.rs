use rune::{
    Color, El, Icon, Key, KeyEvent, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text,
    text_input,
};
use uuid::Uuid;

use crate::connection::{Project, ProjectAction, ProjectActionIcon, ProjectActions};
use crate::ui::{button, sidebar_header};

use super::super::{CONTROL, DESCRIPTION, SECTION, switch_track, workspace};
use super::{
    GEAR, PROJECT_FONT, Palette, RESET, State, error_text, find_project, icon_button, separator,
    setting_row,
};

const ADD: &str = "project-action-add";
const DIALOG: &str = "project-action-editor";
const NAME: &str = "project-action-name";
const COMMAND: &str = "project-action-command";
const RUN: &str = "project-action-on-worktree";
const WAIT: &str = "project-action-wait";
const RUN_LABEL: &str = "Run automatically on worktree creation";
const WAIT_LABEL: &str = "Wait for it to finish before the agent starts";
const MONO: TextStyle = TextStyle::new(12.0, 400.0).mono().lh(18.0);
const TITLE: TextStyle = TextStyle::new(18.0, 600.0).lh(24.0);

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(
                r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">"#,
                $path,
                "</g>"
            ),
        }
    };
}

static PLUS: Icon = glyph!("project-action-plus", r#"<path d="M12 5v14M5 12h14"/>"#);
static PLAY: Icon = glyph!(
    "project-action-play",
    r#"<path d="M6 4.8a1 1 0 0 1 1.5-.9l12 7.2a1 1 0 0 1 0 1.8l-12 7.2a1 1 0 0 1-1.5-.9z"/>"#
);
static TEST: Icon = glyph!(
    "project-action-test",
    r#"<path d="M9 3h6m-5 0v6l-5 8a2 2 0 0 0 1.7 3h10.6a2 2 0 0 0 1.7-3l-5-8V3M8 14h8"/>"#
);
static LINT: Icon = glyph!(
    "project-action-lint",
    r#"<rect x="4" y="4" width="16" height="16" rx="2"/><path d="m8 12 3 3 5-6"/>"#
);
static BUILD: Icon = glyph!(
    "project-action-build",
    r#"<path d="M4 7l8-4 8 4v10l-8 4-8-4zM4 7l8 4 8-4m-8 4v10M8 5l8 4"/>"#
);
static DEBUG: Icon = glyph!(
    "project-action-debug",
    r#"<rect x="7" y="8" width="10" height="12" rx="5"/><path d="M12 8V5m-3 1L7 3m8 3 2-3M4 9l3 2m13-2-3 2M3 14h4m10 0h4M4 19l3-2m13 2-3-2"/>"#
);

#[derive(Default)]
pub(super) struct Panel {
    editor: Option<Editor>,
    pending: Option<Uuid>,
    error: Option<String>,
}

struct Editor {
    id: Option<String>,
    name: TextEdit,
    command: TextEdit,
    run_on_worktree_create: bool,
    wait_for_setup: bool,
    error: Option<String>,
}

impl Editor {
    fn new(script: Option<&ProjectAction>) -> Self {
        let mut name = TextEdit::default();
        let mut command = TextEdit::default();
        if let Some(script) = script {
            name.replace_text(&script.name);
            command.replace_text(&script.command);
        }
        Self {
            id: script.map(|script| script.id.clone()),
            name,
            command,
            run_on_worktree_create: script.is_some_and(|script| script.run_on_worktree_create),
            wait_for_setup: script.is_some_and(|script| {
                script.run_on_worktree_create && script.asynchronous == Some(false)
            }),
            error: None,
        }
    }

    fn focus_key(&self) -> String {
        self.id.as_deref().map_or_else(|| ADD.to_owned(), edit_key)
    }
}

fn selected(state: &State) -> Option<(&ProjectActions, &Project)> {
    let workspace = state.workspace.as_ref()?;
    let form = state.settings.project.as_ref()?;
    Some((
        &state.settings.workspace.current.as_ref()?.actions,
        find_project(workspace, &form.project_id)?,
    ))
}

fn writable(state: &State) -> bool {
    state
        .workspace
        .as_ref()
        .is_some_and(|workspace| !workspace.reconnecting)
        && state
            .settings
            .project
            .as_ref()
            .is_some_and(|form| form.actions.pending.is_none())
        && selected(state).is_some_and(|(actions, _)| actions.writable)
}

pub(super) fn section(state: &State, project: &Project, palette: Palette, narrow: bool) -> El {
    let setting = selected(state).map(|(actions, _)| actions);
    let available = writable(state);
    let overridden = setting.is_some_and(|setting| setting.is_overridden(project));
    let mut add = row()
        .key(ADD)
        .role("button")
        .label("Add action")
        .h(26.0)
        .px(8.0)
        .items_center()
        .gap(5.0)
        .radius(6.0)
        .border(1.0, palette.line)
        .focus_ring(1.0, 2.0, palette.accent)
        .focusable(available)
        .disabled(!available)
        .opacity(if available { 1.0 } else { 0.60 })
        .child(icon(&PLUS, 14.0).color(palette.ink).a11y_hidden())
        .child(text("Add action").font(DESCRIPTION).color(palette.ink));
    if available {
        add = add
            .hover(palette.hover)
            .on_press_tx(|state, tx| open(state, tx, None))
            .on_key(|state, event, tx| {
                sidebar_header::activate_on_return(state, event, tx, |state, tx| {
                    open(state, tx, None)
                })
            });
    } else if let Some(by) = setting.and_then(|setting| setting.managed_by.as_deref()) {
        add = add.help(format!("Managed by {by}."));
    }
    let mut decorations = row().gap(6.0).items_center();
    if setting.is_some() {
        let source = if overridden {
            "Overridden for this project".to_owned()
        } else {
            state.workspace.as_ref().map_or_else(
                || "Inherited from environment".to_owned(),
                |workspace| format!("Inherited from {}", workspace.label),
            )
        };
        decorations = decorations.child(workspace::inheritance_marker(
            source,
            Palette {
                muted: if overridden {
                    palette.accent
                } else {
                    palette.muted
                },
                ..palette
            },
        ));
    }
    if available && overridden {
        decorations = decorations.child(
            icon_button(
                "reset-project-actions",
                "Reset Actions to default",
                &RESET,
                12.0,
                20.0,
                palette,
                reset,
            )
            .help("Reset to inherited value")
            .on_key(|state, event, tx| sidebar_header::activate_on_return(state, event, tx, reset)),
        );
    }
    let mut contents = col()
        .w_full()
        .radius(12.0)
        .border(1.0, palette.line)
        .border_layout()
        .child(setting_row(
            "Actions",
            "Commands that run in this project's checkout or its worktree, with optional shortcuts.",
            add,
            narrow,
            palette,
            Some(decorations),
        ));
    if let Some(setting) = setting {
        let entries = setting.effective(project);
        if entries.is_empty() {
            contents = contents.child(separator(palette)).child(
                text("No actions configured.")
                    .font(PROJECT_FONT)
                    .color(palette.muted)
                    .px(16.0)
                    .py(8.0),
            );
        } else {
            for entry in entries {
                contents = contents
                    .child(separator(palette))
                    .child(action_row(entry, palette, available));
            }
        }
    }
    let mut section = col()
        .key("project-actions")
        .w_full()
        .gap(10.0)
        .child(
            text("Actions")
                .font(SECTION)
                .color(palette.ink)
                .role("heading")
                .px(16.0),
        )
        .child(contents);
    if let Some(error) = state
        .settings
        .project
        .as_ref()
        .and_then(|form| form.actions.error.as_deref())
    {
        section = section.child(error_text(error, palette));
    }
    section
}

fn action_row(entry: &ProjectAction, palette: Palette, available: bool) -> El {
    let glyph = match entry.icon {
        ProjectActionIcon::Play => &PLAY,
        ProjectActionIcon::Test => &TEST,
        ProjectActionIcon::Lint => &LINT,
        ProjectActionIcon::Configure => &GEAR,
        ProjectActionIcon::Build => &BUILD,
        ProjectActionIcon::Debug => &DEBUG,
    };
    let mut heading = row()
        .gap(8.0)
        .items_center()
        .child(icon(glyph, 16.0).color(palette.muted).a11y_hidden())
        .child(
            text(&entry.name)
                .font(CONTROL)
                .color(palette.ink)
                .truncate(),
        );
    for (show, badge) in [
        (entry.run_on_worktree_create, "setup"),
        (entry.run_on_settle == Some(true), "on settle"),
        (entry.preview_url.is_some(), "preview · desktop only"),
    ] {
        if show {
            heading = heading.child(
                text(badge)
                    .font(TextStyle::new(10.0, 400.0).lh(14.0))
                    .color(palette.muted)
                    .px(6.0)
                    .py(1.0)
                    .radius(4.0)
                    .border(1.0, palette.line),
            );
        }
    }
    let identifier = entry.id.clone();
    let key = edit_key(&identifier);
    let label = format!("Edit {}", entry.name);
    let mut edit = el()
        .key(key)
        .role("button")
        .label(&label)
        .help(label)
        .size(26.0, 26.0)
        .center()
        .radius(5.0)
        .focus_ring(1.0, 2.0, palette.accent)
        .focusable(available)
        .disabled(!available)
        .group_opacity(0.0, 1.0)
        .child(icon(&GEAR, 14.0).color(palette.muted).a11y_hidden());
    if available {
        let pressed = identifier.clone();
        edit = edit
            .hover(palette.hover)
            .on_press_tx(move |state, tx| open(state, tx, Some(&pressed)))
            .on_key(move |state, event, tx| {
                sidebar_header::activate_on_return(state, event, tx, |state, tx| {
                    open(state, tx, Some(&identifier))
                })
            });
    }
    row()
        .w_full()
        .min_h(58.0)
        .px(16.0)
        .py(8.0)
        .items_center()
        .gap(8.0)
        .child(
            col().grow(1.0).min_w(0.0).gap(4.0).child(heading).child(
                text(&entry.command)
                    .font(MONO)
                    .color(palette.muted)
                    .truncate(),
            ),
        )
        .child(edit)
}

pub(in crate::ui::settings) fn overlay(
    state: &State,
    palette: Palette,
    width: f32,
    height: f32,
) -> Option<El> {
    let panel = &state.settings.project.as_ref()?.actions;
    let editor = panel.editor.as_ref()?;
    let title = if editor.id.is_some() {
        "Edit Action"
    } else {
        "Add Action"
    };
    let saving = panel.pending.is_some();
    let enabled = writable(state);
    let name = text_input(&editor.name, PROJECT_FONT)
        .key(NAME)
        .label("Name")
        .placeholder("Test", palette.muted)
        .color(palette.ink)
        .single_line()
        .w_full()
        .h(36.0)
        .px(10.0)
        .radius(7.0)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent)
        .disabled(!enabled)
        .on_edit(|state: &mut State| &mut active_editor(state).name)
        .on_submit(save);
    let command = text_input(&editor.command, PROJECT_FONT)
        .key(COMMAND)
        .label("Command")
        .placeholder("bun test", palette.muted)
        .color(palette.ink)
        .w_full()
        .h(84.0)
        .px(10.0)
        .py(7.0)
        .radius(7.0)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent)
        .disabled(!enabled)
        .on_edit(|state: &mut State| &mut active_editor(state).command)
        .on_submit(|state, _| active_editor(state).command.insert("\n"));
    let mut content = col()
        .w_full()
        .min_h(0.0)
        .px(24.0)
        .gap(16.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .gap(6.0)
                .child(text("Name").font(CONTROL).color(palette.ink))
                .child(name),
        )
        .child(
            col()
                .gap(6.0)
                .child(text("Command").font(CONTROL).color(palette.ink))
                .child(command),
        )
        .child(toggle_row(
            RUN,
            RUN_LABEL,
            editor.run_on_worktree_create,
            enabled,
            palette,
            toggle_run,
        ))
        .child(toggle_row(
            WAIT,
            WAIT_LABEL,
            editor.wait_for_setup,
            enabled && editor.run_on_worktree_create,
            palette,
            toggle_wait,
        ));
    if let Some(error) = editor.error.as_deref() {
        content = content.child(error_text(error, palette));
    }
    let save_label = if saving {
        "Saving…"
    } else if editor.id.is_some() {
        "Save changes"
    } else {
        "Save action"
    };
    let mut submit = button(
        "project-action-save",
        save_label,
        palette,
        true,
        enabled,
        save,
    );
    if enabled {
        submit = submit
            .on_key(|state, event, tx| sidebar_header::activate_on_return(state, event, tx, save));
    }
    let cancel = button(
        "project-action-cancel",
        "Cancel",
        palette,
        false,
        true,
        close,
    )
    .on_key(|state, event, tx| sidebar_header::activate_on_return(state, event, tx, close));
    let popup = col()
        .key(DIALOG)
        .hit()
        .role("dialog")
        .label(title)
        .w((width - 32.0).clamp(240.0, 512.0))
        .max_h((height - 32.0).max(236.0))
        .radius(12.0)
        .clip_rounded(12.0)
        .fill(palette.canvas)
        .border(1.0, palette.line)
        .border_layout()
        .on_key(|state: &mut State, event, tx| {
            if event.key == Key::Escape {
                close(state, tx);
                true
            } else {
                false
            }
        })
        .child(col().px(24.0).py(18.0).gap(5.0)
            .child(text(title).font(TITLE).color(palette.ink).role("heading"))
            .child(text("Actions are project-scoped commands you can run from the top bar or keybindings.")
                .font(DESCRIPTION).color(palette.muted).wrap()))
        .child(content)
        .child(row().w_full().px(24.0).py(20.0).gap(8.0).justify_end().child(cancel).child(submit));
    Some(
        el().key("project-action-overlay")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .center()
            .child(
                el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .fill(Color::BLACK.alpha(0.50))
                    .on_press_tx(close),
            )
            .child(popup),
    )
}

fn toggle_row(
    key: &str,
    label: &'static str,
    checked: bool,
    enabled: bool,
    palette: Palette,
    toggle: fn(&mut State, &mut Tx),
) -> El {
    let mut switch = switch_track(checked, false, palette, palette.accent)
        .key(key)
        .label(label)
        .focusable(enabled)
        .disabled(!enabled);
    if enabled {
        switch = switch
            .on_press_tx(toggle)
            .on_key(move |state, event: &KeyEvent, tx| {
                if matches!(&event.key, Key::Text(value) if value == " ") {
                    if !tx.key_repeat() {
                        toggle(state, tx);
                    }
                    true
                } else {
                    sidebar_header::activate_on_return(state, event, tx, toggle)
                }
            });
    }
    row()
        .w_full()
        .min_h(38.0)
        .px(12.0)
        .py(8.0)
        .gap(12.0)
        .items_center()
        .radius(6.0)
        .border(1.0, palette.line)
        .opacity(if enabled { 1.0 } else { 0.60 })
        .child(
            text(label)
                .font(PROJECT_FONT)
                .color(palette.ink)
                .grow(1.0)
                .wrap(),
        )
        .child(switch)
}

fn edit_key(id: &str) -> String {
    format!("edit-project-action-{id}")
}

fn active_editor(state: &mut State) -> &mut Editor {
    state
        .settings
        .project
        .as_mut()
        .and_then(|form| form.actions.editor.as_mut())
        .expect("project action editor is open")
}

fn open(state: &mut State, tx: &mut Tx, id: Option<&str>) {
    if !writable(state) {
        return;
    }
    let Some((actions, project)) = selected(state) else {
        return;
    };
    let script = id.and_then(|id| {
        actions
            .effective(project)
            .iter()
            .find(|script| script.id == id)
    });
    if id.is_some() && script.is_none() {
        return;
    }
    let editor = Editor::new(script);
    if let Some(form) = &mut state.settings.project {
        form.actions.error = None;
        form.actions.editor = Some(editor);
        tx.focus_when_shown(NAME, tx.focus_visible());
    }
}

fn toggle_run(state: &mut State, _: &mut Tx) {
    let editor = active_editor(state);
    editor.run_on_worktree_create = !editor.run_on_worktree_create;
}

fn toggle_wait(state: &mut State, _: &mut Tx) {
    let editor = active_editor(state);
    editor.wait_for_setup = !editor.wait_for_setup;
}

fn close(state: &mut State, tx: &mut Tx) {
    if let Some(form) = &mut state.settings.project
        && let Some(editor) = form.actions.editor.take()
    {
        tx.return_focus(editor.focus_key(), tx.focus_visible());
    }
}

pub(in crate::ui::settings) fn dismiss(state: &mut State) {
    if let Some(form) = &mut state.settings.project {
        form.actions.editor = None;
    }
}

fn field_text(text: &str) -> &str {
    text.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
}

fn save(state: &mut State, tx: &mut Tx) {
    if !writable(state) {
        return;
    }
    let editor = active_editor(state);
    let name = field_text(&editor.name.text).to_owned();
    let command = field_text(&editor.command.text).to_owned();
    if name.is_empty() || command.is_empty() {
        let (message, field) = if name.is_empty() {
            ("Name is required.", NAME)
        } else {
            ("Command is required.", COMMAND)
        };
        editor.error = Some(message.to_owned());
        tx.focus(field, true);
        return;
    }
    let existing = editor.id.clone();
    let run = editor.run_on_worktree_create;
    let waits = editor.wait_for_setup;
    let Some((actions, project)) = selected(state) else {
        return;
    };
    let mut values = actions.effective(project).to_vec();
    let position = existing
        .as_ref()
        .and_then(|id| values.iter().position(|action| &action.id == id));
    if existing.is_some() && position.is_none() {
        active_editor(state).error = Some("This action is no longer available.".into());
        return;
    }
    let mut updated = position.map_or_else(
        || ProjectAction {
            id: actions.next_id(
                &name,
                state
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.shell.as_ref())
                    .map_or(&[], |shell| shell.projects.as_slice()),
            ),
            ..ProjectAction::default()
        },
        |position| values[position].clone(),
    );
    updated.name = name;
    updated.command = command;
    updated.run_on_worktree_create = run;
    updated.asynchronous = (run && waits).then_some(false);
    if run {
        for action in &mut values {
            action.run_on_worktree_create = false;
        }
    }
    if let Some(position) = position {
        values[position] = updated;
    } else {
        values.push(updated);
    }
    write(state, Some(values));
}

fn reset(state: &mut State, tx: &mut Tx) {
    if writable(state) {
        write(state, None);
        tx.focus(ADD, tx.focus_visible());
    }
}

fn write(state: &mut State, values: Option<Vec<ProjectAction>>) {
    let Some(form) = &mut state.settings.project else {
        return;
    };
    let panel = &mut form.actions;
    panel.error = None;
    panel.pending = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_project_actions(form.project_id.clone(), values));
    let error = panel
        .pending
        .is_none()
        .then(|| "Could not save project actions.".into());
    if let Some(editor) = &mut panel.editor {
        editor.error = error;
    } else {
        panel.error = error;
    }
}

pub(in crate::ui::settings) fn settle(
    state: &mut State,
    operation: Uuid,
    result: &Result<(), String>,
    tx: &mut Tx,
) {
    let Some(form) = &mut state.settings.project else {
        return;
    };
    let panel = &mut form.actions;
    if panel.pending != Some(operation) {
        return;
    }
    panel.pending = None;
    match result {
        Ok(()) => close(state, tx),
        Err(error) => {
            if let Some(editor) = &mut panel.editor {
                editor.error = Some(error.clone());
            } else {
                panel.error = Some(error.clone());
            }
        }
    }
}
