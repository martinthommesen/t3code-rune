use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use super::{CodexAdapterError, CodexError, CodexSessionClient, ThreadId};

const TERMINATE: &str = "thread/backgroundTerminals/terminate";
const LIST: &str = "thread/backgroundTerminals/list";
const DEADLINE: Duration = Duration::from_secs(10);

impl CodexSessionClient {
    pub async fn terminate_background_terminal(
        &self,
        thread_id: &ThreadId,
        process_id: &str,
    ) -> Result<(), CodexAdapterError> {
        tokio::time::timeout(DEADLINE, async {
            let result: BackgroundTerminalTermination = match self
                .request(
                    TERMINATE,
                    json!({"threadId": thread_id, "processId": process_id}),
                )
                .await
            {
                Ok(result) => result,
                Err(CodexAdapterError::Transport(
                    CodexError::Closed | CodexError::Exited { .. },
                )) => return Ok(()),
                Err(CodexAdapterError::Transport(CodexError::Remote { error, .. }))
                    if error.code == -32600 && error.message.starts_with("thread not found:") =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if result.terminated {
                return Ok(());
            }
            let mut params = json!({"threadId": thread_id});
            loop {
                let result: BackgroundTerminalPage = self.request(LIST, params.clone()).await?;
                if result.data.iter().any(|item| item.process_id == process_id) {
                    return Err(CodexAdapterError::BackgroundTerminalStillActive);
                }
                let Some(cursor) = result.next_cursor.0 else {
                    return Ok(());
                };
                params["cursor"] = json!(cursor);
            }
        })
        .await
        .map_err(|_| CodexError::Timeout(TERMINATE.into()))?
    }
}

#[derive(Deserialize)]
struct BackgroundTerminalTermination {
    terminated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundTerminalPage {
    data: Vec<BackgroundTerminal>,
    next_cursor: Cursor,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundTerminal {
    process_id: String,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct Cursor(Option<String>);
