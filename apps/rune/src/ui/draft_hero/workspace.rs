use rune::color::{hex, oklch};
use rune::{
    Color, El, Icon, Key, KeyEvent, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text,
    text_input,
};

use crate::connection::GitRef;

use super::super::{Draft, Palette, State, Workspace, WorkspaceMode, sidebar};

const LABEL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const SMALL: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);
const ITEM: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const BADGE: TextStyle = TextStyle::new(10.0, 400.0).lh(14.0);
const INSET: f32 = 8.0;
const ORIGIN_HELP: &str = concat!(
    "Uses the latest commit of the base branch’s tracked remote branch, even if its name differs. ",
    "Otherwise uses the same-named branch on origin. Uses your local branch if that remote branch is missing.",
);

macro_rules! glyph {
    ($name:literal, $body:literal) => {
        Icon { name: $name, view: 24.0, body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $body, "</g>") }
    };
}

static FOLDER: Icon = glyph!(
    "draft-current-checkout",
    r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#
);
static WORKTREE: Icon = glyph!(
    "draft-new-worktree",
    r#"<path d="M18 19a5 5 0 0 1-5-5v8"/><path d="M9 20H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H20a2 2 0 0 1 2 2v5"/><circle cx="13" cy="12" r="2"/><circle cx="20" cy="19" r="2"/>"#
);
static BRANCH: Icon = glyph!(
    "draft-base-branch",
    r#"<path d="M15 6a9 9 0 0 0-9 9V3"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/>"#
);
static SEARCH: Icon = glyph!(
    "draft-refs-search",
    r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.34-4.34"/>"#
);
static REFRESH: Icon = glyph!(
    "draft-refresh-branch",
    r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#
);

pub(in crate::ui) enum Popup {
    Mode,
    BaseRef {
        query: Box<TextEdit>,
        active: Option<String>,
    },
}

#[derive(Clone, Copy)]
struct Colors {
    ink: Color,
    muted: Color,
    panel: Color,
    mode_panel: Color,
    border: Color,
    selected: Color,
    hover: Color,
    focus: Color,
    dark: bool,
}

impl Colors {
    fn new(palette: Palette) -> Self {
        let (ink, muted, panel) = if palette.dark {
            (oklch(0.97, 0.0, 0.0), oklch(0.708, 0.0, 0.0), "#101010")
        } else {
            (
                oklch(0.21, 0.006, 285.885),
                oklch(0.552, 0.016, 285.938),
                "#ffffff",
            )
        };
        Self {
            ink,
            muted,
            panel: hex(panel).expect("static popup palette"),
            mode_panel: if palette.dark {
                hex("#0e0e0e").expect("static workspace popup palette")
            } else {
                Color::WHITE
            },
            border: palette.line,
            selected: ink.alpha(0.08),
            hover: ink.alpha(0.04),
            focus: palette.accent,
            dark: palette.dark,
        }
    }
}

fn graphic(symbol: &'static Icon, size: f32, color: Color) -> El {
    icon(symbol, size).color(color).a11y_hidden()
}

fn mode_details(mode: WorkspaceMode) -> (&'static str, &'static Icon, f32) {
    match mode {
        WorkspaceMode::Local => ("Current checkout", &FOLDER, 132.61),
        WorkspaceMode::Worktree => ("New worktree", &WORKTREE, 113.34),
    }
}

pub(super) fn view(
    state: &State,
    workspace: &Workspace,
    project_id: &str,
    draft: &Draft,
    compact: bool,
    width: f32,
    palette: Palette,
) -> El {
    let colors = Colors::new(palette);
    let (mode_text, mode_icon, trigger_width) = mode_details(draft.workspace_mode);
    let available = workspace.worktree_launch
        && workspace.pending_clone(project_id).is_none()
        && draft.refs.is_repo == Some(true)
        && !draft.pending
        && !draft.refs.switching
        && !workspace.reconnecting;
    let branch_available =
        available && (draft.workspace_mode == WorkspaceMode::Worktree || workspace.checkout_switch);
    let mode_open = available && matches!(state.draft_workspace_popup, Some(Popup::Mode));
    let remote = workspace.remote_worktree_launch;
    let selected = draft
        .refs
        .branches
        .iter()
        .find(|branch| match draft.workspace_mode {
            WorkspaceMode::Local => !branch.is_remote && branch.current,
            WorkspaceMode::Worktree => {
                (remote || !branch.is_remote)
                    && draft.base_ref.as_deref() == Some(branch.name.as_str())
            }
        });
    let mut mode_trigger = trigger(mode_text, mode_icon, colors).w(trigger_width);
    if available {
        let selected_mode = draft.workspace_mode;
        mode_trigger = mode_trigger
            .key("draft-workspace-mode")
            .role("combobox")
            .label("Workspace")
            .a11y_value(mode_text)
            .expanded(mode_open)
            .focusable(true)
            .focus_ring(1.0, -1.0, colors.focus)
            .fill(if mode_open {
                colors.hover
            } else {
                Color::TRANSPARENT
            })
            .hover(colors.hover)
            .on_press_tx(move |state: &mut State, tx| {
                sidebar::close_draft_project_picker(&mut state.sidebar);
                if matches!(state.draft_workspace_popup, Some(Popup::Mode)) {
                    state.draft_workspace_popup = None;
                } else {
                    state.draft_workspace_popup = Some(Popup::Mode);
                    tx.focus_when_shown(mode_key(selected_mode), tx.focus_visible());
                }
            });
    }
    let mut bar = row()
        .key("draft-workspace-context")
        .w_full()
        .h(32.0)
        .pl(INSET)
        .pr(branch_trailing_inset(width))
        .pt(8.0)
        .gap(4.0)
        .center_y()
        .paint_order(1)
        .child(mode_trigger)
        .child(el().grow(1.0));
    if branch_available {
        let name = selected.map_or_else(
            || "Select ref".into(),
            |branch| {
                if draft.workspace_mode == WorkspaceMode::Local {
                    branch.name.clone()
                } else if remote && draft.start_from_origin && !branch.is_remote {
                    format!("From {} (remote)", branch.name)
                } else {
                    format!("From {}", branch.name)
                }
            },
        );
        let project_id = project_id.to_owned();
        let open = matches!(state.draft_workspace_popup, Some(Popup::BaseRef { .. }));
        bar = bar.child(
            trigger(&name, &BRANCH, colors)
                .key("draft-base-branch")
                .min_w(0.0)
                .role("combobox")
                .label(format!("Branch: {name}"))
                .a11y_value(&name)
                .expanded(open)
                .focusable(true)
                .hover(colors.hover)
                .focus_ring(1.0, -1.0, colors.focus)
                .on_press_tx(move |state: &mut State, tx| {
                    sidebar::close_draft_project_picker(&mut state.sidebar);
                    if matches!(state.draft_workspace_popup, Some(Popup::BaseRef { .. })) {
                        state.draft_workspace_popup = None;
                    } else {
                        state.load_draft_refs(&project_id, true);
                        state.draft_workspace_popup = Some(Popup::BaseRef {
                            query: Box::default(),
                            active: None,
                        });
                        tx.focus_when_shown("draft-ref-search", false);
                    }
                }),
        );
    } else if let Some(branch) = selected {
        bar = bar.child(trigger(&branch.name, &BRANCH, colors));
    }
    if !available {
        return bar;
    }
    match &state.draft_workspace_popup {
        Some(Popup::Mode) => {
            bar.child(mode_menu(project_id, draft.workspace_mode, compact, colors))
        }
        Some(Popup::BaseRef { query, active }) if branch_available => bar.child(branch_menu(
            project_id,
            draft,
            query,
            active.as_deref(),
            remote,
            width,
            colors,
        )),
        _ => bar,
    }
}

fn branch_trailing_inset(lane: f32) -> f32 {
    if lane >= 480.0 { 110.17 } else { INSET }
}

fn trigger(label: &str, symbol: &'static Icon, colors: Colors) -> El {
    row()
        .h(24.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(8.0)
        .child(graphic(symbol, 12.0, colors.muted))
        .child(text(label).font(LABEL).color(colors.muted).truncate())
}

fn mode_menu(project: &str, selected: WorkspaceMode, compact: bool, colors: Colors) -> El {
    let mut menu = col()
        .key("draft-workspace-menu")
        .w(159.05)
        .padding(4.0)
        .radius(8.0)
        .fill(colors.mode_panel)
        .border(1.0, colors.border)
        .border_layout()
        .role("listbox")
        .label("Workspace")
        .focus_group()
        .shadow(3.0, 10.0, Color::BLACK.alpha(0.22))
        .paint_order(1)
        .absolute(INSET, if compact { -93.0 } else { 36.25 })
        .on_key(|state: &mut State, event, tx| dismiss(state, event, tx, "draft-workspace-mode"))
        .child(
            row()
                .w_full()
                .h(28.0)
                .px(8.0)
                .center_y()
                .child(text("Workspace").font(SMALL).color(colors.muted)),
        );
    for mode in [WorkspaceMode::Local, WorkspaceMode::Worktree] {
        let (label, symbol, _) = mode_details(mode);
        let id = project.to_owned();
        let active = selected == mode;
        menu = menu.child(
            row()
                .key(mode_key(mode))
                .w_full()
                .h(29.0)
                .px(8.0)
                .gap(6.0)
                .center_y()
                .radius(4.0)
                .role("option")
                .label(label)
                .focusable(true)
                .focus_selected(active)
                .focus_ring(1.0, -1.0, colors.focus)
                .hover(colors.hover)
                .fill(if active {
                    colors.selected
                } else {
                    Color::TRANSPARENT
                })
                .on_press_tx(move |state: &mut State, tx| {
                    state.set_draft_workspace(&id, mode);
                    state.draft_workspace_popup = None;
                    tx.focus("draft-workspace-mode", tx.focus_visible());
                })
                .child(graphic(symbol, 12.0, colors.muted))
                .child(text(label).font(ITEM).color(colors.ink).truncate()),
        );
    }
    menu
}

fn mode_key(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Local => "draft-mode-local",
        WorkspaceMode::Worktree => "draft-mode-worktree",
    }
}

fn visible_branches<'a>(
    draft: &'a Draft,
    query: &str,
    include_remote: bool,
) -> impl Iterator<Item = &'a GitRef> {
    let query = query.trim().to_lowercase();
    let sanitized = query.split_ascii_whitespace().collect::<Vec<_>>().join("-");
    draft.refs.branches.iter().filter(move |branch| {
        if branch.is_remote && !include_remote {
            return false;
        }
        if query.is_empty() {
            return true;
        }
        let name = branch.name.to_lowercase();
        name.contains(&query) || (!sanitized.is_empty() && name.contains(&sanitized))
    })
}

fn highlighted_branch(
    draft: &Draft,
    branches: &[&GitRef],
    query: &str,
    active: Option<&str>,
) -> usize {
    let selected = if draft.workspace_mode == WorkspaceMode::Worktree {
        draft.base_ref.as_deref()
    } else {
        branches
            .iter()
            .find(|branch| branch.current && !branch.is_remote)
            .map(|branch| branch.name.as_str())
    }
    .filter(|_| query.trim().is_empty());
    let wanted = active.or(selected);
    branches
        .iter()
        .position(|branch| wanted == Some(branch.name.as_str()))
        .unwrap_or(0)
}

fn branch_menu(
    project: &str,
    draft: &Draft,
    query: &TextEdit,
    active: Option<&str>,
    include_remote: bool,
    lane: f32,
    colors: Colors,
) -> El {
    let event_project = project.to_owned();
    let submit_project = project.to_owned();
    let edit = text_input(query, TextStyle::new(14.0, 400.0).lh(26.0))
        .key("draft-ref-search")
        .w_full()
        .h(26.0)
        .label("Search refs")
        .placeholder("Search refs...", colors.muted)
        .color(colors.ink)
        .single_line()
        .on_edit(|state: &mut State| match &mut state.draft_workspace_popup {
            Some(Popup::BaseRef { query, .. }) => query,
            _ => unreachable!("the branch picker is open"),
        })
        .on_input_change(|state: &mut State, _| {
            if let Some(Popup::BaseRef { active, .. }) = &mut state.draft_workspace_popup {
                *active = None;
            }
        })
        .on_input_key(move |state: &mut State, event, tx| {
            branch_keys(state, &event_project, event, tx)
        })
        .on_submit(move |state: &mut State, tx| {
            choose_highlighted_branch(state, &submit_project, tx)
        });
    let mut results = col()
        .key("draft-ref-options")
        .role("listbox")
        .label(if draft.workspace_mode == WorkspaceMode::Worktree {
            "Base references"
        } else {
            "Checkout references"
        })
        .w_full()
        .pl(7.0)
        .pr(9.0)
        .py(12.0);
    let branches: Vec<_> = visible_branches(draft, &query.text, include_remote).collect();
    let index = highlighted_branch(draft, &branches, &query.text, active);
    if branches.is_empty() {
        let status = if draft.refs.loading {
            "Loading refs..."
        } else {
            draft.refs.error.as_deref().unwrap_or("No refs found.")
        };
        results = results.child(text(status).font(ITEM).color(colors.muted).wrap_anywhere());
    }
    for (row_index, branch) in branches.into_iter().enumerate() {
        let name = branch.name.clone();
        let project = project.to_owned();
        let badge = if branch.current {
            Some("current")
        } else if branch.is_remote {
            Some("remote")
        } else if branch.worktree_path.is_some() {
            Some("worktree")
        } else if branch.is_default {
            Some("default")
        } else {
            None
        };
        let selected = row_index == index;
        let mut option = row()
            .key(format!("draft-ref-{}", branch.name))
            .w_full()
            .h(28.0)
            .px(8.0)
            .gap(6.0)
            .center_y()
            .radius(4.0)
            .role("option")
            .label(&branch.name)
            .focus_selected(draft.base_ref.as_deref() == Some(branch.name.as_str()))
            .focus_ring(1.0, -1.0, colors.focus)
            .focusable(true)
            .tab_index(-1)
            .fill(if selected {
                colors.ink.alpha(0.09)
            } else {
                Color::TRANSPARENT
            })
            .hover(colors.hover)
            .on_press_tx(move |state: &mut State, tx| select_branch(state, &project, &name, tx))
            .child(text(&branch.name).font(ITEM).color(colors.ink).truncate())
            .child(el().grow(1.0));
        if let Some(badge) = badge {
            option = option.child(text(badge).font(BADGE).color(colors.muted.alpha(0.45)));
        }
        results = results.child(option);
    }
    let width = 318.0_f32.min((lane - 2.0 * INSET).max(0.0));
    let right = (branch_trailing_inset(lane) + 0.83).min((lane - width - INSET).max(INSET));
    let panel = col()
        .key("draft-ref-popup")
        .w(width)
        .radius(8.0)
        .fill(colors.panel)
        .border(1.0, colors.border)
        .border_layout()
        .role("dialog")
        .label("Base branches")
        .shadow(3.0, 10.0, Color::BLACK.alpha(0.22))
        .paint_order(1)
        .pin(None, None, Some(right), Some(28.75))
        .on_key(|state: &mut State, event, tx| dismiss(state, event, tx, "draft-base-branch"))
        .child(
            row().w_full().h(42.0).px(11.0).gap(8.0).center_y().child(
                row()
                    .w_full()
                    .h(34.0)
                    .gap(4.0)
                    .center_y()
                    .border_sides([0.0, 0.0, 1.0, 0.0], colors.border)
                    .child(edit.grow(1.0).min_w(0.0).pl(20.0))
                    .child(graphic(&SEARCH, 16.0, colors.muted.alpha(0.55)).absolute(0.0, 8.0)),
            ),
        )
        .child(
            col()
                .w_full()
                .max_h(224.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(results),
        );
    if include_remote && draft.workspace_mode == WorkspaceMode::Worktree {
        panel.child(origin_control(project, draft.start_from_origin, colors))
    } else {
        panel
    }
}

fn origin_control(project: &str, enabled: bool, colors: Colors) -> El {
    let active = oklch(
        if colors.dark { 0.571 } else { 0.488 },
        if colors.dark { 0.21 } else { 0.217 },
        264.0,
    );
    let neutral = if colors.dark {
        Color::WHITE.alpha(0.11)
    } else {
        hex("#d4d4d8").expect("static switch palette")
    };
    let thumb_color = match (colors.dark, enabled) {
        (true, true) => Color::BLACK,
        (true, false) => colors.muted,
        (false, _) => oklch(0.992, 0.0, 0.0),
    };
    let switch_project = project.to_owned();
    let label_project = project.to_owned();
    row()
        .key("draft-origin-footer")
        .w_full()
        .h(32.0)
        .px(11.0)
        .pt(1.0)
        .gap(6.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], colors.border.alpha(0.6))
        .on_background_press(move |state: &mut State| {
            state.set_draft_start_from_origin(&label_project, !enabled)
        })
        .child(graphic(&REFRESH, 12.0, colors.muted))
        .child(
            text("Start from remote")
                .font(SMALL)
                .color(colors.muted)
                .truncate(),
        )
        .child(el().grow(1.0))
        .child(
            el().key("draft-start-from-remote")
                .w(26.0)
                .h(16.0)
                .radius(8.0)
                .fill(if enabled { active } else { neutral })
                .role("switch")
                .label("Start worktree from remote")
                .checked(enabled)
                .focusable(true)
                .focus_ring(2.0, -1.0, colors.focus)
                .help(ORIGIN_HELP)
                .on_press(move |state: &mut State| {
                    state.set_draft_start_from_origin(&switch_project, !enabled)
                })
                .child(
                    el().absolute(if enabled { 12.0 } else { 2.0 }, 2.0)
                        .w(12.0)
                        .h(12.0)
                        .radius(6.0)
                        .fill(thumb_color)
                        .a11y_hidden(),
                ),
        )
}

fn dismiss(state: &mut State, event: &KeyEvent, tx: &mut Tx, key: &str) -> bool {
    if event.key != Key::Escape {
        return false;
    }
    state.draft_workspace_popup = None;
    tx.focus(key, tx.focus_visible());
    true
}

fn branch_keys(state: &mut State, project: &str, event: &KeyEvent, tx: &mut Tx) -> bool {
    if dismiss(state, event, tx, "draft-base-branch") {
        return true;
    }
    if !matches!(event.key, Key::Up | Key::Down)
        || event.mods.cmd
        || event.mods.alt
        || event.mods.shift
    {
        return false;
    }
    let Some(draft) = state.new_thread_drafts.get(project) else {
        return false;
    };
    let Some(Popup::BaseRef { query, active }) = &mut state.draft_workspace_popup else {
        return false;
    };
    let remote = state
        .workspace
        .as_ref()
        .is_some_and(|workspace| workspace.remote_worktree_launch);
    let branches: Vec<_> = visible_branches(draft, &query.text, remote).collect();
    let index = highlighted_branch(draft, &branches, &query.text, active.as_deref());
    let next = if event.key == Key::Up {
        index.saturating_sub(1)
    } else {
        index
            .saturating_add(1)
            .min(branches.len().saturating_sub(1))
    };
    if let Some(branch) = branches.get(next) {
        *active = Some(branch.name.clone());
        tx.scroll_key_center_at(format!("draft-ref-{}", branch.name), 112.0);
    }
    true
}

fn choose_highlighted_branch(state: &mut State, project: &str, tx: &mut Tx) {
    let Some(draft) = state.new_thread_drafts.get(project) else {
        return;
    };
    let Some(Popup::BaseRef { query, active }) = &state.draft_workspace_popup else {
        return;
    };
    let remote = state
        .workspace
        .as_ref()
        .is_some_and(|workspace| workspace.remote_worktree_launch);
    let branches: Vec<_> = visible_branches(draft, &query.text, remote).collect();
    let index = highlighted_branch(draft, &branches, &query.text, active.as_deref());
    let name = branches.get(index).map(|branch| branch.name.clone());
    if let Some(name) = name {
        select_branch(state, project, &name, tx);
    }
}

fn select_branch(state: &mut State, project: &str, name: &str, tx: &mut Tx) {
    match state
        .new_thread_drafts
        .get(project)
        .map(|draft| draft.workspace_mode)
    {
        Some(WorkspaceMode::Worktree) => state.set_draft_base_ref(project, name),
        Some(WorkspaceMode::Local) => state.switch_draft_checkout(project, name),
        None => {}
    }
    state.draft_workspace_popup = None;
    tx.focus("draft-base-branch", tx.focus_visible());
}
