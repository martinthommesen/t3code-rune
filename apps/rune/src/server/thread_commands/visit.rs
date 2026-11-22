use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use super::create::{NonBlank, event_id, trim};
use super::{
    CommittedThread, INSERT_RECEIPT, LATEST_THREAD_SEQUENCE, RECORD_PROJECTION_SEQUENCE, Receipt,
    SELECT_RECEIPT, ThreadCommandError, ThreadCommands, timestamp,
};

const COMMAND: &str = "thread.visit";
const OPEN_DEDUPE_MS: f64 = 10.0 * 60.0 * 1000.0;
const DECAY_HALF_LIFE_MS: f64 = 14.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const PICK_WINDOW_MS: f64 = 21.0 * 24.0 * 60.0 * 60.0 * 1000.0;

pub(super) const SELECT_THREAD: &str = "
    SELECT provider_instance_id, deleted_at,
           json_extract(payload_json, '$.lastVisitedAt'), json_extract(payload_json, '$.opens')
    FROM orchestration_v2_projection_threads WHERE thread_id = ?1";

pub(super) const APPEND_VISIT: &str = "
    INSERT INTO orchestration_events (
        event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
        command_id, causation_event_id, correlation_id, actor_kind, payload_json,
        metadata_json, application_event_version
    ) VALUES (
        ?1, 'thread', ?2,
        COALESCE((
            SELECT stream_version + 1 FROM orchestration_events
            WHERE aggregate_kind = 'thread' AND stream_id = ?2
            ORDER BY stream_version DESC LIMIT 1
        ), 0),
        'thread.visit-recorded', ?3, ?4, NULL, ?4, 'server', ?5, ?6, 2
    ) RETURNING sequence";

pub(super) const UPDATE_WATERMARK: &str = "
    UPDATE orchestration_v2_projection_threads
    SET payload_json = json_set(payload_json, '$.lastVisitedAt', ?2)
    WHERE thread_id = ?1";

pub(super) const UPDATE_OPEN: &str = "
    UPDATE orchestration_v2_projection_threads
    SET payload_json = json_set(payload_json, '$.lastVisitedAt', ?2, '$.opens', json(?3))
    WHERE thread_id = ?1";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    command_id: NonBlank,
    thread_id: NonBlank,
    visited_at: String,
    #[serde(default)]
    opened: bool,
    #[serde(default)]
    search_query: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenStats {
    opened_at: f64,
    score: f64,
    picks: Vec<Pick>,
}

#[derive(Deserialize, Serialize)]
struct Pick {
    query: String,
    at: f64,
}

impl ThreadCommands {
    pub(super) fn visit(&self, payload: Value) -> Result<CommittedThread, ThreadCommandError> {
        let request: Request = serde_json::from_value(payload)
            .map_err(|error| ThreadCommandError::InvalidRequest(error.to_string()))?;
        let command_id = request.command_id.0;
        let thread_id = request.thread_id.0;
        let visited_at = parse_time(&request.visited_at).ok_or_else(|| {
            ThreadCommandError::InvalidRequest("A valid visit timestamp is required.".into())
        })?;
        if request.search_query.encode_utf16().count() > 200 {
            return Err(ThreadCommandError::InvalidRequest(
                "Search query is too long.".into(),
            ));
        }
        let mut database = self.connect()?;
        let write = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = write
            .query_row(SELECT_RECEIPT, [&command_id], Receipt::read)
            .optional()?
        {
            return receipt.replay(&command_id, &thread_id);
        }
        let thread = write
            .query_row(SELECT_THREAD, [&thread_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .optional()?;
        let now = OffsetDateTime::now_utc();
        let occurred_at = timestamp(now);
        let prior_sequence =
            || write.query_row(LATEST_THREAD_SEQUENCE, [&thread_id], |row| row.get(0));
        let Some((provider_instance, None, previous_visit, previous_opens)) = thread else {
            let message = if thread.is_some() {
                format!("Thread {thread_id} is deleted.")
            } else {
                format!("Thread {thread_id} was not found.")
            };
            write.execute(
                INSERT_RECEIPT,
                params![
                    command_id,
                    thread_id,
                    COMMAND,
                    occurred_at,
                    prior_sequence()?,
                    "rejected",
                    message
                ],
            )?;
            write.commit()?;
            return Err(ThreadCommandError::Rejected(message));
        };
        let moves_forward = previous_visit
            .as_deref()
            .and_then(parse_time)
            .is_none_or(|previous| milliseconds(visited_at) > milliseconds(previous));
        let committed = moves_forward || request.opened;
        let sequence = if committed {
            let last_visited = if moves_forward {
                Some(timestamp(visited_at))
            } else {
                previous_visit
            };
            let mut event = json!({ "lastVisitedAt": last_visited });
            if request.opened {
                let opened = record_open(
                    previous_opens.as_deref(),
                    milliseconds(now),
                    &request.search_query,
                );
                event["opens"] = json!(opened);
                write.execute(
                    UPDATE_OPEN,
                    params![thread_id, last_visited, event["opens"].to_string()],
                )?;
            } else {
                write.execute(UPDATE_WATERMARK, params![thread_id, last_visited])?;
            }
            let sequence: u64 = write.query_row(
                APPEND_VISIT,
                params![
                    event_id(&thread_id, &command_id),
                    thread_id,
                    occurred_at,
                    command_id,
                    event.to_string(),
                    json!({ "providerInstanceId": provider_instance }).to_string(),
                ],
                |row| row.get(0),
            )?;
            write.execute(RECORD_PROJECTION_SEQUENCE, params![sequence, occurred_at])?;
            sequence
        } else {
            prior_sequence()?
        };
        write.execute(
            INSERT_RECEIPT,
            params![
                command_id,
                thread_id,
                COMMAND,
                occurred_at,
                sequence,
                "accepted",
                None::<String>
            ],
        )?;
        write.commit()?;
        Ok(CommittedThread {
            sequence,
            committed,
        })
    }
}

fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|time| time.to_offset(UtcOffset::UTC))
}

fn milliseconds(time: OffsetDateTime) -> f64 {
    time.unix_timestamp_nanos().div_euclid(1_000_000) as f64
}

fn record_open(previous: Option<&str>, at: f64, query: &str) -> OpenStats {
    let previous = previous
        .and_then(|row| serde_json::from_str::<OpenStats>(row).ok())
        .filter(|stats| {
            stats.score >= 0.0
                && stats.picks.len() <= 3
                && stats
                    .picks
                    .iter()
                    .all(|pick| pick.query.encode_utf16().count() <= 32)
        });
    let mut stats = previous.unwrap_or(OpenStats {
        opened_at: at - OPEN_DEDUPE_MS,
        score: 0.0,
        picks: vec![],
    });
    let pick = normalize_pick(query);
    stats
        .picks
        .retain(|old| at - old.at <= PICK_WINDOW_MS && (pick.is_empty() || old.query != pick));
    if !pick.is_empty() {
        stats.picks.insert(
            0,
            Pick {
                query: pick.to_owned(),
                at,
            },
        );
        stats.picks.truncate(3);
    }
    let elapsed = at - stats.opened_at;
    if elapsed >= OPEN_DEDUPE_MS {
        stats.score = stats.score * 2.0f64.powf(-elapsed / DECAY_HALF_LIFE_MS) + 1.0;
        stats.opened_at = at;
    }
    stats
}

fn normalize_pick(query: &str) -> String {
    let quotes = ['\'', '"', '`', '‘', '’', '“', '”'];
    let folded: String = trim(query)
        .trim_matches(quotes)
        .nfc()
        .map(|character| {
            if character.is_ascii() || character.len_utf16() != 1 {
                return character;
            }
            let mut options = character
                .nfkd()
                .filter(|character| !is_combining_mark(*character))
                .flat_map(char::to_lowercase);
            match (options.next(), options.next()) {
                (Some('ς'), None) => 'σ',
                (Some('‘' | '’'), None) => '\'',
                (Some(folded), None) if folded.len_utf16() == 1 => folded,
                _ => character,
            }
        })
        .collect();
    let collapsed = folded
        .to_lowercase()
        .split(|character: char| {
            let mut bytes = [0; 4];
            trim(character.encode_utf8(&mut bytes)).is_empty()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut units = 0;
    let end = collapsed
        .char_indices()
        .find_map(|(index, character)| {
            units += character.len_utf16();
            (units > 32).then_some(index)
        })
        .unwrap_or(collapsed.len());
    trim(&collapsed[..end]).to_owned()
}
