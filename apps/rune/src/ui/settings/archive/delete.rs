use rune::{El, Icon, Tx, el, icon};

use crate::connection::{ArchiveAction, Thread};

use super::{Palette, State, press};

const CANCEL: &str = "archive-delete-cancel";
const CONFIRM: &str = "archive-delete-confirm";
const DESCRIPTION: &str = "archive-delete-description";

static TRASH: rune::Icon = glyph!(
    "trash-2",
    r#"<path d="M3 6h18m-2 0v14a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V6m3 0V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2m-6 4v7m4-7v7"/>"#
);

pub(super) struct Confirmation {
    thread_id: String,
    title: String,
    return_ring: bool,
}

impl State {
    fn request_archive_deletion(&mut self, thread_id: &str, title: &str, tx: &mut Tx) {
        let archive = &mut self.settings.archive;
        if archive.confirmation.is_some()
            || !archive.deletable
            || archive.pending.contains_key(thread_id)
            || self
                .workspace
                .as_ref()
                .is_none_or(|workspace| workspace.reconnecting)
        {
            return;
        }
        if !self.client_settings.confirm_thread_delete() {
            self.submit_archive_action(ArchiveAction::Delete, thread_id, tx);
            return;
        }
        archive.confirmation = Some(Confirmation {
            thread_id: thread_id.to_owned(),
            title: title.to_owned(),
            return_ring: tx.focus_visible(),
        });
        tx.focus_when_shown(CANCEL, true);
    }

    fn cancel_archive_deletion(&mut self, tx: &mut Tx) {
        if let Some(confirmation) = self.settings.archive.confirmation.take() {
            tx.focus_when_shown(key(&confirmation.thread_id), confirmation.return_ring);
        }
    }

    fn commit_archive_deletion(&mut self, tx: &mut Tx) {
        if let Some(confirmation) = self.settings.archive.confirmation.take() {
            tx.focus("close-client-settings", tx.focus_visible());
            self.submit_archive_action(ArchiveAction::Delete, &confirmation.thread_id, tx);
        }
    }
}

pub(super) fn key(thread_id: &str) -> String {
    format!("delete-archived-thread-{thread_id}")
}

pub(super) fn opener(thread: &Thread, enabled: bool, palette: Palette) -> El {
    let thread_id = thread.id.clone();
    let title = thread.title.clone();
    press(
        el().key(key(&thread.id))
            .role("button")
            .label(format!("Delete {}", thread.title))
            .help("Delete")
            .help_above()
            .size(28.0, 28.0)
            .min_w(28.0)
            .center()
            .radius(8.0)
            .fill(palette.field)
            .border(1.0, palette.line)
            .hover(palette.hover)
            .disabled(!enabled)
            .focusable(enabled)
            .focus_ring(1.0, 2.0, palette.accent)
            .child(icon(&TRASH, 14.0).color(palette.error).a11y_hidden()),
        move |state: &mut State, tx| state.request_archive_deletion(&thread_id, &title, tx),
    )
}

pub(in crate::ui::settings) fn confirmation(state: &State, palette: Palette) -> Option<El> {
    let confirmation = state.settings.archive.confirmation.as_ref()?;
    let title = format!("Delete thread \"{}\"?", confirmation.title);
    Some(crate::ui::confirmation::view(
        crate::ui::confirmation::Dialog {
            overlay: "archive-delete-overlay",
            key: "archive-delete-dialog",
            cancel: CANCEL,
            confirm: CONFIRM,
            title: &title,
            detail: Some((
                DESCRIPTION,
                "This permanently clears conversation history for this thread.",
            )),
            destructive: true,
        },
        palette,
        State::cancel_archive_deletion,
        State::commit_archive_deletion,
    ))
}
