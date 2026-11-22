use rune::color::hex;
use rune::{
    Color, El, Icon, Key, KeyEvent, Mods, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row,
    text, text_input,
};
use uuid::Uuid;

use crate::connection::Keybindings;

use super::sidebar::{normalized_search, project_badge, project_scope::ordered_projects};
use super::{Destination, Draft, Palette, Project, ProjectView, State, Workspace};

const SEARCH: &str = "new-thread-project-search";
const PROMPT: &str = "initial-prompt";
const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const SMALL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);

static BACK: Icon = Icon {
    name: "new-thread-picker-back",
    view: 24.0,
    body: r#"<path d="M19 12H5m7 7-7-7 7-7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static NO_PROJECT: Icon = Icon {
    name: "new-thread-picker-no-project",
    view: 24.0,
    body: r#"<path d="M5 21a2 2 0 0 1-2-2v-2l-1 1m1-5v-2m0-4V5a2 2 0 0 1 2-2h2m4 0h2m4 0h2a2 2 0 0 1 2 2v2m0 4v2m0 4v2a2 2 0 0 1-2 2h-2m-4 0h-2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(super) struct Picker {
    query: TextEdit,
    highlighted: Option<usize>,
    return_focus: Option<String>,
}

#[derive(Clone, Copy)]
enum Choice<'a> {
    Project(&'a Project, usize),
    Scratch,
    AddProject,
}

impl Choice<'_> {
    fn label(self) -> &'static str {
        match self {
            Self::Project(..) => "Project",
            Self::Scratch => "No project",
            Self::AddProject => "Add project",
        }
    }

    fn command(self) -> Option<String> {
        match self {
            Self::Project(_, rank) if rank <= 9 => Some(format!("thread.jump.{rank}")),
            Self::Scratch => Some("chat.newWithoutProject".into()),
            _ => None,
        }
    }

    fn key(self) -> String {
        match self {
            Self::Project(project, _) => format!("new-thread-project-{}", project.id),
            Self::Scratch => "new-thread-no-project".to_owned(),
            Self::AddProject => "new-thread-add-project".to_owned(),
        }
    }
}

impl Draft {
    pub(super) fn new_thread() -> Self {
        Self {
            navigation_id: Uuid::new_v4(),
            ..Self::default()
        }
    }

    pub(super) fn can_park(&self) -> bool {
        !self.pending && !self.refs.switching
    }
}

impl State {
    pub(super) fn start_new_thread_shortcut(&mut self, tx: &mut Tx) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.launch_writes && !workspace.reconnecting)
        else {
            return;
        };
        let Some(shell) = &workspace.shell else {
            return;
        };
        let mut active = shell
            .projects
            .iter()
            .filter(|project| project.archived_at.is_none());
        let only_open = active.next().filter(|_| active.next().is_none());
        if let Some(only_open) = only_open {
            let project = workspace
                .selection
                .as_ref()
                .filter(|selection| {
                    matches!(
                        selection.view,
                        ProjectView::Conversation(_) | ProjectView::NewThread
                    )
                })
                .map_or(only_open.id.as_str(), |selection| selection.id.as_str())
                .to_owned();
            self.open_fresh_project_draft(&project, tx);
            return;
        }
        super::sidebar::close_project_picker(self);
        self.draft_workspace_popup = None;
        self.new_thread_picker = Some(Picker {
            query: TextEdit::default(),
            highlighted: None,
            return_focus: tx.focused().map(str::to_owned),
        });
        tx.focus_when_shown(SEARCH, false);
    }

    pub(super) fn open_fresh_project_draft(&mut self, project: &str, tx: &mut Tx) {
        let destination = Destination::NewThread(project.to_owned());
        if !self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.can_visit(&destination))
        {
            return;
        }
        if self
            .new_thread_drafts
            .get(project)
            .is_some_and(|draft| draft.can_park() && !draft.edit.text.is_empty())
        {
            let fresh = Draft::new_thread();
            let id = fresh.navigation_id;
            self.parked_thread_drafts
                .insert(id, (project.to_owned(), fresh));
            self.navigate(Destination::Draft {
                project: project.to_owned(),
                id,
            });
        } else {
            self.navigate(destination);
        }
        tx.focus_when_shown(PROMPT, false);
    }
}

fn choices<'a>(workspace: &'a Workspace, query: &str) -> Vec<Choice<'a>> {
    let query = normalized_search(query.trim());
    let mut projects: Vec<_> = workspace
        .shell
        .as_ref()
        .into_iter()
        .flat_map(|shell| ordered_projects(shell, true))
        .filter(|project| !workspace.is_scratch(project))
        .collect();
    if let Some(selection) = &workspace.selection {
        projects.sort_by_key(|project| (project.pinned_at.is_none(), project.id != selection.id));
    }
    let no_project_position = if workspace.scratch_projects && workspace.project_writes {
        Some(
            (projects
                .iter()
                .take_while(|project| project.pinned_at.is_some())
                .count()
                + 1)
            .min(projects.len()),
        )
    } else {
        None
    };
    let mut items: Vec<_> = projects
        .into_iter()
        .map(|project| Choice::Project(project, 0))
        .collect();
    if let Some(position) = no_project_position {
        items.insert(position, Choice::Scratch);
    }
    if items.is_empty() && workspace.project_writes {
        items.push(Choice::AddProject);
    }
    items.retain(|choice| {
        let content = match choice {
            Choice::Project(project, _) => {
                normalized_search(&format!("{} {}", project.name, project.path))
            }
            Choice::Scratch => "no project without project none".to_owned(),
            Choice::AddProject => normalized_search(choice.label()),
        };
        query.split_whitespace().all(|word| content.contains(word))
    });
    let mut rank = 0;
    for item in &mut items {
        if let Choice::Project(_, number) = item {
            rank += 1;
            *number = rank;
        }
    }
    items
}

fn dismiss(state: &mut State, tx: &mut Tx) {
    if let Some(picker) = state.new_thread_picker.take()
        && let Some(focus) = picker.return_focus
    {
        tx.focus_when_shown(focus, tx.focus_visible());
    }
}

enum Selection {
    Project(String),
    Scratch,
    AddProject,
}

impl From<Choice<'_>> for Selection {
    fn from(choice: Choice<'_>) -> Self {
        match choice {
            Choice::Project(project, _) => Self::Project(project.id.clone()),
            Choice::Scratch => Self::Scratch,
            Choice::AddProject => Self::AddProject,
        }
    }
}

fn select(state: &mut State, tx: &mut Tx, selection: &Selection) {
    state.new_thread_picker = None;
    match selection {
        Selection::Project(id) => state.open_fresh_project_draft(id, tx),
        Selection::Scratch => state.start_scratch_from_new_thread_picker(),
        Selection::AddProject => state.show_add_project(tx),
    }
}

fn picker_bindings(bindings: Option<&Keybindings>) -> &Keybindings {
    match bindings {
        Some(bindings) => bindings,
        None => Keybindings::stock(),
    }
}

fn palette_context(flag: &str) -> bool {
    matches!(flag, "editableFocus" | "isDesktop")
}

fn keyboard(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let bindings = picker_bindings(state.keybindings.as_ref());
    let command = bindings.command(event, tx, palette_context);
    let Some(picker) = &mut state.new_thread_picker else {
        return false;
    };
    if event.key == Key::Escape || (event.key == Key::Backspace && picker.query.text.is_empty()) {
        dismiss(state, tx);
        return true;
    }
    let Some(workspace) = &state.workspace else {
        return false;
    };
    let items = choices(workspace, &picker.query.text);
    if event.mods == Mods::default() && matches!(event.key, Key::Up | Key::Down) {
        if !items.is_empty() {
            let index = picker.highlighted.filter(|index| *index < items.len());
            let next = match (event.key == Key::Up, index) {
                (true, None) => items.len() - 1,
                (true, Some(index)) => (index + items.len() - 1) % items.len(),
                (false, None) => 0,
                (false, Some(index)) => (index + 1) % items.len(),
            };
            picker.highlighted = Some(next);
            tx.scroll_into_view(items[next].key());
        }
        return true;
    }
    if event.key == Key::Tab {
        return true;
    }
    let selection = if event.key == Key::Enter && event.mods == Mods::default() {
        items.get(picker.highlighted.unwrap_or(0)).copied()
    } else if let Some(command) = command {
        items
            .iter()
            .copied()
            .find(|choice| choice.command().as_deref() == Some(command))
    } else {
        return false;
    };
    if let Some(selection) = selection.map(Selection::from) {
        if !tx.key_repeat() {
            select(state, tx, &selection);
        }
        return true;
    }
    event.key == Key::Enter
}

#[derive(Clone, Copy)]
struct Colors {
    surface: Color,
    edge: Color,
    active: Color,
    ink: Color,
    muted: Color,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let [surface, edge, active, ink, muted] = if dark {
            ["#060606", "#1b1b1b", "#202020", "#ededed", "#a3a3a3"]
        } else {
            ["#ffffff", "#e4e4e7", "#f4f4f5", "#18181b", "#71717a"]
        }
        .map(|color| hex(color).expect("picker palette color"));
        Self {
            surface,
            edge,
            active,
            ink,
            muted,
        }
    }
}

pub(super) fn overlay(state: &State, palette: Palette, width: f32, height: f32) -> Option<El> {
    let picker = state.new_thread_picker.as_ref()?;
    let workspace = state.workspace.as_ref()?;
    let colors = Colors::new(palette.dark);
    let matches = choices(workspace, &picker.query.text);
    let width = 576.0_f32.min((width - 32.0).max(0.0));
    let list_height = (height * 0.9 - 96.0).clamp(58.0, 384.0);
    let list = col()
        .key("new-thread-project-options")
        .role("listbox")
        .max_h(list_height)
        .w_full()
        .px(8.0)
        .pt(8.0)
        .pb(10.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            text(if matches.is_empty() {
                "No results found."
            } else {
                "Projects"
            })
            .font(SMALL)
            .color(colors.muted)
            .px(8.0)
            .py(6.0),
        )
        .children(matches.iter().copied().enumerate().map(|(index, choice)| {
            option(
                state,
                workspace,
                choice,
                picker.highlighted == Some(index),
                palette.dark,
                colors,
            )
        }));
    let mut search = text_input(&picker.query, BODY)
        .key(SEARCH)
        .role("combobox")
        .label("Search projects")
        .placeholder("Search...", colors.muted)
        .color(colors.ink)
        .selection_color(palette.selected)
        .h(46.0)
        .py(13.0)
        .min_w(0.0)
        .grow(1.0)
        .single_line()
        .on_edit(|state: &mut State| {
            &mut state
                .new_thread_picker
                .as_mut()
                .expect("project picker is open")
                .query
        })
        .on_input_change(|state: &mut State, _| {
            if let Some(picker) = &mut state.new_thread_picker {
                picker.highlighted = None;
            }
        })
        .on_input_key(keyboard);
    if let Some(choice) = picker.highlighted.and_then(|index| matches.get(index)) {
        search = search.active_descendant(choice.key());
    }
    let back = el()
        .key("new-thread-project-back")
        .role("button")
        .label("Back")
        .size(24.0, 28.0)
        .center()
        .on_press_tx(dismiss)
        .child(icon(&BACK, 14.0).color(colors.muted).a11y_hidden());
    let dialog = col()
        .key("new-thread-project-picker")
        .role("dialog")
        .label("Command palette")
        .w(width)
        .radius(16.0)
        .clip_rounded(16.0)
        .fill(colors.surface)
        .border(1.0, colors.edge)
        .border_layout()
        .child(
            row()
                .w_full()
                .center_y()
                .px(12.0)
                .gap(5.0)
                .child(back)
                .child(search),
        )
        .child(list)
        .child(footer(colors));
    Some(
        col()
            .key("new-thread-project-modal")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .pt(height * 0.1)
            .items_center()
            .fill(Color::BLACK.alpha(if palette.dark { 0.80 } else { 0.35 }))
            .backdrop_blur(4.0)
            .on_background_press_tx(dismiss)
            .child(dialog),
    )
}

fn option(
    state: &State,
    workspace: &Workspace,
    choice: Choice<'_>,
    selected: bool,
    dark: bool,
    colors: Colors,
) -> El {
    let (title, detail, badge) = match choice {
        Choice::Project(project, _) => {
            let current = workspace
                .selection
                .as_ref()
                .is_some_and(|selection| selection.id == project.id);
            let environment = if state.local_environment {
                "Local"
            } else {
                &workspace.label
            };
            let detail = format!(
                "{}{environment} · {}",
                if current { "Current · " } else { "" },
                project.path
            );
            (
                project.name.as_str(),
                Some(detail),
                project_badge::view_at(project, &workspace.favicons, dark, 16.0),
            )
        }
        Choice::Scratch => (
            choice.label(),
            None,
            icon(&NO_PROJECT, 16.0).color(colors.muted).a11y_hidden(),
        ),
        Choice::AddProject => (choice.label(), None, el().size(16.0, 16.0)),
    };
    let bindings = picker_bindings(state.keybindings.as_ref());
    let hint = choice
        .command()
        .and_then(|command| bindings.shortcut_label(&command, palette_context));
    let selection = Selection::from(choice);
    let mut labels = col()
        .grow(1.0)
        .min_w(0.0)
        .child(text(title).font(BODY).color(colors.ink).truncate());
    let height = if let Some(detail) = detail {
        labels = labels.child(text(detail).font(SMALL).color(colors.muted).truncate());
        48.0
    } else {
        32.0
    };
    row()
        .key(choice.key())
        .role("option")
        .label(title)
        .h(height)
        .w_full()
        .center_y()
        .px(8.0)
        .gap(8.0)
        .radius(8.0)
        .fill(if selected {
            colors.active
        } else {
            Color::TRANSPARENT
        })
        .hover(colors.active)
        .on_press_tx(move |state, tx| select(state, tx, &selection))
        .child(badge)
        .child(labels)
        .children(hint.map(|hint| text(hint).font(SMALL).color(colors.muted)))
}

fn footer(colors: Colors) -> El {
    let keys = |label: &'static str| {
        el().h(20.0)
            .px(4.0)
            .center()
            .radius(4.0)
            .fill(colors.active)
            .child(text(label).font(SMALL).color(colors.ink))
    };
    let hint = |label: &'static str| text(label).font(BODY).color(colors.muted);
    row()
        .w_full()
        .h(41.0)
        .min_w(0.0)
        .center_y()
        .gap(5.0)
        .px(16.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], colors.edge)
        .child(keys("↑"))
        .child(keys("↓"))
        .child(hint("Navigate").mr(5.0))
        .child(keys("Enter"))
        .child(hint("Select").mr(5.0))
        .child(keys("Backspace"))
        .child(hint("Back").mr(5.0))
        .child(keys("Esc"))
        .child(hint("Close"))
}
