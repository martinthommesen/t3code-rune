use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QueueMutation {
    Edit { run_id: String, text: String },
    Remove { run_id: String },
}

impl QueueMutation {
    pub(crate) fn run_id(&self) -> &str {
        match self {
            Self::Edit { run_id, .. } | Self::Remove { run_id } => run_id,
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
        }
    }
}
