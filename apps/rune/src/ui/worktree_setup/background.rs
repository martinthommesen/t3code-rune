use rune::color::{hex, oklch};
use rune::{
    Align, Color, El, Key, LoopSpec, ScrollSpec, Shadow, Side, TextStyle, Tx, col, el, icon, row,
    text,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{
    BODY, Colors, Conversation, LOADING, Palette, Phase, Presentation, ProjectView, State,
    Workspace, WorktreeSetup, agent_started, card, format_duration, heading_frame, now_for,
};

const TRIGGER: &str = "worktree-background-setup";
const DETAILS: &str = "worktree-setup-details";
const CHIP: TextStyle = TextStyle::new(11.0, 500.0).lh(17.875);

#[derive(Default)]
pub(in crate::ui) struct Disclosure {
    pub(super) open: bool,
    pub(super) details: bool,
}

pub(super) fn started_at<'a>(
    conversation: &Conversation,
    workspace: &'a Workspace,
) -> Option<&'a str> {
    let thread = workspace.thread(&conversation.id)?;
    if matches!(thread.status.as_str(), "running" | "waiting") {
        thread.run_started_at()
    } else {
        None
    }
}

pub(super) fn working_label(started: &str, now: OffsetDateTime) -> String {
    let seconds = OffsetDateTime::parse(started, &Rfc3339)
        .map_or(0, |started| (now - started).whole_seconds().max(0));
    let duration = if seconds < 60 {
        format!("{seconds}s")
    } else {
        format_duration(i128::from(seconds) * 1000)
    };
    format!("Working for {duration}")
}

pub(super) fn working(
    conversation: &Conversation,
    setup: Option<&WorktreeSetup>,
    started: &str,
    now: OffsetDateTime,
    colors: Colors,
) -> El {
    let mut line = row()
        .key("worktree-working-row")
        .w_full()
        .h(24.0)
        .gap(8.0)
        .items_start()
        .child(
            text(working_label(started, now))
                .font(BODY.features("'tnum' 1"))
                .color(colors.muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if let Some(setup) = setup {
        line = line.child(chip(conversation, setup, colors));
    }
    heading_frame(line, colors)
}

fn chip(conversation: &Conversation, setup: &WorktreeSetup, colors: Colors) -> El {
    let script = setup
        .setup_script
        .as_ref()
        .map_or("Setup script", |script| script.name.as_str());
    let open = conversation.worktree_background.open;
    let ink = if open { colors.ink } else { colors.muted };
    let surface = if colors.dark {
        Color::WHITE.alpha(0.04)
    } else {
        oklch(0.967, 0.001, 286.375)
    };
    let ring = if colors.dark {
        oklch(0.571, 0.21, 264.0)
    } else {
        oklch(0.488, 0.217, 264.0)
    };
    let thread = conversation.id.clone();
    row()
        .key(TRIGGER)
        .role("button")
        .label(format!(
            "Background setup is still running: {script}. Show setup progress."
        ))
        .expanded(open)
        .focusable(true)
        .focus_ring(2.0, -4.0, ring)
        .w(119.765625)
        .h(20.0)
        .px(4.0)
        .gap(2.0)
        .center_y()
        .radius(4.0)
        .fill(if open { surface } else { Color::TRANSPARENT })
        .hover(surface)
        .on_press_tx(move |state: &mut State, tx| {
            if let Some(conversation) = state.conversation_mut(&thread) {
                let open = !conversation.worktree_background.open;
                conversation.worktree_background = Disclosure {
                    open,
                    details: false,
                };
                if open {
                    tx.focus_when_shown(DETAILS, tx.focus_visible());
                }
            }
        })
        .child(
            icon(&LOADING, 12.0)
                .color(ink)
                .looping(LoopSpec::rotation(1.0))
                .a11y_hidden(),
        )
        .child(text("Background setup").font(CHIP).color(ink).truncate())
}

pub(in crate::ui) fn overlay(state: &State, palette: Palette, width: f32) -> Option<[El; 2]> {
    let workspace = state.workspace.as_ref()?;
    let ProjectView::Conversation(conversation) = &workspace.selection.as_ref()?.view else {
        return None;
    };
    let setup = conversation
        .worktree_setup
        .as_deref()
        .filter(|setup| setup.phase == Phase::Running && agent_started(setup))?;
    if !conversation.worktree_background.open || started_at(conversation, workspace).is_none() {
        return None;
    }
    let colors = Colors::new(palette);
    let (fill, blur, tint, spread) = if colors.dark {
        (
            hex("#111111").expect("static background color"),
            16.0,
            0.8,
            44.0,
        )
    } else {
        (Color::WHITE, 12.0, 0.55, 40.0)
    };
    let thread = conversation.id.clone();
    let outside = thread.clone();
    let scrim = el()
        .key("worktree-background-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(move |state: &mut State, tx| close(state, &outside, false, tx));
    let panel = col()
        .key("worktree-background-popup")
        .role("dialog")
        .label("Background setup")
        .anchored_fit(TRIGGER, Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .w((width - 32.0).clamp(0.0, 384.0))
        .min_h(0.0)
        .radius(8.0)
        .fill(fill.alpha(0.836))
        .backdrop_blur(blur)
        .border(1.0, colors.ink.alpha(0.1))
        .border_layout()
        .box_shadow(Shadow {
            x: 0.0,
            y: if colors.dark { 18.0 } else { 16.0 },
            blur: spread,
            spread: -18.0,
            color: Color::BLACK.alpha(tint),
            inset: false,
        })
        .on_key(move |state: &mut State, event, tx| {
            if event.key != Key::Escape {
                return false;
            }
            close(state, &thread, true, tx);
            true
        })
        .child(
            col()
                .w_full()
                .min_h(0.0)
                .shrink()
                .px(12.0)
                .py(8.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(card(
                    conversation,
                    setup,
                    workspace,
                    Presentation::Popup,
                    now_for(conversation),
                    colors,
                )),
        );
    Some([scrim, panel])
}

fn close(state: &mut State, thread: &str, keyboard: bool, tx: &mut Tx) {
    if let Some(conversation) = state.conversation_mut(thread) {
        conversation.worktree_background = Disclosure::default();
        tx.focus(TRIGGER, keyboard);
    }
}
