mod archive;
mod browse;
mod browser_preview;
pub(crate) use browser_preview::{
    BrowserFrame, BrowserSnapshot, BrowserTab, Navigation as BrowserNavigation,
};
mod command_output;
mod draft_defaults;
pub(crate) mod file_preview;
mod file_preview_asset;
mod keybindings;
mod launch;
mod project_actions;
mod project_favicon;
mod projection;
mod provider_metadata;
mod queue_mutation;
mod resume;
mod runtime_approval;
mod runtime_question;
mod runtime_response;
mod saved_reasoning;
mod terminal;
mod terminal_link;
pub(crate) use terminal_link::TerminalFileLink;
pub(crate) mod storage_cleanup;
#[cfg(test)]
mod tests;
mod workspace_retry;
mod workspace_settings;
pub(crate) mod worktree_removal;
pub(crate) mod worktree_setup;

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

pub(crate) use archive::ArchiveAction;
pub(crate) use browse::{FolderBrowseError, FolderListing};
pub(crate) use command_output::{CommandInspection, CommandOutput};
pub(crate) use draft_defaults::WorkspaceMode;
pub(crate) use keybindings::{
    Binding, Condition as KeybindingCondition, Edit as KeybindingEdit, Keybindings,
    Rule as KeybindingRule, TERMINAL_TOGGLE,
};
pub(crate) use launch::{
    DraftModelPolicy, DraftProvider, ModelSelection, Workspace as LaunchWorkspace,
};
pub(crate) use project_actions::{ProjectAction, ProjectActionIcon, ProjectActions};
pub(crate) use project_favicon::{FaviconBytes, FaviconTarget};
pub(crate) use projection::WorkspaceFailure;
pub(crate) use projection::{
    CommandEntry, CommandOutputTarget, CommandStatus, CompletedWork, ConversationEntry, Message,
    Project, ProjectIcon, QueuedMessage, Thread, TodoListEntry, TodoStepStatus,
    latest_thread_by_order,
};
use projection::{Conversation, Shell};
pub(crate) use provider_metadata::{
    ProviderCatalog, ProviderMetadata, ProviderModel, ProviderReadiness, ReasoningLevels,
};
pub(crate) use queue_mutation::QueueMutation;
#[cfg(test)]
pub(crate) use runtime_approval::ApprovalChoice;
pub(crate) use runtime_approval::{ApprovalDecision, ApprovalResponse, PendingApproval};
pub(crate) use runtime_question::{
    Answer, PendingQuestion, Question, QuestionOption, QuestionResponse,
};
use runtime_response::RuntimeResponse;
pub(crate) use runtime_response::RuntimeResponseOutcome;
pub(crate) use terminal::{
    TerminalAttach, TerminalSnapshot, TerminalStatus, TerminalTarget, TerminalUpdate,
};
pub(crate) use workspace_retry::WorkspaceRetry;
pub(crate) use workspace_settings::{
    AddProjectBaseDirectory, BranchNamingMode, BranchTextField, CloneLayoutSetting,
    FavoriteSetting, ModelFavorite, PermissionSetting, ProjectFileWorkspaceSettings,
    ResetWorkspaceOverride, SnoozeSetting, SubmoduleMode, WorkspaceSetting, WorktreeLocation,
};
pub(crate) use worktree_setup::WorktreeSetup;

const PROTOCOL_VERSION: u32 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const PING_INTERVAL: Duration = Duration::from_secs(5);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(60);
const RECONNECT_DELAYS: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_secs(1),
    Duration::from_secs(3),
];
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const LAUNCH_UNCONFIRMED: &str =
    "Thread creation could not be confirmed. Check the project before starting again.";
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadAction {
    Open,
    Send,
    Rename,
}

pub(crate) enum Event {
    Keybindings(Keybindings),
    KeybindingsUnavailable,
    KeybindingSettled {
        operation: Uuid,
        result: Result<(), String>,
    },
    Connected {
        environment_id: String,
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
        filesystem_write: bool,
        named_projects: bool,
        clone_writes: bool,
        terminal: bool,
    },
    Shell {
        projects: Vec<Project>,
        threads: Vec<Thread>,
    },
    ArchiveAvailable {
        readable: bool,
        archivable: bool,
        writable: bool,
        deletable: bool,
    },
    ArchivedThreads(Result<Vec<Thread>, String>),
    ArchiveActionSettled {
        action: ArchiveAction,
        thread_id: String,
        operation: Uuid,
        result: Result<(), String>,
    },
    WorktreeRemovalSettled {
        operation: Uuid,
        result: Result<worktree_removal::Reply, String>,
    },
    ScratchProjectsAvailable,
    ScratchWorkspace {
        root: Option<String>,
        can_start: bool,
    },
    ProjectOrganizationAvailable,
    ProjectFavicon {
        target: FaviconTarget,
        image: Option<FaviconBytes>,
    },
    Providers(ProviderCatalog),
    ProviderCatalogUnavailable,
    WorkspaceConfiguration(Option<Box<WorkspaceSetting>>),
    StorageCleanup(storage_cleanup::Update),
    WorkspaceProjectFile {
        operation: Uuid,
        settings: ProjectFileWorkspaceSettings,
    },
    WorkspaceSettingSettled {
        operation: Uuid,
        result: Result<(), String>,
    },
    Thread {
        thread_id: String,
        entries: Vec<ConversationEntry>,
    },
    CommandOutput {
        thread_id: String,
        attachment: Uuid,
        output: CommandOutput,
    },
    FilePreviewLoaded {
        id: Uuid,
        result: Result<file_preview::Contents, file_preview::ReadFailure>,
    },
    FilePreviewAsset {
        id: Uuid,
        result: file_preview::AssetOutcome,
    },
    QueuedMessages {
        thread_id: String,
        messages: Vec<QueuedMessage>,
        resumable_run: Option<String>,
    },
    ResumeSettled {
        thread_id: String,
        operation: Uuid,
        result: Result<(), String>,
    },
    QueueMutationSettled {
        thread_id: String,
        mutation: QueueMutation,
        operation: Uuid,
        result: Result<(), String>,
    },
    ThreadActions {
        thread_id: String,
        stop: bool,
        queue_worktree: bool,
        codex_reasoning: bool,
    },
    SavedReasoningSettled {
        thread_id: String,
        operation: Uuid,
        result: Result<(), String>,
    },
    ThreadStopFailed {
        thread_id: String,
        message: String,
    },
    ThreadApprovals {
        thread_id: String,
        approvals: Vec<PendingApproval>,
        can_respond: bool,
    },
    ApprovalResponded {
        response: ApprovalResponse,
        outcome: RuntimeResponseOutcome,
    },
    ThreadQuestions {
        thread_id: String,
        questions: Vec<PendingQuestion>,
        can_respond: bool,
    },
    QuestionResponded {
        response: QuestionResponse,
        outcome: RuntimeResponseOutcome,
    },
    PromptAccepted {
        thread_id: String,
    },
    ThreadLaunched {
        project_id: String,
        thread_id: String,
    },
    LaunchError {
        project_id: String,
        message: String,
    },
    GitRefs {
        project_id: String,
        result: Result<GitRefs, String>,
    },
    GitCheckout {
        project_id: String,
        result: Result<Option<String>, String>,
    },
    DraftWorkspaceDefault {
        project_id: String,
        mode: WorkspaceMode,
        start_from_origin: bool,
    },
    DraftModelSettings {
        project_id: String,
        policy: Option<DraftModelPolicy>,
    },
    WorktreeSetup {
        thread_id: String,
        snapshot: Option<Box<WorktreeSetup>>,
    },
    WorktreeSetupCancelSettled {
        thread_id: String,
        result: Result<bool, String>,
    },
    WorkspaceRetrySettled {
        retry: WorkspaceRetry,
        outcome: RuntimeResponseOutcome,
    },
    ProjectAdded {
        command_id: String,
        result: Result<ProjectCreated, String>,
    },
    ProjectFolderCollision {
        command_id: String,
    },
    ProjectEditSettled {
        project_id: String,
        kind: ProjectEditKind,
        result: Result<Box<Project>, String>,
    },
    ProjectClones(Vec<ProjectClone>),
    ProjectCloneError(String),
    FolderBrowsed {
        browse_id: Uuid,
        result: Result<FolderListing, FolderBrowseError>,
    },
    ProjectImages {
        search_id: Uuid,
        result: Result<ProjectImages, String>,
    },
    Terminal {
        attachment: u64,
        update: TerminalUpdate,
    },
    BrowserPreviewOpened {
        thread: String,
        url: String,
        snapshot: Option<BrowserSnapshot>,
    },
    BrowserPreviewUpdated(BrowserSnapshot),
    BrowserPreviewClosed(BrowserTab),
    BrowserPreviewFrame {
        tab: BrowserTab,
        frame: BrowserFrame,
    },
    BrowserPreviewStreamLost(BrowserTab),
    TerminalFileLinkChecked {
        attachment: u64,
        id: Uuid,
        available: bool,
    },
    TerminalFileLinkOpened {
        attachment: u64,
        editor: Option<String>,
        result: Result<(), String>,
    },
    TerminalFailed {
        attachment: u64,
        message: String,
    },
    TerminalIdleWriteSettled {
        request: Uuid,
        result: Result<Option<String>, String>,
    },
    TerminalCloseSettled {
        target: TerminalTarget,
        result: Result<(), String>,
    },
    RequestError {
        thread_id: String,
        action: ThreadAction,
        message: String,
    },
    Reconnecting,
    Reconnected,
    Disconnected(String),
}

enum Command {
    EditKeybinding {
        edit: KeybindingEdit,
        operation: Uuid,
    },
    RefreshProviderCatalog,
    EnsureWorkspaceSettings,
    StorageCleanup(storage_cleanup::Intent),
    ReadWorkspaceProjectFile {
        project_id: String,
        operation: Uuid,
    },
    UpdateWorkspaceSetting {
        target: workspace_settings::Target,
        edit: workspace_settings::Edit,
        operation: Uuid,
    },
    LoadProjectFavicons(Vec<FaviconTarget>),
    OpenThread(String),
    CloseThread,
    ObserveOutput(command_output::Command),
    SendPrompt {
        thread_id: String,
        prompt: String,
        context: Option<crate::composer_context::MessageContext>,
    },
    MutateQueue {
        thread_id: String,
        mutation: QueueMutation,
        operation: Uuid,
    },
    ResumeThread {
        thread_id: String,
        source_run: String,
        operation: Uuid,
    },
    ArchiveAction {
        action: ArchiveAction,
        thread_id: String,
        operation: Uuid,
    },
    WorktreeRemoval {
        operation: Uuid,
        request: worktree_removal::Request,
    },
    SetSavedReasoning(saved_reasoning::Edit),
    StopThread(String),
    RenameThread {
        thread_id: String,
        title: String,
    },
    CancelWorktreeSetup(String),
    ObserveDraftWorktree(Option<String>),
    RetryWorkspacePreparation(WorkspaceRetry),
    RespondToRuntimeRequest(RuntimeResponse),
    LaunchThread(launch::Draft),
    LoadDraftPreferences(String),
    ListGitRefs {
        project_id: String,
        cwd: String,
        refresh: bool,
    },
    ChangeGitCheckout {
        project_id: String,
        cwd: String,
        name: String,
        create: bool,
    },
    AddProject {
        command_id: String,
        workspace_root: String,
        title: String,
    },
    CreateNamedProject {
        command_id: String,
        name: String,
    },
    StartScratchProject(String),
    EditProject {
        project_id: String,
        edit: ProjectEdit,
    },
    CloneProject {
        project_id: String,
        remote_url: String,
        destination_path: String,
        title: String,
    },
    ProjectCloneAction {
        project_id: String,
        action: ProjectCloneAction,
    },
    BrowseFolder {
        browse_id: Uuid,
        partial_path: String,
        cwd: Option<String>,
    },
    ReadFilePreview(file_preview::ReadRequest),
    PrepareFileAsset {
        input: file_preview::ReadRequest,
        purpose: file_preview::AssetPurpose,
    },
    CancelFilePreview(Uuid),
    SearchProjectImages {
        search_id: Uuid,
        cwd: String,
        query: String,
    },
    Terminal(terminal::Command),
    BrowserPreview(browser_preview::Command),
    TerminalFileLink(terminal_link::Request),
}

pub(crate) struct Connection {
    commands: UnboundedSender<Command>,
    events: Receiver<Event>,
    next_terminal_attachment: Cell<u64>,
    terminal_selection: Arc<terminal::Selection>,
    _shutdown: oneshot::Sender<()>,
}

#[derive(Deserialize)]
pub(crate) struct ProjectImages {
    pub(crate) entries: Vec<ProjectImageEntry>,
}

#[derive(Deserialize)]
pub(crate) struct ProjectImageEntry {
    pub(crate) path: String,
    kind: String,
}

impl ProjectImageEntry {
    pub(crate) fn is_file(&self) -> bool {
        self.kind == "file"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ProjectEditKind {
    Title,
    Pinned,
    Archived,
    UnarchiveFromPicker,
    Icon,
}

pub(crate) enum ProjectEdit {
    Title(String),
    Pinned(bool),
    Archived(bool),
    UnarchiveFromPicker,
    Icon(Option<ProjectIcon>),
    IconFile(String),
}

impl ProjectEdit {
    pub(crate) fn kind(&self) -> ProjectEditKind {
        match self {
            Self::Title(_) => ProjectEditKind::Title,
            Self::Pinned(_) => ProjectEditKind::Pinned,
            Self::Archived(_) => ProjectEditKind::Archived,
            Self::UnarchiveFromPicker => ProjectEditKind::UnarchiveFromPicker,
            Self::Icon(_) | Self::IconFile(_) => ProjectEditKind::Icon,
        }
    }

    fn apply(self, payload: &mut Value) {
        match self {
            Self::Title(title) => payload["title"] = json!(title),
            Self::Pinned(pinned) => payload["pinned"] = json!(pinned),
            Self::Archived(archived) => payload["archived"] = json!(archived),
            Self::UnarchiveFromPicker => payload["archived"] = json!(false),
            Self::Icon(icon) => {
                payload["projectIcon"] = json!(icon);
                payload["faviconPath"] = Value::Null;
            }
            Self::IconFile(path) => {
                payload["projectIcon"] = Value::Null;
                payload["faviconPath"] = json!(path);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectCreated {
    pub(crate) project_id: String,
    pub(crate) commit_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitRefs {
    #[serde(rename = "refs")]
    pub(crate) branches: Vec<GitRef>,
    pub(crate) is_repo: bool,
    next_cursor: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitRef {
    pub(crate) name: String,
    pub(crate) current: bool,
    pub(crate) is_default: bool,
    #[serde(default)]
    pub(crate) is_remote: bool,
    pub(crate) worktree_path: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectClone {
    pub(crate) project_id: String,
    pub(crate) phase: ProjectClonePhase,
    pub(crate) stage: ProjectCloneStage,
    pub(crate) percent: Option<u8>,
    pub(crate) detail: Option<String>,
    #[serde(default)]
    pub(crate) stalled: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectClonePhase {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectCloneStage {
    Connecting,
    Counting,
    Receiving,
    Resolving,
    Checkout,
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectCloneAction {
    Cancel,
    Retry,
    Remove,
}

impl Connection {
    pub(crate) fn start_scratch_project(&self, command_id: String) {
        let _ = self.commands.send(Command::StartScratchProject(command_id));
    }

    pub(crate) fn edit_project(&self, project_id: String, edit: ProjectEdit) {
        let _ = self
            .commands
            .send(Command::EditProject { project_id, edit });
    }

    pub(crate) fn refresh_provider_catalog(&self) {
        let _ = self.commands.send(Command::RefreshProviderCatalog);
    }

    pub(crate) fn load_project_favicons(&self, targets: Vec<FaviconTarget>) {
        if !targets.is_empty() {
            let _ = self.commands.send(Command::LoadProjectFavicons(targets));
        }
    }

    pub(crate) fn start(
        server_url: String,
        pairing_code: String,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let (commands, command_rx) = unbounded_channel();
        let (event_tx, events) = channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let terminal_selection = Arc::new(terminal::Selection::default());
        let notify = Arc::new(notify);
        let worker = std::thread::Builder::new()
            .name("t3-environment".to_owned())
            .spawn({
                let event_tx = event_tx.clone();
                let worker_notify = notify.clone();
                let terminal_selection = Arc::clone(&terminal_selection);
                move || {
                    let events = EventSender {
                        event_tx,
                        notify: move || worker_notify(),
                    };
                    let result = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| format!("Could not start the connection worker: {error}"))
                        .and_then(|runtime| {
                            runtime.block_on(async {
                                tokio::select! {
                                    biased;
                                    _ = shutdown_rx => Ok(()),
                                    result = run(server_url, pairing_code, command_rx, &terminal_selection, &events) => result,
                                }
                            })
                        });
                    if let Err(message) = result {
                        events.send(Event::Disconnected(message));
                    }
                }
            });
        if let Err(error) = worker {
            let _ = event_tx.send(Event::Disconnected(format!(
                "Could not start the connection worker: {error}"
            )));
            notify();
        }
        Self {
            commands,
            events,
            next_terminal_attachment: Cell::new(0),
            terminal_selection,
            _shutdown: shutdown,
        }
    }

    pub(crate) fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub(crate) fn open_thread(&self, thread_id: String) {
        self.terminal_selection.keep_thread(Some(&thread_id));
        let _ = self.commands.send(Command::OpenThread(thread_id));
    }

    pub(crate) fn close_thread(&self) {
        self.terminal_selection.keep_thread(None);
        let _ = self.commands.send(Command::CloseThread);
    }

    pub(crate) fn send_prompt(
        &self,
        thread_id: String,
        prompt: String,
        context: Option<crate::composer_context::MessageContext>,
    ) {
        let _ = self.commands.send(Command::SendPrompt {
            thread_id,
            prompt,
            context,
        });
    }

    pub(crate) fn mutate_queue(&self, thread_id: String, mutation: QueueMutation, operation: Uuid) {
        let _ = self.commands.send(Command::MutateQueue {
            thread_id,
            mutation,
            operation,
        });
    }

    pub(crate) fn stop_thread(&self, thread_id: String) {
        let _ = self.commands.send(Command::StopThread(thread_id));
    }

    pub(crate) fn rename_thread(&self, thread_id: String, title: String) {
        let _ = self
            .commands
            .send(Command::RenameThread { thread_id, title });
    }

    pub(crate) fn observe_draft_worktree(&self, thread_id: Option<String>) {
        let _ = self.commands.send(Command::ObserveDraftWorktree(thread_id));
    }

    pub(crate) fn cancel_worktree_setup(&self, thread_id: String) {
        let _ = self.commands.send(Command::CancelWorktreeSetup(thread_id));
    }

    pub(crate) fn retry_workspace_preparation(&self, retry: WorkspaceRetry) {
        let _ = self
            .commands
            .send(Command::RetryWorkspacePreparation(retry));
    }

    pub(crate) fn respond_to_approval(&self, response: ApprovalResponse) {
        let _ = self
            .commands
            .send(Command::RespondToRuntimeRequest(RuntimeResponse::Approval(
                response,
            )));
    }

    pub(crate) fn respond_to_question(&self, response: QuestionResponse) {
        let _ = self
            .commands
            .send(Command::RespondToRuntimeRequest(RuntimeResponse::Question(
                response,
            )));
    }

    pub(crate) fn launch_thread(
        &self,
        project_id: String,
        prompt: String,
        workspace: LaunchWorkspace,
        model: Option<ModelSelection>,
    ) -> String {
        let thread_id = Uuid::new_v4().to_string();
        let _ = self.commands.send(Command::LaunchThread(launch::Draft {
            project_id,
            thread_id: thread_id.clone(),
            prompt,
            workspace,
            model,
        }));
        thread_id
    }

    pub(crate) fn load_draft_preferences(&self, project_id: String) {
        let _ = self
            .commands
            .send(Command::LoadDraftPreferences(project_id));
    }

    pub(crate) fn list_git_refs(&self, project_id: String, cwd: String, refresh: bool) {
        let _ = self.commands.send(Command::ListGitRefs {
            project_id,
            cwd,
            refresh,
        });
    }

    pub(crate) fn change_git_checkout(
        &self,
        project_id: String,
        cwd: String,
        name: String,
        create: bool,
    ) {
        let _ = self.commands.send(Command::ChangeGitCheckout {
            project_id,
            cwd,
            name,
            create,
        });
    }

    pub(crate) fn add_project(&self, command_id: String, workspace_root: String, title: String) {
        let _ = self.commands.send(Command::AddProject {
            command_id,
            workspace_root,
            title,
        });
    }

    pub(crate) fn search_project_images(&self, search_id: Uuid, cwd: String, query: String) {
        let _ = self.commands.send(Command::SearchProjectImages {
            search_id,
            cwd,
            query,
        });
    }

    pub(crate) fn browse_folder(&self, browse_id: Uuid, partial_path: String, cwd: Option<String>) {
        let _ = self.commands.send(Command::BrowseFolder {
            browse_id,
            partial_path,
            cwd,
        });
    }

    pub(crate) fn read_file_preview(&self, request: file_preview::ReadRequest) {
        let _ = self.commands.send(Command::ReadFilePreview(request));
    }

    pub(crate) fn prepare_file_asset(
        &self,
        input: file_preview::ReadRequest,
        purpose: file_preview::AssetPurpose,
    ) {
        let _ = self
            .commands
            .send(Command::PrepareFileAsset { input, purpose });
    }

    pub(crate) fn cancel_file_preview(&self, id: Uuid) {
        let _ = self.commands.send(Command::CancelFilePreview(id));
    }

    pub(crate) fn create_named_project(&self, command_id: String, name: String) {
        let _ = self
            .commands
            .send(Command::CreateNamedProject { command_id, name });
    }

    pub(crate) fn clone_project(
        &self,
        project_id: String,
        remote_url: String,
        destination_path: String,
        title: String,
    ) {
        let _ = self.commands.send(Command::CloneProject {
            project_id,
            remote_url,
            destination_path,
            title,
        });
    }

    pub(crate) fn project_clone_action(&self, project_id: String, action: ProjectCloneAction) {
        let _ = self
            .commands
            .send(Command::ProjectCloneAction { project_id, action });
    }

    pub(crate) fn attach_terminal(&self, attach: TerminalAttach) -> u64 {
        let attachment = self.next_terminal_attachment.get();
        self.next_terminal_attachment.set(attachment + 1);
        let intent = terminal::Intent::new(attachment, attach);
        self.terminal_selection.attach(&intent);
        self.terminal_command(terminal::Command::Attach(intent));
        attachment
    }

    pub(crate) fn detach_terminal(&self, attachment: u64) {
        self.terminal_selection.detach(attachment);
        self.terminal_command(terminal::Command::Detach(attachment));
    }

    pub(crate) fn write_terminal(&self, attachment: u64, data: String, binary: bool, reply: bool) {
        self.terminal_command(terminal::Command::Write {
            attachment,
            data,
            binary,
            reply,
        });
    }

    pub(crate) fn write_idle_terminal(
        &self,
        thread_id: String,
        terminal_ids: Vec<String>,
        data: String,
    ) -> Option<Uuid> {
        let request = Uuid::new_v4();
        let session = self.terminal_selection.write_session()?;
        self.commands
            .send(Command::Terminal(terminal::Command::WriteIdle {
                request,
                session,
                thread_id,
                terminal_ids,
                data,
            }))
            .ok()
            .map(|()| request)
    }

    pub(crate) fn resize_terminal(&self, attachment: u64, cols: u16, rows: u16) {
        self.terminal_command(terminal::Command::Resize {
            attachment,
            cols,
            rows,
        });
    }

    pub(crate) fn close_terminal(&self, target: TerminalTarget, delete_history: bool) {
        self.terminal_command(terminal::Command::Close {
            target,
            delete_history,
        });
    }

    fn terminal_command(&self, command: terminal::Command) {
        let _ = self.commands.send(Command::Terminal(command));
    }
}

struct EventSender<Notify> {
    event_tx: Sender<Event>,
    notify: Notify,
}

impl<Notify: Fn()> EventSender<Notify> {
    fn send(&self, event: Event) {
        if self.event_tx.send(event).is_ok() {
            (self.notify)();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    environment_id: String,
    label: String,
    orchestration_protocol_version: Option<u32>,
    #[serde(default)]
    capabilities: Capabilities,
}

#[derive(Default, Deserialize)]
struct Capabilities {
    #[serde(default, rename = "storageCleanup")]
    storage_cleanup: bool,
    #[serde(default, rename = "storageCleanupRun")]
    storage_cleanup_run: bool,
    #[serde(default, rename = "commandOutputStreaming")]
    command_output_streaming: bool,
    #[serde(default, rename = "t3ReadOnly")]
    read_only: bool,
    #[serde(rename = "t3ProjectWrites")]
    project_writes: Option<bool>,
    #[serde(rename = "t3NamedProjects")]
    named_projects: Option<bool>,
    #[serde(default, rename = "t3ScratchProjects")]
    scratch_projects: bool,
    #[serde(default, rename = "projectOrganization")]
    project_organization: bool,
    #[serde(rename = "t3CloneProjects")]
    clone_projects: Option<bool>,
    #[serde(rename = "t3PromptWrites")]
    prompt_writes: Option<bool>,
    #[serde(rename = "t3ThreadLaunch")]
    launch_writes: Option<bool>,
    #[serde(rename = "t3WorktreeLaunch")]
    worktree_launch: Option<bool>,
    #[serde(rename = "t3RemoteWorktreeLaunch")]
    remote_worktree_launch: Option<bool>,
    #[serde(rename = "t3WorktreeSetup")]
    worktree_setup: Option<bool>,
    #[serde(rename = "t3WorktreeQueue")]
    worktree_queue: Option<bool>,
    #[serde(default, rename = "t3ShellHttp")]
    shell_http: bool,
    #[serde(rename = "shellThreadFamilies")]
    shell_thread_families: Option<bool>,
    #[serde(rename = "shellOlderSettled")]
    shell_older_settled: Option<bool>,
    #[serde(rename = "t3CheckoutSwitch")]
    checkout_switch: Option<bool>,
    #[serde(rename = "t3ThreadStop")]
    thread_stop: Option<bool>,
    #[serde(rename = "t3ArchiveSnapshots")]
    archive_snapshots: Option<bool>,
    #[serde(rename = "t3ThreadArchive")]
    thread_archive: Option<bool>,
    #[serde(default, rename = "t3ThreadUnarchive")]
    thread_unarchive: bool,
    #[serde(default, rename = "t3ArchivedThreadDelete")]
    thread_archive_delete: bool,
    #[serde(default, rename = "t3ThreadVisits")]
    thread_visits: bool,
    #[serde(rename = "t3Terminal")]
    terminal: Option<bool>,
    #[serde(default, rename = "t3TerminalIdleWrite")]
    terminal_idle_write: bool,
    #[serde(default, rename = "serverBrowser")]
    server_browser: bool,
}

#[derive(Deserialize)]
struct AccessToken {
    access_token: String,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct Ticket {
    ticket: String,
}

struct Environment {
    url: Url,
    client: Client,
    descriptor: Descriptor,
    token: AccessToken,
}

fn environment_url(server_url: &str) -> Result<Url, String> {
    let url = Url::parse(server_url.trim())
        .map_err(|_| "Enter a valid T3 Code environment URL.".to_owned())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Enter an HTTP or HTTPS environment URL without credentials.".to_owned());
    }
    Ok(url)
}

fn endpoint(source: &Url, path: &str) -> Url {
    let mut url = source.clone();
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn socket_url(server_url: &Url, ticket: &str) -> Url {
    let mut url = endpoint(server_url, "/ws");
    let scheme = if server_url.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    url.set_scheme(scheme)
        .expect("HTTP and WebSocket schemes share the same URL structure");
    url.query_pairs_mut()
        .append_pair("orchestrationProtocol", &PROTOCOL_VERSION.to_string())
        .append_pair("wsTicket", ticket)
        .append_pair("clientSurface", "desktop")
        .append_pair("clientDeviceType", "desktop")
        .append_pair("connectionMethod", "direct");
    url
}

async fn response_json<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
    operation: &str,
) -> Result<T, String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("{operation}: {}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!("{operation}: {}", response.status()));
    }
    response
        .json()
        .await
        .map_err(|error| format!("{operation}: {}", error.without_url()))
}

async fn pair(server_url: String, pairing_code: String) -> Result<Environment, String> {
    let url = environment_url(&server_url)?;
    let code = pairing_code.trim();
    if code.is_empty() {
        return Err("Enter a pairing code for this environment.".to_owned());
    }
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("Could not configure HTTP: {error}"))?;
    let descriptor: Descriptor = response_json(
        client.get(endpoint(&url, "/.well-known/t3/environment")),
        "Could not discover the T3 Code environment",
    )
    .await?;
    if descriptor.orchestration_protocol_version != Some(PROTOCOL_VERSION) {
        return Err(format!(
            "This environment uses orchestration protocol {}. T3 Code Rune requires version {PROTOCOL_VERSION}.",
            descriptor.orchestration_protocol_version.unwrap_or(1)
        ));
    }
    let token: AccessToken = response_json(
        client.post(endpoint(&url, "/oauth/token")).form(&[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
            ("subject_token", code),
            (
                "subject_token_type",
                "urn:t3:params:oauth:token-type:environment-bootstrap",
            ),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("client_label", "T3 Code Rune"),
            ("client_device_type", "desktop"),
            ("client_os", std::env::consts::OS),
        ]),
        "Pairing failed",
    )
    .await?;
    Ok(Environment {
        url,
        client,
        descriptor,
        token,
    })
}

impl Environment {
    async fn bootstrap_shell(&self) -> Option<(Shell, Value)> {
        let capabilities = &self.descriptor.capabilities;
        if !capabilities.shell_http
            && (capabilities.shell_thread_families.is_none()
                || capabilities.shell_older_settled.is_none())
        {
            return None;
        }
        let snapshot: Value = response_json(
            self.client
                .get(endpoint(&self.url, "/api/orchestration/shell"))
                .bearer_auth(&self.token.access_token)
                .header("x-t3-orchestration-protocol", PROTOCOL_VERSION.to_string()),
            "Could not load the environment shell",
        )
        .await
        .ok()?;
        let sequence = snapshot["snapshotSequence"].as_u64()?;
        let mut payload = json!({"afterSequence":sequence,"requestCompletionMarker":true});
        if let Some(digest) = snapshot["projectIdentities"].as_str() {
            payload["knownProjectIdentities"] = json!(digest);
        }
        let mut shell = Shell::default();
        shell
            .apply(json!({"kind":"snapshot","snapshot":snapshot}))
            .ok()?
            .then_some((shell, payload))
    }

    async fn socket(&self) -> Result<Socket, String> {
        let issue_ticket = |path| {
            self.client
                .post(endpoint(&self.url, path))
                .bearer_auth(&self.token.access_token)
        };
        let response = issue_ticket("/api/auth/websocket-ticket/rpc")
            .send()
            .await
            .map_err(|error| {
                format!(
                    "Could not authorize the connection: {}",
                    error.without_url()
                )
            })?;
        let ticket: Ticket = if response.status() == StatusCode::NOT_FOUND {
            response_json(
                issue_ticket("/api/auth/websocket-ticket"),
                "Could not authorize the connection",
            )
            .await?
        } else if !response.status().is_success() {
            return Err(format!(
                "Could not authorize the connection: {}",
                response.status()
            ));
        } else {
            response
                .json()
                .await
                .map_err(|error| format!("Invalid WebSocket ticket: {}", error.without_url()))?
        };
        let ws_url = socket_url(&self.url, &ticket.ticket);
        let (socket, _) = timeout(REQUEST_TIMEOUT, connect_async(ws_url.as_str()))
            .await
            .map_err(|_| "Timed out connecting to the environment.".to_owned())?
            .map_err(|error| format!("Could not open the environment WebSocket: {error}"))?;
        Ok(socket)
    }
}

#[derive(Deserialize)]
#[serde(tag = "_tag")]
enum ServerFrame {
    Chunk {
        #[serde(rename = "requestId")]
        request_id: String,
        values: Vec<Value>,
    },
    Exit {
        #[serde(rename = "requestId")]
        request_id: String,
        exit: RpcExit,
    },
    Pong,
    Defect,
    ClientProtocolError,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "_tag")]
enum RpcExit {
    Success {
        #[serde(default)]
        value: Value,
    },
    Failure {
        cause: Vec<RpcCause>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "_tag")]
enum RpcCause {
    Fail {
        error: Box<RpcError>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct RpcError {
    message: Option<String>,
    detail: Option<String>,
    reason: Option<Value>,
    #[serde(default, rename = "pathMissing")]
    path_missing: bool,
    #[serde(default, rename = "cause")]
    defect: Value,
    #[serde(flatten)]
    file: file_preview::FailureFields,
}

impl RpcExit {
    fn create_ref_error(&self) -> Option<String> {
        let Self::Failure { cause } = self else {
            return None;
        };
        cause.iter().find_map(|cause| match cause {
            RpcCause::Fail { error } if error.file.tag.as_deref() == Some("GitCommandError") => {
                let detail = error.detail.as_deref()?;
                Some(match error.reason.as_ref().and_then(Value::as_str) {
                    Some(reason) => format!("{detail} ({reason})"),
                    None => detail.to_owned(),
                })
            }
            _ => None,
        })
    }

    fn into_result(self) -> Result<Value, String> {
        match self {
            Self::Success { value } => Ok(value),
            Self::Failure { cause } => Err(cause
                .into_iter()
                .find_map(|reason| match reason {
                    RpcCause::Fail { error } => error.message.or(error.detail),
                    RpcCause::Other => None,
                })
                .unwrap_or_else(|| "The environment could not complete the request.".to_owned())),
        }
    }
}

struct ThreadSubscription {
    request_id: String,
    thread_id: String,
    conversation: Conversation,
    reported_queue: Vec<QueuedMessage>,
    reported_resumable_run: Option<String>,
    reported_approvals: Vec<PendingApproval>,
    reported_questions: Option<Vec<PendingQuestion>>,
    reported_stop: Option<bool>,
    reported_queue_worktree: bool,
    reported_codex_reasoning: bool,
    actions_request_id: Option<String>,
    actions_refresh_needed: bool,
    setup_request_id: Option<String>,
}

enum LaunchStage {
    Config(launch::Draft),
    DefaultModel {
        project_id: String,
        thread_id: String,
        payload: Value,
    },
    Submitted {
        project_id: String,
        thread_id: String,
    },
}

impl LaunchStage {
    fn timeout(&self) -> Duration {
        if matches!(self, Self::DefaultModel { .. }) {
            MODEL_DISCOVERY_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        }
    }

    fn failure(&self, settings_error: &str) -> Event {
        let (project_id, message) = match self {
            Self::Config(draft) => (&draft.project_id, settings_error),
            Self::DefaultModel { project_id, .. } => {
                (project_id, "Could not load the default Codex model.")
            }
            Self::Submitted { project_id, .. } => (project_id, LAUNCH_UNCONFIRMED),
        };
        Event::LaunchError {
            project_id: project_id.clone(),
            message: message.to_owned(),
        }
    }
}

#[derive(Clone, Copy)]
enum ProjectCreation {
    Folder,
    Named,
    Scratch,
    Clone,
}

struct Session {
    favicons: project_favicon::Favicons,
    socket: Socket,
    next_request_id: u64,
    shell: Shell,
    shell_request_id: String,
    initial_shell: bool,
    archive: ArchiveState,
    archive_active_writes: bool,
    archive_writes: bool,
    archive_deletes: bool,
    pending_archive_actions: HashMap<String, archive::PendingAction>,
    pending_worktree_removals: HashMap<String, worktree_removal::Pending>,
    providers_request_id: Option<String>,
    workspace_settings: workspace_settings::Subscription,
    storage_cleanup: storage_cleanup::Client,
    project_clones_request_id: Option<String>,
    command_output_streaming: bool,
    command_outputs: command_output::Subscriptions,
    pending_project_clone_actions: HashMap<String, Instant>,
    thread: Option<ThreadSubscription>,
    draft_setup: Option<(String, String)>,
    pending_prompts: HashMap<String, String>,
    pending_queue_changes: HashMap<String, (String, QueueMutation, Uuid)>,
    pending_resumes: HashMap<String, resume::PendingResume>,
    pending_runtime_responses: HashMap<String, (RuntimeResponse, Instant)>,
    pending_stops: HashMap<String, (String, Instant)>,
    pending_title_edits: HashMap<String, (String, Instant)>,
    pending_saved_reasoning: HashMap<String, saved_reasoning::Pending>,
    pending_setup_cancels: HashMap<String, (String, Instant)>,
    pending_workspace_retries: HashMap<String, (WorkspaceRetry, Instant)>,
    pending_projects: HashMap<String, (String, Instant, ProjectCreation)>,
    keybindings_writable: bool,
    pending_keybinding: Option<keybindings::PendingEdit>,
    pending_project_edits: HashMap<String, (String, ProjectEditKind, Instant)>,
    pending_folder: Option<(String, Uuid, Instant)>,
    pending_file_preview: Option<file_preview::PendingRead>,
    preview_downloads: file_preview_asset::Downloads,
    pending_project_images: Option<(String, Uuid, Instant)>,
    pending_launches: HashMap<String, (LaunchStage, Instant)>,
    pending_git_refs: HashMap<String, GitRefsRequest>,
    pending_git_checkouts: HashMap<String, (String, bool)>,
    pending_workspace_defaults: HashMap<String, (String, Instant)>,
    pending_project_file_defaults: HashMap<String, (String, draft_defaults::Preferences, Instant)>,
    pending_named_project_defaults: HashMap<String, draft_defaults::Preferences>,
    named_projects_root: Option<String>,
    named_projects_root_loaded: bool,
    read_only: bool,
    launch_writes: bool,
    worktree_launch: bool,
    remote_worktree_launch: bool,
    worktree_setup: bool,
    worktree_queue: bool,
    checkout_switch: bool,
    source_control_write: bool,
    native_model_discovery: bool,
    native_stop: bool,
    stop_writes: bool,
    prompt_writes: bool,
    runtime_responses: bool,
    project_writes: bool,
    filesystem_read: bool,
    filesystem_write: bool,
    named_projects: bool,
    scratch_projects: bool,
    project_organization: bool,
    clone_writes: bool,
    thread_visits: bool,
    terminals_enabled: bool,
    terminal_idle_write: bool,
    terminal_write_session: Uuid,
    terminals: BTreeMap<u64, terminal::Subscription>,
    pending_terminal: HashMap<String, terminal::PendingOperation>,
    terminal_links: terminal_link::TerminalLinks,
    browser_preview: browser_preview::Client,
}

struct GitRefsRequest {
    project_id: String,
    cwd: String,
    refresh: bool,
    started: Instant,
    cursor: Option<usize>,
    branches: Vec<GitRef>,
}

enum SessionStop {
    Socket(String),
    Protocol(String),
}

enum ArchiveState {
    Disabled,
    Needed,
    Loading(String),
    Loaded,
}

impl SessionStop {
    fn into_message(self) -> String {
        match self {
            Self::Socket(message) | Self::Protocol(message) => message,
        }
    }
}

impl Session {
    async fn open(
        environment: &Environment,
        thread_id: Option<String>,
        terminals: &[terminal::Intent],
        terminal_selection: &terminal::Selection,
    ) -> Result<Self, String> {
        let capabilities = &environment.descriptor.capabilities;
        let scope = environment.token.scope.as_deref();
        let source_control_write = scope.is_some_and(|scopes| {
            scopes
                .split_whitespace()
                .any(|scope| scope == "source-control:write")
        });
        let can_operate = |scopes: &str| {
            scopes
                .split_whitespace()
                .any(|scope| scope == "orchestration:operate")
        };
        let settings_write = scope.is_some_and(|scopes| {
            scopes
                .split_whitespace()
                .any(|scope| scope == "settings:write")
        });
        let prompt_writes = capabilities
            .prompt_writes
            .unwrap_or(!capabilities.read_only)
            && scope.is_none_or(can_operate);
        let (socket, bootstrap) = tokio::join!(environment.socket(), environment.bootstrap_shell());
        let initial_shell = bootstrap.is_some();
        let (shell, shell_input) = bootstrap.unwrap_or_else(|| (Shell::default(), json!({})));
        let mut session = Self {
            socket: socket?,
            next_request_id: 0,
            shell,
            shell_request_id: String::new(),
            favicons: project_favicon::Favicons::new(environment),
            initial_shell,
            archive: if capabilities
                .archive_snapshots
                .unwrap_or(!capabilities.read_only && scope.is_some())
            {
                ArchiveState::Needed
            } else {
                ArchiveState::Disabled
            },
            archive_active_writes: capabilities
                .thread_archive
                .unwrap_or(!capabilities.read_only)
                && scope.is_some_and(can_operate),
            archive_writes: capabilities.thread_unarchive && scope.is_some_and(can_operate),
            archive_deletes: capabilities.thread_archive_delete && scope.is_some_and(can_operate),
            pending_archive_actions: HashMap::new(),
            pending_worktree_removals: HashMap::new(),
            providers_request_id: None,
            workspace_settings: workspace_settings::Subscription::new(settings_write),
            storage_cleanup: storage_cleanup::Client::new(
                capabilities.storage_cleanup && capabilities.storage_cleanup_run,
                settings_write,
            ),
            project_clones_request_id: None,
            command_output_streaming: capabilities.command_output_streaming,
            command_outputs: command_output::Subscriptions::default(),
            pending_project_clone_actions: HashMap::new(),
            thread: None,
            draft_setup: None,
            pending_prompts: HashMap::new(),
            pending_queue_changes: HashMap::new(),
            pending_resumes: HashMap::new(),
            pending_runtime_responses: HashMap::new(),
            pending_stops: HashMap::new(),
            pending_title_edits: HashMap::new(),
            pending_saved_reasoning: HashMap::new(),
            pending_setup_cancels: HashMap::new(),
            pending_workspace_retries: HashMap::new(),
            pending_projects: HashMap::new(),
            keybindings_writable: settings_write,
            pending_keybinding: None,
            pending_project_edits: HashMap::new(),
            pending_folder: None,
            pending_file_preview: None,
            preview_downloads: file_preview_asset::Downloads::new(environment),
            pending_project_images: None,
            pending_launches: HashMap::new(),
            pending_git_refs: HashMap::new(),
            pending_git_checkouts: HashMap::new(),
            pending_workspace_defaults: HashMap::new(),
            pending_project_file_defaults: HashMap::new(),
            pending_named_project_defaults: HashMap::new(),
            named_projects_root: None,
            named_projects_root_loaded: false,
            read_only: environment.descriptor.capabilities.read_only,
            launch_writes: capabilities
                .launch_writes
                .unwrap_or(!capabilities.read_only)
                && scope.is_none_or(can_operate),
            worktree_launch: capabilities
                .worktree_launch
                .unwrap_or(!capabilities.read_only && scope.is_some())
                && scope.is_some_and(can_operate),
            remote_worktree_launch: capabilities
                .remote_worktree_launch
                .unwrap_or(!capabilities.read_only)
                && scope.is_some_and(can_operate),
            worktree_setup: capabilities.worktree_setup.unwrap_or(
                !capabilities.read_only
                    && scope.is_some_and(|scopes| {
                        scopes
                            .split_whitespace()
                            .any(|scope| scope == "orchestration:read")
                    }),
            ),
            worktree_queue: capabilities.worktree_queue == Some(true)
                && scope.is_some_and(can_operate),
            checkout_switch: capabilities
                .checkout_switch
                .unwrap_or(!capabilities.read_only)
                && source_control_write,
            source_control_write,
            native_model_discovery: capabilities.launch_writes == Some(true),
            native_stop: capabilities.thread_stop == Some(true),
            stop_writes: capabilities.thread_stop.unwrap_or(!capabilities.read_only)
                && scope.is_some_and(can_operate),
            prompt_writes,
            runtime_responses: prompt_writes && scope.is_some_and(can_operate),
            project_writes: environment
                .descriptor
                .capabilities
                .project_writes
                .unwrap_or(!environment.descriptor.capabilities.read_only)
                && scope.is_none_or(can_operate),
            filesystem_read: environment.token.scope.as_deref().is_some_and(|scope| {
                scope
                    .split_whitespace()
                    .any(|scope| scope == "filesystem:read")
            }),
            filesystem_write: scope.is_some_and(|scope| {
                scope
                    .split_whitespace()
                    .any(|permission| permission == "filesystem:write")
            }),
            named_projects: environment
                .descriptor
                .capabilities
                .named_projects
                .unwrap_or(!environment.descriptor.capabilities.read_only),
            scratch_projects: capabilities.scratch_projects && scope.is_some_and(can_operate),
            project_organization: capabilities.project_organization
                && scope.is_some_and(can_operate),
            clone_writes: capabilities
                .clone_projects
                .unwrap_or(!capabilities.read_only)
                && scope.is_some_and(|scopes| {
                    can_operate(scopes)
                        && scopes
                            .split_whitespace()
                            .any(|scope| scope == "source-control:write")
                }),
            thread_visits: environment.descriptor.capabilities.thread_visits
                && environment.token.scope.as_deref().is_none_or(|scope| {
                    scope
                        .split_whitespace()
                        .any(|scope| scope == "orchestration:operate")
                }),
            terminal_idle_write: capabilities.terminal_idle_write
                && scope.is_some_and(|scopes| {
                    scopes
                        .split_whitespace()
                        .any(|scope| scope == "terminal:read")
                }),
            terminal_write_session: Uuid::new_v4(),
            terminals_enabled: environment
                .descriptor
                .capabilities
                .terminal
                .unwrap_or(!environment.descriptor.capabilities.read_only)
                && environment.token.scope.as_deref().is_none_or(|scope| {
                    scope
                        .split_whitespace()
                        .any(|scope| scope == "terminal:operate")
                }),
            terminals: BTreeMap::new(),
            pending_terminal: HashMap::new(),
            terminal_links: terminal_link::TerminalLinks::new(scope.is_some_and(can_operate)),
            browser_preview: browser_preview::Client::new(environment),
        };
        let request_id = session.allocate_id();
        session
            .send_request(&request_id, "orchestration.subscribeShell", shell_input)
            .await
            .map_err(SessionStop::into_message)?;
        session.shell_request_id = request_id;
        if capabilities.shell_http || capabilities.scratch_projects || !capabilities.read_only {
            session
                .subscribe_provider_catalog()
                .await
                .map_err(SessionStop::into_message)?;
        }
        if capabilities.clone_projects == Some(true) || (scope.is_some() && session.clone_writes) {
            session
                .subscribe_project_clones()
                .await
                .map_err(SessionStop::into_message)?;
        }
        if let Some(thread_id) = thread_id {
            session
                .open_thread(thread_id, false)
                .await
                .map_err(SessionStop::into_message)?;
        }
        for terminal in terminals
            .iter()
            .filter(|terminal| terminal_selection.includes(terminal.attachment))
        {
            session
                .start_terminal(terminal.clone())
                .await
                .map_err(SessionStop::into_message)?;
        }
        Ok(session)
    }

    async fn publish_initial_shell(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if !std::mem::take(&mut self.initial_shell) {
            return Ok(());
        }
        let (projects, threads) = self.shell.view();
        let needs_catalog = threads
            .iter()
            .any(|thread| thread.providers.current_instance().is_some());
        events.send(Event::Shell { projects, threads });
        if matches!(self.archive, ArchiveState::Needed) {
            self.load_archive().await?;
        }
        if (needs_catalog || self.project_writes) && self.providers_request_id.is_none() {
            self.subscribe_provider_catalog().await?;
        }
        Ok(())
    }

    async fn subscribe_provider_catalog(&mut self) -> Result<(), SessionStop> {
        let id = self.allocate_id();
        self.send_request(
            &id,
            "subscribeServerConfig",
            json!({
                "workspaceSnapshotsOnDemand": true,
                "providerCatalogsOnDemand": true,
                "providerCatalogDigests": true,
            }),
        )
        .await?;
        self.providers_request_id = Some(id);
        Ok(())
    }

    async fn send(&mut self, frame: Value) -> Result<(), SessionStop> {
        timeout(
            REQUEST_TIMEOUT,
            self.socket
                .send(WebSocketMessage::Text(frame.to_string().into())),
        )
        .await
        .map_err(|_| SessionStop::Socket("Timed out writing to the environment.".to_owned()))?
        .map_err(|error| {
            SessionStop::Socket(format!("Could not write to the environment: {error}"))
        })
    }

    async fn subscribe_project_clones(&mut self) -> Result<(), SessionStop> {
        if self.project_clones_request_id.is_none() {
            let request_id = self.allocate_id();
            self.send_request(&request_id, "subscribeProjectClones", json!({}))
                .await?;
            self.project_clones_request_id = Some(request_id);
        }
        Ok(())
    }

    async fn load_archive(&mut self) -> Result<(), SessionStop> {
        let request_id = self.allocate_id();
        self.send_request(
            &request_id,
            "orchestration.getArchivedShellSnapshot",
            json!({}),
        )
        .await?;
        self.archive = ArchiveState::Loading(request_id);
        Ok(())
    }

    async fn open_thread(&mut self, thread_id: String, opened: bool) -> Result<(), SessionStop> {
        self.close_command_outputs().await?;
        self.cancel_file_preview(None).await?;
        if self
            .terminals
            .values()
            .any(|terminal| terminal.intent.attach.target.thread_id != thread_id)
        {
            self.drop_terminals().await?;
        }
        let request_id = self.allocate_id();
        let setup_request_id = self.worktree_setup.then(|| self.allocate_id());
        let previous = self.thread.replace(ThreadSubscription {
            request_id: request_id.clone(),
            thread_id: thread_id.clone(),
            conversation: Conversation::default(),
            reported_queue: Vec::new(),
            reported_resumable_run: None,
            reported_approvals: Vec::new(),
            reported_questions: None,
            reported_stop: None,
            reported_queue_worktree: false,
            reported_codex_reasoning: false,
            actions_request_id: None,
            actions_refresh_needed: false,
            setup_request_id: setup_request_id.clone(),
        });
        if let Some(previous) = previous {
            self.interrupt(&previous.request_id).await?;
            if let Some(setup) = previous.setup_request_id {
                self.interrupt(&setup).await?;
            }
        }
        if opened
            && self.thread_visits
            && let Ok(visited_at) = OffsetDateTime::now_utc().format(&Rfc3339)
        {
            let visit_request = self.allocate_id();
            self.send_request(
                &visit_request,
                "orchestration.dispatchCommand",
                json!({
                    "type": "thread.visit",
                    "threadId": thread_id,
                    "commandId": Uuid::new_v4().to_string(),
                    "visitedAt": visited_at,
                    "opened": true,
                }),
            )
            .await?;
        }
        self.send_request(
            &request_id,
            "orchestration.subscribeThread",
            json!({"threadId": thread_id}),
        )
        .await?;
        if let Some(setup) = setup_request_id {
            self.send_request(
                &setup,
                "subscribeWorktreeSetup",
                json!({"threadId":thread_id}),
            )
            .await?;
        }
        Ok(())
    }

    async fn close_thread(&mut self) -> Result<(), SessionStop> {
        self.close_command_outputs().await?;
        self.cancel_file_preview(None).await?;
        self.drop_terminals().await?;
        if let Some(thread) = self.thread.take() {
            self.interrupt(&thread.request_id).await?;
            if let Some(setup) = thread.setup_request_id {
                self.interrupt(&setup).await?;
            }
        }
        Ok(())
    }

    async fn refresh_thread_actions(&mut self) -> Result<(), SessionStop> {
        if !self.worktree_queue && !(self.native_stop && self.stop_writes) {
            return Ok(());
        }
        let request_id = self.allocate_id();
        let Some(thread) = &mut self.thread else {
            return Ok(());
        };
        if thread.actions_request_id.is_some() {
            thread.actions_refresh_needed = true;
            return Ok(());
        }
        thread.actions_request_id = Some(request_id.clone());
        let thread_id = thread.thread_id.clone();
        let tag = if self.worktree_queue {
            "t3.threadActions"
        } else {
            "t3.canStopThread"
        };
        self.send_request(&request_id, tag, json!({"threadId":thread_id}))
            .await
    }

    async fn interrupt(&mut self, request_id: &str) -> Result<(), SessionStop> {
        self.send(json!({"_tag": "Interrupt", "requestId": request_id}))
            .await
    }

    fn allocate_id(&mut self) -> String {
        let id = self.next_request_id.to_string();
        self.next_request_id += 1;
        id
    }

    async fn send_request(
        &mut self,
        id: &str,
        tag: &str,
        payload: Value,
    ) -> Result<(), SessionStop> {
        self.send(json!({
            "_tag": "Request",
            "id": id,
            "tag": tag,
            "payload": payload,
            "headers": [],
        }))
        .await
    }

    async fn load_git_ref_page(&mut self, request: GitRefsRequest) -> Result<(), SessionStop> {
        let mut payload = json!({"cwd":request.cwd,"limit":200});
        if !self.remote_worktree_launch {
            payload["refKind"] = json!("local");
        }
        if let Some(cursor) = request.cursor {
            payload["cursor"] = json!(cursor);
        } else if request.refresh {
            payload["refresh"] = json!(true);
        }
        let id = self.allocate_id();
        self.pending_git_refs.insert(id.clone(), request);
        self.send_request(&id, "vcs.listRefs", payload).await
    }

    async fn command(
        &mut self,
        command: Command,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        match command {
            Command::StorageCleanup(intent) => self.storage_cleanup(intent, events).await?,
            Command::EnsureWorkspaceSettings => {
                if self.providers_request_id.is_none() {
                    self.subscribe_provider_catalog().await?;
                }
            }
            Command::ReadWorkspaceProjectFile {
                project_id,
                operation,
            } => {
                self.load_workspace_project_file(project_id, operation, events)
                    .await?;
            }
            Command::UpdateWorkspaceSetting {
                target,
                edit,
                operation,
            } => {
                self.save_workspace_setting(target, edit, operation, events)
                    .await?;
            }
            Command::EditKeybinding { edit, operation } => {
                self.edit_keybinding(edit, operation, events).await?;
            }
            Command::RefreshProviderCatalog => {
                if let Some(request) = self.providers_request_id.take() {
                    self.interrupt(&request).await?;
                }
                self.subscribe_provider_catalog().await?;
            }
            Command::LoadProjectFavicons(targets) => {
                self.favicons.enqueue(targets);
                self.request_project_favicons().await?;
            }
            Command::OpenThread(thread_id) => self.open_thread(thread_id, true).await?,
            Command::CloseThread => self.close_thread().await?,
            Command::ObserveDraftWorktree(thread_id) => {
                self.observe_draft_worktree(thread_id).await?;
            }
            Command::RetryWorkspacePreparation(retry) => {
                self.submit_workspace_retry(retry, events).await?;
            }
            Command::CancelWorktreeSetup(thread_id) => {
                if self.worktree_setup && self.worktree_launch {
                    let id = self.allocate_id();
                    let payload = json!({"threadId":thread_id});
                    self.pending_setup_cancels
                        .insert(id.clone(), (thread_id, Instant::now()));
                    self.send_request(&id, "worktreeSetup.cancel", payload)
                        .await?;
                } else {
                    events.send(Event::WorktreeSetupCancelSettled {
                        thread_id,
                        result: Err("This session cannot cancel workspace setup.".into()),
                    });
                }
            }
            Command::RenameThread { thread_id, title } => {
                if self.launch_writes {
                    let id = self.allocate_id();
                    let payload = json!({"type":"thread.metadata.update","commandId":Uuid::new_v4().to_string(),"threadId":thread_id,"title":title});
                    self.pending_title_edits
                        .insert(id.clone(), (thread_id, Instant::now()));
                    self.send_request(&id, "orchestration.dispatchCommand", payload)
                        .await?;
                } else {
                    events.send(Event::RequestError {
                        thread_id,
                        action: ThreadAction::Rename,
                        message: "This session cannot rename threads.".into(),
                    });
                }
            }
            Command::ArchiveAction {
                action,
                thread_id,
                operation,
            } => {
                self.submit_archive_action(action, thread_id, operation, events)
                    .await?
            }
            Command::SetSavedReasoning(edit) => self.set_saved_reasoning(edit, events).await?,
            Command::WorktreeRemoval { operation, request } => {
                self.submit_worktree_removal(operation, request, events)
                    .await?
            }
            Command::StopThread(thread_id) => {
                let locally_available = self.thread.as_ref().is_some_and(|thread| {
                    thread.thread_id == thread_id && thread.reported_stop == Some(true)
                });
                if !self.stop_writes || (self.native_stop && !locally_available) {
                    events.send(Event::ThreadStopFailed {
                        thread_id,
                        message: "This thread cannot be stopped right now.".into(),
                    });
                } else if !self
                    .pending_stops
                    .values()
                    .any(|(pending, _)| pending == &thread_id)
                {
                    let id = self.allocate_id();
                    let payload = json!({"type":"thread.stop","commandId":Uuid::new_v4().to_string(),"threadId":thread_id});
                    self.pending_stops
                        .insert(id.clone(), (thread_id, Instant::now()));
                    self.send_request(&id, "orchestration.dispatchCommand", payload)
                        .await?;
                }
            }
            Command::Terminal(command) => self.terminal_command(command, events).await?,
            Command::BrowserPreview(command) => {
                self.browser_preview_command(command, events).await?
            }
            Command::TerminalFileLink(request) => self.terminal_file_link(request, events).await?,
            Command::ObserveOutput(command) => self.command_output(command, events).await?,
            Command::RespondToRuntimeRequest(response) => {
                self.respond_to_runtime_request(response, events).await?
            }
            Command::SearchProjectImages {
                search_id,
                cwd,
                query,
            } => {
                if self.filesystem_read {
                    if let Some((previous, ..)) = self.pending_project_images.take() {
                        self.interrupt(&previous).await?;
                    }
                    let id = self.allocate_id();
                    self.pending_project_images = Some((id.clone(), search_id, Instant::now()));
                    self.send_request(
                        &id,
                        "projects.searchEntries",
                        json!({
                            "cwd": cwd,
                            "query": query,
                            "limit": 200,
                            "imageOnly": true,
                        }),
                    )
                    .await?;
                } else {
                    events.send(Event::ProjectImages {
                        search_id,
                        result: Err("This connection cannot search host files.".into()),
                    });
                }
            }
            Command::BrowseFolder {
                browse_id,
                partial_path,
                cwd,
            } => {
                if self.filesystem_read {
                    if let Some((previous, ..)) = self.pending_folder.take() {
                        self.interrupt(&previous).await?;
                    }
                    let id = self.allocate_id();
                    let mut input = json!({ "partialPath": partial_path });
                    if let Some(cwd) = cwd {
                        input["cwd"] = cwd.into();
                    }
                    self.pending_folder = Some((id.clone(), browse_id, Instant::now()));
                    self.send_request(&id, "filesystem.browse", input).await?;
                } else {
                    events.send(Event::FolderBrowsed {
                        browse_id,
                        result: Err("This session cannot browse folders on the environment.".into()),
                    });
                }
            }
            Command::ReadFilePreview(input) => self.read_file_preview(input, events).await?,
            Command::PrepareFileAsset { input, purpose } => {
                self.prepare_file_asset(input, purpose, events).await?;
            }
            Command::CancelFilePreview(id) => self.cancel_file_preview(Some(id)).await?,
            Command::AddProject {
                command_id,
                workspace_root,
                title,
            } => {
                if !self.project_writes {
                    events.send(Event::ProjectAdded {
                        command_id,
                        result: Err("This environment does not allow changes to projects.".into()),
                    });
                } else {
                    let request_id = self.allocate_id();
                    let payload = json!({
                        "type": "project.create",
                        "commandId": command_id,
                        "projectId": Uuid::new_v4(),
                        "workspaceRoot": workspace_root,
                        "title": title,
                        "createWorkspaceRootIfMissing": true,
                        "defaultModelSelection": null,
                    });
                    self.pending_projects.insert(
                        request_id.clone(),
                        (command_id, Instant::now(), ProjectCreation::Folder),
                    );
                    self.send_request(&request_id, "projects.mutate", payload)
                        .await?;
                }
            }
            Command::CreateNamedProject { command_id, name } => {
                if !self.project_writes || !self.named_projects {
                    events.send(Event::ProjectAdded {
                        command_id,
                        result: Err(
                            "This environment does not allow creating a named project.".into()
                        ),
                    });
                } else {
                    let id = self.allocate_id();
                    self.pending_projects.insert(
                        id.clone(),
                        (command_id, Instant::now(), ProjectCreation::Named),
                    );
                    self.send_request(&id, "projects.createNew", json!({"name": name}))
                        .await?;
                }
            }
            Command::StartScratchProject(command_id) => {
                if !self.project_writes || !self.scratch_projects {
                    events.send(Event::ProjectAdded {
                        command_id,
                        result: Err(
                            "This environment cannot start a thread without a project.".into()
                        ),
                    });
                } else {
                    let request = self.allocate_id();
                    self.pending_projects.insert(
                        request.clone(),
                        (command_id, Instant::now(), ProjectCreation::Scratch),
                    );
                    self.send_request(&request, "projects.ensureScratch", json!({}))
                        .await?;
                }
            }
            Command::EditProject { project_id, edit } => {
                let kind = edit.kind();
                if !self.project_writes
                    || matches!(
                        kind,
                        ProjectEditKind::Pinned
                            | ProjectEditKind::Archived
                            | ProjectEditKind::UnarchiveFromPicker
                    ) && !self.project_organization
                    || self.shell.project(&project_id).is_none()
                {
                    events.send(Event::ProjectEditSettled {
                        project_id,
                        kind,
                        result: Err("The project is no longer available for editing.".into()),
                    });
                } else {
                    let request = self.allocate_id();
                    let mut payload = json!({"type": "project.update", "projectId": project_id,
                                             "commandId": Uuid::new_v4().to_string()});
                    edit.apply(&mut payload);
                    self.pending_project_edits
                        .insert(request.clone(), (project_id, kind, Instant::now()));
                    self.send_request(&request, "projects.mutate", payload)
                        .await?;
                }
            }
            Command::CloneProject {
                project_id,
                remote_url,
                destination_path,
                title,
            } => {
                if !self.clone_writes {
                    events.send(Event::ProjectAdded {
                        command_id: project_id,
                        result: Err("This session cannot clone repositories.".into()),
                    });
                } else {
                    self.subscribe_project_clones().await?;
                    let id = self.allocate_id();
                    let payload = json!({
                        "projectId": project_id,
                        "title": title,
                        "createdAt": OffsetDateTime::now_utc().format(&Rfc3339).expect("valid clone timestamp"),
                        "remoteUrl": remote_url,
                        "destinationPath": destination_path,
                    });
                    self.pending_projects.insert(
                        id.clone(),
                        (project_id, Instant::now(), ProjectCreation::Clone),
                    );
                    self.send_request(&id, "projectClone.start", payload)
                        .await?;
                }
            }
            Command::ProjectCloneAction { project_id, action } => {
                let remove = matches!(action, ProjectCloneAction::Remove);
                let allowed = if remove {
                    self.project_writes
                } else {
                    self.clone_writes
                };
                if !allowed {
                    events.send(Event::ProjectCloneError(
                        "This session cannot manage repository clones.".into(),
                    ));
                } else {
                    let id = self.allocate_id();
                    let method = match action {
                        ProjectCloneAction::Cancel => "projectClone.cancel",
                        ProjectCloneAction::Retry => "projectClone.retry",
                        ProjectCloneAction::Remove => "projects.mutate",
                    };
                    self.pending_project_clone_actions
                        .insert(id.clone(), Instant::now());
                    let payload = if remove {
                        json!({"type":"project.delete","projectId":project_id,"commandId":Uuid::new_v4().to_string()})
                    } else {
                        json!({"projectId":project_id})
                    };
                    self.send_request(&id, method, payload).await?;
                }
            }
            Command::LoadDraftPreferences(project_id) => {
                if self.worktree_launch && !self.workspace_default_pending(&project_id) {
                    let id = self.allocate_id();
                    self.pending_workspace_defaults
                        .insert(id.clone(), (project_id, Instant::now()));
                    self.send_request(&id, "server.getSettings", json!({}))
                        .await?;
                }
            }
            Command::ListGitRefs {
                project_id,
                cwd,
                refresh,
            } => {
                if self.worktree_launch {
                    self.load_git_ref_page(GitRefsRequest {
                        project_id,
                        cwd,
                        refresh,
                        started: Instant::now(),
                        cursor: None,
                        branches: Vec::new(),
                    })
                    .await?;
                } else {
                    events.send(Event::GitRefs {
                        project_id,
                        result: Err("This session cannot select a worktree.".into()),
                    });
                }
            }
            Command::ChangeGitCheckout {
                project_id,
                cwd,
                name,
                create,
            } => {
                if self.checkout_switch {
                    let id = self.allocate_id();
                    self.pending_git_checkouts
                        .insert(id.clone(), (project_id, create));
                    let mut payload = json!({"cwd":cwd,"refName":name});
                    let tag = if create {
                        payload["switchRef"] = json!(true);
                        "vcs.createRef"
                    } else {
                        "vcs.switchRef"
                    };
                    self.send_request(&id, tag, payload).await?;
                } else {
                    events.send(Event::GitCheckout {
                        project_id,
                        result: Err("This session cannot switch Git branches.".into()),
                    });
                }
            }
            Command::LaunchThread(draft) => {
                let validation_error = if !self.launch_writes {
                    Some("This session cannot launch a thread.")
                } else if !self.worktree_launch
                    && matches!(&draft.workspace, LaunchWorkspace::Worktree { .. })
                {
                    Some("This session cannot launch a worktree.")
                } else if draft.prompt.trim().is_empty() {
                    Some("Write a message first.")
                } else {
                    self.launch_project(&draft.project_id).err()
                };
                if let Some(message) = validation_error {
                    events.send(LaunchStage::Config(draft).failure(message));
                } else {
                    let id = self.allocate_id();
                    self.pending_launches
                        .insert(id.clone(), (LaunchStage::Config(draft), Instant::now()));
                    self.send_request(&id, "server.getSettings", json!({}))
                        .await?;
                }
            }
            Command::SendPrompt {
                thread_id,
                prompt,
                context,
            } => {
                if !self.prompt_writes || prompt.trim().is_empty() {
                    let message = if !self.prompt_writes {
                        "This environment is read-only."
                    } else {
                        "Write a message first."
                    };
                    events.send(Event::RequestError {
                        thread_id,
                        action: ThreadAction::Send,
                        message: message.to_owned(),
                    });
                } else {
                    let request_id = self.allocate_id();
                    let queue = self.worktree_queue
                        && self.thread.as_ref().is_some_and(|thread| {
                            thread.thread_id == thread_id && thread.reported_queue_worktree
                        });
                    let mut payload = prompt_payload(&thread_id, &prompt, queue, context.as_ref());
                    if !queue
                        && self.thread.as_ref().is_some_and(|thread| {
                            thread.thread_id == thread_id && thread.reported_codex_reasoning
                        })
                        && let Some(selection) = self.shell.saved_codex_model(&thread_id)
                    {
                        payload["modelSelection"] = json!(selection);
                    }
                    self.pending_prompts.insert(request_id.clone(), thread_id);
                    self.send_request(&request_id, "orchestration.dispatchCommand", payload)
                        .await?;
                }
            }
            Command::MutateQueue {
                thread_id,
                mutation,
                operation,
            } => {
                let available = self.worktree_queue
                    && self.thread.as_ref().is_some_and(|thread| {
                        thread.thread_id == thread_id
                            && thread
                                .reported_queue
                                .iter()
                                .any(|message| mutation.available(message))
                    });
                if !available {
                    events.send(Event::QueueMutationSettled {
                        thread_id,
                        mutation,
                        operation,
                        result: Err("The queued message is no longer available.".into()),
                    });
                } else {
                    let request_id = self.allocate_id();
                    let payload = mutation.payload(&thread_id, operation);
                    self.pending_queue_changes
                        .insert(request_id.clone(), (thread_id, mutation, operation));
                    self.send_request(&request_id, "orchestration.dispatchCommand", payload)
                        .await?;
                }
            }
            Command::ResumeThread {
                thread_id,
                source_run,
                operation,
            } => {
                self.submit_resume(thread_id, source_run, operation, events)
                    .await?;
            }
        }
        Ok(())
    }

    async fn respond_to_runtime_request(
        &mut self,
        response: RuntimeResponse,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let (thread_id, runtime_request_id, command_id) = response.identify();
        let error = if !self.runtime_responses {
            Some("This session cannot respond to requests.")
        } else if !self.thread.as_ref().is_some_and(|thread| {
            thread.thread_id == thread_id && response.allows(&thread.conversation)
        }) {
            Some("This request or response is no longer available. Check the conversation.")
        } else if let Some(pending_id) =
            self.pending_runtime_responses
                .values()
                .find_map(|(pending, _)| {
                    let (pending_thread, pending_request, pending_id) = pending.identify();
                    (pending_thread == thread_id && pending_request == runtime_request_id)
                        .then_some(pending_id)
                })
        {
            if pending_id == command_id {
                return Ok(());
            }
            Some("A response to this request is already in progress.")
        } else {
            None
        };
        if let Some(message) = error {
            events.send(response.finish(RuntimeResponseOutcome::Rejected(message.to_owned())));
            return Ok(());
        }
        let request_id = self.allocate_id();
        let payload = response.payload();
        self.pending_runtime_responses
            .insert(request_id.clone(), (response, Instant::now()));
        self.send_request(&request_id, "orchestration.dispatchCommand", payload)
            .await
    }

    fn unconfirm_runtime_responses(&mut self, events: &EventSender<impl Fn()>) {
        for (_, (response, _)) in self.pending_runtime_responses.drain() {
            events.send(response.finish(RuntimeResponseOutcome::Unconfirmed));
        }
    }

    fn finish_interrupted_ref_requests(&mut self, events: &EventSender<impl Fn()>) {
        for (_, request) in self.pending_git_refs.drain() {
            events.send(Event::GitRefs {
                project_id: request.project_id,
                result: Err("Connection lost while loading Git refs.".into()),
            });
        }
        for (_, (project_id, _)) in self.pending_git_checkouts.drain() {
            events.send(Event::GitCheckout {
                project_id,
                result: Err("The connection closed before Git confirmed the checkout.".into()),
            });
        }
    }

    fn launch_project(&self, project_id: &str) -> Result<&projection::ProjectRow, &'static str> {
        self.shell
            .project(project_id)
            .filter(|project| !project.is_archived())
            .ok_or("The project is no longer available for new threads.")
    }

    async fn submit_launch(
        &mut self,
        project_id: String,
        thread_id: String,
        payload: Value,
    ) -> Result<(), SessionStop> {
        let id = self.allocate_id();
        self.pending_launches.insert(
            id.clone(),
            (
                LaunchStage::Submitted {
                    project_id,
                    thread_id,
                },
                Instant::now(),
            ),
        );
        self.send_request(&id, "orchestration.launchThread", payload)
            .await
    }

    async fn finish_launch_step(
        &mut self,
        stage: LaunchStage,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        match stage {
            LaunchStage::Config(draft) => {
                let thread_id = draft.thread_id.clone();
                let payload = result.and_then(|config| {
                    self.launch_project(&draft.project_id)
                        .map_err(str::to_owned)
                        .and_then(|project| launch::payload(config, project, &draft))
                });
                match payload {
                    Ok(payload) if !payload["modelSelection"].is_null() =>
                        self.submit_launch(draft.project_id, thread_id, payload).await?,
                    Ok(payload) if self.native_model_discovery => {
                        let id = self.allocate_id();
                        let input = json!({"projectId":draft.project_id});
                        self.pending_launches.insert(id.clone(), (LaunchStage::DefaultModel {
                            project_id: draft.project_id, thread_id, payload,
                        }, Instant::now()));
                        self.send_request(&id, "t3.getDefaultCodexModel", input).await?;
                    }
                    Ok(_) => events.send(Event::LaunchError {
                        project_id: draft.project_id,
                        message: "Set a default model for this project or environment before starting a thread.".into(),
                    }),
                    Err(message) => events.send(Event::LaunchError {
                        project_id: draft.project_id,
                        message,
                    }),
                }
            }
            LaunchStage::DefaultModel {
                project_id,
                thread_id,
                mut payload,
            } => {
                let model = result.and_then(|value| {
                    serde_json::from_value::<launch::ModelSelection>(value)
                        .ok()
                        .filter(launch::ModelSelection::is_valid)
                        .ok_or_else(|| "The environment returned an invalid default model.".into())
                });
                match model {
                    Ok(model) => {
                        payload["modelSelection"] = json!(model);
                        self.submit_launch(project_id, thread_id, payload).await?;
                    }
                    Err(message) => events.send(Event::LaunchError {
                        project_id,
                        message,
                    }),
                }
            }
            LaunchStage::Submitted {
                project_id,
                thread_id,
            } => {
                events.send(match result {
                    Ok(value) if value["threadId"].as_str() == Some(thread_id.as_str()) => {
                        Event::ThreadLaunched {
                            project_id,
                            thread_id,
                        }
                    }
                    Ok(_) => Event::LaunchError {
                        project_id,
                        message: LAUNCH_UNCONFIRMED.to_owned(),
                    },
                    Err(message) => Event::LaunchError {
                        project_id,
                        message,
                    },
                });
            }
        }
        Ok(())
    }

    fn workspace_default_pending(&self, project_id: &str) -> bool {
        self.pending_named_project_defaults.contains_key(project_id)
            || self
                .pending_workspace_defaults
                .values()
                .any(|(id, _)| id == project_id)
            || self
                .pending_project_file_defaults
                .values()
                .any(|(id, _, _)| id == project_id)
    }

    async fn workspace_settings_exit(
        &mut self,
        project_id: String,
        exit: RpcExit,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let Some(project) = self.shell.project(&project_id) else {
            return Ok(());
        };
        let settings = exit.into_result().unwrap_or(Value::Null);
        if self.native_model_discovery {
            events.send(Event::DraftModelSettings {
                project_id: project_id.clone(),
                policy: launch::draft_model_policy(&settings, project, &project_id),
            });
        }
        let preferences = draft_defaults::Preferences::from_settings(
            &settings,
            &project_id,
            project.default_thread_env_mode,
        );
        if preferences.needs_project_file() && self.filesystem_read {
            let payload = json!({"cwd": project.workspace_path(), "relativePath": "t3.json", "allowMissing": true});
            let id = self.allocate_id();
            self.pending_project_file_defaults
                .insert(id.clone(), (project_id, preferences, Instant::now()));
            self.send_request(&id, "projects.readFile", payload).await?;
        } else if !self.named_projects_root_loaded
            && preferences.needs_named_projects_root(self.shell.project_has_threads(&project_id))
        {
            self.pending_named_project_defaults
                .insert(project_id, preferences);
            if self.providers_request_id.is_none() {
                self.subscribe_provider_catalog().await?;
            }
        } else {
            self.publish_draft_defaults(project_id, preferences, None, events);
        }
        Ok(())
    }

    fn finish_named_project_defaults(&mut self, events: &EventSender<impl Fn()>) {
        self.named_projects_root_loaded = true;
        for (project_id, preferences) in std::mem::take(&mut self.pending_named_project_defaults) {
            self.publish_draft_defaults(project_id, preferences, None, events);
        }
    }

    fn publish_draft_defaults(
        &self,
        project_id: String,
        preferences: draft_defaults::Preferences,
        file: Option<WorkspaceMode>,
        events: &EventSender<impl Fn()>,
    ) {
        if let Some(project) = self.shell.project(&project_id) {
            let mode = preferences.effective_mode(
                project.workspace_path(),
                self.named_projects_root.as_deref(),
                self.shell.project_has_threads(&project_id),
                file,
            );
            events.send(Event::DraftWorkspaceDefault {
                project_id,
                mode,
                start_from_origin: preferences.start_from_origin,
            });
        }
    }

    async fn receive(
        &mut self,
        frame: ServerFrame,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        match frame {
            ServerFrame::Chunk { request_id, values } => {
                if let Some(thread_id) = self.setup_thread(&request_id) {
                    for value in values {
                        if let Ok(snapshot) =
                            serde_json::from_value::<Option<Box<WorktreeSetup>>>(value)
                            && snapshot
                                .as_ref()
                                .is_none_or(|snapshot| snapshot.thread_id == thread_id)
                        {
                            events.send(Event::WorktreeSetup {
                                thread_id: thread_id.to_owned(),
                                snapshot,
                            });
                        }
                    }
                    return Ok(());
                }
                if self.browser_preview.is_stream(&request_id) {
                    self.apply_preview_events(values, events);
                    return Ok(());
                }
                let mut subscribe_providers = false;
                if request_id == self.shell_request_id {
                    if matches!(self.archive, ArchiveState::Loaded)
                        && values.iter().any(|item| match item["kind"].as_str() {
                            Some("thread.removed" | "project.removed") => true,
                            Some("thread.updated") => {
                                item["location"] == "archive"
                                    || item["thread"]["id"]
                                        .as_str()
                                        .is_some_and(|id| self.shell.contains_archived_thread(id))
                            }
                            _ => false,
                        })
                    {
                        self.archive = ArchiveState::Needed;
                    }
                    if apply(values, |value| self.shell.apply(value))
                        .map_err(SessionStop::Protocol)?
                    {
                        let (projects, threads) = self.shell.view();
                        subscribe_providers = self.providers_request_id.is_none()
                            && threads
                                .iter()
                                .any(|thread| thread.providers.current_instance().is_some());
                        events.send(Event::Shell { projects, threads });
                    }
                    self.finish_projected_saved_reasoning(events);
                } else if self.project_clones_request_id.as_ref() == Some(&request_id) {
                    if let Some(clones) = values
                        .into_iter()
                        .rev()
                        .find_map(|snapshot| serde_json::from_value(snapshot).ok())
                    {
                        events.send(Event::ProjectClones(clones));
                    }
                } else if self.providers_request_id.as_ref() == Some(&request_id) {
                    if let Some(config) = values.iter().rev().find_map(|event| {
                        (event["version"] == 1 && event["type"] == "snapshot")
                            .then(|| event["config"].as_object())
                            .flatten()
                    }) {
                        self.terminal_links.editors =
                            terminal_link::config_editors(config.get("availableEditors"));
                        let root = config
                            .get("scratchWorkspaceRoot")
                            .and_then(Value::as_str)
                            .filter(|path| !path.is_empty())
                            .map(str::to_owned);
                        self.named_projects_root = config
                            .get("newProjectsRoot")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        self.finish_named_project_defaults(events);
                        self.scratch_projects =
                            root.is_some() && self.project_writes && self.launch_writes;
                        events.send(Event::ScratchWorkspace {
                            root,
                            can_start: self.scratch_projects,
                        });
                    }
                    self.storage_cleanup.apply_config(&values, events);
                    if let Some(settings) = self.workspace_settings.apply(&values) {
                        events.send(Event::WorkspaceConfiguration(Some(Box::new(settings))));
                    }
                    if let Some(bindings) =
                        Keybindings::config_events(&values, self.keybindings_writable)
                    {
                        events.send(Event::Keybindings(bindings));
                    }
                    if let Some(providers) = provider_metadata::last_catalog(values) {
                        events.send(Event::Providers(providers));
                    }
                } else if self.storage_cleanup.receives(&request_id) {
                    if !self.storage_cleanup.chunk(values, events) {
                        self.stop_invalid_cleanup_report().await?;
                    }
                } else if let Some(attachment) = self.terminal_attachment(&request_id) {
                    self.terminal_chunk(attachment, values, events).await?;
                } else if self.command_outputs.matches(&request_id) {
                    self.command_outputs.chunk(&request_id, values, events);
                } else if let Some(thread) = self
                    .thread
                    .as_mut()
                    .filter(|thread| thread.request_id == request_id)
                {
                    match apply(values, |value| thread.conversation.apply(value)) {
                        Ok(true) => {
                            events.send(Event::Thread {
                                thread_id: thread.thread_id.clone(),
                                entries: thread.conversation.entries(),
                            });
                            if thread.reported_queue != thread.conversation.queued()
                                || thread.reported_resumable_run.as_deref()
                                    != thread.conversation.resumable_run()
                            {
                                thread.reported_queue = thread.conversation.queued().to_vec();
                                thread.reported_resumable_run =
                                    thread.conversation.resumable_run().map(str::to_owned);
                                events.send(Event::QueuedMessages {
                                    thread_id: thread.thread_id.clone(),
                                    messages: thread.reported_queue.clone(),
                                    resumable_run: thread.reported_resumable_run.clone(),
                                });
                            }
                            let approvals = thread.conversation.approvals.visible();
                            if approvals != thread.reported_approvals {
                                thread.reported_approvals = approvals.clone();
                                events.send(Event::ThreadApprovals {
                                    thread_id: thread.thread_id.clone(),
                                    approvals,
                                    can_respond: self.runtime_responses,
                                });
                            }
                            let questions = thread.conversation.questions.visible();
                            if thread.reported_questions.as_ref() != Some(&questions) {
                                thread.reported_questions = Some(questions.clone());
                                events.send(Event::ThreadQuestions {
                                    thread_id: thread.thread_id.clone(),
                                    questions,
                                    can_respond: self.runtime_responses,
                                });
                            }
                        }
                        Ok(false) => {}
                        Err(message) => {
                            let thread_id = thread.thread_id.clone();
                            self.thread = None;
                            self.interrupt(&request_id).await?;
                            events.send(Event::RequestError {
                                thread_id,
                                action: ThreadAction::Open,
                                message,
                            });
                            return Ok(());
                        }
                    }
                    self.refresh_thread_actions().await?;
                }
                self.send(json!({"_tag": "Ack", "requestId": request_id}))
                    .await?;
                if request_id == self.shell_request_id
                    && matches!(self.archive, ArchiveState::Needed)
                {
                    self.load_archive().await?;
                }
                if subscribe_providers {
                    self.subscribe_provider_catalog().await?;
                }
            }
            ServerFrame::Exit { request_id, exit } => {
                if self.storage_cleanup.receives_exit(&request_id) {
                    self.storage_cleanup
                        .exit(&request_id, exit.into_result(), events);
                    return Ok(());
                }
                if self.matches_file_preview(&request_id) {
                    self.finish_file_preview(exit, events);
                    return Ok(());
                }
                if self.keybinding_write_matches(&request_id) {
                    self.keybinding_write_result(exit.into_result(), events);
                    return Ok(());
                }
                if self.workspace_project_file_matches(&request_id) {
                    self.workspace_project_file_result(exit.into_result(), events);
                    return Ok(());
                }
                if self.workspace_write_matches(&request_id) {
                    self.workspace_write_result(exit.into_result(), events);
                    return Ok(());
                }
                if self.providers_request_id.as_ref() == Some(&request_id) {
                    self.providers_request_id = None;
                    events.send(Event::WorkspaceConfiguration(None));
                    events.send(Event::KeybindingsUnavailable);
                    if exit.into_result().is_err() {
                        events.send(Event::ProviderCatalogUnavailable);
                    }
                    self.finish_named_project_defaults(events);
                    return Ok(());
                }
                if self.pending_workspace_retries.contains_key(&request_id) {
                    self.workspace_retry_exit(&request_id, exit, events);
                    return Ok(());
                }
                if self.favicons.has_grant(&request_id) {
                    self.favicons
                        .resolve(&request_id, exit.into_result(), events);
                    return self.request_project_favicons().await;
                }
                if self
                    .draft_setup
                    .as_ref()
                    .is_some_and(|(request, _)| request == &request_id)
                {
                    self.draft_setup = None;
                    return Ok(());
                }
                if self.command_outputs.matches(&request_id) {
                    self.command_outputs
                        .exit(&request_id, exit.into_result(), events);
                    return Ok(());
                }
                if let Some((project_id, create)) = self.pending_git_checkouts.remove(&request_id) {
                    let result = if create && let Some(message) = exit.create_ref_error() {
                        Err(message)
                    } else {
                        exit.into_result()
                    }
                    .and_then(|value| match value.get("refName") {
                        Some(Value::Null) if !create => Ok(None),
                        Some(Value::String(name)) if !name.is_empty() => Ok(Some(name.clone())),
                        _ => Err("The environment returned an invalid Git checkout result.".into()),
                    });
                    events.send(Event::GitCheckout { project_id, result });
                    return Ok(());
                }
                if let Some((thread_id, _)) = self.pending_setup_cancels.remove(&request_id) {
                    events.send(Event::WorktreeSetupCancelSettled {
                        thread_id,
                        result: exit.into_result().and_then(|value| {
                            value["cancelled"].as_bool().ok_or_else(|| {
                                "The environment returned an invalid setup cancellation result."
                                    .into()
                            })
                        }),
                    });
                    return Ok(());
                }
                if self.pending_saved_reasoning.contains_key(&request_id) {
                    self.saved_reasoning_receipt(&request_id, exit.into_result(), events);
                    return Ok(());
                }
                if let Some((thread_id, _)) = self.pending_title_edits.remove(&request_id) {
                    if let Err(message) = exit.into_result() {
                        events.send(Event::RequestError {
                            thread_id,
                            action: ThreadAction::Rename,
                            message,
                        });
                    }
                    return Ok(());
                }
                if let Some((project_id, _)) = self.pending_workspace_defaults.remove(&request_id) {
                    self.workspace_settings_exit(project_id, exit, events)
                        .await?;
                    return Ok(());
                }
                if let Some((project_id, preferences, _)) =
                    self.pending_project_file_defaults.remove(&request_id)
                {
                    let file = exit.into_result().ok().and_then(|value| {
                        if value["exists"] == false || value["truncated"] == true {
                            return None;
                        }
                        crate::server::project_file::workspace_mode(value["contents"].as_str()?)
                    });
                    self.publish_draft_defaults(project_id, preferences, file, events);
                    return Ok(());
                }
                if let Some(mut request) = self.pending_git_refs.remove(&request_id) {
                    let result = exit.into_result().and_then(|value| {
                        serde_json::from_value::<GitRefs>(value)
                            .map_err(|_| "The environment returned an invalid Git ref list.".into())
                    });
                    let result = match result {
                        Ok(mut page) => {
                            request.branches.append(&mut page.branches);
                            if let Some(cursor) = page.next_cursor {
                                if cursor <= request.cursor.unwrap_or_default() {
                                    Err("The environment returned an invalid Git ref cursor."
                                        .into())
                                } else {
                                    request.cursor = Some(cursor);
                                    return self.load_git_ref_page(request).await;
                                }
                            } else {
                                page.branches = request.branches;
                                Ok(page)
                            }
                        }
                        Err(error) => Err(error),
                    };
                    events.send(Event::GitRefs {
                        project_id: request.project_id,
                        result,
                    });
                    return Ok(());
                }
                if matches!(&self.archive, ArchiveState::Loading(id) if id == &request_id) {
                    self.archive = ArchiveState::Loaded;
                    match exit.into_result() {
                        Ok(snapshot) => match self.shell.apply_archive_snapshot(snapshot) {
                            Ok(true) => {
                                let (projects, threads) = self.shell.view();
                                events.send(Event::Shell { projects, threads });
                                events.send(Event::ArchivedThreads(Ok(self
                                    .shell
                                    .archived_threads())));
                            }
                            Ok(false) => self.load_archive().await?,
                            Err(error) => {
                                events.send(Event::ArchivedThreads(Err(error.to_string())))
                            }
                        },
                        Err(error) => events.send(Event::ArchivedThreads(Err(error))),
                    }
                    return Ok(());
                }
                if let Some(action) = self.pending_worktree_removals.remove(&request_id) {
                    action.finish(exit.into_result(), events);
                    return Ok(());
                }
                if let Some(action) = self.pending_archive_actions.remove(&request_id) {
                    return self
                        .finish_archive_action(action, exit.into_result(), events)
                        .await;
                }
                if let Some(thread) = self
                    .thread
                    .as_mut()
                    .filter(|thread| thread.actions_request_id.as_ref() == Some(&request_id))
                {
                    thread.actions_request_id = None;
                    let actions = exit.into_result().unwrap_or(Value::Null);
                    let stop = actions["available"].as_bool().unwrap_or(false);
                    let queue_worktree = actions["queueWorktree"].as_bool().unwrap_or(false);
                    let codex_reasoning = actions["codexReasoning"].as_bool().unwrap_or(false);
                    if thread.reported_stop != Some(stop)
                        || thread.reported_queue_worktree != queue_worktree
                        || thread.reported_codex_reasoning != codex_reasoning
                    {
                        thread.reported_stop = Some(stop);
                        thread.reported_queue_worktree = queue_worktree;
                        thread.reported_codex_reasoning = codex_reasoning;
                        events.send(Event::ThreadActions {
                            thread_id: thread.thread_id.clone(),
                            stop,
                            queue_worktree,
                            codex_reasoning,
                        });
                    }
                    if std::mem::take(&mut thread.actions_refresh_needed) {
                        self.refresh_thread_actions().await?;
                    }
                    return Ok(());
                }
                if let Some((thread_id, _)) = self.pending_stops.remove(&request_id) {
                    if let Err(message) = exit.into_result() {
                        events.send(Event::ThreadStopFailed { thread_id, message });
                    }
                    return Ok(());
                }
                if self
                    .pending_project_clone_actions
                    .remove(&request_id)
                    .is_some()
                {
                    if let Err(error) = exit.into_result() {
                        events.send(Event::ProjectCloneError(error));
                    }
                    return Ok(());
                }
                if let Some((response, _)) = self.pending_runtime_responses.remove(&request_id) {
                    let outcome = match exit.into_result() {
                        Ok(receipt)
                            if receipt.get("sequence").and_then(Value::as_u64).is_some() =>
                        {
                            RuntimeResponseOutcome::Accepted
                        }
                        Ok(_) => RuntimeResponseOutcome::Unconfirmed,
                        Err(message) => RuntimeResponseOutcome::Rejected(message),
                    };
                    events.send(response.finish(outcome));
                    return Ok(());
                }
                if self.browser_preview.is_stream(&request_id)
                    || self.browser_preview.is_opening(&request_id)
                {
                    self.preview_exit(&request_id, exit, events);
                    return Ok(());
                }
                if self.terminal_links.matches(&request_id) {
                    return self
                        .terminal_file_link_exit(&request_id, exit, events)
                        .await;
                }
                if self.terminal_attachment(&request_id).is_some()
                    || self.pending_terminal.contains_key(&request_id)
                {
                    return self.terminal_exit(&request_id, exit, events).await;
                }
                if let Some((command_id, _, kind)) = self.pending_projects.remove(&request_id) {
                    if matches!(kind, ProjectCreation::Folder) && exit.is_project_folder_collision()
                    {
                        events.send(Event::ProjectFolderCollision { command_id });
                        return Ok(());
                    }
                    let result = exit.into_result().and_then(|project| {
                        let decoded = match kind {
                            ProjectCreation::Folder => serde_json::from_value::<Project>(project).map(|project| ProjectCreated {
                                project_id: project.id,
                                commit_error: None,
                            }),
                            ProjectCreation::Named | ProjectCreation::Scratch | ProjectCreation::Clone => serde_json::from_value(project),
                        };
                        decoded.map_err(|_| "The environment returned an invalid project. Check the project list before retrying.".to_owned())
                    });
                    events.send(Event::ProjectAdded { command_id, result });
                    return Ok(());
                }
                if let Some((project_id, kind, _)) = self.pending_project_edits.remove(&request_id)
                {
                    let result = exit.into_result().and_then(|value| {
                        serde_json::from_value::<Project>(value)
                            .ok()
                            .filter(|project| project.id == project_id)
                            .map(Box::new)
                            .ok_or_else(|| {
                                "The environment returned an invalid project edit result."
                                    .to_owned()
                            })
                    });
                    events.send(Event::ProjectEditSettled {
                        project_id,
                        kind,
                        result,
                    });
                    return Ok(());
                }
                if self
                    .pending_project_images
                    .as_ref()
                    .is_some_and(|(id, ..)| *id == request_id)
                {
                    let (_, search_id, _) = self
                        .pending_project_images
                        .take()
                        .expect("matched search request");
                    let result = exit.into_result().and_then(|value| {
                        serde_json::from_value(value).map_err(|_| {
                            "The environment returned an invalid image search result.".to_owned()
                        })
                    });
                    events.send(Event::ProjectImages { search_id, result });
                    return Ok(());
                }
                if self
                    .pending_folder
                    .as_ref()
                    .is_some_and(|(id, ..)| *id == request_id)
                {
                    let (_, browse_id, _) = self
                        .pending_folder
                        .take()
                        .expect("matched a folder request");
                    let result = exit.into_folder_result();
                    events.send(Event::FolderBrowsed { browse_id, result });
                    return Ok(());
                }
                if let Some((launch, _)) = self.pending_launches.remove(&request_id) {
                    return self
                        .finish_launch_step(launch, exit.into_result(), events)
                        .await;
                }
                if let Some((thread_id, mutation, operation)) =
                    self.pending_queue_changes.remove(&request_id)
                {
                    events.send(Event::QueueMutationSettled {
                        thread_id,
                        mutation,
                        operation,
                        result: exit.into_result().map(|_| ()),
                    });
                    return Ok(());
                }
                if let Some(resume) = self.pending_resumes.remove(&request_id) {
                    return self.finish_resume(resume, exit.into_result(), events).await;
                }
                let error = exit.into_result().err();
                if request_id == self.shell_request_id {
                    return Err(SessionStop::Protocol(
                        error.unwrap_or_else(|| "The project stream ended.".to_owned()),
                    ));
                }
                if self
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread.request_id == request_id)
                {
                    let thread_id = self
                        .thread
                        .take()
                        .expect("checked live subscription")
                        .thread_id;
                    events.send(Event::RequestError {
                        thread_id,
                        action: ThreadAction::Open,
                        message: error
                            .unwrap_or_else(|| "The conversation stream ended.".to_owned()),
                    });
                } else if let Some(thread_id) = self.pending_prompts.remove(&request_id) {
                    let event = match error {
                        Some(message) => Event::RequestError {
                            thread_id,
                            action: ThreadAction::Send,
                            message,
                        },
                        None => Event::PromptAccepted { thread_id },
                    };
                    events.send(event);
                }
            }
            ServerFrame::Defect | ServerFrame::ClientProtocolError => {
                return Err(SessionStop::Protocol(
                    "The environment reported an RPC protocol error.".to_owned(),
                ));
            }
            ServerFrame::Pong | ServerFrame::Unknown => {}
        }
        Ok(())
    }
}

fn apply(
    values: Vec<Value>,
    mut apply: impl FnMut(Value) -> Result<bool, serde_json::Error>,
) -> Result<bool, String> {
    let mut changed = false;
    for value in values {
        changed |= apply(value).map_err(|error| format!("Invalid environment update: {error}"))?;
    }
    Ok(changed)
}

fn prompt_payload(
    thread_id: &str,
    prompt: &str,
    queue: bool,
    context: Option<&crate::composer_context::MessageContext>,
) -> Value {
    let dispatch = if queue {
        "queue_after_active"
    } else {
        "start_immediately"
    };
    let mut payload = json!({
        "type": "message.dispatch",
        "commandId": Uuid::new_v4().to_string(),
        "messageId": Uuid::new_v4().to_string(),
        "threadId": thread_id,
        "createdBy": "user",
        "creationSource": "web",
        "text": prompt,
        "attachments": [],
        "dispatchMode": { "type": dispatch },
    });
    if let Some(context) = context {
        payload["context"] = json!(context);
    }
    if !queue {
        payload["deliveryIntent"] = json!("auto");
    }
    payload
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ServerFrames {
    One(ServerFrame),
    Many(Vec<ServerFrame>),
}

async fn run(
    server_url: String,
    pairing_code: String,
    mut commands: UnboundedReceiver<Command>,
    terminal_selection: &terminal::Selection,
    events: &EventSender<impl Fn()>,
) -> Result<(), String> {
    let environment = pair(server_url, pairing_code).await?;
    let mut session = Session::open(&environment, None, &[], terminal_selection).await?;
    terminal_selection.set_write_session(Some(session.terminal_write_session));
    events.send(Event::Connected {
        environment_id: environment.descriptor.environment_id.clone(),
        label: environment.descriptor.label.clone(),
        read_only: session.read_only,
        launch_writes: session.launch_writes,
        worktree_launch: session.launch_writes && session.worktree_launch,
        remote_worktree_launch: session.launch_writes
            && session.worktree_launch
            && session.remote_worktree_launch,
        checkout_switch: session.checkout_switch,
        prompt_writes: session.prompt_writes,
        stop_writes: session.stop_writes,
        native_stop: session.native_stop,
        project_writes: session.project_writes,
        filesystem_read: session.filesystem_read,
        filesystem_write: session.filesystem_write,
        named_projects: session.named_projects && session.project_writes,
        clone_writes: session.clone_writes,
        terminal: session.terminals_enabled,
    });
    if session.scratch_projects && session.project_writes && session.launch_writes {
        events.send(Event::ScratchProjectsAvailable);
    }
    if session.project_organization && session.project_writes {
        events.send(Event::ProjectOrganizationAvailable);
    }

    let mut attempt = 0;
    loop {
        let connected_at = Instant::now();
        session.storage_cleanup.publish_access(events);
        events.send(Event::ArchiveAvailable {
            readable: !matches!(session.archive, ArchiveState::Disabled),
            archivable: session.archive_active_writes,
            writable: session.archive_writes,
            deletable: session.archive_deletes,
        });
        let disconnected = match session.publish_initial_shell(events).await {
            Ok(()) => run_socket(&mut session, &mut commands, events).await,
            Err(error) => Err(error),
        };
        terminal_selection.set_write_session(None);
        session.unconfirm_workspace_retries(events);
        session.unconfirm_archive_actions(events);
        session.unconfirm_worktree_removals(events);
        session.storage_cleanup.connection_lost(events);
        let reason = match disconnected {
            Ok(()) => return Ok(()),
            Err(SessionStop::Protocol(message)) => {
                session.unconfirm_runtime_responses(events);
                session.finish_interrupted_ref_requests(events);
                session.terminal_after_disconnect(events);
                for (_, (stage, _)) in session.pending_launches.drain() {
                    events.send(
                        stage.failure("The connection ended before loading launch settings."),
                    );
                }
                return Err(message);
            }
            Err(SessionStop::Socket(message)) => message,
        };
        if connected_at.elapsed() >= RECONNECT_RESET_AFTER {
            attempt = 0;
        }
        events.send(Event::Reconnecting);
        session.fail_keybinding_write(
            events,
            "The connection closed before confirming the keybinding change.",
        );
        session.fail_workspace_write(
            events,
            "The connection closed before confirming the workspace setting.",
        );
        session.unconfirm_runtime_responses(events);
        session.finish_interrupted_ref_requests(events);
        for (_, resume) in session.pending_resumes.drain() {
            events.send(resume.finish(Err(
                "The connection closed before resuming the thread was confirmed.".into(),
            )));
        }
        for (_, (thread_id, mutation, operation)) in session.pending_queue_changes.drain() {
            events.send(Event::QueueMutationSettled {
                thread_id,
                mutation,
                operation,
                result: Err("The connection closed before confirming the queue change.".into()),
            });
        }
        for (_, thread_id) in session.pending_prompts.drain() {
            events.send(Event::RequestError {
                thread_id,
                action: ThreadAction::Send,
                message: "Connection lost before confirmation. Check the conversation before sending again.".to_owned(),
            });
        }
        for (_, pending) in session.pending_saved_reasoning.drain() {
            events.send(pending.finish(Err(
                "Could not confirm the model selection before the connection closed.".into(),
            )));
        }
        for (_, (thread_id, _)) in session.pending_title_edits.drain() {
            events.send(Event::RequestError {
                thread_id,
                action: ThreadAction::Rename,
                message: "The connection closed before the rename could be confirmed.".into(),
            });
        }
        for (_, (thread_id, _)) in session.pending_setup_cancels.drain() {
            events.send(Event::WorktreeSetupCancelSettled {
                thread_id,
                result: Err("The connection closed before cancellation could be confirmed.".into()),
            });
        }
        for (_, (command_id, ..)) in session.pending_projects.drain() {
            events.send(Event::ProjectAdded { command_id, result: Err("Project creation could not be confirmed. Check the project list before retrying.".into()) });
        }
        for (_, (project_id, kind, _)) in session.pending_project_edits.drain() {
            events.send(Event::ProjectEditSettled {
                project_id,
                kind,
                result: Err("The connection closed before confirming the project change.".into()),
            });
        }
        if let Some((_, search_id, _)) = session.pending_project_images.take() {
            events.send(Event::ProjectImages {
                search_id,
                result: Err("The connection closed before the search finished.".into()),
            });
        }
        if let Some((_, browse_id, _)) = session.pending_folder.take() {
            events.send(Event::FolderBrowsed {
                browse_id,
                result: Err("Connection lost while browsing folders.".into()),
            });
        }
        for (_, (stage, _)) in session.pending_launches.drain() {
            events.send(stage.failure("The connection ended before loading launch settings."));
        }
        let thread_id = session.thread.take().map(|thread| thread.thread_id);
        let command_outputs = session.command_outputs.inspections();
        let terminals = session.terminal_after_disconnect(events);
        drop(session);
        let mut last_error = reason;
        session = loop {
            let Some(delay) = RECONNECT_DELAYS.get(attempt) else {
                return Err(last_error);
            };
            attempt += 1;
            sleep(*delay).await;
            match Session::open(
                &environment,
                thread_id.clone(),
                &terminals,
                terminal_selection,
            )
            .await
            {
                Ok(session) => break session,
                Err(error) => last_error = error,
            }
        };
        terminal_selection.set_write_session(Some(session.terminal_write_session));
        events.send(Event::Reconnected);
        for inspection in command_outputs {
            session
                .command_output(command_output::Command::Open(inspection), events)
                .await
                .map_err(SessionStop::into_message)?;
        }
    }
}

async fn run_socket(
    session: &mut Session,
    commands: &mut UnboundedReceiver<Command>,
    events: &EventSender<impl Fn()>,
) -> Result<(), SessionStop> {
    let mut last_seen = Instant::now();
    let mut ping = interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_seen.elapsed() >= SOCKET_TIMEOUT {
                    return Err(SessionStop::Socket("The environment stopped responding.".to_owned()));
                }
                session.expire_archive_actions(events);
                session.expire_worktree_removals(events);
                session.pending_launches.retain(|_, (stage, started)| {
                    if started.elapsed() < stage.timeout() {
                        return true;
                    }
                    events.send(stage.failure("The environment did not return launch settings in time."));
                    false
                });
                session.pending_git_refs.retain(|_, request| {
                    if request.started.elapsed() < REQUEST_TIMEOUT { return true; }
                    events.send(Event::GitRefs { project_id: request.project_id.clone(), result: Err("The environment did not return Git refs in time.".into()) });
                    false
                });
                session.pending_workspace_defaults.retain(|_, (_, started)| started.elapsed() < REQUEST_TIMEOUT);
                session.expire_file_preview(events).await?;
                session.pending_project_file_defaults.retain(|_, (_, _, started)| started.elapsed() < REQUEST_TIMEOUT);
                session.pending_setup_cancels.retain(|_, (thread_id, started)| {
                    if started.elapsed() < REQUEST_TIMEOUT { return true; }
                    events.send(Event::WorktreeSetupCancelSettled { thread_id: thread_id.clone(), result: Err("The environment did not confirm cancellation in time.".into()) });
                    false
                });
                session.pending_saved_reasoning.retain(|_, pending| {
                    if pending.started.elapsed() < REQUEST_TIMEOUT { return true; }
                    events.send(pending.finish(Err("The environment did not confirm the model selection in time.".into())));
                    false
                });
                session.pending_title_edits.retain(|_, (thread_id, started)| {
                    if started.elapsed() < REQUEST_TIMEOUT { return true; }
                    events.send(Event::RequestError { thread_id: thread_id.clone(), action: ThreadAction::Rename, message: "The environment did not confirm the rename in time.".into() });
                    false
                });
                session.pending_projects.retain(|_, (command_id, started, kind)| {
                    let limit = if matches!(kind, ProjectCreation::Named) { Duration::from_secs(60) } else { REQUEST_TIMEOUT };
                    if started.elapsed() < limit {
                        return true;
                    }
                    events.send(Event::ProjectAdded { command_id: command_id.clone(), result: Err("Project creation could not be confirmed. Check the project list before retrying.".into()) });
                    false
                });
                session.pending_project_edits.retain(|_, (project_id, kind, started)| {
                    if started.elapsed() < REQUEST_TIMEOUT { return true; }
                    events.send(Event::ProjectEditSettled { project_id: project_id.clone(), kind: *kind,
                        result: Err("The environment did not confirm the project change in time.".into()) });
                    false
                });
                session.pending_project_clone_actions.retain(|_, started| {
                    if started.elapsed() < REQUEST_TIMEOUT {
                        return true;
                    }
                    events.send(Event::ProjectCloneError("The repository clone action could not be confirmed.".into()));
                    false
                });
                session.pending_runtime_responses.retain(|_, (response, started)| {
                    if started.elapsed() < REQUEST_TIMEOUT {
                        return true;
                    }
                    events.send(response.clone().finish(RuntimeResponseOutcome::Unconfirmed));
                    false
                });
                session.pending_stops.retain(|_, (thread_id, started)| {
                    if started.elapsed() < REQUEST_TIMEOUT {
                        return true;
                    }
                    events.send(Event::ThreadStopFailed {thread_id:thread_id.clone(), message:"Stop could not be confirmed.".into()});
                    false
                });
                if session.pending_project_images.as_ref().is_some_and(|(_, _, started)| started.elapsed() >= REQUEST_TIMEOUT)
                    && let Some((id, search_id, _)) = session.pending_project_images.take()
                {
                    session.interrupt(&id).await?;
                    events.send(Event::ProjectImages { search_id,
                        result: Err("The environment did not return the image search in time.".into()) });
                }
                if session.pending_folder.as_ref().is_some_and(|(_, _, started)| started.elapsed() >= REQUEST_TIMEOUT)
                    && let Some((id, browse_id, _)) = session.pending_folder.take()
                {
                    events.send(Event::FolderBrowsed { browse_id, result: Err("The environment did not return folders in time.".into()) });
                    session.interrupt(&id).await?;
                }
                session.expire_terminal(events).await?;
                session.expire_terminal_links(events).await?;
                session.expire_preview(events).await?;
                session.expire_workspace_retries(events);
                session.expire_keybinding_write(events);
                session.expire_workspace_write(events);
                session.favicons.expire(events);
                session.request_project_favicons().await?;
                session.expire_command_outputs(events).await?;
                session.send(json!({"_tag": "Ping"})).await?;
            }
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()) };
                session.command(command, events).await?;
            }
            download = session.favicons.next_download() => {
                if let Some((target, image)) = download {
                    events.send(Event::ProjectFavicon { target, image });
                }
                session.request_project_favicons().await?;
            }
            preview = session.browser_preview.next() => {
                if let Some(event)=preview { events.send(event); }
            }
            asset = session.preview_downloads.next() => {
                let (id, result) = asset;
                events.send(Event::FilePreviewAsset { id, result });
            }
            frame = session.socket.next() => {
                let frame = frame
                    .ok_or_else(|| SessionStop::Socket("The environment disconnected.".to_owned()))?
                    .map_err(|error| SessionStop::Socket(format!("The environment disconnected: {error}")))?;
                let raw = match &frame {
                    WebSocketMessage::Text(text) => text.as_bytes(),
                    WebSocketMessage::Binary(bytes) => bytes.as_ref(),
                    WebSocketMessage::Close(_) => return Err(SessionStop::Socket("The environment disconnected.".to_owned())),
                    _ => continue,
                };
                last_seen = Instant::now();
                match serde_json::from_slice(raw).map_err(|error| SessionStop::Protocol(format!("Invalid RPC frame: {error}")))? {
                    ServerFrames::One(frame) => session.receive(frame, events).await?,
                    ServerFrames::Many(frames) => {
                        for frame in frames {
                            session.receive(frame, events).await?;
                        }
                    }
                }
            }
        }
    }
}
