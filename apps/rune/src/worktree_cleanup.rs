use std::collections::HashMap;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeepWhen {
    AnyLocalFiles,
    UncommittedChanges,
    TrackedChanges,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeletePolicy {
    environment: Option<KeepWhen>,
    projects: HashMap<String, Option<KeepWhen>>,
}

impl DeletePolicy {
    pub(crate) fn from_settings(settings: &Value) -> Self {
        let defaults = &settings["storageCleanup"];
        let environment = rules(&settings["worktreeCleanup"], defaults);
        let projects = settings["projectSettingsOverrides"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, project)| {
                project
                    .get("worktreeCleanup")
                    .map(|policy| (id.clone(), rules(policy, defaults)))
            })
            .collect();
        Self {
            environment,
            projects,
        }
    }

    pub(crate) fn when_deleted(&self, project_id: &str) -> Option<KeepWhen> {
        self.projects
            .get(project_id)
            .copied()
            .unwrap_or(self.environment)
    }

    pub(crate) fn automatically_removes_any(&self) -> bool {
        self.environment.is_some() || self.projects.values().any(Option::is_some)
    }
}

fn rules(policy: &Value, defaults: &Value) -> Option<KeepWhen> {
    let rules = match policy["mode"].as_str() {
        Some("off") => return None,
        Some("custom") => &policy["rules"],
        _ => defaults,
    };
    if rules["worktreeOnDelete"] != true {
        return None;
    }
    Some(match rules["worktreeKeepWhen"].as_str() {
        Some("any-local-files") => KeepWhen::AnyLocalFiles,
        Some("tracked-changes") => KeepWhen::TrackedChanges,
        _ => KeepWhen::UncommittedChanges,
    })
}
