use std::fmt::Write;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::ThreadCommandError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCommand {
    #[serde(rename = "type")]
    kind: CommandKind,
    command_id: NonBlank,
    thread_id: NonBlank,
    project_id: NonBlank,
    title: NonBlank,
    #[serde(default, deserialize_with = "present_value")]
    title_renamed: Option<bool>,
    #[serde(rename = "createdBy")]
    _created_by: Actor,
    creation_source: CreationSource,
    model_selection: ModelSelectionInput,
    runtime_mode: RuntimeMode,
    interaction_mode: InteractionMode,
    branch: Nullable<NonBlank>,
    worktree_path: Nullable<NonBlank>,
}

#[derive(Deserialize)]
enum CommandKind {
    #[serde(rename = "thread.create")]
    Create,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Actor {
    User,
    Agent,
    System,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum CreationSource {
    Web,
    Mobile,
    Mcp,
    Provider,
    Server,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

impl RuntimeMode {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::ApprovalRequired => "approval-required",
            Self::AutoAcceptEdits => "auto-accept-edits",
            Self::Auto => "auto",
            Self::FullAccess => "full-access",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum InteractionMode {
    Default,
    Plan,
}

impl InteractionMode {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Nullable<T> {
    Present(T),
    Null,
}

#[derive(Deserialize, Serialize)]
#[serde(try_from = "String")]
pub(super) struct NonBlank(pub(super) String);

impl TryFrom<String> for NonBlank {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let trimmed = trim(&value);
        if trimmed.is_empty() {
            return Err("expected a non-blank string");
        }
        Ok(Self(trimmed.to_owned()))
    }
}

pub(in crate::server) fn trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(character, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
            | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelectionInput {
    #[serde(default, deserialize_with = "present_value")]
    instance_id: Option<Value>,
    provider: Option<Value>,
    model: NonBlank,
    options: Option<Options>,
}

fn present_value<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Options {
    Canonical(Vec<OptionSelection>),
    Legacy(serde_json::Map<String, Value>),
}

#[derive(Deserialize, Serialize)]
struct OptionSelection {
    id: NonBlank,
    value: OptionValue,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum OptionValue {
    Text(NonBlank),
    Boolean(bool),
}

impl Options {
    fn into_selections(self) -> Vec<OptionSelection> {
        match self {
            Self::Canonical(selections) => selections,
            Self::Legacy(values) => {
                let mut entries: Vec<_> = values.into_iter().collect();
                // Object.entries visits array indices first, then other keys in input order.
                entries.sort_by_key(|(key, _)| javascript_property_index(key));
                entries
                    .into_iter()
                    .filter_map(|(key, value)| {
                        let id = NonBlank::try_from(key).ok()?;
                        let value = serde_json::from_value(value).ok()?;
                        Some(OptionSelection { id, value })
                    })
                    .collect()
            }
        }
    }
}

fn javascript_property_index(key: &str) -> u32 {
    if !key.bytes().all(|byte| byte.is_ascii_digit()) || (key.len() > 1 && key.starts_with('0')) {
        return u32::MAX;
    }
    key.parse().unwrap_or(u32::MAX)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelSelection {
    pub(super) instance_id: String,
    model: NonBlank,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<OptionSelection>>,
}

impl TryFrom<ModelSelectionInput> for ModelSelection {
    type Error = ThreadCommandError;

    fn try_from(input: ModelSelectionInput) -> Result<Self, Self::Error> {
        let source = input.instance_id.or(input.provider);
        let instance_id = source.as_ref().and_then(Value::as_str).map(trim);
        let Some(instance_id) = instance_id.filter(|id| {
            id.len() <= 64
                && id.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }) else {
            return Err(ThreadCommandError::InvalidRequest(
                "Invalid model selection provider instance ID.".to_owned(),
            ));
        };
        Ok(Self {
            instance_id: instance_id.to_owned(),
            model: input.model,
            options: input.options.map(Options::into_selections),
        })
    }
}

pub(in crate::server) fn normalize_model_selection(raw: &str) -> Option<Value> {
    let input = serde_json::from_str::<ModelSelectionInput>(raw).ok()?;
    serde_json::to_value(ModelSelection::try_from(input).ok()?).ok()
}

pub(in crate::server) struct CreateThread {
    pub(super) command_id: String,
    pub(super) thread_id: String,
    pub(super) project_id: String,
    pub(super) title: String,
    pub(super) model_selection: ModelSelection,
    pub(super) runtime_mode: RuntimeMode,
    pub(super) interaction_mode: InteractionMode,
    creation_source: CreationSource,
    title_renamed: bool,
    branch: Option<String>,
    worktree_path: Option<String>,
}

impl CreateThread {
    pub(in crate::server) fn parse(payload: Value) -> Result<Self, ThreadCommandError> {
        let parsed = Self::parse_for_launch(payload)?;
        if parsed.worktree_path.is_some() {
            return Err(ThreadCommandError::InvalidRequest(
                "This command supports creating empty root-workspace threads.".to_owned(),
            ));
        }
        Ok(parsed)
    }

    pub(in crate::server) fn parse_for_launch(payload: Value) -> Result<Self, ThreadCommandError> {
        if [
            "initialMessage",
            "importedNativeThread",
            "linkOrigin",
            "delegatedFrom",
        ]
        .iter()
        .any(|key| payload.get(key).is_some())
        {
            return Err(ThreadCommandError::InvalidRequest(
                "This command supports creating empty root-workspace threads.".to_owned(),
            ));
        }
        let CreateCommand {
            kind: CommandKind::Create,
            command_id,
            thread_id,
            project_id,
            title,
            title_renamed,
            _created_by: _,
            creation_source,
            model_selection,
            runtime_mode,
            interaction_mode,
            branch,
            worktree_path,
        } = serde_json::from_value(payload).map_err(|error| {
            ThreadCommandError::InvalidRequest(format!("Invalid thread.create command. {error}"))
        })?;
        Ok(Self {
            command_id: command_id.0,
            thread_id: thread_id.0,
            project_id: project_id.0,
            title: title.0,
            title_renamed: title_renamed.unwrap_or(false),
            branch: match branch {
                Nullable::Null => None,
                Nullable::Present(branch) => Some(branch.0),
            },
            worktree_path: match worktree_path {
                Nullable::Null => None,
                Nullable::Present(path) => Some(path.0),
            },
            creation_source,
            model_selection: model_selection.try_into()?,
            runtime_mode,
            interaction_mode,
        })
    }

    pub(super) fn event_id(&self) -> String {
        event_id(&self.thread_id, &self.command_id)
    }

    pub(super) fn projection(&self, now: &str) -> Value {
        let mut thread = json!({
            "createdBy": "user",
            "creationSource": self.creation_source,
            "id": self.thread_id,
            "projectId": self.project_id,
            "title": self.title,
            "providerInstanceId": self.model_selection.instance_id,
            "modelSelection": self.model_selection,
            "runtimeMode": self.runtime_mode.as_str(),
            "interactionMode": self.interaction_mode.as_str(),
            "branch": self.branch,
            "worktreePath": self.worktree_path,
            "pullRequests": [],
            "activeProviderThreadId": null,
            "lineage": {
                "parentThreadId": null,
                "relationshipToParent": null,
                "rootThreadId": self.thread_id,
            },
            "forkedFrom": null,
            "createdAt": now,
            "updatedAt": now,
            "archivedAt": null,
            "settledOverride": null,
            "settledAt": null,
            "snoozedUntil": null,
            "snoozedAt": null,
            "lastVisitedAt": null,
            "deletedAt": null,
        });
        if self.title_renamed {
            thread["titleRenamed"] = true.into();
        }
        thread
    }
}

fn encode_id_part(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

pub(super) fn event_id(thread_id: &str, command_id: &str) -> String {
    format!(
        "event:thread:{}:command:{}:{}",
        encode_id_part(thread_id),
        encode_id_part(command_id),
        Uuid::new_v4(),
    )
}
