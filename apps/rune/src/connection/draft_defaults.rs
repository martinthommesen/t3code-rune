use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceMode {
    #[default]
    Local,
    Worktree,
}

#[derive(Clone, Copy)]
pub(super) struct Preferences {
    mode: Option<WorkspaceMode>,
    from_environment: bool,
    pub(super) start_from_origin: bool,
}

impl Preferences {
    pub(super) fn from_settings(
        settings: &Value,
        project_id: &str,
        legacy: Option<WorkspaceMode>,
    ) -> Self {
        let project = &settings["projectSettingsOverrides"][project_id];
        let folded = settings["projectSettingsFolded"].as_bool().unwrap_or(false);
        let configured_project =
            mode(&project["defaultThreadEnvMode"]).or(if folded { None } else { legacy });
        let configured_environment = mode(&settings["defaultThreadEnvMode"]);
        Self {
            mode: configured_project.or(configured_environment),
            from_environment: configured_project.is_none(),
            start_from_origin: project["newWorktreesStartFromOrigin"]
                .as_bool()
                .or_else(|| settings["newWorktreesStartFromOrigin"].as_bool())
                .unwrap_or(true),
        }
    }

    pub(super) fn needs_project_file(self) -> bool {
        self.mode.is_none()
    }

    pub(super) fn needs_named_projects_root(self, has_threads: bool) -> bool {
        self.from_environment && self.mode == Some(WorkspaceMode::Worktree) && !has_threads
    }

    pub(super) fn effective_mode(
        self,
        workspace: &str,
        named_projects_root: Option<&str>,
        has_threads: bool,
        file_mode: Option<WorkspaceMode>,
    ) -> WorkspaceMode {
        if self.needs_named_projects_root(has_threads)
            && named_projects_root.is_some_and(|root| is_direct_child(root, workspace))
        {
            return WorkspaceMode::Local;
        }
        self.mode.or(file_mode).unwrap_or_default()
    }
}

fn mode(value: &Value) -> Option<WorkspaceMode> {
    match value.as_str() {
        Some("local") => Some(WorkspaceMode::Local),
        Some("worktree") => Some(WorkspaceMode::Worktree),
        _ => None,
    }
}

fn is_direct_child(root: &str, child: &str) -> bool {
    let root = comparable_path(root);
    let child = comparable_path(child);
    let separator = if root.contains('\\') { '\\' } else { '/' };
    let prefix = root.trim_end_matches(separator);
    child
        .strip_prefix(prefix)
        .and_then(|tail| tail.strip_prefix(separator))
        .is_some_and(|tail| !tail.is_empty() && !tail.contains(separator))
}

fn comparable_path(value: &str) -> Cow<'_, str> {
    let value = value.trim();
    let windows = value.starts_with("\\\\")
        || matches!(value.as_bytes(), [drive, b':', tail @ ..] if drive.is_ascii_alphabetic()
            && matches!(tail.first(), None | Some(b'/' | b'\\')));
    if windows {
        let trimmed = value.trim_end_matches(['/', '\\']);
        let mut normalized = trimmed.replace('/', "\\").to_lowercase();
        if trimmed.len() == 2 && trimmed.len() < value.len() {
            normalized.push('\\');
        }
        Cow::Owned(normalized)
    } else if value.starts_with('/') {
        Cow::Borrowed(value.trim_end_matches('/'))
    } else {
        Cow::Borrowed(value.trim_end_matches(['/', '\\']))
    }
}
