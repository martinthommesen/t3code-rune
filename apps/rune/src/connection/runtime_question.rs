use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::runtime_approval::RuntimeRequest;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Question {
    pub(crate) id: String,
    pub(crate) header: String,
    pub(crate) question: String,
    pub(crate) options: Vec<QuestionOption>,
    #[serde(default)]
    pub(crate) multi_select: bool,
    allow_custom_answer: Option<bool>,
    required: Option<bool>,
    pub(crate) initial_answer: Option<String>,
}

impl Question {
    pub(crate) fn allows_custom_answer(&self) -> bool {
        self.allow_custom_answer.unwrap_or(true)
    }

    pub(crate) fn allows_empty_answer(&self) -> bool {
        self.required == Some(false) && self.options.is_empty()
    }

    pub(crate) fn has_literal_answer(&self) -> bool {
        self.allows_custom_answer() && self.initial_answer.is_some()
    }

    fn offers(&self, value: &str) -> bool {
        self.options.iter().any(|option| option.value() == value)
    }

    fn allows(&self, answer: &Answer) -> bool {
        match answer {
            Answer::Text(text) => {
                if !self.multi_select && self.offers(text) {
                    true
                } else if text.is_empty() {
                    self.allows_empty_answer() || self.has_literal_answer()
                } else {
                    self.allows_custom_answer()
                }
            }
            Answer::Choices(choices) => {
                self.multi_select && !choices.is_empty() && choices.iter().all(|v| self.offers(v))
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct QuestionOption {
    pub(crate) label: String,
    pub(crate) description: String,
    value: Option<String>,
    pub(crate) preview: Option<String>,
}

impl QuestionOption {
    pub(crate) fn value(&self) -> &str {
        self.value.as_deref().unwrap_or_else(|| self.label.trim())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingQuestion {
    pub(crate) id: String,
    pub(crate) questions: Vec<Question>,
    pub(crate) live: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuestionDetails {
    request_id: String,
    status: String,
    questions: Vec<Question>,
}

#[derive(Default)]
pub(super) struct QuestionProjection {
    pending: HashMap<String, RuntimeRequest>,
    details: HashMap<String, Vec<Question>>,
}

impl QuestionProjection {
    pub(super) fn request(&mut self, request: RuntimeRequest) -> bool {
        if request.status != "pending" {
            self.details.remove(&request.id);
            return self.pending.remove(&request.id).is_some();
        }
        self.pending.insert(request.id.clone(), request);
        true
    }

    pub(super) fn detail(&mut self, details: QuestionDetails) -> bool {
        if matches!(
            details.status.as_str(),
            "cancelled" | "completed" | "failed"
        ) {
            return self.details.remove(&details.request_id).is_some();
        }
        self.details.insert(details.request_id, details.questions);
        true
    }

    pub(super) fn visible(&self) -> Vec<PendingQuestion> {
        let mut requests: Vec<_> = self.pending.values().collect();
        requests.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        requests
            .into_iter()
            .filter_map(|request| {
                let questions = self.details.get(&request.id)?.clone();
                (!questions.is_empty()).then(|| PendingQuestion {
                    id: request.id.clone(),
                    questions,
                    live: request.live(),
                })
            })
            .collect()
    }

    pub(super) fn allows(&self, response: &QuestionResponse) -> bool {
        self.pending
            .get(&response.request_id)
            .is_some_and(RuntimeRequest::live)
            && self
                .details
                .get(&response.request_id)
                .is_some_and(|questions| {
                    !questions.is_empty()
                        && questions.len() == response.answers.len()
                        && questions.iter().all(|question| {
                            response
                                .answers
                                .get(&question.id)
                                .is_some_and(|answer| question.allows(answer))
                        })
                })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub(crate) enum Answer {
    Text(String),
    Choices(Vec<String>),
}

#[derive(Clone, Debug)]
pub(crate) struct QuestionResponse {
    pub(crate) thread_id: String,
    pub(crate) request_id: String,
    pub(crate) command_id: Uuid,
    pub(crate) answers: BTreeMap<String, Answer>,
}

impl QuestionResponse {
    pub(super) fn payload(&self) -> Value {
        json!({
            "type": "runtime-request.respond",
            "commandId": self.command_id,
            "threadId": self.thread_id,
            "requestId": self.request_id,
            "answers": self.answers,
        })
    }
}
