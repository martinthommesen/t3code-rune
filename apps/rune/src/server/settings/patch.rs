use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::snooze;

use super::{Fields, favorites, project_overrides::ProjectPatch, text, worktree_location_history};

const RUNTIME_MODES: &str =
    "Expected \"approval-required\" | \"auto-accept-edits\" | \"auto\" | \"full-access\"";

pub(in crate::server) struct Patch {
    fields: Fields,
    projects: Option<ProjectPatch>,
    favorites: Option<Vec<favorites::Change>>,
    settings: bool,
    providers: bool,
    access: bool,
    unsupported: Option<String>,
}

impl Patch {
    pub(in crate::server) fn decode(payload: &Value) -> Result<Self, String> {
        let request = payload.as_object().ok_or("Expected object")?;
        let fields = request
            .get("patch")
            .ok_or("Missing key\n  at [\"patch\"]")?;
        let fields = fields
            .as_object()
            .ok_or("Expected object\n  at [\"patch\"]")?;
        let mut patch = Self {
            fields: Fields::new(),
            projects: None,
            favorites: None,
            settings: false,
            providers: false,
            access: false,
            unsupported: None,
        };
        for key in schema_keys() {
            let Some(value) = fields.get(key) else {
                continue;
            };
            let key = key.as_str();
            match key {
                "providerInstances" | "usageLimitSources" | "textGenerators"
                | "secretProviders" => {
                    patch.providers = true;
                    patch.settings |= key == "secretProviders"
                        && value
                            .as_object()
                            .is_some_and(|entries| entries.values().any(Value::is_null));
                }
                "requireConnectDeviceApproval" | "enablePreviewTunnel" => patch.access = true,
                _ => patch.settings = true,
            }
            if key == "projectSettingsOverrides" {
                let mut projects = ProjectPatch::decode(value)?;
                patch.providers |= projects.references_secret;
                if patch.unsupported.is_none() {
                    patch.unsupported = projects.unsupported.take();
                }
                patch.projects = Some(projects);
                continue;
            }
            if key == snooze::KEY {
                let presets = snooze::decode(value, r#"["patch"]["snoozePresets"]"#)?;
                patch.fields.insert(key.to_owned(), json!(presets));
                continue;
            }
            if key == favorites::DELTA_KEY {
                patch.favorites = Some(favorites::decode_changes(value)?);
                continue;
            }
            if let Some(expected) = invalid_value(key, value) {
                return Err(format!("{expected}\n  at [\"patch\"][\"{key}\"]"));
            }
            if supported(key) {
                let value = if matches!(
                    key,
                    "worktreesDirectory" | "branchNamePrefix" | "branchNameInstructions"
                ) {
                    text(value).expect("validated string setting").into()
                } else {
                    value.clone()
                };
                patch.fields.insert(key.to_owned(), value);
            } else if patch.unsupported.is_none() {
                patch.unsupported = Some(key.to_owned());
            }
        }
        if request.contains_key("providerInstanceMutation") {
            patch.providers = true;
            if patch.unsupported.is_none() {
                patch.unsupported = Some("providerInstanceMutation".to_owned());
            }
        }
        Ok(patch)
    }

    pub(in crate::server) fn required_permission(
        &self,
        settings_write: bool,
    ) -> Option<&'static str> {
        if !settings_write && (self.settings || (!self.providers && !self.access)) {
            return Some("settings:write");
        }
        if self.providers {
            return Some("providers:manage");
        }
        self.access.then_some("access:write")
    }

    pub(in crate::server) fn unsupported(&self) -> Option<&str> {
        self.unsupported.as_deref()
    }

    pub(super) fn edits_projects(&self) -> bool {
        self.projects.is_some()
    }

    pub(super) fn apply(self, stored: &mut Fields, defaults: &Value) -> Result<(), String> {
        if let Some(projects) = self.projects {
            projects.apply(stored)?;
        }
        if let Some(changes) = self.favorites {
            favorites::apply(stored, changes)?;
        }
        for (key, value) in self.fields {
            if key == "worktreesDirectory" {
                update_worktree_location_history(stored, value.as_str().expect("decoded location"));
            }
            if value == defaults[&key] {
                stored.shift_remove(&key);
            } else {
                stored.insert(key, value);
            }
        }
        Ok(())
    }
}

fn update_worktree_location_history(stored: &mut Fields, next: &str) {
    let previous = stored
        .get("worktreesDirectory")
        .and_then(text)
        .unwrap_or("");
    if previous == next {
        return;
    }
    let history = worktree_location_history(stored).unwrap_or_default();
    let remember_previous = !previous.is_empty() && !history.contains(&previous);
    let history: Vec<_> = history
        .into_iter()
        .filter(|directory| *directory != next)
        .chain(remember_previous.then_some(previous))
        .collect();
    let key = "previousWorktreesDirectories";
    if history.is_empty() {
        stored.shift_remove(key);
    } else {
        stored.insert(key.to_owned(), json!(history));
    }
}

fn schema_keys() -> &'static [String] {
    static KEYS: OnceLock<Vec<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        serde_json::from_str(include_str!("patch_keys.json")).expect("client settings schema keys")
    })
}

fn supported(key: &str) -> bool {
    matches!(
        key,
        "responseStreamingMode"
            | "continueThreadsAfterServerUpdate"
            | "defaultAutoPull"
            | "defaultRuntimeMode"
            | "defaultThreadEnvMode"
            | "newWorktreesStartFromOrigin"
            | "worktreesDirectory"
            | "branchNamingMode"
            | "branchNamePrefix"
            | "branchNameInstructions"
    )
}

pub(super) fn invalid_value(key: &str, value: &Value) -> Option<&'static str> {
    match key {
        "worktreesDirectory" | "branchNamePrefix" | "branchNameInstructions"
            if !value.is_string() =>
        {
            Some("Expected string")
        }
        "branchNamingMode" if !matches!(value.as_str(), Some("static" | "semantic" | "custom")) => {
            Some("Expected \"static\" | \"semantic\" | \"custom\"")
        }
        "continueThreadsAfterServerUpdate"
        | "defaultAutoPull"
        | "newWorktreesStartFromOrigin"
        | "requireConnectDeviceApproval"
        | "enablePreviewTunnel"
            if !value.is_boolean() =>
        {
            Some("Expected boolean")
        }
        "responseStreamingMode" if !matches!(value.as_str(), Some("turn" | "paragraph")) => {
            Some("Expected \"turn\" | \"paragraph\"")
        }
        "defaultThreadEnvMode"
            if !value.is_null() && !matches!(value.as_str(), Some("local" | "worktree")) =>
        {
            Some("Expected \"local\" | \"worktree\"")
        }
        "defaultRuntimeMode"
            if !matches!(
                value.as_str(),
                Some("approval-required" | "auto-accept-edits" | "auto" | "full-access")
            ) =>
        {
            Some(RUNTIME_MODES)
        }
        "providerInstances" | "usageLimitSources" | "textGenerators" | "secretProviders"
            if !value.is_object() =>
        {
            Some("Expected object")
        }
        _ => None,
    }
}
