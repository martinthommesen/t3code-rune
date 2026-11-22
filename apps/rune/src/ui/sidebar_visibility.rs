#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SidebarMode {
    #[default]
    Docked,
    Sheet,
}

impl SidebarMode {
    pub(super) fn for_width(width: f32) -> Self {
        if width <= 767.0 {
            Self::Sheet
        } else {
            Self::Docked
        }
    }
}

pub(super) struct SidebarVisibility {
    mode: SidebarMode,
    dock_open: bool,
    sheet_open: bool,
}

impl Default for SidebarVisibility {
    fn default() -> Self {
        Self::new(true)
    }
}

impl SidebarVisibility {
    pub(super) fn new(dock_open: bool) -> Self {
        Self {
            mode: SidebarMode::Docked,
            dock_open,
            sheet_open: false,
        }
    }

    pub(super) fn resize(&mut self, width: f32) {
        self.mode = SidebarMode::for_width(width);
    }

    pub(super) fn open_in(&self, mode: SidebarMode) -> bool {
        match mode {
            SidebarMode::Docked => self.dock_open,
            SidebarMode::Sheet => self.sheet_open,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open_in(self.mode)
    }

    pub(super) fn toggle(&mut self) -> Option<bool> {
        match self.mode {
            SidebarMode::Docked => {
                self.dock_open = !self.dock_open;
                Some(self.dock_open)
            }
            SidebarMode::Sheet => {
                self.sheet_open = !self.sheet_open;
                None
            }
        }
    }

    pub(super) fn close_if_sheet(&mut self) {
        if self.mode == SidebarMode::Sheet {
            self.sheet_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_sidebar_uses_the_dock_only_at_768_points_and_above() {
        let mut sidebar = SidebarVisibility::default();
        sidebar.resize(767.0);
        assert!(!sidebar.is_open());
        sidebar.resize(768.0);
        assert!(sidebar.is_open());
        assert_eq!(sidebar.toggle(), Some(false));
        sidebar.resize(600.0);
        assert!(!sidebar.is_open());
    }

    #[test]
    fn dock_preference_and_sheet_session_survive_width_changes_independently() {
        for saved in [false, true] {
            let mut sidebar = SidebarVisibility::new(saved);
            assert_eq!(sidebar.is_open(), saved);
            sidebar.resize(600.0);
            assert!(!sidebar.is_open());
            assert_eq!(sidebar.toggle(), None);
            assert!(sidebar.is_open());
            sidebar.resize(768.0);
            assert_eq!(sidebar.is_open(), saved);
            sidebar.close_if_sheet();
            sidebar.resize(600.0);
            assert!(sidebar.is_open());
            sidebar.close_if_sheet();
            assert!(!sidebar.is_open());
            sidebar.resize(1120.0);
            assert_eq!(sidebar.toggle(), Some(!saved));
        }
    }
}
