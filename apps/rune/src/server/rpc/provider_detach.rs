use serde_json::{Value, json};

use super::{LocalOperations, ProjectionChange, RequestId, RpcError, failure, success};
use crate::server::turn_commands::TurnCommandError;
use crate::server::turn_commands::provider_detach::{COMMAND, Detach};

pub(super) fn dispatch(
    local: &LocalOperations,
    id: RequestId,
    payload: Value,
    command_id: &str,
) -> Value {
    let result = serde_json::from_value::<Detach>(payload)
        .map_err(TurnCommandError::from)
        .and_then(|command| {
            let (sequence, committed) = local.turns.detach_provider_session(&command)?;
            if committed {
                let _ = local
                    .projection_changes
                    .send(ProjectionChange::Thread(command.thread_id));
                local.turn_queue.wake();
            }
            Ok(sequence)
        });
    match result {
        Ok(sequence) => success(id, json!({"sequence": sequence})),
        Err(error @ TurnCommandError::ProviderSessionNotBound { .. }) => {
            let detail = error.to_string();
            failure(
                id,
                json!({
                    "_tag": "OrchestrationV2DispatchCommandError",
                    "commandId": command_id,
                    "commandType": COMMAND,
                    "message": detail,
                    "detail": detail,
                    "cause": {
                        "name": "OrchestratorDispatchError",
                        "message": format!("Failed to dispatch orchestration command {COMMAND} ({command_id})."),
                        "cause": detail,
                    },
                }),
            )
        }
        Err(error) => failure(
            id,
            RpcError::OrchestrationV2DispatchCommandError {
                command_id,
                command_type: COMMAND,
                message: &error.to_string(),
            },
        ),
    }
}
