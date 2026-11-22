use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::usage::{TokenUsageSnapshot, TurnTokenUsage};
use super::{
    ApprovalDecision, CodexAdapterError, NativeTurn, RequestId, SessionAllowance, SettlementStatus,
    ThreadId, TurnKey, TurnStatus, decode as payload, invalid, validate_thread_id,
};
use crate::providers::CodexEvent;

#[derive(Debug)]
pub enum CodexSessionEvent {
    Turn {
        key: TurnKey,
        event: TurnEvent,
    },
    ThreadModelChanged {
        thread_id: ThreadId,
        model: String,
    },
    RequestResolved {
        thread_id: ThreadId,
        request_id: RequestId,
    },
    UnsupportedRequest {
        request_id: RequestId,
        method: String,
        params: Option<Value>,
    },
}

#[derive(Debug)]
pub enum TurnEvent {
    Started {
        started_at: Option<i64>,
    },
    TextStarted {
        item_id: String,
        phase: Option<AssistantPhase>,
        started_at_ms: Option<i64>,
    },
    TextDelta {
        item_id: String,
        delta: String,
    },
    TextCompleted {
        item_id: String,
        text: String,
        phase: Option<AssistantPhase>,
        completed_at_ms: Option<i64>,
    },
    ReasoningDelta {
        item_id: String,
        stream: ReasoningStream,
        index: usize,
        delta: String,
    },
    ReasoningCompleted {
        item_id: String,
        summary: Vec<String>,
        content: Vec<String>,
    },
    ToolChanged {
        item: ToolItem,
        lifecycle: ToolLifecycle,
    },
    WebSearch(WebSearchUpdate),
    ToolOutputDelta {
        item_id: String,
        delta: String,
    },
    TokenUsageUpdated {
        usage: TokenUsageSnapshot,
    },
    ModelRerouted {
        model: String,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    UserInputRequested {
        request: UserInputRequest,
    },
    Settled {
        status: SettlementStatus,
        completed_at: Option<i64>,
        error: Option<String>,
        usage: TurnTokenUsage,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantPhase {
    Commentary,
    PartialAnswer,
    FinalAnswer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolLifecycle {
    Started,
    Completed,
}

#[derive(Clone, Debug)]
pub struct ToolItem {
    pub id: String,
    pub kind: String,
    pub payload: Value,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
}

#[derive(Debug)]
pub struct WebSearchUpdate {
    pub item_id: String,
    pub lifecycle: ToolLifecycle,
    pub started_at_ms: Option<i64>,
    pub patterns: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStream {
    Summary,
    Content,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApprovalKind {
    Command,
    FileRead,
    FileChange,
}

#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub request_id: RequestId,
    pub item_id: String,
    pub approval_id: Option<String>,
    pub kind: ApprovalKind,
    pub reason: Option<String>,
    pub command: Option<String>,
    pub grant_root: Option<String>,
    pub cwd: Option<PathBuf>,
    pub available_decisions: Option<Vec<Value>>,
    pub(super) session_id: Uuid,
    pub(super) allowance: Option<SessionAllowance>,
    permissions: Option<Box<Value>>,
}

impl ApprovalRequest {
    pub fn permissions(&self) -> Option<&Value> {
        self.permissions.as_deref()
    }

    pub(super) fn response(&self, decision: ApprovalDecision) -> Value {
        if let Some(permissions) = self.permissions() {
            return match decision {
                ApprovalDecision::Accept => json!({"permissions": permissions, "scope": "turn"}),
                ApprovalDecision::AcceptForSession => {
                    json!({"permissions": permissions, "scope": "session"})
                }
                ApprovalDecision::AcceptAlways
                | ApprovalDecision::Decline
                | ApprovalDecision::Cancel => json!({"permissions": {}, "scope": "turn"}),
            };
        }
        let decision = match decision {
            ApprovalDecision::Accept => "accept",
            ApprovalDecision::AcceptForSession | ApprovalDecision::AcceptAlways => {
                "acceptForSession"
            }
            ApprovalDecision::Decline => "decline",
            ApprovalDecision::Cancel => "cancel",
        };
        json!({ "decision": decision })
    }
}

#[derive(Clone, Debug)]
pub struct UserInputRequest {
    pub request_id: RequestId,
    pub item_id: String,
    pub questions: Vec<InputQuestion>,
    pub(super) session_id: Uuid,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    pub options: Option<Vec<InputOption>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InputOption {
    pub label: String,
    pub description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lifecycle {
    thread_id: ThreadId,
    turn: NativeTurn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delta {
    #[serde(flatten)]
    key: TurnKey,
    item_id: String,
    delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningDelta {
    #[serde(flatten)]
    delta: Delta,
    summary_index: Option<usize>,
    content_index: Option<usize>,
}

#[derive(Deserialize)]
struct ReasoningParts {
    #[serde(default)]
    summary: Vec<String>,
    #[serde(default)]
    content: Vec<String>,
}

#[derive(Deserialize)]
struct WebSearchFields {
    query: Option<String>,
    action: Option<WebSearchAction>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WebSearchAction {
    Search {
        queries: Option<Vec<String>>,
        query: Option<String>,
    },
    OpenPage {
        url: Option<String>,
    },
    FindInPage {
        pattern: Option<String>,
        url: Option<String>,
    },
    Other,
}

impl WebSearchFields {
    fn patterns(&self) -> Vec<String> {
        let mut candidates = Vec::new();
        match &self.action {
            Some(WebSearchAction::Search { queries, query }) => {
                candidates.extend(queries.iter().flatten().map(String::as_str));
                candidates.extend(query.as_deref());
            }
            Some(WebSearchAction::OpenPage { url }) => candidates.extend(url.as_deref()),
            Some(WebSearchAction::FindInPage { pattern, url }) => {
                candidates.extend(pattern.as_deref());
                candidates.extend(url.as_deref());
            }
            None | Some(WebSearchAction::Other) => {}
        }
        candidates.extend(self.query.as_deref());
        let mut seen = HashSet::new();
        candidates
            .into_iter()
            .map(|text| {
                text.trim_matches(|ch: char| {
                    ch == '\u{feff}' || (ch.is_whitespace() && ch != '\u{85}')
                })
            })
            .filter(|text| !text.is_empty() && seen.insert(*text))
            .map(str::to_owned)
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    #[serde(flatten)]
    key: TurnKey,
    item: Value,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    #[serde(flatten)]
    key: TurnKey,
    token_usage: TokenUsageSnapshot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadSettingsUpdate {
    thread_id: ThreadId,
    thread_settings: ThreadSettings,
}

#[derive(Deserialize)]
struct ThreadSettings {
    model: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelReroute {
    #[serde(flatten)]
    key: TurnKey,
    to_model: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resolved {
    thread_id: ThreadId,
    request_id: RequestId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Approval {
    #[serde(flatten)]
    key: TurnKey,
    item_id: String,
    approval_id: Option<String>,
    reason: Option<String>,
    command: Option<String>,
    grant_root: Option<String>,
    cwd: Option<PathBuf>,
    available_decisions: Option<Vec<Value>>,
    permissions: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    #[serde(flatten)]
    key: TurnKey,
    item_id: String,
    questions: Vec<InputQuestion>,
}

pub(super) fn decode(
    event: CodexEvent,
    session_id: Uuid,
) -> Result<Option<CodexSessionEvent>, CodexAdapterError> {
    match event {
        CodexEvent::Notification { method, params } => {
            notification(&method, params.unwrap_or(Value::Null))
        }
        CodexEvent::Request { id, method, params } => {
            request(id, method, params, session_id).map(Some)
        }
    }
}

fn notification(
    method: &str,
    params: Value,
) -> Result<Option<CodexSessionEvent>, CodexAdapterError> {
    let (key, event) = match method {
        "turn/started" | "turn/completed" => {
            let lifecycle: Lifecycle = payload(method, params)?;
            let snapshot = lifecycle.turn.snapshot(method, lifecycle.thread_id)?;
            let event = if method == "turn/started" {
                TurnEvent::Started {
                    started_at: snapshot.started_at,
                }
            } else {
                let status = match snapshot.status {
                    TurnStatus::Completed => SettlementStatus::Completed,
                    TurnStatus::Interrupted => SettlementStatus::Interrupted,
                    TurnStatus::Failed | TurnStatus::Running => SettlementStatus::Failed,
                };
                TurnEvent::Settled {
                    status,
                    completed_at: snapshot.completed_at,
                    error: snapshot.error,
                    usage: TurnTokenUsage::unavailable(),
                }
            };
            (snapshot.key, event)
        }
        "item/agentMessage/delta" | "item/commandExecution/outputDelta" => {
            let Delta {
                key,
                item_id,
                delta,
            } = payload(method, params)?;
            let event = if method == "item/agentMessage/delta" {
                TurnEvent::TextDelta { item_id, delta }
            } else {
                TurnEvent::ToolOutputDelta { item_id, delta }
            };
            (key, event)
        }
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            let update: ReasoningDelta = payload(method, params)?;
            let (stream, index) = if method == "item/reasoning/summaryTextDelta" {
                (ReasoningStream::Summary, update.summary_index)
            } else {
                (ReasoningStream::Content, update.content_index)
            };
            let index = index.ok_or_else(|| invalid(method, "missing reasoning part index"))?;
            (
                update.delta.key,
                TurnEvent::ReasoningDelta {
                    item_id: update.delta.item_id,
                    stream,
                    index,
                    delta: update.delta.delta,
                },
            )
        }
        "item/started" | "item/completed" => {
            let item: Item = payload(method, params)?;
            let lifecycle = if method == "item/started" {
                ToolLifecycle::Started
            } else {
                ToolLifecycle::Completed
            };
            let Some(event) = item_event(method, &item, lifecycle)? else {
                return Ok(None);
            };
            (item.key, event)
        }
        "thread/tokenUsage/updated" => {
            let TokenUsage { key, token_usage } = payload(method, params)?;
            (key, TurnEvent::TokenUsageUpdated { usage: token_usage })
        }
        "thread/settings/updated" => {
            let ThreadSettingsUpdate {
                thread_id,
                thread_settings,
            } = payload(method, params)?;
            let thread_id = validate_thread_id(method, thread_id)?;
            return Ok(Some(CodexSessionEvent::ThreadModelChanged {
                thread_id,
                model: thread_settings.model,
            }));
        }
        "model/rerouted" => {
            let ModelReroute { key, to_model } = payload(method, params)?;
            (key, TurnEvent::ModelRerouted { model: to_model })
        }
        "serverRequest/resolved" => {
            let Resolved {
                thread_id,
                request_id,
            } = payload(method, params)?;
            let thread_id = validate_thread_id(method, thread_id)?;
            return Ok(Some(CodexSessionEvent::RequestResolved {
                thread_id,
                request_id,
            }));
        }
        _ => return Ok(None),
    };
    turn_event(method, key, event).map(Some)
}

fn item_event(
    method: &str,
    notification: &Item,
    lifecycle: ToolLifecycle,
) -> Result<Option<TurnEvent>, CodexAdapterError> {
    let item = &notification.item;
    let field = |name| {
        item.get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(method, format!("item is missing {name}")))
    };
    let kind = field("type")?;
    let event = match kind {
        "agentMessage" => {
            let item_id = field("id")?.into();
            let phase = payload::<Option<AssistantPhase>>(
                method,
                item.get("phase").cloned().unwrap_or(Value::Null),
            )?;
            match lifecycle {
                ToolLifecycle::Started => TurnEvent::TextStarted {
                    item_id,
                    phase,
                    started_at_ms: notification.started_at_ms,
                },
                ToolLifecycle::Completed => TurnEvent::TextCompleted {
                    item_id,
                    text: field("text")?.into(),
                    phase,
                    completed_at_ms: notification.completed_at_ms,
                },
            }
        }
        "reasoning" if lifecycle == ToolLifecycle::Completed => {
            let parts: ReasoningParts = payload(method, item.clone())?;
            TurnEvent::ReasoningCompleted {
                item_id: field("id")?.into(),
                summary: parts.summary,
                content: parts.content,
            }
        }
        "webSearch" => {
            let fields = WebSearchFields::deserialize(item)
                .map_err(|error| invalid(method, error.to_string()))?;
            TurnEvent::WebSearch(WebSearchUpdate {
                item_id: field("id")?.to_owned(),
                lifecycle,
                started_at_ms: notification
                    .started_at_ms
                    .filter(|_| lifecycle == ToolLifecycle::Started),
                patterns: fields.patterns(),
            })
        }
        "commandExecution"
        | "fileChange"
        | "mcpToolCall"
        | "dynamicToolCall"
        | "subAgentActivity"
        | "collabAgentToolCall" => TurnEvent::ToolChanged {
            item: ToolItem {
                id: field("id")?.into(),
                kind: kind.into(),
                payload: item.clone(),
                started_at_ms: notification.started_at_ms,
                completed_at_ms: notification.completed_at_ms,
            },
            lifecycle,
        },
        _ => return Ok(None),
    };
    Ok(Some(event))
}

fn request(
    request_id: RequestId,
    method: String,
    params: Option<Value>,
    session_id: Uuid,
) -> Result<CodexSessionEvent, CodexAdapterError> {
    let (key, event) = match method.as_str() {
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval" => {
            let approval: Approval = payload(&method, params.unwrap_or(Value::Null))?;
            let (kind, permissions) = match method.as_str() {
                "item/fileChange/requestApproval" => (ApprovalKind::FileChange, None),
                "item/permissions/requestApproval" => {
                    let permissions = approval
                        .permissions
                        .filter(Value::is_object)
                        .ok_or_else(|| invalid(&method, "permissions missing"))?;
                    (permissions_kind(&permissions), Some(permissions))
                }
                _ => (ApprovalKind::Command, None),
            };
            (
                approval.key,
                TurnEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id,
                        item_id: approval.item_id,
                        approval_id: approval.approval_id,
                        kind,
                        reason: approval.reason,
                        command: approval.command,
                        grant_root: approval.grant_root,
                        cwd: approval.cwd,
                        available_decisions: approval.available_decisions,
                        session_id,
                        allowance: None,
                        permissions: permissions.map(Box::new),
                    },
                },
            )
        }
        "item/tool/requestUserInput" => {
            let Input {
                key,
                item_id,
                questions,
            } = payload(&method, params.unwrap_or(Value::Null))?;
            (
                key,
                TurnEvent::UserInputRequested {
                    request: UserInputRequest {
                        request_id,
                        item_id,
                        questions,
                        session_id,
                    },
                },
            )
        }
        _ => {
            return Ok(CodexSessionEvent::UnsupportedRequest {
                request_id,
                method,
                params,
            });
        }
    };
    turn_event(&method, key, event)
}

fn permissions_kind(permissions: &Value) -> ApprovalKind {
    let filesystem = &permissions["fileSystem"];
    let requests = |mode| {
        filesystem
            .get(mode)
            .and_then(Value::as_array)
            .is_some_and(|paths| !paths.is_empty())
            || filesystem["entries"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["access"] == mode))
    };
    if requests("write") {
        ApprovalKind::FileChange
    } else if requests("read") {
        ApprovalKind::FileRead
    } else {
        ApprovalKind::Command
    }
}

fn turn_event(
    method: &str,
    key: TurnKey,
    event: TurnEvent,
) -> Result<CodexSessionEvent, CodexAdapterError> {
    if key.thread_id.as_str().is_empty() || key.turn_id.as_str().is_empty() {
        return Err(invalid(method, "empty thread or turn ID"));
    }
    Ok(CodexSessionEvent::Turn { key, event })
}
