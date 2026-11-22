use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;

use super::Project;

pub(super) const KEY: &str = "defaultProjectScripts";
const MAX_SCRIPT_ID_LENGTH: usize = 24;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectAction {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) icon: ProjectActionIcon,
    pub(crate) run_on_worktree_create: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_on_settle: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_on_settle: Option<bool>,
    #[serde(rename = "async", skip_serializing_if = "Option::is_none")]
    pub(crate) asynchronous: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preview_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) auto_open_preview: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProjectActionIcon {
    #[default]
    Play,
    Test,
    Lint,
    Configure,
    Build,
    Debug,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ProjectActions {
    defaults: Vec<ProjectAction>,
    overrides: HashMap<String, Vec<ProjectAction>>,
    legacy: HashMap<String, Option<Vec<ProjectAction>>>,
    folded: bool,
    pub(crate) writable: bool,
    pub(crate) managed_by: Option<String>,
}

impl ProjectActions {
    pub(super) fn read(
        settings: &Value,
        projects: &Map<String, Value>,
        writable: bool,
        managed_by: Option<String>,
    ) -> Self {
        Self {
            defaults: Vec::deserialize(&settings[KEY]).unwrap_or_default(),
            overrides: projects
                .iter()
                .filter_map(|(id, project)| {
                    Vec::deserialize(&project[KEY])
                        .ok()
                        .map(|actions| (id.clone(), actions))
                })
                .collect(),
            legacy: HashMap::deserialize(&settings["projectScriptOverrides"]).unwrap_or_default(),
            folded: settings["projectSettingsFolded"] == true,
            writable,
            managed_by,
        }
    }

    pub(crate) fn is_overridden(&self, project: &Project) -> bool {
        self.overrides.contains_key(&project.id)
            || (!self.folded
                && self
                    .legacy
                    .get(&project.id)
                    .map_or(!project.scripts.is_empty(), Option::is_some))
    }

    pub(crate) fn effective<'a>(&'a self, project: &'a Project) -> &'a [ProjectAction] {
        if let Some(override_actions) = self.overrides.get(&project.id) {
            override_actions
        } else if self.folded {
            &self.defaults
        } else if let Some(legacy) = self.legacy.get(&project.id) {
            legacy.as_deref().unwrap_or(&self.defaults)
        } else if project.scripts.is_empty() {
            &self.defaults
        } else {
            &project.scripts
        }
    }

    pub(crate) fn next_id(&self, name: &str, projects: &[Project]) -> String {
        let taken: HashSet<_> = self
            .defaults
            .iter()
            .chain(self.overrides.values().flatten())
            .chain(projects.iter().flat_map(|project| &project.scripts))
            .map(|action| action.id.as_str())
            .collect();
        let mut base = String::new();
        for character in name.to_lowercase().chars() {
            if character.is_ascii_alphanumeric() {
                base.push(character);
            } else if !base.is_empty() && !base.ends_with('-') {
                base.push('-');
            }
            if base.len() == MAX_SCRIPT_ID_LENGTH {
                break;
            }
        }
        let base = base.trim_end_matches('-');
        let base = if base.is_empty() { "script" } else { base };
        if !taken.contains(base) {
            return base.to_owned();
        }
        for suffix in 2..10_000 {
            let suffix = format!("-{suffix}");
            let prefix = &base[..base.len().min(MAX_SCRIPT_ID_LENGTH - suffix.len())];
            let id = format!("{prefix}{suffix}");
            if !taken.contains(id.as_str()) {
                return id;
            }
        }
        let milliseconds = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        let id = format!("{base}-{milliseconds}");
        id[..id.len().min(MAX_SCRIPT_ID_LENGTH)].to_owned()
    }
}
