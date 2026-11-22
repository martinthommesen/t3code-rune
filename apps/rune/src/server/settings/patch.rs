use std::sync::OnceLock;

use serde_json::Value;

use super::{Fields, project_overrides::ProjectPatch};

const RUNTIME_MODES: &str =
    "Expected \"approval-required\" | \"auto-accept-edits\" | \"auto\" | \"full-access\"";

pub(in crate::server) struct Patch {
    fields: Fields,
    projects: Option<ProjectPatch>,
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
            if let Some(expected) = invalid_value(key, value) {
                return Err(format!("{expected}\n  at [\"patch\"][\"{key}\"]"));
            }
            if supported(key) {
                patch.fields.insert(key.to_owned(), value.clone());
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
        for (key, value) in self.fields {
            if value == defaults[&key] {
                stored.shift_remove(&key);
            } else {
                stored.insert(key, value);
            }
        }
        Ok(())
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
    )
}

pub(super) fn invalid_value(key: &str, value: &Value) -> Option<&'static str> {
    match key {
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
