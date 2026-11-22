mod events;
mod terminals;
mod usage;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{CodexAppServer, CodexClient, CodexError, CodexLaunch, RequestId, RpcError};

pub use events::{
    ApprovalKind, ApprovalRequest, AssistantPhase, CodexSessionEvent, InputOption, InputQuestion,
    ReasoningStream, ToolItem, ToolLifecycle, TurnEvent, UserInputRequest, WebSearchUpdate,
};

pub use usage::{
    NativeTokenCounts, TokenTotals, TokenUsageSnapshot, TurnContextUsage, TurnTokenUsage,
    UsageStatus,
};

const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const THREAD_NAME_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnKey {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InteractionMode {
    #[default]
    Default,
    Plan,
}

#[derive(Clone, Debug)]
pub struct ThreadOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub app_thread_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TurnOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub text: String,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    #[serde(rename = "inProgress")]
    Running,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Debug)]
pub struct TurnSnapshot {
    pub key: TurnKey,
    pub status: TurnStatus,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    AcceptForSession,
    AcceptAlways,
    Decline,
    Cancel,
}

#[derive(Debug)]
pub enum CodexAdapterError {
    Transport(CodexError),
    InvalidPayload { method: String, detail: String },
    IdentityMismatch { method: &'static str },
    RequestSessionMismatch,
    BackgroundTerminalStillActive,
}

impl fmt::Display for CodexAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(f),
            Self::InvalidPayload { method, detail } => {
                write!(f, "Invalid Codex {method} payload: {detail}")
            }
            Self::IdentityMismatch { method } => {
                write!(f, "Codex {method} returned a different thread")
            }
            Self::RequestSessionMismatch => {
                f.write_str("The Codex request belongs to a different session")
            }
            Self::BackgroundTerminalStillActive => {
                f.write_str("A Codex background terminal is still active")
            }
        }
    }
}

impl std::error::Error for CodexAdapterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CodexError> for CodexAdapterError {
    fn from(error: CodexError) -> Self {
        Self::Transport(error)
    }
}

pub struct CodexSession {
    server: CodexAppServer,
    scope: Arc<SessionScope>,
}

type SessionAllowance = Arc<Mutex<HashSet<ApprovalKind>>>;

struct SessionScope {
    id: Uuid,
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    interrupting: HashSet<TurnKey>,
    approvals: HashMap<ThreadId, ThreadApprovals>,
    usage: HashMap<ThreadId, usage::ThreadUsage>,
    thread_names: HashMap<ThreadId, String>,
}

struct ThreadApprovals {
    mode: RuntimeMode,
    allowance: SessionAllowance,
    turn: ApprovalTurn,
}

enum ApprovalTurn {
    Starting,
    Active(TurnId),
    Settled,
}

impl CodexSession {
    pub async fn launch(launch: CodexLaunch) -> Result<Self, CodexAdapterError> {
        Ok(Self {
            server: CodexAppServer::launch(launch).await?,
            scope: Arc::new(SessionScope {
                id: Uuid::new_v4(),
                state: Mutex::default(),
            }),
        })
    }

    pub fn client(&self) -> CodexSessionClient {
        CodexSessionClient {
            client: self.server.client(),
            scope: Arc::clone(&self.scope),
        }
    }

    pub async fn next_event(&mut self) -> Result<CodexSessionEvent, CodexAdapterError> {
        loop {
            let raw = self.server.next_event().await?;
            let request_id = match &raw {
                super::CodexEvent::Request { id, .. } => Some(id.clone()),
                _ => None,
            };
            let mut event = match events::decode(raw, self.scope.id) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(error) => {
                    let Some(request_id) = request_id else {
                        return Err(error);
                    };
                    match self
                        .server
                        .client()
                        .respond(
                            request_id,
                            Err(RpcError {
                                code: -32602,
                                message: "Invalid Codex server request parameters".into(),
                                data: None,
                            }),
                        )
                        .await
                    {
                        Ok(()) | Err(CodexError::RequestAlreadyResolved) => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            };
            if let CodexSessionEvent::ThreadModelChanged { thread_id, model } = &event {
                self.scope
                    .state
                    .lock()
                    .expect("Codex session lock")
                    .usage
                    .entry(thread_id.clone())
                    .or_default()
                    .model(model.clone());
            }
            if let CodexSessionEvent::Turn { key, event } = &mut event {
                match event {
                    TurnEvent::Started { .. } => {
                        let mut state = self.scope.state.lock().expect("Codex session lock");
                        state
                            .usage
                            .entry(key.thread_id.clone())
                            .or_default()
                            .begin(&key.turn_id);
                        if let Some(approvals) = state.approvals.get_mut(&key.thread_id)
                            && matches!(approvals.turn, ApprovalTurn::Starting)
                        {
                            approvals.turn = ApprovalTurn::Active(key.turn_id.clone());
                        }
                    }
                    TurnEvent::Settled { status, usage, .. } => {
                        let mut state = self.scope.state.lock().expect("Codex session lock");
                        let interrupted = state.interrupting.remove(key);
                        if interrupted && *status == SettlementStatus::Completed {
                            *status = SettlementStatus::Interrupted;
                        }
                        *usage = state
                            .usage
                            .entry(key.thread_id.clone())
                            .or_default()
                            .finish(&key.turn_id, *status);
                        if let Some(approvals) = state.approvals.get_mut(&key.thread_id)
                            && matches!(&approvals.turn, ApprovalTurn::Active(turn) if turn == &key.turn_id)
                        {
                            approvals.turn = ApprovalTurn::Settled;
                        }
                    }
                    TurnEvent::TokenUsageUpdated { usage } => {
                        self.scope
                            .state
                            .lock()
                            .expect("Codex session lock")
                            .usage
                            .entry(key.thread_id.clone())
                            .or_default()
                            .update(&key.turn_id, *usage);
                    }
                    TurnEvent::ModelRerouted { model } => {
                        self.scope
                            .state
                            .lock()
                            .expect("Codex session lock")
                            .usage
                            .entry(key.thread_id.clone())
                            .or_default()
                            .reroute(&key.turn_id, model.clone());
                    }
                    TurnEvent::ToolChanged { item, lifecycle }
                        if item.kind == "subAgentActivity"
                            || (*lifecycle == ToolLifecycle::Completed
                                && item.kind == "collabAgentToolCall"
                                && item.payload["tool"] == "spawnAgent") =>
                    {
                        self.scope
                            .state
                            .lock()
                            .expect("Codex session lock")
                            .usage
                            .entry(key.thread_id.clone())
                            .or_default()
                            .note_subagent(&key.turn_id);
                    }
                    TurnEvent::ApprovalRequested { request } if request.permissions().is_none() => {
                        request.allowance = self.scope.state.lock().expect("Codex session lock").approvals.get(&key.thread_id)
                            .filter(|approvals| matches!(&approvals.turn, ApprovalTurn::Active(turn) if turn == &key.turn_id))
                            .map(|approvals| Arc::clone(&approvals.allowance));
                        let allowed = request.allowance.as_ref().is_some_and(|allowance| {
                            allowance
                                .lock()
                                .expect("Codex approval lock")
                                .contains(&request.kind)
                        });
                        if allowed {
                            match self
                                .client()
                                .answer_approval(request, ApprovalDecision::AcceptForSession)
                                .await
                            {
                                Ok(())
                                | Err(CodexAdapterError::Transport(
                                    CodexError::RequestAlreadyResolved,
                                )) => continue,
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    _ => {}
                }
            }
            return Ok(event);
        }
    }

    pub async fn shutdown(self) -> Result<(), CodexAdapterError> {
        Ok(self.server.shutdown().await?)
    }
}

#[derive(Clone)]
pub struct CodexSessionClient {
    client: CodexClient,
    scope: Arc<SessionScope>,
}

impl CodexSessionClient {
    pub async fn inject_history(
        &self,
        thread_id: &ThreadId,
        items: &[Value],
    ) -> Result<bool, CodexAdapterError> {
        match self
            .request::<Value>(
                "thread/inject_items",
                json!({"threadId": thread_id, "items": items}),
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(CodexAdapterError::Transport(CodexError::Remote { error, .. }))
                if error.code == -32601 =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn apply_thread_name(&self, thread_id: &ThreadId, name: &str) {
        if self
            .scope
            .state
            .lock()
            .expect("Codex session lock")
            .thread_names
            .get(thread_id)
            .is_some_and(|applied| applied == name)
        {
            return;
        }
        let result = self
            .client
            .request(
                "thread/name/set",
                Some(json!({"threadId": thread_id, "name": name})),
                THREAD_NAME_DEADLINE,
            )
            .await;
        let mut state = self.scope.state.lock().expect("Codex session lock");
        if result.is_ok() {
            state
                .thread_names
                .insert(thread_id.clone(), name.to_owned());
        } else {
            // A failed rename is retried the next time this thread starts a turn.
            state.thread_names.remove(thread_id);
        }
    }

    pub async fn start_thread(
        &self,
        options: &ThreadOptions,
    ) -> Result<ThreadId, CodexAdapterError> {
        const METHOD: &str = "thread/start";
        let response: ThreadResponse = self.request(METHOD, thread_params(options, None)?).await?;
        validate_thread_id(METHOD, response.thread.id)
    }

    pub async fn resume_thread(
        &self,
        thread_id: &ThreadId,
        options: &ThreadOptions,
    ) -> Result<ThreadId, CodexAdapterError> {
        const METHOD: &str = "thread/resume";
        let params = thread_params(options, Some(thread_id))?;
        let response: ThreadResponse = match self.request(METHOD, params.clone()).await {
            Err(CodexAdapterError::Transport(CodexError::Remote { error, .. }))
                if is_archived(&error.message) =>
            {
                self.request::<Value>("thread/unarchive", json!({"threadId": thread_id}))
                    .await?;
                self.request(METHOD, params).await?
            }
            result => result?,
        };
        if &response.thread.id != thread_id {
            return Err(CodexAdapterError::IdentityMismatch { method: METHOD });
        }
        validate_thread_id(METHOD, response.thread.id)
    }

    pub async fn start_turn(
        &self,
        thread_id: &ThreadId,
        options: &TurnOptions,
    ) -> Result<TurnSnapshot, CodexAdapterError> {
        const METHOD: &str = "turn/start";
        let cwd = wire_path(METHOD, &options.cwd)?;
        let summary = match self
            .request::<ConfigResponse>("config/read", json!({"cwd": cwd, "includeLayers": false}))
            .await
        {
            Ok(response) => response
                .config
                .model_reasoning_summary
                .unwrap_or_else(|| "auto".into()),
            Err(CodexAdapterError::Transport(CodexError::Remote { .. })) => "auto".into(),
            Err(error) => return Err(error),
        };
        let allowance = {
            let mut state = self.scope.state.lock().expect("Codex session lock");
            let approvals =
                state
                    .approvals
                    .entry(thread_id.clone())
                    .or_insert_with(|| ThreadApprovals {
                        mode: options.runtime_mode,
                        allowance: Arc::default(),
                        turn: ApprovalTurn::Starting,
                    });
            if approvals.mode != options.runtime_mode {
                approvals.mode = options.runtime_mode;
                approvals.allowance = Arc::default();
            }
            approvals.turn = ApprovalTurn::Starting;
            Arc::clone(&approvals.allowance)
        };
        let response: TurnResponse = self
            .request(METHOD, turn_params(thread_id, cwd, options, summary))
            .await?;
        let snapshot = response.turn.snapshot(METHOD, thread_id.clone())?;
        let mut state = self.scope.state.lock().expect("Codex session lock");
        if let Some(approvals) = state.approvals.get_mut(thread_id)
            && Arc::ptr_eq(&approvals.allowance, &allowance)
            && matches!(approvals.turn, ApprovalTurn::Starting)
        {
            let turn_id = snapshot.key.turn_id.clone();
            approvals.turn = if snapshot.status == TurnStatus::Running {
                ApprovalTurn::Active(turn_id.clone())
            } else {
                ApprovalTurn::Settled
            };
            if snapshot.status == TurnStatus::Running {
                state
                    .usage
                    .entry(thread_id.clone())
                    .or_default()
                    .begin(&turn_id);
            }
        }
        Ok(snapshot)
    }

    pub async fn interrupt_turn(&self, key: &TurnKey) -> Result<(), CodexAdapterError> {
        let fresh = self
            .scope
            .state
            .lock()
            .expect("Codex session lock")
            .interrupting
            .insert(key.clone());
        let result = self.request::<Value>("turn/interrupt", json!(key)).await;
        if result.is_err() && fresh {
            self.scope
                .state
                .lock()
                .expect("Codex session lock")
                .interrupting
                .remove(key);
        }
        result.map(|_| ())
    }

    pub async fn answer_approval(
        &self,
        request: &ApprovalRequest,
        decision: ApprovalDecision,
    ) -> Result<(), CodexAdapterError> {
        if request.session_id != self.scope.id {
            return Err(CodexAdapterError::RequestSessionMismatch);
        }
        self.client
            .respond(request.request_id.clone(), Ok(request.response(decision)))
            .await?;
        if matches!(
            decision,
            ApprovalDecision::AcceptForSession | ApprovalDecision::AcceptAlways
        ) && let Some(allowance) = &request.allowance
        {
            allowance
                .lock()
                .expect("Codex approval lock")
                .insert(request.kind);
        }
        Ok(())
    }

    pub async fn answer_user_input(
        &self,
        request: &UserInputRequest,
        answers: BTreeMap<String, Vec<String>>,
    ) -> Result<(), CodexAdapterError> {
        if request.session_id != self.scope.id {
            return Err(CodexAdapterError::RequestSessionMismatch);
        }
        let answers = request
            .questions
            .iter()
            .enumerate()
            .filter_map(|(index, question)| {
                let raw = question.id.trim();
                let fallback;
                let stored = if raw.is_empty() {
                    fallback = format!("question-{}", index + 1);
                    fallback.as_str()
                } else {
                    raw
                };
                answers
                    .get(stored)
                    .map(|answers| (question.id.clone(), json!({ "answers": answers })))
            })
            .collect::<BTreeMap<_, _>>();
        Ok(self
            .client
            .respond(
                request.request_id.clone(),
                Ok(json!({ "answers": answers })),
            )
            .await?)
    }

    pub async fn reject_unsupported_request(
        &self,
        request_id: RequestId,
    ) -> Result<(), CodexAdapterError> {
        let error = RpcError {
            code: -32601,
            message: "Unsupported Codex server request".into(),
            data: None,
        };
        Ok(self.client.respond(request_id, Err(error)).await?)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, CodexAdapterError> {
        let response = self
            .client
            .request(method, Some(params), REQUEST_DEADLINE)
            .await?;
        decode(method, response)
    }
}

#[derive(Deserialize)]
struct ThreadResponse {
    thread: NativeThread,
}

#[derive(Deserialize)]
struct NativeThread {
    id: ThreadId,
}

#[derive(Deserialize)]
struct ConfigResponse {
    config: CodexConfig,
}

#[derive(Deserialize)]
struct CodexConfig {
    model_reasoning_summary: Option<String>,
}

#[derive(Deserialize)]
struct TurnResponse {
    turn: NativeTurn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeTurn {
    id: TurnId,
    status: TurnStatus,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    error: Option<NativeFailure>,
}

impl NativeTurn {
    fn snapshot(
        self,
        method: &str,
        thread_id: ThreadId,
    ) -> Result<TurnSnapshot, CodexAdapterError> {
        if self.id.as_str().is_empty() || thread_id.as_str().is_empty() {
            return Err(invalid(method, "empty thread or turn ID"));
        }
        Ok(TurnSnapshot {
            key: TurnKey {
                thread_id,
                turn_id: self.id,
            },
            status: self.status,
            started_at: self.started_at,
            completed_at: self.completed_at,
            error: self.error.map(|error| error.message),
        })
    }
}

#[derive(Deserialize)]
struct NativeFailure {
    message: String,
}

fn thread_params(
    options: &ThreadOptions,
    resume: Option<&ThreadId>,
) -> Result<Value, CodexAdapterError> {
    let method = if resume.is_some() {
        "thread/resume"
    } else {
        "thread/start"
    };
    let mut config = json!({
        "tools.update_plan.enabled": true,
        "mcp_optional_startup_grace_ms": 10000,
        "features.concurrent_reasoning_summaries": true,
    });
    if let Some(id) = &options.app_thread_id {
        config["shell_environment_policy.set.T3CODE_THREAD_ID"] = json!(id);
    }
    let mut params = json!({ "cwd": wire_path(method, &options.cwd)?, "model": options.model, "config": config });
    if let Some(thread_id) = resume {
        params["threadId"] = json!(thread_id);
        params["excludeTurns"] = json!(true);
    }
    Ok(params)
}

fn wire_path<'a>(method: &str, cwd: &'a Path) -> Result<&'a str, CodexAdapterError> {
    cwd.to_str()
        .ok_or_else(|| invalid(method, "working directory is not UTF-8"))
}

fn validate_thread_id(method: &str, id: ThreadId) -> Result<ThreadId, CodexAdapterError> {
    if id.as_str().is_empty() {
        Err(invalid(method, "empty thread ID"))
    } else {
        Ok(id)
    }
}

fn is_archived(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("codex unarchive")
        || (message.contains("session ") && message.contains(" is archived"))
}

fn turn_params(thread_id: &ThreadId, cwd: &str, options: &TurnOptions, summary: String) -> Value {
    let (approval, reviewer, sandbox) = match options.runtime_mode {
        RuntimeMode::ApprovalRequired => (json!("untrusted"), "user", "readOnly"),
        RuntimeMode::AutoAcceptEdits => (json!("on-request"), "user", "workspaceWrite"),
        RuntimeMode::Auto => (json!("on-request"), "auto_review", "workspaceWrite"),
        RuntimeMode::FullAccess => (
            json!({ "granular": {
                "mcp_elicitations": true,
                "request_permissions": false,
                "rules": false,
                "sandbox_approval": false,
                "skill_approval": false
            }}),
            "user",
            "dangerFullAccess",
        ),
    };
    let mut params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": options.text }],
        "cwd": cwd,
        "model": options.model,
        "summary": summary,
        "approvalsReviewer": reviewer,
        "approvalPolicy": approval,
        "sandboxPolicy": { "type": sandbox },
        "collaborationMode": { "mode": options.interaction_mode, "settings": { "model": options.model } },
    });
    let default_effort =
        matches!(options.model.as_str(), "gpt-6-astra" | "openai.gpt-6-astra").then_some("medium");
    if let Some(effort) = options.reasoning_effort.as_deref().or(default_effort) {
        params["effort"] = json!(effort);
        params["collaborationMode"]["settings"]["reasoning_effort"] = json!(effort);
    }
    params
}

fn decode<T: DeserializeOwned>(method: &str, value: Value) -> Result<T, CodexAdapterError> {
    serde_json::from_value(value).map_err(|error| invalid(method, error.to_string()))
}

fn invalid(method: &str, detail: impl Into<String>) -> CodexAdapterError {
    CodexAdapterError::InvalidPayload {
        method: method.into(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests;
