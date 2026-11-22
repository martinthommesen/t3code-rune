use std::cmp::Reverse;
use std::collections::HashSet;
use std::time::Duration;

use rune::color::hex;
use rune::{
    Color, El, Icon, Key, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text, text_input,
};
use time::OffsetDateTime;
use unicode_normalization::UnicodeNormalization;

use crate::connection::ProviderCatalog;

use super::navigation_history::Destination;
use super::project_favicons::ProjectFavicons;
use super::sidebar_header::{self, Colors, Layout};
use super::{Project, ProjectView, Shell, State, Thread, Workspace};

pub(super) mod project_badge;
pub(super) mod project_scope;
pub(super) mod provider_badge;

pub(super) use project_scope::overlay as project_scope_overlay;

pub(super) fn most_recent_project(shell: &Shell) -> Option<&Project> {
    project_scope::ordered_projects(shell, false)
        .into_iter()
        .next()
}

const SMALL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const TITLE: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const INACTIVE_TITLE: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const INSET: f32 = 8.0;

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $path, "</g>"),
        }
    };
}

static PLUS: Icon = glyph!("sidebar-empty-plus", r#"<path d="M5 12h14M12 5v14"/>"#);

static SEARCH: Icon = glyph!(
    "search",
    r#"<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>"#
);
static CLEAR: Icon = glyph!("x", r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#);
static FILTER: Icon = glyph!(
    "list-filter",
    r#"<path d="M2 5h20"/><path d="M6 12h12"/><path d="M9 19h6"/>"#
);
static ADD_PROJECT: Icon = glyph!(
    "folder-plus",
    r#"<path d="M12 10v6"/><path d="M9 13h6"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#
);
static NEW_THREAD: Icon = glyph!(
    "square-pen",
    r#"<path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.375 2.625a1 1 0 0 1 3 3l-9.013 9.014a2 2 0 0 1-.853.505l-2.873.84a.5.5 0 0 1-.62-.62l.84-2.873a2 2 0 0 1 .506-.852z"/>"#
);
static SERVER: Icon = glyph!(
    "server",
    r#"<rect width="20" height="8" x="2" y="2" rx="2"/><rect width="20" height="8" x="2" y="14" rx="2"/><line x1="6" x2="6.01" y1="6" y2="6"/><line x1="6" x2="6.01" y1="18" y2="18"/>"#
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectPicker {
    Filter,
    NewThread,
    Draft,
}

#[derive(Default)]
pub(super) struct SidebarState {
    search: TextEdit,
    scope: project_scope::ProjectScope,
    picker: Option<ProjectPicker>,
    now_minute: i64,
    rename: Option<Rename>,
    rename_edit: TextEdit,
}

struct Rename {
    thread_id: String,
    original_title: String,
}

fn begin_rename(state: &mut State, thread: &str) {
    if state.sidebar.rename.is_some() {
        return;
    }
    let Some(row) = state
        .workspace
        .as_ref()
        .filter(|workspace| workspace.launch_writes && !workspace.reconnecting)
        .and_then(|workspace| workspace.thread(thread))
    else {
        return;
    };
    let mut edit = TextEdit::default();
    edit.replace_text(&row.title);
    edit.anchor = 0;
    edit.focus = edit.text.len();
    edit.focused = true;
    state.sidebar.rename = Some(Rename {
        thread_id: thread.to_owned(),
        original_title: row.title.clone(),
    });
    state.sidebar.rename_edit = edit;
}

fn settle_rename(state: &mut State, submit: bool, return_focus: Option<&mut Tx>) -> bool {
    let Some(rename) = state.sidebar.rename.take() else {
        return false;
    };
    if let Some(tx) = return_focus {
        tx.return_focus(format!("thread-{}", rename.thread_id), tx.focus_visible());
    }
    let mut edit = std::mem::take(&mut state.sidebar.rename_edit);
    let text = std::mem::take(&mut edit.text);
    let title =
        text.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}');
    if submit
        && !title.is_empty()
        && title != rename.original_title
        && let Some(connection) = &state.connection
        && state
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.launch_writes && !workspace.reconnecting)
    {
        connection.rename_thread(rename.thread_id, title.to_owned());
    }
    true
}

pub(super) fn poll_rename(state: &mut State) -> bool {
    if state.sidebar.rename.is_some() && !state.sidebar.rename_edit.focused {
        return settle_rename(state, true, None);
    }
    false
}

pub(super) fn cancel_rename(state: &mut State) {
    state.sidebar.rename = None;
    state.sidebar.rename_edit = TextEdit::default();
}

impl SidebarState {
    pub(super) fn poll(&mut self, tx: &mut Tx) -> bool {
        let now = OffsetDateTime::now_utc();
        let seconds = now.unix_timestamp();
        let minute = seconds.div_euclid(60);
        let changed = self.now_minute != minute;
        self.now_minute = minute;
        let remaining = Duration::from_secs((60 - seconds.rem_euclid(60)) as u64)
            - Duration::from_nanos(u64::from(now.nanosecond()));
        tx.wake_after(remaining);
        changed
    }

    pub(super) fn update_project_scope(&mut self, shell: &Shell) {
        let projects = &shell.projects;
        self.scope.selected.retain(|id| {
            projects
                .iter()
                .any(|project| &project.id == id && project.archived_at.is_none())
        });
        if projects.iter().all(|project| project.archived_at.is_some())
            && self.picker == Some(ProjectPicker::Filter)
        {
            self.picker = None;
        }
        if self.picker == Some(ProjectPicker::Filter) {
            self.scope.remember_order(shell);
        }
    }

    pub(super) fn enable_project_organization(&mut self) {
        self.scope.organize_projects = true;
    }

    pub(super) fn supports_project_organization(&self) -> bool {
        self.scope.organize_projects
    }

    pub(super) fn settle_project_pin(&mut self, id: &str) -> bool {
        self.scope.pending_pins.remove(id)
    }

    fn selected_project<'a>(&self, shell: &'a Shell) -> Option<&'a Project> {
        let [selected] = self.scope.selected.as_slice() else {
            return None;
        };
        shell
            .projects
            .iter()
            .find(|project| &project.id == selected)
    }

    fn threads<'a>(&self, shell: &'a Shell) -> Vec<(&'a Project, &'a Thread)> {
        let query = normalized_search(self.search.text.trim());
        let projects: std::collections::HashMap<_, _> = shell
            .projects
            .iter()
            .map(|project| (project.id.as_str(), project))
            .collect();
        let mut threads: Vec<_> = shell
            .threads
            .iter()
            .filter(|thread| self.scope.matches(&thread.project_id))
            .filter_map(|thread| {
                projects
                    .get(thread.project_id.as_str())
                    .map(|project| (*project, thread))
            })
            .filter(|(project, thread)| {
                query.is_empty()
                    || normalized_search(&project.name).contains(&query)
                    || normalized_search(&thread.title).contains(&query)
            })
            .collect();
        threads.sort_by_cached_key(|(_, thread)| {
            (thread.archived, Reverse(thread.activity_timestamp()))
        });
        threads
    }
}

fn relative_time(timestamp: OffsetDateTime, now_minute: i64) -> String {
    let elapsed = (now_minute * 60 - timestamp.unix_timestamp())
        .max(0)
        .div_euclid(60);
    match elapsed {
        0 => "now".into(),
        1..=59 => format!("{elapsed}m"),
        60..=1439 => format!("{}h", elapsed / 60),
        _ => format!("{}d", elapsed / 1440),
    }
}

pub(super) fn normalized_search(text: &str) -> String {
    text.nfkd()
        .filter(|glyph| !unicode_normalization::char::is_combining_mark(*glyph))
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn view(
    state: &State,
    workspace: &Workspace,
    colors: Colors,
    dark: bool,
    layout: Layout,
    height: f32,
) -> El {
    let mut sidebar = col()
        .key("sidebar-contents")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .clip_rounded(0.0);
    if let Some(grain) = sidebar_header::surface_grain() {
        let tile = sidebar_header::WIDTH;
        let tiles = ((height + layout.height()) / tile).ceil() as usize;
        sidebar = sidebar.children((0..tiles).map(|index| {
            el().absolute(0.0, index as f32 * tile - layout.height())
                .size(tile, tile)
                .pixels(grain)
                .a11y_hidden()
        }));
    }
    let Some(shell) = &workspace.shell else {
        return sidebar.child(
            text("Loading projects…")
                .font(SMALL)
                .color(colors.muted)
                .padding(INSET)
                .role("status"),
        );
    };
    let list = thread_list(state, workspace, shell, colors, dark);
    sidebar = sidebar
        .child(toolbar(state, workspace, shell, colors, dark))
        .child(list);
    if state.sidebar.picker == Some(ProjectPicker::NewThread) {
        sidebar = sidebar.child(project_picker(
            workspace,
            shell,
            ProjectPicker::NewThread,
            None,
            colors,
            dark,
            false,
        ));
    }
    sidebar
}

fn toolbar(state: &State, workspace: &Workspace, shell: &Shell, colors: Colors, dark: bool) -> El {
    let icon_color =
        hex(if dark { "#8b8b8b" } else { "#85868e" }).expect("static sidebar icon color");
    let mut search = row()
        .grow(1.0)
        .min_w(0.0)
        .h(32.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .hover(colors.hover)
        .focus_within_ring(1.0, -1.0, colors.ring.alpha(0.7))
        .child(icon(&SEARCH, 16.0).color(icon_color).a11y_hidden())
        .child(
            text_input(&state.sidebar.search, TITLE.lh(21.0))
                .key("thread-search")
                .label("Search threads")
                .placeholder("Search", colors.muted)
                .color(colors.foreground)
                .single_line()
                .grow(1.0)
                .min_w(0.0)
                .on_edit(|state: &mut State| &mut state.sidebar.search)
                .on_input_change(|_: &mut State, tx| tx.scroll_to_instant("threads", 0.0, 0.0))
                .on_submit(open_first_thread),
        );
    if !state.sidebar.search.text.is_empty() {
        search = search.child(
            control("clear-thread-search", "Clear thread search", colors)
                .size(18.0, 18.0)
                .on_press_tx(|state: &mut State, tx| {
                    state.sidebar.search.replace_text(String::new());
                    tx.scroll_to_instant("threads", 0.0, 0.0);
                    tx.focus("thread-search", false);
                })
                .child(icon(&CLEAR, 12.0).color(icon_color).a11y_hidden()),
        );
    }
    let controls = row()
        .key("sidebar-toolbar")
        .w_full()
        .h(48.0)
        .padding(INSET)
        .gap(4.0)
        .center_y()
        .child(search);
    let mut actions = row().center_y();
    if shell
        .projects
        .iter()
        .any(|project| project.archived_at.is_none())
    {
        actions = actions.child(project_scope::view_trigger(
            state, workspace, shell, colors, dark, icon_color,
        ));
    }
    if workspace.project_writes {
        actions = actions.child(
            control("add-project", "Add project", colors)
                .disabled(workspace.reconnecting)
                .on_press_tx(State::show_add_project)
                .child(icon(&ADD_PROJECT, 16.0).color(icon_color).a11y_hidden()),
        );
    }
    if workspace.launch_writes && !shell.projects.is_empty() {
        actions = actions.child(
            control("new-thread", "New thread", colors)
                .disabled(
                    workspace.reconnecting
                        || workspace.selection.as_ref().is_some_and(|selection| {
                            matches!(
                                selection.view,
                                ProjectView::NewThread | ProjectView::WaitingForThread(_)
                            )
                        }),
                )
                .on_press_tx(start_new_thread)
                .child(icon(&NEW_THREAD, 16.0).color(icon_color).a11y_hidden()),
        );
    }
    controls.child(actions)
}

fn control(key: &str, label: &str, colors: Colors) -> El {
    el().key(key)
        .label(label)
        .role("button")
        .size(28.0, 28.0)
        .center()
        .radius(6.0)
        .focusable(true)
        .focus_ring(2.0, -1.0, colors.ring)
        .hover(colors.hover)
        .help(label)
}

fn open_first_thread(state: &mut State, _: &mut Tx) {
    if state.sidebar.search.text.trim().is_empty() {
        return;
    }
    let thread = state
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.shell.as_ref())
        .and_then(|shell| {
            state
                .sidebar
                .threads(shell)
                .first()
                .map(|(_, thread)| thread.id.clone())
        });
    if let Some(thread) = thread {
        state.open_thread(&thread);
    }
}

fn start_new_thread(state: &mut State, tx: &mut Tx) {
    let Some(workspace) = &state.workspace else {
        return;
    };
    let Some(shell) = &workspace.shell else {
        return;
    };
    if let Some(project) = state
        .sidebar
        .selected_project(shell)
        .or_else(|| (shell.projects.len() == 1).then(|| &shell.projects[0]))
    {
        state.navigate(Destination::NewThread(project.id.clone()));
    } else if let Some(project) = shell.projects.first() {
        tx.focus_when_shown(format!("project-option-{}", project.id), tx.focus_visible());
        state.sidebar.picker = Some(ProjectPicker::NewThread);
    }
}

pub(super) fn toggle_draft_project_picker(state: &mut State, project_id: &str, tx: &mut Tx) {
    state.draft_workspace_popup = None;
    if draft_picker_open(state) {
        state.sidebar.picker = None;
    } else {
        state.sidebar.picker = Some(ProjectPicker::Draft);
        let focus = state.workspace.as_ref().and_then(|workspace| {
            let shell = workspace.shell.as_ref()?;
            let current = shell
                .projects
                .iter()
                .find(|project| project.id == project_id)?;
            if workspace.is_scratch(current) {
                let first = shell
                    .projects
                    .iter()
                    .find(|project| !workspace.is_scratch(project));
                Some(first.map_or_else(
                    || "draft-add-project".to_owned(),
                    |project| format!("project-option-{}", project.id),
                ))
            } else {
                Some(format!("project-option-{project_id}"))
            }
        });
        if let Some(focus) = focus {
            tx.focus_when_shown(focus, tx.focus_visible());
        }
    }
}

pub(super) fn draft_picker_open(state: &State) -> bool {
    state.sidebar.picker == Some(ProjectPicker::Draft)
}

pub(super) fn close_project_picker(state: &mut State) {
    state.sidebar.picker = None;
}

pub(super) fn close_draft_project_picker(sidebar: &mut SidebarState) {
    if sidebar.picker == Some(ProjectPicker::Draft) {
        sidebar.picker = None;
    }
}

pub(super) fn draft_project_picker(
    workspace: &Workspace,
    project: &Project,
    left: f32,
    width: f32,
    dark: bool,
) -> Option<El> {
    Some(
        project_picker(
            workspace,
            workspace.shell.as_ref()?,
            ProjectPicker::Draft,
            Some(project),
            Colors::new(dark),
            dark,
            workspace.project_writes,
        )
        .absolute(left, -40.0)
        .w(width)
        .max_h(260.0),
    )
}

pub(super) fn change_draft_project(state: &mut State, target: &str) {
    let Some(current) = state
        .workspace
        .as_ref()
        .filter(|workspace| {
            !workspace.reconnecting
                && workspace.can_visit(&Destination::NewThread(target.to_owned()))
        })
        .and_then(|workspace| workspace.selection.as_ref())
        .filter(|selection| matches!(selection.view, ProjectView::NewThread))
        .map(|selection| selection.id.clone())
    else {
        return;
    };
    if current == target {
        return;
    }
    if state
        .new_thread_drafts
        .get(target)
        .is_none_or(|draft| draft.edit.text.is_empty())
        && state
            .new_thread_drafts
            .get(&current)
            .is_some_and(|draft| !draft.pending && !draft.edit.text.is_empty())
        && let Some(draft) = state.new_thread_drafts.remove(&current)
    {
        state.new_thread_drafts.insert(
            target.to_owned(),
            super::Draft {
                edit: draft.edit,
                ..super::Draft::new_thread()
            },
        );
    }
    state.navigate(Destination::NewThread(target.to_owned()));
}

fn project_picker(
    workspace: &Workspace,
    shell: &Shell,
    picker: ProjectPicker,
    scope: Option<&Project>,
    colors: Colors,
    dark: bool,
    can_add: bool,
) -> El {
    let mut items = col().w_full().padding(4.0);
    for project in &shell.projects {
        if picker == ProjectPicker::Draft && workspace.is_scratch(project) {
            continue;
        }
        let id = project.id.clone();
        items = items.child(
            project_option(
                format!("project-option-{id}"),
                &project.name,
                scope.is_some_and(|current| current.id == id),
                Some(project_badge::view(project, &workspace.favicons, dark)),
                colors,
            )
            .on_press(move |state: &mut State| {
                state.sidebar.picker = None;
                if picker == ProjectPicker::Draft {
                    change_draft_project(state, &id);
                } else {
                    state.navigate(Destination::NewThread(id.clone()));
                }
            }),
        );
    }
    if can_add {
        items = items.child(
            project_option(
                "draft-add-project".into(),
                "Add project",
                false,
                Some(icon(&ADD_PROJECT, 16.0).color(colors.muted).a11y_hidden()),
                colors,
            )
            .on_press_tx(|state: &mut State, tx| {
                state.sidebar.picker = None;
                state.show_add_project(tx);
            }),
        );
    }
    col()
        .key("project-picker")
        .label("Projects")
        .role("menu")
        .absolute(INSET, 46.0)
        .w(sidebar_header::WIDTH - INSET * 2.0 - 1.0)
        .max_h(300.0)
        .radius(8.0)
        .fill(colors.background)
        .border(1.0, colors.border)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(items)
        .on_key(move |state: &mut State, event, tx| {
            if event.key == rune::Key::Escape {
                state.sidebar.picker = None;
                tx.focus(
                    match picker {
                        ProjectPicker::NewThread => "new-thread",
                        ProjectPicker::Draft => "draft-project-selector",
                        ProjectPicker::Filter => unreachable!("scope has its own popup"),
                    },
                    tx.focus_visible(),
                );
                true
            } else {
                false
            }
        })
}

fn project_option(key: String, title: &str, active: bool, badge: Option<El>, colors: Colors) -> El {
    let mut option = row()
        .key(key)
        .label(title)
        .role("button")
        .checked(active)
        .w_full()
        .h(30.0)
        .px(8.0)
        .gap(6.0)
        .radius(5.0)
        .center_y()
        .hover(colors.hover)
        .focusable(true)
        .focus_ring(2.0, -1.0, colors.ring);
    if let Some(badge) = badge {
        option = option.child(badge);
    }
    option.child(
        text(title)
            .font(SMALL)
            .color(colors.foreground)
            .truncate()
            .min_w(0.0)
            .grow(1.0),
    )
}

fn no_projects(workspace: &Workspace, colors: Colors) -> El {
    let mut section = col().w_full().pt(22.0).gap(8.0).items_center().child(
        text("No projects yet")
            .key("sidebar-empty-label")
            .font(SMALL)
            .color(colors.muted.alpha(0.60))
            .role("status"),
    );
    if workspace.project_writes {
        section = section.child(
            row()
                .key("sidebar-empty-add-project")
                .role("button")
                .label("Add project")
                .w(97.0625)
                .h(26.0)
                .center()
                .gap(4.0)
                .radius(6.0)
                .border(1.0, colors.border)
                .border_layout()
                .hover(colors.hover)
                .focusable(!workspace.reconnecting)
                .disabled(workspace.reconnecting)
                .focus_ring(1.0, 1.0, colors.ring)
                .on_press_tx(State::show_add_project)
                .on_key(|state: &mut State, event, tx| {
                    sidebar_header::activate_on_return(state, event, tx, State::show_add_project)
                })
                .child(icon(&PLUS, 12.0).color(colors.muted).a11y_hidden())
                .child(
                    text("Add project")
                        .font(TextStyle::new(11.0, 500.0).lh(16.0))
                        .color(colors.muted),
                ),
        );
    }
    section
}

fn thread_list(
    state: &State,
    workspace: &Workspace,
    shell: &Shell,
    colors: Colors,
    dark: bool,
) -> El {
    let mut projects = HashSet::new();
    let style = CardStyle {
        favicons: &workspace.favicons,
        environment: &workspace.label,
        providers: workspace.providers.as_ref(),
        now: state.sidebar.now_minute,
        colors,
        dark,
        can_rename: workspace.launch_writes && !workspace.reconnecting,
        rename: state
            .sidebar
            .rename
            .as_ref()
            .map(|rename| (rename, &state.sidebar.rename_edit)),
    };
    let selected = workspace
        .selection
        .as_ref()
        .and_then(|selection| match &selection.view {
            ProjectView::Conversation(conversation) => Some(conversation.id.as_str()),
            _ => None,
        });
    let mut rows = col()
        .w_full()
        .px(INSET)
        .pt(2.0)
        .pb(INSET)
        .gap(5.0)
        .children(
            state
                .sidebar
                .threads(shell)
                .into_iter()
                .map(|(project, thread)| {
                    thread_card(
                        project,
                        thread,
                        selected == Some(thread.id.as_str()),
                        projects.insert(project.id.as_str()),
                        style,
                    )
                }),
        );
    if state.sidebar.search.text.trim().is_empty()
        && projects.is_empty()
        && shell
            .projects
            .iter()
            .all(|project| project.archived_at.is_some())
    {
        rows = rows.child(no_projects(workspace, colors));
    } else if state.sidebar.search.text.trim().is_empty() {
        let empty = shell
            .projects
            .iter()
            .filter(|project| state.sidebar.scope.matches(&project.id))
            .filter(|project| !projects.contains(project.id.as_str()));
        let listed = empty
            .clone()
            .filter(|project| !workspace.is_scratch(project));
        if projects.is_empty()
            && listed.clone().next().is_none()
            && empty.clone().any(|project| workspace.is_scratch(project))
        {
            rows = rows.child(
                el().w_full().pt(22.0).center().child(
                    text("No threads yet")
                        .font(SMALL.lh(16.0))
                        .color(colors.muted.alpha(0.60)),
                ),
            );
        }
        rows = rows.children(listed.map(|project| {
            let id = project.id.clone();
            project_option(
                format!("project-{id}"),
                &project.name,
                workspace
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.id == id),
                Some(project_badge::view(project, &workspace.favicons, dark)),
                colors,
            )
            .on_press(move |state: &mut State| state.select_project(&id))
        }));
    } else if projects.is_empty() {
        rows = rows.child(
            text("No matching threads")
                .font(SMALL)
                .color(colors.muted)
                .px(10.0),
        );
    }
    col()
        .key("threads")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(12.0)
        .child(rows)
}

#[derive(Clone, Copy)]
struct CardStyle<'a> {
    favicons: &'a ProjectFavicons,
    environment: &'a str,
    providers: Option<&'a ProviderCatalog>,
    now: i64,
    colors: Colors,
    dark: bool,
    can_rename: bool,
    rename: Option<(&'a Rename, &'a TextEdit)>,
}

fn thread_card(
    project: &Project,
    thread: &Thread,
    selected: bool,
    first_project_row: bool,
    style: CardStyle<'_>,
) -> El {
    let CardStyle {
        favicons,
        environment,
        providers,
        now,
        colors,
        dark,
        can_rename,
        rename,
    } = style;
    let id = thread.id.clone();
    let project_id = project.id.clone();
    let selected_fill = if dark {
        colors.foreground.alpha(0.11)
    } else {
        Color::WHITE
    };
    let secondary =
        hex(if dark { "#8f8f8f" } else { "#71717b" }).expect("static secondary text color");
    let muted_title = !selected
        && matches!(
            thread.status.as_str(),
            "" | "ready" | "idle" | "preparing" | "starting" | "running" | "waiting"
        );
    let status = status(thread).map(str::to_owned).or_else(|| {
        thread
            .activity_timestamp()
            .map(|activity| relative_time(activity, now))
    });
    let mut project_label = row()
        .center_y()
        .gap(6.0)
        .grow(1.0)
        .min_w(0.0)
        .child(project_badge::view(project, favicons, dark))
        .child(
            text(project.name.as_str())
                .font(SMALL)
                .color(secondary)
                .truncate()
                .min_w(0.0)
                .grow(1.0),
        );
    if first_project_row {
        project_label = project_label
            .key(format!("project-{project_id}"))
            .role("button")
            .label(&project.name)
            .focusable(true)
            .focus_ring(2.0, -1.0, colors.ring)
            .on_press(move |state: &mut State| state.select_project(&project_id));
    }
    let mut top = row().h(20.0).center_y().gap(6.0).child(project_label);
    if let Some(status) = status {
        top = top.child(
            row()
                .min_w(32.0)
                .justify_end()
                .child(text(status).font(SMALL).color(secondary)),
        );
    }
    let mut bottom = row()
        .h(16.0)
        .center_y()
        .gap(6.0)
        .child(icon(&SERVER, 14.0).color(colors.muted).a11y_hidden())
        .child(
            text(environment)
                .font(SMALL)
                .color(secondary)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if let Some(stack) = provider_badge::stack(thread, providers, colors, secondary, dark) {
        bottom = bottom.child(stack);
    }
    let normal_title = || {
        text(thread.title.as_str())
            .font(if muted_title { INACTIVE_TITLE } else { TITLE })
            .color(if muted_title {
                colors.foreground.alpha(0.75)
            } else {
                colors.foreground
            })
            .mt(4.0)
            .truncate()
    };
    let title = if let Some((_, edit)) = rename.filter(|(rename, _)| rename.thread_id == thread.id)
    {
        text_input(edit, TITLE)
            .key("rename-thread-title")
            .label("Thread title")
            .single_line()
            .w_full()
            .h(20.0)
            .mt(4.0)
            .px(4.0)
            .radius(3.0)
            .fill(colors.hover)
            .color(colors.foreground)
            .border(1.0, colors.border)
            .focus_ring(1.0, -1.0, colors.foreground)
            .on_edit(|state: &mut State| &mut state.sidebar.rename_edit)
            .on_submit(|state: &mut State, tx| {
                settle_rename(state, true, Some(tx));
            })
            .on_input_key(|state: &mut State, event, tx| {
                event.key == Key::Escape && settle_rename(state, false, Some(tx))
            })
    } else {
        normal_title()
    };
    let mut card = col()
        .key(format!("thread-{id}"))
        .role("button")
        .label(&thread.title)
        .checked(selected)
        .w_full()
        .h(78.0)
        .px(10.0)
        .py(8.0)
        .radius(6.0)
        .fill(if selected {
            selected_fill
        } else {
            Color::TRANSPARENT
        })
        .border(
            if selected && !dark { 1.0 } else { 0.0 },
            colors.border.alpha(0.7),
        )
        .hover(if selected {
            selected_fill
        } else {
            colors.foreground.alpha(0.08)
        })
        .focusable(true)
        .focus_ring(2.0, -1.0, colors.ring)
        .on_press(move |state: &mut State| state.open_thread(&id))
        .child(top)
        .child(title)
        .child(bottom.mt(2.0));
    if can_rename {
        let clicked = thread.id.clone();
        let keyed = clicked.clone();
        card = card
            .on_double_press(move |state: &mut State| begin_rename(state, &clicked))
            .on_key(move |state: &mut State, event, _| {
                if event.key != Key::Function(2) {
                    return false;
                }
                begin_rename(state, &keyed);
                true
            });
    }
    card
}

fn status(thread: &Thread) -> Option<&'static str> {
    if thread.archived {
        return Some("Archived");
    }
    match thread.status.as_str() {
        "preparing" | "starting" => Some("Starting"),
        "running" => Some("Running"),
        "queued" => Some("Queued"),
        "waiting" => Some("Waiting"),
        "failed" => Some("Failed"),
        _ => None,
    }
}
