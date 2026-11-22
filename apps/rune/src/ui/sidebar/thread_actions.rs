use std::collections::HashMap;

use rune::color::hex;
use rune::{
    Color, El, Key, PointerPress, PopupItem, PopupMenu, TextStyle, Tx, col, popup_menu_supported,
    row, text,
};

use crate::connection::ArchiveAction;

use super::super::{Palette, ProjectView, State, confirmation, sidebar_header};
use super::{Destination, begin_rename, project_scope};

pub(in crate::ui) mod undo;
pub(in crate::ui) mod worktree;

const MENU: &str = "sidebar-thread-menu";
const ARCHIVE: &str = "sidebar-archive-thread";
const DELETE: &str = "sidebar-delete-thread";
const RENAME: &str = "sidebar-rename-thread";
const COPY: &str = "sidebar-copy-thread-menu";
const COPY_PATH: &str = "sidebar-copy-thread-path";
const COPY_BRANCH: &str = "sidebar-copy-branch";
const COPY_THREAD: &str = "sidebar-copy-thread-reference";
const COPY_ID: &str = "sidebar-copy-thread-id";
const FILTER_PROJECT: &str = "sidebar-filter-thread-project";
const PROJECT_SETTINGS: &str = "sidebar-thread-project-settings";
const CANCEL: &str = "sidebar-archive-cancel";
const CONFIRM: &str = "sidebar-archive-confirm";
const MENU_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const MENU_ROW_HEIGHT: f32 = 32.0;
const MENU_PADDING: f32 = 4.0;

#[derive(Clone)]
struct Target {
    id: String,
    title: String,
    project_id: String,
    can_rename: bool,
    workspace_path: Option<String>,
    worktree_path: Option<String>,
    branch: Option<String>,
    environment: Option<String>,
}

#[derive(Default)]
pub(in crate::ui) struct Panel {
    menu: Option<(Target, PointerPress)>,
    copy_menu_open: bool,
    confirmation: Option<(Target, ArchiveAction)>,
    pending_archive: HashMap<String, Option<String>>,
    pending_delete: HashMap<String, Deletion>,
    worktree: worktree::Panel,
    pub(in crate::ui) undo: undo::Notice,
}

struct Deletion {
    destination: Option<Destination>,
    checkout: Option<worktree::Checkout>,
}

impl Panel {
    pub(in crate::ui) fn pending_navigation(&self, thread_id: &str) -> bool {
        self.pending_delete
            .get(thread_id)
            .is_some_and(|deletion| deletion.destination.is_some())
            || self
                .pending_archive
                .get(thread_id)
                .is_some_and(Option::is_some)
    }
}

impl State {
    fn can_delete_active_thread(&self, id: &str) -> bool {
        self.connection.is_some()
            && !self.thread_actions.worktree.checking(id)
            && self.settings.archive.can_delete(id)
            && self
                .workspace
                .as_ref()
                .filter(|workspace| !workspace.reconnecting)
                .and_then(|workspace| workspace.thread(id))
                .is_some_and(|thread| {
                    !thread.archived
                        && !matches!(
                            thread.status.as_str(),
                            "preparing" | "queued" | "starting" | "running" | "waiting"
                        )
                })
    }

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
        let Some(workspace) = &self.workspace else {
            return;
        };
        let Some(thread) = workspace.thread(id).filter(|thread| !thread.archived) else {
            return;
        };
        let workspace_path = thread.worktree_path.clone().or_else(|| {
            workspace
                .shell
                .as_ref()?
                .projects
                .iter()
                .find(|project| project.id == thread.project_id)
                .map(|project| project.path.clone())
        });
        let target = Target {
            id: thread.id.clone(),
            title: thread.title.clone(),
            project_id: thread.project_id.clone(),
            can_rename,
            workspace_path,
            worktree_path: thread.worktree_path.clone(),
            branch: thread.branch.clone().filter(|branch| !branch.is_empty()),
            environment: self.draft_environment.clone(),
        };
        if popup_menu_supported() {
            let menu = self.thread_menu_items(&target).into_iter().fold(
                PopupMenu::new(),
                |menu, (key, label, enabled)| {
                    if key == COPY {
                        let copy = PopupMenu::new().items(copy_items(&target).into_iter().map(
                            |(key, label, enabled)| PopupItem::new(key, label).enabled(enabled),
                        ));
                        menu.submenu(label, copy)
                    } else {
                        menu.item(PopupItem::new(key, label).enabled(enabled))
                    }
                },
            );
            tx.popup_menu(menu, point, move |state: &mut State, picked, tx| {
                if let Some(key) = picked {
                    state.thread_action(&target, key, tx);
                }
            });
        } else {
            self.thread_actions.copy_menu_open = false;
            self.thread_actions.menu = Some((target, point));
            tx.focus_when_shown(MENU, tx.by_keys());
        }
    }

    fn thread_menu_items(&self, target: &Target) -> Vec<(&'static str, String, bool)> {
        let mut items = vec![(RENAME, "Rename thread".into(), target.can_rename)];
        if let Some(label) = project_scope::thread_filter_label(self, &target.project_id) {
            items.push((FILTER_PROJECT, label, true));
        }
        items.extend([
            (COPY, "Copy".into(), true),
            (PROJECT_SETTINGS, "Project settings".into(), true),
            (
                ARCHIVE,
                "Archive".into(),
                self.can_archive_thread(&target.id),
            ),
            (
                DELETE,
                "Delete".into(),
                self.can_delete_active_thread(&target.id),
            ),
        ]);
        items
    }

    fn thread_action(&mut self, target: &Target, key: &str, tx: &mut Tx) {
        match key {
            RENAME if target.can_rename => begin_rename(self, &target.id),
            COPY_PATH => {
                if let Some(path) = &target.workspace_path {
                    tx.copy(path);
                }
            }
            COPY_BRANCH => {
                if let Some(branch) = &target.branch {
                    tx.copy(branch);
                }
            }
            COPY_THREAD => {
                if let Some(environment) = &target.environment {
                    tx.copy_rich(crate::composer_context::thread_clipboard::thread_clipboard(
                        environment,
                        &target.id,
                        &target.title,
                    ));
                }
            }
            COPY_ID => tx.copy(&target.id),
            FILTER_PROJECT => project_scope::toggle_for_thread(self, &target.project_id, tx),
            PROJECT_SETTINGS => self.open_project_settings(&target.project_id, tx),
            ARCHIVE if self.can_archive_thread(&target.id) => {
                self.confirm_or_dispatch(target, ArchiveAction::Archive, tx);
            }
            DELETE if self.can_delete_active_thread(&target.id) => {
                self.confirm_or_dispatch(target, ArchiveAction::DeleteActive, tx);
            }
            _ => {}
        }
    }

    fn cancel_thread_action_menu(&mut self, tx: &mut Tx) {
        if let Some((target, _)) = self.thread_actions.menu.take() {
            tx.focus_when_shown(format!("thread-{}", target.id), tx.by_keys());
        }
    }

    fn confirm_or_dispatch(&mut self, target: &Target, action: ArchiveAction, tx: &mut Tx) {
        let confirm = if action == ArchiveAction::DeleteActive {
            self.client_settings.confirm_thread_delete()
        } else {
            self.client_settings.confirm_thread_archive()
        };
        if confirm {
            self.thread_actions.confirmation = Some((target.clone(), action));
            tx.focus_when_shown(CANCEL, true);
        } else {
            self.dispatch_thread_action(target, action, tx);
        }
    }

    fn cancel_thread_confirmation(&mut self, tx: &mut Tx) {
        if let Some((target, _)) = self.thread_actions.confirmation.take() {
            tx.focus_when_shown(format!("thread-{}", target.id), tx.by_keys());
        }
    }

    fn confirm_thread_action(&mut self, tx: &mut Tx) {
        if let Some((target, action)) = self.thread_actions.confirmation.take() {
            self.dispatch_thread_action(&target, action, tx);
        }
    }

    fn dispatch_thread_action(&mut self, target: &Target, action: ArchiveAction, tx: &mut Tx) {
        match action {
            ArchiveAction::Archive => self.archive_active_thread(target, tx),
            ArchiveAction::DeleteActive
                if self.can_delete_active_thread(&target.id)
                    && !worktree::inspect(self, target) =>
            {
                self.delete_active_thread(target, None, tx);
            }
            _ => {}
        }
    }

    fn delete_active_thread(
        &mut self,
        target: &Target,
        checkout: Option<worktree::Checkout>,
        tx: &mut Tx,
    ) {
        if self.can_delete_active_thread(&target.id)
            && self.submit_archive_action(ArchiveAction::DeleteActive, &target.id, tx)
        {
            let destination =
                active_thread(self, &target.id).then(|| delete_fallback(self, target));
            self.thread_actions.pending_delete.insert(
                target.id.clone(),
                Deletion {
                    destination,
                    checkout,
                },
            );
        }
    }

    fn archive_active_thread(&mut self, target: &Target, tx: &mut Tx) {
        if !self.can_archive_thread(&target.id) {
            return;
        }
        if self.submit_archive_action(ArchiveAction::Archive, &target.id, tx) {
            let reopen = active_thread(self, &target.id).then(|| target.project_id.clone());
            self.thread_actions
                .pending_archive
                .insert(target.id.clone(), reopen);
        }
    }
}

fn copy_items(target: &Target) -> Vec<(&'static str, &'static str, bool)> {
    let mut items = vec![(COPY_PATH, "Path", target.workspace_path.is_some())];
    if target.branch.is_some() {
        items.push((COPY_BRANCH, "Branch", true));
    }
    items.push((COPY_THREAD, "Thread", target.environment.is_some()));
    items.push((COPY_ID, "Thread ID", true));
    items
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

pub(in crate::ui) fn settle_archive(
    state: &mut State,
    id: &str,
    result: Result<(), String>,
    tx: &mut Tx,
) {
    let Some(reopen) = state.thread_actions.pending_archive.remove(id) else {
        return;
    };
    if let Err(error) = result {
        state.project_notice = Some(format!("Failed to archive thread. {error}"));
        return;
    }
    state.thread_actions.undo.archived(id, reopen.clone(), tx);
    undo::announce_archive(state, tx);
    if let Some(project) = reopen
        && active_thread(state, id)
    {
        state.navigate(Destination::NewThread(project));
    }
}

pub(in crate::ui) use undo::settle as settle_undo;

fn delete_fallback(state: &State, target: &Target) -> Destination {
    let Some(shell) = state
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.shell.as_ref())
    else {
        return Destination::Home;
    };
    let now = time::OffsetDateTime::now_utc();
    let threads = shell.threads.iter().filter(|thread| {
        thread.project_id == target.project_id
            && thread.id != target.id
            && !thread.archived
            && !thread.is_settled()
            && !thread.is_snoozed(now)
            && !state.thread_actions.pending_delete.contains_key(&thread.id)
    });
    crate::connection::latest_thread_by_order(
        threads,
        state.client_settings.threads_ordered_by_creation(),
    )
    .map_or(Destination::Home, |thread| Destination::Thread {
        project: target.project_id.clone(),
        thread: thread.id.clone(),
    })
}

pub(in crate::ui) fn settle_delete(state: &mut State, id: &str, result: Result<(), String>) {
    let Some(deletion) = state.thread_actions.pending_delete.remove(id) else {
        return;
    };
    if let Err(error) = result {
        state.project_notice = Some(format!("Failed to delete thread. {error}"));
        return;
    }
    state.thread_actions.undo.invalidate(id);
    if let Some(shell) = state
        .workspace
        .as_mut()
        .and_then(|workspace| workspace.shell.as_mut())
    {
        shell.threads.retain(|thread| thread.id != id);
    }
    if let Some(fallback) = deletion.destination
        && active_thread(state, id)
        && !show_delete_fallback(state, fallback)
    {
        state.show_destination(Destination::Home);
    }
    if let Some(checkout) = deletion.checkout {
        worktree::remove(state, checkout);
    }
}

fn show_delete_fallback(state: &mut State, fallback: Destination) -> bool {
    if let Destination::Thread { thread, .. } = &fallback
        && !state
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.thread(thread))
            .is_some_and(|candidate| !candidate.archived)
    {
        return false;
    }
    state.show_destination(fallback)
}

pub(super) fn target(thread: El, id: String, can_rename: bool) -> El {
    thread.on_secondary_press(move |state: &mut State, point, tx| {
        state.open_thread_actions(&id, can_rename, point, tx);
    })
}

pub(in crate::ui) fn menu(state: &State, width: f32, height: f32) -> Option<El> {
    let (target, point) = state.thread_actions.menu.as_ref()?;
    let items = state.thread_menu_items(target);
    let menu_width = 210.0;
    let menu_height = items.len() as f32 * MENU_ROW_HEIGHT + MENU_PADDING * 2.0;
    let x = point.x.clamp(0.0, (width - menu_width).max(0.0));
    let y = point.y.clamp(0.0, (height - menu_height).max(0.0));
    let canvas = hex("#fcfcfc").expect("GTK menu canvas");
    let ink = hex("#2e3436").expect("GTK menu text");
    let highlight = hex("#eaeaea").expect("GTK menu highlight");
    let copy_geometry = state.thread_actions.copy_menu_open.then(|| {
        let submenu_width = 178.0;
        let copy_items = copy_items(target);
        let submenu_height = copy_items.len() as f32 * MENU_ROW_HEIGHT + MENU_PADDING * 2.0;
        let index = items
            .iter()
            .position(|(key, _, _)| *key == COPY)
            .unwrap_or(0);
        let column = if x + menu_width + submenu_width <= width {
            x + menu_width - 1.0
        } else {
            (x - submenu_width + 1.0).max(0.0)
        };
        let top = (y + index as f32 * MENU_ROW_HEIGHT).min((height - submenu_height).max(0.0));
        ([column, top, submenu_width, submenu_height], copy_items)
    });
    let [left, top, right, bottom] = copy_geometry.as_ref().map_or(
        [x, y, x + menu_width, y + menu_height],
        |([column, top, copy_width, copy_height], _)| {
            [
                x.min(*column),
                y.min(*top),
                (x + menu_width).max(column + copy_width),
                (y + menu_height).max(top + copy_height),
            ]
        },
    );
    let copy_menu = copy_geometry.map(|([column, copy_y, copy_width, copy_height], copy_items)| {
        col()
            .key("sidebar-copy-submenu")
            .absolute(column - left, copy_y - top)
            .size(copy_width, copy_height)
            .py(MENU_PADDING)
            .fill(canvas)
            .role("menu")
            .label("Copy")
            .children(copy_items.into_iter().map(|(key, label, enabled)| {
                menu_row(key, label.into(), enabled, [ink, highlight], true)
            }))
    });
    Some(
        col()
            .key(MENU)
            .absolute(left, top)
            .size(right - left, bottom - top)
            .role("menu")
            .label("Thread actions")
            .focusable(true)
            .tab_index(-1)
            .focus_group()
            .modal_tx(true, State::cancel_thread_action_menu)
            .child(
                col()
                    .absolute(x - left, y - top)
                    .size(menu_width, menu_height)
                    .py(MENU_PADDING)
                    .fill(canvas)
                    .children(items.into_iter().map(|(key, label, enabled)| {
                        menu_row(key, label, enabled, [ink, highlight], false)
                    })),
            )
            .children(copy_menu),
    )
}

fn menu_row(
    key: &'static str,
    label: String,
    enabled: bool,
    [ink, highlight]: [Color; 2],
    submenu: bool,
) -> El {
    let press = move |state: &mut State, tx: &mut Tx| {
        if key == COPY {
            state.thread_actions.copy_menu_open = true;
            if tx.by_keys() {
                let first = match state.thread_actions.menu.as_ref() {
                    Some((target, _)) if target.workspace_path.is_some() => COPY_PATH,
                    Some((target, _)) if target.branch.is_some() => COPY_BRANCH,
                    _ => COPY_ID,
                };
                tx.focus_when_shown(first, true);
            }
        } else if let Some((target, _)) = state.thread_actions.menu.take() {
            state.thread_action(&target, key, tx);
        }
    };
    row()
        .key(key)
        .role("menuitem")
        .label(&label)
        .w_full()
        .h(MENU_ROW_HEIGHT)
        .px(20.0)
        .center_y()
        .disabled(!enabled)
        .focusable(enabled)
        .focus_highlight(highlight)
        .on_hover(move |state: &mut State| {
            if !submenu {
                state.thread_actions.copy_menu_open = key == COPY;
            }
        })
        .on_press_tx(press)
        .on_key(move |state: &mut State, event, tx| match event.key {
            Key::Escape | Key::Left if submenu => {
                state.thread_actions.copy_menu_open = false;
                tx.focus(COPY, true);
                true
            }
            Key::Escape => {
                state.cancel_thread_action_menu(tx);
                true
            }
            Key::Right if key == COPY => {
                press(state, tx);
                true
            }
            _ => sidebar_header::activate_on_return(state, event, tx, press),
        })
        .child(
            text(label)
                .font(MENU_FONT)
                .color(if enabled { ink } else { ink.alpha(0.45) })
                .grow(1.0),
        )
        .children((key == COPY).then(|| text("›").font(MENU_FONT).color(ink)))
}

pub(in crate::ui) fn confirmation(state: &State, palette: Palette) -> Option<El> {
    if let Some(dialog) = worktree::view(state, palette) {
        return Some(dialog);
    }
    let (target, action) = state.thread_actions.confirmation.as_ref()?;
    let destructive = *action == ArchiveAction::DeleteActive;
    let verb = if destructive { "Delete" } else { "Archive" };
    let title = format!("{verb} thread \"{}\"?", target.title);
    Some(confirmation::view(
        confirmation::Dialog {
            overlay: "sidebar-archive-overlay",
            key: "sidebar-archive-dialog",
            cancel: CANCEL,
            confirm: CONFIRM,
            title: &title,
            detail: destructive.then_some((
                "sidebar-delete-description",
                "This permanently clears conversation history for this thread.",
            )),
            destructive,
        },
        palette,
        State::cancel_thread_confirmation,
        State::confirm_thread_action,
    ))
}
