use std::collections::HashMap;

use rune::color::oklch;
use rune::{Color, El, Family, Icon, ScrollSpec, TextStyle, col, el, icon, row, text};
use uuid::Uuid;

use crate::connection::{
    ApprovalDecision, ApprovalResponse, PendingApproval, RuntimeResponseOutcome,
};

use super::{Palette, State};

const TEXT: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const ACTION: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);
const MONO: TextStyle = TextStyle::new(12.0, 400.0)
    .lh(20.0)
    .family(Family::SystemMono);

static TERMINAL: Icon = Icon {
    name: "terminal",
    view: 24.0,
    body: r#"<path d="M12 19h8 M4 17l6-6-6-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static SHIELD: Icon = Icon {
    name: "shield",
    view: 24.0,
    body: r#"<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(super) struct Approvals {
    visible: Vec<PendingApproval>,
    can_respond: bool,
    pending: HashMap<String, Submission>,
}

enum Submission {
    Sending(ApprovalResponse),
    AwaitingProjection,
    Unconfirmed(ApprovalResponse),
    Rejected(String),
}

impl Approvals {
    pub(super) fn replace(&mut self, approvals: Vec<PendingApproval>, can_respond: bool) {
        self.pending
            .retain(|id, _| approvals.iter().any(|approval| &approval.id == id));
        self.visible = approvals;
        self.can_respond = can_respond;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    pub(super) fn await_sync(&mut self) {
        self.visible.clear();
        self.can_respond = false;
    }

    fn can_choose(&self, id: &str, decision: ApprovalDecision) -> bool {
        self.can_respond
            && match self.pending.get(id) {
                Some(Submission::Sending(_) | Submission::AwaitingProjection) => false,
                Some(Submission::Unconfirmed(response)) => response.decision == decision,
                Some(Submission::Rejected(_)) | None => true,
            }
    }

    fn start(
        &mut self,
        thread_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<ApprovalResponse> {
        if !self.can_choose(request_id, decision)
            || !self.visible.iter().any(|request| {
                request.id == request_id
                    && request.live
                    && request
                        .choices
                        .iter()
                        .any(|choice| choice.decision == decision)
            })
        {
            return None;
        }
        let response = match self.pending.get(request_id) {
            Some(Submission::Unconfirmed(response)) => response.clone(),
            _ => ApprovalResponse {
                thread_id: thread_id.to_owned(),
                request_id: request_id.to_owned(),
                command_id: Uuid::new_v4(),
                decision,
            },
        };
        self.pending
            .insert(request_id.to_owned(), Submission::Sending(response.clone()));
        Some(response)
    }

    pub(super) fn finish(&mut self, response: ApprovalResponse, outcome: RuntimeResponseOutcome) {
        let Some(submission) = self.pending.get_mut(&response.request_id) else {
            return;
        };
        if !matches!(submission, Submission::Sending(sent) if sent.command_id == response.command_id)
        {
            return;
        }
        *submission = match outcome {
            RuntimeResponseOutcome::Accepted => Submission::AwaitingProjection,
            RuntimeResponseOutcome::Rejected(message) => Submission::Rejected(message),
            RuntimeResponseOutcome::Unconfirmed => Submission::Unconfirmed(response),
        };
    }
}

impl State {
    fn choose_approval(&mut self, thread_id: &str, request_id: &str, decision: ApprovalDecision) {
        if self.connection.is_none()
            || self
                .workspace
                .as_ref()
                .is_none_or(|workspace| workspace.reconnecting || !workspace.prompt_writes)
        {
            return;
        }
        if let Some(response) = self.conversation_mut(thread_id).and_then(|conversation| {
            conversation
                .approvals
                .start(thread_id, request_id, decision)
        }) && let Some(connection) = &self.connection
        {
            connection.respond_to_approval(response);
        }
    }
}

pub(super) fn rows<'a>(
    thread_id: &'a str,
    state: &'a Approvals,
    palette: Palette,
    reconnecting: bool,
) -> impl Iterator<Item = El> + 'a {
    state
        .visible
        .iter()
        .map(move |request| approval_row(thread_id, request, state, palette, reconnecting))
}

fn approval_row(
    thread_id: &str,
    request: &PendingApproval,
    state: &Approvals,
    palette: Palette,
    reconnecting: bool,
) -> El {
    let warning = oklch(0.769, 0.188, 70.08);
    let muted = if palette.dark {
        oklch(0.708, 0.0, 0.0)
    } else {
        oklch(0.552, 0.016, 285.938)
    };
    let ink = if palette.dark {
        oklch(0.97, 0.0, 0.0)
    } else {
        oklch(0.21, 0.006, 285.885)
    };
    let terminal_ink = if palette.dark {
        muted.alpha(0.7)
    } else {
        muted.mix(Color::BLACK, 0.6).alpha(0.7)
    };
    let key = &request.id;
    let command = request.command.as_deref().unwrap_or("Command approval");
    let command = col()
        .key(format!("approval-detail-{key}"))
        .w_full()
        .max_h(160.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            text(command)
                .font(MONO)
                .color(ink)
                .wrap_anywhere()
                .selectable(true)
                .label("Command"),
        );
    let mut body = col().w_full().min_w(0.0).grow(1.0).child(command);
    if let Some(reason) = &request.reason {
        body = body.child(
            text(reason)
                .font(TEXT)
                .color(muted)
                .wrap_anywhere()
                .selectable(true),
        );
    }
    let submission = state.pending.get(key);
    let status = if reconnecting {
        "Reconnecting…"
    } else if !request.live {
        "Provider process is gone. Interrupt or restart the run to respond."
    } else if matches!(
        submission,
        Some(Submission::Sending(_) | Submission::AwaitingProjection)
    ) {
        "Sending response…"
    } else {
        "Waiting for approval"
    };
    let mut actions = row().gap(8.0).center_y().child(
        row()
            .gap(4.0)
            .center_y()
            .child(icon(&SHIELD, 12.0).color(warning).a11y_hidden())
            .child(text(status).font(TEXT).color(warning).role("status")),
    );
    if !request.choices.is_empty() {
        actions = actions.child(text("·").font(TEXT).color(muted).a11y_hidden());
    }
    for choice in &request.choices {
        let decision = choice.decision;
        let action = match decision {
            ApprovalDecision::Accept => "approve",
            ApprovalDecision::Decline => "decline",
        };
        let enabled = !reconnecting && state.can_choose(&request.id, decision);
        let selected_thread = thread_id.to_owned();
        let request_id = key.clone();
        let color = if decision == ApprovalDecision::Accept {
            ink
        } else {
            muted
        };
        let mut button = el()
            .key(format!("{action}-command-{key}"))
            .role("button")
            .label(&choice.label)
            .radius(3.0)
            .disabled(!enabled)
            .focusable(enabled)
            .focus_ring(2.0, 1.0, palette.accent)
            .on_press(move |state: &mut State| {
                state.choose_approval(&selected_thread, &request_id, decision)
            })
            .child(text(&choice.label).font(ACTION).color(if enabled {
                color
            } else {
                color.alpha(0.64)
            }));
        if let Some(warning) = &choice.warning {
            button = button.help(warning);
        }
        actions = actions.child(button);
    }
    body = body.child(actions);
    let error = match submission {
        Some(Submission::Unconfirmed(_)) => Some(
            "The response could not be confirmed. Retry the same decision or check the conversation.",
        ),
        Some(Submission::Rejected(message)) => Some(message.as_str()),
        _ => None,
    };
    if let Some(error) = error {
        body = body.child(
            text(error)
                .font(TEXT)
                .color(palette.error)
                .wrap()
                .role("alert"),
        );
    }
    row()
        .key(format!("command-approval-{key}"))
        .label("Command approval")
        .role("group")
        .w_full()
        .pl(6.0)
        .pr(2.0)
        .py(6.0)
        .gap(10.0)
        .items_start()
        .child(icon(&TERMINAL, 16.0).color(terminal_ink).a11y_hidden())
        .child(body)
}

#[cfg(test)]
mod tests {
    use crate::connection::ApprovalChoice;

    use super::*;

    #[test]
    fn an_unconfirmed_decision_retries_the_same_command_id() {
        let mut state = Approvals::default();
        let request = PendingApproval {
            id: "request".into(),
            command: Some("git status".into()),
            reason: None,
            live: true,
            choices: [ApprovalDecision::Accept, ApprovalDecision::Decline]
                .into_iter()
                .map(|decision| ApprovalChoice {
                    decision,
                    label: "Choice".into(),
                    warning: None,
                })
                .collect(),
        };
        state.replace(vec![request.clone()], false);
        assert!(
            state
                .start("thread", "request", ApprovalDecision::Accept)
                .is_none()
        );
        state.replace(vec![request], true);
        let sent = state
            .start("thread", "request", ApprovalDecision::Accept)
            .unwrap();
        assert!(
            state
                .start("thread", "request", ApprovalDecision::Accept)
                .is_none()
        );
        state.finish(sent.clone(), RuntimeResponseOutcome::Unconfirmed);
        assert!(
            state
                .start("thread", "request", ApprovalDecision::Decline)
                .is_none()
        );
        let retry = state
            .start("thread", "request", ApprovalDecision::Accept)
            .unwrap();
        assert_eq!(sent.command_id, retry.command_id);
        state.finish(retry, RuntimeResponseOutcome::Accepted);
        assert!(
            state
                .start("thread", "request", ApprovalDecision::Accept)
                .is_none()
        );
        state.replace(Vec::new(), true);
        assert!(state.is_empty());
    }
}
