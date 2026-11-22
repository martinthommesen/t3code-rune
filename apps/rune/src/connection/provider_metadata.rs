use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

pub(crate) type ProviderCatalog = HashMap<String, ProviderMetadata>;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderMetadata {
    pub(crate) instance_id: String,
    pub(crate) driver: String,
    pub(crate) display_name: Option<String>,
    pub(crate) accent_color: Option<String>,
    #[serde(default)]
    pub(crate) enabled: bool,
    status: Option<String>,
    installed: Option<bool>,
    auth: Option<ProviderAuth>,
    pub(crate) message: Option<String>,
    availability: Option<String>,
    #[serde(default)]
    pub(crate) native_saved_thread_turns: bool,
    #[serde(default)]
    pub(crate) native_root_launch: bool,
    #[serde(default)]
    models: Vec<ProviderModel>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ProviderModel {
    pub(crate) slug: String,
    pub(crate) name: String,
    #[serde(rename = "shortName")]
    pub(crate) short_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ProviderAuth {
    status: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProviderReadiness {
    Ready,
    SignIn,
    Attention,
    Install,
    Disabled,
    Checking,
}

impl ProviderMetadata {
    pub(crate) fn readiness(&self) -> ProviderReadiness {
        let auth = self.auth.as_ref().map(|auth| auth.status.as_str());
        match (
            self.availability.as_deref(),
            self.enabled,
            self.installed,
            self.status.as_deref(),
            auth,
        ) {
            (Some("unavailable"), _, _, _, _) => ProviderReadiness::Attention,
            (_, false, _, _, _) | (_, _, _, Some("disabled"), _) => ProviderReadiness::Disabled,
            (_, _, Some(false), Some("warning"), Some("unknown")) | (_, _, None, None, _) => {
                ProviderReadiness::Checking
            }
            (_, _, Some(false), _, _) => ProviderReadiness::Install,
            (_, _, _, _, Some("unauthenticated")) => ProviderReadiness::SignIn,
            (_, _, Some(true), Some("ready"), _) => ProviderReadiness::Ready,
            _ => ProviderReadiness::Attention,
        }
    }

    pub(crate) fn selectable(&self, explicitly_requested: bool) -> bool {
        self.enabled
            && self.availability.as_deref() != Some("unavailable")
            && (explicitly_requested || self.status.as_deref() != Some("error"))
    }

    pub(crate) fn model(&self, slug: &str) -> Option<&ProviderModel> {
        if !self.selectable(false) {
            return None;
        }
        self.models.iter().find(|model| model.slug == slug)
    }
}

pub(super) fn last_catalog(values: Vec<Value>) -> Option<ProviderCatalog> {
    values.into_iter().rev().find_map(|value| {
        let providers = match value.get("type")?.as_str()? {
            "snapshot" => value.get("config")?,
            "providerStatuses" => value.get("payload")?,
            _ => return None,
        }
        .get("providers")?
        .as_array()?;
        Some(
            providers
                .iter()
                .filter_map(|provider| ProviderMetadata::deserialize(provider).ok())
                .map(|provider| (provider.instance_id.clone(), provider))
                .collect(),
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::last_catalog;

    #[test]
    fn latest_provider_status_replaces_snapshot_and_ignores_other_events() {
        let catalog = last_catalog(vec![
            json!({"type": "snapshot", "config": {"providers": [
                {"instanceId": "work", "driver": "codex", "enabled": true}
            ]}}),
            json!({"type": "providerStatuses", "payload": {"providers": [
                {"instanceId": "work", "driver": "claudeAgent", "enabled": false},
                {"instanceId": "invalid"}
            ]}}),
            json!({"type": "keybindingsUpdated", "payload": {}}),
        ])
        .unwrap();

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog["work"].driver, "claudeAgent");
        assert!(!catalog["work"].enabled);
    }
}
