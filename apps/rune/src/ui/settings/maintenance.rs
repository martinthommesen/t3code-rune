use rune::{El, Icon, Key, ScrollSpec, TextStyle, Tx, col, icon, row, text};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::connection::storage_cleanup::{Entry, Kind, Outcome, Report, Update};
use crate::ui::action_error;

use super::{DESCRIPTION, Page, Palette, SECTION, State, archive};

mod format;

use format::{format_bytes, group_number};

const LABEL: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const PATH: TextStyle = DESCRIPTION.mono();
const LAST_RUN: &str = "storage-cleanup-last-run";

pub(super) static WRENCH: Icon = glyph!(
    "wrench",
    r#"<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.8-3.8a6 6 0 0 1-7.9 7.9l-7 7a2.1 2.1 0 0 1-3-3l7-7a6 6 0 0 1 7.9-7.9z"/>"#
);
static CHEVRON_DOWN: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);

#[derive(Default)]
pub(super) struct Panel {
    pub(super) writable: Option<bool>,
    rules: bool,
    view: Option<Uuid>,
    run_pending: bool,
    expanded: bool,
    last_run: LastRun,
}

#[derive(Default)]
enum LastRun {
    #[default]
    Loading,
    Failed,
    Report(Box<Report>),
}

impl LastRun {
    fn description(&self, now: OffsetDateTime) -> String {
        let report = match self {
            Self::Loading => return "Loading last run…".into(),
            Self::Failed => return "Could not load the last run.".into(),
            Self::Report(report) => report,
        };
        let mut description = format!(
            "Last run {}. {} recovered.",
            archive::relative_time(&report.finished_at, now),
            format_bytes(report.bytes_freed),
        );
        let failed = report.counts.failed;
        if failed > 0.0 {
            let noun = if failed == 1.0 { "item" } else { "items" };
            description.push_str(&format!(" {failed} {noun} couldn't be removed."));
        }
        description
    }
}

impl Panel {
    fn may_run(&self) -> bool {
        self.writable == Some(true) && self.rules && self.view.is_some() && !self.run_pending
    }

    fn report_received(&mut self, view: Uuid, result: Result<Option<Box<Report>>, String>) {
        if self.view == Some(view) {
            self.last_run = match result {
                Ok(Some(report)) => LastRun::Report(report),
                Ok(None) => LastRun::Loading,
                Err(_) => LastRun::Failed,
            };
            if !matches!(self.last_run, LastRun::Report(_)) {
                self.expanded = false;
            }
        }
    }
}

impl State {
    pub(in crate::ui) fn apply_storage_cleanup(&mut self, update: Update, tx: &mut Tx) {
        match update {
            Update::Ready { writable } => {
                self.settings.maintenance.writable = Some(writable);
                self.open_maintenance();
            }
            Update::Rules(rules) => self.settings.maintenance.rules = rules,
            Update::Report { view, result } => {
                self.settings.maintenance.report_received(view, result)
            }
            Update::Finished(result) => {
                self.settings.maintenance.run_pending = false;
                let Some(workspace) = &self.workspace else {
                    return;
                };
                let notice = match result {
                    Ok(report) => report
                        .entries
                        .into_iter()
                        .find(|entry| entry.outcome == Outcome::Failed)
                        .map(|entry| {
                            (
                                format!("Cleanup failed on {}", workspace.label),
                                entry.reason,
                            )
                        }),
                    Err(error) => Some((
                        format!("Could not run cleanup on {}", workspace.label),
                        error,
                    )),
                };
                if let Some((title, reason)) = notice {
                    action_error::show(self, action_error::Kind::StorageCleanup(title), reason, tx);
                }
            }
        }
    }

    pub(super) fn open_maintenance(&mut self) {
        let panel = &mut self.settings.maintenance;
        if self.settings.open
            && self.settings.page == Page::Maintenance
            && panel.writable.is_some()
            && panel.view.is_none()
            && let Some(connection) = &self.connection
        {
            let view = Uuid::new_v4();
            connection.open_storage_cleanup(view);
            panel.view = Some(view);
        }
    }

    pub(super) fn leave_maintenance(&mut self) {
        let panel = &mut self.settings.maintenance;
        if let Some(view) = panel.view.take()
            && let Some(connection) = &self.connection
        {
            connection.close_storage_cleanup(view);
        }
        panel.expanded = false;
        panel.run_pending = false;
        panel.last_run = LastRun::Loading;
    }

    pub(in crate::ui) fn cleanup_connection_lost(&mut self, final_loss: bool) {
        let panel = &mut self.settings.maintenance;
        panel.view = None;
        panel.rules = false;
        panel.run_pending = false;
        panel.last_run = LastRun::Loading;
        panel.expanded = false;
        if final_loss {
            panel.writable = None;
            if self.settings.page == Page::Maintenance {
                self.settings.page = Page::General;
            }
        }
    }

    fn run_cleanup(&mut self, _: &mut Tx) {
        if self.settings.maintenance.may_run()
            && let Some(connection) = &self.connection
        {
            self.settings.maintenance.run_pending = true;
            connection.run_storage_cleanup();
        }
    }
}

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let panel = &state.settings.maintenance;
    let now_minute = OffsetDateTime::from_unix_timestamp(
        OffsetDateTime::now_utc().unix_timestamp().div_euclid(60) * 60,
    )
    .expect("current minute is representable");
    let mut section = col()
        .w_full()
        .gap(4.0)
        .child(
            text("Cleanup")
                .font(SECTION)
                .role("heading")
                .color(palette.ink)
                .px(16.0)
                .pb(8.0),
        )
        .child(control(panel, now_minute, palette));
    if let LastRun::Report(report) = &panel.last_run {
        let machine = state
            .workspace
            .as_ref()
            .map_or("", |workspace| workspace.label.as_str());
        section = section.child(last_run(report, machine, panel.expanded, palette));
    }
    col()
        .key("maintenance-settings-content")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .max_w(800.0)
                .padding(if width < 440.0 { 16.0 } else { 30.0 })
                .child(section),
        )
}

fn card(palette: Palette) -> El {
    col()
        .w_full()
        .radius(12.0)
        .border(1.0, palette.line)
        .border_layout()
}

fn control(panel: &Panel, now: OffsetDateTime, palette: Palette) -> El {
    let enabled = panel.may_run();
    let label = if panel.run_pending {
        "Deleting…"
    } else {
        "Delete now"
    };
    let button = row()
        .key("storage-delete-now")
        .role("button")
        .label(label)
        .disabled(!enabled)
        .focusable(enabled)
        .focus_ring(1.0, 1.0, palette.accent)
        .opacity(if enabled { 1.0 } else { 0.5 })
        .h(28.0)
        .px(9.0)
        .items_center()
        .radius(8.0)
        .fill(palette.field)
        .border(1.0, palette.line)
        .child(text(label).font(LABEL).color(palette.ink));
    let button = if enabled {
        press(button.hover(palette.hover), State::run_cleanup)
    } else {
        button
    };
    let description = if panel.rules {
        panel.last_run.description(now)
    } else {
        "Turn on a cleanup rule to use this.".into()
    };
    card(palette).child(
        row()
            .w_full()
            .min_h(66.0)
            .gap(12.0)
            .px(16.0)
            .py(10.0)
            .items_center()
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(4.0)
                    .child(text("Delete now").font(LABEL).color(palette.ink))
                    .child(
                        text(description)
                            .font(DESCRIPTION)
                            .color(palette.muted)
                            .wrap(),
                    ),
            )
            .child(button),
    )
}

fn report_summary(report: &Report) -> String {
    let counts = &report.counts;
    let mut summary = format!("{} removed, {} kept", counts.removed, counts.kept);
    if counts.failed > 0.0 {
        summary.push_str(&format!(", {} failed", counts.failed));
    }
    summary
}

fn last_run(report: &Report, machine: &str, expanded: bool, palette: Palette) -> El {
    let header = press(
        row()
            .key(LAST_RUN)
            .role("button")
            .label("Last run")
            .expanded(expanded)
            .focusable(true)
            .focus_ring(1.0, 1.0, palette.accent)
            .w_full()
            .h(44.0)
            .px(16.0)
            .gap(8.0)
            .items_center()
            .hover(palette.hover)
            .child(
                icon(
                    if expanded {
                        &CHEVRON_DOWN
                    } else {
                        &super::CHEVRON
                    },
                    14.0,
                )
                .color(palette.muted)
                .a11y_hidden(),
            )
            .child(text("Last run").font(LABEL).color(palette.ink))
            .child(
                text(report_summary(report))
                    .font(DESCRIPTION)
                    .color(palette.muted)
                    .truncate(),
            ),
        |state: &mut State, _| {
            state.settings.maintenance.expanded = !state.settings.maintenance.expanded;
        },
    );
    let mut result = card(palette).child(header);
    if expanded {
        for entry in &report.entries {
            result = result.child(entry_row(entry, machine, palette));
        }
        let note = if report.omitted_count > 0.0 {
            Some(format!(
                "{machine}: {} more results.",
                group_number(report.omitted_count)
            ))
        } else if report.entries.is_empty() && report.omitted_count == 0.0 {
            Some(format!("{machine}: Nothing needed cleanup."))
        } else {
            None
        };
        if let Some(note) = note {
            result = result.child(
                text(note)
                    .font(DESCRIPTION)
                    .color(palette.muted)
                    .wrap()
                    .px(16.0)
                    .py(10.0)
                    .border_sides([1.0, 0.0, 0.0, 0.0], palette.line),
            );
        }
    }
    result
}

fn entry_name(entry: &Entry) -> &str {
    match entry.kind {
        Kind::Logs => "Rotated logs",
        Kind::BrowserArtifacts => "Browser artifacts",
        Kind::Worktree => entry
            .path
            .as_deref()
            .and_then(|path| {
                if entry.thread_id.is_some() {
                    path.rsplit(['\\', '/']).find(|part| !part.is_empty())
                } else {
                    Some(path)
                }
            })
            .unwrap_or("Worktrees"),
    }
}

fn entry_row(entry: &Entry, machine: &str, palette: Palette) -> El {
    let mut details = col()
        .w_full()
        .gap(3.0)
        .pl(12.0)
        .border_sides([0.0, 0.0, 0.0, 1.0], palette.line)
        .child(detail("Machine", machine, false, palette));
    for (label, value, mono) in [
        ("Project", entry.project_path.as_deref(), true),
        ("Folder", entry.path.as_deref(), true),
        ("Thread", entry.thread_title.as_deref(), false),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            details = details.child(detail(label, value, mono, palette));
        }
    }
    let mut item = row()
        .w_full()
        .gap(12.0)
        .px(16.0)
        .py(12.0)
        .items_center()
        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(
                    text(entry_name(entry))
                        .font(LABEL)
                        .color(palette.ink)
                        .wrap_anywhere()
                        .selectable(true),
                )
                .child(
                    text(&entry.reason)
                        .font(DESCRIPTION)
                        .wrap_anywhere()
                        .selectable(true)
                        .color(if entry.outcome == Outcome::Failed {
                            palette.error
                        } else {
                            palette.muted
                        }),
                )
                .child(details),
        );
    if let Some(size) = entry_size(entry) {
        item = item.child(text(size).font(DESCRIPTION).color(palette.muted));
    }
    item
}

fn detail(label: &str, value: &str, mono: bool, palette: Palette) -> El {
    row()
        .w_full()
        .gap(8.0)
        .child(text(label).w(54.0).font(DESCRIPTION).color(palette.muted))
        .child(
            text(value)
                .grow(1.0)
                .min_w(0.0)
                .wrap_anywhere()
                .selectable(true)
                .font(if mono { PATH } else { DESCRIPTION })
                .color(palette.ink.alpha(0.8)),
        )
}

fn entry_size(entry: &Entry) -> Option<String> {
    if entry.outcome == Outcome::Failed || (entry.outcome == Outcome::Kept && entry.bytes.is_none())
    {
        return None;
    }
    let files = (entry.kind != Kind::Worktree)
        .then_some(entry.files)
        .flatten()
        .map(|files| {
            let noun = if files == 1.0 { "file" } else { "files" };
            format!("{} {noun}", group_number(files))
        });
    let bytes = entry.bytes.map(format_bytes);
    match (files, bytes) {
        (Some(files), Some(bytes)) => Some(format!("{files} · {bytes}")),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub(super) fn press(control: El, action: impl Fn(&mut State, &mut Tx) + Clone + 'static) -> El {
    let keyboard = action.clone();
    control
        .on_press_tx(action)
        .on_key(move |state: &mut State, event, tx| {
            let activation =
                event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ");
            if activation && event.mods == rune::Mods::default() {
                keyboard(state, tx);
                true
            } else {
                false
            }
        })
}

#[cfg(test)]
mod tests;
