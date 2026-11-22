use rune::{Color, El, Key, KeyEvent, Span, TextStyle, Tx, col, el, text};

use crate::connection::{
    TerminalAttach, TerminalSnapshot, TerminalStatus, TerminalTarget, TerminalUpdate,
};

use super::{Palette, ProjectSelection, ProjectView, SMALL, State};

mod control;
mod tabs;
mod view;

use control::TerminalControl;
pub(super) use tabs::TabsByThread;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 16;
const LINE_HEIGHT: f32 = 16.0;
const FONT: TextStyle = TextStyle::new(13.0, 400.0).lh(LINE_HEIGHT).mono();
const CELL_WIDTH: f32 = 7.8;
const INPUT_KEY: &str = "terminal-input";

pub(super) struct Pane {
    pub(super) attachment: u64,
    pub(super) thread_id: String,
    terminal_id: String,
    screen: vt100::Parser<TerminalControl>,
    requested_grid: (u16, u16),
    status: TerminalStatus,
    label: String,
    has_running_subprocess: bool,
    focused: bool,
    message: Option<String>,
    received_snapshot: bool,
    close_prompt: Option<ClosePrompt>,
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
            screen: vt100::Parser::new_with_callbacks(
                DEFAULT_ROWS,
                DEFAULT_COLS,
                2048,
                TerminalControl::default(),
            ),
            requested_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            status: TerminalStatus::Starting,
            label: "Shell".into(),
            has_running_subprocess: false,
            focused: false,
            message: None,
            received_snapshot: false,
            close_prompt: None,
        }
    }

    pub(super) fn apply(&mut self, update: TerminalUpdate) {
        match update {
            TerminalUpdate::Snapshot { snapshot } | TerminalUpdate::Restarted { snapshot } => {
                self.snapshot(snapshot)
            }
            TerminalUpdate::Output { data } => self.screen.process(data.as_bytes()),
            TerminalUpdate::Activity {
                cols,
                rows,
                label,
                has_running_subprocess,
            } => {
                self.label = label;
                self.has_running_subprocess = has_running_subprocess;
                if let (Some(rows), Some(cols)) = (rows, cols) {
                    self.screen
                        .screen_mut()
                        .set_size(rows.clamp(1, 64), cols.clamp(1, 500));
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
        self.screen = vt100::Parser::new_with_callbacks(
            snapshot.rows.unwrap_or(rows).clamp(1, 64),
            snapshot.cols.unwrap_or(cols).clamp(1, 500),
            2048,
            TerminalControl::default(),
        );
        self.screen.process(snapshot.history.as_bytes());
        self.screen.callbacks_mut().take_replies();
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
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let focused = self.window_focused && tx.focused() == Some(INPUT_KEY);
        if std::mem::replace(&mut terminal.focused, focused) != focused
            && terminal.status == TerminalStatus::Running
            && terminal.screen.callbacks().focus_reporting
            && self
                .workspace
                .as_ref()
                .is_some_and(|workspace| !workspace.reconnecting)
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

    pub(super) fn send_terminal_replies(&mut self) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let replies = terminal.screen.callbacks_mut().take_replies();
        if !replies.is_empty()
            && let Some(connection) = &self.connection
        {
            connection.write_terminal(terminal.attachment, replies, false, true);
        }
    }

    pub(super) fn toggle_terminal(&mut self, tx: &mut Tx) {
        if self.terminal.is_some() {
            self.detach_terminal_pane();
            return;
        }
        self.attach_selected_terminal(tx);
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
        if self.terminal_tabs.select_observed(&target, &script.name) {
            self.detach_terminal_pane();
            self.attach_selected_terminal(tx);
        }
    }

    fn attach_selected_terminal(&mut self, tx: &mut Tx) {
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
        let Some(connection) = &self.connection else {
            return;
        };
        let tab = self.terminal_tabs.ensure(&thread.id).selected();
        let attachment = connection.attach_terminal(TerminalAttach {
            target: TerminalTarget {
                thread_id: thread.id.clone(),
                terminal_id: tab.id.clone(),
            },
            cwd: tab.observed_label.is_none().then(|| {
                thread
                    .worktree_path
                    .as_deref()
                    .unwrap_or(&project.path)
                    .to_owned()
            }),
            worktree_path: thread.worktree_path.clone(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            resume: tab.resume,
        });
        self.terminal = Some(Pane::new(attachment, thread.id.clone(), tab.id.clone()));
        tx.focus(INPUT_KEY, false);
    }

    pub(super) fn detach_terminal_pane(&mut self) {
        if let Some(terminal) = self.terminal.take() {
            if let Some(tabs) = self.terminal_tabs.get_mut(&terminal.thread_id) {
                tabs.remember(&terminal);
            }
            if let Some(connection) = &self.connection {
                if terminal.focused
                    && terminal.status == TerminalStatus::Running
                    && terminal.screen.callbacks().focus_reporting
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
            self.detach_terminal_pane();
            self.attach_selected_terminal(tx);
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
                self.detach_terminal_pane();
                self.attach_selected_terminal(tx);
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
                self.detach_terminal_pane();
            }
            if closing_selected && self.terminal_tabs.get(&thread_id).is_some() {
                self.attach_selected_terminal(tx);
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

    pub(super) fn terminal_close_dialog(&self, palette: Palette) -> Option<El> {
        let prompt = self.terminal.as_ref()?.close_prompt.as_ref()?;
        Some(view::close_dialog(&prompt.label, palette))
    }

    pub(super) fn terminal_view(&self, terminal: &Pane, palette: Palette) -> El {
        view::drawer(
            terminal,
            self.terminal_tabs.get(&terminal.thread_id),
            palette,
            self.terminal_tab_actions_enabled(),
        )
    }

    fn resize_terminal_pane(&mut self, [_, _, width, height]: [f32; 4]) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let Some(connection) = &self.connection else {
            return;
        };
        if width <= 8.0 || height <= LINE_HEIGHT {
            return;
        }
        let grid = (
            ((width - 8.0) / CELL_WIDTH).clamp(2.0, 500.0) as u16,
            (height / LINE_HEIGHT).clamp(2.0, 64.0) as u16,
        );
        if grid != terminal.requested_grid {
            terminal.requested_grid = grid;
            connection.resize_terminal(terminal.attachment, grid.0, grid.1);
        }
    }

    fn send_terminal_input(&self, input: String) -> bool {
        let Some(terminal) = self
            .terminal
            .as_ref()
            .filter(|terminal| terminal.status == TerminalStatus::Running)
        else {
            return false;
        };
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
        connection.write_terminal(terminal.attachment, input, false, false);
        true
    }

    pub(super) fn paste_terminal(&mut self, text: &str, tx: &mut Tx) {
        if tx.focused() == Some(INPUT_KEY)
            && let Some(terminal) = &self.terminal
        {
            let data = if terminal.screen.screen().bracketed_paste() {
                format!("\x1b[200~{text}\x1b[201~")
            } else {
                text.to_owned()
            };
            self.send_terminal_input(data);
        }
    }

    fn terminal_key(&mut self, event: &KeyEvent, tx: &mut Tx) -> bool {
        if event.is_command('v') && (event.mods.shift || cfg!(target_os = "macos")) {
            tx.request_paste();
            return true;
        }
        if event.is_command('j') && !event.mods.shift {
            self.detach_terminal_pane();
            return true;
        }
        let primary_only = !event.mods.shift
            && !event.mods.alt
            && if cfg!(target_os = "macos") {
                !event.mods.ctrl
            } else {
                !event.mods.logo
            };
        if primary_only && event.is_command('n') {
            self.new_terminal_tab(tx);
            return true;
        }
        if primary_only && event.is_command('w') {
            self.request_selected_terminal_close(tx);
            return true;
        }
        let Some(terminal) = &self.terminal else {
            return false;
        };
        let application = terminal.screen.screen().application_cursor();
        key_input(event, application).is_some_and(|input| self.send_terminal_input(input))
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

fn viewport(terminal: &Pane, palette: Palette) -> El {
    let (contents, spans) = screen_text(terminal, palette);
    let mut output = col().w_full().grow(1.0).min_h(0.0).clip_rounded(0.0).child(
        text(contents)
            .font(FONT)
            .color(palette.ink)
            .spans(spans)
            .selectable(true),
    );
    if let Some(message) = &terminal.message {
        output = output.child(text(message).font(SMALL).color(palette.muted));
    } else if terminal.status == TerminalStatus::Starting {
        output = output.child(text("Opening terminal…").font(SMALL).color(palette.muted));
    }
    el().key(INPUT_KEY)
        .role("group")
        .label("Terminal input")
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .px(4.0)
        .focusable(true)
        .on_press_tx(|_: &mut State, tx| tx.focus(INPUT_KEY, false))
        .on_key(State::terminal_key)
        .on_layout(State::resize_terminal_pane)
        .child(output)
}

fn screen_text(terminal: &Pane, palette: Palette) -> (String, Vec<Span>) {
    let screen = terminal.screen.screen();
    let (rows, cols) = screen.size();
    let mut contents = String::new();
    let mut spans = Vec::new();
    let cursor = (terminal.status == TerminalStatus::Running && !screen.hide_cursor())
        .then(|| screen.cursor_position());
    for row in 0..rows {
        let end = (0..cols).rev().find(|&col| {
            cursor == Some((row, col))
                || screen.cell(row, col).is_some_and(|cell| {
                    cell.has_contents() || cell.bgcolor() != vt100::Color::Default || cell.inverse()
                })
        });
        if let Some(end) = end {
            for col in 0..=end {
                let Some(cell) = screen
                    .cell(row, col)
                    .filter(|cell| !cell.is_wide_continuation())
                else {
                    continue;
                };
                let start = contents.len();
                contents.push_str(if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                });
                let invert = cell.inverse() ^ (cursor == Some((row, col)));
                let mut fg = color(cell.fgcolor(), palette.ink);
                let mut bg = color(cell.bgcolor(), palette.canvas);
                if invert {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if fg != palette.ink
                    || bg != palette.canvas
                    || cell.bold()
                    || cell.dim()
                    || cell.italic()
                    || cell.underline()
                {
                    let mut span = Span::new(start, contents.len()).color(if cell.dim() {
                        fg.mix(bg, 0.4)
                    } else {
                        fg
                    });
                    if bg != palette.canvas {
                        span = span.bg(bg, 0.0);
                    }
                    if cell.bold() {
                        span = span.bold();
                    }
                    if cell.italic() {
                        span = span.italic();
                    }
                    if cell.underline() {
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

fn color(value: vt100::Color, default: Color) -> Color {
    const BASE: [(u8, u8, u8); 16] = [
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
    let (r, g, b) = match value {
        vt100::Color::Default => return default,
        vt100::Color::Rgb(r, g, b) => (r, g, b),
        vt100::Color::Idx(index @ 0..=15) => BASE[index as usize],
        vt100::Color::Idx(index @ 16..=231) => {
            let n = index - 16;
            let level = |n| if n == 0 { 0 } else { 55 + n * 40 };
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        vt100::Color::Idx(index) => {
            let grey = 8 + (index - 232) * 10;
            (grey, grey, grey)
        }
    };
    let decode = |value| rune::color::srgb_decode(f32::from(value) / 255.0);
    Color {
        r: decode(r),
        g: decode(g),
        b: decode(b),
        a: 1.0,
    }
}
