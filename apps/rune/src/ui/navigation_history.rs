#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Destination {
    Home,
    Project(String),
    Thread { project: String, thread: String },
    NewThread(String),
}

impl Destination {
    pub(super) fn project(&self) -> Option<&str> {
        match self {
            Self::Home => None,
            Self::Project(project) | Self::Thread { project, .. } | Self::NewThread(project) => {
                Some(project)
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Direction {
    Back,
    Forward,
}

#[derive(Default)]
pub(super) struct History {
    back: Vec<Destination>,
    forward: Vec<Destination>,
}

impl History {
    pub(super) fn record(&mut self, previous: Destination) {
        self.back.push(previous);
        self.forward.clear();
    }

    pub(super) fn available(
        &self,
        direction: Direction,
        can_visit: impl Fn(&Destination) -> bool,
    ) -> bool {
        let stack = match direction {
            Direction::Back => &self.back,
            Direction::Forward => &self.forward,
        };
        stack.iter().rev().any(can_visit)
    }

    pub(super) fn go(
        &mut self,
        direction: Direction,
        current: Destination,
        can_visit: impl Fn(&Destination) -> bool,
    ) -> Option<Destination> {
        let (stack, reverse) = match direction {
            Direction::Back => (&mut self.back, &mut self.forward),
            Direction::Forward => (&mut self.forward, &mut self.back),
        };
        while let Some(destination) = stack.pop() {
            if destination != current && can_visit(&destination) {
                reverse.push(current);
                return Some(destination);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Destination, Direction, History};

    #[test]
    fn navigating_back_skips_unavailable_destinations_until_it_finds_a_live_one() {
        let mut history = History::default();
        let project = Destination::Project("project".into());
        history.record(Destination::Home);
        history.record(Destination::Project("deleted".into()));

        let back = history.go(Direction::Back, project.clone(), |d| {
            *d == Destination::Home
        });
        assert_eq!(back, Some(Destination::Home));
        assert!(!history.available(Direction::Back, |_| true));
        assert_eq!(
            history.go(Direction::Forward, Destination::Home, |_| true),
            Some(project)
        );
    }

    #[test]
    fn a_new_destination_replaces_the_forward_branch() {
        let mut history = History::default();
        let project = Destination::Project("project".into());
        history.record(Destination::Home);
        assert_eq!(
            history.go(Direction::Back, project, |_| true),
            Some(Destination::Home)
        );
        history.record(Destination::Home);
        assert!(!history.available(Direction::Forward, |_| true));
    }
}
