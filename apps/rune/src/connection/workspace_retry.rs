use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use super::{
    Event, EventSender, REQUEST_TIMEOUT, RpcExit, RuntimeResponseOutcome, Session, SessionStop,
};

#[derive(Clone)]
pub(crate) struct WorkspaceRetry {
    pub(crate) thread_id: String,
    pub(crate) run_id: String,
    pub(crate) command_id: Uuid,
}

impl WorkspaceRetry {
    fn settle(self, outcome: RuntimeResponseOutcome) -> Event {
        Event::WorkspaceRetrySettled {
            retry: self,
            outcome,
        }
    }
}

impl Session {
    pub(super) async fn submit_workspace_retry(
        &mut self,
        retry: WorkspaceRetry,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let failure = if !self.launch_writes || !self.worktree_launch {
            Some("This session cannot retry workspace preparation.")
        } else if !self.thread.as_ref().is_some_and(|thread| {
            thread.thread_id == retry.thread_id
                && thread
                    .conversation
                    .can_retry_workspace_preparation(&retry.thread_id, &retry.run_id)
        }) {
            Some("This workspace preparation is no longer available to retry.")
        } else if self.pending_workspace_retries.values().any(|(pending, _)| {
            pending.thread_id == retry.thread_id && pending.run_id == retry.run_id
        }) {
            Some("A workspace preparation retry is already in progress.")
        } else {
            None
        };
        if let Some(failure) = failure {
            events.send(retry.settle(RuntimeResponseOutcome::Rejected(failure.to_owned())));
            return Ok(());
        }
        let request = self.allocate_id();
        let payload = json!({
            "type":"prepared-run.retry",
            "commandId":retry.command_id,
            "threadId":retry.thread_id,
            "runId":retry.run_id,
        });
        self.pending_workspace_retries
            .insert(request.clone(), (retry, Instant::now()));
        self.send_request(&request, "orchestration.dispatchCommand", payload)
            .await
    }

    pub(super) fn workspace_retry_exit(
        &mut self,
        request: &str,
        exit: RpcExit,
        events: &EventSender<impl Fn()>,
    ) {
        if let Some((retry, _)) = self.pending_workspace_retries.remove(request) {
            let outcome = match exit.into_result() {
                Ok(value) if value["sequence"].as_u64().is_some() => {
                    RuntimeResponseOutcome::Accepted
                }
                Ok(_) => RuntimeResponseOutcome::Unconfirmed,
                Err(message) => RuntimeResponseOutcome::Rejected(message),
            };
            events.send(retry.settle(outcome));
        }
    }

    pub(super) fn expire_workspace_retries(&mut self, events: &EventSender<impl Fn()>) {
        self.pending_workspace_retries
            .retain(|_, (retry, started)| {
                if started.elapsed() < REQUEST_TIMEOUT {
                    return true;
                }
                events.send(retry.clone().settle(RuntimeResponseOutcome::Unconfirmed));
                false
            });
    }

    pub(super) fn unconfirm_workspace_retries(&mut self, events: &EventSender<impl Fn()>) {
        for (_, (retry, _)) in self.pending_workspace_retries.drain() {
            events.send(retry.settle(RuntimeResponseOutcome::Unconfirmed));
        }
    }
}
