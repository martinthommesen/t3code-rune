use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{read_error, required_string};

pub(super) fn visible(
    runs: &[Value],
    attempts: &[Value],
    items: &[Value],
) -> rusqlite::Result<Vec<Value>> {
    let visible = visibility(runs, attempts, items, History::Current)?;
    items
        .iter()
        .filter(|item| visible(item))
        .enumerate()
        .map(|(position, item)| {
            let mut row = row(item, "local")?;
            row["position"] = position.into();
            Ok(row)
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) enum History {
    Current,
    Inherited,
}

pub(super) fn visibility<'a>(
    runs: &'a [Value],
    attempts: &'a [Value],
    items: &'a [Value],
    history: History,
) -> rusqlite::Result<impl Fn(&Value) -> bool + 'a> {
    let cancelled_runs: HashSet<_> = match history {
        History::Current => HashSet::new(),
        History::Inherited => attempts
            .iter()
            .filter(|attempt| attempt["status"] == "cancelled")
            .filter_map(|attempt| attempt["runId"].as_str())
            .collect(),
    };
    let mut statuses = HashMap::new();
    for run in runs {
        let id = required_string(run, "id")?;
        let status = required_string(run, "status")?;
        let status = match (history, status) {
            (History::Inherited, "rolled_back") if cancelled_runs.contains(id) => "cancelled",
            (History::Inherited, "rolled_back") => continue,
            _ => status,
        };
        statuses.insert(id, status);
    }
    let mut superseded = HashSet::new();
    for attempt in attempts
        .iter()
        .filter(|attempt| attempt["status"] == "superseded")
    {
        if let (Some(run), Some(node)) = (attempt["runId"].as_str(), attempt["rootNodeId"].as_str())
        {
            superseded.insert((run, node));
        }
    }
    let stop_requests: HashSet<_> = items
        .iter()
        .filter(|item| item["type"] == "run_interrupt_request")
        .filter_map(|item| item["runId"].as_str())
        .collect();
    Ok(move |item: &Value| {
        let run = item["runId"].as_str();
        let status = run.and_then(|id| statuses.get(id)).copied();
        if status == Some("rolled_back") {
            return false;
        }
        if matches!(status, Some("queued" | "cancelled"))
            && item["type"] == "user_message"
            && item["inputIntent"] == "queued_turn"
        {
            return false;
        }
        if item["type"] == "run_interrupt_result"
            && let (Some(run), Some(node)) = (run, item["nodeId"].as_str())
        {
            return !superseded.contains(&(run, node)) || stop_requests.contains(run);
        }
        true
    })
}

pub(super) fn row(item: &Value, visibility: &str) -> rusqlite::Result<Value> {
    Ok(json!({
        "visibility": visibility,
        "sourceThreadId": required_string(item, "threadId")?,
        "sourceItemId": required_string(item, "id")?,
        "item": item,
    }))
}

pub(super) fn sort_messages(messages: &mut [Value], items: &[Value]) -> rusqlite::Result<()> {
    let mut item_ordinals = HashMap::new();
    for item in items {
        if matches!(
            item["type"].as_str(),
            Some("user_message" | "assistant_message")
        ) {
            let id = required_string(item, "messageId")?;
            let ordinal = item["ordinal"]
                .as_u64()
                .ok_or_else(|| read_error("Turn item ordinal must be a non-negative integer."))?;
            item_ordinals
                .entry(id)
                .and_modify(|existing: &mut u64| {
                    *existing = (*existing).min(ordinal);
                })
                .or_insert(ordinal);
        }
    }
    let mut sort_keys = HashMap::new();
    for message in messages.iter() {
        let created = required_string(message, "createdAt")?;
        let created = epoch_millis(created)?;
        let id = required_string(message, "id")?;
        let ordinal = item_ordinals.get(id).copied().unwrap_or(u64::MAX);
        sort_keys.insert(id.to_owned(), (ordinal, created));
    }
    messages.sort_by(|left, right| {
        let left = left["id"].as_str().expect("projection message has an ID");
        let right = right["id"].as_str().expect("projection message has an ID");
        sort_keys[left]
            .cmp(&sort_keys[right])
            .then_with(|| left.cmp(right))
    });
    Ok(())
}

pub(super) fn epoch_millis(value: &str) -> rusqlite::Result<i128> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| read_error("Projection timestamp must be RFC 3339."))?;
    Ok(timestamp.unix_timestamp_nanos().div_euclid(1_000_000))
}
