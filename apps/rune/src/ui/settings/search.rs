use std::collections::HashSet;

use rune::Tx;

use super::{Page, keybinding_catalog, keybindings};
use crate::connection::{Binding, Keybindings};
use crate::ui::State;

#[derive(Clone, Copy)]
pub(in crate::ui) enum Item<'a> {
    Setting(&'static Setting),
    Keybinding(&'a Binding),
}

pub(in crate::ui) struct Setting {
    page: Page,
    id: &'static str,
    anchor: &'static str,
    title: &'static str,
    terms: &'static str,
}

const SETTINGS: &[Setting] = &[
    Setting {
        page: Page::Worktrees,
        id: "storage-worktrees-location",
        anchor: "worktree-location-row",
        title: "Worktree location",
        terms: "folder directory path drive external disk",
    },
    Setting {
        page: Page::Threads,
        id: "default-permissions",
        anchor: "new-threads-permissions",
        title: "Permissions",
        terms: "new thread default runtime mode supervised approvals auto accept edits full access",
    },
    Setting {
        page: Page::Appearance,
        id: "color-scheme",
        anchor: "appearance",
        title: "Color scheme",
        terms: "appearance light dark system mode",
    },
    Setting {
        page: Page::Threads,
        id: "snooze-options",
        anchor: "snooze-options",
        title: "Snooze options",
        terms: "snooze presets times later tonight tomorrow next week remind",
    },
    Setting {
        page: Page::Threads,
        id: "new-threads",
        anchor: "settings-new-threads",
        title: "New threads",
        terms: "default workspace mode draft local worktree",
    },
    Setting {
        page: Page::Worktrees,
        id: "start-from-origin",
        anchor: "start-from-origin",
        title: "Start from remote",
        terms: "new worktrees start remote origin latest fetched branch local",
    },
    Setting {
        page: Page::General,
        id: "delete-confirmation",
        anchor: "delete-confirmation",
        title: "Delete confirmation",
        terms: "ask before thread chat history",
    },
    Setting {
        page: Page::General,
        id: "terminal-close-confirmation",
        anchor: "terminal-close-confirmation",
        title: "Terminal close confirmation",
        terms: "ask before delete stop process shell console history",
    },
    Setting {
        page: Page::Worktrees,
        id: "worktree-branch-naming",
        anchor: "worktree-branch-naming-row",
        title: "Worktree branch naming",
        terms: "static semantic prefix custom prompt instructions feat fix refactor chore",
    },
    Setting {
        page: Page::Archive,
        id: "archive",
        anchor: "archive-settings-threads",
        title: "Archived threads",
        terms: "restore reopen deleted history projects",
    },
];

impl Item<'_> {
    pub(in crate::ui) fn key(self) -> String {
        match self {
            Self::Setting(setting) => format!("command-palette-setting-{}", setting.id),
            Self::Keybinding(binding) => {
                format!(
                    "command-palette-setting-keybinding-{}",
                    binding.rule.command
                )
            }
        }
    }

    pub(in crate::ui) fn title(self) -> String {
        match self {
            Self::Setting(setting) => setting.title.to_owned(),
            Self::Keybinding(binding) => keybinding_catalog::command_label(&binding.rule.command),
        }
    }

    pub(in crate::ui) fn page_label(self) -> &'static str {
        match self {
            Self::Setting(setting) => setting.page.label(),
            Self::Keybinding(_) => Page::Keybindings.label(),
        }
    }

    pub(in crate::ui) fn keywords(self) -> Vec<String> {
        match self {
            Self::Setting(setting) => {
                vec![setting.page.label().to_owned(), setting.terms.to_owned()]
            }
            Self::Keybinding(binding) => {
                let mut keywords = vec![self.page_label().to_owned(), binding.rule.command.clone()];
                keywords.extend(
                    Keybindings::stock()
                        .bindings()
                        .iter()
                        .filter(|stock| stock.rule.command == binding.rule.command)
                        .map(|stock| stock.rule.key.clone()),
                );
                keywords
            }
        }
    }

    pub(in crate::ui) fn secondary(self) -> bool {
        matches!(self, Self::Keybinding(_))
    }

    pub(in crate::ui) fn destination(self) -> Destination {
        match self {
            Self::Setting(setting) => Destination {
                page: setting.page,
                key: setting.anchor.into(),
            },
            Self::Keybinding(binding) => Destination {
                page: Page::Keybindings,
                key: keybindings::search_row_key(&binding.rule),
            },
        }
    }
}

pub(in crate::ui) fn items(state: &State) -> Vec<Item<'_>> {
    let bindings = match &state.keybindings {
        Some(bindings) => bindings,
        None => Keybindings::stock(),
    };
    let mut seen = HashSet::new();
    let mut key_bindings: Vec<_> = keybinding_catalog::groups(bindings, "")
        .into_iter()
        .flatten()
        .filter(|row| seen.insert(row.binding.rule.command.as_str()))
        .map(|row| (row.label, Item::Keybinding(row.binding)))
        .collect();
    key_bindings.sort_by_cached_key(|(label, _)| label.to_lowercase());
    SETTINGS
        .iter()
        .map(Item::Setting)
        .chain(key_bindings.into_iter().map(|(_, entry)| entry))
        .collect()
}

pub(in crate::ui) struct Destination {
    page: Page,
    key: String,
}

impl State {
    pub(in crate::ui) fn open_settings_destination(
        &mut self,
        destination: &Destination,
        tx: &mut Tx,
    ) {
        if !self.settings.is_open() {
            self.open_client_settings(tx);
        }
        self.select_settings_page(destination.page);
        if destination.page == Page::Keybindings {
            self.settings.keybindings.clear_view();
        }
        tx.scroll_center(destination.key.clone());
        tx.focus_when_shown(destination.key.clone(), false);
    }
}
