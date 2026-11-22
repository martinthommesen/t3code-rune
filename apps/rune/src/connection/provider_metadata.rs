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
    availability: Option<String>,
}

impl ProviderMetadata {
    pub(crate) fn selectable(&self, explicitly_requested: bool) -> bool {
        self.enabled
            && self.availability.as_deref() != Some("unavailable")
            && (explicitly_requested || self.status.as_deref() != Some("error"))
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
