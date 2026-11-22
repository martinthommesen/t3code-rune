use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in super::super) struct LaunchInput {
    pub(super) command_id: Trimmed,
    pub(super) project_id: Trimmed,
    pub(super) thread_id: Option<Trimmed>,
    pub(super) reuse_existing_thread: Option<bool>,
    pub(super) title: Trimmed,
    pub(super) generate_title: Option<bool>,
    pub(super) model_selection: ModelSelection,
    pub(super) runtime_mode: RuntimeMode,
    pub(super) interaction_mode: InteractionMode,
    pub(super) workspace_strategy: WorkspaceStrategy,
    pub(super) initial_message: Option<LaunchMessage>,
    pub(super) creation_source: Option<CreationSource>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(super) enum WorkspaceStrategy {
    Root {
        #[serde(
            default,
            deserialize_with = "present_value",
            skip_serializing_if = "Option::is_none"
        )]
        branch: Option<Trimmed>,
    },
    ExistingWorktree {
        worktree_path: Trimmed,
        #[serde(
            default,
            deserialize_with = "present_value",
            skip_serializing_if = "Option::is_none"
        )]
        branch: Option<Trimmed>,
    },
    Worktree {
        base_ref: Trimmed,
        #[serde(
            default,
            deserialize_with = "present_value",
            skip_serializing_if = "Option::is_none"
        )]
        branch: Option<Trimmed>,
        #[serde(
            default,
            deserialize_with = "present_value",
            skip_serializing_if = "Option::is_none"
        )]
        start_from_origin: Option<bool>,
        #[serde(
            default,
            deserialize_with = "repository_selection",
            skip_serializing_if = "Option::is_none"
        )]
        repositories: Option<Vec<RepositorySelection>>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct RepositorySelection {
    path: Trimmed,
}

fn repository_selection<'de, D: Deserializer<'de>>(
    input: D,
) -> Result<Option<Vec<RepositorySelection>>, D::Error> {
    let repositories = Vec::<RepositorySelection>::deserialize(input)?;
    if repositories.len() > 128 {
        return Err(serde::de::Error::custom(
            "at most 128 repository selections are supported",
        ));
    }
    Ok(Some(repositories))
}

fn present_value<'de, D, T>(input: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(input).map(Some)
}

impl WorkspaceStrategy {
    pub(super) fn worktree_path(&self) -> Option<&str> {
        match self {
            Self::ExistingWorktree { worktree_path, .. } => Some(worktree_path.0.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LaunchMessage {
    pub(super) message_id: Option<Trimmed>,
    pub(super) text: String,
    pub(super) context: Option<Value>,
    pub(super) attachments: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum CreationSource {
    #[default]
    Web,
    Mobile,
    Mcp,
    Provider,
    Server,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum InteractionMode {
    Default,
    Plan,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub(super) struct Trimmed(pub(super) String);

impl TryFrom<String> for Trimmed {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let trimmed = trim_js(&value);
        if trimmed.is_empty() {
            return Err("expected a non-blank string");
        }
        Ok(Self(trimmed.to_owned()))
    }
}

pub(super) fn trim_js(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelectionSource {
    #[serde(default, deserialize_with = "present")]
    instance_id: Option<Value>,
    provider: Option<Value>,
    model: Trimmed,
    options: Option<Options>,
}

fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Options {
    Selections(Vec<Selection>),
    Legacy(Map<String, Value>),
}

#[derive(Debug, Deserialize, Serialize)]
struct Selection {
    id: Trimmed,
    value: SelectionValue,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum SelectionValue {
    Text(Trimmed),
    Boolean(bool),
}

impl Options {
    fn normalize(self) -> Vec<Selection> {
        match self {
            Self::Selections(values) => values,
            Self::Legacy(values) => {
                let mut entries: Vec<_> = values.into_iter().collect();
                entries.sort_by_key(|(key, _)| javascript_property_index(key));
                entries
                    .into_iter()
                    .filter_map(|(id, value)| {
                        Some(Selection {
                            id: Trimmed::try_from(id).ok()?,
                            value: serde_json::from_value(value).ok()?,
                        })
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", try_from = "ModelSelectionSource")]
pub(super) struct ModelSelection {
    pub(super) instance_id: String,
    model: Trimmed,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<Selection>>,
}

impl TryFrom<ModelSelectionSource> for ModelSelection {
    type Error = &'static str;

    fn try_from(source: ModelSelectionSource) -> Result<Self, Self::Error> {
        let instance = source.instance_id.or(source.provider);
        let Some(instance_id) = instance
            .as_ref()
            .and_then(Value::as_str)
            .map(trim_js)
            .filter(|id| {
                id.len() <= 64
                    && id.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && id.bytes().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, b'-' | b'_')
                    })
            })
        else {
            return Err("invalid provider instance ID");
        };
        Ok(Self {
            instance_id: instance_id.to_owned(),
            model: source.model,
            options: source.options.map(Options::normalize),
        })
    }
}
