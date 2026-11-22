use rune::color::oklch;
use rune::{El, Key, ScrollSpec, Tx, col, row, text};

use super::{
    CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, error_text, icon_button, switch_track,
    workspace,
};

mod branches;
mod location;

pub(super) use branches::overlay;
pub(super) use location::LocationPanel;
pub(in crate::ui) use location::poll as poll_location;

const SWITCH: &str = "start-from-remote";
const LABEL: &str = "Start new worktrees from remote by default";
const DESCRIPTION_COPY: &str = concat!(
    "Uses the latest commit of the base branch’s tracked remote branch, even if its name differs. ",
    "Otherwise uses the same-named branch on origin. Uses your local branch if that remote branch is missing.",
);

pub(super) fn page(state: &State, palette: Palette, width: f32, large: bool) -> El {
    let settings = &state.settings.workspace;
    let origin = settings.origin_view(state);
    let enabled = origin.as_ref().is_none_or(|origin| origin.enabled);
    let writable = origin.as_ref().is_some_and(|origin| origin.writable);
    let blue = oklch(
        if palette.dark { 0.571 } else { 0.488 },
        if palette.dark { 0.21 } else { 0.217 },
        264.0,
    );
    let mut switch = switch_track(enabled, large, palette, blue)
        .key(SWITCH)
        .label(LABEL)
        .disabled(!writable)
        .focusable(writable)
        .opacity(if writable { 1.0 } else { 0.64 });
    if writable {
        switch = switch
            .on_press_tx(toggle)
            .on_key(|state: &mut State, event, tx| {
                if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ")
                {
                    toggle(state, tx);
                    true
                } else {
                    false
                }
            });
    } else if let Some(organization) = origin.as_ref().and_then(|origin| origin.managed_by) {
        switch = switch.help(format!("Managed by {organization}."));
    } else if origin.is_some() {
        switch = switch.help("This connection lacks permission to change environment settings.");
    }

    let mut heading = row()
        .gap(6.0)
        .items_center()
        .child(text("Start from remote").font(CONTROL).color(palette.ink));
    if settings.project_selected() {
        let inherited = if origin.as_ref().is_some_and(|origin| origin.may_reset) {
            "Overridden for this project".to_owned()
        } else {
            state
                .workspace
                .as_ref()
                .map_or("Inherited from environment".to_owned(), |workspace| {
                    format!("Inherited from {}", workspace.label)
                })
        };
        heading = heading.child(workspace::inheritance_marker(inherited, palette));
    }
    if writable && origin.is_some_and(|origin| origin.may_reset) {
        let project = settings.project_selected();
        let label = if project {
            "Reset Start from remote to default"
        } else {
            "Reset new worktrees start from remote to default"
        };
        let reset = icon_button(
            "reset-start-from-remote",
            label,
            &RESET,
            12.0,
            20.0,
            palette,
            reset,
        );
        heading = heading.child(if project {
            reset.help("Reset to inherited value")
        } else {
            reset
        });
    }

    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(heading).child(
        text(DESCRIPTION_COPY)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let row = if width >= 560.0 {
        row().items_center().gap(32.0).child(label).child(switch)
    } else {
        col().gap(12.0).child(label).child(switch)
    };
    let mut section = col().key("worktree-creation").w_full().gap(10.0).child(
        text("Creation")
            .font(SECTION)
            .color(palette.ink)
            .role("heading")
            .px(16.0),
    );
    let mut rows = col()
        .w_full()
        .radius(12.0)
        .border(1.0, palette.line)
        .border_layout();
    if let Some(location) = location::view(state, palette, width) {
        rows = rows
            .child(location)
            .child(rune::el().w_full().h(1.0).fill(palette.line));
    }
    section = section.child(rows.child(row.key("start-from-origin").w_full().px(16.0).py(12.0)));
    if let Some(error) = settings.error() {
        section = section.child(error_text(error, palette));
    }
    col()
        .key("settings-worktrees-content")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .padding(30.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .max_w(800.0)
                .gap(22.0)
                .child(workspace::scope_header(state, palette))
                .child(section)
                .child(branches::section(state, palette, width)),
        )
}

fn toggle(state: &mut State, _: &mut Tx) {
    if let Some(origin) = state.settings.workspace.origin_view(state) {
        workspace::save_origin(state, Some(!origin.enabled));
    }
}

fn reset(state: &mut State, tx: &mut Tx) {
    workspace::save_origin(state, None);
    tx.focus(SWITCH, tx.focus_visible());
}
