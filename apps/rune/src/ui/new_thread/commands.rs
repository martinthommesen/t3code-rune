use std::cmp::Reverse;

use rune::Tx;

use super::ranking::{Query, Rank};
use crate::ui::settings::search as settings_search;
use crate::ui::sidebar::{most_recent_project, project_scope::ordered_projects};
use crate::ui::{Project, ProjectView, State, Thread, Workspace};

#[derive(Clone, Copy)]
pub(super) enum Choice<'a> {
    NewHere(&'a Project),
    ChooseProject,
    Scratch,
    NewProject,
    AddProject,
    ChangeAppearance,
    Settings,
    Setting(settings_search::Item<'a>),
    ProjectSettings(&'a Project),
    Thread(&'a Thread, &'a Project),
    Project(&'a Project),
    NewIn(&'a Project, usize),
}

impl<'a> Choice<'a> {
    pub(super) fn key(self) -> String {
        match self {
            Self::NewHere(_) => "command-palette-new-here".into(),
            Self::ChooseProject => "command-palette-new-in".into(),
            Self::Scratch => "command-palette-no-project".into(),
            Self::NewProject => "command-palette-new-project".into(),
            Self::AddProject => "command-palette-add-project".into(),
            Self::ChangeAppearance => "command-palette-change-appearance".into(),
            Self::Settings => "command-palette-settings".into(),
            Self::Setting(item) => item.key(),
            Self::ProjectSettings(_) => "command-palette-project-settings".into(),
            Self::Thread(thread, _) => format!("command-palette-thread-{}", thread.id),
            Self::Project(project) => format!("command-palette-project-{}", project.id),
            Self::NewIn(project, _) => format!("command-palette-new-in-{}", project.id),
        }
    }

    pub(super) fn title(self) -> String {
        match self {
            Self::NewHere(project) | Self::NewIn(project, _) => {
                format!("New thread in {}", project.name)
            }
            Self::ChooseProject => "New thread in...".into(),
            Self::Scratch => "New thread without a project".into(),
            Self::NewProject => "New project".into(),
            Self::AddProject => "Add project".into(),
            Self::ChangeAppearance => "Change appearance".into(),
            Self::Settings => "Open settings".into(),
            Self::Setting(item) => item.title(),
            Self::ProjectSettings(_) => "Project settings".into(),
            Self::Thread(thread, _) => thread.title.clone(),
            Self::Project(project) => project.name.clone(),
        }
    }

    pub(super) fn icon_name(self) -> &'static str {
        match self {
            Self::NewHere(_) | Self::ChooseProject => "SquarePen",
            Self::Scratch => "MessageSquareDashed",
            Self::NewProject | Self::AddProject => "FolderPlus",
            Self::ChangeAppearance => "Palette",
            Self::Settings | Self::ProjectSettings(_) | Self::Setting(_) => "Settings",
            Self::Thread(..) => "MessageSquare",
            Self::Project(_) | Self::NewIn(..) => "Folder",
        }
    }

    pub(super) fn description(self, environment: &str) -> Option<String> {
        match self {
            Self::Project(project) | Self::NewIn(project, _) => {
                Some(format!("{environment} · {}", project.path))
            }
            Self::ProjectSettings(project) => Some(project.name.clone()),
            Self::Setting(item) => Some(format!("Settings · {}", item.page_label())),
            Self::Thread(_, project) => Some(format!("{} · {environment}", project.name)),
            _ => None,
        }
    }

    pub(super) fn project_badge(self) -> Option<&'a Project> {
        match self {
            Self::Project(project) | Self::NewIn(project, _) => Some(project),
            _ => None,
        }
    }

    pub(super) fn command(self) -> Option<String> {
        match self {
            Self::NewHere(_) => Some("chat.newLocal".into()),
            Self::Scratch => Some("chat.newWithoutProject".into()),
            Self::NewIn(_, rank) if rank <= 9 => Some(format!("thread.jump.{rank}")),
            _ => None,
        }
    }

    fn secondary(self) -> bool {
        matches!(self, Self::Setting(item) if item.secondary())
    }

    fn rank(self, query: &Query, typos: bool) -> Option<Rank> {
        if let Self::Setting(item) = self {
            let keywords = item.keywords();
            let words: Vec<_> = keywords.iter().map(String::as_str).collect();
            let mut rank = query.rank(&item.title(), &words, typos)?;
            if item.secondary() {
                rank.group_class = 0;
            }
            return Some(rank);
        }
        let keywords: Vec<&str> = match self {
            Self::NewHere(project) | Self::NewIn(project, _) => {
                vec![project.name.as_str(), project.path.as_str()]
            }
            Self::ChooseProject => vec!["new thread", "project", "start chat"],
            Self::Scratch => vec!["no project", "scratch", "without folder"],
            Self::NewProject => vec!["create project"],
            Self::AddProject => vec!["open folder workspace", "repository"],
            Self::ChangeAppearance => vec!["light dark system color mode"],
            Self::Setting(_) => unreachable!("indexed settings are ranked above"),
            Self::Settings => vec!["settings", "preferences", "configuration", "keybindings"],
            Self::ProjectSettings(project) => {
                vec![&project.name, "project settings", "name icon model scripts"]
            }
            Self::Thread(thread, project) => {
                vec![&project.name, thread.branch.as_deref().unwrap_or("")]
            }
            Self::Project(project) => vec![&project.path],
        };
        query.rank(&self.title(), &keywords, typos)
    }
}

pub(super) enum Selection {
    NewThread(String),
    ChooseProject,
    Scratch,
    NewProject,
    AddProject,
    ChangeAppearance,
    Settings,
    Setting(settings_search::Destination),
    ProjectSettings(String),
    Thread(String),
    Project(String),
}

impl From<Choice<'_>> for Selection {
    fn from(choice: Choice<'_>) -> Self {
        match choice {
            Choice::NewHere(project) | Choice::NewIn(project, _) => {
                Self::NewThread(project.id.clone())
            }
            Choice::ChooseProject => Self::ChooseProject,
            Choice::Scratch => Self::Scratch,
            Choice::NewProject => Self::NewProject,
            Choice::AddProject => Self::AddProject,
            Choice::ChangeAppearance => Self::ChangeAppearance,
            Choice::Settings => Self::Settings,
            Choice::Setting(item) => Self::Setting(item.destination()),
            Choice::ProjectSettings(project) => Self::ProjectSettings(project.id.clone()),
            Choice::Thread(thread, _) => Self::Thread(thread.id.clone()),
            Choice::Project(project) => Self::Project(project.id.clone()),
        }
    }
}

pub(super) fn select(state: &mut State, tx: &mut Tx, selection: &Selection) {
    let submenu = match selection {
        Selection::ChooseProject => Some(super::Page::Projects),
        Selection::ChangeAppearance => Some(super::Page::Appearance),
        _ => None,
    };
    if let Some(page) = submenu {
        super::show_submenu(state, page, tx);
        return;
    }
    state.command_palette = None;
    if state.settings.is_open()
        && !matches!(
            selection,
            Selection::Settings | Selection::ProjectSettings(_) | Selection::Setting(_)
        )
    {
        state.close_client_settings(tx);
    }
    match selection {
        Selection::NewThread(id) => state.open_fresh_project_draft(id, tx),
        Selection::Scratch => state.start_scratch_from_command_palette(),
        Selection::NewProject => {
            state.show_add_project(tx);
            state.choose_project_name(tx);
        }
        Selection::AddProject => state.show_add_project(tx),
        Selection::Settings => state.open_client_settings(tx),
        Selection::Setting(destination) => state.open_settings_destination(destination, tx),
        Selection::ProjectSettings(id) => state.open_project_settings(id, tx),
        Selection::Thread(id) => open_thread(state, tx, id),
        Selection::Project(id) => {
            let current = state
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.shell.as_ref())
                .and_then(|shell| {
                    shell
                        .threads
                        .iter()
                        .filter(|thread| !thread.archived && &thread.project_id == id)
                        .max_by_key(|thread| thread.latest_user_activity())
                })
                .map(|thread| thread.id.clone());
            if let Some(thread) = current {
                open_thread(state, tx, &thread);
            } else if state
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.launch_writes)
            {
                state.open_fresh_project_draft(id, tx);
            } else {
                state.select_project(id);
            }
        }
        Selection::ChooseProject | Selection::ChangeAppearance => unreachable!(),
    }
}

fn open_thread(state: &mut State, tx: &mut Tx, id: &str) {
    state.open_thread(id);
    tx.focus_when_shown("prompt", false);
}

pub(super) struct Section<'a> {
    pub(super) label: &'static str,
    pub(super) entries: Vec<Choice<'a>>,
}

pub(super) fn sections<'a>(
    state: &'a State,
    workspace: &'a Workspace,
    raw: &str,
) -> Vec<Section<'a>> {
    let query = Query::new(raw);
    let Some(shell) = &workspace.shell else {
        return vec![Section {
            label: "Actions",
            entries: vec![Choice::Settings],
        }];
    };
    let current = workspace.selection.as_ref().and_then(|selection| {
        shell
            .projects
            .iter()
            .find(|project| project.id == selection.id)
    });
    let mut actions = Vec::new();
    if workspace.launch_writes && !workspace.reconnecting {
        if let Some(project) = current.or_else(|| most_recent_project(shell)) {
            actions.push(Choice::NewHere(project));
        }
        actions.push(Choice::ChooseProject);
        if workspace.scratch_projects && workspace.project_writes {
            actions.push(Choice::Scratch);
        }
    }
    if workspace.project_writes && !workspace.reconnecting {
        if workspace.named_projects {
            actions.push(Choice::NewProject);
        }
        actions.push(Choice::AddProject);
    }
    actions.push(Choice::ChangeAppearance);
    actions.push(Choice::Settings);
    if let Some(project) = current {
        actions.push(Choice::ProjectSettings(project));
    }
    let active_thread = workspace
        .selection
        .as_ref()
        .and_then(|selection| match &selection.view {
            ProjectView::Conversation(conversation) => Some(&conversation.id),
            _ => None,
        });
    let mut sections = vec![Section {
        label: "Actions",
        entries: actions,
    }];
    if query.actions_only() {
        return if query.is_empty() {
            sections
        } else {
            search(sections, &query)
        };
    }
    let mut threads: Vec<_> = shell
        .threads
        .iter()
        .filter_map(|thread| {
            if thread.archived || (query.is_empty() && active_thread == Some(&thread.id)) {
                return None;
            }
            let project = shell
                .projects
                .iter()
                .find(|project| project.id == thread.project_id)?;
            Some((thread, project))
        })
        .collect();
    threads.sort_by_cached_key(|(thread, _)| Reverse(thread.latest_user_activity()));
    let thread_entries = |limit| {
        threads
            .iter()
            .take(limit)
            .map(|(thread, project)| Choice::Thread(thread, project))
            .collect()
    };
    if query.is_empty() {
        if !threads.is_empty() {
            sections.push(Section {
                label: "Recent Threads",
                entries: thread_entries(12),
            });
        }
        return sections;
    }
    let projects: Vec<_> = ordered_projects(shell, true)
        .into_iter()
        .filter(|project| !workspace.is_scratch(project))
        .collect();
    if workspace.launch_writes && !workspace.reconnecting {
        sections.push(Section {
            label: "Suggestions",
            entries: projects
                .iter()
                .enumerate()
                .map(|(index, project)| Choice::NewIn(project, index + 1))
                .collect(),
        });
    }
    sections.push(Section {
        label: "Threads",
        entries: thread_entries(usize::MAX),
    });
    sections.push(Section {
        label: "Projects",
        entries: projects.into_iter().map(Choice::Project).collect(),
    });
    sections.push(Section {
        label: "Settings",
        entries: settings_search::items(state)
            .into_iter()
            .map(Choice::Setting)
            .collect(),
    });
    search(sections, &query)
}

fn search<'a>(sections: Vec<Section<'a>>, query: &Query) -> Vec<Section<'a>> {
    let rank = |typos| {
        sections
            .iter()
            .map(|section| {
                (
                    section.label,
                    section
                        .entries
                        .iter()
                        .filter_map(|entry| entry.rank(query, typos).map(|rank| (*entry, rank)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut ranked = rank(false);
    if !ranked
        .iter()
        .any(|(_, entries)| entries.iter().any(|(_, rank)| rank.strong))
    {
        ranked = rank(true);
    }
    ranked.retain(|(_, entries)| !entries.is_empty());
    ranked.sort_by_cached_key(|(_, entries)| {
        Reverse(
            entries
                .iter()
                .map(|(_, rank)| rank.group_class)
                .max()
                .unwrap_or_default(),
        )
    });
    let threads_match = ranked.iter().any(|(label, _)| *label == "Threads");
    ranked
        .into_iter()
        .map(|(label, mut entries)| {
            entries.sort_by_cached_key(|(entry, rank)| (entry.secondary(), Reverse(rank.score)));
            let limit = match label {
                "Threads" => 20,
                "Suggestions" => 3,
                _ if threads_match => 3,
                _ => usize::MAX,
            };
            Section {
                label,
                entries: entries
                    .into_iter()
                    .take(limit)
                    .map(|(entry, _)| entry)
                    .collect(),
            }
        })
        .collect()
}
