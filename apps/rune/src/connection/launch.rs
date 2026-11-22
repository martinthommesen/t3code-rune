use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::projection::ProjectRow;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelSelection {
    #[serde(alias = "provider")]
    instance_id: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Value>,
}

impl ModelSelection {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn is_valid(&self) -> bool {
        !self.instance_id.trim().is_empty() && !self.model.trim().is_empty()
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerSettings {
    default_model_selection: Option<ModelSelection>,
    default_runtime_mode: RuntimeMode,
    #[serde(default)]
    project_settings_folded: bool,
    #[serde(default)]
    project_settings_overrides: HashMap<String, Value>,
    #[serde(default)]
    provider_instances: HashMap<String, ProviderInstance>,
    provider_model_preferences: Option<HashMap<String, ModelPreferences>>,
}

#[derive(Deserialize)]
struct ProviderInstance {
    driver: String,
    enabled: Option<bool>,
    #[serde(default)]
    config: Value,
}

impl ProviderInstance {
    fn enabled(&self) -> bool {
        let config_enabled = self.config.get("enabled").and_then(Value::as_bool);
        self.enabled != Some(false)
            && config_enabled != Some(false)
            && self.enabled.or(config_enabled).unwrap_or({
                !matches!(
                    self.driver.as_str(),
                    "cursor" | "grok" | "fx" | "kiro" | "muse" | "pi" | "opencode" | "antigravity"
                )
            })
    }

    fn defines_custom_model(&self, slug: &str) -> bool {
        self.config
            .get("customModels")
            .and_then(Value::as_array)
            .is_some_and(|models| {
                models
                    .iter()
                    .filter_map(|model| {
                        model
                            .as_str()
                            .or_else(|| model.get("slug").and_then(Value::as_str))
                    })
                    .any(|model| model.trim() == slug)
            })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPreferences {
    #[serde(default)]
    hidden_models: Vec<String>,
}

impl ServerSettings {
    fn project_model_available(&self, model: &ModelSelection, overrides: Option<&Value>) -> bool {
        let instance = self.provider_instances.get(&model.instance_id);
        let enabled = overrides
            .and_then(|project| project.get("providerInstances"))
            .and_then(|instances| instances.get(&model.instance_id))
            .and_then(|instance| instance.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                instance.map_or_else(
                    || matches!(model.instance_id.as_str(), "codex" | "claudeAgent"),
                    ProviderInstance::enabled,
                )
            });
        let hidden = self
            .provider_model_preferences
            .as_ref()
            .and_then(|preferences| preferences.get(&model.instance_id))
            .is_some_and(|preferences| preferences.hidden_models.contains(&model.model));
        enabled
            && (!hidden
                || instance.is_some_and(|instance| instance.defines_custom_model(&model.model)))
    }
}

pub(super) struct Draft {
    pub(super) project_id: String,
    pub(super) thread_id: String,
    pub(super) prompt: String,
    pub(super) workspace: Workspace,
}

#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Workspace {
    Root,
    Worktree {
        base_ref: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        start_from_origin: bool,
    },
}

pub(super) fn payload(config: Value, project: &ProjectRow, draft: &Draft) -> Result<Value, String> {
    let settings: ServerSettings = serde_json::from_value(config)
        .map_err(|_| "The environment returned invalid launch settings.".to_owned())?;
    let overrides = settings.project_settings_overrides.get(&draft.project_id);
    let project_model = match overrides.and_then(|fields| fields.get("defaultModelSelection")) {
        Some(value) => Some(
            serde_json::from_value(value.clone())
                .map_err(|_| "The project has an invalid default model.".to_owned())?,
        ),
        None if !settings.project_settings_folded => {
            project.default_model_selection.clone().map(Some)
        }
        None => None,
    };
    let model_selection = match project_model {
        Some(None) => None,
        Some(Some(model)) if settings.project_model_available(&model, overrides) => Some(model),
        _ => settings.default_model_selection,
    }
    .filter(ModelSelection::is_valid);
    let runtime_mode = match overrides.and_then(|fields| fields.get("defaultRuntimeMode")) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| "The project has an invalid default runtime mode.".to_owned())?,
        None => settings.default_runtime_mode,
    };
    let title = initial_title(&draft.prompt);

    Ok(json!({
        "commandId": Uuid::new_v4().to_string(),
        "threadId": draft.thread_id,
        "creationSource": "web",
        "projectId": draft.project_id,
        "title": title,
        "generateTitle": true,
        "modelSelection": model_selection,
        "runtimeMode": runtime_mode,
        "interactionMode": "default",
        "workspaceStrategy": draft.workspace,
        "initialMessage": {
            "messageId": Uuid::new_v4().to_string(),
            "text": draft.prompt,
            "attachments": [],
        },
    }))
}

fn initial_title(prompt: &str) -> String {
    let normalized: String = prompt
        .split(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .nfc()
        .collect();
    if normalized.is_empty() {
        return "New thread".into();
    }
    if let Some((end, _)) = normalized.char_indices().nth(50) {
        format!("{}...", &normalized[..end])
    } else {
        normalized
    }
}
