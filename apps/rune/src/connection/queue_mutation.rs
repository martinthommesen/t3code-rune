use serde_json::{Value, json};
use uuid::Uuid;

use super::QueuedMessage;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QueueMutation {
    Edit { run_id: String, text: String },
    Remove { run_id: String },
    SendNow { run_id: String },
}

impl QueueMutation {
    pub(crate) fn available(&self, queued: &QueuedMessage) -> bool {
        queued.worktree_queue
            && match self {
                Self::Edit { run_id, .. } => queued.run_id == *run_id && queued.text_editable,
                Self::Remove { run_id } => queued.run_id == *run_id,
                Self::SendNow { run_id } => queued.run_id == *run_id && queued.can_send_now,
            }
    }

    pub(super) fn payload(&self, thread_id: &str, operation: Uuid) -> Value {
        let command_id = operation.to_string();
        match self {
            Self::Edit { run_id, text } => json!({
                "type": "queued-run.edit", "commandId":command_id,
                "threadId":thread_id, "runId":run_id, "text":text,
            }),
            Self::Remove { run_id } => json!({
                "type": "queued-run.cancel", "commandId":command_id,
                "threadId":thread_id, "runId":run_id,
            }),
            Self::SendNow { run_id } => json!({
                "type": "queued-run.start", "commandId":command_id,
                "threadId":thread_id, "runId":run_id,
            }),
        }
    }
}
