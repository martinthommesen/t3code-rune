use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::discovery::trim;

pub(super) fn icon_path(root: &Path) -> Option<String> {
    let source = fs::read(root.join("t3.json")).ok()?;
    let file = super::super::parse_settings_json(&String::from_utf8_lossy(&source))?;
    let file = file.as_object()?;
    let icon = trimmed(file.get("iconPath")?, 512)?;
    optional(file, "$schema", Value::is_string)?;
    optional(file, "defaultThreadEnvMode", |value| {
        matches!(value.as_str(), Some("local" | "worktree"))
    })?;
    optional(file, "worktreeSubmodules", |value| {
        matches!(value.as_str(), Some("recursive" | "top-level" | "none"))
    })?;
    optional(file, "ignoreRepositories", |value| {
        value
            .as_array()
            .is_some_and(|repositories| repositories.iter().all(Value::is_string))
    })?;
    optional(file, "worktreeLayout", |value| {
        let Some(layout) = value.as_object() else {
            return false;
        };
        layout
            .get("create")
            .and_then(|value| trimmed(value, 4096))
            .is_some()
            && optional(layout, "remove", |value| trimmed(value, 4096).is_some()).is_some()
    })?;
    optional(file, "worktreeSparsePatternsPath", |value| {
        trimmed(value, 512).is_some()
    })?;
    optional(file, "scripts", |value| {
        value
            .as_array()
            .is_some_and(|scripts| scripts.len() <= 50 && scripts.iter().all(valid_script))
    })?;
    Some(icon.to_owned())
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
    let trimmed = trim(original);
    (!trimmed.is_empty() && original.encode_utf16().count() <= limit).then_some(trimmed)
}
