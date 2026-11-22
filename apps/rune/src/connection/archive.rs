use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveAction {
    Archive,
    Unarchive,
    UndoArchive,
    Delete,
    DeleteActive,
}

impl ArchiveAction {
    fn command(self) -> &'static str {
        match self {
            Self::Archive => "thread.archive",
            Self::Unarchive | Self::UndoArchive => "thread.unarchive",
            Self::Delete | Self::DeleteActive => "thread.delete",
        }
    }

    pub(crate) fn activity(self) -> &'static str {
        match self {
            Self::Archive => "archiving",
            Self::Unarchive | Self::UndoArchive => "unarchiving",
            Self::Delete | Self::DeleteActive => "deleting",
        }
    }
}

pub(super) struct PendingAction {
    action: ArchiveAction,
    thread_id: String,
    operation: Uuid,
    sent_at: Instant,
}

impl PendingAction {
    fn settled(&self, result: Result<(), String>) -> Event {
        Event::ArchiveActionSettled {
            action: self.action,
            thread_id: self.thread_id.clone(),
            operation: self.operation,
            result,
        }
    }
}

impl Connection {
    pub(crate) fn archive_action(&self, action: ArchiveAction, thread_id: String, operation: Uuid) {
        let _ = self.commands.send(Command::ArchiveAction {
            action,
            thread_id,
            operation,
        });
    }
}

impl Session {
    pub(super) async fn submit_archive_action(
        &mut self,
        action: ArchiveAction,
        thread_id: String,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let request = PendingAction {
            action,
            thread_id,
            operation,
            sent_at: Instant::now(),
        };
        let allowed = match action {
            ArchiveAction::Archive => {
                self.archive_active_writes
                    && self.shell.contains_unarchived_thread(&request.thread_id)
            }
            ArchiveAction::Unarchive => {
                self.archive_writes && self.shell.contains_archived_thread(&request.thread_id)
            }
            ArchiveAction::UndoArchive => self.archive_writes,
            ArchiveAction::Delete => {
                self.archive_deletes && self.shell.contains_archived_thread(&request.thread_id)
            }
            ArchiveAction::DeleteActive => {
                self.archive_deletes && self.shell.contains_unarchived_thread(&request.thread_id)
            }
        };
        if !allowed {
            events.send(request.settled(Err("This archive action is unavailable.".into())));
            return Ok(());
        }
        if self
            .pending_archive_actions
            .values()
            .any(|other| other.thread_id == request.thread_id)
        {
            return Ok(());
        }
        let payload = json!({
            "type": action.command(),
            "commandId": request.operation.to_string(),
            "threadId": request.thread_id,
        });
        let request_id = self.allocate_id();
        self.pending_archive_actions
            .insert(request_id.clone(), request);
        self.send_request(&request_id, "orchestration.dispatchCommand", payload)
            .await
    }

    pub(super) async fn finish_archive_action(
        &mut self,
        action: PendingAction,
        receipt: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let result = receipt.and_then(|receipt| {
            receipt["sequence"].as_u64().map(|_| ()).ok_or_else(|| {
                format!(
                    "The environment did not confirm {} the thread.",
                    action.action.activity()
                )
            })
        });
        let reload = result.is_ok();
        events.send(action.settled(result));
        if reload {
            self.load_archive().await?;
        }
        Ok(())
    }

    pub(super) fn expire_archive_actions(&mut self, events: &EventSender<impl Fn()>) {
        self.pending_archive_actions.retain(|_, action| {
            if action.sent_at.elapsed() < REQUEST_TIMEOUT {
                return true;
            }
            events.send(action.settled(Err(format!(
                "The environment did not confirm {} the thread in time.",
                action.action.activity()
            ))));
            false
        });
    }

    pub(super) fn unconfirm_archive_actions(&mut self, events: &EventSender<impl Fn()>) {
        for (_, action) in self.pending_archive_actions.drain() {
            events.send(action.settled(Err(format!(
                "The connection closed before {} the thread was confirmed.",
                action.action.activity()
            ))));
        }
    }
}
