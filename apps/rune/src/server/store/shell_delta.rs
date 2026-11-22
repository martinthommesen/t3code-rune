use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::shell_snapshot;

const REPLAY_MAX_GAP: u64 = 1_000;
const REPLAY_MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENTS_PER_BATCH: usize = 512;

const PRUNED_THROUGH: &str = "
    SELECT COALESCE((
        SELECT last_sequence FROM orchestration_v2_projection_metadata
        WHERE projection_name = 'event-log-pruned-through'
    ), 0)";

const SUBAGENTS: &str = "
    SELECT thread_id FROM orchestration_v2_projection_threads
    WHERE deleted_at IS NULL AND archived_at IS NULL
      AND json_extract(payload_json, '$.lineage.relationshipToParent') = 'subagent'";

const APPLICATION_RANGE: &str = "
    FROM orchestration_events INDEXED BY idx_orchestration_events_application_high_water
    WHERE sequence > ?1 AND sequence <= ?2
      AND (aggregate_kind = 'project'
        OR (application_event_version = 2 AND aggregate_kind = 'thread'))";

const PAYLOAD_BYTES: &str = "
    SELECT COALESCE(SUM(length(CAST(
        CASE
          WHEN event_type IN ('subagent.updated', 'turn-item.updated')
            AND json_type(payload_json, '$.promptSequence') = 'integer'
          THEN json_set(
            json_remove(payload_json, '$.promptSequence'),
            '$.prompt',
            (SELECT json_extract(anchor.payload_json, '$.prompt')
             FROM orchestration_events AS anchor
             WHERE anchor.sequence = json_extract(orchestration_events.payload_json, '$.promptSequence'))
          )
          ELSE payload_json
        END AS BLOB)
    )), 0)";

const EVENT_COLUMNS: &str = "
    SELECT sequence, aggregate_kind, stream_id, event_type,
        CASE WHEN event_type = 'thread.created' THEN
            json_extract(payload_json, '$.lineage.relationshipToParent') = 'subagent'
        END AS creates_subagent,
        CASE WHEN event_type = 'turn-item.updated' THEN
            json_extract(payload_json, '$.type') IN ('secret_request', 'link_request', 'user_message')
        END AS changes_user_wait";

// Domain event types accepted by the V2 replay. A newer database can contain
// events this build cannot interpret. Its cursor still advances past those rows.
const THREAD_EVENTS: &[&str] = &[
    "thread.created",
    "thread.archived",
    "thread.unarchived",
    "thread.deleted",
    "thread.settled",
    "thread.unsettled",
    "thread.snoozed",
    "thread.unsnoozed",
    "thread.pinned",
    "thread.auto-settle-set",
    "thread.unpinned",
    "thread.pin-reordered",
    "thread.active-reordered",
    "thread.tags-updated",
    "thread.visited",
    "thread.marked-unread",
    "thread.metadata-updated",
    "thread.pull-request-synced",
    "thread.issue-synced",
    "thread.runtime-mode-updated",
    "thread.interaction-mode-updated",
    "thread.model-selection-updated",
    "thread.provider-switched",
    "thread.guest-share-updated",
    "thread.pull-request-link-updated",
    "thread.visit-recorded",
    "run.created",
    "run.updated",
    "run.background-work-cancelled",
    "run.output-submitted",
    "run-attempt.created",
    "run-attempt.updated",
    "node.updated",
    "subagent.updated",
    "provider-session.attached",
    "provider-session.updated",
    "provider-session.detached",
    "provider-thread.updated",
    "provider-turn.updated",
    "runtime-request.updated",
    "message.updated",
    "turn-item.updated",
    "plan.updated",
    "checkpoint-scope.created",
    "checkpoint.captured",
    "checkpoint.rollback-requested",
    "context-handoff.updated",
    "context-transfer.created",
    "context-transfer.updated",
];

pub(in crate::server) struct ShellDeltaBatch {
    pub(in crate::server) sequence: u64,
    pub(in crate::server) items: Vec<Value>,
    pub(in crate::server) subagent_thread_ids: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Aggregate {
    Project,
    Thread,
}

struct Event {
    sequence: u64,
    aggregate: Aggregate,
    id: String,
    deleted: bool,
}

pub(super) fn read(
    database: &Connection,
    after_sequence: u64,
    sequence: u64,
    known_subagents: Option<&HashSet<String>>,
) -> rusqlite::Result<Option<ShellDeltaBatch>> {
    if !can_resume(database, after_sequence, sequence)? {
        return Ok(None);
    }
    let mut subagent_thread_ids = match known_subagents {
        Some(ids) => ids.clone(),
        None => database
            .prepare(SUBAGENTS)?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?,
    };
    let mut items = Vec::new();
    if after_sequence < sequence {
        let events = read_events(database, after_sequence, sequence, &mut subagent_thread_ids)?;
        for group in events.chunks(MAX_EVENTS_PER_BATCH) {
            let mut by_scope = HashMap::new();
            for event in group {
                by_scope.insert((event.aggregate, event.id.as_str()), event);
            }
            let mut changes: Vec<_> = by_scope.into_values().collect();
            changes.sort_unstable_by_key(|event| event.sequence);
            for event in changes {
                items.push(project_event(database, event)?);
            }
        }
    }
    Ok(Some(ShellDeltaBatch {
        sequence,
        items,
        subagent_thread_ids,
    }))
}

pub(super) fn can_resume(
    database: &Connection,
    after_sequence: u64,
    sequence: u64,
) -> rusqlite::Result<bool> {
    if after_sequence == sequence {
        return Ok(true);
    }
    if sequence
        .checked_sub(after_sequence)
        .is_none_or(|gap| gap > REPLAY_MAX_GAP)
    {
        return Ok(false);
    }
    let pruned: u64 = database.query_row(PRUNED_THROUGH, [], |row| row.get(0))?;
    if after_sequence < pruned {
        return Ok(false);
    }
    let bytes: u64 = database.query_row(
        &format!("{PAYLOAD_BYTES}{APPLICATION_RANGE}"),
        params![after_sequence, sequence],
        |row| row.get(0),
    )?;
    Ok(bytes <= REPLAY_MAX_BYTES)
}

fn read_events(
    database: &Connection,
    after_sequence: u64,
    sequence: u64,
    subagents: &mut HashSet<String>,
) -> rusqlite::Result<Vec<Event>> {
    let mut statement = database.prepare(&format!(
        "{EVENT_COLUMNS}{APPLICATION_RANGE} ORDER BY sequence"
    ))?;
    let mut rows = statement.query(params![after_sequence, sequence])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        let aggregate: String = row.get(1)?;
        let id: String = row.get(2)?;
        let event_type: String = row.get(3)?;
        let event_type = event_type.as_str();
        let aggregate = match aggregate.as_str() {
            "project" => {
                if !matches!(
                    event_type,
                    "project.created" | "project.meta-updated" | "project.deleted"
                ) {
                    continue;
                }
                Aggregate::Project
            }
            _ => {
                if !THREAD_EVENTS.contains(&event_type) {
                    continue;
                }
                if event_type == "thread.created" && row.get::<_, Option<bool>>(4)?.unwrap_or(false)
                {
                    subagents.insert(id.clone());
                }
                if subagents.contains(&id)
                    && matches!(
                        event_type,
                        "node.updated"
                            | "turn-item.updated"
                            | "provider-turn.updated"
                            | "message.updated"
                    )
                    && !row.get::<_, Option<bool>>(5)?.unwrap_or(false)
                {
                    continue;
                }
                Aggregate::Thread
            }
        };
        events.push(Event {
            sequence: row.get(0)?,
            aggregate,
            id,
            deleted: event_type == "project.deleted",
        });
    }
    Ok(events)
}

fn project_event(database: &Connection, event: &Event) -> rusqlite::Result<Value> {
    let Event {
        sequence,
        aggregate,
        id,
        deleted,
    } = event;
    let value = match aggregate {
        Aggregate::Project => {
            let project = if *deleted {
                None
            } else {
                shell_snapshot::read_project(database, id)?
            };
            match project {
                Some(project) => {
                    json!({"kind": "project.updated", "sequence": sequence, "project": project})
                }
                None => json!({"kind": "project.removed", "sequence": sequence, "projectId": id}),
            }
        }
        Aggregate::Thread => match shell_snapshot::read_active_thread(database, id)? {
            Some(thread) => {
                json!({"kind": "thread.updated", "sequence": sequence, "location": "active", "thread": thread})
            }
            None => {
                json!({"kind": "thread.removed", "sequence": sequence, "location": "active", "threadId": id})
            }
        },
    };
    Ok(value)
}

#[cfg(test)]
mod tests;
