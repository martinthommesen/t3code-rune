use rune::{
    Align, Color, El, Key, KeyEvent, ScrollSpec, Shadow, Side, TextEdit, Tx, col, el, icon, row,
    text, text_input,
};
use uuid::Uuid;

use crate::connection::{Project, ProjectFileWorkspaceSettings};
use crate::ui::sidebar::project_badge;

use super::{CHEVRON, CONTROL, ITEM, Palette, State, surface};

const TRIGGER: &str = "threads-project-scope";
const LIST: &str = "threads-project-scope-options";
const SEARCH: &str = "threads-project-scope-search";

#[derive(Default)]
pub(super) struct Scope {
    pub(super) id: Option<String>,
    pub(super) file: ProjectFileWorkspaceSettings,
    file_request: Option<Uuid>,
    highlight: Option<usize>,
    query: TextEdit,
}

impl Scope {
    pub(super) fn apply_file(&mut self, operation: Uuid, settings: ProjectFileWorkspaceSettings) {
        if self.file_request == Some(operation) {
            self.file_request = None;
            self.file = settings;
        }
    }

    pub(super) fn close(&mut self) {
        self.highlight = None;
        self.query = TextEdit::default();
    }

    pub(super) fn reload(state: &mut State) {
        if !matches!(
            state.settings.page,
            super::super::Page::Threads | super::super::Page::Worktrees
        ) {
            return;
        }
        let scope = &mut state.settings.workspace.scope;
        scope.file_request = scope
            .id
            .clone()
            .and_then(|id| state.connection.as_ref()?.read_workspace_project_file(id));
    }
}

pub(super) fn projects(state: &State) -> &[Project] {
    state
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.shell.as_ref())
        .map_or(&[], |shell| shell.projects.as_slice())
}

pub(super) fn project_is_available(state: &State, id: &str) -> bool {
    projects(state).iter().any(|project| project.id == id)
}

fn choices(state: &State) -> Vec<Option<&Project>> {
    let needle = state
        .settings
        .workspace
        .scope
        .query
        .text
        .trim()
        .to_lowercase();
    let mut projects: Vec<_> = projects(state)
        .iter()
        .filter(|project| {
            needle.is_empty()
                || project.name.to_lowercase().contains(&needle)
                || project.path.to_lowercase().contains(&needle)
        })
        .collect();
    projects
        .sort_by_cached_key(|project| (project.name.to_lowercase(), &project.path, &project.id));
    let all = needle.is_empty() || "all projects".contains(&needle);
    all.then_some(None)
        .into_iter()
        .chain(projects.into_iter().map(Some))
        .collect()
}

fn choice_key(id: Option<&str>) -> String {
    id.map_or_else(
        || "threads-scope-all-projects".to_owned(),
        |id| format!("threads-scope-project-{id}"),
    )
}

pub(super) fn view(state: &State, palette: Palette) -> El {
    let scope = &state.settings.workspace.scope;
    let project = scope
        .id
        .as_ref()
        .and_then(|id| projects(state).iter().find(|project| &project.id == id));
    let label = project
        .map(|project| project.name.as_str())
        .unwrap_or(if scope.id.is_some() {
            "Unavailable project"
        } else {
            "All projects"
        });
    let width = if project.is_some() {
        ((label.chars().count() as f32 * 7.5) + 62.0).clamp(120.0, 288.0)
    } else if scope.id.is_some() {
        175.0
    } else {
        111.0
    };
    let mut trigger = row()
        .key(TRIGGER)
        .role("button")
        .label(format!("Project scope: {label}"))
        .expanded(scope.highlight.is_some())
        .size(width, 28.0)
        .min_w(width)
        .pl(9.0)
        .pr(5.0)
        .gap(6.0)
        .items_center()
        .radius(8.0)
        .fill(surface(palette, "#ffffff", "#171717"))
        .border(1.0, surface(palette, "#d4d4d8", "#313131"))
        .focusable(true)
        .focus_ring(2.0, 1.0, palette.accent)
        .on_press_tx(open);
    if let (Some(project), Some(workspace)) = (project, &state.workspace) {
        trigger = trigger.child(project_badge::view_at(
            project,
            &workspace.favicons,
            palette.dark,
            14.0,
        ));
    }
    let trigger = trigger
        .child(
            text(label)
                .font(CONTROL)
                .color(palette.ink)
                .grow(1.0)
                .truncate(),
        )
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden());
    row()
        .key("threads-workspace-scope")
        .px(16.0)
        .gap(6.0)
        .items_center()
        .child(
            text("Applying settings for")
                .font(ITEM)
                .color(palette.muted),
        )
        .child(trigger)
        .child(
            text("across All environments")
                .font(ITEM)
                .color(palette.muted)
                .wrap()
                .min_w(0.0)
                .shrink(),
        )
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let scope = &state.settings.workspace.scope;
    let requested = scope.highlight?;
    let choices = choices(state);
    let focused = requested.min(choices.len().saturating_sub(1));
    let mut list = col()
        .key(LIST)
        .role("menu")
        .label("Project scope")
        .w_full()
        .max_h(288.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .focusable(true)
        .on_key(navigate);
    if let Some(project) = choices.get(focused) {
        list = list.active_descendant(choice_key(project.map(|p| p.id.as_str())));
    }
    for (index, project) in choices.iter().enumerate() {
        let id = project.map(|project| project.id.clone());
        let selected = scope.id == id;
        let duplicate = project.is_some_and(|current| {
            projects(state)
                .iter()
                .filter(|other| other.name == current.name)
                .count()
                > 1
        });
        let label = project.map_or("All projects", |project| project.name.as_str());
        let mut item = row()
            .key(choice_key(id.as_deref()))
            .role("menuitemradio")
            .label(label)
            .checked(selected)
            .focus_selected(selected)
            .w_full()
            .h(28.0)
            .px(8.0)
            .items_center()
            .gap(8.0)
            .radius(4.0)
            .fill(if focused == index || selected {
                palette.ink.alpha(0.08)
            } else {
                Color::TRANSPARENT
            })
            .on_hover(move |state: &mut State| {
                state.settings.workspace.scope.highlight = Some(index)
            })
            .on_press_tx(move |state, tx| choose(state, id.clone(), tx));
        if let (Some(project), Some(workspace)) = (project, &state.workspace) {
            item = item.child(project_badge::view_at(
                project,
                &workspace.favicons,
                palette.dark,
                14.0,
            ));
        }
        item = item.child(
            text(label)
                .font(ITEM)
                .color(palette.ink)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
        if let Some(project) = project.filter(|project| duplicate || project.archived_at.is_some())
        {
            let note = if project.archived_at.is_some() {
                "Archived"
            } else {
                &project.path
            };
            item = item.child(
                text(note)
                    .font(super::DESCRIPTION)
                    .color(palette.muted)
                    .truncate()
                    .max_w(132.0),
            );
        }
        if selected {
            item = item.child(text("✓").font(ITEM).color(palette.muted).a11y_hidden());
        }
        list = list.child(item);
        if index == 0 && project.is_none() && choices.len() > 1 {
            list = list.child(el().w_full().h(1.0).fill(palette.line).my(4.0));
        }
    }
    if choices.is_empty() {
        list = list.child(
            text("No matches found.")
                .font(ITEM)
                .color(palette.muted)
                .padding(8.0),
        );
    }
    let mut menu = col()
        .key("threads-project-scope-popup")
        .anchored_fit(TRIGGER, Side::Bottom, Align::Start, 4.0)
        .anchored_round()
        .w(298.0)
        .padding(4.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(surface(palette, "#ffffff", "#111111").alpha(if palette.dark { 0.84 } else { 0.97 }))
        .backdrop_blur(16.0)
        .box_shadow(Shadow {
            x: 0.0,
            y: 16.0,
            blur: 40.0,
            spread: -18.0,
            color: Color::BLACK.alpha(if palette.dark { 0.80 } else { 0.55 }),
            inset: false,
        });
    if projects(state).len() > 6 {
        menu = menu.child(
            text_input(&scope.query, ITEM)
                .key(SEARCH)
                .role("searchbox")
                .label("Search projects")
                .placeholder("Search projects", palette.muted)
                .single_line()
                .h(32.0)
                .px(8.0)
                .color(palette.ink)
                .on_edit(|state: &mut State| &mut state.settings.workspace.scope.query)
                .on_input_change(|state: &mut State, _| {
                    state.settings.workspace.scope.highlight = Some(0)
                })
                .on_input_key(navigate),
        );
    }
    let outside = el()
        .key("threads-project-scope-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    Some([outside, menu.child(list)])
}

fn open(state: &mut State, tx: &mut Tx) {
    if state.settings.workspace.scope.highlight.is_some() {
        close(state, tx);
        return;
    }
    state.settings.workspace.close_popups();
    let scope = &state.settings.workspace.scope;
    let active = choices(state)
        .iter()
        .position(|project| project.map(|project| project.id.as_str()) == scope.id.as_deref());
    state.settings.workspace.scope.highlight = Some(active.unwrap_or(0));
    let target = if projects(state).len() > 6 {
        SEARCH
    } else {
        LIST
    };
    tx.focus_when_shown(target, tx.focus_visible());
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.workspace.scope.close();
    tx.return_focus(TRIGGER, tx.focus_visible());
}

pub(super) fn choose(state: &mut State, id: Option<String>, tx: &mut Tx) {
    close(state, tx);
    let settings = &mut state.settings.workspace;
    if settings.scope.id != id {
        settings.submodules.collapse();
    }
    settings.scope.id = id;
    settings.scope.file = ProjectFileWorkspaceSettings::default();
    settings.scope.file_request = None;
    settings.error = None;
    Scope::reload(state);
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let choices = choices(state);
    let active = state.settings.workspace.scope.highlight.unwrap_or(0);
    match event.key {
        Key::Escape => close(state, tx),
        Key::Tab => {
            close(state, tx);
            return false;
        }
        Key::Enter => {
            if let Some(project) = choices.get(active.min(choices.len().saturating_sub(1))) {
                choose(state, project.map(|project| project.id.clone()), tx);
            }
        }
        Key::Home => state.settings.workspace.scope.highlight = Some(0),
        Key::End => {
            state.settings.workspace.scope.highlight = Some(choices.len().saturating_sub(1))
        }
        Key::Down => {
            state.settings.workspace.scope.highlight =
                Some((active + 1).min(choices.len().saturating_sub(1)))
        }
        Key::Up => state.settings.workspace.scope.highlight = Some(active.saturating_sub(1)),
        _ => return false,
    }
    true
}
