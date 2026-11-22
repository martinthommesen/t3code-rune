use serde::Deserialize;
use serde_json::json;

use super::{Session, SessionStop};

impl Session {
    pub(super) async fn observe_draft_worktree(
        &mut self,
        thread_id: Option<String>,
    ) -> Result<(), SessionStop> {
        let thread_id = thread_id.filter(|_| self.worktree_setup && self.worktree_launch);
        if self.draft_setup.as_ref().map(|(_, thread)| thread.as_str()) == thread_id.as_deref() {
            return Ok(());
        }
        if let Some((request, _)) = self.draft_setup.take() {
            self.interrupt(&request).await?;
        }
        if let Some(thread_id) = thread_id {
            let request = self.allocate_id();
            self.send_request(
                &request,
                "subscribeWorktreeSetup",
                json!({"threadId": thread_id}),
            )
            .await?;
            self.draft_setup = Some((request, thread_id));
        }
        Ok(())
    }

    pub(super) fn setup_thread(&self, request_id: &str) -> Option<&str> {
        self.draft_setup
            .as_ref()
            .filter(|(request, _)| request == request_id)
            .map(|(_, thread)| thread.as_str())
            .or_else(|| {
                self.thread
                    .as_ref()
                    .filter(|thread| thread.setup_request_id.as_deref() == Some(request_id))
                    .map(|thread| thread.thread_id.as_str())
            })
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorktreeSetup {
    pub(crate) thread_id: String,
    pub(crate) phase: Phase,
    pub(crate) started_at: String,
    pub(crate) ended_at: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) base_ref: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) setup_script: Option<Script>,
    pub(crate) stages: Vec<Stage>,
    pub(crate) error: Option<String>,
    pub(crate) sequence: u64,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Script {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) terminal_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Stage {
    pub(crate) id: StageId,
    pub(crate) status: Status,
    pub(crate) started_at: Option<String>,
    pub(crate) ended_at: Option<String>,
    pub(crate) percent: Option<u8>,
    pub(crate) detail: Option<String>,
    pub(crate) tail: Vec<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StageId {
    Fetch,
    Checkout,
    Submodules,
    SetupScript,
    Agent,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Pending,
    Running,
    Done,
    Skipped,
    Warning,
    Failed,
}
