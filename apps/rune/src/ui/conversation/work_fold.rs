use rune::color::hex;
use rune::{El, Icon, col, el, icon, row, text};

use super::{Colors, Conversation, State};
use crate::connection::CompletedWork;

static COLLAPSED: Icon = glyph!("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static EXPANDED: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);

pub(super) fn view(
    conversation: &Conversation,
    run_id: Option<&str>,
    work: Option<&CompletedWork>,
    colors: Colors,
    child: impl FnOnce() -> El,
) -> Option<El> {
    let (Some(run_id), Some(work)) = (run_id, work) else {
        return Some(child());
    };
    let expanded = conversation.expanded_work_runs.contains(run_id);
    if !work.first_row {
        return expanded.then(child);
    }
    let label = work.elapsed_ms.map_or_else(
        || "Worked".to_owned(),
        |ms| format!("Worked for {}", super::worktree_setup::format_duration(ms)),
    );
    let fold = work_fold(conversation, run_id, label, expanded, colors);
    Some(col().w_full().child(fold).children(expanded.then(child)))
}

fn work_fold(
    conversation: &Conversation,
    run_id: &str,
    label: String,
    expanded: bool,
    colors: Colors,
) -> El {
    let thread_id = conversation.id.clone();
    let key = format!("turn-fold-{run_id}");
    let run_id = run_id.to_owned();
    let button = row()
        .key(key)
        .role("button")
        .label(&label)
        .expanded(expanded)
        .focusable(true)
        .radius(6.0)
        .focus_ring(2.0, -1.0, colors.fallbacks.accent)
        .px(4.0)
        .gap(4.0)
        .center_y()
        .on_press(move |state: &mut State| {
            let Some(conversation) = state.conversation_mut(&thread_id) else {
                return;
            };
            if conversation.expanded_work_runs.remove(&run_id) {
                super::command_row::hide_run(state, &thread_id, &run_id);
            } else {
                conversation.expanded_work_runs.insert(run_id.clone());
            }
        })
        .child(text(&label).font(super::PROSE).color(colors.muted))
        .child(
            icon(if expanded { &EXPANDED } else { &COLLAPSED }, 14.0)
                .color(colors.muted)
                .a11y_hidden(),
        );
    let divider = if colors.fallbacks.dark {
        "#0c0c0c"
    } else {
        "#eeeef0"
    };
    col()
        .w_full()
        .pt(4.0)
        .pb(6.0)
        .child(row().w_full().pr(2.0).pb(8.0).child(button))
        .child(
            el().w_full()
                .h(1.0)
                .fill(hex(divider).expect("static work divider color")),
        )
}
