use std::collections::HashMap;

use rune::color::hex;
use rune::{
    Align, Color, El, Key, KeyEvent, Mods, ScrollSpec, Shadow, Side, TextEdit, TextStyle, Tx, col,
    el, icon, row, text, text_input,
};

use super::project_scope::{self, ProjectOrdering};
use super::{ADD_PROJECT, ProjectPicker, SEARCH as SEARCH_ICON, normalized_search, project_badge};
use crate::connection::Keybindings;
use crate::ui::{Palette, Project, ProjectView, State, Workspace};

pub(in crate::ui) const TRIGGER: &str = "draft-project-selector";
const SEARCH: &str = "draft-project-search";
const LIST: &str = "draft-project-options";
const ADD: &str = "draft-add-project";
const PIN: &str = "thread.pin";
const PROMPT: &str = "initial-prompt";
const OPTION: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

#[derive(Default)]
pub(super) struct Panel {
    query: TextEdit,
    highlighted: Option<String>,
    order: HashMap<String, usize>,
}

impl Panel {
    pub(super) fn remember_order(&mut self, projects: Vec<&Project>) {
        for project in projects {
            let next = self.order.len();
            self.order.entry(project.id.clone()).or_insert(next);
        }
    }
}

#[derive(Clone, Copy)]
enum Choice<'a> {
    Project(&'a Project),
    Add,
}

impl<'a> Choice<'a> {
    fn key(self) -> String {
        match self {
            Self::Project(project) => format!("project-option-{}", project.id),
            Self::Add => ADD.into(),
        }
    }

    fn label(self) -> &'a str {
        match self {
            Self::Project(project) => &project.name,
            Self::Add => "Add project",
        }
    }

    fn project_id(self) -> Option<&'a str> {
        match self {
            Self::Project(project) => Some(&project.id),
            Self::Add => None,
        }
    }
}

pub(in crate::ui) fn toggle(state: &mut State, tx: &mut Tx) {
    state.draft_workspace_popup = None;
    if super::draft_picker_open(state) {
        close(state, tx);
    } else {
        let mut query = TextEdit::default();
        query.focused = true;
        state.sidebar.picker = Some(ProjectPicker::Draft);
        let mut panel = Panel {
            query,
            ..Panel::default()
        };
        if let Some((workspace, current)) = context(state)
            && let Some(shell) = &workspace.shell
        {
            panel.remember_order(ProjectOrdering::current(state).picker(shell, Some(current)));
        }
        state.sidebar.draft_project_picker = panel;
        tx.focus_when_shown(SEARCH, tx.focus_visible());
    }
}

pub(in crate::ui) fn poll(state: &mut State) -> bool {
    if super::draft_picker_open(state) && !state.sidebar.draft_project_picker.query.focused {
        state.sidebar.picker = None;
        return true;
    }
    false
}

fn context(state: &State) -> Option<(&Workspace, &str)> {
    if !super::draft_picker_open(state) {
        return None;
    }
    let workspace = state.workspace.as_ref()?;
    let selection = workspace.selection.as_ref()?;
    matches!(selection.view, ProjectView::NewThread).then_some((workspace, &selection.id))
}

fn choices<'a>(state: &'a State, workspace: &'a Workspace, selected: &str) -> Vec<Choice<'a>> {
    let needle = normalized_search(state.sidebar.draft_project_picker.query.text.trim());
    let mut projects = workspace.shell.as_ref().map_or_else(Vec::new, |shell| {
        ProjectOrdering::current(state).picker(shell, Some(selected))
    });
    projects.sort_by_key(|project| {
        state
            .sidebar
            .draft_project_picker
            .order
            .get(&project.id)
            .copied()
            .unwrap_or(usize::MAX)
    });
    projects
        .into_iter()
        .filter(|project| !workspace.is_scratch(project))
        .filter(|project| needle.is_empty() || normalized_search(&project.name).contains(&needle))
        .map(Choice::Project)
        .chain((needle.is_empty() && workspace.project_writes).then_some(Choice::Add))
        .collect()
}

fn highlighted(panel: &Panel, options: &[Choice<'_>], selected: &str) -> Option<usize> {
    if let Some(index) = panel
        .highlighted
        .as_ref()
        .and_then(|key| options.iter().position(|choice| choice.key() == *key))
    {
        return Some(index);
    }
    if panel.query.text.trim().is_empty() {
        options
            .iter()
            .position(|choice| choice.project_id() == Some(selected))
    } else {
        (!options.is_empty()).then_some(0)
    }
}

pub(in crate::ui) fn overlay(state: &State, palette: Palette) -> Option<El> {
    let (workspace, selected) = context(state)?;
    let options = choices(state, workspace, selected);
    let panel = &state.sidebar.draft_project_picker;
    let active = highlighted(panel, &options, selected);
    let pin_allowed = can_pin(state, workspace);
    let pin_shortcut = keybindings(state).shortcut_label(PIN, |flag| flag == "isDesktop");
    let [fill, ink, muted, accent, border] = if palette.dark {
        ["#111111", "#f5f5f5", "#a3a3a3", "#346bf1", "#ffffff"]
    } else {
        ["#ffffff", "#18181b", "#71717b", "#1b4ed8", "#e4e4e7"]
    }
    .map(|value| hex(value).expect("static picker color"));
    let mut editor = text_input(&panel.query, TextStyle::new(14.0, 400.0).lh(26.0))
        .key(SEARCH)
        .role("combobox")
        .label("Search projects")
        .expanded(true)
        .placeholder("Search projects...", muted)
        .color(ink)
        .single_line()
        .h(26.0)
        .grow(1.0)
        .min_w(0.0)
        .on_edit(|state: &mut State| &mut state.sidebar.draft_project_picker.query)
        .on_input_change(|state: &mut State, tx| {
            state.sidebar.draft_project_picker.highlighted = None;
            tx.scroll_to_instant(LIST, 0.0, 0.0);
        })
        .on_input_key(key);
    if let Some(index) = active {
        editor = editor.active_descendant(options[index].key());
    }
    let header = col()
        .w_full()
        .px(12.0)
        .pt(9.0)
        .pb(1.0)
        .gap(6.0)
        .child(
            row()
                .w_full()
                .h(26.0)
                .center_y()
                .gap(4.0)
                .child(
                    icon(&SEARCH_ICON, 16.0)
                        .color(muted.alpha(0.55))
                        .a11y_hidden(),
                )
                .child(editor),
        )
        .child(el().w_full().h(1.0).fill(accent).a11y_hidden());
    let mut list = col()
        .key(LIST)
        .role("listbox")
        .label("Projects")
        .w_full()
        .min_h(0.0)
        .shrink()
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    if options.is_empty() {
        list = list.child(
            el().w_full().padding(8.0).center().child(
                text("No matching projects.")
                    .font(OPTION)
                    .color(muted)
                    .role("status"),
            ),
        );
    } else {
        list = list
            .padding(4.0)
            .children(options.into_iter().enumerate().map(|(index, choice)| {
                let key = choice.key();
                let hover = key.clone();
                let target = choice.project_id().map(str::to_owned);
                let selected = choice.project_id() == Some(selected);
                let (symbol, height) = match choice {
                    Choice::Project(project) => (
                        project_badge::view(project, &workspace.favicons, palette.dark),
                        32.0,
                    ),
                    Choice::Add => (icon(&ADD_PROJECT, 16.0).color(muted), 28.0),
                };
                let opacity = if Some(index) == active {
                    0.09
                } else if selected {
                    0.06
                } else {
                    0.0
                };
                let mut option = row()
                    .key(key)
                    .role("option")
                    .label(choice.label())
                    .focus_selected(selected)
                    .w_full()
                    .h(height)
                    .px(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(4.0)
                    .fill(ink.alpha(opacity))
                    .preserve_focus_on_press(true)
                    .on_hover(move |state: &mut State| {
                        state.sidebar.draft_project_picker.highlighted = Some(hover.clone())
                    })
                    .on_press_tx(move |state: &mut State, tx| choose(state, target.as_deref(), tx))
                    .child(symbol.a11y_hidden())
                    .child(
                        text(choice.label())
                            .font(OPTION)
                            .color(ink)
                            .grow(1.0)
                            .min_w(0.0)
                            .truncate(),
                    );
                if let Choice::Project(project) = choice
                    && pin_allowed
                {
                    let show_pin = Some(index) == active || project.pinned_at.is_some();
                    option = option.child(
                        project_scope::pin_button(
                            &state.sidebar.scope,
                            project,
                            muted,
                            SEARCH,
                            pin_shortcut.as_deref(),
                        )
                        .opacity(if show_pin { 1.0 } else { 0.0 })
                        .focusable(false)
                        .preserve_focus_on_press(true)
                        .a11y_hidden(),
                    );
                }
                option
            }));
    }
    Some(
        col()
            .key("draft-project-picker")
            .role("group")
            .label("Projects")
            .hit()
            .preserve_focus_on_press(true)
            .anchored_fit(TRIGGER, Side::Bottom, Align::Center, 4.0)
            .anchored_round()
            .w(288.0)
            .max_h(368.0)
            .min_h(0.0)
            .radius(8.0)
            .fill(fill.alpha(if palette.dark { 0.836 } else { 0.97 }))
            .backdrop_blur(16.0)
            .border(
                1.0,
                if palette.dark {
                    border.alpha(0.08)
                } else {
                    border
                },
            )
            .border_layout()
            .box_shadow(Shadow {
                x: 0.0,
                y: 18.0,
                blur: 44.0,
                spread: -18.0,
                color: Color::BLACK.alpha(if palette.dark { 0.8 } else { 0.55 }),
                inset: false,
            })
            .child(header)
            .child(list),
    )
}

fn keybindings(state: &State) -> &Keybindings {
    match &state.keybindings {
        Some(bindings) => bindings,
        None => Keybindings::stock(),
    }
}

fn can_pin(state: &State, workspace: &Workspace) -> bool {
    state.sidebar.scope.organize_projects && workspace.project_writes && !workspace.reconnecting
}

fn key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some((workspace, selected)) = context(state) else {
        return false;
    };
    let pin = can_pin(state, workspace)
        && keybindings(state).command(event, tx, |flag| {
            matches!(flag, "isDesktop" | "editableFocus")
        }) == Some(PIN);
    let navigation = event.mods == Mods::default()
        && matches!(event.key, Key::Up | Key::Down | Key::Enter | Key::Escape);
    if !pin && !navigation {
        return false;
    }
    let options = choices(state, workspace, selected);
    let active = highlighted(&state.sidebar.draft_project_picker, &options, selected);
    if pin
        && let Some(id) = active
            .and_then(|index| options[index].project_id())
            .map(str::to_owned)
    {
        if !tx.key_repeat() {
            project_scope::toggle_pin(state, &id, SEARCH, tx);
        }
        return true;
    }
    if !navigation {
        return false;
    }
    if event.key == Key::Escape {
        close(state, tx);
        return true;
    }
    if event.key == Key::Enter {
        if let Some(index) = active {
            let target = options[index].project_id().map(str::to_owned);
            choose(state, target.as_deref(), tx);
        } else {
            close(state, tx);
        }
        return true;
    }
    let count = options.len();
    if count != 0 {
        let index = match (event.key == Key::Down, active) {
            (true, None) => 0,
            (false, None) => count - 1,
            (true, Some(index)) => (index + 1) % count,
            (false, Some(index)) => (index + count - 1) % count,
        };
        let target = options[index].key();
        state.sidebar.draft_project_picker.highlighted = Some(target.clone());
        tx.scroll_into_view_in(LIST, target);
    }
    true
}

fn close(state: &mut State, tx: &mut Tx) {
    state.sidebar.picker = None;
    tx.return_focus(TRIGGER, tx.focus_visible());
}

fn choose(state: &mut State, target: Option<&str>, tx: &mut Tx) {
    let Some((_, current)) = context(state) else {
        return;
    };
    if target == Some(current) {
        close(state, tx);
        return;
    }
    state.sidebar.picker = None;
    if let Some(project) = target {
        super::change_draft_project(state, project);
        tx.focus_when_shown(PROMPT, false);
    } else {
        state.show_add_project(tx);
    }
}
