use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop};

pub(super) struct Unarchive {
    thread_id: String,
    operation: Uuid,
    sent_at: Instant,
}

impl Unarchive {
    fn settled(&self, result: Result<(), String>) -> Event {
        Event::ThreadUnarchiveSettled {
            thread_id: self.thread_id.clone(),
            operation: self.operation,
            result,
        }
    }
}

impl Connection {
    pub(crate) fn unarchive_thread(&self, thread_id: String, operation: Uuid) {
        let _ = self.commands.send(Command::UnarchiveThread {
            thread_id,
            operation,
        });
    }
}

impl Session {
    pub(super) async fn submit_unarchive(
        &mut self,
        thread_id: String,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let action = Unarchive {
            thread_id,
            operation,
            sent_at: Instant::now(),
        };
        if !self.archive_writes || !self.shell.contains_archived_thread(&action.thread_id) {
            events.send(action.settled(Err("This thread cannot be unarchived.".into())));
            return Ok(());
        }
        if self
            .pending_unarchive
            .values()
            .any(|other| other.thread_id == action.thread_id)
        {
            return Ok(());
        }
        let payload = json!({
            "type": "thread.unarchive",
            "commandId": action.operation.to_string(),
            "threadId": action.thread_id,
        });
        let request_id = self.allocate_id();
        self.pending_unarchive.insert(request_id.clone(), action);
        self.send_request(&request_id, "orchestration.dispatchCommand", payload)
            .await
    }

    pub(super) async fn finish_unarchive(
        &mut self,
        action: Unarchive,
        receipt: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let result = receipt.and_then(|receipt| {
            receipt["sequence"]
                .as_u64()
                .map(|_| ())
                .ok_or_else(|| "The environment did not confirm unarchiving the thread.".into())
        });
        let reload = result.is_ok();
        events.send(action.settled(result));
        if reload {
            self.load_archive().await?;
        }
        Ok(())
    }

    pub(super) fn expire_unarchive(&mut self, events: &EventSender<impl Fn()>) {
        self.pending_unarchive.retain(|_, action| {
            if action.sent_at.elapsed() < REQUEST_TIMEOUT {
                return true;
            }
            events.send(action.settled(Err(
                "The environment did not confirm unarchiving the thread in time.".into(),
            )));
            false
        });
    }

    pub(super) fn unconfirm_unarchive(&mut self, events: &EventSender<impl Fn()>) {
        for (_, action) in self.pending_unarchive.drain() {
            events.send(action.settled(Err(
                "The connection closed before unarchiving was confirmed.".into(),
            )));
        }
    }
}
