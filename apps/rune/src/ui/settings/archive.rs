use std::collections::HashMap;

use icu_collator::{Collator, options::CollatorOptions};
use icu_locale_core::locale;
use rune::{Color, El, Icon, Key, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::connection::{ArchiveAction, Project, Thread};
use crate::ui::sidebar::project_badge;

use super::super::{Palette, State, error_text};
use super::{DESCRIPTION, Page, SECTION};

mod delete;
pub(super) use delete::confirmation;

const ROW_TITLE: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const PROJECT_SCOPE: &str = "archive-project-scope";
const ALL_PROJECTS: &str = "archive-all-projects";

pub(super) static ARCHIVE: rune::Icon = glyph!(
    "archive",
    r#"<rect x="2" y="3" width="20" height="5" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8M10 12h4"/>"#
);
static UNARCHIVE: rune::Icon = glyph!(
    "archive-x",
    r#"<rect x="2" y="3" width="20" height="5" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8m-10 4 4 4m0-4-4 4"/>"#
);
static EXPAND: rune::Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);
static CHECK: rune::Icon = glyph!("check", r#"<path d="m20 6-11 11-5-5"/>"#);

#[derive(Default)]
pub(in crate::ui) struct Panel {
    available: bool,
    writable: bool,
    deletable: bool,
    confirmation: Option<delete::Confirmation>,
    threads: Option<Result<Vec<Thread>, String>>,
    project_id: Option<String>,
    scope_open: bool,
    pending: HashMap<String, (ArchiveAction, Uuid)>,
    error: Option<String>,
}

impl Panel {
    pub(in crate::ui) fn available(&mut self, readable: bool, writable: bool, deletable: bool) {
        self.available = readable;
        self.writable = writable;
        self.deletable = deletable;
        if !readable {
            self.threads = None;
        }
    }

    pub(in crate::ui) fn update(&mut self, threads: Result<Vec<Thread>, String>) {
        self.threads = Some(threads);
    }

    pub(super) fn dismiss_scope(&mut self, tx: &mut Tx) -> bool {
        if !std::mem::take(&mut self.scope_open) {
            return false;
        }
        tx.focus(PROJECT_SCOPE, tx.focus_visible());
        true
    }

    pub(super) fn leave(&mut self) {
        self.scope_open = false;
        self.confirmation = None;
        self.error = None;
    }
}

impl State {
    fn select_archive_project(&mut self, project_id: Option<String>, tx: &mut Tx) {
        self.settings.archive.project_id = project_id;
        self.settings.archive.dismiss_scope(tx);
    }

    fn submit_archive_action(&mut self, action: ArchiveAction, thread_id: &str, tx: &mut Tx) {
        let archive = &mut self.settings.archive;
        let enabled = match action {
            ArchiveAction::Unarchive => archive.writable,
            ArchiveAction::Delete => archive.deletable,
        };
        let connected = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.reconnecting);
        let Some(connection) = self.connection.as_ref().filter(|_| connected && enabled) else {
            return;
        };
        if archive.pending.contains_key(thread_id) {
            return;
        }
        let operation = Uuid::new_v4();
        archive
            .pending
            .insert(thread_id.to_owned(), (action, operation));
        archive.error = None;
        if tx
            .focused()
            .is_some_and(|key| key == action_key(action, thread_id))
        {
            tx.focus("close-client-settings", tx.focus_visible());
        }
        connection.archive_action(action, thread_id.to_owned(), operation);
    }
}

pub(in crate::ui) fn settle(
    state: &mut State,
    action: ArchiveAction,
    thread_id: &str,
    operation: Uuid,
    result: Result<(), String>,
    tx: &mut Tx,
) {
    let archive = &mut state.settings.archive;
    if archive.pending.get(thread_id) != Some(&(action, operation)) {
        return;
    }
    archive.pending.remove(thread_id);
    match result {
        Ok(()) => {
            if let Some(Ok(threads)) = archive.threads.as_mut() {
                threads.retain(|thread| thread.id != thread_id);
            }
            if state.settings.open && state.settings.page == Page::Archive {
                let action = action_key(action, thread_id);
                if tx.focused().is_none_or(|key| key == action) {
                    tx.focus("close-client-settings", tx.focus_visible());
                }
            }
        }
        Err(message) => {
            let label = match action {
                ArchiveAction::Unarchive => "Failed to unarchive thread.",
                ArchiveAction::Delete => "Failed to delete thread.",
            };
            archive.error = Some(format!("{label} {message}"));
        }
    }
}

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let archive = &state.settings.archive;
    let workspace = state.workspace.as_ref();
    let projects = workspace
        .and_then(|workspace| workspace.shell.as_ref())
        .map_or(&[][..], |shell| &shell.projects);
    let project = archive
        .project_id
        .as_ref()
        .and_then(|id| projects.iter().find(|p| &p.id == id));
    let threads = archive
        .threads
        .as_ref()
        .and_then(|threads| threads.as_ref().ok());
    let collation = Collator::try_new(locale!("en-US").into(), CollatorOptions::default())
        .expect("English collation data");
    let mut sections = col().w_full().gap(34.0);
    let mut has_sections = false;
    if let (Some(workspace), Some(threads)) = (workspace, threads) {
        let now = OffsetDateTime::now_utc();
        for candidate in projects
            .iter()
            .filter(|candidate| project.is_none_or(|selected| candidate.id == selected.id))
        {
            let mut group = threads
                .iter()
                .filter(|thread| thread.project_id == candidate.id)
                .collect::<Vec<_>>();
            if group.is_empty() {
                continue;
            }
            group.sort_by(|left, right| {
                collation
                    .compare(archive_timestamp(right), archive_timestamp(left))
                    .then_with(|| collation.compare(&right.id, &left.id))
            });
            let mut card = col()
                .key(format!("archived-project-{}", candidate.id))
                .w_full()
                .radius(12.0)
                .border(1.0, palette.line.alpha(0.58))
                .border_layout();
            for (index, thread) in group.into_iter().enumerate() {
                let enabled = !workspace.reconnecting && !archive.pending.contains_key(&thread.id);
                card = card.child(thread_row(
                    thread,
                    palette,
                    now,
                    archive.writable,
                    archive.deletable && thread.worktree_path.is_none(),
                    enabled,
                    index != 0,
                ));
            }
            sections = sections.child(
                col()
                    .gap(12.0)
                    .w_full()
                    .child(
                        row()
                            .items_center()
                            .gap(8.0)
                            .h(24.0)
                            .px(16.0)
                            .child(project_badge::view_at(
                                candidate,
                                &workspace.favicons,
                                palette.dark,
                                16.0,
                            ))
                            .child(
                                text(&candidate.name)
                                    .font(SECTION)
                                    .color(palette.ink)
                                    .role("heading")
                                    .truncate(),
                            ),
                    )
                    .child(card),
            );
            has_sections = true;
        }
    }
    if !has_sections {
        let (title, description) = match archive.threads.as_ref() {
            Some(Err(message)) => ("Could not load archived threads", message.as_str()),
            Some(Ok(_)) => ("No archived threads", "Archived threads will appear here."),
            None if archive.available => (
                "Loading archived threads",
                "Checking the connected environment.",
            ),
            None => (
                "Could not load archived threads",
                "The connected environment does not provide an Archive.",
            ),
        };
        sections = sections.child(
            col()
                .w_full()
                .gap(4.0)
                .px(16.0)
                .py(12.0)
                .radius(12.0)
                .border(1.0, palette.line)
                .child(text(title).font(ROW_TITLE).color(palette.ink))
                .child(
                    text(description)
                        .font(DESCRIPTION)
                        .color(palette.muted)
                        .wrap(),
                ),
        );
    }
    if let Some(error) = archive.error.as_deref() {
        sections = sections.child(error_text(error, palette));
    }
    let mut body = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    if archive.scope_open {
        sections = sections.child(
            el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .paint_order(1)
                .on_background_press_tx(|state: &mut State, tx| {
                    state.settings.archive.dismiss_scope(tx);
                }),
        );
    }
    let scope = project_scope(projects, project, palette, archive.scope_open, &collation);
    body = body.child(
        col()
            .w_full()
            .max_w(800.0)
            .padding(if width < 440.0 { 16.0 } else { 30.0 })
            .pt(24.0)
            .gap(34.0)
            .child(scope)
            .child(sections),
    );
    body
}

fn project_scope(
    projects: &[Project],
    selected: Option<&Project>,
    palette: Palette,
    open: bool,
    collation: &icu_collator::CollatorBorrowed<'_>,
) -> El {
    let mut control = col().child(press(
        row()
            .key(PROJECT_SCOPE)
            .role("button")
            .label("Project scope")
            .expanded(open)
            .focusable(true)
            .focus_ring(1.0, 2.0, palette.accent)
            .items_center()
            .h(28.0)
            .px(9.0)
            .gap(4.0)
            .radius(8.0)
            .fill(palette.field)
            .border(1.0, palette.line)
            .border_layout()
            .hover(palette.hover)
            .child(
                text(selected.map_or("All projects", |project| &project.name))
                    .font(ROW_TITLE)
                    .color(palette.ink)
                    .truncate(),
            )
            .child(icon(&EXPAND, 12.0).color(palette.muted).a11y_hidden()),
        |state: &mut State, tx| {
            let archive = &mut state.settings.archive;
            if !archive.dismiss_scope(tx) {
                archive.scope_open = true;
                let choice = archive
                    .project_id
                    .as_ref()
                    .map_or_else(|| ALL_PROJECTS.to_owned(), |id| project_key(id));
                tx.focus(choice, tx.focus_visible());
            }
        },
    ));
    if open {
        let mut menu = col()
            .key("archive-project-scope-menu")
            .role("menu")
            .label("Projects")
            .hit()
            .focus_group()
            .absolute(0.0, 34.0)
            .w(248.0)
            .max_h(300.0)
            .padding(4.0)
            .radius(8.0)
            .fill(palette.field)
            .border(1.0, palette.line)
            .border_layout()
            .shadow(3.0, 10.0, Color::BLACK.alpha(0.24))
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(project_choice(None, selected.is_none(), palette));
        let mut ordered = projects.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| collation.compare(&left.name, &right.name));
        for project in ordered {
            menu = menu.child(project_choice(
                Some(project),
                selected.is_some_and(|item| item.id == project.id),
                palette,
            ));
        }
        control = control.child(menu);
    }
    row()
        .paint_order(2)
        .w_full()
        .px(16.0)
        .gap(6.0)
        .items_center()
        .child(
            text("Applying settings for")
                .font(ROW_TITLE)
                .color(palette.muted),
        )
        .child(control)
}

fn project_choice(project: Option<&Project>, selected: bool, palette: Palette) -> El {
    let label = project.map_or_else(
        || "All projects".into(),
        |project| {
            if project.archived_at.is_some() {
                format!("{} Archived", project.name)
            } else {
                project.name.clone()
            }
        },
    );
    let project_id = project.map(|project| project.id.clone());
    press(
        row()
            .key(
                project_id
                    .as_deref()
                    .map_or_else(|| ALL_PROJECTS.into(), project_key),
            )
            .role("menuitemradio")
            .label(&label)
            .checked(selected)
            .focusable(true)
            .focus_selected(selected)
            .focus_ring(1.0, 1.0, palette.accent)
            .w_full()
            .h(28.0)
            .px(8.0)
            .gap(6.0)
            .items_center()
            .radius(5.0)
            .hover(palette.hover)
            .child(
                el().size(14.0, 14.0).child(
                    icon(&CHECK, 14.0)
                        .color(if selected {
                            palette.ink
                        } else {
                            Color::TRANSPARENT
                        })
                        .a11y_hidden(),
                ),
            )
            .child(text(label).font(ROW_TITLE).color(palette.ink).truncate()),
        move |state: &mut State, tx| state.select_archive_project(project_id.clone(), tx),
    )
}

fn thread_row(
    thread: &Thread,
    palette: Palette,
    now: OffsetDateTime,
    writable: bool,
    deletable: bool,
    enabled: bool,
    divider: bool,
) -> El {
    let description = format!(
        "Archived {} · Created {}",
        relative_time(archive_timestamp(thread), now),
        relative_time(thread.created_at.as_deref().unwrap_or_default(), now)
    );
    let mut entry = row()
        .w_full()
        .h(67.0)
        .px(16.0)
        .gap(16.0)
        .items_center()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(
                    text(&thread.title)
                        .font(ROW_TITLE)
                        .color(palette.ink)
                        .truncate(),
                )
                .child(
                    text(description)
                        .font(DESCRIPTION)
                        .color(palette.muted)
                        .truncate(),
                ),
        );
    if divider {
        entry = entry.border_sides([1.0, 0.0, 0.0, 0.0], palette.line.alpha(0.56));
    }
    let mut actions = row().gap(8.0).items_center();
    if writable {
        let id = thread.id.clone();
        actions = actions.child(press(
            el().key(unarchive_key(&thread.id))
                .role("button")
                .label(format!("Unarchive {}", thread.title))
                .help("Unarchive")
                .help_above()
                .size(28.0, 28.0)
                .min_w(28.0)
                .radius(8.0)
                .center()
                .fill(palette.field)
                .border(1.0, palette.line)
                .hover(palette.hover)
                .disabled(!enabled)
                .focusable(enabled)
                .focus_ring(1.0, 2.0, palette.accent)
                .child(icon(&UNARCHIVE, 14.0).color(palette.muted).a11y_hidden()),
            move |state: &mut State, tx| {
                state.submit_archive_action(ArchiveAction::Unarchive, &id, tx)
            },
        ));
    }
    if deletable {
        actions = actions.child(delete::opener(thread, enabled, palette));
    }
    entry.child(actions)
}

pub(super) fn press(control: El, action: impl Fn(&mut State, &mut Tx) + Clone + 'static) -> El {
    let activate = action.clone();
    control
        .on_press_tx(action)
        .on_key(move |state: &mut State, event, tx| {
            if event.key != Key::Enter || event.mods != rune::Mods::default() {
                return false;
            }
            activate(state, tx);
            true
        })
}

fn archive_timestamp(thread: &Thread) -> &str {
    thread
        .archived_at()
        .or(thread.created_at.as_deref())
        .unwrap_or_default()
}

fn relative_time(timestamp: &str, now: OffsetDateTime) -> String {
    let Ok(timestamp) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return String::new();
    };
    let minutes = (now - timestamp).whole_minutes();
    match minutes {
        ..=0 => "just now".into(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

fn project_key(project_id: &str) -> String {
    format!("archive-project-{project_id}")
}

fn unarchive_key(thread_id: &str) -> String {
    format!("unarchive-thread-{thread_id}")
}

fn action_key(action: ArchiveAction, thread_id: &str) -> String {
    match action {
        ArchiveAction::Unarchive => unarchive_key(thread_id),
        ArchiveAction::Delete => delete::key(thread_id),
    }
}
