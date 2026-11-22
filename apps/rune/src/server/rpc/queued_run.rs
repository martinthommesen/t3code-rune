use serde::Deserialize;
use serde_json::{Value, json};

use super::{LocalOperations, ProjectionChange, RequestId, RpcError, failure, success};
use crate::server::turn_commands::queued_run::{MutationKind, MutationOutcome, QueuedRunMutation};

pub(super) fn dispatch(
    local: &LocalOperations,
    id: RequestId,
    payload: &Value,
    command_id: &str,
    command_type: &str,
) -> Value {
    let ordinary_failure = |id, message: &str| {
        failure(
            id,
            RpcError::OrchestrationV2DispatchCommandError {
                command_id,
                command_type,
                message,
            },
        )
    };
    let command = match QueuedRunMutation::deserialize(payload) {
        Ok(command) => command,
        Err(error) => return ordinary_failure(id, &error.to_string()),
    };
    match local.turns.mutate_queued_run(&command) {
        Ok(MutationOutcome::Accepted {
            sequence,
            committed,
        }) => {
            if committed {
                let _ = local
                    .projection_changes
                    .send(ProjectionChange::Thread(command.thread_id));
            }
            success(id, json!({"sequence": sequence}))
        }
        Ok(MutationOutcome::Rejected(detail)) => {
            let (name, message) = match command.kind {
                MutationKind::Edit => (
                    "OrchestratorCommandRejectedError",
                    format!(
                        "Orchestration command {command_type} ({command_id}) was rejected before commit."
                    ),
                ),
                MutationKind::Cancel => (
                    "OrchestratorDispatchError",
                    format!(
                        "Failed to dispatch orchestration command {command_type} ({command_id})."
                    ),
                ),
            };
            dispatch_failure(
                id,
                command_id,
                command_type,
                &detail,
                json!({"name":name, "message":message, "cause":detail}),
            )
        }
        Ok(MutationOutcome::PreviouslyRejected(detail)) => {
            let message = format!("Command {command_id} was previously rejected: {detail}");
            dispatch_failure(
                id,
                command_id,
                command_type,
                &message,
                json!({"name":"OrchestratorCommandPreviouslyRejectedError", "message":message}),
            )
        }
        Err(error) => ordinary_failure(id, &error.to_string()),
    }
}

fn dispatch_failure(
    id: RequestId,
    command_id: &str,
    command_type: &str,
    detail: &str,
    cause: Value,
) -> Value {
    failure(
        id,
        json!({
            "_tag":"OrchestrationV2DispatchCommandError",
            "commandId":command_id,
            "commandType":command_type,
            "message":detail,
            "detail":detail,
            "cause":cause,
        }),
    )
}
