use serde_json::{Map, Value, json};

use crate::server::store::decode_legacy_attachment;

use super::super::{
    nonempty, normalize_model_selection, parse_timestamp, pull_requests, timestamp,
};

const RUNTIME_MODES: &[&str] = &[
    "approval-required",
    "auto-accept-edits",
    "auto",
    "full-access",
];
const ACCESS_MODES: &[&str] = &[
    "read-only",
    "approval-required",
    "auto-accept-edits",
    "auto",
    "full-access",
];
const SETTLEMENT: &[&str] = &["settled", "active"];

pub(super) fn stored_thread(encoded: &str) -> Option<Map<String, Value>> {
    let raw: Value = serde_json::from_str(encoded).ok()?;
    let mut thread = fields(
        &raw,
        &[
            ("createdBy", |v| literal(v, &["user", "agent", "system"])),
            ("creationSource", |v| {
                literal(v, &["web", "mobile", "mcp", "provider", "server"])
            }),
            ("id", nonblank),
            ("projectId", nonblank),
            ("title", nonblank),
            ("providerInstanceId", provider_id),
            ("modelSelection", |v| {
                normalize_model_selection(&v.to_string())
            }),
            ("runtimeMode", |v| literal(v, RUNTIME_MODES)),
            ("interactionMode", |v| literal(v, &["default", "plan"])),
            ("branch", nullable_text),
            ("worktreePath", nullable_text),
            ("activeProviderThreadId", nullable_text),
            ("lineage", lineage),
            ("forkedFrom", |v| nullable(v, forked_from)),
            ("createdAt", datetime),
            ("updatedAt", datetime),
            ("archivedAt", nullable_datetime),
            ("deletedAt", nullable_datetime),
        ],
        &[
            ("linkedPullRequest", |v| {
                nullable(v, pull_requests::decode_linked)
            }),
            ("pullRequests", pull_requests::decode_links),
            ("branchPullRequest", |v| {
                nullable(v, pull_requests::decode_linked)
            }),
            ("issues", |v| array(v, issue)),
            ("historyOrigin", |v| literal(v, &["native", "v1_import"])),
            ("linkOrigin", link_origin),
            ("delegatedFrom", delegated_from),
            ("settledOverride", |v| {
                nullable(v, |v| literal(v, SETTLEMENT))
            }),
            ("settledAt", nullable_datetime),
            ("unsettledAt", nullable_datetime),
            ("snoozedUntil", nullable_datetime),
            ("snoozedAt", nullable_datetime),
            ("lastSnoozeWakeAt", nullable_datetime),
            ("limitRecovery", |v| nullable(v, limit_recovery)),
            ("guestShare", |v| nullable(v, guest_share)),
            ("pinnedAt", nullable_datetime),
            ("autoSettleDisabledAt", nullable_datetime),
            ("pinOrderKey", nullable_text),
            ("activeOrderKey", nullable_text),
            ("tags", |v| {
                Some(array(v, string).unwrap_or_else(|| json!([])))
            }),
            ("lastVisitedAt", nullable_datetime),
            ("opens", |v| Some(opens(v).unwrap_or(Value::Null))),
            ("titleRegeneration", |v| nullable(v, title_regeneration)),
            ("lastTitleRegeneration", |v| {
                nullable(v, last_title_regeneration)
            }),
            ("titleRenamed", boolean),
            ("rollbackRequestId", nonblank),
            ("rollbackFailure", |v| nullable(v, rollback_failure)),
            ("rollbackUnrestored", |v| nullable(v, rollback_unrestored)),
        ],
    )?;
    for key in ["settledOverride", "settledAt", "lastVisitedAt"] {
        thread.entry(key).or_insert(Value::Null);
    }
    Some(thread)
}

fn lineage(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("parentThreadId", nullable_text),
            ("relationshipToParent", |v| {
                nullable(v, |v| literal(v, &["fork", "subagent"]))
            }),
            ("rootThreadId", nonblank),
        ],
        &[("sideChat", boolean)],
    )
}

fn forked_from(raw: &Value) -> Option<Value> {
    let kind = raw.get("type")?.as_str()?;
    let mut data = match kind {
        "run" => fields(raw, &[("threadId", nonblank), ("runId", nonblank)], &[]),
        "node" => fields(raw, &[("nodeId", nonblank)], &[]),
        "provider_thread" => fields(
            raw,
            &[("providerThreadId", nonblank)],
            &[("providerTurnId", nonblank)],
        ),
        _ => None,
    }?;
    data.insert("type".to_owned(), kind.into());
    Some(Value::Object(data))
}

fn link_origin(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[("sessionId", nonblank), ("label", string)],
        &[("access", |v| literal(v, ACCESS_MODES))],
    )
}

fn delegated_from(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("environmentId", nonblank),
            ("threadId", nonblank),
            ("title", string),
        ],
        &[],
    )
}

fn issue(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("tracker", tracker_id),
            ("host", nonblank),
            ("scope", nonblank),
            ("key", nonblank),
            ("projectId", nonblank),
            ("url", nonblank),
            ("source", |v| literal(v, &["manual", "agent"])),
            ("linkedAt", string),
            ("displayKey", nonblank),
            ("title", nonblank),
        ],
        &[],
    )
}

fn limit_recovery(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("runId", nonblank),
            ("resetAt", string),
            ("autoResume", boolean),
        ],
        &[("requestId", nonblank), ("snooze", boolean)],
    )
}

fn guest_share(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("id", nonblank),
            ("host", nonblank),
            ("createdAt", string),
            ("expiresAt", string),
            ("guestName", nullable_text),
            ("messages", |v| array(v, guest_message)),
        ],
        &[],
    )
}

fn guest_message(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("id", nonblank),
            ("text", string),
            ("status", |v| literal(v, &["pending", "sent", "dismissed"])),
            ("submittedAt", string),
        ],
        &[("attachments", |v| array(v, decode_legacy_attachment))],
    )
}

fn opens(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("openedAt", number),
            ("score", |v| (v.as_f64()? >= 0.0).then(|| v.clone())),
            ("picks", |v| {
                (v.as_array()?.len() <= 3).then_some(())?;
                array(v, |pick| {
                    object(
                        pick,
                        &[
                            ("query", |v| {
                                (v.as_str()?.encode_utf16().count() <= 32).then(|| v.clone())
                            }),
                            ("at", number),
                        ],
                        &[],
                    )
                })
            }),
        ],
        &[],
    )
}

fn title_regeneration(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[("requestId", nonblank), ("startedAt", datetime)],
        &[],
    )
}

fn last_title_regeneration(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("requestId", nonblank),
            ("status", |v| {
                literal(v, &["renamed", "unchanged", "failed"])
            }),
            ("completedAt", datetime),
        ],
        &[("message", string)],
    )
}

fn rollback_failure(raw: &Value) -> Option<Value> {
    object(raw, &[("requestId", nonblank), ("message", nonblank)], &[])
}

fn rollback_unrestored(raw: &Value) -> Option<Value> {
    object(
        raw,
        &[
            ("requestId", nonblank),
            ("repositories", |v| array(v, nonblank)),
        ],
        &[],
    )
}

type Field = (&'static str, fn(&Value) -> Option<Value>);

fn fields(raw: &Value, required: &[Field], optional: &[Field]) -> Option<Map<String, Value>> {
    let raw = raw.as_object()?;
    let mut result = Map::new();
    for (key, decoder) in required {
        result.insert((*key).to_owned(), decoder(raw.get(*key)?)?);
    }
    for (key, decoder) in optional {
        if let Some(value) = raw.get(*key) {
            result.insert((*key).to_owned(), decoder(value)?);
        }
    }
    Some(result)
}

fn object(raw: &Value, required: &[Field], optional: &[Field]) -> Option<Value> {
    fields(raw, required, optional).map(Value::Object)
}

fn nullable(raw: &Value, decode: impl FnOnce(&Value) -> Option<Value>) -> Option<Value> {
    if raw.is_null() {
        Some(Value::Null)
    } else {
        decode(raw)
    }
}

fn array(raw: &Value, decode: impl Fn(&Value) -> Option<Value>) -> Option<Value> {
    let values = raw
        .as_array()?
        .iter()
        .map(decode)
        .collect::<Option<Vec<_>>>()?;
    Some(Value::Array(values))
}

fn nonblank(raw: &Value) -> Option<Value> {
    nonempty(raw.as_str()).map(Value::from)
}

fn nullable_text(raw: &Value) -> Option<Value> {
    nullable(raw, nonblank)
}

fn string(raw: &Value) -> Option<Value> {
    raw.as_str().map(Value::from)
}

fn boolean(raw: &Value) -> Option<Value> {
    raw.as_bool().map(Value::from)
}

fn number(raw: &Value) -> Option<Value> {
    raw.is_number().then(|| raw.clone())
}

fn datetime(raw: &Value) -> Option<Value> {
    parse_timestamp(raw.as_str()?).map(|value| timestamp(value).into())
}

fn nullable_datetime(raw: &Value) -> Option<Value> {
    nullable(raw, datetime)
}

fn literal(raw: &Value, values: &[&str]) -> Option<Value> {
    let text = raw.as_str()?;
    values.contains(&text).then(|| text.into())
}

fn provider_id(raw: &Value) -> Option<Value> {
    let id = nonempty(raw.as_str())?;
    (id.len() <= 64
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| id.into())
}

fn tracker_id(raw: &Value) -> Option<Value> {
    let id = nonempty(raw.as_str())?;
    (id.len() <= 64
        && id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    .then(|| id.into())
}
