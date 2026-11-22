use std::collections::HashMap;

use rune::{El, Tx};
use uuid::Uuid;

use crate::connection::worktree_removal::{Reply, Request};

use super::super::super::{Palette, State, confirmation};
use super::Target;

const CANCEL: &str = "sidebar-keep-worktree";
const CONFIRM: &str = "sidebar-remove-worktree";

#[derive(Default)]
pub(super) struct Panel {
    pending: HashMap<Uuid, Operation>,
    confirmation: Option<Choice>,
}

enum Operation {
    Inspect(Box<Choice>),
    Remove,
}

struct Choice {
    target: Target,
    checkout: Checkout,
}

pub(super) struct Checkout {
    cwd: String,
    path: String,
}

impl Panel {
    pub(super) fn checking(&self, id: &str) -> bool {
        self.pending.values().any(
            |operation| matches!(operation, Operation::Inspect(choice) if choice.target.id == id),
        )
    }
}

pub(super) fn inspect(state: &mut State, target: &Target) -> bool {
    let Some(workspace) = &state.workspace else {
        return false;
    };
    let Some(path) = target.worktree_path.as_ref() else {
        return false;
    };
    let Some(project) = workspace.shell.as_ref().and_then(|shell| {
        shell
            .projects
            .iter()
            .find(|project| project.id == target.project_id)
    }) else {
        return false;
    };
    if workspace.scratch_project.as_deref() == Some(project.id.as_str())
        || state.automatically_removes_worktree(&project.id)
    {
        return false;
    }
    let Some(connection) = &state.connection else {
        return false;
    };
    let operation = Uuid::new_v4();
    state.thread_actions.worktree.pending.insert(
        operation,
        Operation::Inspect(Box::new(Choice {
            target: target.clone(),
            checkout: Checkout {
                cwd: project.path.clone(),
                path: path.clone(),
            },
        })),
    );
    connection.worktree_removal(
        operation,
        Request::Inspect {
            thread_id: target.id.clone(),
            path: path.clone(),
        },
    );
    true
}

pub(super) fn remove(state: &mut State, checkout: Checkout) {
    let Some(connection) = &state.connection else {
        return;
    };
    let operation = Uuid::new_v4();
    state
        .thread_actions
        .worktree
        .pending
        .insert(operation, Operation::Remove);
    connection.worktree_removal(
        operation,
        Request::Remove {
            cwd: checkout.cwd,
            path: checkout.path,
        },
    );
}

pub(in crate::ui) fn settle(
    state: &mut State,
    operation: Uuid,
    result: Result<Reply, String>,
    tx: &mut Tx,
) {
    match state.thread_actions.worktree.pending.remove(&operation) {
        Some(Operation::Inspect(choice)) => {
            if matches!(result, Ok(Reply::Exclusive(true)))
                && state.can_delete_active_thread(&choice.target.id)
                && state.thread_actions.confirmation.is_none()
                && state.thread_actions.worktree.confirmation.is_none()
            {
                state.thread_actions.worktree.confirmation = Some(*choice);
                tx.focus_when_shown(CANCEL, true);
            } else {
                state.delete_active_thread(&choice.target, None, tx);
            }
        }
        Some(Operation::Remove) => {
            if let Err(error) = result {
                state.project_notice = Some(format!("Worktree removal failed. {error}"));
            }
        }
        None => {}
    }
}

fn keep(state: &mut State, tx: &mut Tx) {
    if let Some(choice) = state.thread_actions.worktree.confirmation.take() {
        state.delete_active_thread(&choice.target, None, tx);
    }
}

fn confirm(state: &mut State, tx: &mut Tx) {
    if let Some(choice) = state.thread_actions.worktree.confirmation.take() {
        state.delete_active_thread(&choice.target, Some(choice.checkout), tx);
    }
}

pub(super) fn view(state: &State, palette: Palette) -> Option<El> {
    let choice = state.thread_actions.worktree.confirmation.as_ref()?;
    let path = choice.checkout.path.trim();
    let name = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(path);
    let detail = format!("This thread is the only one linked to this worktree:\n{name}");
    Some(confirmation::view(
        confirmation::Dialog {
            overlay: "sidebar-worktree-remove-overlay",
            key: "sidebar-worktree-remove-dialog",
            cancel: CANCEL,
            confirm: CONFIRM,
            title: "Delete the worktree too?",
            detail: Some(("sidebar-worktree-remove-detail", &detail)),
            destructive: true,
        },
        palette,
        keep,
        confirm,
    ))
}
