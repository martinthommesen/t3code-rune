use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    #[serde(rename = "trigger")]
    pub(super) _trigger: Trigger,
    #[serde(rename = "startedAt")]
    _started_at: String,
    pub(crate) finished_at: String,
    pub(crate) entries: Vec<Entry>,
    pub(crate) counts: Counts,
    pub(crate) omitted_count: f64,
    #[serde(default)]
    pub(crate) bytes_freed: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Trigger {
    Automatic,
    Manual,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Counts {
    pub(crate) removed: f64,
    pub(crate) kept: f64,
    pub(crate) failed: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    pub(crate) outcome: Outcome,
    pub(crate) reason: String,
    #[serde(deserialize_with = "nullable_string")]
    pub(crate) path: Option<String>,
    pub(crate) project_path: Option<String>,
    #[serde(deserialize_with = "nullable_thread_id")]
    pub(crate) thread_id: Option<String>,
    #[serde(deserialize_with = "nullable_string")]
    pub(crate) thread_title: Option<String>,
    pub(crate) bytes: Option<f64>,
    pub(crate) files: Option<f64>,
}

fn nullable_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(deserializer)
}

fn nullable_thread_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    nullable_string(deserializer)?
        .map(|value| {
            let value = value.trim_matches(|character: char| {
                (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
            });
            if value.is_empty() {
                return Err(serde::de::Error::custom("threadId must not be blank"));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Kind {
    Worktree,
    BrowserArtifacts,
    Logs,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    Removed,
    Kept,
    Failed,
}

pub(super) fn has_rules(settings: &Value) -> bool {
    let storage = &settings["storageCleanup"];
    let environment = &settings["worktreeCleanup"];
    let environment_worktrees = match environment["mode"].as_str() {
        Some("off") => false,
        Some("custom") => enabled_worktree_rules(&environment["rules"]),
        _ => enabled_worktree_rules(storage),
    };
    storage["browserArtifactsAfterDays"].is_number()
        || storage["logsAfterDays"].is_number()
        || environment_worktrees
        || settings["projectSettingsOverrides"]
            .as_object()
            .into_iter()
            .flat_map(|overrides| overrides.values())
            .any(|override_settings| {
                let policy = &override_settings["worktreeCleanup"];
                policy["mode"] == "custom" && enabled_worktree_rules(&policy["rules"])
            })
}

fn enabled_worktree_rules(rules: &Value) -> bool {
    rules["worktreeAfterDays"].is_number()
        || rules["worktreeSettledAfterDays"].is_number()
        || rules["worktreeOnMerge"] == true
        || rules["worktreeOnDelete"] == true
        || rules["worktreeUnchanged"] == true
}
