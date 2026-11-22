use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

use super::shell_snapshot;

const OLDER_ROWS: &str = "
    WITH older AS (
        SELECT t.thread_id, t.project_id,
               json_extract(t.payload_json, '$.settledAt') AS settled_at
        FROM orchestration_v2_projection_threads t
        WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
          AND json_extract(t.payload_json, '$.lineage.relationshipToParent') IS NOT 'subagent'
          AND json_extract(t.payload_json, '$.lineage.sideChat') IS NOT 1
          AND json_extract(t.payload_json, '$.settledOverride') = 'settled'
          AND json_extract(t.payload_json, '$.settledAt') IS NOT NULL
          AND (?2 IS NULL OR t.project_id IN (SELECT value FROM json_each(?2)))
    )";

const BELOW_CURSOR: &str = "
    WHERE older.settled_at < ?1
       OR (older.settled_at = ?1 AND older.thread_id > ?3)";

pub(in crate::server) struct Request {
    settled_at: String,
    thread_id: Option<String>,
    project_ids: Option<Vec<String>>,
    limit: u8,
}

impl Request {
    pub(in crate::server) fn decode(payload: &Value) -> Result<Self, String> {
        let fields = payload.as_object().ok_or("Expected object")?;
        let cursor = required(fields, "before", r#"["before"]"#)?
            .as_object()
            .ok_or_else(|| error_at("Expected object", r#"["before"]"#))?;
        let time_path = r#"["before"]["settledAt"]"#;
        let settled_at = required(cursor, "settledAt", time_path)?
            .as_str()
            .ok_or_else(|| error_at("Expected string", time_path))?;
        let thread_id = cursor
            .get("threadId")
            .map(|value| id(value, r#"["before"]["threadId"]"#))
            .transpose()?;
        let project_ids = fields
            .get("projectIds")
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| error_at("Expected array", r#"["projectIds"]"#))?
                    .iter()
                    .enumerate()
                    .map(|(index, value)| id(value, &format!(r#"["projectIds"][{index}]"#)))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let limit_path = r#"["limit"]"#;
        let limit = required(fields, "limit", limit_path)?
            .as_f64()
            .ok_or_else(|| error_at("Expected number", limit_path))?;
        if limit.fract() != 0.0 {
            return Err(error_at("Expected an integer", limit_path));
        }
        if !(0.0..=50.0).contains(&limit) {
            return Err(error_at("Expected a value between 0 and 50", limit_path));
        }
        Ok(Self {
            settled_at: settled_at.to_owned(),
            thread_id,
            project_ids,
            limit: limit as u8,
        })
    }
}

fn required<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a Value, String> {
    fields.get(key).ok_or_else(|| error_at("Missing key", path))
}

fn id(value: &Value, path: &str) -> Result<String, String> {
    let value = value
        .as_str()
        .ok_or_else(|| error_at("Expected string", path))?;
    let value =
        value.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}');
    if value.is_empty() {
        return Err(error_at("Expected a non-blank string", path));
    }
    Ok(value.to_owned())
}

fn error_at(message: &str, path: &str) -> String {
    format!("{message}\n  at {path}")
}

pub(super) fn read(
    database: &Connection,
    sequence: u64,
    request: &Request,
) -> rusqlite::Result<Value> {
    let project_ids = request
        .project_ids
        .as_deref()
        .map(|projects| serde_json::to_string(projects).expect("serializable project IDs"));
    let settled_at = &request.settled_at;
    let thread_id = request.thread_id.as_deref();
    let (threads, counts) = if request.limit == 0 {
        let sql = format!(
            "{OLDER_ROWS} SELECT older.project_id, COUNT(*) FROM older {BELOW_CURSOR}
            GROUP BY older.project_id"
        );
        let counts = database
            .prepare(&sql)?
            .query_map(params![settled_at, project_ids, thread_id], |row| {
                Ok(json!({"projectId": row.get::<_, String>(0)?, "count": row.get::<_, u64>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        (Vec::new(), counts)
    } else {
        let sql = format!(
            "{OLDER_ROWS} SELECT older.thread_id FROM older {BELOW_CURSOR}
            ORDER BY older.settled_at DESC, older.thread_id ASC
            LIMIT ?4"
        );
        let ids = database
            .prepare(&sql)?
            .query_map(
                params![settled_at, project_ids, thread_id, request.limit],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        let mut threads = shell_snapshot::read_selected_active_threads(database, &ids)?;
        let order: HashMap<_, _> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        threads.sort_by_key(|row| order[row["id"].as_str().expect("shell thread ID")]);
        (threads, Vec::new())
    };
    Ok(json!({"snapshotSequence": sequence, "threads": threads, "counts": counts}))
}
