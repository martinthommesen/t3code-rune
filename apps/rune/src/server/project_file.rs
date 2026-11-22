use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::{parse_settings_json, thread_commands::trim_js};
use crate::connection::WorkspaceMode;

pub(crate) fn icon_path(root: &Path) -> Option<String> {
    let source = fs::read(root.join("t3.json")).ok()?;
    let file = parse(&String::from_utf8_lossy(&source))?;
    Some(trimmed(file.get("iconPath")?, 512)?.to_owned())
}

pub(crate) fn workspace_mode(source: &str) -> Option<WorkspaceMode> {
    let file = parse(source)?;
    match file.get("defaultThreadEnvMode")?.as_str()? {
        "local" => Some(WorkspaceMode::Local),
        "worktree" => Some(WorkspaceMode::Worktree),
        _ => None,
    }
}

pub(super) fn parse(source: &str) -> Option<Map<String, Value>> {
    let Value::Object(file) = parse_settings_json(source)? else {
        return None;
    };
    optional(&file, "$schema", Value::is_string)?;
    optional(&file, "iconPath", |value| trimmed(value, 512).is_some())?;
    optional(&file, "defaultThreadEnvMode", |value| {
        matches!(value.as_str(), Some("local" | "worktree"))
    })?;
    optional(&file, "worktreeSubmodules", |value| {
        matches!(value.as_str(), Some("recursive" | "top-level" | "none"))
    })?;
    optional(&file, "ignoreRepositories", |value| {
        value
            .as_array()
            .is_some_and(|repositories| repositories.iter().all(Value::is_string))
    })?;
    optional(&file, "worktreeLayout", |value| {
        let Some(layout) = value.as_object() else {
            return false;
        };
        layout
            .get("create")
            .and_then(|value| trimmed(value, 4096))
            .is_some()
            && optional(layout, "remove", |value| trimmed(value, 4096).is_some()).is_some()
    })?;
    optional(&file, "worktreeSparsePatternsPath", |value| {
        trimmed(value, 512).is_some()
    })?;
    optional(&file, "scripts", |value| {
        value
            .as_array()
            .is_some_and(|scripts| scripts.len() <= 50 && scripts.iter().all(valid_script))
    })?;
    Some(file)
}

fn valid_script(value: &Value) -> bool {
    let Some(script) = value.as_object() else {
        return false;
    };
    for field in ["name", "command"] {
        if script
            .get(field)
            .and_then(|value| trimmed(value, usize::MAX))
            .is_none()
        {
            return false;
        }
    }
    for field in [
        "runOnWorktreeCreate",
        "runOnSettle",
        "stopOnSettle",
        "async",
        "autoOpenPreview",
    ] {
        if optional(script, field, Value::is_boolean).is_none() {
            return false;
        }
    }
    optional(script, "previewUrl", |value| {
        trimmed(value, usize::MAX).is_some()
    })
    .is_some()
        && optional(script, "icon", |value| {
            matches!(
                value.as_str(),
                Some("play" | "test" | "lint" | "configure" | "build" | "debug")
            )
        })
        .is_some()
}

fn optional(
    value: &Map<String, Value>,
    field: &str,
    check: impl FnOnce(&Value) -> bool,
) -> Option<()> {
    value.get(field).is_none_or(check).then_some(())
}

fn trimmed(value: &Value, limit: usize) -> Option<&str> {
    let original = value.as_str()?;
    let trimmed = trim_js(original);
    (!trimmed.is_empty() && original.encode_utf16().count() <= limit).then_some(trimmed)
}
