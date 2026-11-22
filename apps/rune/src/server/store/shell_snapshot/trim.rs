use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, Row};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::timestamp;

const SETTLED_KEPT: usize = 25;

const ACTIVE_INDEX: &str = "
    SELECT t.thread_id, t.project_id,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.lineage.relationshipToParent') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.lineage.parentThreadId') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.lineage.rootThreadId') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.lineage.sideChat') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.settledOverride') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.settledAt') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.pinnedAt') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.snoozedUntil') END,
      (
        t.thread_id IN (
          SELECT thread_id FROM orchestration_v2_projection_runs
          WHERE status IN ('queued', 'preparing', 'starting', 'running', 'waiting')
        )
        OR t.thread_id IN (
          SELECT thread_id FROM orchestration_v2_projection_runtime_requests WHERE status = 'pending'
        )
        OR t.thread_id IN (
          SELECT thread_id FROM orchestration_v2_projection_turn_items
            INDEXED BY orchestration_v2_projection_turn_items_shell_pending_idx
          WHERE type IN ('command_execution', 'dynamic_tool', 'subagent')
            AND status NOT IN ('completed', 'interrupted', 'failed', 'cancelled')
        )
        OR t.thread_id IN (
          SELECT thread_id FROM orchestration_v2_projection_turn_items
          WHERE type = 'link_request' AND status = 'waiting'
        )
        OR t.thread_id IN (
          SELECT thread_id FROM orchestration_v2_projection_provider_threads
          WHERE CASE WHEN json_valid(payload_json)
            THEN json_array_length(payload_json, '$.pendingBackgroundTasks') > 0
            ELSE 0 END
        )
        OR CASE WHEN CASE WHEN json_valid(t.payload_json)
          THEN json_type(t.payload_json, '$.pullRequests[0]') END IS NOT NULL
          THEN EXISTS (
            SELECT 1 FROM json_each(t.payload_json, '$.pullRequests') link
            WHERE json_type(link.value, '$.watch') IS NOT NULL
          )
          ELSE 0 END
      ),
      t.thread_id IN (
        SELECT thread_id FROM orchestration_v2_projection_subagents
        WHERE origin = 'app_owned' AND child_thread_id IS NULL
          AND json_type(payload_json, '$.remoteChild') = 'object'
      )
    FROM orchestration_v2_projection_threads t
    WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
    ORDER BY t.updated_at ASC, t.thread_id ASC";

#[derive(Clone, Copy)]
pub(super) struct Options {
    pub(super) omit_idle_subagents: bool,
    pub(super) omit_older_settled: bool,
}

struct RowFacts {
    id: String,
    project_id: String,
    parent_thread_id: Option<String>,
    root_thread_id: Option<String>,
    subagent: bool,
    side_chat: bool,
    settled_at: Option<OffsetDateTime>,
    pinned: bool,
    snoozed: bool,
    may_be_live: bool,
    has_remote_tasks: bool,
}

impl RowFacts {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        let settled_override: Option<String> = row.get(6)?;
        let settled_at: Option<String> = row.get(7)?;
        let relationship: Option<String> = row.get(2)?;
        Ok(Self {
            id: row.get(0)?,
            project_id: row.get(1)?,
            parent_thread_id: row.get(3)?,
            root_thread_id: row.get(4)?,
            subagent: relationship.as_deref() == Some("subagent"),
            side_chat: row.get::<_, Option<i64>>(5)? == Some(1),
            settled_at: settled_at
                .as_deref()
                .filter(|_| settled_override.as_deref() == Some("settled"))
                .and_then(parse_date),
            pinned: row.get::<_, Option<String>>(8)?.is_some(),
            snoozed: row.get::<_, Option<String>>(9)?.is_some(),
            may_be_live: row.get(10)?,
            has_remote_tasks: row.get(11)?,
        })
    }

    fn listed_settled_at(&self) -> Option<OffsetDateTime> {
        self.settled_at
            .filter(|_| !self.side_chat && !self.subagent)
    }
}

pub(super) struct Plan {
    pub(super) ids: Vec<String>,
    pub(super) active_subagents: HashSet<String>,
    subagent_parents: HashSet<String>,
    remote_task_parents: HashSet<String>,
    older_cutoff: Option<OffsetDateTime>,
    older_counts: Vec<Value>,
    options: Options,
}

impl Plan {
    pub(super) fn read(database: &Connection, options: Options) -> rusqlite::Result<Self> {
        let rows = database
            .prepare(ACTIVE_INDEX)?
            .query_map([], RowFacts::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let active_subagents = rows
            .iter()
            .filter(|row| row.subagent)
            .map(|row| row.id.clone())
            .collect();
        let subagent_parents: HashSet<_> = rows
            .iter()
            .filter(|row| row.subagent)
            .filter_map(|row| row.parent_thread_id.clone())
            .collect();
        let remote_task_parents = rows
            .iter()
            .filter(|row| row.has_remote_tasks)
            .map(|row| row.id.clone())
            .collect();
        let older_cutoff = options
            .omit_older_settled
            .then(|| {
                let mut dates: Vec<_> = rows
                    .iter()
                    .filter_map(RowFacts::listed_settled_at)
                    .collect();
                dates.sort_unstable_by(|left, right| right.cmp(left));
                dates
                    .get(SETTLED_KEPT - 1)
                    .or_else(|| dates.last())
                    .copied()
            })
            .flatten();
        let older_counts = older_cutoff.map_or_else(Vec::new, |before| count_older(&rows, before));
        let kept_subagents: HashSet<_> = rows
            .iter()
            .filter(|row| row.subagent)
            .filter(|row| {
                !options.omit_idle_subagents
                    || row.may_be_live
                    || row.has_remote_tasks
                    || subagent_parents.contains(&row.id)
            })
            .map(|row| row.id.as_str())
            .collect();
        let ancestors: HashSet<_> = rows
            .iter()
            .filter(|row| row.side_chat || kept_subagents.contains(row.id.as_str()))
            .flat_map(|row| {
                [
                    row.parent_thread_id.as_deref(),
                    row.root_thread_id.as_deref(),
                ]
            })
            .flatten()
            .collect();
        let ids = rows
            .iter()
            .filter(|row| {
                if row.subagent {
                    kept_subagents.contains(row.id.as_str())
                } else {
                    !row.listed_settled_at()
                        .zip(older_cutoff)
                        .is_some_and(|(date, cut)| date < cut)
                        || row.pinned
                        || row.snoozed
                        || row.may_be_live
                        || row.has_remote_tasks
                        || ancestors.contains(row.id.as_str())
                }
            })
            .map(|row| row.id.clone())
            .collect();
        Ok(Self {
            ids,
            active_subagents,
            subagent_parents,
            remote_task_parents,
            older_cutoff,
            older_counts,
            options,
        })
    }

    pub(super) fn apply(&self, threads: &mut Vec<Value>, sequence: u64) -> Option<Value> {
        if self.options.omit_idle_subagents {
            threads.retain(|thread| {
                let id = thread["id"].as_str().unwrap_or_default();
                !subagent(thread)
                    || live(thread)
                    || self.subagent_parents.contains(id)
                    || self.remote_task_parents.contains(id)
            });
        }
        if !self.options.omit_older_settled {
            return None;
        }
        let now = OffsetDateTime::now_utc();
        let before = self.older_cutoff.unwrap_or(now);
        let ancestors: HashSet<_> = threads
            .iter()
            .filter(|thread| !listed(thread))
            .flat_map(|thread| {
                [
                    thread["lineage"]["parentThreadId"].as_str(),
                    thread["lineage"]["rootThreadId"].as_str(),
                ]
            })
            .flatten()
            .map(str::to_owned)
            .collect();
        threads.retain(|thread| {
            let older = listed(thread)
                && thread["settledOverride"] == "settled"
                && thread["settledAt"]
                    .as_str()
                    .and_then(parse_date)
                    .is_some_and(|date| date < before);
            let id = thread["id"].as_str().unwrap_or_default();
            !older
                || !thread["pinnedAt"].is_null()
                || thread["snoozedUntil"]
                    .as_str()
                    .and_then(parse_date)
                    .is_some_and(|date| date > now)
                || live(thread)
                || ancestors.contains(id)
                || self.remote_task_parents.contains(id)
        });
        Some(json!({
            "before": timestamp(&before.format(&Rfc3339).expect("shell cutoff is an ISO date")),
            "asOfSequence": sequence,
            "counts": self.older_counts,
        }))
    }
}

fn count_older(rows: &[RowFacts], before: OffsetDateTime) -> Vec<Value> {
    let mut project_order = Vec::new();
    let mut counts = HashMap::new();
    for row in rows
        .iter()
        .filter(|row| row.listed_settled_at().is_some_and(|date| date < before))
    {
        let count = counts.entry(row.project_id.as_str()).or_insert_with(|| {
            project_order.push(row.project_id.as_str());
            0
        });
        *count += 1;
    }
    project_order
        .into_iter()
        .map(|id| json!({"projectId": id, "count": counts[id]}))
        .collect()
}

fn parse_date(text: &str) -> Option<OffsetDateTime> {
    let date = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    OffsetDateTime::from_unix_timestamp_nanos(
        date.unix_timestamp_nanos().div_euclid(1_000_000) * 1_000_000,
    )
    .ok()
}

pub(super) fn subagent(row: &Value) -> bool {
    row["lineage"]["relationshipToParent"] == "subagent"
}

fn listed(row: &Value) -> bool {
    !subagent(row) && row["lineage"]["sideChat"] != true
}

fn live(row: &Value) -> bool {
    !row["activityRunStatus"].is_null()
        || matches!(
            row["status"].as_str(),
            Some("queued" | "preparing" | "starting" | "running" | "waiting")
        )
        || !row["pendingRuntimeRequest"].is_null()
        || row["pendingBackgroundTasks"]
            .as_array()
            .is_some_and(|tasks| !tasks.is_empty())
}
