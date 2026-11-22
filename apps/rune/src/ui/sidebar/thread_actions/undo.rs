use std::collections::HashMap;
use std::time::{Duration, Instant};

use rune::{El, Key, KeyEvent, LiveRegion, PhysicalKeyCode, Tx, icon, row};

use crate::connection::{ArchiveAction, Keybindings};
use crate::ui::{Palette, ProjectView, State, sidebar_header};

use super::Destination;

const UNDO: &str = "sidebar-archive-undo";
const LIFETIME: Duration = Duration::from_secs(5);

static UNDO_ICON: rune::Icon = rune::Icon {
    name: "undo-2",
    view: 24.0,
    body: r#"<path d="m9 14-5-5 5-5M4 9h10.5a5.5 5.5 0 0 1 0 11H11" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

struct Thread {
    id: String,
    reopen_project: Option<String>,
}

#[derive(Default)]
pub(in crate::ui) struct Notice {
    threads: Vec<Thread>,
    expires_at: Option<Instant>,
    shortcut_claimed: bool,
    held_key: Option<PhysicalKeyCode>,
    pending_navigation: HashMap<String, String>,
    reopen: Option<Destination>,
}

impl Notice {
    pub(super) fn archived(&mut self, id: &str, reopen_project: Option<String>, tx: &mut Tx) {
        self.poll(tx);
        self.threads.retain(|thread| thread.id != id);
        self.threads.push(Thread {
            id: id.into(),
            reopen_project,
        });
        self.expires_at = Some(Instant::now() + LIFETIME);
        self.shortcut_claimed = true;
        tx.wake_after(LIFETIME);
    }

    pub(in crate::ui) fn invalidate(&mut self, id: &str) {
        self.threads.retain(|thread| thread.id != id);
    }

    pub(in crate::ui) fn poll(&mut self, tx: &mut Tx) -> bool {
        let Some(deadline) = self.expires_at else {
            return false;
        };
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            tx.wake_after(remaining);
            return false;
        }
        self.expires_at = None;
        !std::mem::take(&mut self.threads).is_empty()
    }

    pub(in crate::ui) fn release_shortcut(&mut self) {
        self.shortcut_claimed = false;
    }
}

fn shortcut_label(state: &State) -> String {
    state
        .keybindings
        .as_ref()
        .unwrap_or_else(|| Keybindings::stock())
        .shortcut_label("thread.undo", |flag| flag == "isDesktop")
        .map_or_else(|| "Undo".into(), |keys| format!("{keys} to undo"))
}

pub(super) fn announce_archive(state: &State, tx: &mut Tx) {
    let count = state.thread_actions.undo.threads.len();
    let noun = if count == 1 { "thread" } else { "threads" };
    tx.announce(
        format!("Archived {count} {noun}, {}", shortcut_label(state)),
        LiveRegion::Polite,
    );
}

pub(in crate::ui) fn footer(state: &State, palette: Palette) -> Option<El> {
    if state.thread_actions.undo.threads.is_empty() {
        return None;
    }
    let label = shortcut_label(state);
    let press = |state: &mut State, tx: &mut Tx| {
        activate(state, tx);
    };
    Some(
        row()
            .key(UNDO)
            .size(24.0, 32.0)
            .mr(32.0)
            .center()
            .role("button")
            .label(&label)
            .help(label)
            .focusable(true)
            .focus_ring(1.0, -1.0, palette.accent)
            .on_press_tx(press)
            .on_key(move |state: &mut State, event, tx| {
                sidebar_header::activate_on_return(state, event, tx, press)
            })
            .child(icon(&UNDO_ICON, 16.0).color(palette.ink).a11y_hidden()),
    )
}

fn activate(state: &mut State, tx: &mut Tx) -> bool {
    let undo = &mut state.thread_actions.undo;
    undo.poll(tx);
    let entries = std::mem::take(&mut undo.threads);
    if entries.is_empty() {
        return false;
    }
    undo.expires_at = None;
    for thread in entries.into_iter().rev() {
        if state.submit_archive_action(ArchiveAction::UndoArchive, &thread.id, tx)
            && let Some(project) = thread.reopen_project
        {
            state
                .thread_actions
                .undo
                .pending_navigation
                .insert(thread.id, project);
        }
    }
    true
}

pub(in crate::ui) fn key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if matches!(event.key, Key::Modifier { .. }) {
        return false;
    }
    let notice = &mut state.thread_actions.undo;
    if tx.key_repeat() {
        return notice.held_key.is_some() && notice.held_key == tx.physical_key();
    }
    notice.held_key = None;
    let has_notice = !notice.threads.is_empty();
    let editor_override = state.sidebar_visibility.is_open() && notice.shortcut_claimed;
    if !has_notice || !shortcut_available(state) {
        return false;
    }
    if state.resolve_shortcut_allowing_thread_undo(event, tx, editor_override)
        != Some("thread.undo")
    {
        state.thread_actions.undo.release_shortcut();
        return false;
    }
    if !activate(state, tx) {
        return false;
    }
    state.thread_actions.undo.held_key = tx.physical_key();
    true
}

fn shortcut_available(state: &State) -> bool {
    !state.settings.is_open()
        && state.command_palette.is_none()
        && state.thread_actions.confirmation.is_none()
        && state.thread_actions.menu.is_none()
        && state
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.selection.as_ref())
            .is_none_or(|selection| match &selection.view {
                ProjectView::Conversation(thread) => {
                    thread.model_picker.is_none() && thread.saved_model_picker.is_none()
                }
                ProjectView::NewThread => state
                    .new_thread_drafts
                    .get(&selection.id)
                    .is_none_or(|draft| draft.model_picker.is_none()),
                _ => true,
            })
}

pub(in crate::ui) fn settle(state: &mut State, id: &str, result: Result<(), String>, tx: &mut Tx) {
    let undo = &mut state.thread_actions.undo;
    let project = undo.pending_navigation.remove(id);
    if let Err(error) = result {
        state.project_notice = Some(format!("Failed to undo archive. {error}"));
        return;
    }
    tx.announce("Undid archive of 1 thread", LiveRegion::Polite);
    if let Some(project) = project {
        undo.reopen = Some(Destination::Thread {
            project,
            thread: id.into(),
        });
        reopen(state);
    }
}

pub(in crate::ui) fn reopen(state: &mut State) {
    let Some(Destination::Thread { thread, .. }) = &state.thread_actions.undo.reopen else {
        return;
    };
    if state
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.thread(thread))
        .is_none_or(|thread| thread.archived)
    {
        return;
    }
    if let Some(destination) = state.thread_actions.undo.reopen.take() {
        state.navigate(destination);
    }
}
