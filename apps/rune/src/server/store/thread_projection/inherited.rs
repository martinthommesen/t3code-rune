use std::collections::HashMap;

use serde_json::{Value, json};

use super::{read_error, required_string, timeline};

pub(super) struct Source<'a> {
    pub(super) thread_id: &'a str,
    run_id: &'a str,
}

pub(super) fn source(thread: &Value) -> rusqlite::Result<Option<Source<'_>>> {
    let forked_from = &thread["forkedFrom"];
    if forked_from["type"] != "run" || thread["lineage"]["sideChat"] == true {
        return Ok(None);
    }
    Ok(Some(Source {
        thread_id: required_string(forked_from, "threadId")?,
        run_id: required_string(forked_from, "runId")?,
    }))
}

pub(super) fn merge(projection: &mut Value, inherited: &Value) -> rusqlite::Result<()> {
    let thread = &projection["thread"];
    let fork =
        source(thread)?.ok_or_else(|| read_error("The target thread has no fork source."))?;
    let fork_time = required_string(thread, "createdAt")?;
    let mut visible = through_run(inherited, &fork, timeline::epoch_millis(fork_time)?)?;
    visible.push(marker(thread, &fork, fork_time)?);
    visible.append(
        projection["visibleTurnItems"]
            .as_array_mut()
            .expect("visible items are a projection record set"),
    );
    for (position, item) in visible.iter_mut().enumerate() {
        item["position"] = position.into();
    }
    projection["visibleTurnItems"] = visible.into();
    Ok(())
}

fn through_run(
    projection: &Value,
    source: &Source<'_>,
    fork_time: i128,
) -> rusqlite::Result<Vec<Value>> {
    let runs = rows(projection, "runs");
    let Some(boundary_run) = runs.iter().find(|run| run["id"] == source.run_id) else {
        return Ok(Vec::new());
    };
    let boundary = ordinal(boundary_run)?;
    let mut ordinals = HashMap::new();
    for run in runs {
        if run["status"] == "rolled_back"
            && let Some(rolled_back_at) = run["completedAt"].as_str()
            && timeline::epoch_millis(rolled_back_at)? < fork_time
        {
            continue;
        }
        ordinals.insert(required_string(run, "id")?, ordinal(run)?);
    }
    let items = rows(projection, "turnItems");
    let is_visible = timeline::visibility(
        runs,
        rows(projection, "attempts"),
        items,
        timeline::History::Inherited,
    )?;
    let imported = projection["thread"]["historyOrigin"] == "v1_import";
    let mut prefix: Vec<_> = rows(projection, "visibleTurnItems")
        .iter()
        .filter(|row| row["item"]["threadId"] != source.thread_id || row["item"]["type"] == "fork")
        .cloned()
        .map(|mut row| {
            row["visibility"] = "inherited".into();
            row
        })
        .collect();
    for item in items.iter().filter(|item| is_visible(item)) {
        let at_or_before = item["runId"].as_str().map_or(imported, |id| {
            ordinals.get(id).is_some_and(|ordinal| *ordinal <= boundary)
        });
        if at_or_before {
            prefix.push(timeline::row(item, "inherited")?);
        }
    }
    Ok(prefix)
}

fn marker(thread: &Value, source: &Source<'_>, fork_time: &str) -> rusqlite::Result<Value> {
    let thread_id = required_string(thread, "id")?;
    let item_id = format!("turn-item:fork:{thread_id}");
    Ok(json!({
        "visibility": "synthetic",
        "sourceThreadId": source.thread_id,
        "sourceItemId": item_id,
        "item": {
            "id": item_id,
            "threadId": thread_id,
            "runId": null,
            "nodeId": null,
            "providerTurnId": null,
            "nativeItemRef": null,
            "parentItemId": null,
            "ordinal": 0,
            "status": "completed",
            "title": "Forked from conversation",
            "startedAt": null,
            "completedAt": fork_time,
            "updatedAt": fork_time,
            "type": "fork",
            "source": { "type": "run", "threadId": source.thread_id, "runId": source.run_id },
            "targetThreadId": thread_id,
        },
    }))
}

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field]
        .as_array()
        .expect("projection records are an array")
}

fn ordinal(run: &Value) -> rusqlite::Result<u64> {
    run["ordinal"]
        .as_u64()
        .ok_or_else(|| read_error("Run ordinal must be a non-negative integer."))
}
