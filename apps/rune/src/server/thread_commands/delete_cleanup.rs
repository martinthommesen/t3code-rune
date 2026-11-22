use std::io;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use time::OffsetDateTime;
use tokio::sync::{Notify, broadcast};
use uuid::Uuid;

use crate::server::turn_commands::timestamp;
use crate::server::{ProjectionChange, browser_preview::Preview, terminal::TerminalManager};

use super::{ThreadCommands, now_timestamp};

pub(super) const DELETION_EFFECTS: [&str; 3] = [
    "scheduled-tasks.cleanup",
    "terminal.cleanup",
    "preview.cleanup",
];
const MAX_ATTEMPTS: i64 = 5;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(in crate::server) async fn run(
    commands: ThreadCommands,
    terminals: Arc<TerminalManager>,
    preview: Option<Arc<Preview>>,
    mut notifications: broadcast::Receiver<ProjectionChange>,
    worktree_cleanup: Arc<Notify>,
) {
    let recovery = commands.clone();
    if !matches!(
        tokio::task::spawn_blocking(move || recovery.recover_deleted_thread_cleanup()).await,
        Ok(Ok(()))
    ) {
        eprintln!("Thread cleanup could not recover its pending work.");
        return;
    }
    let worker = Arc::new(format!("rune-archive-cleanup:{}", Uuid::new_v4()));
    loop {
        match commands
            .cleanup_deleted_thread(
                Arc::clone(&terminals),
                preview.as_deref(),
                Arc::clone(&worker),
            )
            .await
        {
            Ok(Ok(true)) => {
                worktree_cleanup.notify_one();
                continue;
            }
            Ok(Ok(false)) => {}
            Ok(Err(error)) => eprintln!("Could not settle thread cleanup: {error}"),
            Err(error) => eprintln!("Thread cleanup task stopped: {error}"),
        }
        tokio::select! {
            notification = notifications.recv() => {
                if matches!(notification, Err(broadcast::error::RecvError::Closed)) {
                    break;
                }
            }
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

struct Effect {
    id: String,
    thread_id: String,
    kind: String,
    attempt_count: i64,
}

impl ThreadCommands {
    fn recover_deleted_thread_cleanup(&self) -> rusqlite::Result<()> {
        self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status='pending', lease_owner=NULL, available_at=?1, updated_at=?1
             WHERE status='running' AND lease_owner LIKE 'rune-archive-cleanup:%'",
            [now_timestamp()],
        )?;
        Ok(())
    }

    async fn cleanup_deleted_thread(
        &self,
        terminals: Arc<TerminalManager>,
        preview: Option<&Preview>,
        worker: Arc<String>,
    ) -> Result<rusqlite::Result<bool>, tokio::task::JoinError> {
        let commands = self.clone();
        let claim_owner = Arc::clone(&worker);
        let effect = match tokio::task::spawn_blocking(move || {
            commands.claim_deleted_thread_cleanup(&claim_owner)
        })
        .await?
        {
            Ok(Some(effect)) => effect,
            Ok(None) => return Ok(Ok(false)),
            Err(error) => return Ok(Err(error)),
        };
        let outcome = match effect.kind.as_str() {
            "scheduled-tasks.cleanup" | "terminal.cleanup" => {
                let schedules = effect.kind == "scheduled-tasks.cleanup";
                let commands = self.clone();
                let thread_id = effect.thread_id.clone();
                tokio::task::spawn_blocking(move || {
                    if schedules {
                        commands
                            .cleanup_deleted_thread_schedules(&thread_id)
                            .map_err(io::Error::other)
                    } else {
                        terminals.cleanup_deleted_thread(&thread_id)
                    }
                })
                .await?
            }
            "preview.cleanup" => match preview {
                Some(preview) => preview.close(&effect.thread_id, None).await,
                None => Ok(()),
            },
            _ => return Ok(Err(rusqlite::Error::InvalidQuery)),
        };
        let commands = self.clone();
        tokio::task::spawn_blocking(move || {
            commands.settle_deleted_thread_cleanup(effect, &worker, outcome)
        })
        .await
    }

    fn settle_deleted_thread_cleanup(
        &self,
        effect: Effect,
        worker: &str,
        outcome: io::Result<()>,
    ) -> rusqlite::Result<bool> {
        let now = OffsetDateTime::now_utc();
        let updated = timestamp(now);
        let (status, available, completed, error) = match outcome {
            Ok(()) => ("succeeded", None, Some(&updated), None),
            Err(error) if effect.attempt_count >= MAX_ATTEMPTS => {
                ("failed", None, Some(&updated), Some(error.to_string()))
            }
            Err(error) => {
                let delay = time::Duration::milliseconds(100 << (effect.attempt_count - 1));
                (
                    "pending",
                    Some(timestamp(now + delay)),
                    None,
                    Some(error.to_string()),
                )
            }
        };
        self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox SET status=?3, lease_owner=NULL,
             available_at=COALESCE(?4,available_at), completed_at=?5, last_error=?6, updated_at=?7
             WHERE effect_id=?1 AND lease_owner=?2 AND status='running'",
            params![
                effect.id, worker, status, available, completed, error, updated
            ],
        )?;
        Ok(true)
    }

    fn claim_deleted_thread_cleanup(&self, worker: &str) -> rusqlite::Result<Option<Effect>> {
        let mut database = self.connect()?;
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_timestamp();
        let effect = transaction
            .query_row(SELECT_CLEANUP, [&now], |row| {
                Ok(Effect {
                    id: row.get(0)?,
                    thread_id: row.get(1)?,
                    kind: row.get(2)?,
                    attempt_count: row.get::<_, i64>(3)? + 1,
                })
            })
            .optional()?;
        if let Some(effect) = &effect {
            transaction.execute(
                "UPDATE orchestration_v2_effect_outbox
                 SET status='running', attempt_count=?3, lease_owner=?2, updated_at=?4
                 WHERE effect_id=?1 AND status='pending'",
                params![effect.id, worker, effect.attempt_count, now],
            )?;
        }
        transaction.commit()?;
        Ok(effect)
    }

    fn cleanup_deleted_thread_schedules(&self, thread_id: &str) -> rusqlite::Result<()> {
        let mut database = self.connect()?;
        database.pragma_update(None, "foreign_keys", true)?;
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM scheduled_task_webhook_deliveries
             WHERE task_id IN (SELECT task_id FROM scheduled_tasks WHERE thread_id=?1)",
            [thread_id],
        )?;
        transaction.execute(
            "DELETE FROM scheduled_tasks WHERE thread_id=?1",
            [thread_id],
        )?;
        transaction.commit()
    }
}

const SELECT_CLEANUP: &str = "
    SELECT effect.effect_id, effect.thread_id, effect.effect_type, effect.attempt_count
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id=effect.thread_id
    WHERE effect.status='pending' AND effect.available_at<=?1
      AND effect.effect_type IN ('scheduled-tasks.cleanup','terminal.cleanup','preview.cleanup')
      AND effect.effect_id='effect:' || effect.command_id || ':' || effect.effect_type
      AND json_extract(effect.payload_json,'$.type')=effect.effect_type
      AND ((thread.deleted_at IS NOT NULL AND EXISTS (
        SELECT 1 FROM orchestration_events AS tombstone
        JOIN orchestration_command_receipts AS receipt ON receipt.command_id=tombstone.command_id
        WHERE tombstone.aggregate_kind='thread' AND tombstone.application_event_version=2
          AND tombstone.stream_id=effect.thread_id AND tombstone.event_type='thread.deleted'
          AND tombstone.sequence<=receipt.result_sequence
          AND receipt.aggregate_kind='thread' AND receipt.command_type='thread.delete' AND receipt.status='accepted'
          AND ((receipt.aggregate_id=effect.thread_id AND receipt.command_id=effect.command_id)
            OR (receipt.aggregate_id=json_extract(thread.payload_json,'$.lineage.parentThreadId')
                AND json_extract(thread.payload_json,'$.lineage.sideChat')=1
                AND effect.command_id=receipt.command_id || ':side-chat:' || effect.thread_id))
      )) OR (
        thread.deleted_at IS NULL AND effect.effect_type='terminal.cleanup'
        AND EXISTS (
            SELECT 1 FROM orchestration_events AS archived
            JOIN orchestration_command_receipts AS receipt ON receipt.command_id=archived.command_id
            WHERE archived.aggregate_kind='thread' AND archived.application_event_version=2
              AND archived.stream_id=effect.thread_id AND archived.event_type='thread.archived'
              AND archived.sequence<=receipt.result_sequence
              AND receipt.aggregate_kind='thread' AND receipt.command_type='thread.archive'
              AND receipt.status='accepted'
              AND ((receipt.aggregate_id=effect.thread_id AND receipt.command_id=effect.command_id)
                OR (receipt.aggregate_id=json_extract(archived.payload_json,'$.lineage.parentThreadId')
                    AND json_extract(archived.payload_json,'$.lineage.sideChat')=1
                    AND effect.command_id=receipt.command_id || ':side-chat:' || effect.thread_id))
        )
      ))
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_effect_outbox AS earlier
        WHERE earlier.thread_id=effect.thread_id AND earlier.command_id=effect.command_id
          AND earlier.rowid < effect.rowid AND earlier.status IN ('pending','running')
      )
    ORDER BY effect.rowid LIMIT 1
";
