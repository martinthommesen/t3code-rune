use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::connection::TerminalTarget;

pub(super) const MAX_TERMINALS_PER_GROUP: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SplitDirection {
    Horizontal,
    Vertical,
}

pub(super) struct Group {
    pub(super) id: String,
    pub(super) members: Vec<String>,
    pub(super) direction: SplitDirection,
}

impl Group {
    fn single(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            members: vec![id.to_owned()],
            direction: SplitDirection::Horizontal,
        }
    }
}

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
                let mut tabs = Tabs::empty();
                tabs.append(
                    closing
                        .into_iter()
                        .map(|target| target.terminal_id.as_str()),
                    None,
                );
                tabs
            } else {
                Tabs::default()
            }
        })
    }

    pub(super) fn append(&mut self, thread_id: &str) -> bool {
        self.place(thread_id, None)
    }

    pub(super) fn split(&mut self, thread_id: &str, direction: SplitDirection) -> bool {
        self.place(thread_id, Some(direction))
    }

    fn place(&mut self, thread_id: &str, direction: Option<SplitDirection>) -> bool {
        let Some(tabs) = self.layouts.get_mut(thread_id) else {
            return false;
        };
        tabs.append(
            self.closing
                .iter()
                .filter(|target| target.thread_id == thread_id)
                .map(|target| target.terminal_id.as_str()),
            direction,
        )
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
        tabs.push(
            Tab {
                id: target.terminal_id.clone(),
                resume: true,
                observed_label: Some(label.to_owned()),
            },
            None,
        );
        true
    }

    pub(super) fn begin_close(&mut self, target: TerminalTarget) -> bool {
        let Some(tabs) = self.layouts.get_mut(&target.thread_id) else {
            return false;
        };
        if !tabs.remove(&target.terminal_id) {
            return false;
        }
        if tabs.entries.is_empty() {
            self.layouts.remove(&target.thread_id);
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
    pub(super) groups: Vec<Group>,
    active: usize,
}

impl Default for Tabs {
    fn default() -> Self {
        let mut tabs = Self::empty();
        tabs.push(Tab::new("term-1".into()), None);
        tabs
    }
}

impl Tabs {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            groups: Vec::new(),
            active: 0,
        }
    }

    pub(super) fn active_group(&self) -> Option<&Group> {
        let id = &self.entries.get(self.active)?.id;
        self.groups.iter().find(|group| group.members.contains(id))
    }

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

    fn append<'a>(
        &mut self,
        closing: impl Iterator<Item = &'a str>,
        direction: Option<SplitDirection>,
    ) -> bool {
        if direction.is_some()
            && self
                .active_group()
                .is_some_and(|group| group.members.len() >= MAX_TERMINALS_PER_GROUP)
        {
            return false;
        }
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
        self.push(
            Tab::new(format!("term-{number}-{}", Uuid::new_v4())),
            direction,
        );
        true
    }

    fn push(&mut self, tab: Tab, direction: Option<SplitDirection>) {
        if let Some(direction) = direction
            && let Some(selected) = self.entries.get(self.active)
            && let Some((group, index)) = self.groups.iter_mut().find_map(|group| {
                group
                    .members
                    .iter()
                    .position(|id| *id == selected.id)
                    .map(|index| (group, index))
            })
        {
            group.members.insert(index + 1, tab.id.clone());
            group.direction = direction;
        } else {
            self.groups.push(Group::single(&tab.id));
        }
        self.active = self.entries.len();
        self.entries.push(tab);
    }

    fn remove(&mut self, id: &str) -> bool {
        let Some(index) = self.entries.iter().position(|tab| tab.id == id) else {
            return false;
        };
        let mut replacement = None;
        if let Some((group, position)) = self.groups.iter_mut().find_map(|group| {
            group
                .members
                .iter()
                .position(|member| member == id)
                .map(|position| (group, position))
        }) {
            group.members.remove(position);
            replacement = group
                .members
                .get(position)
                .or_else(|| {
                    position
                        .checked_sub(1)
                        .and_then(|index| group.members.get(index))
                })
                .cloned();
        }
        self.groups.retain(|group| !group.members.is_empty());
        self.entries.remove(index);
        self.active = if self.active == index {
            replacement
                .and_then(|id| self.entries.iter().position(|tab| tab.id == id))
                .unwrap_or_else(|| index.min(self.entries.len().saturating_sub(1)))
        } else {
            self.active - usize::from(index < self.active)
        };
        true
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
    fn split_inserts_after_the_active_member_and_updates_the_group_direction() {
        let mut tabs = TabsByThread::default();
        tabs.ensure("thread");
        assert!(tabs.split("thread", SplitDirection::Horizontal));
        let second = tabs.get("thread").unwrap().selected().id.clone();
        assert!(tabs.get_mut("thread").unwrap().select("term-1"));
        assert!(tabs.split("thread", SplitDirection::Vertical));
        let group = tabs.get("thread").unwrap().active_group().unwrap();
        assert_eq!(group.direction, SplitDirection::Vertical);
        assert_eq!(group.members[0], "term-1");
        assert_eq!(group.members[2], second);
        let third = group.members[1].clone();
        assert!(tabs.begin_close(target("thread", &third)));
        assert_eq!(tabs.get("thread").unwrap().selected().id, second);
    }

    #[test]
    fn split_limit_keeps_terminal_numbers_available_for_new_groups() {
        let mut tabs = TabsByThread::default();
        tabs.ensure("thread");
        for _ in 1..MAX_TERMINALS_PER_GROUP {
            assert!(tabs.split("thread", SplitDirection::Horizontal));
        }
        assert!(!tabs.split("thread", SplitDirection::Vertical));
        assert!(tabs.append("thread"));
        let tabs = tabs.get("thread").unwrap();
        assert_eq!(tabs.selected().label(), "Terminal 5");
        assert_eq!(tabs.active_group().unwrap().members.len(), 1);
        assert_eq!(tabs.groups[0].direction, SplitDirection::Horizontal);
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
