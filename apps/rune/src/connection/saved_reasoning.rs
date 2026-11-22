use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, ModelSelection, Session, SessionStop};

pub(super) struct Edit {
    thread_id: String,
    selection: ModelSelection,
    operation: Uuid,
}

pub(super) struct Pending {
    thread_id: String,
    operation: Uuid,
    pub(super) started: Instant,
    sequence: Option<u64>,
}

impl Pending {
    pub(super) fn finish(&self, result: Result<(), String>) -> Event {
        Event::SavedReasoningSettled {
            thread_id: self.thread_id.clone(),
            operation: self.operation,
            result,
        }
    }
}

impl Connection {
    pub(crate) fn set_saved_codex_model(
        &self,
        thread_id: String,
        selection: ModelSelection,
    ) -> Option<Uuid> {
        let operation = Uuid::new_v4();
        self.commands
            .send(Command::SetSavedReasoning(Edit {
                thread_id,
                selection,
                operation,
            }))
            .ok()?;
        Some(operation)
    }
}

impl Session {
    pub(super) fn saved_reasoning_receipt(
        &mut self,
        id: &str,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        let receipt = result.and_then(|value| {
            value["sequence"].as_u64().ok_or_else(|| {
                "The environment returned an invalid model selection receipt.".into()
            })
        });
        match receipt {
            Ok(sequence) => {
                if let Some(pending) = self.pending_saved_reasoning.get_mut(id) {
                    pending.sequence = Some(sequence);
                }
                self.finish_projected_saved_reasoning(events);
            }
            Err(message) => {
                if let Some(pending) = self.pending_saved_reasoning.remove(id) {
                    events.send(pending.finish(Err(message)));
                }
            }
        }
    }

    pub(super) fn finish_projected_saved_reasoning(&mut self, events: &EventSender<impl Fn()>) {
        let projected = self.shell.sequence();
        self.pending_saved_reasoning.retain(|_, pending| {
            if pending
                .sequence
                .is_some_and(|sequence| sequence <= projected)
            {
                events.send(pending.finish(Ok(())));
                false
            } else {
                true
            }
        });
    }

    pub(super) async fn set_saved_reasoning(
        &mut self,
        edit: Edit,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let pending = Pending {
            thread_id: edit.thread_id,
            operation: edit.operation,
            started: Instant::now(),
            sequence: None,
        };
        let available = self.thread.as_ref().is_some_and(|thread| {
            thread.thread_id == pending.thread_id && thread.reported_codex_reasoning
        });
        if !available {
            events.send(pending.finish(Err(
                "This thread cannot change its reasoning effort right now.".into(),
            )));
            return Ok(());
        }
        let id = self.allocate_id();
        let payload = json!({
            "type": "thread.model-selection.set",
            "commandId": pending.operation.to_string(),
            "threadId": pending.thread_id,
            "modelSelection": edit.selection,
        });
        self.pending_saved_reasoning.insert(id.clone(), pending);
        self.send_request(&id, "orchestration.dispatchCommand", payload)
            .await
    }
}
