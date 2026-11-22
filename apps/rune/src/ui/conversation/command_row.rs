use std::collections::{HashMap, HashSet};

use rune::color::oklch;
use rune::{El, Family, Icon, ScrollSpec, TextStyle, col, el, icon, row, text};
use rune_kit::syntax::{SyntaxTheme, highlight};
use uuid::Uuid;

use crate::connection::{
    CommandEntry, CommandInspection, CommandOutput, CommandOutputTarget, CommandStatus,
};
use crate::server::TerminalOutput;

use super::{
    Colors, Conversation, ConversationEntry, State, command_ansi, shell_command, work_fold,
};

const CODE: TextStyle = TextStyle::new(13.0, 400.0)
    .lh(21.125)
    .family(Family::SystemMono);
static TERMINAL: Icon = glyph!("terminal", r#"<path d="m4 17 6-6-6-6m8 14h8"/>"#);
static EXPAND: Icon = glyph!("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static COLLAPSE: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);

#[derive(Default)]
pub(in crate::ui) struct Panels {
    entries: HashMap<CommandOutputTarget, Panel>,
    expanded_groups: HashSet<String>,
}

struct Panel {
    inspection: CommandInspection,
    run_id: Option<String>,
    group_id: Option<String>,
    output: CommandOutput,
}

fn fallback_revision(command: &CommandEntry) -> Option<String> {
    command.output_omitted.then(|| {
        if command.status.is_active() {
            "live".into()
        } else {
            command.updated_at.clone().unwrap_or_default()
        }
    })
}

pub(in crate::ui) fn update(
    state: &mut State,
    thread_id: &str,
    attachment: Uuid,
    output: CommandOutput,
) {
    if let Some(conversation) = state.conversation_mut(thread_id)
        && let Some(panel) = conversation
            .command_panels
            .entries
            .values_mut()
            .find(|panel| panel.inspection.attachment == attachment)
    {
        panel.output = output;
    }
}

pub(in crate::ui) fn entries_changed(
    state: &mut State,
    thread_id: &str,
    entries: &[ConversationEntry],
) {
    let Some(conversation) = state.conversation_mut(thread_id) else {
        return;
    };
    if conversation.command_panels.entries.is_empty() {
        return;
    }
    let open = &conversation.expanded_work_runs;
    let groups = &conversation.command_panels.expanded_groups;
    let visible: HashMap<_, _> = entries
        .iter()
        .filter_map(|entry| match entry {
            ConversationEntry::Command(command)
                if (command.completed_work.is_none()
                    || command
                        .run_id
                        .as_ref()
                        .is_some_and(|run| open.contains(run)))
                    && command
                        .group
                        .as_ref()
                        .is_none_or(|group| groups.contains(&group.id)) =>
            {
                Some((command.target(), command))
            }
            _ => None,
        })
        .collect();
    let mut close = Vec::new();
    let mut refresh = Vec::new();
    conversation.command_panels.entries.retain(|target, panel| {
        let Some(command) = visible.get(target) else {
            close.push(panel.inspection.attachment);
            return false;
        };
        let revision = fallback_revision(command);
        panel.group_id = command.group.as_ref().map(|group| group.id.clone());
        if panel.inspection.fallback_revision != revision {
            panel.inspection.fallback_revision = revision;
            refresh.push(panel.inspection.clone());
        }
        true
    });
    if let Some(connection) = &state.connection {
        for id in close {
            connection.close_command_output(id);
        }
        for intent in refresh {
            connection.inspect_command_output(intent);
        }
    }
}

pub(super) fn hide_run(state: &mut State, thread_id: &str, run_id: &str) {
    let Some(conversation) = state.conversation_mut(thread_id) else {
        return;
    };
    let mut close = Vec::new();
    conversation.command_panels.entries.retain(|_, panel| {
        let keep = panel.run_id.as_deref() != Some(run_id);
        if !keep {
            close.push(panel.inspection.attachment);
        }
        keep
    });
    if let Some(connection) = &state.connection {
        for id in close {
            connection.close_command_output(id);
        }
    }
}

fn toggle(
    state: &mut State,
    owner_thread_id: &str,
    target: &CommandOutputTarget,
    run_id: Option<String>,
    group_id: Option<String>,
    revision: Option<String>,
) {
    let Some(conversation) = state.conversation_mut(owner_thread_id) else {
        return;
    };
    if let Some(panel) = conversation.command_panels.entries.remove(target) {
        if let Some(connection) = &state.connection {
            connection.close_command_output(panel.inspection.attachment);
        }
        return;
    }
    let intent = CommandInspection {
        owner_thread_id: owner_thread_id.to_owned(),
        target: target.clone(),
        attachment: Uuid::new_v4(),
        fallback_revision: revision,
    };
    conversation.command_panels.entries.insert(
        target.clone(),
        Panel {
            inspection: intent.clone(),
            run_id,
            group_id,
            output: CommandOutput::Pending,
        },
    );
    if let Some(connection) = &state.connection {
        connection.inspect_command_output(intent);
    }
}

pub(super) fn view(
    conversation: &Conversation,
    command: &CommandEntry,
    colors: Colors,
    continues: bool,
) -> Option<El> {
    if let Some(group) = &command.group
        && !group.first
        && !conversation
            .command_panels
            .expanded_groups
            .contains(&group.id)
    {
        return None;
    }
    work_fold::view(
        conversation,
        command.run_id.as_deref(),
        command.completed_work.as_ref(),
        colors,
        || grouped_command(conversation, command, colors, continues),
    )
}

fn grouped_command(
    conversation: &Conversation,
    command: &CommandEntry,
    colors: Colors,
    continues: bool,
) -> El {
    let Some(group) = &command.group else {
        return command_row(conversation, command, colors, continues);
    };
    let expanded = conversation
        .command_panels
        .expanded_groups
        .contains(&group.id);
    if !group.first {
        return command_row(conversation, command, colors, continues);
    }
    let label = match &group.active_input {
        Some(input) => shell_command::program(input).map_or_else(
            || "Running command".into(),
            |program| format!("Running {program}"),
        ),
        None if group.count == 1 => "Ran 1 command".into(),
        None => format!("Ran {} commands", group.count),
    };
    let group_id = group.id.clone();
    let thread_id = conversation.id.clone();
    let header = row()
        .key(format!("toggle-command-group-{}", group.id))
        .role("button")
        .label(&label)
        .expanded(expanded)
        .focusable(true)
        .radius(6.0)
        .focus_ring(2.0, -1.0, colors.fallbacks.accent)
        .w_full()
        .min_h(24.0)
        .center_y()
        .gap(6.0)
        .on_press(move |state: &mut State| toggle_group(state, &thread_id, &group_id))
        .child(
            el().size(24.0, 24.0)
                .center()
                .child(icon(&TERMINAL, 16.0).color(colors.muted).a11y_hidden()),
        )
        .child(
            text(label)
                .font(super::PROSE)
                .color(colors.muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
        .child(
            el().size(16.0, 16.0).center().child(
                icon(if expanded { &COLLAPSE } else { &EXPAND }, 12.0)
                    .color(colors.muted.alpha(0.7))
                    .a11y_hidden(),
            ),
        );
    col()
        .w_full()
        .child(col().w_full().px(2.0).pt(2.0).pb(2.0).child(header))
        .children(expanded.then(|| command_row(conversation, command, colors, continues)))
}

fn toggle_group(state: &mut State, thread_id: &str, group_id: &str) {
    let Some(conversation) = state.conversation_mut(thread_id) else {
        return;
    };
    if !conversation.command_panels.expanded_groups.remove(group_id) {
        conversation
            .command_panels
            .expanded_groups
            .insert(group_id.to_owned());
        return;
    }
    let mut close = Vec::new();
    conversation.command_panels.entries.retain(|_, panel| {
        let keep = panel.group_id.as_deref() != Some(group_id);
        if !keep {
            close.push(panel.inspection.attachment);
        }
        keep
    });
    if let Some(connection) = &state.connection {
        for id in close {
            connection.close_command_output(id);
        }
    }
}

fn command_row(
    conversation: &Conversation,
    command: &CommandEntry,
    colors: Colors,
    continues: bool,
) -> El {
    let target = command.target();
    let panel = conversation.command_panels.entries.get(&target);
    let expanded = panel.is_some();
    let label = row_label(command);
    let action_label = if command.failed() {
        format!("{label}, tool call failed")
    } else {
        label.clone()
    };
    let can_expand = !command.input.trim().is_empty()
        || command.output_omitted
        || command
            .output
            .as_deref()
            .is_some_and(|out| !out.trim().is_empty())
        || command.exit_code.is_some_and(|exit| exit != 0);
    let failure = failure_color(colors);
    let mut header = row()
        .key(format!("toggle-command-{}", command.id))
        .w_full()
        .min_h(24.0)
        .center_y()
        .gap(6.0)
        .child(
            el().size(24.0, 24.0).center().child(
                icon(&TERMINAL, 16.0)
                    .color(if command.failed() {
                        failure
                    } else {
                        colors.muted
                    })
                    .a11y_hidden(),
            ),
        )
        .child(
            text(label)
                .font(super::PROSE)
                .color(colors.muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if can_expand {
        let owner = conversation.id.clone();
        let target = target.clone();
        let run_id = command.run_id.clone();
        let group_id = command.group.as_ref().map(|group| group.id.clone());
        let revision = fallback_revision(command);
        header = header
            .role("button")
            .label(action_label)
            .expanded(expanded)
            .focusable(true)
            .radius(6.0)
            .focus_ring(2.0, -1.0, colors.fallbacks.accent)
            .on_press(move |state: &mut State| {
                toggle(
                    state,
                    &owner,
                    &target,
                    run_id.clone(),
                    group_id.clone(),
                    revision.clone(),
                )
            })
            .child(
                el().size(16.0, 16.0).center().child(
                    icon(if expanded { &COLLAPSE } else { &EXPAND }, 12.0)
                        .color(colors.muted.alpha(0.7))
                        .a11y_hidden(),
                ),
            );
    }
    col()
        .w_full()
        .px(2.0)
        .pt(2.0)
        .pb(if continues { 2.0 } else { 10.0 })
        .child(header)
        .children(panel.map(|panel| command_details(command, &panel.output, colors)))
}

fn row_label(command: &CommandEntry) -> String {
    if !command.input.trim().is_empty() {
        return shell_command::heading(&command.input);
    }
    if let Some(title) = command
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !matches!(*title, "" | "Ran command" | "Run command" | "Command"))
    {
        return title.into();
    }
    match command.status {
        CommandStatus::Pending | CommandStatus::Running | CommandStatus::Waiting => {
            "Running command"
        }
        CommandStatus::Failed => "Failed command",
        CommandStatus::Cancelled | CommandStatus::Interrupted => "Stopped command",
        _ => "Ran command",
    }
    .into()
}

fn failure_color(colors: Colors) -> rune::Color {
    if colors.fallbacks.dark {
        oklch(0.704, 0.191, 22.216)
    } else {
        oklch(0.505, 0.213, 27.518)
    }
}

fn command_details(command: &CommandEntry, output: &CommandOutput, colors: Colors) -> El {
    let mut details = col()
        .key(format!("command-details-{}", command.id))
        .w_full()
        .pt(2.0)
        .pb(14.0)
        .gap(6.0);
    let input = command.input.trim();
    if !input.is_empty() {
        let code = shell_command::visible_controls(input);
        let spans = SyntaxTheme::neutral(colors.fallbacks.dark)
            .spans(&highlight(Some(shell_command::language(input)), &code), 0);
        details = details.child(
            text(code)
                .key(format!("command-input-{}", command.id))
                .font(CODE)
                .color(colors.foreground.alpha(0.85))
                .spans(spans)
                .wrap_anywhere()
                .selectable(true)
                .selection_color(colors.fallbacks.selected),
        );
    }
    let inline;
    let output = if matches!(output, CommandOutput::Inline) {
        let mut terminal = TerminalOutput::default();
        terminal.append(command.output.as_deref().unwrap_or_default());
        inline = CommandOutput::Output {
            text: terminal.text().into(),
            truncated: terminal.truncated(),
            running: command.status.is_active(),
        };
        &inline
    } else {
        output
    };
    let message = |content: &str| text(content).font(CODE.italic()).color(colors.muted);
    let output_view = match output {
        CommandOutput::Pending => Some(message("Loading output…")),
        CommandOutput::Failed(error) => Some(
            text(format!("Couldn't load output: {error}"))
                .font(CODE)
                .color(failure_color(colors)),
        ),
        CommandOutput::Missing => Some(message("Output is no longer available.")),
        CommandOutput::Output {
            text: output,
            truncated,
            ..
        } if !output.is_empty() => {
            let (content, spans) = command_ansi::spans(
                output,
                colors.fallbacks.dark,
                colors.muted,
                colors.background,
            );
            Some(
                col()
                    .key(format!("command-output-{}", command.id))
                    .w_full()
                    .max_h(320.0)
                    .scroll(ScrollSpec { stick_bottom: true })
                    .children(truncated.then(|| message("Earlier output not shown")))
                    .child(
                        text(content)
                            .font(CODE)
                            .color(colors.muted)
                            .spans(spans)
                            .wrap_anywhere()
                            .selectable(true)
                            .selection_color(colors.fallbacks.selected),
                    ),
            )
        }
        CommandOutput::Output { running: false, .. } => Some(message("No output.")),
        CommandOutput::Inline | CommandOutput::Output { .. } => None,
    };
    details
        .children(output_view)
        .children(command.exit_code.filter(|exit| *exit != 0).map(|exit| {
            text(format!("exit {exit}"))
                .font(CODE)
                .color(failure_color(colors))
        }))
}
