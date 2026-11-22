use std::collections::HashSet;

use serde_json::{Map, Number, Value};

use super::read_error;

const JS_MAX_INTEGER: f64 = 9_007_199_254_740_991.0;
const KNOWN_CONTEXT_KINDS: &[&str] = &[
    "image",
    "file",
    "terminal",
    "element",
    "preview-annotation",
    "review-comment",
    "mention",
    "skill",
    "thread",
    "environment",
    "response",
];

pub(super) fn attachments(raw: Option<&str>) -> rusqlite::Result<Vec<Value>> {
    let parsed = parse_json(raw)?;
    Ok(parsed
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|items| items.iter().map(attachment).collect())
        .unwrap_or_default())
}

pub(super) fn context(raw: Option<&str>) -> rusqlite::Result<Option<Value>> {
    Ok(parse_json(raw)?.as_ref().and_then(message_context))
}

fn parse_json(raw: Option<&str>) -> rusqlite::Result<Option<Value>> {
    let Some(raw) = raw else { return Ok(None) };
    match serde_json::from_str(raw) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            let message = error.to_string();
            if message.starts_with("recursion limit exceeded")
                || message.starts_with("lone leading surrogate")
                || message.starts_with("unexpected end of hex escape")
                || message.starts_with("number out of range")
            {
                return Err(read_error("legacy JSON cannot be represented losslessly"));
            }
            Ok(None)
        }
    }
}

struct Fields<'a> {
    input: &'a Map<String, Value>,
    output: Map<String, Value>,
}

impl<'a> Fields<'a> {
    fn new(value: &'a Value) -> Option<Self> {
        Some(Self {
            input: value.as_object()?,
            output: Map::new(),
        })
    }

    fn require<T: Into<Value>>(
        &mut self,
        name: &str,
        decode: impl FnOnce(&'a Value) -> Option<T>,
    ) -> Option<()> {
        self.output
            .insert(name.into(), decode(self.input.get(name)?)?.into());
        Some(())
    }

    fn optional<T: Into<Value>>(
        &mut self,
        name: &str,
        decode: impl FnOnce(&'a Value) -> Option<T>,
    ) -> Option<()> {
        if let Some(raw) = self.input.get(name) {
            self.output.insert(name.into(), decode(raw)?.into());
        }
        Some(())
    }

    fn finish(self) -> Value {
        Value::Object(self.output)
    }
}

fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn json_len(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number_len(number),
        Value::String(text) => json_string_len(text),
        Value::Array(items) => {
            2 + items.len().saturating_sub(1) + items.iter().map(json_len).sum::<usize>()
        }
        Value::Object(fields) => {
            2 + fields.len().saturating_sub(1)
                + fields
                    .iter()
                    .map(|(key, value)| json_string_len(key) + 1 + json_len(value))
                    .sum::<usize>()
        }
    }
}

fn json_string_len(value: &str) -> usize {
    2 + value
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => 2,
            '\0'..='\u{1f}' => 6,
            _ => ch.len_utf16(),
        })
        .sum::<usize>()
}

fn number_len(number: &Number) -> usize {
    let value = number
        .as_f64()
        .expect("JSON numbers have a finite representation");
    if value == 0.0 {
        return 1;
    }
    // JSON.stringify uses decimal notation at exponents -6 through 20.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("scientific notation has an exponent");
    let exponent = exponent
        .parse::<i32>()
        .expect("scientific exponent is an integer");
    if !(-6..21).contains(&exponent) {
        return mantissa.len() + 2 + exponent.unsigned_abs().to_string().len();
    }
    let sign = usize::from(value.is_sign_negative());
    let digits = mantissa.len() - sign - usize::from(mantissa.contains('.'));
    if exponent < 0 {
        sign + 1 + exponent.unsigned_abs() as usize + digits
    } else {
        let integer_digits = exponent as usize + 1;
        sign + integer_digits.max(digits) + usize::from(integer_digits < digits)
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
}

fn text(value: &Value, limit: usize) -> Option<&str> {
    value.as_str().filter(|value| js_len(value) <= limit)
}

fn name(value: &Value, limit: usize) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty() && js_len(value) <= limit)
}

fn reference_id(value: &Value) -> Option<&str> {
    name(value, 128).filter(|id| {
        id.bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-'))
    })
}

fn entity_id(value: &Value) -> Option<&str> {
    name(value, usize::MAX)
}

fn non_negative(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (0.0..=JS_MAX_INTEGER).contains(&number)).then_some(number as u64)
}

fn positive(value: &Value) -> Option<u64> {
    non_negative(value).filter(|number| *number > 0)
}

fn one(value: &Value) -> Option<u64> {
    non_negative(value).filter(|number| *number == 1)
}

fn null_or<'a, T: Into<Value>>(
    value: &'a Value,
    decode: impl FnOnce(&'a Value) -> Option<T>,
) -> Option<Value> {
    if value.is_null() {
        Some(Value::Null)
    } else {
        decode(value).map(Into::into)
    }
}

fn array<'a, T: Into<Value>>(
    value: &'a Value,
    limit: usize,
    mut decode: impl FnMut(&'a Value) -> Option<T>,
) -> Option<Vec<Value>> {
    let items = value.as_array()?;
    if items.len() > limit {
        return None;
    }
    items
        .iter()
        .map(|item| decode(item).map(Into::into))
        .collect()
}

fn limited_json(value: &Value, limit: usize) -> Option<Value> {
    (json_len(value) <= limit).then(|| value.clone())
}

pub(in crate::server) fn attachment(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    let raw_kind = fields.input.get("type")?.as_str()?;
    let kind = match raw_kind {
        "image" | "file" => raw_kind,
        _ => {
            name(fields.input.get("type")?, 50).filter(|kind| !matches!(*kind, "image" | "file"))?
        }
    };
    fields.output.insert("type".into(), kind.into());
    fields.require("id", reference_id)?;
    fields.require("name", |value| name(value, 255))?;
    fields.require("mimeType", |value| {
        name(value, 100).filter(|mime| {
            kind != "image"
                || mime
                    .get(..6)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        })
    })?;
    fields.require("sizeBytes", |value| match kind {
        "file" => positive(value).filter(|size| *size <= 50 * 1024 * 1024),
        "image" => non_negative(value).filter(|size| *size <= 10 * 1024 * 1024),
        _ => non_negative(value),
    })?;
    match kind {
        "file" => fields.optional("source", |value| {
            let mut source = Fields::new(value)?;
            source.require("_tag", |value| {
                value.as_str().filter(|tag| *tag == "pasted-text")
            })?;
            Some(source.finish())
        })?,
        "image" => fields.optional("source", snapshot_source)?,
        _ => (),
    }
    Some(fields.finish())
}

fn snapshot_source(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    fields.require("kind", |value| {
        value.as_str().filter(|kind| *kind == "snap-shot")
    })?;
    fields.require("capturedAt", Value::as_str)?;
    fields.require("appName", |value| name(value, 255))?;
    fields.require("windowTitle", |value| {
        value
            .as_str()
            .map(js_trim)
            .filter(|value| js_len(value) <= 1_000)
    })?;
    fields.optional("accessibleText", |value| name(value, 32_000))?;
    fields.optional("accessibility", accessibility)?;
    fields.optional("appIdentifier", |value| name(value, 255))?;
    fields.optional("appIconDataUrl", |value| {
        name(value, 100_000).filter(|url| {
            url.get(..22)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:image/png;base64,"))
        })
    })?;
    Some(fields.finish())
}

fn dimensions(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    fields.require("width", positive)?;
    fields.require("height", positive)?;
    Some(fields.finish())
}

fn accessibility(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    let format = fields.input.get("format")?.as_str()?;
    fields.output.insert("format".into(), format.into());
    fields.require("truncated", Value::as_bool)?;
    match format {
        "flat-text" => fields.require("text", |value| name(value, 32_000))?,
        "element-tree" => {
            fields.require("coordinateSpace", |value| {
                value.as_str().filter(|space| *space == "captured-image")
            })?;
            fields.require("imageSize", dimensions)?;
            let mut nodes = 0;
            fields.require("root", |value| accessibility_node(value, &mut nodes))?;
            let output = fields.finish();
            return (json_len(&output) <= 32_000).then_some(output);
        }
        _ => return None,
    }
    Some(fields.finish())
}

fn accessibility_node(value: &Value, nodes: &mut usize) -> Option<Value> {
    *nodes += 1;
    if *nodes > 10_000 {
        return None;
    }
    let mut fields = Fields::new(value)?;
    fields.require("role", |value| name(value, 100))?;
    fields.optional("name", |value| name(value, 1_000))?;
    fields.optional("value", |value| name(value, 8_000))?;
    fields.optional("description", |value| name(value, 2_000))?;
    fields.require("bounds", |value| {
        null_or(value, |value| {
            let mut fields = Fields::new(value)?;
            fields.require("x", non_negative)?;
            fields.require("y", non_negative)?;
            fields.require("width", positive)?;
            fields.require("height", positive)?;
            Some(fields.finish())
        })
    })?;
    fields.optional("state", |value| {
        let mut state = Fields::new(value)?;
        for key in [
            "active", "busy", "editable", "enabled", "expanded", "focused", "selected", "visible",
        ] {
            state.optional(key, Value::as_bool)?;
        }
        state.optional("checked", |value| {
            value
                .as_str()
                .filter(|state| matches!(*state, "on" | "off" | "mixed"))
        })?;
        Some(state.finish())
    })?;
    fields.optional("actions", |value| {
        array(value, 32, |entry| name(entry, 100))
    })?;
    fields.require("children", |value| {
        array(value, 10_000, |entry| accessibility_node(entry, nodes))
    })?;
    Some(fields.finish())
}

fn message_context(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    fields.require("version", one)?;
    fields.optional("guest", guest_attribution)?;
    fields.require("records", |value| {
        let records = value.as_array()?;
        if records.len() > 200 || json_len(value) > 16_000_000 {
            return None;
        }
        let decoded: Vec<_> = records.iter().filter_map(context_record).collect();
        let mut ids = HashSet::with_capacity(decoded.len());
        if decoded
            .iter()
            .any(|record| !ids.insert(record["contextId"].as_str()))
        {
            return None;
        }
        Some(decoded)
    })?;
    Some(fields.finish())
}

fn guest_attribution(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    fields.require("guestMessageId", entity_id)?;
    fields.require("name", |value| name(value, 40))?;
    fields.require("text", |value| text(value, 4_000))?;
    fields.optional("shareId", entity_id)?;
    Some(fields.finish())
}

fn context_record(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    fields.require("version", one)?;
    fields.require("contextId", reference_id)?;
    fields.require("label", |value| text(value, 200))?;
    let raw_kind = fields.input.get("kind")?.as_str()?;
    let kind = js_trim(raw_kind);
    if KNOWN_CONTEXT_KINDS.contains(&kind) && kind != raw_kind {
        return None;
    }
    fields.output.insert("kind".into(), kind.into());
    match kind {
        "image" | "file" => {
            fields.require("attachmentId", reference_id)?;
            fields.require("name", |value| name(value, 255))?;
            fields.require("mimeType", |value| name(value, 100))?;
            fields.require("sizeBytes", non_negative)?;
            if kind == "image" {
                fields.optional("source", snapshot_source)?;
            }
        }
        "terminal" => {
            fields.require("terminalId", |value| name(value, 255))?;
            fields.require("terminalLabel", |value| name(value, 255))?;
            fields.require("lineStart", non_negative)?;
            fields.require("lineEnd", non_negative)?;
            if fields.output["lineEnd"].as_u64()? < fields.output["lineStart"].as_u64()? {
                return None;
            }
            fields.require("text", |value| text(value, 64_000))?;
        }
        "element" => element_details(&mut fields)?,
        "preview-annotation" => preview_annotation(&mut fields)?,
        "review-comment" => review_comment(&mut fields)?,
        "mention" => fields.require("path", |value| name(value, 2_048))?,
        "skill" => fields.require("name", |value| name(value, 255))?,
        "thread" | "environment" => {
            fields.require("environmentId", entity_id)?;
            if kind == "thread" {
                fields.require("threadId", entity_id)?;
                fields.require("title", |value| text(value, 200))?;
            }
        }
        "response" => {
            fields.require("citation", citation)?;
            fields.require("itemId", entity_id)?;
            fields.optional("subagents", |value| {
                array(value, 20, |value| {
                    let mut fields = Fields::new(value)?;
                    fields.require("threadId", entity_id)?;
                    fields.require("title", |value| text(value, 200))?;
                    fields.optional("environmentId", entity_id)?;
                    Some(fields.finish())
                })
            })?;
        }
        _ => {
            let mut bytes = kind.bytes();
            if kind.len() > 40
                || !bytes.next()?.is_ascii_lowercase()
                || !bytes.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
            {
                return None;
            }
            fields.require("payload", |value| limited_json(value, 64_000))?;
        }
    }
    Some(fields.finish())
}

fn short_text(value: &Value) -> Option<&str> {
    text(value, 2_048)
}

fn element_details(fields: &mut Fields<'_>) -> Option<()> {
    fields.require("pageUrl", short_text)?;
    fields.require("pageTitle", |value| null_or(value, short_text))?;
    fields.require("tagName", |value| name(value, 255))?;
    fields.require("selector", |value| null_or(value, short_text))?;
    fields.require("htmlPreview", |value| text(value, 8_000))?;
    fields.require("componentName", |value| null_or(value, short_text))?;
    fields.require("source", |value| {
        null_or(value, |value| {
            let mut source = Fields::new(value)?;
            for key in ["functionName", "fileName"] {
                source.require(key, |value| null_or(value, short_text))?;
            }
            for key in ["lineNumber", "columnNumber"] {
                source.require(key, |value| null_or(value, non_negative))?;
            }
            Some(source.finish())
        })
    })?;
    fields.require("styles", |value| text(value, 8_000))?;
    Some(())
}

fn preview_annotation(fields: &mut Fields<'_>) -> Option<()> {
    fields.require("annotationId", short_text)?;
    fields.require("pageUrl", short_text)?;
    fields.require("pageTitle", |value| null_or(value, short_text))?;
    fields.require("comment", |value| text(value, 8_000))?;
    fields.require("targetSummary", short_text)?;
    fields.require("styleChanges", |value| array(value, 200, short_text))?;
    fields.optional("elements", |value| {
        array(value, 50, |value| {
            let mut element = Fields::new(value)?;
            element_details(&mut element)?;
            Some(element.finish())
        })
    })?;
    fields.optional("elementIds", |value| array(value, 50, short_text))?;
    fields.optional("regionCount", non_negative)?;
    fields.optional("strokeCount", non_negative)?;
    fields.optional("styleChangeDetails", |value| {
        array(value, 200, |value| {
            let mut change = Fields::new(value)?;
            change.require("targetId", short_text)?;
            change.require("selector", |value| null_or(value, short_text))?;
            change.require("property", short_text)?;
            change.require("previousValue", |value| text(value, 8_000))?;
            change.require("value", |value| text(value, 8_000))?;
            Some(change.finish())
        })
    })?;
    fields.optional("screenshotContextId", reference_id)?;
    Some(())
}

fn review_comment(fields: &mut Fields<'_>) -> Option<()> {
    fields.require("sectionId", |value| name(value, 255))?;
    fields.require("sectionTitle", short_text)?;
    fields.require("filePath", |value| name(value, 2_048))?;
    fields.require("startIndex", non_negative)?;
    fields.require("endIndex", non_negative)?;
    if fields.output["endIndex"].as_u64()? < fields.output["startIndex"].as_u64()? {
        return None;
    }
    fields.require("rangeLabel", short_text)?;
    fields.require("text", |value| text(value, 16_000))?;
    fields.require("diff", |value| text(value, 32_000))?;
    fields.optional("fenceLanguage", |value| text(value, 64))?;
    fields.optional("pullRequest", |value| {
        let mut request = Fields::new(value)?;
        request.require("number", positive)?;
        for key in ["title", "url", "headBranch", "baseBranch"] {
            request.require(key, short_text)?;
        }
        request.require("state", |value| {
            value
                .as_str()
                .filter(|state| matches!(*state, "open" | "closed" | "merged"))
        })?;
        request.require("isDraft", Value::as_bool)?;
        Some(request.finish())
    })?;
    Some(())
}

fn citation(value: &Value) -> Option<Value> {
    let mut fields = Fields::new(value)?;
    fields.require("version", one)?;
    for key in ["environmentId", "threadId", "messageId"] {
        fields.require(key, |value| name(value, 4_096))?;
    }
    fields.require("text", |value| {
        text(value, 8_000).filter(|value| !js_trim(value).is_empty())
    })?;
    fields.optional("comment", |value| text(value, 8_000))?;
    fields.require("start", non_negative)?;
    fields.require("end", non_negative)?;
    if fields.output["end"].as_u64()? <= fields.output["start"].as_u64()? {
        return None;
    }
    fields.require("prefix", |value| text(value, 32))?;
    fields.require("suffix", |value| text(value, 32))?;
    Some(fields.finish())
}
