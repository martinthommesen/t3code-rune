use rune::color::oklch;
use rune::{Color, El, Icon, Tx, el, icon};
use uuid::Uuid;

use super::super::{Conversation, Draft, Palette, ProjectView, State, Transcript, Workspace};
use super::provider_unavailable;

const RESUME_KEY: &str = "resume-thread";

static PLAY: Icon = Icon {
    name: "resume-thread",
    view: 24.0,
    body: r#"<path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" fill="currentColor" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

fn conversation<'a>(workspace: &'a Workspace, thread_id: &str) -> Option<&'a Conversation> {
    let ProjectView::Conversation(conversation) = &workspace.selection.as_ref()?.view else {
        return None;
    };
    (conversation.id == thread_id).then_some(conversation.as_ref())
}

fn available<'a>(workspace: &'a Workspace, thread_id: &str) -> Option<&'a Conversation> {
    let conversation = conversation(workspace, thread_id)?;
    (workspace.native_stop
        && workspace.prompt_writes
        && workspace
            .thread(thread_id)
            .is_some_and(|thread| !thread.archived)
        && matches!(conversation.transcript, Transcript::Loaded(_))
        && conversation.queued_edit.is_none()
        && conversation.resumable_run.is_some())
    .then_some(conversation)
}

pub(super) fn pending(workspace: &Workspace, thread_id: &str) -> bool {
    conversation(workspace, thread_id)
        .is_some_and(|conversation| conversation.resume_pending.is_some())
}

pub(super) fn button(
    workspace: &Workspace,
    thread_id: &str,
    draft: &Draft,
    palette: Palette,
    blocked: bool,
) -> Option<El> {
    let conversation = available(workspace, thread_id)?;
    let busy = conversation.resume_pending.is_some();
    let ready = !blocked
        && !busy
        && !draft.pending
        && !workspace.reconnecting
        && conversation.queue_change_pending.is_none();
    let accent = oklch(0.571, 0.21, 264.0);
    Some(
        el().key(RESUME_KEY)
            .role("button")
            .label(if busy {
                "Resuming thread"
            } else {
                "Resume thread"
            })
            .help("Resume thread")
            .help_above()
            .help_open(ready && conversation.resume_focused)
            .size(32.0, 32.0)
            .radius(16.0)
            .center()
            .fill(if ready {
                accent
            } else {
                palette.ink.alpha(0.12)
            })
            .hover(oklch(0.61, 0.21, 264.0))
            .disabled(!ready)
            .focusable(ready)
            .focus_ring(1.0, -2.0, accent.alpha(0.5))
            .on_press_tx(|state: &mut State, tx| {
                if state.begin_resume_thread() {
                    tx.focus("prompt", tx.focus_visible());
                }
            })
            .child(
                icon(&PLAY, 16.0)
                    .color(if ready { Color::WHITE } else { palette.muted })
                    .a11y_hidden(),
            ),
    )
}

impl State {
    pub(in crate::ui) fn observe_resume_focus(&mut self, tx: &Tx) -> bool {
        let Some(ProjectView::Conversation(conversation)) = self
            .workspace
            .as_mut()
            .and_then(|workspace| workspace.selection.as_mut())
            .map(|selection| &mut selection.view)
        else {
            return false;
        };
        let focused = tx.focused() == Some(RESUME_KEY) && tx.focus_visible();
        let changed = conversation.resume_focused != focused;
        conversation.resume_focused = focused;
        changed
    }

    pub(super) fn submit_thread_prompt(&mut self, tx: &mut Tx) {
        let thread = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.selection.as_ref())
            .and_then(|selection| match &selection.view {
                ProjectView::Conversation(conversation) => Some(conversation),
                _ => None,
            });
        if let Some(conversation) = thread {
            if conversation.resume_pending.is_some() {
                return;
            }
            if self
                .drafts
                .get(&conversation.id)
                .is_some_and(|draft| draft.edit.text.trim().is_empty())
                && conversation.resumable_run.is_some()
            {
                self.begin_resume_thread();
                return;
            }
        }
        self.send_prompt(tx);
    }

    fn begin_resume_thread(&mut self) -> bool {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| !workspace.reconnecting)
        else {
            return false;
        };
        let Some(ProjectView::Conversation(conversation)) = workspace
            .selection
            .as_ref()
            .map(|selection| &selection.view)
        else {
            return false;
        };
        let thread_id = &conversation.id;
        if self
            .drafts
            .get(thread_id)
            .is_some_and(|draft| draft.pending_saved_reasoning.is_some())
        {
            return false;
        }
        let Some(run) = available(workspace, thread_id)
            .filter(|conversation| {
                conversation.resume_pending.is_none() && conversation.queue_change_pending.is_none()
            })
            .and_then(|conversation| conversation.resumable_run.clone())
        else {
            return false;
        };
        let Some(thread) = workspace.thread(thread_id) else {
            return false;
        };
        if self.connection.is_none()
            || workspace.pending_clone(&thread.project_id).is_some()
            || provider_unavailable(workspace, thread.providers.current_instance())
        {
            return false;
        }
        let thread_id = thread_id.clone();
        let Some(draft) = self
            .drafts
            .get_mut(&thread_id)
            .filter(|draft| !draft.pending && draft.edit.text.trim().is_empty())
        else {
            return false;
        };
        draft.failure = None;
        let operation = Uuid::new_v4();
        if let Some(conversation) = self.conversation_mut(&thread_id) {
            conversation.resume_pending = Some(operation);
        }
        if let Some(connection) = &self.connection {
            connection.resume_thread(thread_id, run, operation);
        }
        true
    }
}
