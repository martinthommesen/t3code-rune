use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use rune::color::oklch;
use rune::{App, AppSpec, Color, Cx, El, TextEdit, TextStyle, Tx, Waker};
use rune::{col, el, row, text, text_input};

use crate::client_settings::ClientSettings;
use crate::connection::{
    Connection, Event, GitRef, LaunchWorkspace, Message, Project, ProviderCatalog, Thread,
    ThreadAction, WorktreeSetup, worktree_setup::Phase as SetupPhase,
};

mod approval;
mod composer;
mod conversation;
mod draft_hero;
mod navigation_history;
mod project;
mod question;
mod settings;
mod sidebar;
mod sidebar_header;
mod terminal;
mod window_chrome;
mod window_placement;
mod worktree_setup;

use navigation_history::{Destination, Direction, History};

const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(22.0);
const LABEL: TextStyle = TextStyle::new(13.0, 550.0);
const SMALL: TextStyle = TextStyle::new(12.0, 450.0);
const TITLE: TextStyle = TextStyle::new(19.0, 620.0).tracking(-0.3);
const HEADING: TextStyle = TextStyle::new(27.0, 640.0).tracking(-0.55);

struct Shell {
    projects: Vec<Project>,
    threads: Vec<Thread>,
}

enum Transcript {
    Loading,
    Loaded(Vec<Message>),
    Failed(String),
}

struct Conversation {
    id: String,
    transcript: Transcript,
    expanded_messages: HashSet<String>,
    approvals: approval::Approvals,
    stop_available: bool,
    queue_worktree_available: bool,
    stop_requested_at: Option<Instant>,
    worktree_setup: Option<Box<WorktreeSetup>>,
    worktree_cancel_pending: bool,
    worktree_details_open: bool,
    worktree_elapsed_second: Option<i64>,
    worktree_background: worktree_setup::Disclosure,
}

enum ProjectView {
    Threads,
    NewThread,
    WaitingForThread(String),
    Conversation(Box<Conversation>),
}

struct ProjectSelection {
    id: String,
    view: ProjectView,
}

struct Workspace {
    label: String,
    read_only: bool,
    launch_writes: bool,
    worktree_launch: bool,
    remote_worktree_launch: bool,
    checkout_switch: bool,
    prompt_writes: bool,
    stop_writes: bool,
    native_stop: bool,
    project_writes: bool,
    filesystem_read: bool,
    named_projects: bool,
    clone_writes: bool,
    clones: Vec<crate::connection::ProjectClone>,
    terminal: bool,
    reconnecting: bool,
    shell: Option<Shell>,
    providers: Option<ProviderCatalog>,
    selection: Option<ProjectSelection>,
}

impl Workspace {
    fn queue_worktree_message(&self, thread_id: &str) -> bool {
        !self.reconnecting
            && self.selection.as_ref().is_some_and(|selection| {
                matches!(
                    &selection.view,
                    ProjectView::Conversation(conversation)
                        if conversation.id == thread_id && conversation.queue_worktree_available
                )
            })
    }

    fn stop_generation(&self, thread_id: &str) -> Option<bool> {
        if !self.stop_writes || self.reconnecting {
            return None;
        }
        let ProjectView::Conversation(conversation) = &self.selection.as_ref()?.view else {
            return None;
        };
        if conversation.id != thread_id {
            return None;
        }
        let thread = self.thread(thread_id)?;
        let available = !thread.archived
            && if self.native_stop {
                conversation.stop_available
            } else {
                matches!(
                    thread.status.as_str(),
                    "preparing" | "starting" | "running" | "waiting"
                )
            };
        available.then(|| {
            conversation
                .stop_requested_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(10))
        })
    }

    fn thread(&self, id: &str) -> Option<&Thread> {
        self.shell
            .as_ref()?
            .threads
            .iter()
            .find(|thread| thread.id == id)
    }

    fn destination(&self) -> Destination {
        let Some(ProjectSelection { id, view }) = &self.selection else {
            return Destination::Home;
        };
        match view {
            ProjectView::Threads => Destination::Project(id.clone()),
            ProjectView::Conversation(conversation) => Destination::Thread {
                project: id.clone(),
                thread: conversation.id.clone(),
            },
            ProjectView::NewThread | ProjectView::WaitingForThread(_) => {
                Destination::NewThread(id.clone())
            }
        }
    }

    fn can_visit(&self, destination: &Destination) -> bool {
        if let Some(project) = destination.project()
            && !self
                .shell
                .as_ref()
                .is_some_and(|shell| shell.projects.iter().any(|entry| entry.id == project))
        {
            return false;
        }
        match destination {
            Destination::Thread { project, thread } => self
                .thread(thread)
                .is_some_and(|entry| entry.project_id == *project),
            Destination::NewThread(_) => self.launch_writes,
            Destination::Home | Destination::Project(_) => true,
        }
    }
}

#[derive(Default)]
struct Draft {
    edit: TextEdit,
    pending: bool,
    failure: Option<String>,
    workspace_mode: WorkspaceMode,
    base_ref: Option<String>,
    start_from_origin: bool,
    start_from_origin_default: Option<bool>,
    start_from_origin_changed: bool,
    refs: DraftRefs,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum WorkspaceMode {
    #[default]
    Local,
    Worktree,
}

#[derive(Default)]
struct DraftRefs {
    branches: Vec<GitRef>,
    is_repo: Option<bool>,
    loading: bool,
    refresh_again: bool,
    switching: bool,
    error: Option<String>,
}

struct State {
    sidebar_open: bool,
    sidebar: sidebar::SidebarState,
    history: History,
    url: TextEdit,
    pairing_code: TextEdit,
    wake: Option<Waker>,
    connection: Option<Connection>,
    connection_error: Option<String>,
    workspace: Option<Workspace>,
    draft_environment: Option<String>,
    drafts: HashMap<String, Draft>,
    new_thread_drafts: HashMap<String, Draft>,
    questions: HashMap<String, question::Questions>,
    question_auto_advance: Option<question::AutoAdvance>,
    draft_workspace_popup: Option<draft_hero::WorkspacePopup>,
    add_project: Option<project::AddProject>,
    added_project: Option<project::AddedProject>,
    project_notice: Option<String>,
    client_settings: ClientSettings,
    settings: settings::Panel,
    terminal: Option<terminal::Pane>,
    terminal_tabs: terminal::TabsByThread,
    window_focused: bool,
    window_chrome: window_chrome::ChromeState,
    window_placement: window_placement::Tracker,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            sidebar: sidebar::SidebarState::default(),
            history: History::default(),
            url: TextEdit::new("http://127.0.0.1:3773"),
            pairing_code: TextEdit::new("").secure(),
            wake: None,
            connection: None,
            connection_error: None,
            workspace: None,
            draft_environment: None,
            drafts: HashMap::new(),
            new_thread_drafts: HashMap::new(),
            questions: HashMap::new(),
            question_auto_advance: None,
            draft_workspace_popup: None,
            add_project: None,
            added_project: None,
            project_notice: None,
            client_settings: ClientSettings::default(),
            settings: settings::Panel::default(),
            terminal: None,
            terminal_tabs: terminal::TabsByThread::default(),
            window_focused: true,
            window_chrome: window_chrome::ChromeState::default(),
            window_placement: window_placement::Tracker::default(),
        }
    }
}

impl State {
    fn can_connect(&self) -> bool {
        self.connection.is_none()
            && !self.url.text.trim().is_empty()
            && !self.pairing_code.text.trim().is_empty()
    }

    fn connect(&mut self) {
        if !self.can_connect() {
            return;
        }
        let Some(wake) = self.wake.clone() else {
            return;
        };
        self.sidebar = sidebar::SidebarState::default();
        self.connection_error = None;
        self.connection = Some(Connection::start(
            self.url.text.trim().into(),
            self.pairing_code.text.trim().into(),
            move || wake(),
        ));
    }

    fn disconnect(&mut self) {
        self.sidebar = sidebar::SidebarState::default();
        self.history = History::default();
        self.connection = None;
        self.workspace = None;
        self.draft_environment = None;
        self.drafts.clear();
        self.new_thread_drafts.clear();
        self.questions.clear();
        self.question_auto_advance = None;
        self.add_project = None;
        self.added_project = None;
        self.terminal = None;
    }

    fn select_project(&mut self, id: &str) {
        if let Some(workspace) = &self.workspace
            && workspace
                .selection
                .as_ref()
                .is_none_or(|project| project.id != id)
        {
            self.navigate(Destination::Project(id.into()));
        }
    }

    fn open_thread(&mut self, id: &str) {
        if let Some(thread) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.thread(id))
        {
            self.navigate(Destination::Thread {
                project: thread.project_id.clone(),
                thread: id.into(),
            });
        }
    }

    fn cancel_new_thread(&mut self) {
        if let Some(ProjectSelection {
            id,
            view: ProjectView::NewThread,
        }) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.selection.as_ref())
        {
            self.navigate(Destination::Project(id.clone()));
        }
    }

    fn can_go(&self, direction: Direction) -> bool {
        self.workspace.as_ref().is_some_and(|workspace| {
            let current = workspace.destination();
            self.history.available(direction, |destination| {
                *destination != current && workspace.can_visit(destination)
            })
        })
    }

    fn go(&mut self, direction: Direction) {
        let Some(workspace) = &self.workspace else {
            return;
        };
        if let Some(destination) =
            self.history
                .go(direction, workspace.destination(), |destination| {
                    workspace.can_visit(destination)
                })
        {
            self.show_destination(destination);
        }
    }

    fn navigate(&mut self, destination: Destination) {
        let Some(previous) = self.workspace.as_ref().map(Workspace::destination) else {
            return;
        };
        let changed = previous != destination;
        if self.show_destination(destination) && changed {
            self.history.record(previous);
        }
    }

    fn show_destination(&mut self, destination: Destination) -> bool {
        let Some(workspace) = &mut self.workspace else {
            return false;
        };
        if !workspace.can_visit(&destination) {
            return false;
        }
        if workspace.destination() == destination
            && !matches!(
                workspace.selection.as_ref().map(|project| &project.view),
                Some(ProjectView::Conversation(conversation))
                    if matches!(conversation.transcript, Transcript::Failed(_))
            )
        {
            return false;
        }
        let has_thread = matches!(
            workspace.selection.as_ref().map(|project| &project.view),
            Some(ProjectView::Conversation(_))
        );
        self.question_auto_advance = None;
        sidebar::close_draft_project_picker(&mut self.sidebar);
        self.draft_workspace_popup = None;
        if let Some(terminal) = self.terminal.take()
            && let Some(connection) = &self.connection
        {
            connection.detach_terminal(terminal.attachment);
        }
        match &destination {
            Destination::Thread { thread, .. } => {
                self.drafts.entry(thread.clone()).or_default();
                if let Some(questions) = self.questions.get_mut(thread) {
                    questions.pause();
                }
                if let Some(connection) = &self.connection {
                    connection.open_thread(thread.clone());
                }
            }
            _ => {
                if has_thread && let Some(connection) = &self.connection {
                    connection.close_thread();
                }
            }
        }
        let new_draft = match &destination {
            Destination::NewThread(id) => Some(id.clone()),
            _ => None,
        };
        workspace.selection = match destination {
            Destination::Home => None,
            Destination::Project(id) => Some(ProjectSelection {
                id,
                view: ProjectView::Threads,
            }),
            Destination::Thread { project, thread } => Some(ProjectSelection {
                id: project,
                view: ProjectView::Conversation(Box::new(Conversation {
                    id: thread,
                    transcript: Transcript::Loading,
                    expanded_messages: HashSet::new(),
                    approvals: approval::Approvals::default(),
                    stop_available: false,
                    queue_worktree_available: false,
                    stop_requested_at: None,
                    worktree_setup: None,
                    worktree_cancel_pending: false,
                    worktree_details_open: false,
                    worktree_elapsed_second: None,
                    worktree_background: worktree_setup::Disclosure::default(),
                })),
            }),
            Destination::NewThread(id) => {
                self.new_thread_drafts.entry(id.clone()).or_default();
                Some(ProjectSelection {
                    id,
                    view: ProjectView::NewThread,
                })
            }
        };
        if let Some(project_id) = new_draft {
            self.load_draft_refs(&project_id, false);
        }
        true
    }

    fn load_draft_refs(&mut self, project_id: &str, refresh: bool) {
        let Some(project) = self
            .workspace
            .as_ref()
            .filter(|workspace| {
                workspace.worktree_launch
                    && !workspace.reconnecting
                    && workspace.pending_clone(project_id).is_none()
            })
            .and_then(|workspace| workspace.shell.as_ref())
            .and_then(|shell| {
                shell
                    .projects
                    .iter()
                    .find(|project| project.id == project_id)
            })
        else {
            return;
        };
        let Some(connection) = &self.connection else {
            return;
        };
        let Some(draft) = self
            .new_thread_drafts
            .get_mut(project_id)
            .filter(|draft| (refresh || draft.refs.is_repo.is_none()) && !draft.pending)
        else {
            return;
        };
        if draft.refs.loading {
            draft.refs.refresh_again |= refresh;
            return;
        }
        draft.refs.loading = true;
        draft.refs.error = None;
        connection.list_git_refs(project_id.to_owned(), project.path.clone(), refresh);
    }

    fn set_draft_workspace(&mut self, project_id: &str, mode: WorkspaceMode) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.worktree_launch && !workspace.reconnecting)
        else {
            return;
        };
        if let Some(draft) = self
            .new_thread_drafts
            .get_mut(project_id)
            .filter(|draft| !draft.pending)
        {
            if mode == WorkspaceMode::Worktree && draft.refs.is_repo == Some(false) {
                return;
            }
            draft.workspace_mode = mode;
            draft.start_from_origin = mode == WorkspaceMode::Worktree
                && workspace.remote_worktree_launch
                && draft.start_from_origin_default.unwrap_or(true);
            draft.start_from_origin_changed = false;
            if mode == WorkspaceMode::Worktree {
                self.load_draft_refs(project_id, workspace.remote_worktree_launch);
            }
        }
    }

    fn set_draft_base_ref(&mut self, project_id: &str, name: &str) {
        let remote = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.remote_worktree_launch);
        if let Some(draft) = self.new_thread_drafts.get_mut(project_id).filter(|draft| {
            !draft.pending
                && draft.workspace_mode == WorkspaceMode::Worktree
                && draft
                    .refs
                    .branches
                    .iter()
                    .any(|branch| branch.name == name && (remote || !branch.is_remote))
        }) {
            draft.base_ref = Some(name.to_owned());
        }
    }

    fn change_draft_checkout(&mut self, project_id: &str, name: &str, create: bool) {
        let Some(project) = self
            .workspace
            .as_ref()
            .filter(|workspace| {
                workspace.checkout_switch
                    && !workspace.reconnecting
                    && workspace.pending_clone(project_id).is_none()
            })
            .and_then(|workspace| workspace.shell.as_ref())
            .and_then(|shell| {
                shell
                    .projects
                    .iter()
                    .find(|project| project.id == project_id)
            })
        else {
            return;
        };
        let Some(connection) = &self.connection else {
            return;
        };
        let Some(draft) = self.new_thread_drafts.get_mut(project_id).filter(|draft| {
            !draft.pending
                && !draft.refs.switching
                && draft.workspace_mode == WorkspaceMode::Local
                && draft.refs.is_repo == Some(true)
        }) else {
            return;
        };
        let matching = draft.refs.branches.iter().find(|entry| entry.name == name);
        let unavailable = if create {
            matching.is_some()
        } else {
            matching.is_none_or(|entry| entry.current)
        };
        if name.is_empty() || unavailable {
            return;
        }
        draft.refs.switching = true;
        connection.change_git_checkout(
            project_id.to_owned(),
            project.path.clone(),
            name.to_owned(),
            create,
        );
    }

    fn set_draft_start_from_origin(&mut self, project_id: &str, enabled: bool) {
        if self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.remote_worktree_launch && !workspace.reconnecting)
            && let Some(draft) = self.new_thread_drafts.get_mut(project_id)
            && !draft.pending
            && draft.workspace_mode == WorkspaceMode::Worktree
        {
            draft.start_from_origin = enabled;
            draft.start_from_origin_changed = true;
        }
    }

    fn start_thread(&mut self, _: &mut Tx) {
        let Some(ProjectSelection {
            id,
            view: ProjectView::NewThread,
        }) = self
            .workspace
            .as_ref()
            .filter(|workspace| {
                workspace.launch_writes
                    && !workspace.reconnecting
                    && !composer::provider_unavailable(workspace, None)
            })
            .and_then(|workspace| workspace.selection.as_ref())
        else {
            return;
        };
        if self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.pending_clone(id).is_some())
        {
            return;
        }
        let Some(connection) = &self.connection else {
            return;
        };
        let remote = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.remote_worktree_launch);
        let draft = self
            .new_thread_drafts
            .get_mut(id)
            .expect("a new thread form has a draft");
        if draft.pending || draft.refs.switching || draft.edit.text.trim().is_empty() {
            return;
        }
        let workspace =
            match draft.workspace_mode {
                WorkspaceMode::Local => LaunchWorkspace::Root,
                WorkspaceMode::Worktree => {
                    let Some(base_ref) = draft.base_ref.clone().filter(|base_ref| {
                        draft.refs.is_repo == Some(true)
                            && draft.refs.branches.iter().any(|branch| {
                                (remote || !branch.is_remote) && branch.name == *base_ref
                            })
                    }) else {
                        return;
                    };
                    LaunchWorkspace::Worktree {
                        base_ref,
                        start_from_origin: remote && draft.start_from_origin,
                    }
                }
            };
        draft.pending = true;
        draft.failure = None;
        connection.launch_thread(id.clone(), draft.edit.text.clone(), workspace);
    }

    fn stop_generation(&mut self, thread_id: &str) {
        if self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.stop_generation(thread_id))
            != Some(false)
        {
            return;
        }
        let Some(connection) = &self.connection else {
            return;
        };
        connection.stop_thread(thread_id.to_owned());
        if let Some(conversation) = self.conversation_mut(thread_id) {
            conversation.stop_requested_at = Some(Instant::now());
        }
    }

    fn cancel_worktree_setup(&mut self, thread_id: &str) {
        if self.connection.is_none()
            || self
                .workspace
                .as_ref()
                .is_none_or(|workspace| !workspace.worktree_launch || workspace.reconnecting)
        {
            return;
        }
        let Some(conversation) = self.conversation_mut(thread_id).filter(|conversation| {
            !conversation.worktree_cancel_pending
                && conversation
                    .worktree_setup
                    .as_ref()
                    .is_some_and(|setup| setup.phase == SetupPhase::Running)
        }) else {
            return;
        };
        conversation.worktree_cancel_pending = true;
        if let Some(connection) = &self.connection {
            connection.cancel_worktree_setup(thread_id.to_owned());
        }
    }

    fn send_prompt(&mut self, _: &mut Tx) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.prompt_writes && !workspace.reconnecting)
        else {
            return;
        };
        let Some(ProjectSelection {
            view: ProjectView::Conversation(conversation),
            ..
        }) = &workspace.selection
        else {
            return;
        };
        if !matches!(conversation.transcript, Transcript::Loaded(_)) {
            return;
        }
        let id = &conversation.id;
        let Some(thread) = workspace.thread(id).filter(|thread| !thread.archived) else {
            return;
        };
        if workspace.pending_clone(&thread.project_id).is_some()
            || composer::provider_unavailable(workspace, thread.providers.current_instance())
        {
            return;
        }
        let Some(connection) = &self.connection else {
            return;
        };
        let draft = self.drafts.get_mut(id).expect("an open thread has a draft");
        if draft.pending || draft.edit.text.trim().is_empty() {
            return;
        }
        draft.pending = true;
        draft.failure = None;
        connection.send_prompt(id.clone(), draft.edit.text.clone());
    }

    fn poll(&mut self, clock: f64, tx: &mut Tx) -> bool {
        self.window_placement.poll(tx);
        self.update_terminal_focus(tx);
        let mut changed = self.window_chrome.poll(tx);
        while let Some(event) = self.connection.as_ref().and_then(Connection::try_recv) {
            self.apply_event(event, tx);
            changed = true;
        }
        changed |= self.poll_project_browse();
        changed |= self.poll_question(clock);
        changed |= sidebar::poll_rename(self);
        changed |= worktree_setup::poll(self, tx);
        if self.sidebar_open
            && self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.shell.as_ref())
                .is_some_and(|shell| !shell.threads.is_empty())
        {
            changed |= self.sidebar.poll(tx);
        }
        changed
    }

    fn apply_event(&mut self, event: Event, tx: &mut Tx) {
        match event {
            Event::Connected {
                environment_id,
                label,
                read_only,
                launch_writes,
                worktree_launch,
                remote_worktree_launch,
                checkout_switch,
                prompt_writes,
                stop_writes,
                native_stop,
                project_writes,
                filesystem_read,
                named_projects,
                clone_writes,
                terminal,
            } => {
                self.history = History::default();
                self.pairing_code.clear();
                if self.draft_environment.as_deref() != Some(&environment_id) {
                    self.drafts.clear();
                    self.new_thread_drafts.clear();
                    self.questions.clear();
                    self.question_auto_advance = None;
                    self.terminal_tabs.clear();
                    self.draft_environment = Some(environment_id);
                }
                self.workspace = Some(Workspace {
                    label,
                    read_only,
                    launch_writes,
                    worktree_launch,
                    remote_worktree_launch,
                    checkout_switch,
                    prompt_writes,
                    stop_writes,
                    native_stop,
                    project_writes,
                    filesystem_read,
                    named_projects,
                    clone_writes,
                    clones: Vec::new(),
                    terminal,
                    reconnecting: false,
                    shell: None,
                    providers: None,
                    selection: None,
                });
            }
            Event::Shell { projects, threads } => {
                self.update_shell(Shell { projects, threads });
                self.select_added_project();
            }
            Event::Providers(providers) => {
                if let Some(workspace) = &mut self.workspace {
                    workspace.providers = Some(providers);
                }
            }
            Event::FolderBrowsed { browse_id, result } => self.folder_browsed(browse_id, result),
            Event::Thread {
                thread_id,
                messages,
            } => {
                if let Some(conversation) = self.conversation_mut(&thread_id) {
                    if matches!(conversation.transcript, Transcript::Loading) {
                        tx.scroll_to_bottom(format!("transcript-{thread_id}"));
                    }
                    conversation.transcript = Transcript::Loaded(messages);
                }
            }
            Event::ThreadApprovals {
                thread_id,
                approvals,
                can_respond,
            } => {
                if let Some(conversation) = self.conversation_mut(&thread_id) {
                    conversation.approvals.replace(approvals, can_respond);
                }
            }
            Event::ApprovalResponded { response, outcome } => {
                if let Some(conversation) = self.conversation_mut(&response.thread_id) {
                    conversation.approvals.finish(response, outcome);
                }
            }
            Event::ThreadQuestions {
                thread_id,
                questions,
                can_respond,
            } => {
                if questions.is_empty() {
                    self.questions.remove(&thread_id);
                } else {
                    self.questions
                        .entry(thread_id)
                        .or_default()
                        .replace(questions, can_respond);
                }
            }
            Event::QuestionResponded { response, outcome } => {
                if let Some(questions) = self.questions.get_mut(&response.thread_id) {
                    questions.finish(response, outcome);
                }
            }
            Event::PromptAccepted { thread_id } => {
                if let Some(draft) = self.drafts.get_mut(&thread_id) {
                    draft.pending = false;
                    draft.edit.clear();
                }
            }
            Event::ThreadLaunched {
                project_id,
                thread_id,
            } => {
                self.new_thread_drafts.remove(&project_id);
                let Some(workspace) = &mut self.workspace else {
                    return;
                };
                if let Some(project) = &mut workspace.selection
                    && project.id == project_id
                    && matches!(project.view, ProjectView::NewThread)
                {
                    project.view = ProjectView::WaitingForThread(thread_id);
                    self.open_confirmed_thread();
                }
            }
            Event::LaunchError {
                project_id,
                message,
            } => {
                if let Some(draft) = self.new_thread_drafts.get_mut(&project_id) {
                    draft.pending = false;
                    draft.failure = Some(message);
                }
            }
            Event::ProjectAdded { command_id, result } => self.project_added(&command_id, result),
            Event::GitRefs { project_id, result } => {
                let remote = self
                    .workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.remote_worktree_launch);
                if let Some(draft) = self.new_thread_drafts.get_mut(&project_id) {
                    draft.refs.loading = false;
                    if std::mem::take(&mut draft.refs.refresh_again) {
                        self.load_draft_refs(&project_id, true);
                        return;
                    }
                    match result {
                        Ok(refs) => {
                            draft.refs.is_repo = Some(refs.is_repo);
                            draft.refs.branches = refs.branches;
                            let branches = || {
                                draft
                                    .refs
                                    .branches
                                    .iter()
                                    .filter(|branch| remote || !branch.is_remote)
                            };
                            if !refs.is_repo {
                                draft.workspace_mode = WorkspaceMode::Local;
                                draft.base_ref = None;
                                draft.start_from_origin = false;
                            } else if draft
                                .base_ref
                                .as_ref()
                                .is_none_or(|base| !branches().any(|branch| branch.name == *base))
                            {
                                draft.base_ref = branches()
                                    .find(|branch| branch.is_default)
                                    .or_else(|| branches().find(|branch| branch.current))
                                    .map(|branch| branch.name.clone());
                            }
                        }
                        Err(message) => draft.refs.error = Some(message),
                    }
                }
            }
            Event::GitCheckout { project_id, result } => {
                if let Some(draft) = self.new_thread_drafts.get_mut(&project_id) {
                    draft.refs.switching = false;
                    match result {
                        Ok(name) => {
                            for branch in &mut draft.refs.branches {
                                branch.current = !branch.is_remote
                                    && name.as_deref() == Some(branch.name.as_str());
                            }
                        }
                        Err(message) => self.project_notice = Some(message),
                    }
                    self.load_draft_refs(&project_id, true);
                }
            }
            Event::DraftWorkspaceDefault {
                project_id,
                start_from_origin,
            } => {
                if let Some(draft) = self.new_thread_drafts.get_mut(&project_id) {
                    draft.start_from_origin_default = Some(start_from_origin);
                    if draft.workspace_mode == WorkspaceMode::Worktree
                        && !draft.start_from_origin_changed
                    {
                        draft.start_from_origin = start_from_origin;
                    }
                }
            }
            Event::WorktreeSetup {
                thread_id,
                snapshot,
            } => {
                if let Some(conversation) = self.conversation_mut(&thread_id)
                    && let Some(snapshot) = snapshot
                    && conversation.worktree_setup.as_ref().is_none_or(|previous| {
                        previous.started_at != snapshot.started_at
                            || previous.sequence <= snapshot.sequence
                    })
                {
                    if snapshot.phase != SetupPhase::Running {
                        conversation.worktree_cancel_pending = false;
                    }
                    conversation.worktree_setup = Some(snapshot);
                }
            }
            Event::WorktreeSetupCancelSettled { thread_id, result } => {
                if let Some(conversation) = self.conversation_mut(&thread_id) {
                    conversation.worktree_cancel_pending = false;
                    if let Err(message) = result {
                        self.project_notice = Some(message);
                    }
                }
            }
            Event::ThreadActions {
                thread_id,
                stop,
                queue_worktree,
            } => {
                if let Some(conversation) = self.conversation_mut(&thread_id) {
                    conversation.stop_available = stop;
                    conversation.queue_worktree_available = queue_worktree;
                    if !stop {
                        conversation.stop_requested_at = None;
                    }
                }
            }
            Event::ThreadStopFailed { thread_id, message } => {
                if let Some(conversation) = self.conversation_mut(&thread_id) {
                    conversation.stop_requested_at = None;
                    self.project_notice = Some(message);
                }
            }
            Event::ProjectClones(clones) => {
                if let Some(workspace) = &mut self.workspace {
                    let selected = workspace
                        .selection
                        .as_ref()
                        .filter(|project| {
                            matches!(project.view, ProjectView::NewThread)
                                && workspace.pending_clone(&project.id).is_some()
                        })
                        .map(|project| project.id.clone());
                    workspace.clones = clones;
                    if let Some(project_id) =
                        selected.filter(|id| workspace.pending_clone(id).is_none())
                    {
                        if let Some(draft) = self.new_thread_drafts.get_mut(&project_id) {
                            draft.refs.is_repo = None;
                        }
                        self.load_draft_refs(&project_id, true);
                    }
                }
            }
            Event::ProjectCloneError(message) => self.project_notice = Some(message),
            Event::Terminal { attachment, update } => {
                if let Some(terminal) = self
                    .terminal
                    .as_mut()
                    .filter(|terminal| terminal.attachment == attachment)
                {
                    terminal.apply(update);
                    self.send_terminal_replies();
                }
            }
            Event::TerminalFailed {
                attachment,
                message,
            } => {
                if let Some(terminal) = self
                    .terminal
                    .as_mut()
                    .filter(|terminal| terminal.attachment == attachment)
                {
                    terminal.fail(message);
                }
            }
            Event::TerminalCloseSettled { target, result } => {
                self.terminal_close_settled(target, result);
            }
            Event::RequestError {
                thread_id,
                action: ThreadAction::Open,
                message,
            } => {
                if let Some(conversation) = self.conversation_mut(&thread_id) {
                    conversation.transcript = Transcript::Failed(message);
                }
            }
            Event::RequestError {
                thread_id,
                action: ThreadAction::Send,
                message,
            } => {
                if let Some(draft) = self.drafts.get_mut(&thread_id) {
                    draft.pending = false;
                    draft.failure = Some(message);
                }
            }
            Event::RequestError {
                action: ThreadAction::Rename,
                message,
                ..
            } => self.project_notice = Some(message),
            event @ (Event::Reconnecting | Event::Reconnected) => {
                self.question_auto_advance = None;
                for questions in self.questions.values_mut() {
                    questions.pause();
                }
                let mut draft = None;
                if let Some(workspace) = &mut self.workspace {
                    workspace.reconnecting = matches!(event, Event::Reconnecting);
                    if !workspace.reconnecting
                        && let Some(ProjectSelection { id, view }) = &mut workspace.selection
                    {
                        match view {
                            ProjectView::Conversation(conversation) => {
                                conversation.approvals.await_sync()
                            }
                            ProjectView::NewThread => draft = Some(id.clone()),
                            _ => {}
                        }
                    }
                }
                if let Some(project_id) = draft {
                    self.load_draft_refs(&project_id, true);
                }
            }
            Event::Disconnected(reason) => {
                sidebar::cancel_rename(self);
                self.connection = None;
                self.workspace = None;
                self.question_auto_advance = None;
                self.connection_error = Some(reason);
                self.add_project = None;
                self.added_project = None;
                self.terminal = None;
                for draft in self.drafts.values_mut().filter(|draft| draft.pending) {
                    draft.pending = false;
                    draft.failure = Some(
                        "Delivery is uncertain. Check the conversation before sending again."
                            .into(),
                    );
                }
                for draft in self
                    .new_thread_drafts
                    .values_mut()
                    .filter(|draft| draft.pending)
                {
                    draft.pending = false;
                    draft.failure = Some(
                        "The thread may have started. Check the thread list before trying again."
                            .into(),
                    );
                }
            }
        }
    }

    fn update_shell(&mut self, shell: Shell) {
        let Some(workspace) = &mut self.workspace else {
            return;
        };
        if let Some(project) = &mut workspace.selection {
            let project_present = shell.projects.iter().any(|entry| entry.id == project.id);
            if let ProjectView::Conversation(conversation) = &project.view
                && (!project_present
                    || !shell
                        .threads
                        .iter()
                        .any(|thread| thread.id == conversation.id))
            {
                project.view = ProjectView::Threads;
                if let Some(connection) = &self.connection {
                    connection.close_thread();
                }
            }
            if !project_present {
                workspace.selection = None;
            }
        }
        workspace.shell = Some(shell);
        if let Some(terminal) = &self.terminal
            && !workspace.thread(&terminal.thread_id).is_some_and(|thread| matches!(workspace.selection.as_ref().map(|project| &project.view), Some(ProjectView::Conversation(conversation)) if conversation.id == thread.id))
        {
            self.detach_terminal_pane();
        }
        self.open_confirmed_thread();
    }

    fn open_confirmed_thread(&mut self) {
        let Some(workspace) = &self.workspace else {
            return;
        };
        if let Some(ProjectSelection {
            id,
            view: ProjectView::WaitingForThread(thread_id),
        }) = &workspace.selection
            && workspace
                .thread(thread_id)
                .is_some_and(|thread| thread.project_id == *id)
        {
            self.show_destination(Destination::Thread {
                project: id.clone(),
                thread: thread_id.clone(),
            });
        }
    }

    fn conversation_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        match &mut self.workspace.as_mut()?.selection.as_mut()?.view {
            ProjectView::Conversation(conversation) if conversation.id == id => Some(conversation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    canvas: Color,
    sidebar: Color,
    field: Color,
    hover: Color,
    selected: Color,
    line: Color,
    ink: Color,
    muted: Color,
    accent: Color,
    on_accent: Color,
    error: Color,
}

impl Palette {
    fn new(dark: bool) -> Self {
        Self {
            dark,
            canvas: oklch(if dark { 0.166 } else { 0.993 }, 0.006, 265.0),
            sidebar: oklch(if dark { 0.205 } else { 0.966 }, 0.007, 265.0),
            field: oklch(if dark { 0.236 } else { 1.0 }, 0.009, 265.0),
            hover: oklch(if dark { 0.28 } else { 0.927 }, 0.012, 265.0),
            selected: oklch(if dark { 0.31 } else { 0.895 }, 0.038, 257.0),
            line: oklch(if dark { 0.31 } else { 0.85 }, 0.011, 265.0),
            ink: oklch(if dark { 0.95 } else { 0.21 }, 0.011, 265.0),
            muted: oklch(if dark { 0.72 } else { 0.446 }, 0.019, 265.0),
            accent: oklch(if dark { 0.76 } else { 0.43 }, 0.13, 253.0),
            on_accent: if dark { Color::BLACK } else { Color::WHITE },
            error: oklch(if dark { 0.76 } else { 0.45 }, 0.16, 24.0),
        }
    }
}

fn view(state: &State, cx: &mut Cx<State>) -> El {
    let palette = Palette::new(
        state
            .client_settings
            .appearance_mode()
            .is_dark(cx.system_dark()),
    );
    let chrome = cx.chrome_insets();
    let chrome_layout = sidebar_header::Layout::new(chrome);
    let pane_size = cx.layout_size("detail-pane").unwrap_or([864.0, 740.0]);
    let detail_layout = conversation::Layout {
        header_height: chrome_layout.height(),
        header_left: if state.sidebar_open {
            20.0
        } else {
            chrome_layout.content_left()
        },
        native_trailing: chrome.trailing.max(state.window_chrome.trailing_inset()),
        pane_width: pane_size[0],
        viewport: [
            pane_size[0]
                + if state.sidebar_open {
                    sidebar_header::WIDTH
                } else {
                    0.0
                },
            pane_size[1],
        ],
    };
    let content = match &state.workspace {
        Some(workspace) => workspace_view(
            state,
            workspace,
            palette,
            chrome_layout,
            cx.layout_size("sidebar-contents")
                .map_or(645.0, |size| size[1]),
            detail_layout,
        ),
        None => connection_view(state, palette),
    };
    let width = cx
        .layout_size("t3-code-window")
        .map_or(1100.0, |size| size[0]);
    let modal = state
        .terminal_close_dialog(palette)
        .or_else(|| settings::overlay(state, palette, width));
    let modal_open = modal.is_some();
    let mut body = col()
        .key("t3-code-window")
        .w_full()
        .h_full()
        .fill(palette.canvas)
        .child(if modal_open { content.inert() } else { content })
        .on_paste(State::paste_terminal)
        .on_window_focus(State::terminal_window_focus)
        .on_key(move |state: &mut State, event, tx| {
            if modal_open {
                return false;
            }
            if event.is_command('j')
                && !event.mods.shift
                && state
                    .workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.terminal)
            {
                state.toggle_terminal(tx);
                true
            } else {
                state.question_shortcut(event, tx)
            }
        });
    if !modal_open && let Some(message) = &state.project_notice {
        body = body.child(
            col()
                .absolute(12.0, 48.0)
                .w(320.0)
                .padding(12.0)
                .gap(8.0)
                .radius(8.0)
                .fill(palette.canvas)
                .border(1.0, palette.line)
                .role("status")
                .child(text(message).font(SMALL).color(palette.ink).wrap())
                .child(button(
                    "dismiss-project-notice",
                    "Dismiss",
                    palette,
                    false,
                    true,
                    |state, _| state.project_notice = None,
                )),
        );
    }
    if !modal_open
        && state
            .workspace
            .as_ref()
            .is_none_or(|workspace| workspace.selection.is_none())
    {
        let left = if state.workspace.is_none() {
            0.0
        } else if state.sidebar_open {
            sidebar_header::WIDTH
        } else {
            chrome_layout.content_left()
        };
        if let Some(band) = state.window_chrome.drag_band(left) {
            body = body.child(band);
        }
    }
    if let Some(dialog) = modal {
        body = body.child(dialog);
    } else if let Some(form) = &state.add_project {
        body = body.child(project::dialog(form, palette));
    } else if let Some(overlays) = worktree_setup::overlay(state, palette, width) {
        body = body.children(overlays);
    }
    if let Some(caption) = state.window_chrome.overlay(palette.dark) {
        body = body.child(caption);
    }
    body
}

fn connection_view(state: &State, palette: Palette) -> El {
    let connecting = state.connection.is_some();
    let mut form = col()
        .w_full()
        .max_w(440.0)
        .gap(18.0)
        .child(
            text("Connect to T3 Code")
                .font(HEADING)
                .color(palette.ink)
                .role("heading"),
        )
        .child(
            text("Enter your server address and a pairing code from that environment.")
                .font(BODY)
                .color(palette.muted)
                .wrap(),
        )
        .child(form_field(
            "Server address",
            input(
                "server-address",
                "http://127.0.0.1:3773",
                &state.url,
                palette,
            )
            .single_line()
            .on_edit(|state: &mut State| &mut state.url)
            .on_submit(|state: &mut State, _| state.connect())
            .disabled(connecting),
            palette,
        ))
        .child(form_field(
            "Pairing code",
            input(
                "pairing-code",
                "Enter a pairing code",
                &state.pairing_code,
                palette,
            )
            .single_line()
            .secure()
            .on_edit(|state: &mut State| &mut state.pairing_code)
            .on_submit(|state: &mut State, _| state.connect())
            .disabled(connecting),
            palette,
        ));
    if let Some(error) = &state.connection_error {
        form = form.child(error_text(error, palette));
    }
    let mut actions = row().gap(10.0).child(button(
        "connect",
        if connecting {
            "Connecting…"
        } else {
            "Connect"
        },
        palette,
        true,
        state.can_connect(),
        |state, _| state.connect(),
    ));
    if connecting {
        actions = actions.child(button(
            "cancel",
            "Cancel",
            palette,
            false,
            true,
            |state, _| state.disconnect(),
        ));
    }
    col()
        .w_full()
        .h_full()
        .center()
        .padding(24.0)
        .child(form.child(actions))
}

fn workspace_view(
    state: &State,
    workspace: &Workspace,
    palette: Palette,
    layout: sidebar_header::Layout,
    sidebar_height: f32,
    detail_layout: conversation::Layout,
) -> El {
    let colors = sidebar_header::Colors::new(palette.dark);
    let frame = row().w_full().h_full();
    if !state.sidebar_open {
        return frame
            .child(detail(state, workspace, palette, detail_layout))
            .child(sidebar_header::toggle(state, layout, colors));
    }
    let status = if workspace.reconnecting {
        "Reconnecting…"
    } else {
        workspace.label.as_str()
    };
    let side = col()
        .w(sidebar_header::WIDTH)
        .h_full()
        .min_h(0.0)
        .fill(colors.background)
        .border_sides([0.0, 1.0, 0.0, 0.0], colors.border)
        .border_layout()
        .child(sidebar_header::view(state, layout, colors))
        .child(sidebar::view(
            state,
            workspace,
            colors,
            palette.dark,
            layout,
            sidebar_height,
        ))
        .child(
            row()
                .px(12.0)
                .py(10.0)
                .gap(8.0)
                .center_y()
                .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
                .child(settings::opener(palette))
                .child(button(
                    "disconnect",
                    "Disconnect",
                    palette,
                    false,
                    true,
                    |state, _| state.disconnect(),
                ))
                .child(
                    text(status)
                        .font(SMALL)
                        .color(palette.muted)
                        .role("status")
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                ),
        );
    frame
        .child(side)
        .child(detail(state, workspace, palette, detail_layout))
}

fn detail(
    state: &State,
    workspace: &Workspace,
    palette: Palette,
    layout: conversation::Layout,
) -> El {
    let colors = conversation::Colors::new(palette);
    let pane = col()
        .key("detail-pane")
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .fill(colors.background);
    let Some(shell) = &workspace.shell else {
        return pane.child(placeholder("Loading environment…", "", palette));
    };
    let Some((selection, project)) = workspace.selection.as_ref().and_then(|selection| {
        shell
            .projects
            .iter()
            .find(|project| project.id == selection.id)
            .map(|project| (selection, project))
    }) else {
        return pane.child(placeholder(
            if shell.projects.is_empty() {
                "No projects yet"
            } else {
                "Select a project"
            },
            if shell.projects.is_empty() {
                "Add a project in your T3 Code environment to see it here."
            } else {
                "Open a project in the sidebar to browse its threads."
            },
            palette,
        ));
    };
    let conversation = match &selection.view {
        ProjectView::Threads => {
            return pane
                .child(conversation::header(
                    state, workspace, project, None, layout, colors,
                ))
                .child(placeholder(
                    "Select a thread",
                    if workspace.launch_writes {
                        "Open a thread or start a new one."
                    } else {
                        "Open a thread to read its conversation."
                    },
                    palette,
                ));
        }
        ProjectView::NewThread => {
            return pane
                .child(conversation::header(
                    state, workspace, project, None, layout, colors,
                ))
                .child(composer::draft(
                    state,
                    workspace,
                    project,
                    &state.new_thread_drafts[&project.id],
                    layout,
                    palette,
                ));
        }
        ProjectView::WaitingForThread(_) => {
            return pane
                .child(conversation::header(
                    state, workspace, project, None, layout, colors,
                ))
                .child(placeholder("Opening thread…", "", palette));
        }
        ProjectView::Conversation(conversation) => conversation,
    };
    let thread = workspace.thread(&conversation.id);
    let pane = pane.child(conversation::header(
        state, workspace, project, thread, layout, colors,
    ));
    match &conversation.transcript {
        Transcript::Loading => pane.child(placeholder("Loading conversation…", "", palette)),
        Transcript::Failed(error) => {
            let id = conversation.id.clone();
            pane.child(
                col()
                    .w_full()
                    .grow(1.0)
                    .center()
                    .padding(24.0)
                    .gap(14.0)
                    .child(
                        text("Could not load the thread")
                            .font(TITLE)
                            .color(palette.ink),
                    )
                    .child(error_text(error, palette).max_w(460.0))
                    .child(button(
                        "retry-thread",
                        "Try again",
                        palette,
                        false,
                        true,
                        move |state, _| state.open_thread(&id),
                    )),
            )
        }
        Transcript::Loaded(messages) => {
            let transcript = if messages.is_empty()
                && conversation.approvals.is_empty()
                && conversation.worktree_setup.is_none()
            {
                placeholder("No messages yet", "", palette)
            } else {
                conversation::transcript(conversation, messages, layout, colors, workspace)
            };
            let pane = pane.child(transcript);
            let read_only_reason = if thread.is_some_and(|thread| thread.archived) {
                Some("This thread is archived.")
            } else if !workspace.prompt_writes {
                Some(if workspace.read_only && !workspace.project_writes {
                    "This environment is read-only."
                } else {
                    "This session cannot send prompts."
                })
            } else {
                None
            };
            let pane =
                if let Some(question) = question::view(state, &conversation.id, layout, palette) {
                    pane.child(question)
                } else if let Some(read_only_reason) = read_only_reason {
                    pane.child(
                        row()
                            .w_full()
                            .px(26.0)
                            .py(14.0)
                            .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
                            .child(text(read_only_reason).font(SMALL).color(palette.muted)),
                    )
                } else {
                    pane.child(composer::thread(
                        workspace,
                        &conversation.id,
                        &state.drafts[&conversation.id],
                        layout,
                        palette,
                    ))
                };
            if let Some(terminal) = state
                .terminal
                .as_ref()
                .filter(|terminal| terminal.thread_id == conversation.id)
            {
                pane.child(state.terminal_view(terminal, palette))
            } else {
                pane
            }
        }
    }
}

fn placeholder(title: &str, description: &str, palette: Palette) -> El {
    let mut body = col()
        .w_full()
        .max_w(350.0)
        .gap(7.0)
        .child(text(title).font(TITLE).color(palette.ink).role("heading"));
    if !description.is_empty() {
        body = body.child(text(description).font(BODY).color(palette.muted).wrap());
    }
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .center()
        .padding(26.0)
        .child(body)
}

fn error_text(message: &str, palette: Palette) -> El {
    text(message)
        .font(BODY)
        .color(palette.error)
        .wrap_anywhere()
        .role("alert")
}

fn form_field(label: &str, input: El, palette: Palette) -> El {
    col()
        .w_full()
        .gap(6.0)
        .child(text(label).font(LABEL).color(palette.ink))
        .child(input.label(label))
}

fn input(key: &str, placeholder: &str, draft: &TextEdit, palette: Palette) -> El {
    text_input(draft, BODY)
        .key(key)
        .w_full()
        .h(40.0)
        .padding(9.0)
        .radius(7.0)
        .fill(palette.field)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent)
        .color(palette.ink)
        .placeholder(placeholder, palette.muted)
        .selection_color(palette.selected)
}

fn button(
    key: &str,
    label: &str,
    palette: Palette,
    primary: bool,
    enabled: bool,
    press: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    let (fill, ink) = match (primary, enabled) {
        (true, true) => (palette.accent, palette.on_accent),
        (false, true) => (Color::TRANSPARENT, palette.ink),
        (_, false) => (palette.sidebar, palette.muted),
    };
    let element = el()
        .key(key)
        .role("button")
        .h(36.0)
        .px(12.0)
        .center()
        .radius(7.0)
        .fill(fill)
        .border(
            1.0,
            if primary && enabled {
                fill
            } else {
                palette.line
            },
        )
        .child(text(label).font(LABEL).color(ink));
    if enabled {
        element
            .focusable(true)
            .focus_ring(1.0, 2.0, palette.accent)
            .hover(if primary {
                fill.mix(ink, 0.13)
            } else {
                palette.hover
            })
            .on_press_tx(press)
    } else {
        element.disabled(true)
    }
}

pub(crate) fn run(
    url: Option<String>,
    pairing_code: Option<String>,
    rune_args: Vec<String>,
    state_dir: Option<&Path>,
    electron_profile: Option<&Path>,
) -> Result<(), String> {
    rune::set_app_id("com.t3tools.T3Code");
    let mut client_settings = state_dir
        .map(ClientSettings::at_state_dir)
        .unwrap_or_default();
    if let Some(profile) = electron_profile {
        client_settings
            .import_electron_appearance(profile)
            .map_err(|error| format!("Could not import Electron appearance. {error}"))?;
    }
    let spec = AppSpec {
        min_size: Some((400.0, 360.0)),
        fonts: &[include_bytes!("../assets/fonts/InterVariable.ttf")],
        setup: Some(|state: &mut State, wake| {
            state.wake = Some(wake);
            state.connect();
        }),
        poll: Some(State::poll),
        ..AppSpec::new("T3 Code", (1100.0, 780.0), State::default, view)
    };
    let app = App::new(spec)
        .titlebar(rune::Titlebar::new(sidebar_header::HEIGHT))
        .chrome(rune::Chrome {
            full_size_content: false,
            ..rune::Chrome::DEFAULT
        })
        .init(move || {
            let mut state = State {
                client_settings,
                ..State::default()
            };
            if let Some(url) = url {
                state.url.replace_text(url);
            }
            if let Some(code) = pairing_code {
                state.pairing_code.replace_text(code);
            }
            state
        });
    window_placement::run(window_chrome::configure(app), rune_args, state_dir)
}

#[cfg(test)]
mod tests {
    use rune::{Env, Runtime};

    use super::*;

    fn publish(runtime: &mut Runtime<State>, event: Event) {
        runtime.update(|state, tx| state.apply_event(event, tx));
        runtime.frame(0.01);
    }

    fn click(runtime: &mut Runtime<State>, key: &str) {
        let (x, y) = runtime.find(key).expect("control is visible");
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(0.01);
    }

    fn shell_with_thread(thread_id: &str) -> Event {
        Event::Shell {
            projects: vec![Project {
                id: "project".into(),
                name: "Example".into(),
                path: "/example".into(),
            }],
            threads: vec![Thread {
                id: thread_id.into(),
                title: "Example thread".into(),
                project_id: "project".into(),
                status: "idle".into(),
                updated_at: None,
                activity: Default::default(),
                providers: Default::default(),
                worktree_path: None,
                archived: false,
            }],
        }
    }

    fn open_thread(runtime: &mut Runtime<State>, environment_id: &str) {
        publish(
            runtime,
            Event::Connected {
                environment_id: environment_id.into(),
                label: "Test environment".into(),
                read_only: false,
                launch_writes: true,
                worktree_launch: false,
                remote_worktree_launch: false,
                checkout_switch: false,
                prompt_writes: true,
                stop_writes: false,
                native_stop: false,
                project_writes: true,
                filesystem_read: false,
                named_projects: false,
                clone_writes: false,
                terminal: false,
            },
        );
        publish(runtime, shell_with_thread("thread"));
        click(runtime, "project-project");
        click(runtime, "thread-thread");
        publish(
            runtime,
            Event::Thread {
                thread_id: "thread".into(),
                messages: vec![],
            },
        );
    }

    fn prompt_text(runtime: &Runtime<State>, key: &str) -> String {
        runtime
            .elements()
            .into_iter()
            .find(|element| element.key.as_deref() == Some(key))
            .and_then(|element| element.text)
            .expect("composer is visible")
    }

    #[test]
    fn sidebar_controls_collapse_and_restore_the_selected_thread() {
        let mut runtime =
            Runtime::new(State::default(), view, (1120.0, 740.0), Env::new(true, 1.0));
        open_thread(&mut runtime, "local");

        click(&mut runtime, "toggle-sidebar");
        assert!(runtime.find("sidebar-titlebar").is_none());
        assert!(runtime.find("prompt").is_some());
        click(&mut runtime, "toggle-sidebar");
        assert!(runtime.find("sidebar-titlebar").is_some());

        click(&mut runtime, "go-back");
        assert!(runtime.find("prompt").is_none());
        click(&mut runtime, "go-forward");
        publish(
            &mut runtime,
            Event::Thread {
                thread_id: "thread".into(),
                messages: vec![],
            },
        );
        assert!(runtime.find("prompt").is_some());

        click(&mut runtime, "home");
        assert!(runtime.find("thread-thread").is_some());
        assert!(runtime.find("prompt").is_none());
    }

    #[test]
    fn sidebar_search_and_scope_filter_the_list_without_closing_the_selected_thread() {
        let mut runtime =
            Runtime::new(State::default(), view, (1120.0, 740.0), Env::new(true, 1.0));
        open_thread(&mut runtime, "local");
        let Event::Shell {
            mut projects,
            mut threads,
        } = shell_with_thread("thread")
        else {
            unreachable!("shell fixture")
        };
        let mut project = projects[0].clone();
        project.id = "other".into();
        project.name = "Café".into();
        let mut thread = threads[0].clone();
        thread.id = "other-thread".into();
        thread.project_id.clone_from(&project.id);
        thread.title = "Testing notifications".into();
        projects.push(project);
        threads.push(thread);
        publish(&mut runtime, Event::Shell { projects, threads });

        assert!(runtime.find("thread-thread").is_some());
        assert!(runtime.find("thread-other-thread").is_some());

        click(&mut runtime, "project-filter");
        click(&mut runtime, "project-option-other");
        assert!(runtime.find("thread-thread").is_none());
        assert!(runtime.find("thread-other-thread").is_some());
        assert!(runtime.find("prompt").is_some());

        click(&mut runtime, "project-filter");
        click(&mut runtime, "all-projects");
        click(&mut runtime, "thread-search");
        runtime.paste("cafe");
        runtime.frame(0.01);
        assert!(runtime.find("thread-thread").is_none());
        assert!(runtime.find("thread-other-thread").is_some());
        assert!(runtime.find("prompt").is_some());

        click(&mut runtime, "thread-other-thread");
        publish(
            &mut runtime,
            Event::Thread {
                thread_id: "other-thread".into(),
                messages: vec![],
            },
        );
        assert!(runtime.find("prompt").is_some());
        assert!(runtime.find("thread-other-thread").is_some());

        click(&mut runtime, "clear-thread-search");
        assert!(runtime.find("thread-thread").is_some());
        assert!(runtime.find("thread-other-thread").is_some());
        assert!(runtime.find("prompt").is_some());
    }

    #[test]
    fn conversations_expand_attributed_prompts_and_copy_original_messages() {
        let mut runtime =
            Runtime::new(State::default(), view, (1120.0, 740.0), Env::new(true, 1.0));
        let clipboard = rune::clipboard::memory();
        runtime.use_clipboard(&clipboard);
        open_thread(&mut runtime, "local");
        let response = "Keep the **Markdown** in the clipboard.\n\nSecond paragraph.";
        publish(
            &mut runtime,
            Event::Thread {
                thread_id: "thread".into(),
                messages: vec![
                    Message {
                        id: "agent-prompt".into(),
                        role: "user".into(),
                        text: "First line.\nSecond line.".into(),
                        created_by: Some("agent".into()),
                    },
                    Message {
                        id: "ordinary-prompt".into(),
                        role: "user".into(),
                        text: "Ordinary prompt".into(),
                        created_by: None,
                    },
                    Message {
                        id: "response".into(),
                        role: "assistant".into(),
                        text: response.into(),
                        created_by: Some("agent".into()),
                    },
                ],
            },
        );
        assert!(runtime.find("message-text-ordinary-prompt").is_some());
        assert!(runtime.find("message-text-agent-prompt").is_none());
        click(&mut runtime, "toggle-message-agent-prompt");
        assert!(runtime.find("message-text-agent-prompt").is_some());
        click(&mut runtime, "toggle-message-agent-prompt");
        assert!(runtime.find("message-text-agent-prompt").is_none());

        click(&mut runtime, "copy-message-ordinary-prompt");
        assert_eq!(
            clipboard.text().as_deref().map(String::as_str),
            Some("Ordinary prompt")
        );
        click(&mut runtime, "copy-message-response");
        assert_eq!(
            clipboard.text().as_deref().map(String::as_str),
            Some(response)
        );
    }

    #[test]
    fn provider_catalog_gates_submitting_without_discarding_the_draft() {
        let mut runtime =
            Runtime::new(State::default(), view, (1120.0, 740.0), Env::new(true, 1.0));
        open_thread(&mut runtime, "local");
        let catalog = |status| {
            serde_json::from_value(serde_json::json!({
                "work": {"instanceId": "work", "driver": "codex", "enabled": true, "status": status},
                "offline": {"instanceId": "offline", "driver": "codex", "enabled": true,
                    "status": "ready", "availability": "unavailable"}
            })).unwrap()
        };
        let can_send = |runtime: &Runtime<State>| {
            runtime
                .elements()
                .iter()
                .any(|element| element.key.as_deref() == Some("send") && element.focusable)
        };
        publish(&mut runtime, Event::Providers(catalog("error")));
        click(&mut runtime, "prompt");
        runtime.paste("Saved while the provider is unavailable.");
        runtime.frame(0.01);
        assert_eq!(
            prompt_text(&runtime, "prompt"),
            "Saved while the provider is unavailable."
        );
        assert!(!can_send(&runtime));

        publish(&mut runtime, Event::Providers(catalog("ready")));
        assert_eq!(
            prompt_text(&runtime, "prompt"),
            "Saved while the provider is unavailable."
        );
        assert!(can_send(&runtime));
    }

    #[test]
    fn reconnect_restores_the_draft_only_for_the_same_environment() {
        let mut runtime =
            Runtime::new(State::default(), view, (1120.0, 740.0), Env::new(true, 1.0));
        open_thread(&mut runtime, "local");
        click(&mut runtime, "prompt");
        runtime.paste("Keep this draft");
        runtime.frame(0.01);
        assert_eq!(prompt_text(&runtime, "prompt"), "Keep this draft");

        publish(&mut runtime, Event::Disconnected("Connection ended".into()));
        open_thread(&mut runtime, "local");
        assert_eq!(prompt_text(&runtime, "prompt"), "Keep this draft");

        publish(&mut runtime, Event::Disconnected("Connection ended".into()));
        open_thread(&mut runtime, "other");
        assert_ne!(prompt_text(&runtime, "prompt"), "Keep this draft");
    }

    #[test]
    fn new_thread_keeps_the_draft_on_rejection_and_opens_after_confirmation() {
        let mut runtime =
            Runtime::new(State::default(), view, (1120.0, 740.0), Env::new(true, 1.0));
        open_thread(&mut runtime, "local");
        click(&mut runtime, "new-thread");
        click(&mut runtime, "initial-prompt");
        runtime.paste("Review the project");
        runtime.frame(0.01);

        publish(
            &mut runtime,
            Event::LaunchError {
                project_id: "project".into(),
                message: "The environment rejected the launch.".into(),
            },
        );
        assert_eq!(
            prompt_text(&runtime, "initial-prompt"),
            "Review the project"
        );

        publish(
            &mut runtime,
            Event::ThreadLaunched {
                project_id: "project".into(),
                thread_id: "new".into(),
            },
        );
        assert!(runtime.find("initial-prompt").is_none());
        assert!(runtime.find("prompt").is_none());
        publish(&mut runtime, shell_with_thread("new"));
        publish(
            &mut runtime,
            Event::Thread {
                thread_id: "new".into(),
                messages: vec![],
            },
        );
        assert!(runtime.find("prompt").is_some());

        click(&mut runtime, "new-thread");
        assert_ne!(
            prompt_text(&runtime, "initial-prompt"),
            "Review the project"
        );
    }
}
