use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop};

pub(crate) enum Request {
    Inspect { thread_id: String, path: String },
    Remove { cwd: String, path: String },
}

pub(crate) enum Reply {
    Exclusive(bool),
    Removed,
}

pub(super) struct Pending {
    operation: Uuid,
    deleted_threads: Option<Vec<String>>,
    sent_at: Instant,
}

impl Pending {
    pub(super) fn finish(self, result: Result<Value, String>, events: &EventSender<impl Fn()>) {
        let result = result.and_then(|value| {
            if let Some(removed) = self.deleted_threads {
                serde_json::from_value::<Vec<String>>(value["threadIds"].clone())
                    .map(|occupants| {
                        Reply::Exclusive(occupants.iter().all(|id| removed.contains(id)))
                    })
                    .map_err(|_| "The workspace did not return a worktree owner list.".to_owned())
            } else {
                Ok(Reply::Removed)
            }
        });
        events.send(Event::WorktreeRemovalSettled {
            operation: self.operation,
            result,
        });
    }
}

impl Connection {
    pub(crate) fn worktree_removal(&self, operation: Uuid, request: Request) {
        let _ = self
            .commands
            .send(Command::WorktreeRemoval { operation, request });
    }
}

impl Session {
    pub(super) async fn submit_worktree_removal(
        &mut self,
        operation: Uuid,
        action: Request,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let (deleted_threads, tag, payload) = match action {
            Request::Inspect { thread_id, path } => {
                let mut removed = self.shell.side_chats_of(&thread_id);
                removed.push(thread_id);
                (
                    Some(removed),
                    "orchestration.getWorktreeThreadIds",
                    json!({"worktreePath": path}),
                )
            }
            Request::Remove { cwd, path } => (
                None,
                "vcs.removeWorktree",
                json!({"cwd": cwd, "path": path, "force": true}),
            ),
        };
        let request = Pending {
            operation,
            deleted_threads,
            sent_at: Instant::now(),
        };
        if !self.source_control_write {
            request.finish(
                Err("This connection cannot remove worktrees.".into()),
                events,
            );
            return Ok(());
        }
        let id = self.allocate_id();
        self.pending_worktree_removals.insert(id.clone(), request);
        self.send_request(&id, tag, payload).await
    }

    pub(super) fn expire_worktree_removals(&mut self, events: &EventSender<impl Fn()>) {
        let expired = self
            .pending_worktree_removals
            .extract_if(|_, action| action.sent_at.elapsed() >= REQUEST_TIMEOUT);
        for (_, action) in expired {
            action.finish(
                Err("The worktree request was not confirmed in time.".into()),
                events,
            );
        }
    }

    pub(super) fn unconfirm_worktree_removals(&mut self, events: &EventSender<impl Fn()>) {
        for (_, action) in self.pending_worktree_removals.drain() {
            action.finish(
                Err("The connection closed before the worktree action was confirmed.".into()),
                events,
            );
        }
    }
}
