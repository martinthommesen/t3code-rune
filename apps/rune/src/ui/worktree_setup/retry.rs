use std::collections::HashMap;

use rune::color::hex;
use rune::{Color, El, Icon, TextStyle, col, el, icon, row, text};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::connection::{
    ConversationEntry, RuntimeResponseOutcome, WorkspaceFailure, WorkspaceRetry,
};

use super::super::Transcript;
use super::{BODY, Colors, Palette, SMALL, State, Workspace};

const TITLE: TextStyle = TextStyle::new(14.0, 500.0).lh(22.75);
const BUTTON: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);

static ALERT: Icon = Icon {
    name: "workspace-preparation-alert",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 8v4m0 4h.01"/></g>"#,
};

static RETRY: Icon = Icon {
    name: "workspace-preparation-retry",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/></g>"#,
};

#[derive(Default)]
pub(in crate::ui) struct PendingRetries(HashMap<String, PendingRetry>);

struct PendingRetry {
    command_id: Uuid,
    failure_id: String,
    updated_at: Option<String>,
    waiting: bool,
}

impl PendingRetries {
    pub(in crate::ui) fn observe(&mut self, thread_id: &str, entries: &[ConversationEntry]) {
        self.0.retain(|run_id, pending| {
            entries.iter().any(|entry| {
                matches!(entry, ConversationEntry::WorkspaceFailure(failure)
                if failure.id == pending.failure_id && failure.updated_at == pending.updated_at
                    && failure.run_id.as_deref() == Some(run_id) && failure.can_retry(thread_id))
            })
        });
    }

    fn submit(&mut self, run_id: &str, failure: &WorkspaceFailure) -> Option<Uuid> {
        let pending = self
            .0
            .entry(run_id.to_owned())
            .or_insert_with(|| PendingRetry {
                command_id: Uuid::new_v4(),
                failure_id: failure.id.clone(),
                updated_at: failure.updated_at.clone(),
                waiting: false,
            });
        if pending.waiting {
            return None;
        }
        pending.waiting = true;
        Some(pending.command_id)
    }

    fn waiting(&self, run_id: &str) -> bool {
        self.0.get(run_id).is_some_and(|pending| pending.waiting)
    }
}

impl State {
    fn retry_workspace_preparation(&mut self, thread_id: &str, run_id: &str) {
        if self.connection.is_none()
            || !self
                .workspace
                .as_ref()
                .is_some_and(|workspace| allowed(workspace, thread_id))
        {
            return;
        }
        let Some(conversation) = self.conversation_mut(thread_id) else {
            return;
        };
        let Transcript::Loaded(entries) = &conversation.transcript else {
            return;
        };
        let Some(failure) = entries.iter().find_map(|entry| match entry {
            ConversationEntry::WorkspaceFailure(failure)
                if failure.run_id.as_deref() == Some(run_id) && failure.can_retry(thread_id) =>
            {
                Some(failure)
            }
            _ => None,
        }) else {
            return;
        };
        let Some(command_id) = conversation.preparation_retries.submit(run_id, failure) else {
            return;
        };
        if let Some(connection) = &self.connection {
            connection.retry_workspace_preparation(WorkspaceRetry {
                thread_id: thread_id.to_owned(),
                run_id: run_id.to_owned(),
                command_id,
            });
        }
    }

    pub(in crate::ui) fn settle_workspace_retry(
        &mut self,
        retry: WorkspaceRetry,
        outcome: RuntimeResponseOutcome,
    ) {
        let Some(conversation) = self.conversation_mut(&retry.thread_id) else {
            return;
        };
        let Some(pending) = conversation
            .preparation_retries
            .0
            .get_mut(&retry.run_id)
            .filter(|pending| pending.command_id == retry.command_id)
        else {
            return;
        };
        match outcome {
            RuntimeResponseOutcome::Accepted => {}
            RuntimeResponseOutcome::Rejected(message) => {
                conversation.preparation_retries.0.remove(&retry.run_id);
                self.project_notice = Some(message);
            }
            RuntimeResponseOutcome::Unconfirmed => {
                pending.waiting = false;
                self.project_notice =
                    Some("The environment did not confirm the workspace preparation retry.".into());
            }
        }
    }
}

fn allowed(workspace: &Workspace, thread_id: &str) -> bool {
    workspace.worktree_launch
        && !workspace.reconnecting
        && workspace
            .thread(thread_id)
            .is_some_and(|thread| !thread.archived)
}

pub(in crate::ui) fn failure_row(
    conversation: &super::Conversation,
    failure: &WorkspaceFailure,
    workspace: &Workspace,
    viewport_width: f32,
    palette: Palette,
) -> El {
    let colors = Colors::new(palette);
    let title = failure
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Provider error");
    let mut heading = row()
        .w_full()
        .min_h(24.0)
        .gap(6.0)
        .items_start()
        .child(
            el().size(24.0, 24.0)
                .center()
                .child(icon(&ALERT, 16.0).color(colors.destructive).a11y_hidden()),
        )
        .child(
            text(title)
                .font(TITLE)
                .color(colors.destructive)
                .mt(1.09375)
                .grow(1.0)
                .min_w(0.0)
                .wrap(),
        );
    if let Some((timestamp, tooltip)) = failure
        .started_at
        .as_deref()
        .or(failure.updated_at.as_deref())
        .and_then(timestamp)
    {
        heading = heading.child(
            text(timestamp)
                .font(SMALL.features("'tnum' 1"))
                .color(colors.muted)
                .mr(4.0)
                .help(tooltip),
        );
    }
    let mut body = col().w_full().px(2.0).py(2.0).child(heading);
    if let Some(message) = failure.message() {
        body = body.child(
            text(message)
                .font(BODY)
                .color(colors.ink.alpha(0.8))
                .ml(28.0)
                .py(4.0)
                .wrap_anywhere()
                .selectable(true),
        );
    }
    if let Some(run_id) = failure
        .run_id
        .as_deref()
        .filter(|_| failure.can_retry(&conversation.id) && allowed(workspace, &conversation.id))
    {
        let thread = conversation.id.clone();
        let run = run_id.to_owned();
        let waiting = conversation.preparation_retries.waiting(run_id);
        let surface = if colors.dark {
            Color::WHITE.alpha(0.035)
        } else {
            Color::WHITE
        };
        let outline = if colors.dark {
            Color::WHITE.alpha(0.11)
        } else {
            hex("#d4d4d8").expect("static input border")
        };
        let button = row()
            .key(format!("retry-workspace-preparation-{run_id}"))
            .role("button")
            .label("Retry")
            .focusable(true)
            .disabled(waiting)
            .focus_ring(2.0, -2.0, colors.accent)
            .size(60.484375, if viewport_width < 640.0 { 28.0 } else { 24.0 })
            .px(6.0)
            .gap(2.0)
            .center_y()
            .radius(8.0)
            .border(1.0, outline)
            .fill(surface)
            .hover(colors.hover)
            .on_press(move |state: &mut State| state.retry_workspace_preparation(&thread, &run))
            .child(icon(&RETRY, 14.0).color(colors.muted).a11y_hidden())
            .child(text("Retry").font(BUTTON).color(colors.ink));
        body = body.child(col().w_full().pl(28.0).pb(5.0).child(button));
    }
    col()
        .key(format!("workspace-preparation-failure-{}", failure.id))
        .w_full()
        .pb(8.0)
        .child(body)
}

fn timestamp(value: &str) -> Option<(String, String)> {
    let instant = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    let local = instant.to_offset(UtcOffset::local_offset_at(instant).ok()?);
    let today = OffsetDateTime::now_local().ok()?.date();
    let day = local.day();
    let month = u8::from(local.month());
    let year = local.year();
    let hour = local.hour();
    let wall_time = format!(
        "{}:{:02} {}",
        (hour + 11) % 12 + 1,
        local.minute(),
        if hour < 12 { "AM" } else { "PM" }
    );
    let label = match (today - local.date()).whole_days() {
        days if days <= 0 => wall_time.clone(),
        1 => format!("yesterday at {wall_time}"),
        _ if year == today.year() => format!("{month}/{day} {wall_time}"),
        _ => format!("{month}/{day}/{year} {wall_time}"),
    };
    let ordinal = match (day % 100, day % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    Some((
        label,
        format!("{wall_time}, {day}{ordinal} {} {year}", local.month()),
    ))
}
