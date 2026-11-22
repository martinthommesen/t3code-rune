use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::connection::TerminalTarget;

#[derive(Default)]
pub(in crate::ui) struct TabsByThread {
    layouts: HashMap<String, Tabs>,
    closing: HashSet<TerminalTarget>,
}

impl TabsByThread {
    pub(in crate::ui) fn clear(&mut self) {
        self.layouts.clear();
        self.closing.clear();
    }

    pub(super) fn get(&self, thread_id: &str) -> Option<&Tabs> {
        self.layouts.get(thread_id)
    }

    pub(super) fn get_mut(&mut self, thread_id: &str) -> Option<&mut Tabs> {
        self.layouts.get_mut(thread_id)
    }

    pub(super) fn ensure(&mut self, thread_id: &str) -> &mut Tabs {
        self.layouts.entry(thread_id.to_owned()).or_insert_with(|| {
            let closing: Vec<_> = self
                .closing
                .iter()
                .filter(|close| close.thread_id == thread_id)
                .collect();
            if closing.iter().any(|target| target.terminal_id == "term-1") {
                let mut tabs = Tabs {
                    entries: Vec::new(),
                    active: 0,
                };
                tabs.append(
                    closing
                        .into_iter()
                        .map(|target| target.terminal_id.as_str()),
                );
                tabs
            } else {
                Tabs::default()
            }
        })
    }

    pub(super) fn append(&mut self, thread_id: &str) -> bool {
        let Some(tabs) = self.layouts.get_mut(thread_id) else {
            return false;
        };
        tabs.append(
            self.closing
                .iter()
                .filter(|target| target.thread_id == thread_id)
                .map(|target| target.terminal_id.as_str()),
        );
        true
    }

    pub(super) fn select_observed(&mut self, target: &TerminalTarget, label: &str) -> bool {
        if self.closing.contains(target) {
            return false;
        }
        let tabs = self.ensure(&target.thread_id);
        if let Some(index) = tabs
            .entries
            .iter()
            .position(|tab| tab.id == target.terminal_id)
        {
            tabs.entries[index].resume = true;
            tabs.entries[index].observed_label = Some(label.to_owned());
            tabs.active = index;
            return true;
        }
        tabs.active = tabs.entries.len();
        tabs.entries.push(Tab {
            id: target.terminal_id.clone(),
            resume: true,
            observed_label: Some(label.to_owned()),
        });
        true
    }

    pub(super) fn begin_close(&mut self, target: TerminalTarget) -> bool {
        let Some(tabs) = self.layouts.get_mut(&target.thread_id) else {
            return false;
        };
        let Some(index) = tabs
            .entries
            .iter()
            .position(|tab| tab.id == target.terminal_id)
        else {
            return false;
        };
        tabs.entries.remove(index);
        if tabs.entries.is_empty() {
            self.layouts.remove(&target.thread_id);
        } else {
            tabs.active = tabs
                .active
                .saturating_sub(usize::from(index < tabs.active))
                .min(tabs.entries.len() - 1);
        }
        self.closing.insert(target);
        true
    }

    pub(super) fn settle_close(&mut self, target: &TerminalTarget) -> bool {
        self.closing.remove(target)
    }
}

pub(super) struct Tabs {
    pub(super) entries: Vec<Tab>,
    active: usize,
}

impl Default for Tabs {
    fn default() -> Self {
        Self {
            entries: vec![Tab::new("term-1".into())],
            active: 0,
        }
    }
}

impl Tabs {
    pub(super) fn selected(&self) -> &Tab {
        &self.entries[self.active]
    }

    pub(super) fn tab(&self, terminal_id: &str) -> Option<&Tab> {
        self.entries.iter().find(|tab| tab.id == terminal_id)
    }

    pub(super) fn select(&mut self, id: &str) -> bool {
        let Some(index) = self.entries.iter().position(|tab| tab.id == id) else {
            return false;
        };
        let changed = index != self.active;
        self.active = index;
        changed
    }

    fn append<'a>(&mut self, closing: impl Iterator<Item = &'a str>) {
        let used: HashSet<_> = self
            .entries
            .iter()
            .map(|tab| tab.id.as_str())
            .filter_map(terminal_number)
            .chain(closing.filter_map(terminal_number))
            .collect();
        let number = (1..)
            .find(|number| !used.contains(number))
            .expect("one available terminal number");
        self.active = self.entries.len();
        self.entries
            .push(Tab::new(format!("term-{number}-{}", Uuid::new_v4())));
    }

    pub(super) fn remember(&mut self, pane: &super::Pane) {
        if let Some(tab) = self
            .entries
            .iter_mut()
            .find(|tab| tab.id == pane.terminal_id)
        {
            tab.resume |= pane.received_snapshot;
        }
    }
}

pub(super) struct Tab {
    pub(super) id: String,
    pub(super) resume: bool,
    pub(super) observed_label: Option<String>,
}

impl Tab {
    fn new(id: String) -> Self {
        Self {
            id,
            resume: false,
            observed_label: None,
        }
    }

    pub(super) fn label(&self) -> String {
        self.observed_label.clone().unwrap_or_else(|| {
            terminal_number(&self.id).map_or_else(|| self.id.clone(), |n| format!("Terminal {n}"))
        })
    }
}

fn terminal_number(id: &str) -> Option<usize> {
    let id = id.to_ascii_lowercase();
    let body = id
        .strip_prefix("term-")
        .or_else(|| id.strip_prefix("terminal-"))?;
    let (number, suffix) = body.split_once('-').unwrap_or((body, ""));
    if (number.len() != body.len() && (suffix.len() != 36 || Uuid::parse_str(suffix).is_err()))
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    number.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(thread_id: &str, terminal_id: &str) -> TerminalTarget {
        TerminalTarget {
            thread_id: thread_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
        }
    }

    #[test]
    fn independent_threads_keep_selection_and_reuse_the_first_free_number() {
        let mut by_thread = TabsByThread::default();
        assert_eq!(by_thread.ensure("first").selected().id, "term-1");
        assert!(by_thread.append("first"));
        let second = by_thread.get("first").unwrap().selected().id.clone();
        assert!(second.starts_with("term-2-"));
        let tabs = by_thread.get("first").unwrap();
        assert_eq!(tabs.selected().label(), "Terminal 2");
        assert!(by_thread.append("first"));
        let tabs = by_thread.get_mut("first").unwrap();
        assert!(tabs.selected().id.starts_with("term-3-"));
        assert!(tabs.select(&second));
        let second_target = target("first", &second);
        assert!(by_thread.begin_close(second_target.clone()));
        assert_eq!(
            by_thread.get("first").unwrap().selected().label(),
            "Terminal 3"
        );
        assert!(by_thread.append("first"));
        assert_eq!(
            by_thread.get("first").unwrap().selected().label(),
            "Terminal 4"
        );
        assert!(by_thread.settle_close(&second_target));
        assert!(by_thread.append("first"));
        let tabs = by_thread.get("first").unwrap();
        assert_eq!(tabs.selected().label(), "Terminal 2");
        assert_ne!(tabs.selected().id, second);
        assert_eq!(by_thread.ensure("other").selected().id, "term-1");
        assert_eq!(
            by_thread.get("first").unwrap().selected().label(),
            "Terminal 2"
        );
        let other = target("other", "term-1");
        assert!(by_thread.begin_close(other.clone()));
        assert!(by_thread.get("other").is_none());
        assert!(by_thread.settle_close(&other));
        assert_eq!(by_thread.ensure("other").selected().id, "term-1");
    }

    #[test]
    fn closing_an_inactive_terminal_keeps_the_selected_tab() {
        let mut by_thread = TabsByThread::default();
        by_thread.ensure("thread");
        assert!(by_thread.append("thread"));
        let active = by_thread.get("thread").unwrap().selected().id.clone();
        assert!(by_thread.begin_close(target("thread", "term-1")));
        assert_eq!(by_thread.get("thread").unwrap().selected().id, active);
        assert!(by_thread.get("thread").unwrap().tab("term-1").is_none());
    }
}
