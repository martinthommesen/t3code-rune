use rune::{El, Icon, ScrollSpec, col};
use time::OffsetDateTime;

use super::super::conversation::{self, Layout, MAX_LANE};
use super::{
    Colors, FAILED, Palette, Phase, Presentation, State, Workspace, WorktreeSetup, action,
    action_row, can_cancel_before_handoff, card_content, details, details_action, heading,
};

static LAPTOP: Icon = Icon {
    name: "setup-work-locally",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 16V7a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v9m16 0 2 3H2l2-3m0 0h16"/></g>"#,
};

pub(in crate::ui) struct DraftLaunch {
    pub(in crate::ui) thread_id: String,
    pub(in crate::ui) prompt: String,
    pub(in crate::ui) snapshot: Option<Box<WorktreeSetup>>,
    pub(in crate::ui) accepted: bool,
    pub(in crate::ui) cancelling: bool,
    pub(in crate::ui) details_open: bool,
}

impl DraftLaunch {
    pub(in crate::ui) fn new(thread_id: String, prompt: String) -> Self {
        Self {
            thread_id,
            prompt,
            snapshot: None,
            accepted: false,
            cancelling: false,
            details_open: false,
        }
    }

    pub(in crate::ui) fn can_cancel(&self) -> bool {
        self.snapshot
            .as_deref()
            .is_some_and(can_cancel_before_handoff)
    }

    pub(in crate::ui) fn update(&mut self, snapshot: Option<Box<WorktreeSetup>>) {
        if let Some(snapshot) = snapshot
            && self.snapshot.as_ref().is_none_or(|previous| {
                previous.started_at != snapshot.started_at || previous.sequence <= snapshot.sequence
            })
        {
            if snapshot.phase != Phase::Running {
                self.cancelling = false;
            }
            self.snapshot = Some(snapshot);
        }
    }
}

pub(in crate::ui) fn draft_transcript(
    launch: &DraftLaunch,
    workspace: &Workspace,
    layout: Layout,
    palette: Palette,
) -> El {
    let colors = Colors::new(palette);
    let mut lane =
        col()
            .w_full()
            .max_w(MAX_LANE)
            .pt(24.0)
            .pb(16.0)
            .child(conversation::draft_prompt(
                &launch.thread_id,
                &launch.prompt,
                layout,
                palette,
            ));
    if launch
        .snapshot
        .as_ref()
        .is_none_or(|snapshot| snapshot.phase == Phase::Running)
    {
        lane = lane.child(heading("Setting up worktree…", None, colors.muted, colors));
    }
    if let Some(snapshot) = launch.snapshot.as_deref() {
        let mut card = card_content(
            snapshot,
            Presentation::Full,
            OffsetDateTime::now_utc(),
            colors,
        )
        .child(draft_actions(launch, workspace, colors));
        if launch.details_open {
            card = card.child(details(snapshot, colors));
        }
        lane = lane.child(card);
    }
    col()
        .key("draft-worktree-transcript")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(layout.timeline_gutter())
        .items_center()
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(24.0)
        .child(lane)
}

fn draft_actions(launch: &DraftLaunch, workspace: &Workspace, colors: Colors) -> El {
    let id = launch.thread_id.clone();
    let mut controls = action_row().child(details_action(launch.details_open, colors).on_press(
        move |state: &mut State| {
            if let Some(launch) = state.active_worktree_draft_mut(&id) {
                launch.details_open = !launch.details_open;
            }
        },
    ));
    if launch.can_cancel() && workspace.worktree_launch && !workspace.reconnecting {
        let work_locally = action(
            "worktree-setup-work-locally",
            "Work locally",
            100.578125,
            &LAPTOP,
            colors,
        );
        let cancel = action("worktree-setup-cancel", "Cancel", 69.84375, &FAILED, colors);
        for button in [work_locally, cancel] {
            let id = launch.thread_id.clone();
            controls = controls.child(button.disabled(launch.cancelling).on_press(
                move |state: &mut State| {
                    state.cancel_draft_worktree(&id);
                },
            ));
        }
    }
    controls
}
