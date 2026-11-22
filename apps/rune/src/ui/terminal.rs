use std::time::Instant;

use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{TermMode, cell::Flags, color::Colors},
    vte::ansi::{Color as TerminalColor, CursorShape, NamedColor},
};
use rune::{Color, Drag, El, Family, Key, KeyEvent, Phase, Span, TextStyle, Tx, col, el, text};

use crate::connection::{
    TerminalAttach, TerminalSnapshot, TerminalStatus, TerminalTarget, TerminalUpdate,
};

use super::{Palette, ProjectSelection, ProjectView, SMALL, State, conversation::Layout};

mod control;
mod history;
mod links;
mod menu;
mod mouse;
mod mouse_modes;
mod selection;
mod tabs;
mod view;

pub(super) use mouse::primary_paste_enabled;

use control::TerminalScreen;
pub(super) use menu::view as terminal_menu;
use tabs::SplitDirection;
pub(super) use tabs::TabsByThread;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 16;
const DEFAULT_DRAWER_HEIGHT: f32 = 280.0;
const MIN_DRAWER_HEIGHT: f32 = 180.0;
const LINE_HEIGHT: f32 = 18.0;
const CELL_WIDTH: f32 = 7.801_269_5;
const CELL_PADDING: f32 = 4.0;
const TERMINAL_FONT: Family = if cfg!(target_os = "linux") {
    Family::Named("Liberation Mono, DejaVu Sans Mono")
} else {
    Family::SystemMono
};
const FONT: TextStyle = TextStyle::new(13.0, 400.0)
    .lh(LINE_HEIGHT)
    .family(TERMINAL_FONT);
pub(super) const INPUT_KEY: &str = "terminal-input";

pub(super) struct Pane {
    pub(super) attachment: u64,
    pub(super) thread_id: String,
    terminal_id: String,
    screen: TerminalScreen,
    requested_grid: (u16, u16),
    status: TerminalStatus,
    label: String,
    has_running_subprocess: bool,
    focused: bool,
    message: Option<String>,
    received_snapshot: bool,
    close_prompt: Option<ClosePrompt>,
    menu: Option<menu::Menu>,
    mouse: mouse::Mouse,
    links: links::Links,
    resize_preview: Option<f32>,
    wheel_remainder: f32,
    viewport_height: f32,
}

struct ClosePrompt {
    target: TerminalTarget,
    label: String,
}

impl Pane {
    fn new(attachment: u64, thread_id: String, terminal_id: String) -> Self {
        Self {
            attachment,
            thread_id,
            terminal_id,
            screen: TerminalScreen::new(DEFAULT_ROWS, DEFAULT_COLS),
            requested_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            status: TerminalStatus::Starting,
            label: "Shell".into(),
            has_running_subprocess: false,
            focused: false,
            message: None,
            received_snapshot: false,
            close_prompt: None,
            menu: None,
            mouse: mouse::Mouse::default(),
            links: links::Links::default(),
            resize_preview: None,
            wheel_remainder: 0.0,
            viewport_height: 0.0,
        }
    }

    pub(super) fn apply(&mut self, update: TerminalUpdate) {
        match update {
            TerminalUpdate::Snapshot { snapshot } | TerminalUpdate::Restarted { snapshot } => {
                self.snapshot(snapshot)
            }
            TerminalUpdate::Output { data } => {
                self.screen.process(data.as_bytes());
                self.mouse
                    .sync_mode(self.screen.mouse_modes.protocol().tracking);
            }
            TerminalUpdate::Activity {
                cols,
                rows,
                label,
                has_running_subprocess,
            } => {
                self.label = label;
                self.has_running_subprocess = has_running_subprocess;
                if let (Some(rows), Some(cols)) = (rows, cols) {
                    self.screen.resize(rows, cols);
                }
            }
            TerminalUpdate::Exited {
                exit_code,
                exit_signal,
            } => {
                self.status = TerminalStatus::Exited;
                self.message = Some(exit_message(exit_code, exit_signal));
            }
            TerminalUpdate::Cleared => {
                self.screen.process(b"\x1b[H\x1b[2J\x1b[3J");
            }
            TerminalUpdate::Closed => {
                self.status = TerminalStatus::Exited;
                self.message = Some("Terminal closed".into());
            }
            TerminalUpdate::Error { message } => self.fail(message),
        }
    }

    pub(super) fn fail(&mut self, message: String) {
        self.status = TerminalStatus::Error;
        self.message = Some(message);
    }

    fn snapshot(&mut self, snapshot: TerminalSnapshot) {
        self.received_snapshot = true;
        let (cols, rows) = self.requested_grid;
        self.screen =
            TerminalScreen::new(snapshot.rows.unwrap_or(rows), snapshot.cols.unwrap_or(cols));
        self.wheel_remainder = 0.0;
        self.mouse = mouse::Mouse::default();
        self.links = links::Links::default();
        self.screen.process(snapshot.history.as_bytes());
        self.mouse
            .sync_mode(self.screen.mouse_modes.protocol().tracking);
        self.screen.take_replies();
        self.status = snapshot.status;
        self.label = snapshot.label;
        self.message = snapshot.error.or_else(|| {
            (snapshot.status == TerminalStatus::Exited)
                .then(|| exit_message(snapshot.exit_code, snapshot.exit_signal))
        });
    }
}

fn exit_message(code: Option<i64>, signal: Option<i32>) -> String {
    if let Some(signal) = signal {
        format!("Process stopped by signal {signal}")
    } else if let Some(code) = code {
        format!("Process exited with code {code}")
    } else {
        "Process exited".into()
    }
}

impl State {
    pub(super) fn terminal_window_focus(&mut self, focused: bool, tx: &mut Tx) {
        self.window_focused = focused;
        self.update_terminal_focus(tx);
    }

    pub(super) fn update_terminal_focus(&mut self, tx: &Tx) {
        let focus = self.window_focused && tx.focused() == Some(INPUT_KEY);
        let can_report = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.reconnecting);
        for (terminal, focused) in self
            .terminal
            .iter_mut()
            .map(|terminal| (terminal, focus))
            .chain(
                self.terminal_peers
                    .iter_mut()
                    .map(|terminal| (terminal, false)),
            )
        {
            if std::mem::replace(&mut terminal.focused, focused) != focused
                && terminal.status == TerminalStatus::Running
                && terminal.screen.term.mode().contains(TermMode::FOCUS_IN_OUT)
                && can_report
                && let Some(connection) = &self.connection
            {
                connection.write_terminal(
                    terminal.attachment,
                    if focused { "\x1b[I" } else { "\x1b[O" }.into(),
                    false,
                    false,
                );
            }
        }
    }

    fn terminal_attachment_mut(&mut self, attachment: u64) -> Option<&mut Pane> {
        self.terminal
            .iter_mut()
            .chain(&mut self.terminal_peers)
            .find(|pane| pane.attachment == attachment)
    }

    pub(super) fn apply_terminal_update(&mut self, attachment: u64, update: TerminalUpdate) {
        let Some(terminal) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        terminal.apply(update);
        let replies = terminal.screen.take_replies();
        if !replies.is_empty()
            && let Some(connection) = &self.connection
        {
            connection.write_terminal(attachment, replies, false, true);
        }
    }

    pub(super) fn poll_terminal_output(&mut self, tx: &mut Tx) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for pane in self.terminal.iter_mut().chain(&mut self.terminal_peers) {
            let Some(deadline) = pane.screen.synchronized_until() else {
                continue;
            };
            if deadline > now {
                tx.wake_after(deadline - now);
                continue;
            }
            pane.screen.finish_synchronized_update();
            pane.mouse
                .sync_mode(pane.screen.mouse_modes.protocol().tracking);
            let replies = pane.screen.take_replies();
            if !replies.is_empty()
                && let Some(connection) = &self.connection
            {
                connection.write_terminal(pane.attachment, replies, false, true);
            }
            changed = true;
        }
        changed
    }

    pub(super) fn fail_terminal_attachment(&mut self, attachment: u64, message: String) {
        if let Some(terminal) = self.terminal_attachment_mut(attachment) {
            terminal.fail(message);
        }
    }

    pub(super) fn toggle_terminal(&mut self, tx: &mut Tx) {
        if let Some(terminal) = &self.terminal {
            let thread_id = terminal.thread_id.clone();
            self.detach_terminal_pane();
            self.persist_terminal_layout(&thread_id, false);
            return;
        }
        self.attach_selected_terminal(tx, true);
    }

    pub(super) fn restore_terminal_for_selection(&mut self, tx: &mut Tx) -> bool {
        if self.terminal.is_some() {
            return false;
        }
        let should_restore = self.workspace.as_ref().is_some_and(|workspace| {
            let Some(ProjectView::Conversation(conversation)) = workspace
                .selection
                .as_ref()
                .map(|selection| &selection.view)
            else {
                return false;
            };
            matches!(conversation.transcript, super::Transcript::Loaded(_))
                && self
                    .draft_environment
                    .as_deref()
                    .is_some_and(|environment| {
                        self.client_settings
                            .terminal_layout(environment, &conversation.id)
                            .is_some_and(|layout| layout.terminal_open)
                    })
        });
        if should_restore {
            self.attach_selected_terminal(tx, false);
        }
        self.terminal.is_some()
    }

    fn persist_terminal_layout(&mut self, thread_id: &str, open: bool) {
        if let Some(environment) = self.draft_environment.as_deref()
            && let Err(error) = self.client_settings.set_terminal_layout(
                environment,
                thread_id,
                self.terminal_tabs.snapshot(thread_id, open),
            )
        {
            self.project_notice = Some(format!("Could not save terminal layout. {error}"));
        }
    }

    pub(super) fn open_worktree_setup_terminal(
        &mut self,
        thread_id: &str,
        terminal_id: &str,
        tx: &mut Tx,
    ) {
        let Some(script) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.terminal && !workspace.reconnecting)
            .and_then(|workspace| workspace.selection.as_ref())
            .and_then(|selection| match &selection.view {
                ProjectView::Conversation(conversation) if conversation.id == thread_id => {
                    conversation.worktree_setup.as_ref()
                }
                _ => None,
            })
            .and_then(|setup| setup.setup_script.as_ref())
            .filter(|script| script.terminal_id == terminal_id)
        else {
            return;
        };
        if self
            .terminal
            .as_ref()
            .is_some_and(|pane| pane.thread_id == thread_id && pane.terminal_id == terminal_id)
        {
            tx.focus(INPUT_KEY, false);
            return;
        }
        let target = TerminalTarget {
            thread_id: thread_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
        };
        let label = script.name.clone();
        self.restore_terminal_tabs(thread_id);
        if self.terminal_tabs.select_observed(&target, &label) {
            self.attach_selected_terminal(tx, true);
        }
    }

    fn restore_terminal_tabs(&mut self, thread_id: &str) {
        if let Some(environment) = self.draft_environment.as_deref()
            && let Some(layout) = self.client_settings.terminal_layout(environment, thread_id)
        {
            self.terminal_tabs.restore(thread_id, layout);
        }
    }

    fn attach_selected_terminal(&mut self, tx: &mut Tx, focus: bool) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.terminal && !workspace.reconnecting)
        else {
            return;
        };
        let Some(ProjectSelection {
            id: project_id,
            view: ProjectView::Conversation(conversation),
        }) = &workspace.selection
        else {
            return;
        };
        let Some(thread) = workspace.thread(&conversation.id) else {
            return;
        };
        let Some(project) = workspace.shell.as_ref().and_then(|shell| {
            shell
                .projects
                .iter()
                .find(|project| &project.id == project_id)
        }) else {
            return;
        };
        if self.connection.is_none() {
            return;
        }
        let thread_id = thread.id.clone();
        let worktree = thread.worktree_path.clone();
        let cwd = worktree.as_ref().unwrap_or(&project.path).clone();
        self.restore_terminal_tabs(&thread_id);
        let tabs = self.terminal_tabs.ensure(&thread_id);
        let Some(group) = tabs.active_group() else {
            return;
        };
        let selected = tabs.selected().id.clone();
        let targets: Vec<_> = group
            .members
            .iter()
            .filter_map(|id| tabs.tab(id))
            .map(|tab| TerminalAttach {
                target: TerminalTarget {
                    thread_id: thread_id.clone(),
                    terminal_id: tab.id.clone(),
                },
                cwd: tab.observed_label.is_none().then(|| cwd.clone()),
                worktree_path: worktree.clone(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                resume: tab.resume,
            })
            .collect();
        let mut visible = Vec::with_capacity(targets.len());
        for pane in self
            .terminal
            .take()
            .into_iter()
            .chain(std::mem::take(&mut self.terminal_peers))
        {
            if pane.thread_id == thread_id
                && targets
                    .iter()
                    .any(|target| target.target.terminal_id == pane.terminal_id)
            {
                visible.push(pane);
            } else {
                self.detach_one_terminal(pane);
            }
        }
        let Some(connection) = &self.connection else {
            return;
        };
        for target in targets {
            if visible
                .iter()
                .any(|pane| pane.terminal_id == target.target.terminal_id)
            {
                continue;
            }
            let id = target.target.terminal_id.clone();
            let attachment = connection.attach_terminal(target);
            visible.push(Pane::new(attachment, thread_id.clone(), id));
        }
        if let Some(index) = visible.iter().position(|pane| pane.terminal_id == selected) {
            self.terminal = Some(visible.swap_remove(index));
            self.terminal_peers = visible;
            if focus {
                tx.focus_when_shown(INPUT_KEY, false);
            }
            self.persist_terminal_layout(&thread_id, true);
        }
    }

    pub(super) fn detach_terminal_pane(&mut self) {
        for pane in self
            .terminal
            .take()
            .into_iter()
            .chain(std::mem::take(&mut self.terminal_peers))
        {
            self.detach_one_terminal(pane);
        }
    }

    fn detach_one_terminal(&mut self, terminal: Pane) {
        if let Some(tabs) = self.terminal_tabs.get_mut(&terminal.thread_id) {
            tabs.remember(&terminal);
        }
        if let Some(connection) = &self.connection {
            if terminal.focused
                && terminal.status == TerminalStatus::Running
                && terminal.screen.term.mode().contains(TermMode::FOCUS_IN_OUT)
                && self
                    .workspace
                    .as_ref()
                    .is_some_and(|workspace| !workspace.reconnecting)
            {
                connection.write_terminal(terminal.attachment, "\x1b[O".into(), false, false);
            }
            connection.detach_terminal(terminal.attachment);
        }
    }

    fn terminal_tab_actions_enabled(&self) -> bool {
        self.terminal.is_some()
            && self.connection.is_some()
            && self
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.terminal && !workspace.reconnecting)
    }

    fn new_terminal_tab(&mut self, tx: &mut Tx) {
        if !self.terminal_tab_actions_enabled() {
            return;
        }
        if let Some(pane) = &self.terminal
            && self.terminal_tabs.append(&pane.thread_id)
        {
            self.attach_selected_terminal(tx, true);
        }
    }

    fn split_terminal(&mut self, direction: SplitDirection, tx: &mut Tx) {
        if self.terminal_tab_actions_enabled()
            && let Some(pane) = &self.terminal
            && self.terminal_tabs.split(&pane.thread_id, direction)
        {
            self.attach_selected_terminal(tx, true);
        }
    }

    fn activate_terminal_attachment(&mut self, attachment: u64, tx: &mut Tx) {
        if let Some(id) = self
            .terminal_attachment_mut(attachment)
            .map(|pane| pane.terminal_id.clone())
        {
            self.select_terminal_tab(&id, tx);
        }
    }

    fn select_terminal_tab(&mut self, id: &str, tx: &mut Tx) {
        if !self.terminal_tab_actions_enabled() {
            return;
        }
        if let Some(pane) = &self.terminal
            && let Some(tabs) = self.terminal_tabs.get_mut(&pane.thread_id)
        {
            if tabs.select(id) {
                self.attach_selected_terminal(tx, true);
            } else if pane.terminal_id == id {
                tx.focus(INPUT_KEY, false);
            }
        }
    }

    fn request_selected_terminal_close(&mut self, tx: &mut Tx) {
        if let Some(id) = self.terminal.as_ref().map(|pane| pane.terminal_id.clone()) {
            self.request_terminal_close(&id, tx);
        }
    }

    fn request_terminal_close(&mut self, id: &str, tx: &mut Tx) {
        if !self.terminal_tab_actions_enabled() {
            return;
        }
        if let Some(pane) = &mut self.terminal
            && pane.close_prompt.is_none()
            && let Some(tab) = self
                .terminal_tabs
                .get(&pane.thread_id)
                .and_then(|tabs| tabs.tab(id))
        {
            pane.close_prompt = Some(ClosePrompt {
                target: TerminalTarget {
                    thread_id: pane.thread_id.clone(),
                    terminal_id: id.to_owned(),
                },
                label: tab.label(),
            });
            if self.client_settings.confirm_terminal_close() {
                tx.focus(view::CANCEL_CLOSE_KEY, false);
            } else {
                self.confirm_terminal_close(tx);
            }
        }
    }

    fn cancel_terminal_close(&mut self, tx: &mut Tx) {
        if let Some(terminal) = &mut self.terminal {
            terminal.close_prompt = None;
            tx.focus(INPUT_KEY, false);
        }
    }

    fn confirm_terminal_close(&mut self, tx: &mut Tx) {
        let enabled = self.terminal_tab_actions_enabled();
        if let Some(pane) = &mut self.terminal
            && let Some(prompt) = pane.close_prompt.take()
        {
            tx.focus(INPUT_KEY, false);
            if !enabled || pane.thread_id != prompt.target.thread_id {
                return;
            }
            let closing_selected = pane.terminal_id == prompt.target.terminal_id;
            let thread_id = pane.thread_id.clone();
            let Some(connection) = &self.connection else {
                return;
            };
            if !self.terminal_tabs.begin_close(prompt.target.clone()) {
                return;
            }
            connection.close_terminal(prompt.target, true);
            if closing_selected {
                pane.focused = false;
            }
            if self.terminal_tabs.get(&thread_id).is_some() {
                self.attach_selected_terminal(tx, true);
            } else {
                self.detach_terminal_pane();
                self.persist_terminal_layout(&thread_id, false);
            }
        }
    }

    pub(super) fn terminal_close_settled(
        &mut self,
        target: TerminalTarget,
        result: Result<(), String>,
    ) {
        if self.terminal_tabs.settle_close(&target)
            && let Err(message) = result
            && let Some(pane) = &mut self.terminal
            && pane.thread_id == target.thread_id
        {
            pane.message = Some(message);
        }
    }

    pub(super) fn terminal_close_confirmation_open(&self) -> bool {
        self.terminal
            .as_ref()
            .is_some_and(|pane| pane.close_prompt.is_some())
    }

    pub(super) fn terminal_close_dialog(&self, palette: Palette) -> Option<El> {
        let prompt = self.terminal.as_ref()?.close_prompt.as_ref()?;
        Some(view::close_dialog(&prompt.label, palette))
    }

    pub(super) fn terminal_view(&self, terminal: &Pane, layout: Layout, palette: Palette) -> El {
        let window_height = layout.viewport[1];
        let conversation_budget =
            (window_height - layout.header_height - layout.terminal_composer_height).max(0.0);
        let maximum = (window_height * 0.75)
            .floor()
            .max(MIN_DRAWER_HEIGHT)
            .min(conversation_budget);
        let stored = self
            .draft_environment
            .as_deref()
            .and_then(|environment| {
                self.client_settings
                    .terminal_height(environment, &terminal.thread_id)
            })
            .map_or(DEFAULT_DRAWER_HEIGHT, |height| height as f32);
        let height = terminal
            .resize_preview
            .unwrap_or(stored)
            .max(MIN_DRAWER_HEIGHT)
            .min(maximum);
        let bindings = match &self.keybindings {
            Some(bindings) => bindings,
            None => crate::connection::Keybindings::stock(),
        };
        let shortcut = |command| {
            bindings.shortcut_label(command, |flag| {
                matches!(flag, "terminalFocus" | "terminalOpen" | "isDesktop")
            })
        };
        let shortcuts = view::ShortcutLabels {
            horizontal: shortcut("terminal.split"),
            vertical: shortcut("terminal.splitVertical"),
            new: shortcut("terminal.new"),
            close: shortcut("terminal.close"),
        };
        view::drawer(
            terminal,
            &self.terminal_peers,
            self.terminal_tabs.get(&terminal.thread_id),
            [height, maximum],
            palette,
            self.terminal_tab_actions_enabled(),
            shortcuts,
        )
    }

    fn resize_terminal_drawer(&mut self, drag: &mut Drag, height: f32, maximum: f32) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        match drag.phase {
            Phase::Down => drag.data = height,
            Phase::Move => {
                terminal.resize_preview = Some(
                    (drag.data - drag.dy)
                        .round()
                        .max(MIN_DRAWER_HEIGHT)
                        .min(maximum),
                );
            }
            Phase::Up => {
                let Some(new_height) = terminal.resize_preview.take() else {
                    return;
                };
                if drag.cancelled() || new_height == drag.data {
                    return;
                }
                if let Some(environment) = self.draft_environment.as_deref()
                    && let Err(error) = self.client_settings.set_terminal_height(
                        environment,
                        &terminal.thread_id,
                        new_height as u32,
                    )
                {
                    self.project_notice = Some(format!("Could not save terminal height. {error}"));
                }
            }
        }
    }

    fn resize_terminal_pane(&mut self, attachment: u64, [_, _, width, height]: [f32; 4]) {
        let Some(terminal) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        terminal.viewport_height = height;
        let content_width = width - 2.0 * CELL_PADDING;
        let content_height = height - 2.0 * CELL_PADDING;
        let grid = (
            (content_width / CELL_WIDTH).clamp(1.0, 500.0) as u16,
            (content_height / LINE_HEIGHT).clamp(1.0, 64.0) as u16,
        );
        if grid != terminal.requested_grid {
            terminal.requested_grid = grid;
            if let Some(connection) = &self.connection {
                connection.resize_terminal(attachment, grid.0, grid.1);
            }
        }
    }

    fn send_terminal_input(&self, input: String) -> bool {
        self.terminal
            .as_ref()
            .is_some_and(|pane| self.write_terminal_input_at(pane.attachment, input, false))
    }

    fn write_terminal_input_at(&self, attachment: u64, input: String, binary: bool) -> bool {
        if !self
            .terminal
            .iter()
            .chain(&self.terminal_peers)
            .any(|pane| pane.attachment == attachment && pane.status == TerminalStatus::Running)
        {
            return false;
        }
        let Some(connection) = &self.connection else {
            return false;
        };
        if self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.reconnecting)
        {
            return false;
        }
        connection.write_terminal(attachment, input, binary, false);
        true
    }

    pub(super) fn paste_terminal(&mut self, text: &str, tx: &mut Tx) {
        if tx.focused() == Some(INPUT_KEY)
            && let Some(pane) = &self.terminal
        {
            self.paste_terminal_at(pane.attachment, text);
        }
    }

    fn paste_terminal_at(&self, attachment: u64, text: &str) {
        let Some(pane) = self
            .terminal
            .iter()
            .chain(&self.terminal_peers)
            .find(|pane| pane.attachment == attachment)
        else {
            return;
        };
        let input = if pane.screen.term.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text.replace('\n', "\r")
        };
        self.write_terminal_input_at(attachment, input, false);
    }

    pub(super) fn matches_terminal_toggle(&self, event: &KeyEvent, tx: &Tx) -> bool {
        self.resolve_shortcut(event, tx) == Some("terminal.toggle")
    }

    pub(super) fn resolve_shortcut(&self, event: &KeyEvent, tx: &Tx) -> Option<&str> {
        let binding = match &self.keybindings {
            Some(binding) => binding,
            None => crate::connection::Keybindings::stock(),
        };
        let terminal_focus = tx.focused() == Some(INPUT_KEY);
        let selection = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.selection.as_ref());
        let draft_route =
            selection.is_some_and(|selection| matches!(selection.view, ProjectView::NewThread));
        let composer_focus = selection
            .and_then(|selection| match &selection.view {
                ProjectView::NewThread => self.new_thread_drafts.get(&selection.id),
                ProjectView::Conversation(conversation) => self.drafts.get(&conversation.id),
                _ => None,
            })
            .is_some_and(|draft| draft.edit.focused);
        binding.command(event, tx, |flag| match flag {
            "terminalFocus" => terminal_focus,
            "terminalOpen" => self.terminal.is_some(),
            "draftThreadRoute" => draft_route,
            "isDesktop" => true,
            "composerFocus" => composer_focus,
            "editableFocus" => terminal_focus || composer_focus,
            _ => false,
        })
    }

    fn terminal_key(&mut self, event: &KeyEvent, tx: &mut Tx) -> bool {
        match self.resolve_shortcut(event, tx) {
            Some("terminal.toggle") => {
                self.toggle_terminal(tx);
                tx.blur();
            }
            Some("terminal.new") => self.new_terminal_tab(tx),
            Some("terminal.split") => self.split_terminal(SplitDirection::Horizontal, tx),
            Some("terminal.splitVertical") => self.split_terminal(SplitDirection::Vertical, tx),
            Some("sidebar.toggle") => return false,
            Some("terminal.close") => {
                if !tx.key_repeat() {
                    self.request_selected_terminal_close(tx);
                }
            }
            _ => {
                let Some(terminal) = &mut self.terminal else {
                    return false;
                };
                if selection::clipboard_key(terminal, event, tx) {
                    return true;
                }
                let application = terminal.screen.term.mode().contains(TermMode::APP_CURSOR);
                let attachment = terminal.attachment;
                if let Some(action) = history::input_key(event)
                    && self.scroll_history(attachment, action)
                {
                    return true;
                }
                return key_input(event, application)
                    .is_some_and(|input| self.send_terminal_input(input));
            }
        }
        true
    }
}

fn key_input(event: &KeyEvent, application_cursor: bool) -> Option<String> {
    let cursor = |ordinary, application| {
        if application_cursor {
            application
        } else {
            ordinary
        }
    };
    let key = match &event.key {
        Key::Text(text) if !event.mods.cmd || event.mods.alt => text.as_str(),
        Key::Command(letter)
            if (event.mods.ctrl || !cfg!(target_os = "macos")) && letter.is_ascii_lowercase() =>
        {
            return Some(char::from(*letter as u8 - b'a' + 1).to_string());
        }
        Key::Enter => "\r",
        Key::Backspace => "\x7f",
        Key::Delete => "\x1b[3~",
        Key::Tab if event.mods.shift => "\x1b[Z",
        Key::Tab => "\t",
        Key::Escape => "\x1b",
        Key::Up => cursor("\x1b[A", "\x1bOA"),
        Key::Down => cursor("\x1b[B", "\x1bOB"),
        Key::Right => cursor("\x1b[C", "\x1bOC"),
        Key::Left => cursor("\x1b[D", "\x1bOD"),
        Key::Home => cursor("\x1b[H", "\x1bOH"),
        Key::End => cursor("\x1b[F", "\x1bOF"),
        Key::PageUp => "\x1b[5~",
        Key::PageDown => "\x1b[6~",
        _ => return None,
    };
    Some(if event.mods.alt && !event.mods.ctrl {
        format!("\x1b{key}")
    } else {
        key.to_owned()
    })
}

fn viewport(terminal: &Pane, active: bool, palette: Palette) -> El {
    let attachment = terminal.attachment;
    let key = if active {
        INPUT_KEY.to_owned()
    } else {
        format!("{INPUT_KEY}-{attachment}")
    };
    let hovered_link = links::highlighted_cells(terminal);
    let (contents, spans) = screen_text(terminal, palette, hovered_link.as_ref());
    let mut output = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .clip_rounded(0.0)
        .children(selection::highlights(terminal, palette))
        .child(text(contents).font(FONT).color(palette.ink).spans(spans));
    if let Some(message) = &terminal.message {
        output = output.child(text(message).font(SMALL).color(palette.muted));
    } else if terminal.status == TerminalStatus::Starting {
        output = output.child(text("Opening terminal…").font(SMALL).color(palette.muted));
    }
    let mut viewport = el()
        .key(key)
        .role("group")
        .label("Terminal input")
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .padding(CELL_PADDING)
        .on_background_press_tx(move |state: &mut State, tx| {
            state.activate_terminal_attachment(attachment, tx)
        })
        .focusable(active)
        .no_focus_ring()
        .cursor(if hovered_link.is_some() {
            rune::Cursor::Pointer
        } else {
            rune::Cursor::Text
        })
        .on_secondary_press(move |state: &mut State, point, tx| {
            if !state.terminal_mouse_captured(attachment, tx.mods()) {
                state.open_terminal_menu(attachment, point, tx);
            }
        })
        .on_drag(move |state: &mut State, drag, tx| {
            state.terminal_pointer_drag(attachment, drag, tx)
        })
        .pointer_buttons([rune::PointerButton::Middle, rune::PointerButton::Secondary])
        .on_pointer_move_local(move |state: &mut State, event, tx| {
            state.terminal_pointer_move(attachment, event, tx);
            state.terminal_link_hover(attachment, Some(event), tx);
        })
        .on_hover_change(move |state: &mut State, hovered, tx| {
            if !hovered {
                state.terminal_link_hover(attachment, None, tx);
            }
        })
        .on_key(State::terminal_key)
        .on_wheel(move |state: &mut State, event, _| state.terminal_wheel(attachment, event))
        .on_layout(move |state: &mut State, rect| state.resize_terminal_pane(attachment, rect))
        .child(output);
    if terminal.screen.mouse_modes.protocol().tracking != mouse_modes::Tracking::Disabled {
        viewport =
            viewport.pointer_buttons([rune::PointerButton::Back, rune::PointerButton::Forward]);
    }
    if let Some(scrollbar) = history::scrollbar(terminal, palette) {
        viewport = viewport.child(scrollbar);
    }
    viewport
}

fn screen_text(
    terminal: &Pane,
    palette: Palette,
    hovered_link: Option<&std::ops::RangeInclusive<Point>>,
) -> (String, Vec<Span>) {
    let term = &terminal.screen.term;
    let grid = term.grid();
    let content = term.renderable_content();
    let cursor = (terminal.status == TerminalStatus::Running
        && content.display_offset == 0
        && content.cursor.shape != CursorShape::Hidden)
        .then_some(content.cursor.point);
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let mut contents = String::new();
    let mut spans = Vec::new();
    for row in 0..rows {
        let line = Line(row as i32 - content.display_offset as i32);
        let end = (0..cols).rev().find(|&col| {
            let cell = &grid[line][Column(col)];
            let point = Point::new(line, Column(col));
            cursor == Some(point)
                || cell.c != ' '
                || cell.bg != TerminalColor::Named(NamedColor::Background)
                || cell.flags.contains(Flags::INVERSE)
        });
        if let Some(end) = end {
            for col in 0..=end {
                let cell = &grid[line][Column(col)];
                let flags = cell.flags;
                if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                let start = contents.len();
                if flags.contains(Flags::HIDDEN) {
                    contents.push(' ');
                    if flags.contains(Flags::WIDE_CHAR) {
                        contents.push(' ');
                    }
                } else {
                    contents.push(cell.c);
                    contents.extend(cell.zerowidth().unwrap_or_default());
                }
                let invert = flags.contains(Flags::INVERSE)
                    ^ (cursor == Some(Point::new(line, Column(col))));
                let mut fg = color(cell.fg, content.colors, palette.ink);
                let mut bg = color(cell.bg, content.colors, palette.canvas);
                if invert {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if bg != palette.canvas
                    && content
                        .selection
                        .is_some_and(|selection| selection.contains(Point::new(line, Column(col))))
                {
                    bg = selection::background(bg, palette.dark);
                }
                let underline = flags.intersects(Flags::ALL_UNDERLINES)
                    || hovered_link
                        .is_some_and(|link| link.contains(&Point::new(line, Column(col))));
                if fg != palette.ink
                    || bg != palette.canvas
                    || underline
                    || flags.intersects(Flags::BOLD_ITALIC | Flags::DIM)
                {
                    let mut span =
                        Span::new(start, contents.len()).color(if flags.contains(Flags::DIM) {
                            fg.mix(bg, 0.4)
                        } else {
                            fg
                        });
                    if bg != palette.canvas {
                        span = span.bg(bg, 0.0);
                    }
                    if flags.contains(Flags::BOLD) {
                        span = span.bold();
                    }
                    if flags.contains(Flags::ITALIC) {
                        span = span.italic();
                    }
                    if underline {
                        span = span.underline(fg);
                    }
                    spans.push(span);
                }
            }
        }
        if row + 1 < rows {
            contents.push('\n');
        }
    }
    (contents, spans)
}

fn color(value: TerminalColor, colors: &Colors, default: Color) -> Color {
    const BASE: [(usize, usize, usize); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 128, 0),
        (205, 205, 0),
        (0, 76, 172),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let index = match value {
        TerminalColor::Named(name) => name as usize,
        TerminalColor::Indexed(index) => usize::from(index),
        TerminalColor::Spec(rgb) => return terminal_rgb(rgb.r, rgb.g, rgb.b),
    };
    if let Some(rgb) = colors[index] {
        return terminal_rgb(rgb.r, rgb.g, rgb.b);
    }
    let (r, g, b) = match index {
        0..=15 => BASE[index],
        16..=231 => {
            let n = index - 16;
            let level = |n| if n == 0 { 0 } else { 55 + n * 40 };
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        232..=255 => {
            let grey = 8 + (index - 232) * 10;
            (grey, grey, grey)
        }
        _ => return default,
    };
    terminal_rgb(r as u8, g as u8, b as u8)
}

fn terminal_rgb(r: u8, g: u8, b: u8) -> Color {
    let decode = |value| rune::color::srgb_decode(f32::from(value) / 255.0);
    Color {
        r: decode(r),
        g: decode(g),
        b: decode(b),
        a: 1.0,
    }
}
