use serde_json::Value;

use super::{Fields, trim};

pub(super) fn decode(value: &Value, path: &str) -> Result<Value, String> {
    let scripts = value
        .as_array()
        .ok_or_else(|| invalid("Expected array", path))?;
    scripts
        .iter()
        .enumerate()
        .map(|(index, script)| decode_script(script, &format!("{path}[{index}]")))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

pub(in crate::server) fn decode_script(value: &Value, path: &str) -> Result<Value, String> {
    let script = value
        .as_object()
        .ok_or_else(|| invalid("Expected object", path))?;
    let required = |key: &str| {
        script
            .get(key)
            .ok_or_else(|| invalid("Missing key", &field_path(path, key)))
    };
    let mut fields = Fields::new();
    for key in ["id", "name", "command"] {
        fields.insert(
            key.to_owned(),
            text(required(key)?, &field_path(path, key))?,
        );
    }
    let icon = required("icon")?;
    if !matches!(
        icon.as_str(),
        Some("play" | "test" | "lint" | "configure" | "build" | "debug")
    ) {
        return Err(invalid(
            "Expected \"play\" | \"test\" | \"lint\" | \"configure\" | \"build\" | \"debug\"",
            &field_path(path, "icon"),
        ));
    }
    fields.insert("icon".to_owned(), icon.clone());
    fields.insert(
        "runOnWorktreeCreate".to_owned(),
        boolean(
            required("runOnWorktreeCreate")?,
            &field_path(path, "runOnWorktreeCreate"),
        )?,
    );
    for key in [
        "runOnSettle",
        "stopOnSettle",
        "async",
        "previewUrl",
        "autoOpenPreview",
    ] {
        if let Some(value) = script.get(key) {
            let path = field_path(path, key);
            let value = if key == "previewUrl" {
                if !value.is_string() {
                    return Err(invalid("Expected string | null", &path));
                }
                text(value, &path)?
            } else {
                value
                    .as_bool()
                    .map(Value::Bool)
                    .ok_or_else(|| invalid("Expected boolean | null", &path))?
            };
            fields.insert(key.to_owned(), value);
        }
    }
    Ok(fields.into())
}

fn text(value: &Value, path: &str) -> Result<Value, String> {
    let value = value
        .as_str()
        .ok_or_else(|| invalid("Expected string", path))?;
    let value = trim(value);
    if value.is_empty() {
        return Err(invalid("Expected a non-blank string", path));
    }
    Ok(value.into())
}

fn boolean(value: &Value, path: &str) -> Result<Value, String> {
    value
        .as_bool()
        .map(Value::Bool)
        .ok_or_else(|| invalid("Expected boolean", path))
}

fn field_path(path: &str, key: &str) -> String {
    format!("{path}[\"{key}\"]")
}

fn invalid(message: &str, path: &str) -> String {
    format!("{message}\n  at {path}")
}
