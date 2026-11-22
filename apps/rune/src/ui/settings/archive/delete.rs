use rune::{Color, El, Icon, Key, TextStyle, Tx, col, el, icon, row, text};

use crate::connection::{ArchiveAction, Thread};

use super::{Palette, State, press};

const CANCEL: &str = "archive-delete-cancel";
const CONFIRM: &str = "archive-delete-confirm";
const DESCRIPTION: &str = "archive-delete-description";
const TITLE_FONT: TextStyle = TextStyle::new(20.0, 600.0).lh(28.0);
const BODY_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const BUTTON_FONT: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);

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
    fn confirm_archive_deletion(&mut self, thread_id: &str, title: &str, tx: &mut Tx) {
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
        move |state: &mut State, tx| state.confirm_archive_deletion(&thread_id, &title, tx),
    )
}

pub(in crate::ui::settings) fn confirmation(state: &State, palette: Palette) -> Option<El> {
    let confirmation = state.settings.archive.confirmation.as_ref()?;
    let title = format!("Delete thread \"{}\"?", confirmation.title);
    let neutral = |hex| rune::color::hex(hex).expect("static dialog palette");
    let red = neutral(if palette.dark { "#fb414a" } else { "#fb2c36" });
    let scrim = if palette.dark {
        Color::BLACK.alpha(0.55)
    } else {
        Color::WHITE.alpha(0.62)
    };
    Some(
        col()
            .key("archive-delete-overlay")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .padding(16.0)
            .center()
            .hit()
            .backdrop_blur(4.0)
            .fill(scrim)
            .child(
                col()
                    .key("archive-delete-dialog")
                    .role("alertdialog")
                    .label(&title)
                    .described_by(DESCRIPTION)
                    .w_full()
                    .max_w(512.0)
                    .radius(16.0)
                    .clip_rounded(16.0)
                    .border(1.0, palette.line)
                    .border_layout()
                    .fill(palette.canvas)
                    .shadow(9.0, 24.0, Color::BLACK.alpha(0.28))
                    .hit()
                    .on_key(|state: &mut State, event, tx| {
                        if event.key != Key::Escape {
                            return false;
                        }
                        state.cancel_archive_deletion(tx);
                        true
                    })
                    .child(
                        col()
                            .w_full()
                            .px(24.0)
                            .py(24.0)
                            .gap(8.0)
                            .child(text(title).font(TITLE_FONT).color(palette.ink).wrap())
                            .child(
                                text(
                                    "This permanently clears conversation history for this thread.",
                                )
                                .key(DESCRIPTION)
                                .font(BODY_FONT)
                                .color(palette.muted)
                                .wrap(),
                            ),
                    )
                    .child(
                        row()
                            .w_full()
                            .h(64.0)
                            .px(24.0)
                            .items_center()
                            .justify_end()
                            .gap(8.0)
                            .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
                            .fill(if palette.dark {
                                palette.sidebar.mix(palette.canvas, 0.28)
                            } else {
                                neutral("#f8f8f8")
                            })
                            .child(press(
                                dialog_button(
                                    CANCEL,
                                    "Cancel",
                                    70.0,
                                    palette.field,
                                    palette.ink,
                                    palette.line,
                                    palette,
                                ),
                                State::cancel_archive_deletion,
                            ))
                            .child(press(
                                dialog_button(
                                    CONFIRM,
                                    "Confirm",
                                    78.0,
                                    red,
                                    Color::WHITE,
                                    red,
                                    palette,
                                ),
                                State::commit_archive_deletion,
                            )),
                    ),
            ),
    )
}

fn dialog_button(
    key: &str,
    label: &str,
    width: f32,
    fill: Color,
    ink: Color,
    border: Color,
    palette: Palette,
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .size(width, 32.0)
        .radius(8.0)
        .center()
        .fill(fill)
        .border(1.0, border)
        .hover(fill.mix(ink, 0.10))
        .focusable(true)
        .focus_ring(1.0, 2.0, palette.accent)
        .child(text(label).font(BUTTON_FONT).color(ink))
}
