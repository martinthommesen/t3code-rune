use std::time::Duration;

use rune::color::{hex, oklch};
use rune::{Color, El, Icon, LoopSpec, TextStyle, Tx, col, el, icon, row, text};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::connection::worktree_setup::{Phase, Stage, StageId, Status, WorktreeSetup};

use super::{Conversation, Message, Palette, ProjectView, State, Workspace};

const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(22.75);
const SMALL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const BUTTON: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);
const CODE: TextStyle = TextStyle::new(11.0, 400.0).lh(17.875).mono();
const DETAILS_CODE: TextStyle = SMALL.mono();
const INDENT: f32 = 32.0;
const TAIL_LINES: usize = 4;

macro_rules! glyph {
    ($name:literal, $body:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $body, "</g>"),
        }
    };
}

static DONE: Icon = glyph!("setup-check", r#"<path d="m20 6-11 11-5-5"/>"#);
static LOADING: Icon = glyph!("setup-loading", r#"<path d="M21 12a9 9 0 1 1-9-9"/>"#);
static FAILED: Icon = glyph!("setup-close", r#"<path d="m18 6-12 12M6 6l12 12"/>"#);
static WARNING: Icon = glyph!(
    "setup-warning",
    r#"<circle cx="12" cy="12" r="10"/><path d="M12 8v4m0 4h.01"/>"#
);
static SKIPPED: Icon = glyph!("setup-minus", r#"<path d="M5 12h14"/>"#);
static PENDING: Icon = glyph!("setup-pending", r#"<circle cx="12" cy="12" r="10"/>"#);
static CHEVRON_DOWN: Icon = glyph!("setup-chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);
static CHEVRON_RIGHT: Icon = glyph!("setup-chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static TERMINAL: Icon = glyph!("setup-terminal", r#"<path d="m4 17 6-6-6-6m8 14h8"/>"#);

#[derive(Clone, Copy)]
struct Colors {
    ink: Color,
    muted: Color,
    warning: Color,
    error: Color,
    destructive: Color,
    code: Color,
    frame: Color,
    hover: Color,
    accent: Color,
    dark: bool,
}

impl Colors {
    fn new(palette: Palette) -> Self {
        let dark = palette.dark;
        let (ink, muted, warning, error, destructive, code, frame) = if dark {
            (
                oklch(0.97, 0.0, 0.0),
                oklch(0.708, 0.0, 0.0),
                oklch(0.828, 0.189, 84.429),
                oklch(0.704, 0.191, 22.216),
                hex("#fb414a").expect("static error color"),
                hex("#101010").expect("static code color"),
                Color::WHITE.alpha(0.08),
            )
        } else {
            (
                oklch(0.21, 0.006, 285.885),
                oklch(0.552, 0.016, 285.938),
                oklch(0.555, 0.163, 48.998),
                oklch(0.505, 0.213, 27.518),
                oklch(0.637, 0.237, 25.331),
                Color::WHITE,
                hex("#e4e4e7").expect("static code border"),
            )
        };
        Self {
            ink,
            muted,
            warning,
            error,
            destructive,
            code,
            frame,
            hover: ink.alpha(0.06),
            accent: palette.accent,
            dark,
        }
    }
}

pub(super) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let Some(ProjectView::Conversation(conversation)) = state
        .workspace
        .as_mut()
        .and_then(|workspace| workspace.selection.as_mut())
        .map(|selection| &mut selection.view)
    else {
        return false;
    };
    if conversation
        .worktree_setup
        .as_deref()
        .is_none_or(|setup| setup.phase != Phase::Running || agent_started(setup))
    {
        return false;
    }
    let now = OffsetDateTime::now_utc();
    let second = now.unix_timestamp();
    let changed = conversation.worktree_elapsed_second.replace(second) != Some(second);
    tx.wake_after(Duration::from_nanos(
        1_000_000_000 - u64::from(now.nanosecond()),
    ));
    changed
}

pub(super) fn view(
    conversation: &Conversation,
    messages: &[Message],
    workspace: &Workspace,
    palette: Palette,
) -> Option<El> {
    let setup = conversation.worktree_setup.as_deref()?;
    let thread = workspace.thread(&conversation.id);
    let activity_started = thread.is_some_and(|thread| thread.has_started_run())
        || messages.iter().any(|message| message.role == "assistant");
    let handed_off = activity_started && agent_started(setup);
    let running = setup.phase == Phase::Running;
    let failed_stage = setup
        .stages
        .iter()
        .any(|stage| stage.status == Status::Failed);
    let follow_up = messages
        .iter()
        .filter(|message| message.role == "user" && message.created_by.as_deref() != Some("agent"))
        .nth(1)
        .is_some();
    if (running && handed_off)
        || (!running && follow_up)
        || (setup.phase == Phase::Done && activity_started && !failed_stage)
    {
        return None;
    }
    let colors = Colors::new(palette);
    let now = conversation
        .worktree_elapsed_second
        .and_then(|second| OffsetDateTime::from_unix_timestamp(second).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    let mut section = col()
        .key("worktree-setup")
        .role("group")
        .label("Worktree setup")
        .w_full()
        .min_w(0.0);
    if handed_off && !running {
        let status = if failed_stage || matches!(setup.phase, Phase::Failed | Phase::Cancelled) {
            Status::Failed
        } else {
            Status::Done
        };
        section = section.child(stage_line(
            "worktree-setup-summary",
            header_label(setup),
            status,
            None,
            elapsed(Some(&setup.started_at), setup.ended_at.as_deref(), None),
            colors,
        ));
    } else {
        if matches!(setup.phase, Phase::Failed | Phase::Cancelled) {
            section = section.child(heading(
                header_label(setup),
                elapsed(Some(&setup.started_at), setup.ended_at.as_deref(), None),
                if setup.phase == Phase::Failed {
                    colors.error
                } else {
                    colors.muted
                },
                colors,
            ));
        }
        for stage in &setup.stages {
            section = section.child(stage_view(stage, setup, now, colors));
        }
    }
    if setup.phase == Phase::Failed
        && let Some(error) = &setup.error
    {
        section = section.child(
            text(error)
                .font(SMALL)
                .color(colors.muted)
                .w_full()
                .pl(INDENT)
                .mt(4.0)
                .wrap_anywhere(),
        );
    }
    let can_cancel = running
        && !handed_off
        && !setup.stages.iter().any(|stage| {
            stage.id == StageId::Agent && matches!(stage.status, Status::Running | Status::Done)
        })
        && workspace.worktree_launch
        && !workspace.reconnecting
        && thread.is_some_and(|thread| !thread.archived);
    let can_open_terminal = setup.phase != Phase::Cancelled
        && workspace.terminal
        && !workspace.reconnecting
        && setup
            .stages
            .iter()
            .any(|stage| stage.id == StageId::SetupScript && stage.status != Status::Pending);
    section = section.child(actions(
        conversation,
        setup,
        can_cancel,
        can_open_terminal,
        colors,
    ));
    if conversation.worktree_details_open {
        section = section.child(details(setup, colors));
    }
    if !handed_off && matches!(setup.phase, Phase::Running | Phase::Done) {
        let title = if running {
            "Setting up worktree…".into()
        } else {
            elapsed(Some(&setup.started_at), None, Some(now)).map_or_else(
                || "Working...".into(),
                |elapsed| format!("Working for {elapsed}"),
            )
        };
        Some(
            col()
                .w_full()
                .child(heading(&title, None, colors.muted, colors))
                .child(section),
        )
    } else {
        Some(section)
    }
}

fn agent_started(setup: &WorktreeSetup) -> bool {
    setup
        .stages
        .iter()
        .any(|stage| stage.id == StageId::Agent && stage.status == Status::Done)
}

fn header_label(setup: &WorktreeSetup) -> &'static str {
    match setup.phase {
        Phase::Running => "Setting up worktree…",
        Phase::Done
            if setup
                .stages
                .iter()
                .any(|stage| stage.status == Status::Failed) =>
        {
            "Worktree ready, setup script failed"
        }
        Phase::Done => "Worktree ready",
        Phase::Failed => "Worktree setup failed",
        Phase::Cancelled => "Worktree setup cancelled",
    }
}

fn heading(label: &str, duration: Option<String>, ink: Color, colors: Colors) -> El {
    let mut line = row().w_full().h(24.0).center_y().gap(8.0).child(
        text(label)
            .font(BODY)
            .color(ink)
            .grow(1.0)
            .min_w(0.0)
            .truncate(),
    );
    if let Some(duration) = duration {
        line = line.child(text(duration).font(SMALL).color(colors.muted));
    }
    col()
        .w_full()
        .h(37.0)
        .px(4.0)
        .pt(4.0)
        .border_sides(
            [0.0, 0.0, 1.0, 0.0],
            colors.frame.alpha(colors.frame.a * 0.6),
        )
        .child(line)
        .mb(6.0)
}

fn stage_view(stage: &Stage, setup: &WorktreeSetup, now: OffsetDateTime, colors: Colors) -> El {
    let label = match stage.id {
        StageId::Fetch => "Fetch base branch",
        StageId::Checkout => "Check out files",
        StageId::Submodules => "Init submodules",
        StageId::SetupScript => setup
            .setup_script
            .as_ref()
            .map_or("Run setup script", |script| &script.name),
        StageId::Agent => "Start agent",
    };
    let key = match stage.id {
        StageId::Fetch => "worktree-stage-fetch",
        StageId::Checkout => "worktree-stage-checkout",
        StageId::Submodules => "worktree-stage-submodules",
        StageId::SetupScript => "worktree-stage-script",
        StageId::Agent => "worktree-stage-agent",
    };
    let detail = match stage.status {
        Status::Pending => None,
        Status::Skipped => Some(stage.detail.clone().unwrap_or_else(|| "skipped".into())),
        Status::Running if stage.id == StageId::Checkout && stage.percent.is_some() => {
            stage.percent.map(|percent| format!("{percent}%"))
        }
        _ => stage.detail.clone(),
    };
    let duration = (!matches!(stage.status, Status::Skipped | Status::Pending))
        .then(|| {
            elapsed(
                stage.started_at.as_deref(),
                stage.ended_at.as_deref().or(setup.ended_at.as_deref()),
                (setup.phase == Phase::Running).then_some(now),
            )
        })
        .flatten();
    let line = stage_line(key, label, stage.status, detail, duration, colors);
    if stage.id == StageId::SetupScript
        && (stage.status == Status::Failed
            || (stage.status == Status::Running && !stage.tail.is_empty()))
    {
        col().w_full().child(line).child(output_tail(
            &stage.tail,
            stage.status == Status::Failed,
            colors,
        ))
    } else {
        line
    }
}

fn stage_line(
    key: &str,
    label: &str,
    status: Status,
    detail: Option<String>,
    duration: Option<String>,
    colors: Colors,
) -> El {
    let symbol = match status {
        Status::Pending => &PENDING,
        Status::Running => &LOADING,
        Status::Done => &DONE,
        Status::Skipped => &SKIPPED,
        Status::Warning => &WARNING,
        Status::Failed => &FAILED,
    };
    let ink = match status {
        Status::Pending => colors.muted.alpha(0.4),
        Status::Warning => colors.warning,
        Status::Failed => colors.error,
        _ => colors.muted,
    };
    let mut glyph = icon(symbol, 16.0)
        .a11y_hidden()
        .color(if status == Status::Pending {
            ink
        } else {
            colors.muted
        });
    if status == Status::Running {
        glyph = glyph.looping(LoopSpec::rotation(1.0));
    }
    let mut line = row()
        .key(key)
        .w_full()
        .h(28.0)
        .px(2.0)
        .gap(6.0)
        .center_y()
        .child(el().w(24.0).h(24.0).center().child(glyph))
        .child(
            text(label)
                .font(BODY)
                .color(ink)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if let Some(detail) = detail {
        line = line.child(text(detail).font(SMALL).color(colors.muted).truncate());
    }
    if let Some(duration) = duration {
        line = line.child(text(duration).font(SMALL).color(colors.muted));
    }
    line
}

fn output_tail(lines: &[String], failed: bool, colors: Colors) -> El {
    let start = lines.len().saturating_sub(TAIL_LINES);
    let visible = &lines[start..];
    let leading = TAIL_LINES.saturating_sub(visible.len());
    let tail = col()
        .key("worktree-setup-tail")
        .role("group")
        .label("Setup output")
        .w_full()
        .min_w(0.0)
        .px(10.0)
        .py(6.0)
        .radius(6.0)
        .border(
            1.0,
            if failed {
                colors.destructive.alpha(0.2)
            } else {
                colors.frame
            },
        )
        .border_layout()
        .fill(if failed {
            colors
                .destructive
                .alpha(if colors.dark { 0.16 } else { 0.08 })
        } else {
            colors.code
        })
        .children((0..TAIL_LINES).map(|index| {
            let line = index
                .checked_sub(leading)
                .and_then(|index| visible.get(index))
                .map_or("\u{a0}", String::as_str);
            text(line)
                .font(CODE)
                .h(17.875)
                .w_full()
                .color(if failed { colors.error } else { colors.muted })
                .truncate()
                .selectable(true)
        }));
    col().w_full().pl(INDENT).pb(4.0).child(tail)
}

fn actions(
    conversation: &Conversation,
    setup: &WorktreeSetup,
    cancel: bool,
    terminal: bool,
    colors: Colors,
) -> El {
    let thread_id = conversation.id.clone();
    let expanded = conversation.worktree_details_open;
    let mut controls = row()
        .w_full()
        .pl(19.0)
        .pt(2.0)
        .gap(2.0)
        .center_y()
        .flex_wrap()
        .child(
            action(
                "worktree-setup-details",
                "Details",
                69.1875,
                if expanded {
                    &CHEVRON_DOWN
                } else {
                    &CHEVRON_RIGHT
                },
                colors,
            )
            .expanded(expanded)
            .on_press(move |state: &mut State| {
                if let Some(conversation) = state.conversation_mut(&thread_id) {
                    conversation.worktree_details_open = !conversation.worktree_details_open;
                }
            }),
        );
    if terminal && let Some(script) = &setup.setup_script {
        let terminal_id = script.terminal_id.clone();
        let thread_id = conversation.id.clone();
        controls = controls.child(
            action(
                "worktree-setup-open-terminal",
                "Open terminal",
                110.640625,
                &TERMINAL,
                colors,
            )
            .on_press_tx(move |state: &mut State, tx| {
                state.open_worktree_setup_terminal(&thread_id, &terminal_id, tx)
            }),
        );
    }
    if cancel {
        let thread_id = conversation.id.clone();
        controls = controls.child(
            action("worktree-setup-cancel", "Cancel", 69.84375, &FAILED, colors)
                .disabled(conversation.worktree_cancel_pending)
                .on_press(move |state: &mut State| state.cancel_worktree_setup(&thread_id)),
        );
    }
    controls
}

fn action(key: &str, label: &str, width: f32, symbol: &'static Icon, colors: Colors) -> El {
    row()
        .key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .focus_ring(2.0, -1.0, colors.accent)
        .w(width)
        .h(24.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(8.0)
        .hover(colors.hover)
        .child(icon(symbol, 14.0).color(colors.muted).a11y_hidden())
        .child(text(label).font(BUTTON).color(colors.muted))
}

fn details(setup: &WorktreeSetup, colors: Colors) -> El {
    col()
        .key("worktree-setup-metadata")
        .w_full()
        .pl(INDENT)
        .pt(4.0)
        .gap(2.0)
        .children(
            [
                ("Branch", setup.branch.as_deref()),
                ("Base", setup.base_ref.as_deref()),
                ("Path", setup.worktree_path.as_deref()),
                (
                    "Setup",
                    setup
                        .setup_script
                        .as_ref()
                        .map(|script| script.command.as_str()),
                ),
            ]
            .into_iter()
            .filter_map(|(label, value)| {
                value.map(|value| {
                    row()
                        .w_full()
                        .h(16.0)
                        .gap(12.0)
                        .child(
                            text(label)
                                .w(40.15625)
                                .font(SMALL)
                                .color(colors.ink.alpha(0.8)),
                        )
                        .child(
                            text(value)
                                .font(DETAILS_CODE)
                                .color(colors.muted)
                                .grow(1.0)
                                .min_w(0.0)
                                .truncate()
                                .selectable(true),
                        )
                })
            }),
        )
}

fn elapsed(
    started: Option<&str>,
    ended: Option<&str>,
    now: Option<OffsetDateTime>,
) -> Option<String> {
    let start = OffsetDateTime::parse(started?, &Rfc3339).ok()?;
    let end = ended
        .map(|timestamp| OffsetDateTime::parse(timestamp, &Rfc3339).ok())
        .unwrap_or(now)?;
    let milliseconds = (end - start).whole_milliseconds().max(0);
    if milliseconds < 1000 {
        return Some(format!("{}ms", milliseconds.max(1)));
    }
    if milliseconds < 10_000 {
        return Some(format!("{}s", ((milliseconds + 50) / 100) as f64 / 10.0));
    }
    let seconds = (milliseconds + 500) / 1000;
    let duration = match (seconds / 3600, seconds / 60 % 60, seconds % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, 0) => format!("{h}h"),
        (h, 0, s) => format!("{h}h {s}s"),
        (h, m, s) => format!("{h}h {m}m {s}s"),
    };
    Some(duration)
}
