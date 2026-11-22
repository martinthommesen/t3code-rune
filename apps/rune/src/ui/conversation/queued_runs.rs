use rune::{El, Icon, TextEdit, Tx, col, el, icon, row, text};
use uuid::Uuid;

use super::{Colors, Conversation, SUMMARY_TEXT, State, Workspace};
use crate::connection::{QueueMutation, QueuedMessage};
use crate::ui::{QueuedEdit, button};

const VISIBLE_ROWS: usize = 3;

pub(super) fn view(
    conversation: &Conversation,
    workspace: &Workspace,
    width: f32,
    colors: Colors,
) -> Option<El> {
    let count = conversation.queued.len();
    if count == 0 {
        return None;
    }
    let collapsible = count > VISIBLE_ROWS;
    let visible = if collapsible && !conversation.show_all_queued {
        VISIBLE_ROWS - 1
    } else {
        count
    };
    let mut lane = col()
        .key("queued-messages")
        .role("region")
        .label(if count == 1 {
            "1 queued message".to_owned()
        } else {
            format!("{count} queued messages")
        })
        .w_full()
        .items_end()
        .gap(12.0);
    for (index, queued) in conversation.queued.iter().take(visible).enumerate() {
        let editing = conversation
            .queued_edit
            .as_ref()
            .is_some_and(|edit| edit.run_id == queued.run_id);
        let bubble = col()
            .key(format!("queued-message-{}", queued.run_id))
            .role("listitem")
            .label(format!("Queued message {} of {count}, not sent", index + 1))
            .px(12.0)
            .py(8.0)
            .min_w(0.0)
            .max_w(width)
            .radius(16.0)
            .border(
                1.0,
                if editing {
                    colors.fallbacks.accent.alpha(0.6)
                } else {
                    colors.muted.alpha(0.4)
                },
            )
            .border_layout()
            .border_dash(3.0, 2.0)
            .child(
                text(queued.text.trim())
                    .font(SUMMARY_TEXT)
                    .color(if editing {
                        colors.foreground
                    } else {
                        colors.muted
                    })
                    .wrap_anywhere(),
            );
        let card = col().w_full().gap(4.0).items_end().child(bubble);
        lane = lane.child(
            if queued.worktree_queue && can_change(workspace, &conversation.id) {
                card.child(controls(conversation, queued, editing, colors))
            } else {
                card
            },
        );
    }
    if collapsible {
        let thread_id = conversation.id.clone();
        let expanded = conversation.show_all_queued;
        let label = if expanded {
            "Show fewer".to_owned()
        } else {
            format!("Show all {count} queued")
        };
        lane = lane.child(
            row()
                .key("toggle-queued-messages")
                .role("button")
                .label(&label)
                .expanded(expanded)
                .focusable(true)
                .focus_ring(2.0, -1.0, colors.fallbacks.accent)
                .on_press(move |state: &mut State| {
                    if let Some(conversation) = state.conversation_mut(&thread_id) {
                        conversation.show_all_queued = !conversation.show_all_queued;
                    }
                })
                .child(text(label).font(SUMMARY_TEXT).color(colors.muted)),
        );
    }
    Some(lane)
}

static PENCIL: Icon = Icon {
    name: "queue-pencil",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.174 6.812a1 1 0 0 0-3.987-3.987L3.842 16.173a2 2 0 0 0-.5.845l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .845-.5z"/><path d="m15 5 4 4"/></g>"#,
};

static REMOVE: Icon = Icon {
    name: "queue-remove",
    view: 24.0,
    body: r#"<path d="m18 6-12 12M6 6l12 12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
};

fn can_change(workspace: &Workspace, thread_id: &str) -> bool {
    workspace.native_stop
        && workspace.prompt_writes
        && !workspace.reconnecting
        && workspace
            .thread(thread_id)
            .is_some_and(|thread| !thread.archived)
}

fn controls(
    conversation: &Conversation,
    queued: &QueuedMessage,
    editing: bool,
    colors: Colors,
) -> El {
    let id = conversation.id.clone();
    let run_id = queued.run_id.clone();
    let ready = conversation.queue_change_pending.is_none();
    let actions = row().center_y().gap(4.0);
    if editing {
        actions
            .child(
                text("Editing in the composer")
                    .font(SUMMARY_TEXT)
                    .color(colors.foreground),
            )
            .child(button(
                &format!("cancel-edit-queue-{}", queued.run_id),
                "Cancel",
                colors.fallbacks,
                false,
                ready,
                move |state, tx| {
                    state.end_queued_edit(&id);
                    tx.focus("prompt", tx.focus_visible());
                },
            ))
    } else {
        let edit = action(
            format!("edit-queue-{}", queued.run_id),
            "Edit queued message",
            &PENCIL,
            ready && queued.text_editable && conversation.queued_edit.is_none(),
            colors,
            move |state, tx| state.begin_queued_edit(&id, &run_id, tx),
        );
        let id = conversation.id.clone();
        let run_id = queued.run_id.clone();
        let remove = action(
            format!("remove-queue-{}", queued.run_id),
            "Remove queued message",
            &REMOVE,
            ready,
            colors,
            move |state, _| {
                state.mutate_queued_message(
                    &id,
                    QueueMutation::Remove {
                        run_id: run_id.clone(),
                    },
                );
            },
        );
        actions.child(edit).child(remove)
    }
}

fn action(
    key: String,
    label: &'static str,
    picture: &'static Icon,
    enabled: bool,
    colors: Colors,
    press: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .help(label)
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .hover(colors.accent)
        .disabled(!enabled)
        .focusable(enabled)
        .focus_ring(2.0, -1.0, colors.fallbacks.accent)
        .on_press_tx(press)
        .child(icon(picture, 12.0).color(colors.muted).a11y_hidden())
}

impl State {
    fn begin_queued_edit(&mut self, thread_id: &str, run_id: &str, tx: &mut Tx) {
        if !self
            .workspace
            .as_ref()
            .is_some_and(|workspace| can_change(workspace, thread_id))
        {
            return;
        }
        let Some(text) = self
            .conversation_mut(thread_id)
            .filter(|conversation| {
                conversation.queued_edit.is_none() && conversation.queue_change_pending.is_none()
            })
            .and_then(|conversation| {
                conversation.queued.iter().find(|queued| {
                    queued.run_id == run_id && queued.text_editable && queued.worktree_queue
                })
            })
            .map(|queued| queued.text.clone())
        else {
            return;
        };
        let Some(draft) = self
            .drafts
            .get_mut(thread_id)
            .filter(|draft| !draft.pending)
        else {
            return;
        };
        let edit = QueuedEdit {
            run_id: run_id.to_owned(),
            draft: std::mem::replace(&mut draft.edit, TextEdit::new(&text)),
            failure: draft.failure.take(),
        };
        if let Some(conversation) = self.conversation_mut(thread_id) {
            conversation.queued_edit = Some(edit);
        }
        tx.focus("prompt", tx.focus_visible());
    }

    pub(in crate::ui) fn end_queued_edit(&mut self, thread_id: &str) {
        if let Some(edit) = self
            .conversation_mut(thread_id)
            .and_then(|conversation| conversation.queued_edit.take())
            && let Some(draft) = self.drafts.get_mut(thread_id)
        {
            draft.edit = edit.draft;
            draft.failure = edit.failure;
            draft.pending = false;
        }
    }

    pub(in crate::ui) fn mutate_queued_message(
        &mut self,
        thread_id: &str,
        mutation: QueueMutation,
    ) {
        if self.connection.is_none()
            || !self
                .workspace
                .as_ref()
                .is_some_and(|workspace| can_change(workspace, thread_id))
            || matches!(&mutation, QueueMutation::Edit { text, .. } if text.trim().is_empty())
        {
            return;
        }
        let editing = matches!(mutation, QueueMutation::Edit { .. });
        let Some(conversation) = self.conversation_mut(thread_id).filter(|conversation| {
            conversation.queue_change_pending.is_none()
                && conversation.queued.iter().any(|queued| {
                    queued.worktree_queue
                        && queued.run_id == mutation.run_id()
                        && (!editing || queued.text_editable)
                })
        }) else {
            return;
        };
        let id = Uuid::new_v4();
        conversation.queue_change_pending = Some(id);
        if editing && let Some(draft) = self.drafts.get_mut(thread_id) {
            draft.pending = true;
            draft.failure = None;
        }
        if let Some(connection) = &self.connection {
            connection.mutate_queue(thread_id.to_owned(), mutation, id);
        }
    }
}
