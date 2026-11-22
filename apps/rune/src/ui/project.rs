use rune::{Color, El, Key, KeyEvent, ScrollSpec, TextEdit, Tx, col, el, row, text};
use uuid::Uuid;

use crate::connection::{FolderListing, ProjectCreated};

use super::{BODY, Palette, SMALL, State, TITLE, button, error_text, form_field, input};

pub(super) mod clone_banner;
mod git_url;
mod home;

pub(super) use home::{ScratchRequest, empty_home};

#[derive(Default)]
pub(super) struct AddProject {
    path: TextEdit,
    pending: Option<String>,
    error: Option<String>,
    browse: Option<Browse>,
    step: ProjectStep,
    sources: ProjectSources,
    clone: Option<git_url::Destination>,
}

pub(super) struct AddedProject {
    id: String,
    destination: ProjectDestination,
}

enum ProjectDestination {
    Project,
    Draft,
    MoveDraft(String),
    NewThread(super::Destination),
}

#[derive(Clone, Copy, Default)]
struct ProjectSources {
    named: bool,
    git: bool,
}

#[derive(Clone, Copy, Default)]
enum ProjectStep {
    Sources,
    #[default]
    Folder,
    Name,
    GitUrl,
    GitDestination,
}

#[derive(Default)]
struct Browse {
    cwd: Option<String>,
    requested_path: String,
    pending: Option<Uuid>,
    listing: Option<FolderListing>,
    error: Option<String>,
    active_row: usize,
    parent: Option<String>,
    exact_entry: bool,
}

impl Browse {
    fn row_count(&self) -> usize {
        self.listing
            .as_ref()
            .map_or(0, |listing| listing.entries.len())
            + usize::from(self.parent.is_some())
    }

    fn selected_path(&self) -> Option<&str> {
        let index = self.active_row.checked_sub(1)?;
        if let Some(parent) = &self.parent
            && index == 0
        {
            return Some(parent);
        }
        self.listing
            .as_ref()?
            .entries
            .get(index - usize::from(self.parent.is_some()))
            .map(|entry| entry.full_path.as_str())
    }
}

impl State {
    pub(super) fn show_add_project(&mut self, tx: &mut Tx) {
        if let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.project_writes && !workspace.reconnecting)
        {
            self.project_notice = None;
            self.add_project.get_or_insert_with(|| AddProject {
                sources: ProjectSources {
                    named: workspace.named_projects,
                    git: workspace.clone_writes,
                },
                step: if workspace.filesystem_read
                    || workspace.named_projects
                    || workspace.clone_writes
                {
                    tx.focus_when_shown("project-source-folder", false);
                    ProjectStep::Sources
                } else {
                    ProjectStep::Folder
                },
                ..AddProject::default()
            });
        }
    }

    fn choose_project_folder(&mut self, tx: &mut Tx) {
        let cwd = self.project_browse_cwd();
        let Some(form) = &mut self.add_project else {
            return;
        };
        form.step = ProjectStep::Folder;
        if self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.filesystem_read)
        {
            form.browse = Some(Browse {
                cwd,
                ..Browse::default()
            });
            form.path.replace_text("~/");
        }
        tx.focus_when_shown("project-folder", false);
    }

    fn project_browse_cwd(&self) -> Option<String> {
        let workspace = self.workspace.as_ref()?;
        let project_id = &workspace.selection.as_ref()?.id;
        workspace
            .shell
            .as_ref()?
            .projects
            .iter()
            .find(|project| &project.id == project_id)
            .map(|project| project.path.clone())
    }

    pub(super) fn choose_project_name(&mut self, tx: &mut Tx) {
        if let Some(form) = &mut self.add_project {
            form.step = ProjectStep::Name;
            form.browse = None;
            form.path.clear();
            tx.focus_when_shown("project-name", false);
        }
    }

    fn submit_named_project(&mut self, _: &mut Tx) {
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return;
        };
        let Some(connection) = &self.connection else {
            return;
        };
        let name = form.path.text.trim();
        if name.is_empty() || name.encode_utf16().count() > 200 {
            return;
        }
        let command_id = Uuid::new_v4().to_string();
        connection.create_named_project(command_id.clone(), name.to_owned());
        form.pending = Some(command_id);
        form.error = None;
    }

    pub(super) fn poll_project_browse(&mut self) -> bool {
        let Some(connection) = self.connection.as_ref().filter(|_| {
            self.workspace
                .as_ref()
                .is_some_and(|workspace| !workspace.reconnecting)
        }) else {
            return false;
        };
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return false;
        };
        let Some(browse) = form.browse.as_mut() else {
            return false;
        };
        let path = form.path.text.trim();
        if path == browse.requested_path {
            return false;
        }
        browse.requested_path = path.into();
        browse.listing = None;
        browse.pending = None;
        browse.error = None;
        browse.active_row = 0;
        browse.parent = None;
        browse.exact_entry = false;
        if !path.is_empty() && path.encode_utf16().count() <= 512 {
            let id = Uuid::new_v4();
            browse.pending = Some(id);
            let query = form
                .clone
                .as_ref()
                .and_then(|clone| clone.browse_query(path));
            connection.browse_folder(id, query.unwrap_or(path).into(), browse.cwd.clone());
        }
        true
    }

    pub(super) fn folder_browsed(
        &mut self,
        browse_id: Uuid,
        result: Result<FolderListing, String>,
    ) {
        let Some(form) = self.add_project.as_mut() else {
            return;
        };
        let Some(browse) = form
            .browse
            .as_mut()
            .filter(|browse| browse.pending == Some(browse_id))
        else {
            return;
        };
        browse.pending = None;
        match result {
            Ok(mut listing) => {
                if let Some(clone) = &form.clone {
                    browse.parent =
                        git_url::browse_parent(&browse.requested_path, &listing.parent_path);
                    browse.exact_entry = clone.filter_entries(&browse.requested_path, &mut listing);
                }
                browse.listing = Some(listing);
            }
            Err(error) => browse.error = Some(error),
        }
    }

    fn navigate_project_folder(&mut self, path: &str, tx: &mut Tx) {
        if let Some(form) = &mut self.add_project {
            if let Some(clone) = &form.clone {
                form.path.replace_text(clone.browse_destination(path));
                tx.focus_when_shown("project-git-destination", false);
                return;
            }
            let separator = if path.contains('\\') && !path.contains('/') {
                '\\'
            } else {
                '/'
            };
            if path.ends_with(['/', '\\']) {
                form.path.replace_text(path);
            } else {
                form.path.replace_text(format!("{path}{separator}"));
            }
            tx.focus_when_shown("project-folder", false);
        }
    }

    fn browse_keyboard(&mut self, event: &KeyEvent, tx: &mut Tx) -> bool {
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return false;
        };
        let Some(browse) = form.browse.as_mut() else {
            return false;
        };
        let entries = browse
            .listing
            .as_ref()
            .map_or(&[][..], |listing| listing.entries.as_slice());
        match event.key {
            Key::Escape => self.add_project = None,
            Key::Enter if event.mods.cmd && matches!(form.step, ProjectStep::GitDestination) => {
                self.submit_git_project(tx);
            }
            Key::Up | Key::Down if !event.mods.cmd && !event.mods.alt && !event.mods.shift => {
                browse.active_row = if event.key == Key::Up {
                    browse.active_row.saturating_sub(1)
                } else {
                    browse.active_row.saturating_add(1).min(browse.row_count())
                };
                if let Some(path) = browse.selected_path() {
                    tx.scroll_key_center_at(format!("project-entry-{path}"), 156.0);
                }
            }
            Key::Tab if !event.mods.cmd && !event.mods.alt && !event.mods.shift => {
                let selected = browse
                    .selected_path()
                    .or_else(|| (entries.len() == 1).then(|| entries[0].full_path.as_str()));
                let Some(path) = selected.map(str::to_owned) else {
                    return false;
                };
                self.navigate_project_folder(&path, tx);
            }
            _ => return false,
        }
        true
    }

    fn activate_project_browse(&mut self, tx: &mut Tx) {
        let selected = self
            .add_project
            .as_ref()
            .and_then(|form| form.browse.as_ref()?.selected_path().map(str::to_owned));
        if let Some(path) = selected {
            self.navigate_project_folder(&path, tx);
        } else if self
            .add_project
            .as_ref()
            .is_some_and(|form| matches!(form.step, ProjectStep::GitDestination))
        {
            self.submit_git_project(tx);
        } else {
            self.submit_project(tx);
        }
    }

    fn submit_project(&mut self, _: &mut Tx) {
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return;
        };
        let Some(connection) = &self.connection else {
            return;
        };
        let typed_path = form.path.text.trim();
        if typed_path.is_empty() {
            return;
        }
        let workspace_root = form
            .browse
            .as_ref()
            .filter(|browse| browse.requested_path == typed_path)
            .and_then(|browse| browse.listing.as_ref())
            .and_then(|listing| {
                if typed_path == "~" || typed_path.ends_with(['/', '\\']) {
                    Some(listing.parent_path.clone())
                } else {
                    let leaf = typed_path.rsplit(['/', '\\']).next()?;
                    listing
                        .entries
                        .iter()
                        .find(|entry| entry.name == leaf)
                        .map(|entry| entry.full_path.clone())
                }
            })
            .unwrap_or_else(|| typed_path.to_owned());
        let title = workspace_root
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(&workspace_root);
        let command_id = Uuid::new_v4().to_string();
        connection.add_project(
            command_id.clone(),
            workspace_root.to_owned(),
            title.to_owned(),
        );
        form.pending = Some(command_id);
        form.error = None;
    }

    pub(super) fn project_added(
        &mut self,
        command_id: &str,
        result: Result<ProjectCreated, String>,
        tx: &mut Tx,
    ) {
        if self
            .scratch_request
            .as_ref()
            .is_some_and(|pending| pending.command_id == command_id)
        {
            let pending = self
                .scratch_request
                .take()
                .expect("matched scratch request");
            match result {
                Ok(project) => {
                    if let Some(workspace) = &mut self.workspace {
                        workspace.scratch_project = Some(project.project_id.clone());
                    }
                    self.added_project = Some(AddedProject {
                        id: project.project_id,
                        destination: pending.destination,
                    });
                    self.select_added_project(tx);
                }
                Err(message) => self.project_notice = Some(message),
            }
            return;
        }
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.as_deref() == Some(command_id))
        else {
            return;
        };
        match result {
            Ok(project) => {
                let destination = if matches!(form.step, ProjectStep::GitDestination) {
                    ProjectDestination::Draft
                } else {
                    ProjectDestination::Project
                };
                self.add_project = None;
                self.project_notice = project.commit_error;
                self.added_project = Some(AddedProject {
                    id: project.project_id,
                    destination,
                });
                self.select_added_project(tx);
            }
            Err(error) => {
                form.pending = None;
                form.error = Some(error);
            }
        }
    }

    pub(super) fn select_added_project(&mut self, tx: &mut Tx) {
        let Some(added) = &self.added_project else {
            return;
        };
        if self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.shell.as_ref())
            .is_some_and(|shell| shell.projects.iter().any(|project| project.id == added.id))
        {
            let added = self
                .added_project
                .take()
                .expect("a project selection is pending");
            match added.destination {
                ProjectDestination::Project => self.select_project(&added.id),
                ProjectDestination::Draft => {
                    let destination = super::Destination::NewThread(added.id.clone());
                    if self
                        .workspace
                        .as_ref()
                        .is_some_and(|workspace| workspace.can_visit(&destination))
                    {
                        self.navigate(destination);
                    } else {
                        self.select_project(&added.id);
                    }
                }
                ProjectDestination::MoveDraft(origin) => {
                    if self.workspace.as_ref().is_some_and(|workspace| {
                        workspace.destination() == super::Destination::NewThread(origin)
                    }) {
                        super::sidebar::change_draft_project(self, &added.id);
                    }
                }
                ProjectDestination::NewThread(origin) => {
                    if self.history_destination().as_ref() == Some(&origin) {
                        self.open_fresh_project_draft(&added.id, tx);
                    }
                }
            }
        }
    }
}

pub(super) fn dialog(form: &AddProject, palette: Palette) -> El {
    match form.step {
        ProjectStep::Sources => return source_dialog(form.sources, palette),
        ProjectStep::Name => return name_dialog(form, palette),
        ProjectStep::GitUrl | ProjectStep::GitDestination => return git_url::dialog(form, palette),
        ProjectStep::Folder => {}
    }
    if let Some(browse) = &form.browse {
        return browse_dialog(form, browse, palette);
    }
    let busy = form.pending.is_some();
    let mut card = col()
        .w_full()
        .max_w(576.0)
        .padding(20.0)
        .gap(12.0)
        .radius(10.0)
        .fill(palette.canvas)
        .border(1.0, palette.line)
        .role("dialog")
        .label("Add project")
        .child(
            text("Add project")
                .font(TITLE)
                .color(palette.ink)
                .role("heading"),
        )
        .child(
            text("Enter the folder path on this environment.")
                .font(BODY)
                .color(palette.muted),
        )
        .child(form_field(
            "Folder path",
            input("project-folder", "Folder path", &form.path, palette)
                .single_line()
                .disabled(busy)
                .on_edit(|state: &mut State| {
                    &mut state
                        .add_project
                        .as_mut()
                        .expect("the project dialog is open")
                        .path
                })
                .on_submit(State::submit_project),
            palette,
        ));
    if let Some(error) = &form.error {
        card = card.child(error_text(error, palette));
    }
    el().absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .padding(16.0)
        .fill(Color::BLACK.alpha(0.55))
        .child(
            card.child(
                row()
                    .gap(8.0)
                    .child(button(
                        "cancel-project",
                        "Cancel",
                        palette,
                        false,
                        !busy,
                        |state, _| state.add_project = None,
                    ))
                    .child(button(
                        "submit-project",
                        if busy { "Adding…" } else { "Add project" },
                        palette,
                        true,
                        !busy && !form.path.text.trim().is_empty(),
                        State::submit_project,
                    )),
            ),
        )
}

fn source_dialog(sources: ProjectSources, palette: Palette) -> El {
    let mut choices = col().gap(4.0);
    if sources.git {
        choices = choices.child(
            text("Sources")
                .font(SMALL)
                .color(palette.muted)
                .px(8.0)
                .py(6.0),
        );
    }
    if sources.named {
        choices = choices.child(button(
            "project-source-name",
            "New project",
            palette,
            false,
            true,
            State::choose_project_name,
        ));
    }
    choices = choices.child(button(
        "project-source-folder",
        "Local folder",
        palette,
        false,
        true,
        State::choose_project_folder,
    ));
    if sources.git {
        choices = choices.child(git_url::source_item(palette));
    }
    project_dialog(choices.child(close_button(palette, false)), palette)
}

fn name_dialog(form: &AddProject, palette: Palette) -> El {
    let busy = form.pending.is_some();
    let valid_name =
        !form.path.text.trim().is_empty() && form.path.text.trim().encode_utf16().count() <= 200;
    let mut contents = col().gap(4.0).child(
        input("project-name", "Enter a project name", &form.path, palette)
            .single_line()
            .label("Project name")
            .disabled(busy)
            .on_edit(|state: &mut State| {
                &mut state
                    .add_project
                    .as_mut()
                    .expect("open project dialog")
                    .path
            })
            .on_submit(State::submit_named_project),
    );
    if let Some(error) = &form.error {
        contents = contents.child(error_text(error, palette));
    }
    contents = contents.child(button(
        "submit-named-project",
        if busy {
            "Creating…"
        } else {
            "Create project"
        },
        palette,
        false,
        valid_name && !busy,
        State::submit_named_project,
    ));
    project_dialog(contents.child(close_button(palette, busy)), palette)
}

fn close_button(palette: Palette, busy: bool) -> El {
    button(
        "cancel-project",
        "Close",
        palette,
        false,
        !busy,
        |state, _| state.add_project = None,
    )
}

fn project_dialog(contents: El, palette: Palette) -> El {
    col()
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .items_center()
        .pt(74.0)
        .fill(Color::BLACK.alpha(0.55))
        .child(
            col()
                .w_full()
                .max_w(576.0)
                .padding(8.0)
                .radius(12.0)
                .fill(palette.canvas)
                .border(1.0, palette.line)
                .role("dialog")
                .label("Add project")
                .child(contents)
                .on_key(|state: &mut State, event, _| {
                    if event.key == Key::Escape
                        && state
                            .add_project
                            .as_ref()
                            .is_some_and(|form| form.pending.is_none())
                    {
                        state.add_project = None;
                        return true;
                    }
                    false
                }),
        )
}

fn browse_dialog(form: &AddProject, browse: &Browse, palette: Palette) -> El {
    let busy = form.pending.is_some();
    let mut options = col().w_full();
    if let Some(listing) = &browse.listing
        && !listing.entries.is_empty()
    {
        options = options.child(text("Directories").font(SMALL).color(palette.muted));
        for (index, entry) in listing.entries.iter().enumerate() {
            let path = entry.full_path.clone();
            let selected = browse.active_row == index + 1;
            options = options.child(
                button(
                    &format!("project-entry-{}", entry.full_path),
                    &entry.name,
                    palette,
                    false,
                    !busy,
                    move |state, tx| state.navigate_project_folder(&path, tx),
                )
                .fill(if selected {
                    palette.selected
                } else {
                    Color::TRANSPARENT
                })
                .w_full(),
            );
        }
    }
    let edit = input(
        "project-folder",
        "Enter path (e.g. ~/projects/my-app)",
        &form.path,
        palette,
    )
    .single_line()
    .label("Folder path")
    .disabled(busy)
    .on_edit(|state: &mut State| &mut state.add_project.as_mut().expect("open folder dialog").path)
    .on_submit(State::activate_project_browse)
    .on_input_key(State::browse_keyboard);
    let mut card = col()
        .w_full()
        .max_w(576.0)
        .padding(8.0)
        .gap(4.0)
        .radius(12.0)
        .fill(palette.canvas)
        .border(1.0, palette.line)
        .role("dialog")
        .label("Add project")
        .child(edit)
        .child(
            button(
                "submit-project",
                if busy { "Adding…" } else { "Add" },
                palette,
                false,
                !busy && !form.path.text.trim().is_empty(),
                State::submit_project,
            )
            .fill(if browse.active_row == 0 && !busy {
                palette.selected
            } else {
                Color::TRANSPARENT
            })
            .w_full(),
        );
    if let Some(error) = form.error.as_ref().or(browse.error.as_ref()) {
        card = card.child(error_text(error, palette));
    }
    card = card
        .child(
            options
                .key("project-folder-list")
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .max_h(340.0),
        )
        .child(button(
            "cancel-project",
            "Close",
            palette,
            false,
            !busy,
            |state, _| state.add_project = None,
        ));
    col()
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .items_center()
        .pt(74.0)
        .fill(Color::BLACK.alpha(0.55))
        .child(card)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Mods, Runtime};

    #[test]
    fn folder_keyboard_activates_the_highlighted_directory() {
        let mut state = State {
            add_project: Some(AddProject {
                browse: Some(Browse {
                    listing: Some(
                        serde_json::from_value(serde_json::json!({
                            "parentPath": "/projects",
                            "entries": [{"name": "child", "fullPath": "/projects/child"}]
                        }))
                        .unwrap(),
                    ),
                    ..Browse::default()
                }),
                ..AddProject::default()
            }),
            ..State::default()
        };
        state
            .add_project
            .as_mut()
            .unwrap()
            .path
            .replace_text("/projects/");
        let mut runtime = Runtime::new(
            state,
            super::super::view,
            (1100.0, 780.0),
            Env::new(true, 1.0),
        );
        runtime.frame(0.01);
        let (x, y) = runtime.find("project-folder").unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(0.01);
        runtime.key(KeyEvent {
            key: Key::Down,
            mods: Mods::default(),
        });
        runtime.frame(0.01);
        assert_eq!(
            runtime
                .state
                .add_project
                .as_ref()
                .unwrap()
                .browse
                .as_ref()
                .unwrap()
                .active_row,
            1
        );
        runtime.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        runtime.frame(0.01);
        assert_eq!(
            runtime.state.add_project.as_ref().unwrap().path.text,
            "/projects/child/"
        );
    }
}
