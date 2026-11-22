use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::sync::watch;

use super::{CodexAdapterError, CodexError, CodexSessionClient, ThreadId, decode};

const CLOSE_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum UnsubscribeStatus {
    NotLoaded,
    NotSubscribed,
    Unsubscribed,
}

#[derive(Deserialize)]
struct UnsubscribeResponse {
    #[serde(rename = "status")]
    _status: UnsubscribeStatus,
}

impl CodexSessionClient {
    pub async fn unsubscribe_thread(&self, thread_id: &ThreadId) -> Result<(), CodexAdapterError> {
        const METHOD: &str = "thread/unsubscribe";
        {
            let mut state = self.scope.state.lock().expect("Codex session lock");
            state.approvals.remove(thread_id);
            state
                .thread_closings
                .entry(thread_id.clone())
                .or_insert_with(|| watch::channel(()).0);
        }
        let response = self
            .client
            .request(METHOD, Some(json!({"threadId": thread_id})), CLOSE_DEADLINE)
            .await?;
        decode::<UnsubscribeResponse>(METHOD, response)?;
        Ok(())
    }

    pub(super) async fn await_thread_closed(&self, thread_id: &ThreadId) {
        let closing = self
            .scope
            .state
            .lock()
            .expect("Codex session lock")
            .thread_closings
            .get(thread_id)
            .map(watch::Sender::subscribe);
        if let Some(mut closing) = closing {
            // The reader removes the sender when Codex reports thread/closed.
            let _ = tokio::time::timeout(CLOSE_DEADLINE, closing.changed()).await;
        }
    }
}

pub(super) fn is_closing(error: &CodexAdapterError) -> bool {
    let CodexAdapterError::Transport(CodexError::Remote { error, .. }) = error else {
        return false;
    };
    let words: Vec<_> = error.message.split_whitespace().collect();
    words.windows(4).any(|parts| {
        parts[0].eq_ignore_ascii_case("thread")
            && parts[2].eq_ignore_ascii_case("is")
            && parts[3]
                .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .eq_ignore_ascii_case("closing")
    })
}
