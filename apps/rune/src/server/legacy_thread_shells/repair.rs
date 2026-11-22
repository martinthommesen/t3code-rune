use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    LEGACY_COLUMNS, LegacyThread, append_shell_event, pull_requests, record_projected_sequence,
    store_projection, timestamp,
};

mod shape;

const MISSING_METADATA: &str = "
    FROM orchestration_v2_legacy_imports AS legacy_import
    JOIN projection_threads AS thread ON thread.thread_id = legacy_import.thread_id
    JOIN orchestration_v2_projection_threads AS projection ON projection.thread_id = legacy_import.thread_id
    WHERE json_type(projection.payload_json, '$.pinnedAt') IS NULL
       OR json_type(projection.payload_json, '$.pinOrderKey') IS NULL
       OR json_type(projection.payload_json, '$.snoozedUntil') IS NULL
       OR json_type(projection.payload_json, '$.snoozedAt') IS NULL
       OR json_type(projection.payload_json, '$.unsettledAt') IS NULL
       OR json_type(projection.payload_json, '$.linkedPullRequest') IS NULL
       OR json_type(projection.payload_json, '$.pullRequests') IS NULL
       OR json_type(projection.payload_json, '$.branchPullRequest') IS NULL
       OR json_type(projection.payload_json, '$.activeOrderKey') IS NULL
    ORDER BY thread.created_at, thread.thread_id";

pub(super) fn reconcile(database: &mut Connection, at: OffsetDateTime) -> rusqlite::Result<usize> {
    let candidates = database
        .prepare(&format!(
            "SELECT {LEGACY_COLUMNS}, projection.payload_json {MISSING_METADATA}"
        ))?
        .query_map([], |row| {
            Ok((LegacyThread::from_row(row)?, row.get::<_, String>(24)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = timestamp(at);
    let mut count = 0;
    for (source, original) in candidates {
        let Some(mut current) = shape::stored_thread(&original) else {
            continue;
        };
        let legacy = source.payload(at)?;
        if !current.contains_key("pullRequests") {
            let pulls = if current.get("linkedPullRequest") == Some(&Value::Null) {
                Value::Array(Vec::new())
            } else if legacy["pullRequests"]
                .as_array()
                .is_some_and(|links| !links.is_empty())
            {
                legacy["pullRequests"].clone()
            } else {
                let link = current
                    .get("linkedPullRequest")
                    .unwrap_or(&legacy["linkedPullRequest"]);
                pull_requests::links("[]", link.is_object().then_some(link)).into()
            };
            current.insert("pullRequests".to_owned(), pulls);
        }
        for field in [
            "pinnedAt",
            "autoSettleDisabledAt",
            "pinOrderKey",
            "snoozedUntil",
            "snoozedAt",
            "unsettledAt",
            "linkedPullRequest",
            "branchPullRequest",
            "activeOrderKey",
        ] {
            current
                .entry(field)
                .or_insert_with(|| legacy[field].clone());
        }
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM orchestration_v2_projection_threads WHERE thread_id = ?1 AND payload_json = ?2
            )",
            params![source.id, original], |
            row| row.get::<_, bool>(0),
        )? {
            continue;
        }
        let payload = Value::Object(current);
        let id = format!(
            "migration:v1:thread:{}:metadata-repair:{}",
            source.id,
            Uuid::new_v4()
        );
        let sequence =
            append_shell_event(&transaction, &id, "thread.metadata-updated", &now, &payload)?;
        store_projection(&transaction, &payload)?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        count += 1;
    }
    Ok(count)
}
