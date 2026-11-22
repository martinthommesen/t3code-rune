use rune::Tx;

use crate::connection::{Project, ProjectEdit, Thread, latest_thread_by_order};
use crate::workspace_path::comparison_path;

use super::super::{Destination, State};

impl State {
    pub(super) fn open_registered_project(&mut self, path: &str, tx: &mut Tx) -> bool {
        let Some(workspace) = &self.workspace else {
            return false;
        };
        let Some(shell) = &workspace.shell else {
            return false;
        };
        let Some(project) = existing_project(&shell.projects, path) else {
            return false;
        };
        let project_id = project.id.clone();
        let latest = latest_thread(
            &shell.threads,
            &project_id,
            self.client_settings.threads_ordered_by_creation(),
        )
        .filter(|thread| !thread.is_settled())
        .map(|thread| thread.id.clone());
        if project.archived_at.is_some()
            && let Some(connection) = &self.connection
        {
            connection.edit_project(project_id.clone(), ProjectEdit::UnarchiveFromPicker);
        }
        self.add_project = None;
        if let Some(thread) = latest {
            self.open_thread(&thread);
            tx.focus_when_shown("prompt", false);
        } else if workspace.launch_writes {
            self.navigate(Destination::NewThread(project_id));
            tx.focus_when_shown("initial-prompt", false);
        } else {
            self.select_project(&project_id);
        }
        true
    }
}

fn existing_project<'a>(projects: &'a [Project], path: &str) -> Option<&'a Project> {
    let key = comparison_path(path);
    if key.is_empty() {
        return None;
    }
    projects
        .iter()
        .find(|project| comparison_path(&project.path) == key)
}

fn latest_thread<'a>(
    threads: &'a [Thread],
    project_id: &str,
    ordered_by_creation: bool,
) -> Option<&'a Thread> {
    let threads = threads.iter().filter(|thread| {
        thread.project_id == project_id && !thread.archived && thread.archived_at().is_none()
    });
    latest_thread_by_order(threads, ordered_by_creation)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn registered_folder_matches_stock_posix_and_windows_paths() {
        let projects = [
            serde_json::from_value(json!({
                "id": "unix", "title": "Unix", "workspaceRoot": "/projects/T3\\file"
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "id": "windows", "title": "Windows", "workspaceRoot": "C:\\Projects\\T3"
            }))
            .unwrap(),
        ];
        assert_eq!(
            existing_project(&projects, " /projects/T3\\file///\u{feff}")
                .map(|project| project.id.as_str()),
            Some("unix")
        );
        assert!(existing_project(&projects, "/projects/t3\\file").is_none());
        assert!(existing_project(&projects, "/projects/T3/file").is_none());
        assert_eq!(
            existing_project(&projects, "c:/projects/t3/").map(|project| project.id.as_str()),
            Some("windows")
        );
    }

    #[test]
    fn latest_eligible_thread_respects_device_order_and_settlement() {
        let seed = |id, created, authored, archived_at, settled| {
            serde_json::from_value(json!({
                "id": id, "title": id, "projectId": "p", "createdAt": created,
                "updatedAt": "2025-01-21T12:00:00Z", "latestUserAuthoredMessageAt": authored,
                "archivedAt": archived_at, "settledOverride": settled
            }))
            .unwrap()
        };
        let projects = [
            seed("newer-created", "2025-01-14T12:00:00Z", None, None, None),
            seed(
                "more-recent",
                "2025-01-13T12:00:00Z",
                Some("2025-01-21T11:00:00Z"),
                None,
                Some("settled"),
            ),
            seed(
                "archived",
                "2025-01-18T12:00:00Z",
                Some("2025-01-21T12:00:00Z"),
                Some("2025-01-21T12:00:00Z"),
                None,
            ),
        ];
        let latest = latest_thread(&projects, "p", false).unwrap();
        assert_eq!(latest.id, "more-recent");
        assert!(latest.is_settled());
        let latest = latest_thread(&projects, "p", true).unwrap();
        assert_eq!(latest.id, "newer-created");
        assert!(!latest.is_settled());
    }

    #[test]
    fn latest_ties_use_javascript_id_order_and_millisecond_precision() {
        let threads = [
            serde_json::from_value(json!({
                "id": "\u{e000}", "title": "BMP", "projectId": "p",
                "createdAt": "2025-01-14T12:00:00.000Z"
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "id": "\u{10000}", "title": "Supplementary", "projectId": "p",
                "createdAt": "2025-01-14T12:00:00.000999Z"
            }))
            .unwrap(),
        ];
        assert_eq!(latest_thread(&threads, "p", true).unwrap().title, "BMP");
    }
}
