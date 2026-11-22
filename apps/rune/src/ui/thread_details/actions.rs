use rune::color::hex;
use rune::{
    Align, Color, El, Icon, Key, KeyEvent, ScrollSpec, Shadow, Side, TextStyle, Tx, col, el, icon,
    row, text,
};

use crate::connection::{ProjectAction, ProjectActionIcon};

use super::{CARD_WIDTH, LABEL, Palette, Project, ProjectView, State, Thread, Workspace};

const GROUP: &str = "project-script-controls";
const PRIMARY: &str = "project-script-primary";
const TRIGGER: &str = "project-script-actions";
const MENU: &str = "project-script-menu";
const ITEM_PREFIX: &str = "project-script-run-";
const ITEM_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static CHEVRON: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);
static PLAY: Icon = glyph!("play", r#"<path d="m6 3 15 9-15 9V3z"/>"#);
static TEST: Icon = glyph!(
    "flask-conical",
    r#"<path d="M9 3h6m-5 0v6l-5 8a2 2 0 0 0 1.7 3h10.6a2 2 0 0 0 1.7-3l-5-8V3M8 14h8"/>"#
);
static CONFIGURE: Icon = glyph!(
    "wrench",
    r#"<path d="M14.7 6.3 18 3a5 5 0 0 0-6.3 6.3l-8.4 8.4a2.1 2.1 0 1 0 3 3l8.4-8.4A5 5 0 0 0 21 6l-3.3 3.3z"/>"#
);
static LINT: Icon = glyph!(
    "check-square",
    r#"<rect x="4" y="4" width="16" height="16" rx="2"/><path d="m8 12 3 3 5-6"/>"#
);
static BUILD: Icon = glyph!(
    "package",
    r#"<path d="M4 7l8-4 8 4v10l-8 4-8-4zM4 7l8 4 8-4m-8 4v10M8 5l8 4"/>"#
);
static DEBUG: Icon = glyph!(
    "bug",
    r#"<rect x="7" y="8" width="10" height="12" rx="5"/><path d="M12 8V5m-3 1L7 3m8 3 2-3M4 9l3 2m13-2-3 2M3 14h4m10 0h4M4 19l3-2m13 2-3-2"/>"#
);

pub(super) struct Menu {
    thread_id: String,
    active: usize,
}

pub(super) fn owns_focus(key: &str) -> bool {
    matches!(key, PRIMARY | TRIGGER | MENU) || key.starts_with(ITEM_PREFIX)
}

fn action_icon(action: &ProjectAction) -> &'static Icon {
    match action.icon {
        ProjectActionIcon::Play => &PLAY,
        ProjectActionIcon::Test => &TEST,
        ProjectActionIcon::Lint => &LINT,
        ProjectActionIcon::Configure => &CONFIGURE,
        ProjectActionIcon::Build => &BUILD,
        ProjectActionIcon::Debug => &DEBUG,
    }
}

fn primary<'a>(
    actions: &'a [ProjectAction],
    remembered: Option<&str>,
) -> Option<&'a ProjectAction> {
    actions
        .iter()
        .find(|action| remembered == Some(action.id.as_str()))
        .or_else(|| {
            actions
                .iter()
                .find(|action| !action.run_on_worktree_create && action.run_on_settle != Some(true))
        })
        .or_else(|| {
            actions
                .iter()
                .find(|action| action.run_on_settle != Some(true))
        })
}

fn current_project<'a>(state: &'a State, thread_id: &str) -> Option<&'a Project> {
    let workspace = state.workspace.as_ref()?;
    let selection = workspace.selection.as_ref()?;
    let ProjectView::Conversation(conversation) = &selection.view else {
        return None;
    };
    if conversation.id != thread_id {
        return None;
    }
    workspace
        .shell
        .as_ref()?
        .projects
        .iter()
        .find(|project| project.id == selection.id)
}

pub(super) fn control(
    state: &State,
    workspace: &Workspace,
    project: &Project,
    thread: &Thread,
    palette: Palette,
) -> Option<El> {
    let actions = state.settings.project_actions(project);
    if actions.is_empty() {
        return None;
    }
    let expanded = state
        .thread_details
        .actions
        .as_ref()
        .is_some_and(|menu| menu.thread_id == thread.id);
    let hovered = palette.ink.alpha(if palette.dark { 0.08 } else { 0.035 });
    let can_run = workspace.terminal && !workspace.reconnecting && !thread.archived;
    let mut group = row()
        .key(GROUP)
        .role("group")
        .label("Project scripts")
        .w_full()
        .h(32.0)
        .center_y()
        .radius(8.0)
        .fill(if expanded {
            hovered
        } else {
            Color::TRANSPARENT
        })
        .hover(hovered);
    let preferred = state.client_settings.last_project_action(&project.id);
    if let Some(action) = primary(actions, preferred) {
        let name = format!("Run {}", action.name);
        let thread_id = thread.id.clone();
        let action_id = action.id.clone();
        let key_thread = thread_id.clone();
        let key_action = action_id.clone();
        group = group
            .child(
                row()
                    .key(PRIMARY)
                    .role("button")
                    .label(name.clone())
                    .help(name)
                    .min_w(0.0)
                    .grow(1.0)
                    .h(32.0)
                    .px(10.0)
                    .gap(10.0)
                    .center_y()
                    .radius(8.0)
                    .hover(hovered)
                    .focus_ring(2.0, 1.0, palette.accent)
                    .focusable(can_run)
                    .disabled(!can_run)
                    .on_press_tx(move |state, tx| run(state, &thread_id, &action_id, tx))
                    .on_key(move |state, event, tx| {
                        on_activate(event, tx, |tx| run(state, &key_thread, &key_action, tx))
                    })
                    .child(
                        icon(action_icon(action), 16.0)
                            .color(palette.muted)
                            .a11y_hidden(),
                    )
                    .child(
                        text(&action.name)
                            .font(LABEL)
                            .color(palette.ink.alpha(0.8))
                            .truncate(),
                    ),
            )
            .child(el().w(1.0).h(16.0).fill(if expanded {
                palette.line.alpha(0.65)
            } else {
                Color::TRANSPARENT
            }));
    }
    let thread_id = thread.id.clone();
    let key_thread = thread_id.clone();
    Some(
        group.child(
            row()
                .key(TRIGGER)
                .role("button")
                .label("Script actions")
                .help("Script actions")
                .expanded(expanded)
                .size(32.0, 32.0)
                .center()
                .radius(8.0)
                .hover(hovered)
                .focus_ring(2.0, 1.0, palette.accent)
                .focusable(can_run)
                .disabled(!can_run)
                .on_press_tx(move |state, tx| toggle(state, &thread_id, false, tx))
                .on_key(move |state: &mut State, event, tx| match event.key {
                    Key::Down | Key::Up => {
                        toggle(state, &key_thread, event.key == Key::Up, tx);
                        true
                    }
                    _ => on_activate(event, tx, |tx| toggle(state, &key_thread, false, tx)),
                })
                .child(icon(&CHEVRON, 16.0).color(palette.muted).a11y_hidden()),
        ),
    )
}

pub(super) fn overlay(
    state: &State,
    project: &Project,
    thread: &Thread,
    palette: Palette,
) -> Option<[El; 2]> {
    let menu = state
        .thread_details
        .actions
        .as_ref()
        .filter(|menu| menu.thread_id == thread.id)?;
    let actions = state.settings.project_actions(project);
    let active = menu.active.min(actions.len().checked_sub(1)?);
    let outside = el()
        .key("project-script-menu-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(|state, tx| {
            dismiss(state, tx);
        });
    let list = col()
        .key(MENU)
        .role("menu")
        .label("Script actions")
        .focusable(true)
        .tab_index(-1)
        .active_descendant(format!("{ITEM_PREFIX}{}", actions[active].id))
        .anchored_fit(GROUP, Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .w(CARD_WIDTH - 18.0)
        .max_h(320.0)
        .padding(4.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(
            hex(if palette.dark { "#111111" } else { "#ffffff" })
                .expect("static project scripts color")
                .alpha(if palette.dark { 0.84 } else { 0.97 }),
        )
        .backdrop_blur(16.0)
        .box_shadow(Shadow {
            x: 0.0,
            y: if palette.dark { 18.0 } else { 16.0 },
            blur: if palette.dark { 44.0 } else { 40.0 },
            spread: -18.0,
            color: Color::BLACK.alpha(if palette.dark { 0.8 } else { 0.55 }),
            inset: false,
        })
        .on_key(navigate)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .children(actions.iter().enumerate().map(|(index, action)| {
            let thread_id = thread.id.clone();
            let action_id = action.id.clone();
            let name = match (
                action.run_on_worktree_create,
                action.run_on_settle == Some(true),
            ) {
                (true, true) => format!("{} (setup, on settle)", action.name),
                (true, false) => format!("{} (setup)", action.name),
                (false, true) => format!("{} (on settle)", action.name),
                (false, false) => action.name.clone(),
            };
            row()
                .key(format!("{ITEM_PREFIX}{action_id}"))
                .role("menuitem")
                .label(name.clone())
                .w_full()
                .h(32.0)
                .px(8.0)
                .gap(8.0)
                .center_y()
                .radius(4.0)
                .fill(if active == index {
                    palette.ink.alpha(0.08)
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    if let Some(menu) = state.thread_details.actions.as_mut() {
                        menu.active = index;
                    }
                })
                .on_press_tx(move |state, tx| run(state, &thread_id, &action_id, tx))
                .child(
                    icon(action_icon(action), 16.0)
                        .color(palette.muted)
                        .a11y_hidden(),
                )
                .child(text(name).font(ITEM_FONT).color(palette.ink).truncate())
        }));
    Some([outside, list])
}

fn on_activate(event: &KeyEvent, tx: &mut Tx, action: impl FnOnce(&mut Tx)) -> bool {
    if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ") {
        if !tx.key_repeat() {
            action(tx);
        }
        true
    } else {
        false
    }
}

fn toggle(state: &mut State, thread_id: &str, last: bool, tx: &mut Tx) {
    if dismiss(state, tx) {
        return;
    }
    if let Some(project) = current_project(state, thread_id)
        && let Some(last_index) = state.settings.project_actions(project).len().checked_sub(1)
    {
        state.thread_details.actions = Some(Menu {
            thread_id: thread_id.to_owned(),
            active: if last { last_index } else { 0 },
        });
        tx.focus_when_shown(MENU, tx.focus_visible());
    }
}

pub(super) fn dismiss(state: &mut State, tx: &mut Tx) -> bool {
    if state.thread_details.actions.take().is_some() {
        tx.return_focus(TRIGGER, tx.focus_visible());
        true
    } else {
        false
    }
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(menu) = state.thread_details.actions.as_ref() else {
        return false;
    };
    let Some(project) = current_project(state, &menu.thread_id) else {
        return dismiss(state, tx);
    };
    let actions = state.settings.project_actions(project);
    let Some(last) = actions.len().checked_sub(1) else {
        return dismiss(state, tx);
    };
    let active = menu.active.min(last);
    let next = match event.key {
        Key::Up if active > 0 => active - 1,
        Key::Up | Key::End => last,
        Key::Down if active < last => active + 1,
        Key::Down | Key::Home => 0,
        Key::Escape => return dismiss(state, tx),
        Key::Tab => {
            dismiss(state, tx);
            return false;
        }
        _ => {
            let thread_id = menu.thread_id.clone();
            let action_id = actions[active].id.clone();
            return on_activate(event, tx, |tx| run(state, &thread_id, &action_id, tx));
        }
    };
    let option_key = format!("{ITEM_PREFIX}{}", actions[next].id);
    if let Some(menu) = state.thread_details.actions.as_mut() {
        menu.active = next;
    }
    tx.scroll_into_view_in(MENU, option_key);
    true
}

fn run(state: &mut State, thread_id: &str, action_id: &str, tx: &mut Tx) {
    let Some(project) = current_project(state, thread_id) else {
        return;
    };
    let Some(action) = state
        .settings
        .project_actions(project)
        .iter()
        .find(|action| action.id == action_id)
    else {
        return;
    };
    let project_id = project.id.clone();
    let input = format!(" {}\r", action.command);
    if state.open_project_action_terminal(thread_id, input, tx) {
        state.thread_details.close();
        if let Err(error) = state
            .client_settings
            .remember_project_action(&project_id, action_id)
        {
            state.project_notice = Some(format!(
                "Could not remember the last project script. {error}"
            ));
        }
    }
}
