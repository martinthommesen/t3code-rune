use serde_json::Value;
use uuid::Uuid;

use super::{ApprovalResponse, Conversation, Event, QuestionResponse};

#[derive(Clone)]
pub(super) enum RuntimeResponse {
    Approval(ApprovalResponse),
    Question(QuestionResponse),
}

#[derive(Debug)]
pub(crate) enum RuntimeResponseOutcome {
    Accepted,
    Rejected(String),
    Unconfirmed,
}

impl RuntimeResponse {
    pub(super) fn identify(&self) -> (&str, &str, Uuid) {
        match self {
            Self::Approval(response) => (
                &response.thread_id,
                &response.request_id,
                response.command_id,
            ),
            Self::Question(response) => (
                &response.thread_id,
                &response.request_id,
                response.command_id,
            ),
        }
    }

    pub(super) fn allows(&self, conversation: &Conversation) -> bool {
        match self {
            Self::Approval(response) => conversation
                .approvals
                .allows(&response.request_id, response.decision),
            Self::Question(response) => conversation.questions.allows(response),
        }
    }

    pub(super) fn payload(&self) -> Value {
        match self {
            Self::Approval(response) => response.payload(),
            Self::Question(response) => response.payload(),
        }
    }

    pub(super) fn finish(self, outcome: RuntimeResponseOutcome) -> Event {
        match self {
            Self::Approval(response) => Event::ApprovalResponded { response, outcome },
            Self::Question(response) => Event::QuestionResponded { response, outcome },
        }
    }
}
