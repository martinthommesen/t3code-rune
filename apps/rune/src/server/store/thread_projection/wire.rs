use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use super::super::THREAD_SEQUENCE;
use super::{read, read_error, required_string};

mod tool_output;

#[cfg(test)]
mod tests;

const CHECKPOINT_FILE_LIMIT: usize = 200;
const DETAIL_LIMIT: usize = 32_768;
const DYNAMIC_INPUT_LIMIT: usize = 16_384;
const TRUNCATION_MARKER: &str = "\n… output truncated for transport";

pub(in crate::server) fn snapshot(
    database: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Option<Value>> {
    let Some(mut projection) = read(database, thread_id)? else {
        return Ok(None);
    };
    let sequence: u64 = database.query_row(THREAD_SEQUENCE, [thread_id], |row| row.get(0))?;
    project(&mut projection)?;
    Ok(Some(json!({
        "kind": "snapshot",
        "snapshotSequence": sequence,
        "projection": projection,
    })))
}

fn project(projection: &mut Value) -> rusqlite::Result<()> {
    for checkpoint in records(projection, "checkpoints")? {
        cap_checkpoint_files(fields(checkpoint)?)?;
    }
    for handoff in records(projection, "contextHandoffs")? {
        let fields = fields(handoff)?;
        fields.shift_remove("history");
        fields.shift_remove("delivery");
        fields.insert("summaryText".to_owned(), "".into());
    }
    let mut local = std::mem::take(records(projection, "turnItems")?);
    {
        let mut projected = HashMap::new();
        for item in &mut local {
            project_item_once(item, &mut projected)?;
        }
        for row in records(projection, "visibleTurnItems")? {
            let item = fields(row)?
                .get_mut("item")
                .ok_or_else(|| read_error("The visible turn item was missing its payload."))?;
            project_item_once(item, &mut projected)?;
        }
    }
    *records(projection, "turnItems")? = local;
    Ok(())
}

fn project_item_once<'a>(
    item: &'a mut Value,
    cached: &mut HashMap<String, &'a Value>,
) -> rusqlite::Result<()> {
    let thread_id = required_string(item, "threadId")?;
    let id = required_string(item, "id")?;
    let key = format!("{thread_id}:{id}");
    if let Some(projected) = cached.get(&key) {
        *item = (*projected).clone();
    } else {
        project_item(item)?;
        cached.insert(key, item);
    }
    Ok(())
}

fn project_item(item: &mut Value) -> rusqlite::Result<()> {
    let kind = required_string(item, "type")?.to_owned();
    let fields = fields(item)?;
    match kind.as_str() {
        "checkpoint" => cap_checkpoint_files(fields)?,
        "handoff" => {
            fields.shift_remove("summary");
        }
        "command_execution" => project_command(fields)?,
        "file_change" => {
            let diff = take_text(fields, "diffStr")?;
            fields.shift_remove("oldStr");
            fields.shift_remove("newStr");
            if fields
                .get("status")
                .is_some_and(|status| status == "failed")
                && let Some(diff) = diff.filter(|text| !text.trim_matches(js_whitespace).is_empty())
            {
                fields.insert("diffStr".to_owned(), truncate_detail(diff).into());
            }
        }
        "subagent" => {
            for key in ["prompt", "progress", "result"] {
                if let Some(text) = fields.get_mut(key).and_then(|value| match value {
                    Value::String(text) => Some(text),
                    _ => None,
                }) && text.len() > DETAIL_LIMIT
                {
                    *text = truncate_detail(std::mem::take(text));
                }
            }
        }
        "dynamic_tool" => {
            if let Some(raw_output) = fields.shift_remove("output") {
                if let Some(compact_output) = tool_output::compact(&raw_output) {
                    fields.insert("output".to_owned(), compact_output);
                }
                if has_dynamic_value(&raw_output) {
                    fields.insert("outputOmitted".to_owned(), true.into());
                }
            }
            let input = fields
                .get_mut("input")
                .ok_or_else(|| read_error("Dynamic tool input is missing."))?;
            summarize_dynamic_input(input);
        }
        "notification"
        | "user_message"
        | "assistant_message"
        | "reasoning"
        | "proposed_plan"
        | "todo_list"
        | "user_input_request"
        | "file_search"
        | "web_search"
        | "approval_request"
        | "run_interrupt_request"
        | "run_interrupt_result"
        | "system_notice"
        | "error"
        | "compaction"
        | "fork"
        | "thread_created"
        | "secret_request"
        | "link_request" => {}
        _ => return Err(read_error("The stored turn item type is unsupported.")),
    }
    Ok(())
}

fn records<'a>(projection: &'a mut Value, name: &str) -> rusqlite::Result<&'a mut Vec<Value>> {
    projection
        .get_mut(name)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| read_error("Projection field is missing its records."))
}

fn fields(value: &mut Value) -> rusqlite::Result<&mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| read_error("Projection record must be a JSON object."))
}

fn take_text(fields: &mut Map<String, Value>, key: &str) -> rusqlite::Result<Option<String>> {
    match fields.shift_remove(key) {
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(read_error(
            "Projection record contains an invalid text field.",
        )),
        None => Ok(None),
    }
}

fn cap_checkpoint_files(checkpoint: &mut Map<String, Value>) -> rusqlite::Result<()> {
    if checkpoint.contains_key("filesTotal") {
        return Ok(());
    }
    let files = checkpoint
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| read_error("Checkpoint file list must be an array."))?;
    let count = files.len();
    if count <= CHECKPOINT_FILE_LIMIT {
        return Ok(());
    }
    let mut totals = [0_u64; 2];
    for file in files.iter() {
        for (index, field) in ["additions", "deletions"].into_iter().enumerate() {
            totals[index] = file[field]
                .as_u64()
                .and_then(|count| totals[index].checked_add(count))
                .ok_or_else(|| read_error("Checkpoint file counts are invalid."))?;
        }
    }
    files.truncate(CHECKPOINT_FILE_LIMIT);
    checkpoint.insert(
        "filesTotal".to_owned(),
        json!({
            "count": count, "additions": totals[0], "deletions": totals[1],
        }),
    );
    Ok(())
}

fn project_command(fields: &mut Map<String, Value>) -> rusqlite::Result<()> {
    let output = take_text(fields, "output")?;
    let failed = match fields.get("exitCode") {
        Some(code) => {
            code.as_i64()
                .ok_or_else(|| read_error("Command exit code must be an integer."))?
                != 0
        }
        None => {
            fields
                .get("outputIndicatesFailure")
                .is_some_and(|flag| flag == true)
                || output
                    .as_deref()
                    .is_some_and(command_text_indicates_failure)
        }
    };
    if failed {
        fields.insert("outputIndicatesFailure".to_owned(), true.into());
    }
    if output.is_some_and(|text| !text.trim_matches(js_whitespace).is_empty()) {
        fields.insert("outputOmitted".to_owned(), true.into());
    }
    Ok(())
}

fn command_text_indicates_failure(output: &str) -> bool {
    let text = utf16_prefix(output, DETAIL_LIMIT).to_ascii_lowercase();
    [
        "file not found",
        "no files found",
        "enoent",
        "no such file",
        "commandnotfoundexception",
        "command not found",
        "is not recognized as the name of a cmdlet",
        "a parameter cannot be found that matches parameter name",
    ]
    .into_iter()
    .any(|needle| text.contains(needle))
        || (text.contains("cannot find path") && text.contains("because it does not exist"))
        || (text.contains("is not recognized") && text.contains("the term '"))
        || ["exit with exit code", "exited with exit code"]
            .into_iter()
            .any(|needle| {
                text.match_indices(needle).any(|(index, _)| {
                    let raw = &text[index + needle.len()..];
                    let trimmed = raw.trim_start_matches(js_whitespace);
                    trimmed.len() < raw.len() && starts_nonzero(trimmed)
                })
            })
        || text.match_indices("exit code").any(|(index, key)| {
            let raw = &text[index + key.len()..];
            let trimmed = raw.trim_start_matches(js_whitespace);
            let tail = if let Some(after_colon) = trimmed.strip_prefix(':') {
                after_colon.trim_start_matches(js_whitespace)
            } else if trimmed.len() < raw.len() {
                trimmed
            } else {
                return false;
            };
            starts_nonzero(tail)
                && tail
                    .bytes()
                    .find(|ch| !ch.is_ascii_digit())
                    .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != b'_')
        })
}

fn starts_nonzero(text: &str) -> bool {
    matches!(text.as_bytes().first(), Some(b'1'..=b'9'))
}

fn has_dynamic_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim_matches(js_whitespace).is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

fn summarize_dynamic_input(value: &mut Value) {
    let summary = if let Value::String(text) = value {
        if text.encode_utf16().count() <= DYNAMIC_INPUT_LIMIT
            && serde_json::to_string(text)
                .expect("JSON encodes strings")
                .len()
                <= DYNAMIC_INPUT_LIMIT
        {
            return;
        }
        summarize(text)
    } else {
        let mut serialized = String::new();
        stringify(value, &mut serialized);
        if serialized.len() <= DYNAMIC_INPUT_LIMIT {
            return;
        }
        summarize(&serialized)
    };
    *value = summary;
}

fn summarize(text: &str) -> Value {
    let mut summary = String::new();
    let mut pending_space = false;
    let mut units = 0;
    for ch in text.trim_start_matches(js_whitespace).chars() {
        if ch == '\n' {
            break;
        }
        if js_whitespace(ch) {
            pending_space = true;
            continue;
        }
        if pending_space {
            summary.push(' ');
            units += 1;
        }
        summary.push(ch);
        units += ch.len_utf16();
        pending_space = false;
        if units > 160 {
            break;
        }
    }
    if summary.is_empty() {
        summary.push_str("Large tool output");
    } else if units > 160 {
        summary.truncate(
            utf16_prefix(&summary, 159)
                .trim_end_matches(js_whitespace)
                .len(),
        );
        summary.push('…');
    }
    json!({ "summary": summary, "truncated": true })
}

fn truncate_detail(text: String) -> String {
    if text.len() <= DETAIL_LIMIT {
        return text;
    }
    let display =
        String::from_utf8_lossy(&text.as_bytes()[..DETAIL_LIMIT - TRUNCATION_MARKER.len()]);
    let prefix = display.strip_suffix('\u{fffd}').unwrap_or(&display);
    format!("{prefix}{TRUNCATION_MARKER}")
}

fn utf16_prefix(value: &str, limit: usize) -> &str {
    let mut length = 0;
    for (index, ch) in value.char_indices() {
        length += ch.len_utf16();
        if length > limit {
            return &value[..index];
        }
    }
    value
}

fn js_whitespace(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}'
}

fn stringify(value: &Value, output: &mut String) {
    match value {
        Value::Object(fields) => {
            output.push('{');
            let mut keys: Vec<_> = fields.iter().collect();
            keys.sort_by_key(|(key, _)| {
                key.parse::<u32>()
                    .ok()
                    .filter(|value| value.to_string() == **key)
                    .unwrap_or(u32::MAX)
            });
            for (index, (key, value)) in keys.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("JSON encodes strings"));
                output.push(':');
                stringify(value, output);
            }
            output.push('}');
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                stringify(value, output);
            }
            output.push(']');
        }
        Value::Number(number) => {
            let value = number
                .as_f64()
                .expect("JSON numbers have a finite representation");
            if value == 0.0 {
                output.push('0');
            } else if (1e-6..1e21).contains(&value.abs()) {
                output.push_str(&value.to_string());
            } else {
                let scientific = format!("{value:e}");
                let (mantissa, exponent) = scientific.split_once('e').expect("scientific notation");
                output.push_str(mantissa);
                output.push('e');
                if !exponent.starts_with('-') {
                    output.push('+');
                }
                output.push_str(exponent);
            }
        }
        _ => output.push_str(&value.to_string()),
    }
}
