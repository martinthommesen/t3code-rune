use serde::Deserialize;

const CODE: &str = "workspace_preparation_failed";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceFailure {
    #[serde(default)]
    pub(crate) id: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) title: Option<String>,
    status: Option<String>,
    failure: Option<Detail>,
    pub(crate) started_at: Option<String>,
    pub(crate) completed_at: Option<String>,
    pub(crate) updated_at: Option<String>,
    #[serde(skip)]
    pub(crate) retryable: bool,
}

impl WorkspaceFailure {
    pub(super) fn is_workspace_preparation(&self) -> bool {
        !self.id.is_empty()
            && self
                .failure
                .as_ref()
                .and_then(|failure| failure.code.as_deref())
                == Some(CODE)
    }

    pub(super) fn is_failed(&self) -> bool {
        self.status.as_deref() == Some("failed")
    }

    pub(crate) fn message(&self) -> Option<&str> {
        self.failure.as_ref()?.message.as_deref()
    }

    pub(crate) fn can_retry(&self, thread_id: &str) -> bool {
        self.retryable && self.thread_id.as_deref().is_none_or(|id| id == thread_id)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Detail {
    code: Option<String>,
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{Conversation, ConversationEntry};

    #[test]
    fn the_canonical_error_enables_retry_until_preparation_is_reopened() {
        let mut conversation = Conversation::default();
        let run = |status| json!({"id":"run","status":status,"workspacePreparation":{"type":"worktree","baseRef":"main"}});
        let error = |status| {
            json!({"type":"error","id":"failure","threadId":"thread","runId":"run","status":status,
            "failure":{"code":"workspace_preparation_failed","message":"Setup failed","retryable":false}})
        };

        conversation
            .apply(json!({"kind":"snapshot","snapshotSequence":1,"projection":{
            "messages":[],"runs":[run("failed")],"visibleTurnItems":[{"item":error("failed")}]}}))
            .unwrap();
        assert!(conversation.can_retry_workspace_preparation("thread", "run"));
        assert!(!conversation.can_retry_workspace_preparation("other", "run"));
        assert!(
            matches!(&conversation.entries()[0], ConversationEntry::WorkspaceFailure(failure) if failure.can_retry("thread"))
        );

        conversation.apply(json!({"kind":"event","sequence":2,"event":{"type":"run.updated","payload":run("preparing")}})).unwrap();
        assert!(!conversation.can_retry_workspace_preparation("thread", "run"));
        assert!(
            matches!(&conversation.entries()[0], ConversationEntry::WorkspaceFailure(failure) if !failure.can_retry("thread"))
        );

        conversation.apply(json!({"kind":"event","sequence":3,"event":{"type":"turn-item.updated","payload":error("cancelled")}})).unwrap();
        assert!(conversation.entries().is_empty());
    }
}
