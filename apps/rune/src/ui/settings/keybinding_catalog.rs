use std::collections::HashSet;

use crate::connection::{Binding, KeybindingRule, Keybindings, TERMINAL_TOGGLE};

pub(super) const EDITABLE_COMMANDS: &[&str] = &[
    "chat.new",
    "commandPalette.toggle",
    "sidebar.toggle",
    "terminal.close",
    "terminal.new",
    "terminal.split",
    "terminal.splitVertical",
    TERMINAL_TOGGLE,
];

pub(super) const GROUPS: &[(&str, &[&str])] = &[
    (
        "Navigation",
        &[
            "sidebar",
            "rightPanel",
            "commandPalette",
            "filePicker",
            "projectSearch",
            "editor",
        ],
    ),
    ("Threads", &["thread", "chat", "pullRequest"]),
    ("Composer", &["composer", "modelPicker"]),
    ("Terminal", &["terminal"]),
    ("Preview & diff", &["preview", "diff"]),
    ("Appearance", &["theme", "appearance", "themeEditor"]),
    ("Project scripts", &["script"]),
    ("Other", &[]),
];

pub(super) struct Row<'a> {
    pub(super) binding: &'a Binding,
    pub(super) label: String,
    pub(super) source: Source,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Default,
    Custom,
    Project,
}

impl Source {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Custom => "Custom",
            Self::Project => "Project",
        }
    }
}

impl Row<'_> {
    pub(super) fn unassigned(&self) -> bool {
        self.binding.rule.when.as_deref() == Some("false")
            && stock_rule(&self.binding.rule).is_some_and(|rule| rule.key == self.binding.rule.key)
    }
}

pub(super) fn stock_rule(rule: &KeybindingRule) -> Option<&'static KeybindingRule> {
    let targets = Keybindings::stock()
        .bindings()
        .iter()
        .map(|binding| &binding.rule)
        .filter(|candidate| candidate.command == rule.command);
    targets
        .clone()
        .find(|candidate| *candidate == rule)
        .or_else(|| {
            targets.clone().find(|candidate| {
                candidate.when == rule.when
                    || (rule.when.as_deref() == Some("false") && candidate.key == rule.key)
            })
        })
        .or_else(|| targets.clone().next())
}

pub(super) fn groups<'a>(bindings: &'a Keybindings, query: &str) -> Vec<Vec<Row<'a>>> {
    let query = query.trim().to_lowercase();
    let mut groups: Vec<Vec<Row<'_>>> = GROUPS.iter().map(|_| Vec::new()).collect();
    let mut seen = HashSet::new();
    for binding in bindings.bindings().iter().rev() {
        let rule = &binding.rule;
        if !seen.insert((&rule.command, &rule.key, &rule.when)) {
            continue;
        }
        let label = command_label(&rule.command);
        let source = if rule.command.starts_with("script.") {
            Source::Project
        } else if stock_rule(rule).is_some_and(|default| default == rule) {
            Source::Default
        } else {
            Source::Custom
        };
        let matches = [
            rule.command.as_str(),
            &label,
            &rule.key,
            rule.when.as_deref().unwrap_or_default(),
            source.label(),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(&query));
        if matches {
            let prefix = rule.command.split('.').next().unwrap_or_default();
            let group = GROUPS
                .iter()
                .position(|(_, prefixes)| prefixes.contains(&prefix))
                .unwrap_or(GROUPS.len() - 1);
            groups[group].push(Row {
                binding,
                label,
                source,
            });
        }
    }
    for rows in &mut groups {
        rows.sort_by(|left, right| {
            let left = &left.binding.rule;
            let right = &right.binding.rule;
            sort_key(&left.command)
                .cmp(&sort_key(&right.command))
                .then_with(|| left.key.cmp(&right.key))
        });
    }
    groups
}

fn sort_key(command: &str) -> (&str, usize) {
    let usage = [
        "usage.cost",
        "usage.tokens",
        "usage.limits",
        "usage.period.day",
        "usage.period.week",
        "usage.period.month",
        "usage.period.quarter",
    ];
    usage
        .iter()
        .position(|candidate| *candidate == command)
        .map_or((command, 0), |rank| ("usage.open", rank + 1))
}

pub(super) fn command_label(command: &str) -> String {
    let special = match command {
        "workspace.focusNext" => "Focus Next Workspace Area",
        "workspace.focusPrevious" => "Focus Previous Workspace Area",
        "composer.sendAlternate" => "Composer: Opposite Queue or Steer Action",
        "composer.sendBackground" => "Composer: Start in Background",
        "composer.sendAndNewThread" => "Composer: Send and Start New Thread",
        "composer.dictation" => "Composer: Start or Finish Dictation",
        "thread.steerQueuedMessage" => "Queue: Send First Queued Message",
        "thread.editQueuedMessage" => "Queue: Edit Last Queued Message",
        "thread.copyReference" => "Pull Request: Copy Link or Thread",
        "sidebar.toggle" => "Sidebar: Toggle",
        "navigation.back" => "Navigation: Back",
        "navigation.forward" => "Navigation: Forward",
        "terminal.toggle" => "Terminal: Toggle",
        "terminal.split" => "Terminal: Split",
        "terminal.splitVertical" => "Terminal: Split Vertical",
        "terminal.new" => "Terminal: New",
        "terminal.close" => "Terminal: Close",
        "rightPanel.toggle" => "Right Panel: Toggle",
        "rightPanel.new" => "Right Panel: New",
        "threadPanel.toggle" => "Thread Panel: Toggle",
        "rightPanel.toggleMaximized" => "Right Panel: Toggle Maximized",
        "rightPanel.close" => "Right Panel: Close",
        "pullRequest.copyNumber" => "Pull Request: Copy Number",
        "diff.toggle" => "Diff: Toggle",
        "preview.toggle" => "Preview: Toggle",
        "preview.refresh" => "Preview: Refresh",
        "preview.zoomIn" => "Preview: Zoom In",
        "preview.zoomOut" => "Preview: Zoom Out",
        "preview.resetZoom" => "Preview: Reset Zoom",
        "commandPalette.toggle" => "Command Palette: Toggle",
        "filePicker.toggle" => "File Picker: Toggle",
        "projectSearch.toggle" => "Project Search: Toggle",
        "usage.open" => "Usage: Open",
        "theme.select" => "Theme: Select",
        "appearance.cycle" => "Appearance: Cycle",
        "themeEditor.toggle" => "Theme Editor: Toggle",
        "composer.stash" => "Composer: Stash",
        "composer.host" => "Composer: Host",
        "composer.cycleHost" => "Composer: Cycle Host",
        "composer.effort" => "Composer: Effort",
        "composer.increaseEffort" => "Composer: Increase Effort",
        "composer.decreaseEffort" => "Composer: Decrease Effort",
        "composer.mode" => "Composer: Mode",
        "composer.workspace" => "Composer: Workspace",
        "composer.previousWorktree" => "Composer: Previous Worktree",
        "composer.branch" => "Composer: Branch",
        "chat.new" => "Chat: New",
        "chat.newLocal" => "Chat: New Local",
        "chat.newWithoutProject" => "Chat: New Without Project",
        "chat.find" => "Chat: Find",
        "editor.openFavorite" => "Editor: Open Favorite",
        "modelPicker.toggle" => "Model Picker: Toggle",
        "modelPicker.previousProvider" => "Model Picker: Previous Provider",
        "modelPicker.nextProvider" => "Model Picker: Next Provider",
        "thread.stop" => "Thread: Stop",
        "thread.previous" => "Thread: Previous",
        "thread.next" => "Thread: Next",
        "thread.settle" => "Thread: Settle",
        "thread.pin" => "Thread: Pin",
        "thread.regenerateTitle" => "Thread: Regenerate Title",
        "thread.undo" => "Thread: Undo",
        "usage.cost" => "Usage: Cost",
        "usage.tokens" => "Usage: Tokens",
        "usage.limits" => "Usage: Limits",
        "usage.period.day" => "Usage: Period: Past 24h",
        "usage.period.week" => "Usage: Period: 7 days",
        "usage.period.month" => "Usage: Period: 30 days",
        "usage.period.quarter" => "Usage: Period: 90 days",
        "view.reopenClosed" => "Reopen Closed Tab",
        "preview.focusUrl" => "Preview: Focus URL",
        "preview.find" => "Preview: Find on Page",
        _ => "",
    };
    if !special.is_empty() {
        return special.to_owned();
    }
    if let Some(number) = command.strip_prefix("thread.jump.") {
        return format!("Thread: Jump: {number}");
    }
    if let Some(number) = command.strip_prefix("modelPicker.jump.") {
        return format!("Model Picker: Jump: {number}");
    }
    if let Some(script) = command
        .strip_prefix("script.")
        .and_then(|name| name.strip_suffix(".run"))
    {
        return format!("Run Script: {}", title_case(script));
    }
    command
        .split('.')
        .map(title_case)
        .collect::<Vec<_>>()
        .join(": ")
}

fn title_case(segment: &str) -> String {
    let mut text = String::new();
    let mut boundary = true;
    for character in segment.chars() {
        if character == '-' || character == '_' || character.is_whitespace() {
            boundary = true;
            continue;
        }
        if boundary || character.is_uppercase() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.extend(character.to_uppercase());
        } else {
            text.push(character);
        }
        boundary = false;
    }
    text
}
