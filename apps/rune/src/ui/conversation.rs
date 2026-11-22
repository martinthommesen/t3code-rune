use rune::color::hex;
use rune::{Color, El, Icon, ScrollSpec, TextStyle, col, el, icon, row, text};
use rune_kit::{Markdown, MarkdownStyle};
use unicode_normalization::UnicodeNormalization;

use super::approval;
use super::navigation_history::Destination;
use super::sidebar::project_badge;
use super::worktree_setup;
use super::{
    Conversation, ConversationEntry, Message, Palette, Project, ProjectView, State, Thread,
    Workspace,
};

const TOPBAR_TEXT: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const SUMMARY_TEXT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const PROSE: TextStyle = TextStyle::new(14.0, 400.0).lh(22.75);
pub(super) const MAX_LANE: f32 = 736.0;
const LANE_EDGE: f32 = 48.0;

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $path, "</g>"),
        }
    };
}

static TERMINAL: Icon = glyph!(
    "panel-bottom",
    r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 15h18"/>"#
);
static MESSAGES: Icon = glyph!(
    "messages-square",
    r#"<path d="M16 10a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 14.286V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"/><path d="M20 9a2 2 0 0 1 2 2v10.286a.71.71 0 0 1-1.212.502l-2.202-2.202A2 2 0 0 0 17.172 19H10a2 2 0 0 1-2-2v-1"/>"#
);
static COLLAPSED: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);
static EXPANDED: Icon = glyph!("chevron-up", r#"<path d="m6 15 6-6 6 6"/>"#);
static COPY: Icon = glyph!(
    "copy",
    r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#
);

mod command_ansi;
pub(super) mod command_row;
mod queued_runs;
mod shell_command;
mod todo_list;
mod work_fold;

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub(super) header_height: f32,
    pub(super) header_left: f32,
    pub(super) native_trailing: f32,
    pub(super) pane_width: f32,
    pub(super) viewport: [f32; 2],
}

impl Layout {
    fn header_right(self) -> f32 {
        let native_controls = if cfg!(target_os = "macos") {
            12.0
        } else if self.native_trailing > 0.0 {
            self.native_trailing + 12.0
        } else {
            126.0
        };
        native_controls + 32.0
    }

    pub(super) fn timeline_gutter(self) -> f32 {
        if self.pane_width < 280.0 {
            12.0
        } else {
            LANE_EDGE
        }
    }

    fn max_bubble_width(self) -> f32 {
        ((self.pane_width - 2.0 * self.timeline_gutter()).min(MAX_LANE) * 0.8).max(0.0)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Colors {
    pub(super) background: Color,
    foreground: Color,
    muted: Color,
    accent: Color,
    bubble: Color,
    fallbacks: Palette,
}

impl Colors {
    pub(super) fn new(palette: Palette) -> Self {
        let [background, foreground, muted, accent, bubble] = if palette.dark {
            ["#000000", "#f1f3f7", "#a3a3a3", "#191a1d", "#1e1e1e"]
        } else {
            ["#fcfcfc", "#18181b", "#71717b", "#f4f4f5", "#ebebed"]
        }
        .map(|value| hex(value).expect("static conversation palette"));
        Self {
            background,
            foreground,
            muted,
            accent,
            bubble,
            fallbacks: palette,
        }
    }
}

pub(super) fn header(
    state: &State,
    workspace: &Workspace,
    project: &Project,
    thread: Option<&Thread>,
    layout: Layout,
    colors: Colors,
) -> El {
    let mut project_item = row()
        .key("conversation-project")
        .min_w(0.0)
        .center_y()
        .gap(6.0)
        .radius(4.0)
        .child(project_badge::view_at(
            project,
            &workspace.favicons,
            colors.fallbacks.dark,
            14.0,
        ))
        .child(
            text(&project.name)
                .font(TOPBAR_TEXT)
                .color(colors.muted)
                .truncate(),
        );
    if workspace.launch_writes && !workspace.reconnecting && thread.is_some() {
        let id = project.id.clone();
        let label = format!("New thread in {}", project.name);
        project_item = project_item
            .role("button")
            .focusable(true)
            .label(&label)
            .help(label)
            .focus_ring(2.0, -1.0, colors.fallbacks.accent)
            .on_press(move |state: &mut State| state.navigate(Destination::NewThread(id.clone())));
    }
    let mut breadcrumb = row()
        .key("conversation-breadcrumb")
        .label("Thread breadcrumb")
        .role("navigation")
        .grow(1.0)
        .min_w(0.0)
        .center_y()
        .gap(12.0)
        .child(project_item);
    if let Some(thread) = thread {
        breadcrumb = breadcrumb
            .child(
                text("/")
                    .font(SUMMARY_TEXT)
                    .color(colors.muted)
                    .a11y_hidden(),
            )
            .child(
                text(&thread.title)
                    .font(TOPBAR_TEXT)
                    .color(colors.foreground)
                    .role("heading")
                    .truncate(),
            );
    } else if workspace.selection.as_ref().is_some_and(|selection| {
        matches!(
            selection.view,
            ProjectView::NewThread | ProjectView::WaitingForThread(_)
        )
    }) {
        breadcrumb = breadcrumb
            .child(
                text("/")
                    .font(SUMMARY_TEXT)
                    .color(colors.muted)
                    .a11y_hidden(),
            )
            .child(
                text("New thread")
                    .font(TOPBAR_TEXT)
                    .color(colors.foreground)
                    .truncate(),
            );
    } else {
        breadcrumb = breadcrumb.child(
            text(&project.path)
                .font(SUMMARY_TEXT)
                .color(colors.muted)
                .truncate(),
        );
    }
    let mut header = state
        .window_chrome
        .drag_region(
            row()
                .key("conversation-topbar")
                .w_full()
                .h(layout.header_height)
                .pl(layout.header_left)
                .pr(layout.header_right())
                .gap(8.0)
                .center_y(),
        )
        .child(breadcrumb);
    if let Some(thread) = thread.filter(|_| workspace.terminal && !workspace.reconnecting) {
        let open = state
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.thread_id == thread.id);
        header = header.child(
            el().key("terminal-toggle")
                .role("button")
                .label("Toggle terminal drawer")
                .help("Toggle terminal drawer")
                .size(28.0, 28.0)
                .center()
                .radius(8.0)
                .checked(open)
                .focusable(true)
                .focus_ring(2.0, -1.0, colors.fallbacks.accent)
                .fill(if open {
                    colors.accent
                } else {
                    Color::TRANSPARENT
                })
                .hover(colors.accent)
                .on_press_tx(State::toggle_terminal)
                .child(
                    icon(&TERMINAL, 16.0)
                        .color(colors.foreground.alpha(0.8))
                        .a11y_hidden(),
                ),
        );
    }
    header
}

pub(super) fn transcript(
    conversation: &Conversation,
    entries: &[ConversationEntry],
    layout: Layout,
    colors: Colors,
    workspace: &Workspace,
) -> El {
    let bubble_width = layout.max_bubble_width();
    let split = entries
        .iter()
        .position(|entry| {
            entry
                .message()
                .is_some_and(|message| message.role == "user")
        })
        .map_or(entries.len(), |first_user| first_user + 1);
    let (before, after) = entries.split_at(split);
    let entry_row = |entry: &ConversationEntry, index: usize| match entry {
        ConversationEntry::Message(message) if message.role == "user" => Some(user_message(
            Some(conversation),
            message,
            bubble_width,
            layout.viewport[0],
            colors,
        )),
        ConversationEntry::Message(message) => Some(response(message, colors)),
        ConversationEntry::TodoList(list) => todo_list::view(
            conversation,
            list,
            colors,
            entries
                .get(index + 1)
                .is_some_and(|next| entry.continues_work_before(next)),
        ),
        ConversationEntry::Command(command) => command_row::view(
            conversation,
            command,
            colors,
            entries
                .get(index + 1)
                .is_some_and(|next| entry.continues_work_before(next)),
        ),
    };
    col()
        .key(format!("transcript-{}", conversation.id))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(layout.timeline_gutter())
        .items_center()
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(24.0)
        .child(
            col()
                .key("transcript-lane")
                .w_full()
                .max_w(MAX_LANE)
                .pt(24.0)
                .pb(16.0)
                .selection_group(true)
                .children(
                    before
                        .iter()
                        .enumerate()
                        .filter_map(|(index, entry)| entry_row(entry, index)),
                )
                .children(worktree_setup::view(
                    conversation,
                    entries.iter().filter_map(ConversationEntry::message),
                    workspace,
                    colors.fallbacks,
                ))
                .children(
                    after
                        .iter()
                        .enumerate()
                        .filter_map(|(index, entry)| entry_row(entry, split + index)),
                )
                .children(approval::rows(
                    &conversation.id,
                    &conversation.approvals,
                    colors.fallbacks,
                    workspace.reconnecting,
                ))
                .children(queued_runs::view(
                    conversation,
                    workspace,
                    bubble_width,
                    colors,
                )),
        )
}

pub(super) fn draft_prompt(thread_id: &str, prompt: &str, layout: Layout, palette: Palette) -> El {
    user_message(
        None,
        &Message {
            id: format!("draft-{thread_id}"),
            role: "user".into(),
            text: prompt.to_owned(),
            created_by: None,
        },
        layout.max_bubble_width(),
        layout.viewport[0],
        Colors::new(palette),
    )
}

fn user_message(
    conversation: Option<&Conversation>,
    message: &Message,
    max_width: f32,
    viewport_width: f32,
    colors: Colors,
) -> El {
    let agent = message.created_by.as_deref() == Some("agent");
    let expanded = conversation
        .is_some_and(|conversation| conversation.expanded_messages.contains(&message.id));
    let body = || {
        text(&message.text)
            .key(format!("message-text-{}", message.id))
            .font(PROSE)
            .color(colors.foreground)
            .wrap_anywhere()
            .selectable(true)
            .selection_color(colors.fallbacks.selected)
    };
    let mut bubble = col()
        .key(format!("message-bubble-{}", message.id))
        .max_w(max_width)
        .min_w(0.0)
        .fill(colors.bubble)
        .radius(16.0);
    if agent && let Some(conversation) = conversation {
        let id = message.id.clone();
        let thread = conversation.id.clone();
        let summary: String = message
            .text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .nfc()
            .collect();
        let control = row()
            .key(format!("toggle-message-{id}"))
            .role("button")
            .label(format!("Agent {summary}"))
            .expanded(expanded)
            .focusable(true)
            .focus_ring(2.0, -1.0, colors.fallbacks.accent)
            .h(36.0)
            .px(12.0)
            .gap(6.0)
            .center_y()
            .radius(16.0)
            .on_press(move |state: &mut State| {
                if let Some(conversation) = state.conversation_mut(&thread)
                    && !conversation.expanded_messages.remove(&id)
                {
                    conversation.expanded_messages.insert(id.clone());
                }
            })
            .child(icon(&MESSAGES, 16.0).color(colors.muted).a11y_hidden())
            .child(text("Agent").font(SUMMARY_TEXT).color(colors.muted))
            .child(
                text(summary)
                    .font(SUMMARY_TEXT)
                    .color(colors.foreground)
                    .truncate()
                    .min_w(0.0),
            )
            .child(
                icon(if expanded { &EXPANDED } else { &COLLAPSED }, 14.0)
                    .color(colors.muted)
                    .a11y_hidden(),
            );
        bubble = bubble.child(control);
        if expanded {
            bubble = bubble.w(max_width).child(
                col()
                    .w_full()
                    .px(12.0)
                    .pb(12.0)
                    .max_h(384.0)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .child(body()),
            );
        }
    } else {
        bubble = bubble.label("You").padding(12.0).child(body());
    }
    let mut message_row = col()
        .key(format!("message-{}", message.id))
        .role("article")
        .w_full()
        .items_end()
        .pb(16.0)
        .hover_group_focus()
        .child(bubble);
    if (!agent || expanded) && !message.text.is_empty() {
        let action_height = if viewport_width < 640.0 { 28.0 } else { 24.0 };
        message_row = message_row.child(
            row()
                .pt(4.0)
                .pr(4.0)
                .child(copy_button(message, colors).w(28.0).h(action_height)),
        );
    }
    message_row
}

fn response(message: &Message, colors: Colors) -> El {
    let content = if message.role == "assistant" {
        let style = MarkdownStyle::neutral(colors.fallbacks.dark)
            .body_font(PROSE)
            .text(colors.foreground.alpha(0.9))
            .muted(colors.muted)
            .link(colors.fallbacks.accent)
            .selection(colors.fallbacks.selected)
            .code_block(colors.bubble, colors.fallbacks.line);
        Markdown::new(message.text.as_str())
            .selectable(true)
            .tasks_read_only()
            .scroll_wide(true)
            .render(&style)
            .key("markdown")
    } else {
        text(&message.text)
            .font(PROSE)
            .color(colors.foreground)
            .wrap_anywhere()
            .selectable(true)
            .selection_color(colors.fallbacks.selected)
    };
    let mut response = col()
        .key(format!("message-{}", message.id))
        .role("article")
        .label(if message.role == "assistant" {
            "T3 Code"
        } else {
            message.role.as_str()
        })
        .w_full()
        .px(4.0)
        .pt(2.0)
        .pb(18.0)
        .gap(6.0)
        .hover_group_focus()
        .child(content);
    if !message.text.is_empty() {
        response = response.child(copy_button(message, colors));
    }
    response
}

fn copy_button(message: &Message, colors: Colors) -> El {
    let clipboard = message.text.clone();
    el().key(format!("copy-message-{}", message.id))
        .role("button")
        .label("Copy message")
        .help("Copy message")
        .size(24.0, 24.0)
        .center()
        .radius(8.0)
        .group_opacity(0.0, 1.0)
        .hover(colors.accent)
        .focusable(true)
        .focus_ring(2.0, -1.0, colors.fallbacks.accent)
        .on_press_tx(move |_: &mut State, tx| tx.copy(clipboard.clone()))
        .child(icon(&COPY, 12.0).color(colors.muted).a11y_hidden())
}
