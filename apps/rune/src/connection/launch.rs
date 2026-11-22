use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use t3_rune::providers::codex::{ReasoningEffort, ReasoningOptions, RuntimeMode, ServiceTier};

use super::projection::ProjectRow;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelSelection {
    #[serde(alias = "provider")]
    instance_id: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Value>,
}

impl ModelSelection {
    pub(crate) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn is_valid(&self) -> bool {
        !self.instance_id.trim().is_empty() && !self.model.trim().is_empty()
    }

    pub(crate) fn plain_model(&self) -> Option<&str> {
        self.options.is_none().then_some(self.model.as_str())
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn plain(provider: DraftProvider, model: &str) -> Self {
        Self {
            instance_id: provider.instance_id().to_owned(),
            model: model.to_owned(),
            options: None,
        }
    }

    pub(crate) fn codex_reasoning(effort: ReasoningEffort) -> Self {
        Self::codex_with_reasoning(ReasoningEffort::MODEL_ID, effort, None)
    }

    pub(crate) fn codex_with_reasoning(
        model: &str,
        effort: ReasoningEffort,
        service_tier: Option<ServiceTier>,
    ) -> Self {
        let mut options = vec![json!({"id":ReasoningEffort::OPTION_ID,"value":effort})];
        if let Some(tier) = service_tier {
            options.push(json!({"id":ServiceTier::OPTION_ID,"value":tier.as_str()}));
        }
        Self {
            instance_id: DraftProvider::Codex.instance_id().to_owned(),
            model: model.to_owned(),
            options: Some(Value::Array(options)),
        }
    }

    pub(crate) fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.codex_options().map(|options| options.effort)
    }

    pub(crate) fn service_tier(&self) -> Option<ServiceTier> {
        self.codex_options()
            .and_then(|options| options.service_tier)
    }

    fn codex_options(&self) -> Option<ReasoningOptions> {
        if self.instance_id != DraftProvider::Codex.instance_id() {
            return None;
        }
        ReasoningOptions::parse(&self.model, self.options.as_ref()?)
    }
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
        let hidden = self
            .provider_model_preferences
            .as_ref()
            .and_then(|preferences| preferences.get(&model.instance_id))
            .is_some_and(|preferences| preferences.hidden_models.contains(&model.model));
        self.provider_enabled(&model.instance_id, overrides)
            && (!hidden
                || instance.is_some_and(|instance| instance.defines_custom_model(&model.model)))
    }

    fn provider_enabled(&self, instance_id: &str, overrides: Option<&Value>) -> bool {
        let instance = self.provider_instances.get(instance_id);
        overrides
            .and_then(|project| project.get("providerInstances"))
            .and_then(|instances| instances.get(instance_id))
            .and_then(|instance| instance.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                instance.map_or_else(
                    || matches!(instance_id, "codex" | "claudeAgent"),
                    ProviderInstance::enabled,
                )
            })
    }
}

pub(super) struct Draft {
    pub(super) project_id: String,
    pub(super) thread_id: String,
    pub(super) prompt: String,
    pub(super) workspace: Workspace,
    pub(super) model: Option<ModelSelection>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DraftProvider {
    Codex,
    Claude,
}

impl DraftProvider {
    pub(crate) fn from_instance(instance: &str) -> Option<Self> {
        match instance {
            "codex" => Some(Self::Codex),
            "claudeAgent" => Some(Self::Claude),
            _ => None,
        }
    }

    pub(crate) fn instance_id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claudeAgent",
        }
    }
}

pub(crate) struct DraftModelPolicy {
    pub(crate) default_model: Option<ModelSelection>,
    runtime_mode: RuntimeMode,
    codex: ProviderPolicy,
    claude: ProviderPolicy,
}

struct ProviderPolicy {
    enabled: bool,
    hidden_models: Vec<String>,
}

impl DraftModelPolicy {
    pub(crate) fn full_access(&self) -> bool {
        matches!(self.runtime_mode, RuntimeMode::FullAccess)
    }

    pub(crate) fn allows(&self, provider: DraftProvider, model: &str) -> bool {
        let policy = match provider {
            DraftProvider::Codex => &self.codex,
            DraftProvider::Claude if self.full_access() && model == "claude-sonnet-4-6" => {
                &self.claude
            }
            DraftProvider::Claude => return false,
        };
        policy.enabled && !policy.hidden_models.iter().any(|hidden| hidden == model)
    }
}

fn defaults(
    settings: &ServerSettings,
    project: &ProjectRow,
    project_id: &str,
) -> Result<(Option<ModelSelection>, RuntimeMode), String> {
    let overrides = settings.project_settings_overrides.get(project_id);
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
        _ => settings.default_model_selection.clone(),
    }
    .filter(ModelSelection::is_valid);
    let runtime_mode = match overrides.and_then(|fields| fields.get("defaultRuntimeMode")) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| "The project has an invalid default runtime mode.".to_owned())?,
        None => settings.default_runtime_mode,
    };
    Ok((model_selection, runtime_mode))
}

fn read_settings(config: &Value) -> Result<ServerSettings, String> {
    ServerSettings::deserialize(config)
        .map_err(|_| "The environment returned invalid launch settings.".to_owned())
}

pub(super) fn draft_model_policy(
    config: &Value,
    project: &ProjectRow,
    project_id: &str,
) -> Option<DraftModelPolicy> {
    let settings = read_settings(config).ok()?;
    make_policy(&settings, project, project_id).ok()
}

fn make_policy(
    settings: &ServerSettings,
    project: &ProjectRow,
    project_id: &str,
) -> Result<DraftModelPolicy, String> {
    let (default_model, runtime_mode) = defaults(settings, project, project_id)?;
    let overrides = settings.project_settings_overrides.get(project_id);
    let policy = |provider: DraftProvider| {
        let id = provider.instance_id();
        ProviderPolicy {
            enabled: settings.provider_enabled(id, overrides),
            hidden_models: settings
                .provider_model_preferences
                .as_ref()
                .and_then(|preferences| preferences.get(id))
                .map_or_else(Vec::new, |preferences| {
                    preferences
                        .hidden_models
                        .iter()
                        .filter(|slug| {
                            settings
                                .provider_instances
                                .get(id)
                                .is_none_or(|instance| !instance.defines_custom_model(slug))
                        })
                        .cloned()
                        .collect()
                }),
        }
    };
    Ok(DraftModelPolicy {
        default_model,
        runtime_mode,
        codex: policy(DraftProvider::Codex),
        claude: policy(DraftProvider::Claude),
    })
}

pub(super) fn payload(config: Value, project: &ProjectRow, draft: &Draft) -> Result<Value, String> {
    let settings = read_settings(&config)?;
    let policy = make_policy(&settings, project, &draft.project_id)?;
    let model_selection = match &draft.model {
        Some(model) => {
            let provider = DraftProvider::from_instance(model.instance_id());
            if (model.plain_model().is_none() && model.reasoning_effort().is_none())
                || !provider.is_some_and(|provider| policy.allows(provider, model.model()))
                || (provider == Some(DraftProvider::Claude)
                    && !matches!(draft.workspace, Workspace::Root))
            {
                return Err(
                    "The selected model is unavailable for this project or workspace.".into(),
                );
            }
            Some(model.clone())
        }
        None => policy.default_model,
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
        "runtimeMode": policy.runtime_mode,
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
