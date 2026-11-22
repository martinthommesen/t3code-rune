use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApprovalDecision {
    Accept,
    Decline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovalChoice {
    pub(crate) decision: ApprovalDecision,
    pub(crate) label: String,
    pub(crate) warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingApproval {
    pub(crate) id: String,
    pub(crate) command: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) live: bool,
    pub(crate) choices: Vec<ApprovalChoice>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RuntimeRequest {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) status: String,
    pub(super) created_at: String,
    response_capability: ResponseCapability,
}

impl RuntimeRequest {
    pub(super) fn live(&self) -> bool {
        self.response_capability.kind == "live"
    }
}

#[derive(Deserialize)]
struct ResponseCapability {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApprovalDetails {
    request_id: String,
    request_kind: String,
    status: String,
    prompt: Option<String>,
    reason: Option<String>,
    options: Option<Vec<ProviderOption>>,
}

#[derive(Deserialize)]
struct ProviderOption {
    decision: String,
    label: String,
    warning: Option<String>,
}

#[derive(Default)]
pub(super) struct ApprovalProjection {
    pending: HashMap<String, RuntimeRequest>,
    details: HashMap<String, ApprovalDetails>,
}

impl ApprovalProjection {
    pub(super) fn request(&mut self, request: RuntimeRequest) -> bool {
        if request.kind != "command" {
            return false;
        }
        if request.status != "pending" {
            self.details.remove(&request.id);
            return self.pending.remove(&request.id).is_some();
        }
        self.pending.insert(request.id.clone(), request);
        true
    }

    pub(super) fn detail(&mut self, details: ApprovalDetails) -> bool {
        if details.request_kind != "command" {
            return false;
        }
        if matches!(
            details.status.as_str(),
            "cancelled" | "completed" | "failed"
        ) {
            return self.details.remove(&details.request_id).is_some();
        }
        self.details.insert(details.request_id.clone(), details);
        true
    }

    pub(super) fn visible(&self) -> Vec<PendingApproval> {
        let mut requests: Vec<_> = self.pending.values().collect();
        requests.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        requests
            .into_iter()
            .map(|request| self.visible_request(request))
            .collect()
    }

    pub(super) fn allows(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        self.pending.get(request_id).is_some_and(|request| {
            let approval = self.visible_request(request);
            approval.live
                && approval
                    .choices
                    .iter()
                    .any(|choice| choice.decision == decision)
        })
    }

    fn visible_request(&self, request: &RuntimeRequest) -> PendingApproval {
        let details = self.details.get(&request.id);
        let command = details
            .and_then(|details| details.prompt.clone())
            .filter(|text| !text.trim().is_empty());
        let live = request.live();
        let choices = if command.is_some() && live {
            details.map_or_else(Vec::new, primary_choices)
        } else {
            Vec::new()
        };
        PendingApproval {
            id: request.id.clone(),
            command,
            reason: details.and_then(|details| details.reason.clone()),
            live,
            choices,
        }
    }
}

fn primary_choices(details: &ApprovalDetails) -> Vec<ApprovalChoice> {
    use ApprovalDecision::{Accept, Decline};

    let Some(options) = &details.options else {
        return [(Decline, "Decline"), (Accept, "Approve")]
            .into_iter()
            .map(|(decision, label)| ApprovalChoice {
                decision,
                label: label.into(),
                warning: None,
            })
            .collect();
    };
    options
        .iter()
        .filter_map(|option| {
            let decision = match option.decision.as_str() {
                "accept" => Accept,
                "decline" => Decline,
                _ => return None,
            };
            Some(ApprovalChoice {
                decision,
                label: option.label.clone(),
                warning: option.warning.clone(),
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalResponse {
    pub(crate) thread_id: String,
    pub(crate) request_id: String,
    pub(crate) command_id: Uuid,
    pub(crate) decision: ApprovalDecision,
}

impl ApprovalResponse {
    pub(super) fn payload(&self) -> Value {
        json!({
            "type": "runtime-request.respond",
            "commandId": self.command_id,
            "threadId": self.thread_id,
            "requestId": self.request_id,
            "decision": self.decision,
        })
    }
}
