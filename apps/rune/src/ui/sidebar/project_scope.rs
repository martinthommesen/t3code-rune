use std::cmp::Reverse;
use std::collections::HashMap;

use rune::color::hex;
use rune::{Align, Color, El, Key, KeyEvent, ScrollSpec, Shadow, Side, TextEdit, TextStyle, Tx};
use rune::{col, el, icon, row, text, text_input};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{Colors, FILTER, Project, ProjectPicker, SEARCH, Shell, State, Workspace};
use super::{control, normalized_search, project_badge};
use crate::ui::Palette;

const TRIGGER: &str = "project-filter";
const SEARCH_FIELD: &str = "project-scope-search";
const LIST: &str = "project-scope-options";
const ALL: &str = "all-projects";
const OPTION_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static FOLDER: rune::Icon = rune::Icon {
    name: "project-scope-folder",
    view: 24.0,
    body: r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static CHECK: rune::Icon = rune::Icon {
    name: "project-scope-selected",
    view: 24.0,
    body: r#"<path d="m20 6-11 11-5-5" fill="none" stroke="currentColor" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(super) struct ProjectScope {
    pub(super) selected: Vec<String>,
    query: TextEdit,
    highlighted: Option<String>,
}

impl ProjectScope {
    pub(super) fn matches(&self, id: &str) -> bool {
        self.selected.is_empty() || self.selected.iter().any(|selected| selected == id)
    }

    fn reset_search(&mut self) {
        self.query = TextEdit::default();
        self.highlighted = None;
    }
}

#[derive(Clone, Copy)]
enum Choice<'a> {
    All,
    Project(&'a Project),
}

impl Choice<'_> {
    fn key(self) -> String {
        match self {
            Self::All => ALL.into(),
            Self::Project(project) => format!("project-option-{}", project.id),
        }
    }

    fn selected(self, selection: &ProjectScope) -> bool {
        match self {
            Self::All => selection.selected.is_empty(),
            Self::Project(project) => selection.selected.contains(&project.id),
        }
    }
}

fn ordered_projects(shell: &Shell) -> Vec<&Project> {
    let mut activity = HashMap::<&str, Option<OffsetDateTime>>::new();
    for thread in shell.threads.iter().filter(|thread| !thread.archived) {
        let latest = thread.latest_user_activity();
        activity
            .entry(thread.project_id.as_str())
            .and_modify(|time| *time = (*time).max(latest))
            .or_insert(latest);
    }
    let mut projects: Vec<_> = shell
        .projects
        .iter()
        .filter(|project| project.archived_at.is_none())
        .collect();
    projects.sort_by_cached_key(|project| {
        let timestamp = activity
            .get(project.id.as_str())
            .copied()
            .unwrap_or_else(|| {
                [&project.updated_at, &project.created_at]
                    .into_iter()
                    .filter_map(Option::as_deref)
                    .find_map(|time| OffsetDateTime::parse(time, &Rfc3339).ok())
            });
        (
            project.pinned_at.is_none(),
            Reverse(timestamp),
            normalized_search(&project.name),
            &project.id,
        )
    });
    projects
}

fn choices<'a>(scope: &ProjectScope, shell: &'a Shell) -> Vec<Choice<'a>> {
    let query = normalized_search(scope.query.text.trim());
    query
        .is_empty()
        .then_some(Choice::All)
        .into_iter()
        .chain(
            ordered_projects(shell)
                .into_iter()
                .filter(|project| {
                    query.is_empty() || normalized_search(&project.name).contains(&query)
                })
                .map(Choice::Project),
        )
        .collect()
}

fn highlighted(scope: &ProjectScope, choices: &[Choice<'_>]) -> Option<usize> {
    scope
        .highlighted
        .as_ref()
        .and_then(|key| choices.iter().position(|choice| choice.key() == *key))
        .or_else(|| {
            (scope.query.text.is_empty() && scope.selected.len() <= 1)
                .then(|| choices.iter().position(|choice| choice.selected(scope)))
                .flatten()
        })
        .or_else(|| (!choices.is_empty()).then_some(0))
}

pub(super) fn view_trigger(
    state: &State,
    workspace: &Workspace,
    shell: &Shell,
    colors: Colors,
    dark: bool,
    tone: Color,
) -> El {
    let open = state.sidebar.picker == Some(ProjectPicker::Filter);
    let selected = &state.sidebar.scope.selected;
    let scoped: Vec<_> = if selected.is_empty() {
        Vec::new()
    } else {
        ordered_projects(shell)
            .into_iter()
            .filter(|project| selected.contains(&project.id))
            .collect()
    };
    let label = if scoped.is_empty() {
        "Filter threads by project".into()
    } else {
        let names: Vec<_> = scoped.iter().map(|project| project.name.as_str()).collect();
        format!("Filter threads by project: {}", names.join(", "))
    };
    let symbol = match scoped.as_slice() {
        [] => icon(&FILTER, 16.0).color(tone).a11y_hidden(),
        [project] => project_badge::view(project, &workspace.favicons, dark),
        [first, second, ..] => row()
            .size(24.0, 16.0)
            .center_y()
            .child(project_badge::view_at(
                first,
                &workspace.favicons,
                dark,
                14.0,
            ))
            .child(
                el().absolute(10.0, 1.0)
                    .size(14.0, 14.0)
                    .radius(4.0)
                    .fill(colors.background)
                    .outline(2.0, 0.0, colors.background)
                    .child(project_badge::view_at(
                        second,
                        &workspace.favicons,
                        dark,
                        14.0,
                    )),
            ),
    };
    let mut trigger = control(TRIGGER, &label, colors)
        .role("combobox")
        .expanded(open);
    if open {
        trigger = trigger.fill(colors.hover);
    }
    trigger
        .on_press_tx(|state: &mut State, tx| {
            if state.sidebar.picker == Some(ProjectPicker::Filter) {
                close(state, tx, false);
            } else {
                state.sidebar.scope.reset_search();
                state.sidebar.picker = Some(ProjectPicker::Filter);
                tx.focus_when_shown(SEARCH_FIELD, false);
            }
        })
        .child(symbol)
}

pub(in crate::ui) fn overlay(state: &State, palette: Palette, height: f32) -> Option<[El; 2]> {
    if !state.sidebar_open || state.sidebar.picker != Some(ProjectPicker::Filter) {
        return None;
    }
    let workspace = state.workspace.as_ref()?;
    let scope = &state.sidebar.scope;
    let items = choices(scope, workspace.shell.as_ref()?);
    let active_index = highlighted(scope, &items);
    let [fill, ink, muted, accent, border] = if palette.dark {
        ["#111111", "#f5f5f5", "#a3a3a3", "#346bf1", "#ffffff"]
    } else {
        ["#ffffff", "#18181b", "#71717b", "#1b4ed8", "#e4e4e7"]
    }
    .map(|color| hex(color).expect("static project picker color"));
    let mut editor = text_input(&scope.query, TextStyle::new(14.0, 400.0).lh(26.0))
        .key(SEARCH_FIELD)
        .role("combobox")
        .label("Search projects")
        .expanded(true)
        .placeholder("Search projects...", muted)
        .color(ink)
        .single_line()
        .h(26.0)
        .grow(1.0)
        .min_w(0.0)
        .on_edit(|state: &mut State| &mut state.sidebar.scope.query)
        .on_input_change(|state: &mut State, tx| {
            state.sidebar.scope.highlighted = None;
            tx.scroll_to_instant(LIST, 0.0, 0.0);
        })
        .on_input_key(key);
    if let Some(index) = active_index {
        editor = editor.active_descendant(items[index].key());
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
                .child(icon(&SEARCH, 16.0).color(muted).a11y_hidden())
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
        .padding(4.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    if items.is_empty() {
        list = list.child(
            text("No matching projects.")
                .font(OPTION_FONT)
                .color(muted)
                .padding(8.0),
        );
    }
    for (index, choice) in items.into_iter().enumerate() {
        let key = choice.key();
        let hover = key.clone();
        let (id, name, badge) = match choice {
            Choice::All => (
                None,
                "All projects",
                icon(&FOLDER, 16.0).color(muted).a11y_hidden(),
            ),
            Choice::Project(project) => (
                Some(project.id.clone()),
                project.name.as_str(),
                project_badge::view(project, &workspace.favicons, palette.dark),
            ),
        };
        let selected = choice.selected(scope);
        let mut glyph = el().size(16.0, 16.0).child(badge).a11y_hidden();
        if selected {
            glyph = glyph.child(
                el().absolute(8.0, 8.0)
                    .size(12.0, 12.0)
                    .radius(6.0)
                    .fill(accent)
                    .outline(2.0, 0.0, fill)
                    .center()
                    .child(icon(&CHECK, 8.0).color(Color::WHITE).a11y_hidden()),
            );
        }
        let highlighted = active_index == Some(index);
        list = list.child(
            row()
                .key(key)
                .role("option")
                .label(name)
                .focus_selected(selected)
                .w_full()
                .h(32.0)
                .px(8.0)
                .gap(8.0)
                .center_y()
                .radius(4.0)
                .fill(if highlighted {
                    ink.alpha(0.09)
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    state.sidebar.scope.highlighted = Some(hover.clone())
                })
                .on_press_tx(move |state: &mut State, tx| {
                    choose(state, id.as_deref(), tx.mods().shift, tx)
                })
                .child(glyph)
                .child(
                    text(name)
                        .font(OPTION_FONT)
                        .color(ink)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                ),
        );
    }
    let outside = el()
        .key("project-scope-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(|state: &mut State, tx| close(state, tx, false));
    let panel = col()
        .key("project-picker")
        .role("group")
        .label("Filter projects")
        .anchored_fit(TRIGGER, Side::Bottom, Align::Start, 4.0)
        .anchored_round()
        .w(288.0)
        .max_h((height - 92.0).clamp(120.0, 336.0))
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
            y: 16.0,
            blur: 44.0,
            spread: -18.0,
            color: Color::BLACK.alpha(0.6),
            inset: false,
        })
        .child(header)
        .child(list);
    Some([outside, panel])
}

fn key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if event.key == Key::Escape {
        close(state, tx, true);
        return true;
    }
    if !matches!(event.key, Key::Up | Key::Down | Key::Enter)
        || event.mods.ctrl
        || event.mods.alt
        || event.mods.cmd
        || event.mods.logo
    {
        return false;
    }
    let Some(shell) = state
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.shell.as_ref())
    else {
        return false;
    };
    let scope = &state.sidebar.scope;
    let items = choices(scope, shell);
    let Some(index) = highlighted(scope, &items) else {
        return true;
    };
    match event.key {
        Key::Enter => {
            let project = match items[index] {
                Choice::All if event.mods.shift => return true,
                Choice::All => None,
                Choice::Project(project) => Some(project.id.clone()),
            };
            choose(state, project.as_deref(), event.mods.shift, tx);
        }
        Key::Up | Key::Down => {
            let next = if event.key == Key::Up {
                index.saturating_sub(1)
            } else {
                (index + 1).min(items.len() - 1)
            };
            let key = items[next].key();
            state.sidebar.scope.highlighted = Some(key.clone());
            tx.scroll_into_view_in(LIST, key);
        }
        _ => unreachable!("key type checked above"),
    }
    true
}

fn choose(state: &mut State, project: Option<&str>, toggle: bool, tx: &mut Tx) {
    let selected = &mut state.sidebar.scope.selected;
    match project {
        Some(id) if toggle => {
            if let Some(index) = selected.iter().position(|selected| selected == id) {
                selected.remove(index);
            } else {
                selected.push(id.to_owned());
            }
        }
        project => *selected = project.map(str::to_owned).into_iter().collect(),
    }
    tx.scroll_to_instant("threads", 0.0, 0.0);
    if toggle && project.is_some() {
        tx.focus(SEARCH_FIELD, false);
    } else {
        close(state, tx, tx.focus_visible());
    }
}

fn close(state: &mut State, tx: &mut Tx, keyboard: bool) {
    state.sidebar.picker = None;
    state.sidebar.scope.reset_search();
    if keyboard {
        tx.return_focus(TRIGGER, true);
    }
}
