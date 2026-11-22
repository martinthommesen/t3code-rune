use rune::color::oklch;
use rune::{El, TextStyle, col, el, row, text};

use super::conversation::{Layout, MAX_LANE};
use super::project::clone_banner;
use super::{Draft, Palette, Project, State, Workspace, sidebar};

mod workspace;
pub(super) use workspace::Popup as WorkspacePopup;

const SUBTITLE: TextStyle = TextStyle::new(16.0, 500.0).lh(24.0);
const CHOOSE_PROJECT: &str = "Choose project";

pub(super) fn gutter(layout: Layout) -> f32 {
    if layout.viewport[0] < 640.0 {
        12.0
    } else {
        48.0
    }
}

pub(super) fn view(
    state: &State,
    workspace: &Workspace,
    project: &Project,
    draft: &Draft,
    layout: Layout,
    palette: Palette,
    form: El,
) -> El {
    let [viewport_width, viewport_height] = layout.viewport;
    let width = (layout.pane_width - 2.0 * gutter(layout)).clamp(0.0, MAX_LANE);
    let compact = viewport_height <= 544.0;
    let scratch = workspace.scratch_project.as_deref() == Some(project.id.as_str());
    let clearance = if viewport_width < 640.0 { 16.0 } else { 20.0 };
    let mut stack = col()
        .key("draft-composer-stack")
        .w_full()
        .min_w(0.0)
        .max_w(MAX_LANE)
        .child(form);
    if scratch {
        stack = stack.child(el().h(32.0).a11y_hidden());
    } else {
        stack = stack.child(workspace::view(
            state,
            workspace,
            &project.id,
            draft,
            compact,
            width,
            palette,
        ));
    }
    let clone = workspace
        .pending_clone(&project.id)
        .map(|clone| clone_banner::view(workspace, clone, palette));
    if let Some(banner) = clone {
        let banner = if compact {
            banner.mt(16.0)
        } else {
            banner.pin(Some(0.0), None, Some(0.0), Some(clearance - 50.0))
        };
        stack = stack.child(banner);
    }
    stack = stack.child(el().h(clearance));
    let (headline, trigger_center) = headline(
        state,
        project,
        width,
        layout,
        draft.pending || workspace.reconnecting,
        scratch,
        palette,
    );
    stack = stack.child(headline);
    if sidebar::draft_picker_open(state) {
        let popup_width = width.min(288.0);
        let left = (trigger_center - popup_width / 2.0).clamp(0.0, width - popup_width);
        if let Some(picker) =
            sidebar::draft_project_picker(workspace, project, left, popup_width, palette.dark)
        {
            stack = stack.child(picker);
        }
    }
    let body = col()
        .key("draft-composer")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(gutter(layout))
        .items_center()
        .child(stack);
    if compact {
        body.justify_end()
    } else {
        body.center()
    }
}

fn headline(
    state: &State,
    project: &Project,
    width: f32,
    layout: Layout,
    disabled: bool,
    scratch: bool,
    palette: Palette,
) -> (El, f32) {
    let [window_width, window_height] = layout.viewport;
    let phrase = "What should we build in";
    let ink = if palette.dark {
        oklch(0.97, 0.0, 0.0)
    } else {
        oklch(0.21, 0.006, 285.885)
    };
    let (size, height) = if window_width < 640.0 {
        (24.0, 32.0)
    } else {
        (30.0, 36.0)
    };
    let regular = TextStyle::new(size, 400.0)
        .lh(height)
        .tracking(-size * 0.025);
    let medium = TextStyle::new(size, 500.0)
        .lh(height)
        .tracking(-size * 0.025);
    let name_width = rune::text::measure(&project.name, &[], medium, None).0;
    let lead = rune::text::measure(phrase, &[], regular, None).0 + 8.0;
    let total = lead + name_width + rune::text::measure("?", &[], regular, None).0;
    let show_greeting = window_height > 448.0;
    let inline = !scratch && show_greeting && window_width >= 1024.0 && total <= width;
    let project_id = project.id.clone();
    let button = el()
        .key("draft-project-selector")
        .max_w(width)
        .center()
        .radius(4.0)
        .role("button")
        .label(CHOOSE_PROJECT)
        .help(&project.name)
        .expanded(sidebar::draft_picker_open(state))
        .disabled(disabled)
        .focusable(!disabled)
        .focus_ring(1.0, -1.0, palette.accent)
        .on_press_tx(move |state: &mut State, tx| {
            sidebar::toggle_draft_project_picker(state, &project_id, tx)
        })
        .child(
            text(&project.name)
                .font(if inline { medium } else { SUBTITLE })
                .color(ink)
                .truncate(),
        );
    let (content, height, center) = if inline {
        (
            row()
                .center()
                .gap(8.0)
                .child(text(phrase).font(regular).color(ink))
                .child(
                    row()
                        .center_y()
                        .child(button)
                        .child(text("?").font(regular).color(ink)),
                ),
            height,
            (width - total) / 2.0 + lead + name_width / 2.0,
        )
    } else {
        let mut content = col().items_center().gap(8.0);
        if show_greeting {
            content = content.child(
                text(if scratch {
                    "What should we work on?"
                } else {
                    "What should we build?"
                })
                .font(regular)
                .color(ink)
                .truncate(),
            );
        }
        (
            content.child(button),
            24.0 + if show_greeting { height + 8.0 } else { 0.0 },
            width / 2.0,
        )
    };
    let composer_gap = if scratch { 16.0 } else { 48.0 };
    (
        content.key("draft-headline").role("heading").pin(
            Some(0.0),
            Some(-height - composer_gap),
            Some(0.0),
            None,
        ),
        center,
    )
}
