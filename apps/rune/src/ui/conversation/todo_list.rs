use rune::color::oklch;
use rune::{El, Family, Icon, ScrollSpec, TextStyle, col, el, icon, row, text};

use crate::connection::{TodoListEntry, TodoStepStatus};

use super::{Colors, Conversation, State};

const DETAILS: TextStyle = TextStyle::new(13.0, 400.0)
    .lh(21.125)
    .family(Family::SystemMono);

static LIST_TODO: Icon = glyph!(
    "list-todo",
    r#"<path d="M13 5h8M13 12h8M13 19h8m-18-7 2 2 4-4"/><rect width="6" height="6" x="3" y="2" rx="1"/><rect width="6" height="6" x="3" y="16" rx="1"/>"#
);
static COLLAPSED: Icon = glyph!("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static EXPANDED: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);

pub(super) fn view(
    conversation: &Conversation,
    list: &TodoListEntry,
    colors: Colors,
    continues: bool,
) -> Option<El> {
    super::work_fold::view(
        conversation,
        list.run_id.as_deref(),
        list.completed_work.as_ref(),
        colors,
        || list_row(conversation, list, colors, continues),
    )
}

fn list_row(
    conversation: &Conversation,
    list: &TodoListEntry,
    colors: Colors,
    continues: bool,
) -> El {
    let has_steps = !list.steps.is_empty();
    let expanded = has_steps && conversation.expanded_todo_lists.contains(&list.id);
    let label = summary(list);
    let label_color = if colors.fallbacks.dark {
        oklch(0.97, 0.0, 0.0)
    } else {
        oklch(0.21, 0.006, 285.885)
    }
    .alpha(0.8);
    let mut header = row()
        .key(format!("toggle-todo-list-{}", list.id))
        .w_full()
        .min_h(24.0)
        .center_y()
        .gap(6.0)
        .child(
            el().size(24.0, 24.0)
                .center()
                .child(icon(&LIST_TODO, 16.0).color(colors.muted).a11y_hidden()),
        )
        .child(
            text(&label)
                .key(format!("todo-list-summary-{}", list.id))
                .font(super::PROSE)
                .color(label_color)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if has_steps {
        let thread_id = conversation.id.clone();
        let list_id = list.id.clone();
        header = header
            .role("button")
            .label(&label)
            .expanded(expanded)
            .focusable(true)
            .radius(6.0)
            .focus_ring(2.0, -1.0, colors.fallbacks.accent)
            .on_press(move |state: &mut State| {
                if let Some(conversation) = state.conversation_mut(&thread_id)
                    && !conversation.expanded_todo_lists.remove(&list_id)
                {
                    conversation.expanded_todo_lists.insert(list_id.clone());
                }
            })
            .child(
                el().size(16.0, 16.0).center().child(
                    icon(if expanded { &EXPANDED } else { &COLLAPSED }, 12.0)
                        .color(colors.muted.alpha(0.7))
                        .a11y_hidden(),
                ),
            );
    }
    let mut card = col()
        .w_full()
        .px(2.0)
        .pt(2.0)
        .pb(if continues { 2.0 } else { 10.0 })
        .child(header);
    if expanded {
        let details = col()
            .key(format!("todo-list-steps-{}", list.id))
            .w_full()
            .pt(2.0)
            .pb(14.0)
            .max_h(320.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .children(list.steps.iter().map(|step| {
                let symbol = match step.status {
                    TodoStepStatus::Completed => '✓',
                    TodoStepStatus::Running => '◉',
                    TodoStepStatus::Pending => '○',
                };
                text(format!("{symbol} {}", step.text))
                    .key(format!("todo-list-step-{}-{}", list.id, step.id))
                    .font(DETAILS)
                    .color(colors.muted)
                    .wrap_anywhere()
                    .selectable(true)
                    .selection_color(colors.fallbacks.selected)
            }));
        card = card.child(details);
    }
    card
}

fn summary(list: &TodoListEntry) -> String {
    let completed = list
        .steps
        .iter()
        .filter(|step| step.status == TodoStepStatus::Completed)
        .count();
    let total = list.steps.len();
    let current = list
        .steps
        .iter()
        .find(|step| step.status == TodoStepStatus::Running)
        .or_else(|| {
            list.steps
                .iter()
                .find(|step| step.status == TodoStepStatus::Pending)
        });
    current.map_or_else(
        || format!("Tasks {completed}/{total}"),
        |step| format!("Tasks {completed}/{total} · {}", step.text),
    )
}
