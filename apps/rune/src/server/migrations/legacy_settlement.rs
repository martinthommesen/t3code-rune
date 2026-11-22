use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{OptionalExtension, Row, Transaction, params};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const PAGE_SIZE: usize = 500;

const SWEEPS: &str = "
WITH
legacy_settlements AS (
  SELECT stream_id AS thread_id, occurred_at, unixepoch(occurred_at, 'subsec') AS occurred_seconds
  FROM orchestration_events
  WHERE aggregate_kind = 'thread' AND event_type = 'thread.settled' AND actor_kind = 'client'
    AND application_event_version = 1 AND json_type(payload_json, '$.settledAt') = 'text'
    AND json_extract(payload_json, '$.settledAt') = occurred_at
    AND json_extract(metadata_json, '$.origin.appVersion') IN ('0.0.35', '0.0.36', '0.0.37', '0.0.38')
    AND julianday(occurred_at) < julianday('2026-09-05T00:00:00.000Z')
),
ordered AS (
  SELECT thread_id, occurred_at, occurred_seconds,
    occurred_seconds - LAG(occurred_seconds) OVER (ORDER BY occurred_seconds, thread_id) AS gap_seconds
  FROM legacy_settlements
),
clustered AS (
  SELECT thread_id, occurred_at, occurred_seconds,
    SUM(CASE WHEN gap_seconds IS NULL OR gap_seconds > 30 THEN 1 ELSE 0 END)
      OVER (ORDER BY occurred_seconds, thread_id) AS cluster_id
  FROM ordered
),
burst_clusters AS (
  SELECT cluster_id, COUNT(DISTINCT thread_id) AS distinct_thread_count,
    MAX(occurred_seconds) - MIN(occurred_seconds) AS span_seconds
  FROM clustered GROUP BY cluster_id
),
automatic_settlements AS (
  SELECT clustered.thread_id, clustered.occurred_at
  FROM clustered JOIN burst_clusters ON burst_clusters.cluster_id = clustered.cluster_id
  WHERE burst_clusters.distinct_thread_count >= 10 AND burst_clusters.span_seconds <= 120
)
SELECT DISTINCT thread_id, occurred_at FROM automatic_settlements ORDER BY thread_id, occurred_at";

const LEGACY_EVENTS: &str = "
SELECT sequence, event_type, occurred_at,
  json_extract(payload_json, '$.turnId'), json_extract(payload_json, '$.createdAt'),
  json_extract(payload_json, '$.updatedAt'), json_extract(payload_json, '$.completedAt'),
  json_extract(payload_json, '$.role'), json_extract(payload_json, '$.streaming'),
  json_extract(payload_json, '$.session.status'), json_extract(payload_json, '$.session.activeTurnId'),
  json_extract(payload_json, '$.session.updatedAt'), json_extract(payload_json, '$.checkpointTurnCount'),
  json_extract(payload_json, '$.turnCount')
FROM orchestration_events INDEXED BY idx_orch_events_stream_sequence
WHERE aggregate_kind = 'thread' AND stream_id = ?1 AND application_event_version = 1 AND sequence > ?2
  AND event_type IN ('thread.created', 'thread.turn-start-requested', 'thread.session-set',
    'thread.message-sent', 'thread.turn-interrupt-requested', 'thread.turn-diff-completed', 'thread.reverted')
ORDER BY sequence LIMIT ?3";

struct Sweep {
    occurred_at: String,
    cutoff: i128,
    activity: Option<(String, i128)>,
}

pub(super) fn migrate_66(database: &Transaction<'_>) -> rusqlite::Result<()> {
    let mut by_thread = BTreeMap::<String, Vec<Sweep>>::new();
    for row in database.prepare(SWEEPS)?.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (thread, occurred_at) = row?;
        let cutoff = datetime(database, &occurred_at)?.ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                "Invalid settlement time".into(),
            )
        })?;
        by_thread.entry(thread).or_default().push(Sweep {
            occurred_at,
            cutoff,
            activity: None,
        });
    }
    for (thread, mut sweeps) in by_thread {
        for index in 0..sweeps.len() {
            let value: Option<String> = database
                .query_row(
                    "SELECT created_at FROM projection_thread_messages
                 WHERE thread_id = ?1 AND role = 'user' AND julianday(created_at) <= julianday(?2)
                 ORDER BY julianday(created_at) DESC LIMIT 1",
                    params![thread, sweeps[index].occurred_at],
                    |row| row.get(0),
                )
                .optional()?;
            record_activity(database, &mut sweeps, value.as_deref())?;
        }
        let mut replay = Replay::default();
        let mut after = 0_i64;
        let mut select = database.prepare(LEGACY_EVENTS)?;
        loop {
            let rows = select.query_map(params![thread, after, PAGE_SIZE], Event::read)?;
            let mut count = 0;
            for row in rows {
                let event = row?;
                replay.apply(database, &mut sweeps, &event)?;
                after = event.sequence;
                count += 1;
            }
            if count < PAGE_SIZE {
                break;
            }
        }
        for sweep in sweeps {
            repair(database, &thread, sweep)?;
        }
    }
    Ok(())
}

fn datetime(database: &Transaction<'_>, value: &str) -> rusqlite::Result<Option<i128>> {
    if let Ok(date) = OffsetDateTime::parse(value, &Rfc3339) {
        return Ok(Some(date.unix_timestamp_nanos().div_euclid(1_000_000)));
    }
    let timestamp: Option<i64> = database.query_row(
        "SELECT CAST(ROUND(unixepoch(?1, 'subsec') * 1000) AS INTEGER)",
        [value],
        |row| row.get(0),
    )?;
    Ok(timestamp.map(i128::from))
}

fn record_activity(
    database: &Transaction<'_>,
    sweeps: &mut [Sweep],
    value: Option<&str>,
) -> rusqlite::Result<()> {
    if let Some(value) = value
        && let Some(timestamp) = datetime(database, value)?
    {
        for sweep in sweeps {
            if timestamp <= sweep.cutoff
                && sweep
                    .activity
                    .as_ref()
                    .is_none_or(|(_, at)| timestamp > *at)
            {
                sweep.activity = Some((value.to_owned(), timestamp));
            }
        }
    }
    Ok(())
}

struct Event {
    sequence: i64,
    kind: String,
    occurred_at: String,
    turn: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    completed_at: Option<String>,
    role: Option<String>,
    streaming: Option<i64>,
    status: Option<String>,
    session_turn: Option<String>,
    session_at: Option<String>,
    checkpoint: Option<i64>,
    turn_count: Option<i64>,
}

impl Event {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            sequence: row.get(0)?,
            kind: row.get(1)?,
            occurred_at: row.get(2)?,
            turn: row.get(3)?,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
            completed_at: row.get(6)?,
            role: row.get(7)?,
            streaming: row.get(8)?,
            status: row.get(9)?,
            session_turn: row.get(10)?,
            session_at: row.get(11)?,
            checkpoint: row.get(12)?,
            turn_count: row.get(13)?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Completed,
    Interrupted,
    Error,
}

struct Turn {
    status: Status,
    checkpoint: Option<i64>,
}

#[derive(Default)]
struct Replay {
    turns: HashMap<String, Turn>,
    running: HashSet<String>,
    active: Option<String>,
}

impl Replay {
    fn set_turn(&mut self, id: &str, status: Status, checkpoint: Option<i64>) {
        self.turns
            .insert(id.to_owned(), Turn { status, checkpoint });
        if status == Status::Running {
            self.running.insert(id.to_owned());
        } else {
            self.running.remove(id);
        }
    }

    fn apply(
        &mut self,
        database: &Transaction<'_>,
        sweeps: &mut [Sweep],
        event: &Event,
    ) -> rusqlite::Result<()> {
        let mut record = |value| record_activity(database, sweeps, value);
        match event.kind.as_str() {
            "thread.created" => *self = Self::default(),
            "thread.turn-start-requested" => record(event.created_at.as_deref())?,
            "thread.session-set" => self.session(database, sweeps, event)?,
            "thread.message-sent" if event.role.as_deref() == Some("assistant") => {
                let Some(id) = event.turn.as_deref() else {
                    return Ok(());
                };
                let existing = self.turns.get(id);
                record(event.created_at.as_deref())?;
                if event.streaming == Some(0) && self.active.as_deref() != Some(id) {
                    if existing.is_none_or(|turn| turn.status == Status::Running) {
                        record(event.updated_at.as_deref())?;
                        self.set_turn(
                            id,
                            Status::Completed,
                            existing.and_then(|turn| turn.checkpoint),
                        );
                    }
                    self.running.remove(id);
                } else if existing.is_none() {
                    self.set_turn(id, Status::Running, None);
                }
            }
            "thread.turn-interrupt-requested" => {
                if let Some(id) = event.turn.as_deref() {
                    record(event.created_at.as_deref())?;
                    self.set_turn(
                        id,
                        Status::Interrupted,
                        self.turns.get(id).and_then(|turn| turn.checkpoint),
                    );
                }
            }
            "thread.turn-diff-completed" => {
                if let Some(id) = event.turn.as_deref() {
                    record(event.completed_at.as_deref())?;
                    let previous = self.turns.get(id).map(|turn| turn.status);
                    let status = if self.active.as_deref() == Some(id) {
                        previous.unwrap_or(Status::Running)
                    } else if previous == Some(Status::Interrupted) {
                        Status::Interrupted
                    } else {
                        Status::Completed
                    };
                    self.set_turn(id, status, event.checkpoint);
                }
            }
            "thread.reverted" => {
                let cutoff = event.turn_count.unwrap_or_default();
                self.turns.retain(|id, turn| {
                    let keep = turn
                        .checkpoint
                        .is_some_and(|checkpoint| checkpoint <= cutoff);
                    if !keep {
                        self.running.remove(id);
                    }
                    keep
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn session(
        &mut self,
        database: &Transaction<'_>,
        sweeps: &mut [Sweep],
        event: &Event,
    ) -> rusqlite::Result<()> {
        let state = event.status.as_deref();
        let active = event
            .session_turn
            .as_deref()
            .filter(|_| state == Some("running"));
        self.active = active.map(str::to_owned);
        let ended_at = match event.session_at.as_deref() {
            Some(at) if datetime(database, at)?.is_some() => at,
            _ => &event.occurred_at,
        };
        if let Some(active) = active {
            let mut closed = false;
            self.running.retain(|id| {
                if id == active {
                    return true;
                }
                self.turns.get_mut(id).expect("running turn").status = Status::Completed;
                closed = true;
                false
            });
            if closed {
                record_activity(database, sweeps, Some(ended_at))?;
            }
            match self.turns.get_mut(active) {
                None => {
                    self.set_turn(active, Status::Running, None);
                    record_activity(database, sweeps, Some(&event.occurred_at))?;
                }
                Some(turn) if turn.status == Status::Interrupted => {
                    turn.status = Status::Running;
                    self.running.insert(active.to_owned());
                }
                _ => {}
            }
        } else if let Some(status) = match state {
            Some("idle" | "ready") => Some(Status::Completed),
            Some("error") => Some(Status::Error),
            Some("stopped" | "interrupted") => Some(Status::Interrupted),
            _ => None,
        } && !self.running.is_empty()
        {
            for id in self.running.drain() {
                self.turns.get_mut(&id).expect("running turn").status = status;
            }
            record_activity(database, sweeps, Some(ended_at))?;
        }
        Ok(())
    }
}

fn repair(database: &Transaction<'_>, thread: &str, sweep: Sweep) -> rusqlite::Result<()> {
    let activity = sweep.activity.map(|(at, _)| at);
    let arguments = params![thread, sweep.occurred_at, activity];
    database.execute(
        "UPDATE projection_threads SET settled_at = COALESCE(?3, created_at)
         WHERE thread_id = ?1 AND settled_override = 'settled' AND settled_at = ?2",
        arguments,
    )?;
    database.execute(
        "UPDATE orchestration_v2_projection_threads
         SET payload_json = json_set(payload_json, '$.settledAt', COALESCE(?3, json_extract(payload_json, '$.createdAt')))
         WHERE thread_id = ?1 AND json_extract(payload_json, '$.settledOverride') = 'settled'
         AND json_extract(payload_json, '$.settledAt') = ?2",
        arguments,
    )?;
    database.execute(
        "UPDATE orchestration_events SET payload_json = json_set(payload_json, '$.settledAt',
           COALESCE(?3, json_extract(payload_json, '$.createdAt'), json_extract(payload_json, '$.settledAt')))
         WHERE aggregate_kind = 'thread' AND stream_id = ?1 AND application_event_version = 2
         AND json_extract(payload_json, '$.settledOverride') = 'settled'
         AND json_extract(payload_json, '$.settledAt') = ?2",
        arguments,
    )?;
    Ok(())
}
