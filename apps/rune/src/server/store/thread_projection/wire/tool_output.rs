use serde_json::{Map, Value, json};

const MAX_PARSED_BYTES: usize = 16_384;
const MAX_METADATA_BYTES: usize = 8_192;
const MAX_ENVELOPE_DEPTH: usize = 4;
const MAX_ENVELOPE_NODES: usize = 128;
const MAX_CONTENT_BLOCKS: usize = 32;
const MAX_THREADS: usize = 100;
const MAX_ID_LENGTH: usize = 256;
const MAX_SENT_MESSAGE_LENGTH: usize = 4_000;
const MAX_HTML_TITLE_LENGTH: usize = 200;
const MAX_MEASURED_HEIGHTS: usize = 24;
const MAX_DOMAIN_LENGTH: usize = 256;
const MAX_DOMAINS: usize = 32;
const MAX_RESOURCE_URI_LENGTH: usize = 4_096;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const MCP_APP: &str = "t3McpApp";
const TERMINAL_OUTPUT_MARKER: &str = "\nCommand output:\n";

type Object = Map<String, Value>;

enum Input<'a> {
    Value(Option<&'a Value>),
    Json(&'a str),
}

struct Reader {
    remaining_bytes: usize,
    remaining_nodes: usize,
    exceeded: bool,
    metadata: Option<Object>,
    failed: bool,
}

pub(super) fn compact(output: &Value) -> Option<Value> {
    let mut reader = Reader {
        remaining_bytes: MAX_PARSED_BYTES,
        remaining_nodes: MAX_ENVELOPE_NODES,
        exceeded: false,
        metadata: None,
        failed: false,
    };
    reader.read(Input::Value(Some(output)), 0);
    let mut metadata = Object::new();
    if reader.failed {
        metadata.insert("isError".into(), Value::Bool(true));
    }
    if !reader.exceeded
        && let Some(data) = reader.metadata
    {
        metadata.extend(data);
    }

    if !metadata.is_empty() && oversized(&metadata) {
        metadata.shift_remove("threads");
        omit_thread_summary(&mut metadata);
        if metadata.contains_key(MCP_APP) && oversized(&metadata) {
            if let Some(Value::Object(app)) = metadata.get_mut(MCP_APP) {
                app.shift_remove("csp");
            }
            if oversized(&metadata) {
                metadata.shift_remove(MCP_APP);
            }
        }
    }
    (!metadata.is_empty()).then_some(Value::Object(metadata))
}

impl Reader {
    fn read(&mut self, input: Input<'_>, depth: usize) {
        if depth > MAX_ENVELOPE_DEPTH || self.remaining_nodes == 0 {
            self.exceeded = true;
            return;
        }
        self.remaining_nodes -= 1;

        match input {
            Input::Json(text) => self.read_json(text, depth),
            Input::Value(Some(Value::String(text))) => self.read_json(text, depth),
            Input::Value(Some(Value::Array(blocks))) => self.read_blocks(blocks, depth),
            Input::Value(Some(Value::Object(object))) => self.read_object(object, depth),
            Input::Value(_) => (),
        }
    }

    fn read_json(&mut self, text: &str, depth: usize) {
        if text.len() > self.remaining_bytes {
            self.exceeded = true;
            return;
        }
        self.remaining_bytes -= text.len();
        if let Ok(parsed) = serde_json::from_str(text) {
            self.read(Input::Value(Some(&parsed)), depth + 1);
        }
    }

    fn read_blocks(&mut self, blocks: &[Value], depth: usize) {
        if blocks.len() > MAX_CONTENT_BLOCKS {
            self.exceeded = true;
            return;
        }
        for block in blocks {
            let text = block.get("text");
            let content = text.and_then(|text| non_null(text.get("text"))).or(text);
            self.read(Input::Value(content), depth + 1);
            if self.exceeded {
                break;
            }
        }
    }

    fn read_object(&mut self, object: &Object, depth: usize) {
        self.failed |= object.get("isError") == Some(&Value::Bool(true))
            || object.get("is_error") == Some(&Value::Bool(true))
            || object.get("_tag").and_then(Value::as_str) == Some("OrchestratorMcpFailure")
            || non_null(object.get("error")).is_some();
        let nested = non_null(object.get("structuredContent"))
            .or_else(|| non_null(object.get("content")))
            .map(|value| Input::Value(Some(value)))
            .or_else(|| {
                let output = object.get("output")?.as_str()?;
                let (_, json) = output.split_once(TERMINAL_OUTPUT_MARKER)?;
                Some(Input::Json(json))
            });
        if let Some(nested) = nested {
            self.read(nested, depth + 1);
        } else if self.metadata.is_none() {
            self.metadata = Some(select_metadata(object));
        }
    }
}

fn select_metadata(data: &Object) -> Object {
    let mut metadata = Object::new();
    for key in ["threadId", "messageId", "taskId", "scheduledTaskId"] {
        if let Some(id) = data
            .get(key)
            .and_then(|value| bounded_text(value, MAX_ID_LENGTH))
        {
            metadata.insert(key.into(), id.into());
        }
    }
    if rolled_back(data) {
        metadata.insert("status".into(), "rolled_back".into());
    }
    for key in ["linkedCount", "unlinkedCount"] {
        if let Some(count) = data.get(key).and_then(safe_integer) {
            metadata.insert(key.into(), count.into());
        }
    }
    if let Some(message) = data
        .get("sentMessage")
        .and_then(|value| bounded_text(value, MAX_SENT_MESSAGE_LENGTH))
    {
        metadata.insert("sentMessage".into(), message.into());
    }
    if let Some(reference) = data.get("htmlRender").and_then(html_reference) {
        metadata.insert("htmlRender".into(), reference);
    }
    if let Some(reference) = data.get(MCP_APP).and_then(app_reference) {
        metadata.insert(MCP_APP.into(), reference);
    }
    if let Some(id) = data
        .get("thread")
        .and_then(|thread| thread.get("threadId"))
        .and_then(|value| bounded_text(value, MAX_ID_LENGTH))
    {
        metadata.insert("thread".into(), json!({ "threadId": id }));
    }
    if let Some(entries) = data.get("threads").and_then(Value::as_array) {
        if let Some(batch) = thread_batch(entries) {
            metadata.insert("threads".into(), Value::Array(batch));
        } else {
            omit_thread_summary(&mut metadata);
        }
    }
    metadata
}

fn thread_batch(entries: &[Value]) -> Option<Vec<Value>> {
    if entries.len() > MAX_THREADS {
        return None;
    }
    entries
        .iter()
        .map(|entry| {
            let entry = entry.as_object()?;
            let mut summary = Object::new();
            if let Some(id) = entry
                .get("threadId")
                .and_then(|value| bounded_text(value, MAX_ID_LENGTH))
            {
                summary.insert("threadId".into(), id.into());
            }
            if rolled_back(entry) {
                summary.insert("status".into(), "rolled_back".into());
            }
            (!summary.is_empty()).then_some(Value::Object(summary))
        })
        .collect()
}

fn html_reference(value: &Value) -> Option<Value> {
    let id = value.get("attachmentId")?.as_str()?;
    if id.is_empty() || !within_utf16_limit(id, MAX_ID_LENGTH) {
        return None;
    }
    let title = trim(value.get("title")?.as_str()?);
    let title = if title.is_empty() {
        "HTML"
    } else {
        utf16_prefix(title, MAX_HTML_TITLE_LENGTH)
    };
    let height = clipped_height(value.get("height")?)?;
    let mut reference = json!({ "attachmentId": id, "title": title, "height": height });
    if let Some(heights) = value.get("heights").and_then(measured_heights) {
        reference["heights"] = json!(heights);
    }
    Some(reference)
}

fn measured_heights(value: &Value) -> Option<Vec<(i64, u64)>> {
    let entries = value.as_array()?;
    if entries.is_empty() || entries.len() > MAX_MEASURED_HEIGHTS {
        return None;
    }
    let mut heights = entries
        .iter()
        .map(|entry| {
            let [width, height] = entry.as_array()?.as_slice() else {
                return None;
            };
            let width = safe_integer(width).filter(|width| (1..=10_000).contains(width))?;
            Some((width, clipped_height(height)?))
        })
        .collect::<Option<Vec<_>>>()?;
    heights.sort_by_key(|(width, _)| *width);
    Some(heights)
}

fn app_reference(value: &Value) -> Option<Value> {
    let id = bounded_text(value.get("attachmentId")?, MAX_ID_LENGTH)?;
    let server = bounded_text(value.get("server")?, MAX_ID_LENGTH)?;
    let tool = bounded_text(value.get("tool")?, MAX_ID_LENGTH)?;
    let uri = value.get("resourceUri")?.as_str()?;
    if !uri.starts_with("ui://") || !within_utf16_limit(uri, MAX_RESOURCE_URI_LENGTH) {
        return None;
    }
    let mut reference = json!({
        "attachmentId": id,
        "server": server,
        "tool": tool,
        "resourceUri": uri,
    });
    if let Some(csp) = value.get("csp").and_then(app_csp) {
        reference["csp"] = csp;
    }
    if let Some(permissions) = value.get("permissions").and_then(app_permissions) {
        reference["permissions"] = permissions;
    }
    if let Some(border) = value.get("prefersBorder").and_then(Value::as_bool) {
        reference["prefersBorder"] = border.into();
    }
    Some(reference)
}

fn app_csp(value: &Value) -> Option<Value> {
    let mut csp = Object::new();
    for key in [
        "connectDomains",
        "resourceDomains",
        "frameDomains",
        "baseUriDomains",
    ] {
        let Some(domains) = value.get(key).and_then(Value::as_array) else {
            continue;
        };
        let domains: Vec<_> = domains
            .iter()
            .filter_map(Value::as_str)
            .filter(|domain| valid_domain(domain))
            .take(MAX_DOMAINS)
            .map(Value::from)
            .collect();
        if !domains.is_empty() {
            csp.insert(key.into(), Value::Array(domains));
        }
    }
    (!csp.is_empty()).then_some(Value::Object(csp))
}

fn valid_domain(value: &str) -> bool {
    if value.len() > MAX_DOMAIN_LENGTH {
        return false;
    }
    let Some((scheme, authority)) = value.split_once("://") else {
        return false;
    };
    if !["http", "https", "ws", "wss"]
        .iter()
        .any(|known| scheme.eq_ignore_ascii_case(known))
    {
        return false;
    }
    let authority = authority.strip_prefix("*.").unwrap_or(authority);
    let host = if let Some((host, port)) = authority.split_once(':') {
        if port.is_empty() || port.len() > 5 || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        host
    } else {
        authority
    };
    host.split('.').all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn app_permissions(value: &Value) -> Option<Value> {
    let permissions: Object = ["camera", "microphone", "geolocation", "clipboardWrite"]
        .into_iter()
        .filter(|key| value.get(*key).is_some_and(Value::is_object))
        .map(|key| (key.into(), Value::Object(Object::new())))
        .collect();
    (!permissions.is_empty()).then_some(Value::Object(permissions))
}

fn oversized(metadata: &Object) -> bool {
    serde_json::to_vec(metadata).is_ok_and(|encoded| encoded.len() > MAX_METADATA_BYTES)
}

fn omit_thread_summary(metadata: &mut Object) {
    metadata.shift_remove("threadId");
    metadata.shift_remove("status");
}

fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

fn rolled_back(data: &Object) -> bool {
    data.get("status").and_then(Value::as_str) == Some("rolled_back")
}

fn safe_integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&number))
        .then_some(number as i64)
}

fn clipped_height(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .map(|height| height.clamp(80.0, 2_000.0).round() as u64)
}

fn bounded_text(value: &Value, limit: usize) -> Option<&str> {
    let text = value.as_str()?;
    (within_utf16_limit(text, limit) && !trim(text).is_empty()).then_some(text)
}

fn trim(text: &str) -> &str {
    text.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
}

fn within_utf16_limit(text: &str, limit: usize) -> bool {
    text.len() <= limit || text.encode_utf16().take(limit + 1).count() <= limit
}

fn utf16_prefix(text: &str, limit: usize) -> &str {
    let mut remaining = limit;
    for (index, ch) in text.char_indices() {
        let Some(next) = remaining.checked_sub(ch.len_utf16()) else {
            return &text[..index];
        };
        remaining = next;
    }
    text
}
