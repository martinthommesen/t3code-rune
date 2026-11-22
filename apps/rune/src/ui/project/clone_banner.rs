use rune::color::{hex, oklch};
use rune::{El, Icon, TextStyle, el, icon, row, text};

use crate::connection::{ProjectClone, ProjectCloneAction, ProjectClonePhase, ProjectCloneStage};

use super::super::{Palette, State, Workspace};

const TEXT: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const LABEL: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);

static DOWNLOAD: Icon = Icon {
    name: "project-clone-download",
    view: 24.0,
    body: r#"<path d="M12 3v12m-5-5 5 5 5-5m4 5v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

impl Workspace {
    pub(in crate::ui) fn pending_clone(&self, project_id: &str) -> Option<&ProjectClone> {
        self.clones
            .iter()
            .find(|clone| clone.project_id == project_id && clone.phase != ProjectClonePhase::Done)
    }
}

impl State {
    fn change_clone(&mut self, project_id: &str, action: ProjectCloneAction) {
        let Some(workspace) = self.workspace.as_ref().filter(|workspace| {
            can_change_clone(workspace, action)
                && workspace
                    .selection
                    .as_ref()
                    .is_some_and(|project| project.id == project_id)
        }) else {
            return;
        };
        let Some(clone) = workspace.pending_clone(project_id) else {
            return;
        };
        let matches_state = matches!(
            (clone.phase, action),
            (ProjectClonePhase::Running, ProjectCloneAction::Cancel)
                | (
                    ProjectClonePhase::Failed | ProjectClonePhase::Cancelled,
                    ProjectCloneAction::Retry | ProjectCloneAction::Remove
                )
        );
        if matches_state && let Some(connection) = &self.connection {
            connection.project_clone_action(project_id.to_owned(), action);
        }
    }
}

fn can_change_clone(workspace: &Workspace, action: ProjectCloneAction) -> bool {
    !workspace.reconnecting
        && match action {
            ProjectCloneAction::Remove => workspace.project_writes,
            _ => workspace.clone_writes,
        }
}

pub(in crate::ui) fn send_block_reason(clone: &ProjectClone) -> &'static str {
    match clone.phase {
        ProjectClonePhase::Running => "Cloning repository",
        _ => "Repository not cloned",
    }
}

pub(in crate::ui) fn view(workspace: &Workspace, clone: &ProjectClone, palette: Palette) -> El {
    let palette = Palette {
        ink: if palette.dark {
            oklch(0.97, 0.0, 0.0)
        } else {
            oklch(0.21, 0.006, 285.885)
        },
        muted: if palette.dark {
            oklch(0.708, 0.0, 0.0)
        } else {
            oklch(0.552, 0.016, 285.938)
        },
        ..palette
    };
    let name = workspace
        .shell
        .as_ref()
        .and_then(|shell| {
            shell
                .projects
                .iter()
                .find(|project| project.id == clone.project_id)
        })
        .map_or_else(
            || "repository",
            |project| {
                project
                    .path
                    .trim_end_matches(['/', '\\'])
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&project.name)
            },
        );
    let (title, detail, action, action_key, action_label, role) = match clone.phase {
        ProjectClonePhase::Running => (
            format!("Cloning {name}"),
            Some(progress(clone)),
            ProjectCloneAction::Cancel,
            "cancel-project-clone",
            "Cancel",
            "status",
        ),
        ProjectClonePhase::Cancelled => (
            format!("Cancelled cloning {name}"),
            Some("Retry to bring in the repository.".into()),
            ProjectCloneAction::Retry,
            "retry-project-clone",
            "Retry",
            "alert",
        ),
        ProjectClonePhase::Failed => (
            format!("Failed to clone {name}"),
            clone.error.clone(),
            ProjectCloneAction::Retry,
            "retry-project-clone",
            "Retry",
            "alert",
        ),
        ProjectClonePhase::Done => unreachable!("only incomplete checkouts have a banner"),
    };
    let surface = if palette.dark { "#0e0e0e" } else { "#ffffff" };
    let background = hex(surface).expect("static clone palette");
    let (background, outline) = match clone.phase {
        ProjectClonePhase::Failed => (
            background.mix(palette.error, 0.08),
            background.mix(palette.error, 0.32),
        ),
        ProjectClonePhase::Cancelled => {
            let colors = if palette.dark {
                ["#21190d", "#5f3d09"]
            } else {
                ["#fff7eb", "#ffdeaa"]
            };
            let [background, outline] =
                colors.map(|color| hex(color).expect("static clone warning palette"));
            (background, outline)
        }
        _ => (background, palette.line),
    };
    let mut content = row()
        .min_w(0.0)
        .grow(1.0)
        .gap(4.0)
        .center_y()
        .child(text(title).font(LABEL).color(palette.ink).truncate());
    if let Some(detail) = detail {
        content = content.child(
            text(&detail)
                .font(TEXT)
                .color(palette.muted)
                .help(detail)
                .min_w(0.0)
                .grow(1.0)
                .truncate(),
        );
    }
    let mut banner = row()
        .key("project-clone-banner")
        .w_full()
        .h(34.0)
        .px(4.0)
        .gap(4.0)
        .center_y()
        .radius(16.0)
        .role(role)
        .fill(background)
        .border(1.0, outline)
        .child(
            el().size(24.0, 24.0)
                .center()
                .child(icon(&DOWNLOAD, 14.0).color(palette.muted).a11y_hidden()),
        )
        .child(content);
    if matches!(
        clone.phase,
        ProjectClonePhase::Cancelled | ProjectClonePhase::Failed
    ) {
        banner = banner.child(
            action_button(
                workspace,
                &clone.project_id,
                ProjectCloneAction::Remove,
                "remove-project-clone",
                "Remove project",
                palette,
            )
            .radius(8.0),
        );
    }
    banner.child(action_button(
        workspace,
        &clone.project_id,
        action,
        action_key,
        action_label,
        palette,
    ))
}

fn action_button(
    workspace: &Workspace,
    project_id: &str,
    action: ProjectCloneAction,
    key: &str,
    label: &str,
    palette: Palette,
) -> El {
    let enabled = can_change_clone(workspace, action);
    let project_id = project_id.to_owned();
    el().key(key)
        .h(24.0)
        .px(8.0)
        .center()
        .corners([8.0, 12.0, 12.0, 8.0])
        .role("button")
        .label(label)
        .hover(palette.selected)
        .focusable(enabled)
        .disabled(!enabled)
        .focus_ring(1.0, 1.0, palette.accent)
        .on_press(move |state: &mut State| state.change_clone(&project_id, action))
        .child(text(label).font(LABEL).color(palette.ink))
}

fn progress(clone: &ProjectClone) -> String {
    let mut text = match clone.stage {
        ProjectCloneStage::Connecting => "Connecting",
        ProjectCloneStage::Counting => "Counting objects",
        ProjectCloneStage::Receiving => "Receiving objects",
        ProjectCloneStage::Resolving => "Resolving deltas",
        ProjectCloneStage::Checkout => "Checking out files",
    }
    .to_owned();
    if let Some(percent) = clone.percent {
        text.push_str(&format!(" · {percent}%"));
    }
    let detail = if clone.stalled {
        Some("Stalled")
    } else {
        clone.detail.as_deref()
    };
    if let Some(detail) = detail {
        text.push_str(" · ");
        text.push_str(detail);
    }
    text
}
