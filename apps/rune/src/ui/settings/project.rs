use std::collections::HashSet;

use rune::{
    Color, El, Key, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text, text_input,
};

use crate::connection::{Project, ProjectEdit, ProjectEditKind, ProjectIcon};
use crate::ui::sidebar::project_badge;

use super::super::{Palette, State, Workspace, error_text};
use super::{CONTROL, DESCRIPTION, DIALOG, GEAR, Page, RESET, icon_button};

mod actions;
mod icon_files;
mod icon_picker;

pub(super) use actions::{
    dismiss as dismiss_actions, overlay as actions_overlay, settle as settle_actions,
};
pub(super) use icon_files::overlay as file_overlay;
pub(in crate::ui) use icon_files::{poll as poll_files, settle as settle_files};
pub(super) use icon_picker::overlay as icon_overlay;

const PROJECT_NAME: &str = "project-settings-name";
const PROJECT_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static INFORMATION: rune::Icon = rune::Icon {
    name: "project-settings-information",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><path d="M12 16v-4m0-4h.01"/></g>"#,
};

pub(super) struct Form {
    project_id: String,
    name: String,
    custom_name: bool,
    edit: TextEdit,
    dirty: bool,
    pending: HashSet<ProjectEditKind>,
    actions: actions::Panel,
    icon_picker: Option<icon_picker::Picker>,
    file_picker: Option<icon_files::Picker>,
}

impl Form {
    fn new(project: &Project) -> Self {
        let mut edit = TextEdit::default();
        edit.replace_text(&project.name);
        Self {
            project_id: project.id.clone(),
            name: project.name.clone(),
            custom_name: project.has_custom_title,
            edit,
            dirty: false,
            pending: HashSet::new(),
            actions: actions::Panel::default(),
            icon_picker: None,
            file_picker: None,
        }
    }

    fn sync_name(&mut self, name: &str, custom_name: bool) {
        self.name = name.to_owned();
        self.custom_name = custom_name;
        if !self.dirty && !self.edit.focused {
            self.edit.replace_text(name);
        }
    }
}

fn find_project<'a>(workspace: &'a Workspace, project_id: &str) -> Option<&'a Project> {
    workspace
        .shell
        .as_ref()?
        .projects
        .iter()
        .find(|project| project.id == project_id)
}

impl State {
    pub(in crate::ui) fn open_project_settings(&mut self, project_id: &str, tx: &mut Tx) {
        let Some(project) = self
            .workspace
            .as_ref()
            .and_then(|workspace| find_project(workspace, project_id))
        else {
            return;
        };
        self.settings.project = Some(Form::new(project));
        self.settings.page = Page::Projects;
        self.settings.error = None;
        self.settings.open = true;
        self.settings.return_focus = "project-filter";
        self.ensure_open_workspace_settings();
        tx.focus_when_shown(DIALOG, false);
    }

    fn edit_project_setting(&mut self, edit: ProjectEdit) {
        let kind = edit.kind();
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.project_writes && !workspace.reconnecting)
        else {
            return;
        };
        let (Some(form), Some(connection)) = (&mut self.settings.project, &self.connection) else {
            return;
        };
        if find_project(workspace, &form.project_id).is_none()
            || matches!(kind, ProjectEditKind::Pinned | ProjectEditKind::Archived)
                && !self.sidebar.supports_project_organization()
            || !form.pending.insert(kind)
        {
            return;
        }
        self.settings.error = None;
        connection.edit_project(form.project_id.clone(), edit);
    }

    pub(super) fn save_project_name(&mut self) -> bool {
        let Some(form) = self.settings.project.as_mut() else {
            return false;
        };
        if !std::mem::take(&mut form.dirty) {
            if form.edit.text != form.name && !form.pending.contains(&ProjectEditKind::Title) {
                form.edit.replace_text(&form.name);
                return true;
            }
            return false;
        }
        let title = form
            .edit
            .text
            .trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
            .to_owned();
        if title.is_empty() {
            form.edit.replace_text(&form.name);
            self.settings.error = Some("Project title cannot be empty".into());
        } else if title != form.name || !form.custom_name {
            form.edit.replace_text(&title);
            self.edit_project_setting(ProjectEdit::Title(title));
        } else {
            form.edit.replace_text(&form.name);
        }
        true
    }
}

pub(in crate::ui) fn poll(state: &mut State) -> bool {
    if !state.settings.open || state.settings.page != Page::Projects {
        return false;
    }
    let Some(form) = state.settings.project.as_ref() else {
        return false;
    };
    if !form.edit.focused && form.dirty {
        return state.save_project_name();
    }
    let Some(project) = state
        .workspace
        .as_ref()
        .and_then(|workspace| find_project(workspace, &form.project_id))
        .filter(|project| {
            !form.pending.contains(&ProjectEditKind::Title)
                && (project.name != form.name || project.has_custom_title != form.custom_name)
        })
    else {
        return false;
    };
    if let Some(form) = &mut state.settings.project {
        form.sync_name(&project.name, project.has_custom_title);
    }
    true
}

pub(in crate::ui) fn settle(
    state: &mut State,
    project_id: &str,
    kind: ProjectEditKind,
    result: Result<Box<Project>, String>,
) {
    let picker = kind == ProjectEditKind::Pinned && state.sidebar.settle_project_pin(project_id);
    let settings = state
        .settings
        .project
        .as_mut()
        .filter(|form| form.project_id == project_id)
        .and_then(|form| form.pending.remove(&kind).then_some(form));
    match result {
        Ok(updated) => {
            if let Some(form) = settings.filter(|_| kind == ProjectEditKind::Title) {
                form.sync_name(&updated.name, true);
            }
        }
        Err(message) => {
            if let Some(form) = settings {
                if kind == ProjectEditKind::Title {
                    form.edit.replace_text(&form.name);
                }
                state.settings.error = Some(message);
            } else if picker {
                state.project_notice = Some(message);
            } else if kind == ProjectEditKind::UnarchiveFromPicker {
                state.project_notice = Some(format!("Failed to unarchive project. {message}"));
            }
        }
    }
}

pub(in crate::ui) fn gear(
    project: &Project,
    ink: Color,
    open: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    let label = format!("Project settings for {}", project.name);
    el().key(format!("settings-project-{}", project.id))
        .role("button")
        .label(&label)
        .help(label)
        .size(24.0, 24.0)
        .radius(5.0)
        .center()
        .hover(ink.alpha(0.12))
        .on_press_tx(open)
        .child(icon(&GEAR, 14.0).color(ink).a11y_hidden())
}

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let mut view = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .padding(24.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    let Some(workspace) = state.workspace.as_ref() else {
        return view;
    };
    let Some(form) = state.settings.project.as_ref() else {
        return view.child(project_choices(workspace, palette));
    };
    let Some(project) = find_project(workspace, &form.project_id) else {
        return view.child(
            text("This project is no longer available.")
                .font(PROJECT_FONT)
                .color(palette.muted),
        );
    };
    let writable = workspace.project_writes && !workspace.reconnecting;
    let can_organize = state.sidebar.supports_project_organization();
    let narrow = width < 560.0;
    let scope = row()
        .w_full()
        .min_h(39.0)
        .gap(8.0)
        .items_center()
        .child(
            text("Applying settings for")
                .font(PROJECT_FONT)
                .color(palette.muted),
        )
        .child(project_picker_button(project, palette))
        .child(text("across").font(PROJECT_FONT).color(palette.muted))
        .child(text("All environments").font(CONTROL).color(palette.ink));
    let info = row()
        .w_full()
        .min_h(44.0)
        .gap(8.0)
        .px(12.0)
        .py(8.0)
        .items_center()
        .radius(8.0)
        .fill(palette.accent.alpha(0.08))
        .border(1.0, palette.accent.alpha(0.20))
        .child(icon(&INFORMATION, 14.0).color(palette.accent).a11y_hidden())
        .child(
            text("Can't find a setting? Keep this project picked above and hop to any other settings page.")
                .font(PROJECT_FONT).color(palette.ink).wrap(),
        );
    let mut details = col()
        .w_full()
        .radius(12.0)
        .border(1.0, palette.line)
        .child(setting_row(
            "Name",
            "The shared name for this project group in the sidebar and thread lists.",
            name_input(form, palette, writable, narrow),
            narrow,
            palette,
            None,
        ))
        .child(separator(palette))
        .child(icon_setting(
            state, workspace, project, form, writable, narrow, palette,
        ));
    if can_organize {
        let pinned = project.pinned_at.is_some();
        let archived = project.archived_at.is_some();
        details = details
            .child(separator(palette))
            .child(setting_row(
                "Pin",
                "Pinned projects come first in project pickers.",
                pin_switch(form, pinned, writable, palette),
                narrow,
                palette,
                None,
            ))
            .child(separator(palette))
            .child(setting_row(
                "Archive",
                if archived {
                    "Hidden from project pickers. Threads and files are kept."
                } else {
                    "Hides the project from project pickers. Threads and files are kept."
                },
                archive_button(form, archived, writable, palette),
                narrow,
                palette,
                None,
            ));
    }
    view = view.child(
        col()
            .w_full()
            .max_w(800.0)
            .gap(20.0)
            .child(scope)
            .child(info)
            .child(details)
            .child(actions::section(state, project, palette, narrow)),
    );
    if let Some(error) = &state.settings.error {
        view = view.child(error_text(error, palette));
    }
    view
}

fn project_picker_button(project: &Project, palette: Palette) -> El {
    row()
        .key("project-settings-change-project")
        .role("button")
        .label(format!("Change project from {}", project.name))
        .focusable(true)
        .focus_ring(1.0, 1.0, palette.accent)
        .h(28.0)
        .px(9.0)
        .items_center()
        .radius(6.0)
        .border(1.0, palette.line)
        .hover(palette.hover)
        .on_press_tx(|state: &mut State, tx| {
            state.save_project_name();
            state.settings.project = None;
            tx.focus(DIALOG, false);
        })
        .child(
            text(&project.name)
                .font(CONTROL)
                .color(palette.ink)
                .truncate(),
        )
}

fn project_choices(workspace: &Workspace, palette: Palette) -> El {
    let mut choices = col().w_full().max_w(800.0).gap(6.0).child(
        text("Choose a project")
            .font(super::SECTION)
            .color(palette.ink)
            .role("heading"),
    );
    if let Some(shell) = &workspace.shell {
        for project in &shell.projects {
            let id = project.id.clone();
            let label = if project.archived_at.is_some() {
                format!("{} (archived)", project.name)
            } else {
                project.name.clone()
            };
            choices = choices.child(
                row()
                    .key(format!("project-settings-select-{id}"))
                    .role("button")
                    .label(&label)
                    .focusable(true)
                    .focus_ring(1.0, 1.0, palette.accent)
                    .w_full()
                    .h(32.0)
                    .px(8.0)
                    .items_center()
                    .radius(6.0)
                    .hover(palette.hover)
                    .child(text(label).font(PROJECT_FONT).color(palette.ink))
                    .on_press_tx(move |state: &mut State, tx| {
                        let return_focus = state.settings.return_focus;
                        state.open_project_settings(&id, tx);
                        state.settings.return_focus = return_focus;
                    }),
            );
        }
    }
    choices
}

fn name_input(form: &Form, palette: Palette, writable: bool, narrow: bool) -> El {
    let editor = text_input(&form.edit, PROJECT_FONT)
        .key(PROJECT_NAME)
        .label("Project name")
        .color(palette.ink)
        .single_line()
        .h(26.0)
        .px(9.0)
        .radius(6.0)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent);
    let editor = if narrow {
        editor.w_full()
    } else {
        editor.w(254.0)
    };
    if !writable || form.pending.contains(&ProjectEditKind::Title) {
        return editor.disabled(true);
    }
    editor
        .on_edit(|state: &mut State| {
            &mut state
                .settings
                .project
                .as_mut()
                .expect("project settings open")
                .edit
        })
        .on_input_change(|state: &mut State, _| {
            if let Some(form) = &mut state.settings.project {
                form.dirty = true;
            }
        })
        .on_submit(|state: &mut State, tx| {
            state.save_project_name();
            tx.blur();
        })
        .on_input_key(|state: &mut State, event, tx| {
            if event.key != Key::Escape {
                return false;
            }
            if let Some(form) = &mut state.settings.project {
                form.dirty = false;
                form.edit.replace_text(&form.name);
            }
            tx.blur();
            true
        })
}

fn separator(palette: Palette) -> El {
    el().w_full().h(1.0).fill(palette.line)
}

fn setting_row(
    title: &str,
    description: &str,
    control: El,
    narrow: bool,
    palette: Palette,
    reset: Option<El>,
) -> El {
    let mut heading = row()
        .items_center()
        .gap(6.0)
        .child(text(title).font(CONTROL).color(palette.ink));
    if let Some(reset) = reset {
        heading = heading.child(reset);
    }
    let copy = col().grow(1.0).min_w(0.0).gap(4.0).child(heading).child(
        text(description)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let setting = if narrow {
        col().gap(8.0)
    } else {
        row().gap(16.0).items_center()
    };
    setting
        .w_full()
        .min_h(66.0)
        .pl(16.0)
        .pr(24.0)
        .py(12.0)
        .child(copy)
        .child(control)
}

fn pin_switch(form: &Form, pinned: bool, writable: bool, palette: Palette) -> El {
    let enabled = writable && !form.pending.contains(&ProjectEditKind::Pinned);
    let neutral = if palette.dark {
        rune::color::hex("#333333").expect("static switch color")
    } else {
        rune::color::hex("#d4d4d8").expect("static switch color")
    };
    el().key("project-settings-pin")
        .role("switch")
        .label("Pin project")
        .checked(pinned)
        .focusable(enabled)
        .disabled(!enabled)
        .focus_ring(2.0, 1.0, palette.accent)
        .size(30.0, 18.0)
        .radius(9.0)
        .fill(if pinned { palette.accent } else { neutral })
        .on_press_tx(move |state: &mut State, _| {
            state.edit_project_setting(ProjectEdit::Pinned(!pinned));
        })
        .child(
            el().absolute(if pinned { 14.0 } else { 2.0 }, 2.0)
                .size(14.0, 14.0)
                .radius(7.0)
                .fill(if pinned || !palette.dark {
                    Color::WHITE
                } else {
                    palette.muted
                }),
        )
}

fn archive_button(form: &Form, archived: bool, writable: bool, palette: Palette) -> El {
    let label = if archived { "Unarchive" } else { "Archive" };
    let enabled = writable && !form.pending.contains(&ProjectEditKind::Archived);
    el().key("project-settings-archive")
        .role("button")
        .label(label)
        .focusable(enabled)
        .disabled(!enabled)
        .focus_ring(1.0, 1.0, palette.accent)
        .w(if archived { 87.0 } else { 71.0 })
        .h(28.0)
        .center()
        .radius(6.0)
        .border(1.0, palette.line)
        .hover(palette.hover)
        .on_press_tx(move |state: &mut State, _| {
            state.edit_project_setting(ProjectEdit::Archived(!archived));
        })
        .child(text(label).font(CONTROL).color(palette.ink))
}

fn icon_setting(
    _state: &State,
    workspace: &Workspace,
    project: &Project,
    form: &Form,
    writable: bool,
    narrow: bool,
    palette: Palette,
) -> El {
    let description = match &project.icon {
        Some(ProjectIcon::Lucide { name, color }) => format!("{name} · {color}"),
        Some(ProjectIcon::Monogram { text, color }) => format!("{text} · {color}"),
        Some(ProjectIcon::Emoji { emoji }) => emoji.clone(),
        _ => project
            .favicon_path
            .as_deref()
            .unwrap_or("Automatic")
            .to_owned(),
    };
    let scratch = workspace.scratch_project.as_deref() == Some(project.id.as_str());
    let default_icon = scratch.then(|| ProjectIcon::Lucide {
        name: "message-square-dashed".into(),
        color: "gray".into(),
    });
    let enabled = writable && !form.pending.contains(&ProjectEditKind::Icon);
    let show_reset = project.favicon_path.is_some() || project.icon != default_icon;
    let reset = show_reset.then(|| {
        icon_button(
            "reset-project-icon",
            "Reset project icon to default",
            &RESET,
            12.0,
            20.0,
            palette,
            |state, _| {
                let scratch = state.settings.project.as_ref().is_some_and(|form| {
                    state
                        .workspace
                        .as_ref()
                        .and_then(|workspace| workspace.scratch_project.as_deref())
                        == Some(form.project_id.as_str())
                });
                let icon = scratch.then(|| ProjectIcon::Lucide {
                    name: "message-square-dashed".into(),
                    color: "gray".into(),
                });
                state.edit_project_setting(ProjectEdit::Icon(icon));
            },
        )
        .disabled(!enabled)
        .focusable(enabled)
    });
    let controls = row()
        .gap(8.0)
        .items_center()
        .child(project_badge::view_at(
            project,
            &workspace.favicons,
            palette.dark,
            24.0,
        ))
        .child(icon_picker::opener(palette, enabled))
        .child(icon_files::opener(
            palette,
            enabled && workspace.filesystem_read,
        ));
    setting_row(
        "Project icon",
        &description,
        controls,
        narrow,
        palette,
        reset,
    )
}
