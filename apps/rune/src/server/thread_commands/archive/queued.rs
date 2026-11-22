use rusqlite::Transaction;
use serde_json::{Value, json};

use crate::server::turn_commands::ThreadEventStream;

use super::decode_projection;

pub(super) fn cancel(
    transaction: &Transaction<'_>,
    stream: &ThreadEventStream<'_>,
    thread: &str,
    now: &str,
    mut sequence: i64,
) -> rusqlite::Result<i64> {
    let mut events = Vec::new();
    for (kind, statement) in [
        ("run.updated", SELECT_RUNS),
        ("run-attempt.updated", SELECT_ATTEMPTS),
        ("node.updated", SELECT_NODES),
    ] {
        let mut query = transaction.prepare_cached(statement)?;
        let rows = query.query_map([thread], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (encoded, run, provider, node) = row?;
            let mut payload = decode_projection(&encoded)?;
            payload["status"] = json!("cancelled");
            payload["completedAt"] = json!(now);
            if kind == "run.updated" {
                payload["queuePosition"] = Value::Null;
            }
            let metadata = if let Some(node) = node {
                json!({"runId":run,"nodeId":node,"providerInstanceId":provider})
            } else {
                json!({"runId":run,"providerInstanceId":provider})
            };
            events.push((kind, metadata, payload));
        }
    }
    for (kind, metadata, payload) in events {
        sequence = stream.append(kind, now, &metadata, &payload)?;
    }
    Ok(sequence)
}

const SELECT_RUNS: &str = "
    SELECT run.payload_json, run.run_id, COALESCE(run.provider_instance_id,run.provider), NULL
    FROM orchestration_v2_projection_runs AS run
    WHERE run.thread_id=?1 AND run.status='queued'
    ORDER BY run.ordinal, run.run_id";

const SELECT_ATTEMPTS: &str = "
    SELECT attempt.payload_json, run.run_id, COALESCE(run.provider_instance_id,run.provider), attempt.root_node_id
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.run_id=run.run_id
    WHERE run.thread_id=?1 AND run.status='queued' AND attempt.status IN ('pending','running')
    ORDER BY run.ordinal, attempt.attempt_ordinal, attempt.attempt_id";

const SELECT_NODES: &str = "
    SELECT node.payload_json, run.run_id, COALESCE(run.provider_instance_id,run.provider), node.node_id
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_nodes AS node ON node.run_id=run.run_id
    WHERE run.thread_id=?1 AND run.status='queued' AND node.status IN ('pending','running','waiting')
    ORDER BY run.ordinal, node.rowid";
