use rune::color::hex;
use rune::{Color, El, Icon, Key, KeyEvent, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};

use super::conversation::{Layout, MAX_LANE};
use super::{Palette, Project, ProjectView, State, Thread, Workspace};

const CARD_WIDTH: f32 = 280.0;
const GAP: f32 = 12.0;
const LANE_CLEARANCE: f32 = 32.0;
const LANE_PADDING: f32 = 48.0;
const MIN_LANE_WIDTH: f32 = 640.0;
const MIN_INLINE_HEIGHT: f32 = 160.0;
const CARD_KEY: &str = "thread-details-card";
const TOGGLE_KEY: &str = "thread-details-toggle";
const LABEL: TextStyle = TextStyle::new(13.0, 500.0).lh(19.5);

macro_rules! glyph {
    ($name:literal, $markup:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(
                r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#,
                $markup,
                "</g>"
            ),
        }
    };
}

static SQUARE_MENU: Icon = glyph!(
    "square-menu",
    r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M7 8h10M7 12h10M7 16h10"/>"#
);
static SERVER: Icon = glyph!(
    "server",
    r#"<rect width="20" height="8" x="2" y="2" rx="2"/><rect width="20" height="8" x="2" y="14" rx="2"/><path d="M6 6h.01M6 18h.01"/>"#
);
static FOLDER: Icon = glyph!(
    "folder",
    r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#
);
static FOLDER_GIT: Icon = glyph!(
    "folder-git-2",
    r#"<path d="M18 19a5 5 0 0 1-5-5v8"/><path d="M9 20H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H20a2 2 0 0 1 2 2v5"/><circle cx="13" cy="12" r="2"/><circle cx="20" cy="19" r="2"/>"#
);
static BRANCH: Icon = glyph!(
    "git-branch",
    r#"<path d="M15 6a9 9 0 0 0-9 9V3"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/>"#
);

mod actions;

#[derive(Default)]
pub(super) struct Popover {
    thread_id: Option<String>,
    focused: bool,
    actions: Option<actions::Menu>,
}

impl Popover {
    pub(super) fn close(&mut self) -> bool {
        self.focused = false;
        self.actions = None;
        self.thread_id.take().is_some()
    }

    pub(super) fn observe_focus(&mut self, tx: &Tx) -> bool {
        if self.thread_id.is_none() {
            return false;
        }
        if matches!(tx.focused(), Some(CARD_KEY | TOGGLE_KEY))
            || tx.focused().is_some_and(actions::owns_focus)
        {
            self.focused = true;
            false
        } else {
            self.focused && self.close()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inline,
    Popover,
}

fn mode(width: f32, height: f32, header_height: f32) -> Mode {
    if width >= CARD_WIDTH + GAP + LANE_CLEARANCE + LANE_PADDING + MIN_LANE_WIDTH
        && height - header_height - 2.0 * GAP >= MIN_INLINE_HEIGHT
    {
        Mode::Inline
    } else {
        Mode::Popover
    }
}

fn presentation(layout: Layout) -> Mode {
    mode(layout.pane_width, layout.viewport[1], layout.header_height)
}

fn is_open(state: &State, thread_id: &str, mode: Mode) -> bool {
    match mode {
        Mode::Inline => state.draft_environment.as_ref().is_none_or(|environment| {
            state
                .client_settings
                .thread_details_inline_open(environment, thread_id)
        }),
        Mode::Popover => state.thread_details.thread_id.as_deref() == Some(thread_id),
    }
}

pub(super) fn observe_pane(state: &mut State, [_, _, width, height]: [f32; 4], header: f32) {
    if mode(width, height, header) == Mode::Inline && state.thread_details.thread_id.is_some() {
        state.thread_details.close();
    }
}

pub(super) fn content_layout(state: &State, thread: Option<&Thread>, mut layout: Layout) -> Layout {
    if presentation(layout) == Mode::Inline
        && thread.is_some_and(|thread| is_open(state, &thread.id, Mode::Inline))
    {
        layout.pane_width = inline_lane_container(layout.pane_width);
    }
    layout
}

fn inline_lane_container(width: f32) -> f32 {
    let lane_right = width - CARD_WIDTH - GAP - LANE_CLEARANCE;
    let lane_width = MAX_LANE.min(lane_right - LANE_PADDING);
    let lane_left = ((width - lane_width) / 2.0).min(lane_right - lane_width);
    lane_left * 2.0 + lane_width
}

fn toggle_panel(state: &mut State, layout: Layout, tx: &mut Tx) -> bool {
    let Some((environment, thread_id)) = state
        .draft_environment
        .as_ref()
        .zip(state.workspace.as_ref())
        .and_then(|(environment, workspace)| {
            let ProjectView::Conversation(conversation) = &workspace.selection.as_ref()?.view
            else {
                return None;
            };
            workspace
                .thread(&conversation.id)
                .map(|thread| (environment, &thread.id))
        })
    else {
        return false;
    };
    state.thread_details.actions = None;
    if let Some(connection) = &state.connection {
        connection.ensure_workspace_settings();
    }
    match presentation(layout) {
        Mode::Inline => {
            let open = !state
                .client_settings
                .thread_details_inline_open(environment, thread_id);
            if let Err(error) =
                state
                    .client_settings
                    .set_thread_details_inline_open(environment, thread_id, open)
            {
                state.project_notice = Some(format!("Could not save thread details. {error}"));
            } else if open {
                tx.focus_when_shown(CARD_KEY, false);
            } else if tx.focused() == Some(CARD_KEY) {
                tx.focus(TOGGLE_KEY, false);
            }
        }
        Mode::Popover => {
            if state.thread_details.thread_id.as_ref() == Some(thread_id) {
                state.thread_details.close();
                tx.focus(TOGGLE_KEY, false);
            } else {
                state.thread_details.thread_id = Some(thread_id.clone());
                state.thread_details.focused = false;
                tx.focus_when_shown(CARD_KEY, false);
            }
        }
    }
    true
}

pub(super) fn shortcut(state: &mut State, layout: Layout, event: &KeyEvent, tx: &mut Tx) -> bool {
    if state.add_project.is_some() || tx.focused() == Some("terminal-input") {
        return false;
    }
    if event.key == Key::Escape && actions::dismiss(state, tx) {
        true
    } else if event.key == Key::Escape && state.thread_details.close() {
        tx.focus(TOGGLE_KEY, false);
        true
    } else if event.is_command('b') && event.mods.shift && !event.mods.alt {
        !tx.key_repeat() && toggle_panel(state, layout, tx)
    } else {
        false
    }
}

pub(super) fn toggle(state: &State, thread: &Thread, layout: Layout, palette: Palette) -> El {
    let open = is_open(state, &thread.id, presentation(layout));
    let (ink, accent) = if palette.dark {
        ("#f5f5f5", "#0a0a0a")
    } else {
        ("#18181b", "#f4f4f5")
    };
    let accent = hex(accent).expect("static thread details color");
    el().key(TOGGLE_KEY)
        .role("button")
        .label("Toggle thread details panel")
        .help(if cfg!(target_os = "macos") {
            "Toggle thread details (⇧⌘B)"
        } else {
            "Toggle thread details (Ctrl+Shift+B)"
        })
        .checked(open)
        .size(28.0, 28.0)
        .radius(8.0)
        .center()
        .fill(if open { accent } else { Color::TRANSPARENT })
        .hover(accent)
        .focusable(true)
        .focus_ring(2.0, -1.0, palette.accent)
        .on_press_tx(move |state, tx| {
            toggle_panel(state, layout, tx);
        })
        .child(
            icon(&SQUARE_MENU, 16.0)
                .color(hex(ink).expect("static thread details color").alpha(0.8))
                .a11y_hidden(),
        )
}

pub(super) fn card(
    state: &State,
    workspace: &Workspace,
    project: &Project,
    thread: Option<&Thread>,
    layout: Layout,
    palette: Palette,
) -> Option<El> {
    let mode = presentation(layout);
    let thread = thread.filter(|thread| is_open(state, &thread.id, mode))?;
    let top = layout.header_height + if mode == Mode::Inline { GAP } else { 8.0 };
    let height = (layout.viewport[1] - top - GAP).max(0.0);
    let width = CARD_WIDTH.min((layout.pane_width - 2.0 * GAP).max(0.0));
    let (glass, ink, environment_ink, blur) = if palette.dark {
        ("#111111", "#f5f5f5", "#a3a3a3", 16.0)
    } else {
        ("#ffffff", "#18181b", "#18181b", 12.0)
    };
    let glass = hex(glass).expect("static thread details color");
    let ink = hex(ink).expect("static thread details color");
    let environment_ink = hex(environment_ink)
        .expect("static thread details color")
        .alpha(if palette.dark { 1.0 } else { 0.8 });
    let border = ink.alpha(0.1);
    let (workspace_path, workspace_label, workspace_icon) =
        if let Some(path) = &thread.worktree_path {
            let basename = path.rsplit(['/', '\\']).find(|part| !part.is_empty());
            (path.as_str(), basename.unwrap_or(path), &FOLDER_GIT)
        } else {
            (project.path.as_str(), "Local checkout", &FOLDER)
        };
    let workspace_row = row()
        .key("thread-details-workspace")
        .role("group")
        .label(format!(
            "Run context: {}, {workspace_label}",
            workspace.label
        ))
        .help(workspace_path)
        .w_full()
        .h(32.0)
        .pl(10.0)
        .pr(11.0)
        .gap(9.0)
        .center_y()
        .child(icon(&SERVER, 18.0).color(environment_ink).a11y_hidden())
        .child(
            text(&workspace.label)
                .min_w(0.0)
                .grow(1.0)
                .font(LABEL)
                .color(environment_ink)
                .truncate(),
        )
        .child(
            icon(workspace_icon, 16.0)
                .color(environment_ink)
                .a11y_hidden(),
        );
    let mut contents = col().w_full().child(
        section("Workspace")
            .child(workspace_row)
            .children(actions::control(state, workspace, project, thread, palette)),
    );
    if let Some(branch) = &thread.branch {
        let branch = row()
            .key("thread-details-branch")
            .role("group")
            .label(format!("Branch: {branch}"))
            .w_full()
            .h(32.0)
            .px(11.0)
            .gap(10.0)
            .center_y()
            .child(icon(&BRANCH, 16.0).color(ink.alpha(0.8)).a11y_hidden())
            .child(text(branch).font(LABEL).color(ink.alpha(0.8)).truncate());
        contents = contents.child(
            section("Version Control")
                .border_sides([1.0, 0.0, 0.0, 0.0], border)
                .border_layout()
                .child(branch),
        );
    }
    Some(
        col()
            .key(CARD_KEY)
            .pin(None, Some(top), Some(GAP), None)
            .w(width)
            .max_h(height)
            .role("region")
            .label("Thread details")
            .radius(16.0)
            .border(1.0, border)
            .border_layout()
            .fill(glass.alpha(0.836))
            .backdrop_blur(blur)
            .shadow(
                6.0,
                18.0,
                Color::BLACK.alpha(if palette.dark { 0.28 } else { 0.09 }),
            )
            .focusable(true)
            .tab_index(-1)
            .preserve_focus_on_press(true)
            .hit()
            .clip_rounded(16.0)
            .child(
                col()
                    .min_h(0.0)
                    .w_full()
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .child(contents),
            ),
    )
}

pub(super) fn actions_menu(
    state: &State,
    project: &Project,
    thread: Option<&Thread>,
    layout: Layout,
    palette: Palette,
) -> Option<[El; 2]> {
    let thread = thread.filter(|thread| is_open(state, &thread.id, presentation(layout)))?;
    actions::overlay(state, project, thread, palette)
}

fn section(label: &str) -> El {
    col()
        .w_full()
        .role("region")
        .label(label)
        .px(8.0)
        .pt(8.0)
        .pb(10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_reserve_only_the_space_to_the_right_of_the_chat_lane() {
        assert!(mode(1011.0, 740.0, 40.0) == Mode::Popover);
        assert!(mode(1012.0, 223.0, 40.0) == Mode::Popover);
        assert!(mode(1012.0, 224.0, 40.0) == Mode::Inline);
        assert_eq!(inline_lane_container(1012.0), 736.0);
        assert_eq!(inline_lane_container(1108.0), 832.0);
        assert_eq!(inline_lane_container(1264.0), 1144.0);
        assert_eq!(inline_lane_container(1500.0), 1500.0);
    }
}
