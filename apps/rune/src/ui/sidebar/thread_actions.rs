use std::collections::HashMap;

use rune::color::hex;
use rune::{
    El, Key, PointerPress, PopupItem, PopupMenu, TextStyle, Tx, col, popup_menu_supported, row,
    text,
};

use crate::connection::ArchiveAction;

use super::super::{Palette, ProjectView, State, confirmation, sidebar_header};
use super::{Destination, begin_rename};

const MENU: &str = "sidebar-thread-menu";
const ARCHIVE: &str = "sidebar-archive-thread";
const RENAME: &str = "sidebar-rename-thread";
const COPY_ID: &str = "sidebar-copy-thread-id";
const CANCEL: &str = "sidebar-archive-cancel";
const CONFIRM: &str = "sidebar-archive-confirm";
const MENU_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

#[derive(Clone)]
struct Target {
    id: String,
    title: String,
    project_id: String,
    can_rename: bool,
}

#[derive(Default)]
pub(in crate::ui) struct Panel {
    menu: Option<(Target, PointerPress)>,
    confirmation: Option<Target>,
    pending_navigation: HashMap<String, String>,
}

impl Panel {
    pub(in crate::ui) fn pending_navigation(&self, thread_id: &str) -> bool {
        self.pending_navigation.contains_key(thread_id)
    }
}

impl State {
    fn can_archive_thread(&self, thread_id: &str) -> bool {
        self.connection.is_some()
            && self.settings.archive.can_archive(thread_id)
            && self.workspace.as_ref().is_some_and(|workspace| {
                !workspace.reconnecting
                    && workspace.thread(thread_id).is_some_and(|thread| {
                        !thread.archived
                            && !matches!(
                                thread.status.as_str(),
                                "preparing" | "starting" | "running"
                            )
                    })
            })
    }

    fn open_thread_actions(
        &mut self,
        id: &str,
        can_rename: bool,
        point: PointerPress,
        tx: &mut Tx,
    ) {
        let Some(thread) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.thread(id))
            .filter(|thread| !thread.archived)
        else {
            return;
        };
        let target = Target {
            id: thread.id.clone(),
            title: thread.title.clone(),
            project_id: thread.project_id.clone(),
            can_rename,
        };
        if popup_menu_supported() {
            let menu = PopupMenu::new().items([
                PopupItem::new(RENAME, "Rename thread").enabled(target.can_rename),
                PopupItem::new(COPY_ID, "Copy thread ID"),
                PopupItem::new(ARCHIVE, "Archive").enabled(self.can_archive_thread(id)),
            ]);
            tx.popup_menu(menu, point, move |state: &mut State, picked, tx| {
                if let Some(key) = picked {
                    state.thread_action(&target, key, tx);
                }
            });
        } else {
            self.thread_actions.menu = Some((target, point));
            tx.focus_when_shown(MENU, tx.by_keys());
        }
    }

    fn thread_action(&mut self, target: &Target, key: &str, tx: &mut Tx) {
        match key {
            RENAME if target.can_rename => begin_rename(self, &target.id),
            COPY_ID => tx.copy(&target.id),
            ARCHIVE if self.can_archive_thread(&target.id) => {
                if self.client_settings.confirm_thread_archive() {
                    self.thread_actions.confirmation = Some(target.clone());
                    tx.focus_when_shown(CANCEL, true);
                } else {
                    self.archive_active_thread(target, tx);
                }
            }
            _ => {}
        }
    }

    fn cancel_thread_action_menu(&mut self, tx: &mut Tx) {
        if let Some((target, _)) = self.thread_actions.menu.take() {
            tx.focus_when_shown(format!("thread-{}", target.id), tx.by_keys());
        }
    }

    fn cancel_active_archive(&mut self, tx: &mut Tx) {
        if let Some(target) = self.thread_actions.confirmation.take() {
            tx.focus_when_shown(format!("thread-{}", target.id), tx.by_keys());
        }
    }

    fn confirm_active_archive(&mut self, tx: &mut Tx) {
        if let Some(target) = self.thread_actions.confirmation.take() {
            self.archive_active_thread(&target, tx);
        }
    }

    fn archive_active_thread(&mut self, target: &Target, tx: &mut Tx) {
        if !self.can_archive_thread(&target.id) {
            return;
        }
        if active_thread(self, &target.id) {
            self.thread_actions
                .pending_navigation
                .insert(target.id.clone(), target.project_id.clone());
        }
        self.submit_archive_action(ArchiveAction::Archive, &target.id, tx);
    }
}

fn active_thread(state: &State, id: &str) -> bool {
    state
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.selection.as_ref())
        .is_some_and(
            |project| matches!(&project.view, ProjectView::Conversation(thread) if thread.id == id),
        )
}

pub(in crate::ui) fn settle(state: &mut State, id: &str, result: Result<(), String>) {
    let target = state.thread_actions.pending_navigation.remove(id);
    if let Err(error) = result {
        state.project_notice = Some(format!("Failed to archive thread. {error}"));
    } else if let Some(project) = target
        && active_thread(state, id)
    {
        state.navigate(Destination::NewThread(project));
    }
}

pub(super) fn target(thread: El, id: String, can_rename: bool) -> El {
    thread.on_secondary_press(move |state: &mut State, point, tx| {
        state.open_thread_actions(&id, can_rename, point, tx);
    })
}

pub(in crate::ui) fn menu(state: &State, width: f32, height: f32) -> Option<El> {
    let (target, point) = state.thread_actions.menu.as_ref()?;
    let menu_width = 210.0;
    let menu_height = 104.0;
    let x = point.x.clamp(0.0, (width - menu_width).max(0.0));
    let y = point.y.clamp(0.0, (height - menu_height).max(0.0));
    let canvas = hex("#fcfcfc").expect("GTK menu canvas");
    let ink = hex("#2e3436").expect("GTK menu text");
    let highlight = hex("#eaeaea").expect("GTK menu highlight");
    Some(
        col()
            .key(MENU)
            .absolute(x, y)
            .size(menu_width, menu_height)
            .py(4.0)
            .fill(canvas)
            .role("menu")
            .label("Thread actions")
            .focusable(true)
            .tab_index(-1)
            .focus_group()
            .modal_tx(true, State::cancel_thread_action_menu)
            .children(
                [
                    (RENAME, "Rename thread", target.can_rename),
                    (COPY_ID, "Copy thread ID", true),
                    (ARCHIVE, "Archive", state.can_archive_thread(&target.id)),
                ]
                .into_iter()
                .map(|(key, label, enabled)| {
                    let press = move |state: &mut State, tx: &mut Tx| {
                        if let Some((target, _)) = state.thread_actions.menu.take() {
                            state.thread_action(&target, key, tx);
                        }
                    };
                    row()
                        .key(key)
                        .role("menuitem")
                        .label(label)
                        .w_full()
                        .h(32.0)
                        .px(20.0)
                        .center_y()
                        .disabled(!enabled)
                        .focusable(enabled)
                        .focus_highlight(highlight)
                        .on_press_tx(press)
                        .on_key(move |state: &mut State, event, tx| {
                            if event.key == Key::Escape {
                                state.cancel_thread_action_menu(tx);
                                true
                            } else {
                                sidebar_header::activate_on_return(state, event, tx, press)
                            }
                        })
                        .child(text(label).font(MENU_FONT).color(if enabled {
                            ink
                        } else {
                            ink.alpha(0.45)
                        }))
                }),
            ),
    )
}

pub(in crate::ui) fn confirmation(state: &State, palette: Palette) -> Option<El> {
    let target = state.thread_actions.confirmation.as_ref()?;
    let title = format!("Archive thread \"{}\"?", target.title);
    Some(confirmation::view(
        confirmation::Dialog {
            overlay: "sidebar-archive-overlay",
            key: "sidebar-archive-dialog",
            cancel: CANCEL,
            confirm: CONFIRM,
            title: &title,
            detail: None,
            destructive: false,
        },
        palette,
        State::cancel_active_archive,
        State::confirm_active_archive,
    ))
}
