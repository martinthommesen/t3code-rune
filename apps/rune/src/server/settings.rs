use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedReceiver;

use super::config_updates::Subscribers;

mod client;
mod disk;
mod jsonc;
mod patch;
mod project_overrides;
mod project_scripts;
mod providers;

pub(crate) use jsonc::parse as parse_settings_json;
pub(super) use patch::Patch;
use providers::ProviderInstance;
pub(super) use providers::{ClaudeInstance, CodexInstance, CodexInstanceError, EnvironmentValue};

const DEFAULTS: &str = include_str!("settings/defaults.json");

pub(super) fn write_document(path: &std::path::Path, contents: &[u8]) -> io::Result<()> {
    disk::write(path, contents)
}

#[derive(Clone)]
pub(super) struct SettingsFile {
    path: PathBuf,
    shared: Arc<Changes>,
}

#[derive(Default)]
struct Changes {
    writes: Mutex<()>,
    subscribers: Subscribers,
}

pub(super) struct SettingsSnapshot {
    provider_instances: Vec<(String, ProviderInstance)>,
    public: Value,
    readable: bool,
}

#[derive(Debug)]
pub(super) struct SettingsError(io::Error);

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Could not read the environment's settings file.")
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl SettingsFile {
    pub(super) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            shared: Arc::default(),
        }
    }

    pub(super) fn read(&self) -> Result<SettingsSnapshot, SettingsError> {
        match fs::read_to_string(&self.path) {
            Ok(content) => Ok(SettingsSnapshot::decode(&content)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(SettingsSnapshot::decode("{}"))
            }
            Err(error) => Err(SettingsError(error)),
        }
    }

    pub(super) fn subscribe_changes(&self) -> UnboundedReceiver<Arc<Value>> {
        self.shared.subscribers.subscribe()
    }

    pub(super) fn update(&self, patch: Patch, database: &Path) -> Result<Value, Value> {
        let _write = self
            .shared
            .writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut stored =
            disk::read(&self.path).map_err(|error| self.error("read-file", &error.to_string()))?;
        if SettingsSnapshot::decode_fields(&stored).is_none() {
            return Err(self.error("read-file", "Invalid settings file."));
        }
        let edits_projects = patch.edits_projects();
        if edits_projects {
            project_overrides::fold_legacy(&mut stored, database)
                .map_err(|error| self.error("read-project-settings", &error.to_string()))?;
        }
        patch
            .apply(&mut stored, &defaults())
            .map_err(|error| self.error("normalize", &error))?;
        if edits_projects || stored.get("projectSettingsFolded") == Some(&Value::Bool(true)) {
            project_overrides::refresh_legacy(&mut stored);
        }
        let next = SettingsSnapshot::decode_fields(&stored)
            .ok_or_else(|| self.error("normalize", "Could not normalize environment settings."))?;
        let mut contents = serde_json::to_string_pretty(&stored)
            .map_err(|error| self.error("normalize", &error.to_string()))?;
        contents.push('\n');
        disk::write(&self.path, contents.as_bytes())
            .map_err(|error| self.error("write-file", &error.to_string()))?;
        let settings = next.for_client();
        self.shared.subscribers.publish(&settings);
        Ok(settings)
    }

    pub(super) fn error(&self, operation: &str, cause: &str) -> Value {
        serde_json::json!({
            "_tag":"ServerSettingsError", "operation":operation,
            "settingsPath":self.path.to_string_lossy(), "cause":cause,
        })
    }
}

impl SettingsSnapshot {
    fn decode(content: &str) -> Self {
        if let Some(Value::Object(raw)) = super::parse_settings_json(content)
            && let Some(settings) = Self::decode_fields(&raw)
        {
            return settings;
        }

        let mut public = defaults();
        public["continueThreadsAfterServerUpdate"] = false.into();
        public["usageHistoryRetentionDays"] = "unlimited".into();
        Self {
            provider_instances: Vec::new(),
            public,
            readable: false,
        }
    }

    fn decode_fields(raw: &Fields) -> Option<Self> {
        let provider_instances = providers::resolve_instances(raw).ok()?;
        let public = client::project(defaults(), raw, &provider_instances)?;
        Some(Self {
            provider_instances,
            public,
            readable: true,
        })
    }

    pub(super) fn for_client(&self) -> Value {
        self.public.clone()
    }

    pub(super) fn codex_instance(
        &self,
        project_id: Option<&str>,
        instance_id: &str,
    ) -> Result<CodexInstance, CodexInstanceError> {
        if !self.readable {
            return Err(CodexInstanceError::InvalidSettings);
        }
        let (config, enabled) = self.effective_instance(project_id, instance_id);
        if config.map_or(instance_id != "codex", |instance| {
            instance.driver != "codex"
        }) {
            return Err(CodexInstanceError::Unavailable);
        }
        if !enabled {
            return Err(CodexInstanceError::Disabled);
        }
        providers::codex_config(config)
    }

    pub(super) fn claude_instance(
        &self,
        project_id: Option<&str>,
        instance_id: &str,
    ) -> Option<ClaudeInstance> {
        let (config, enabled) = self.effective_instance(project_id, instance_id);
        (self.readable
            && enabled
            && config.map_or(instance_id == "claudeAgent", |instance| {
                instance.driver == "claudeAgent"
            }))
        .then(|| providers::claude_config(config))
        .flatten()
    }

    fn effective_instance(
        &self,
        project_id: Option<&str>,
        instance_id: &str,
    ) -> (Option<&ProviderInstance>, bool) {
        let config = self
            .provider_instances
            .iter()
            .find(|(id, _)| id == instance_id)
            .map(|(_, instance)| instance);
        let enabled = project_id
            .and_then(|id| self.public["projectSettingsOverrides"].get(id))
            .and_then(|settings| settings["providerInstances"].get(instance_id))
            .and_then(|instance| instance["enabled"].as_bool())
            .unwrap_or_else(|| config.is_none_or(ProviderInstance::enabled));
        (config, enabled)
    }
}

fn defaults() -> Value {
    serde_json::from_str(DEFAULTS).expect("valid server settings defaults")
}

type Fields = Map<String, Value>;

fn text(value: &Value) -> Option<&str> {
    value.as_str().map(trim)
}

fn nonblank(value: &Value) -> Option<&str> {
    text(value).filter(|value| !value.is_empty())
}

fn trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(character, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
            | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

fn slug(value: &str) -> bool {
    ascii_slug(value, u8::is_ascii_alphabetic)
}

fn secret_slug(value: &str) -> bool {
    ascii_slug(value, u8::is_ascii_alphanumeric)
}

fn ascii_slug(value: &str, valid_prefix: fn(&u8) -> bool) -> bool {
    value.len() <= 64
        && value.as_bytes().first().is_some_and(valid_prefix)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests;
