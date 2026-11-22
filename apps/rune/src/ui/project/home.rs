use rune::color::{hex, oklch};
use rune::{Color, El, Icon, TextStyle, Tx, col, icon, row, text};
use uuid::Uuid;

use super::ProjectDestination;
use crate::ui::{Destination, Palette, Project, State, Workspace, conversation::Layout};
use crate::workspace_path::comparison_path;

pub(in crate::ui) struct ScratchRequest {
    pub(super) command_id: String,
    pub(super) destination: ProjectDestination,
}

impl Workspace {
    fn scratch_draft_available(&self) -> bool {
        self.scratch_projects && self.project_writes && self.launch_writes && !self.reconnecting
    }

    pub(in crate::ui) fn is_scratch(&self, project: &Project) -> bool {
        if let Some(root) = &self.scratch_workspace_root {
            let root = comparison_path(root);
            !root.is_empty() && root == comparison_path(&project.path)
        } else {
            self.scratch_project.as_deref() == Some(project.id.as_str())
        }
    }
}

static PLUS: Icon = Icon {
    name: "empty-home-add-project",
    view: 24.0,
    body: r#"<path d="M5 12h14m-7-7v14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
};

static SCRATCH: Icon = Icon {
    name: "empty-home-start-scratch",
    view: 24.0,
    body: r#"<path d="M4 12v-2m0-4a2 2 0 0 1 2-2m4 0h2m4 0h2a2 2 0 0 1 2 2m0 4v2m0 4a2 2 0 0 1-2 2h-2m-4 0-3-2m-4 1-1 3v-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(in crate::ui) fn empty_home(
    state: &State,
    workspace: &Workspace,
    layout: Layout,
    palette: Palette,
) -> El {
    let scratch = workspace.scratch_projects && workspace.launch_writes;
    let compact = layout.viewport[0] < 640.0;
    let heading = if compact { 24.0 } else { 30.0 };
    let compact_button_height = if compact { 32.0 } else { 28.0 };
    let copy = if scratch {
        "Add a project, or start without one."
    } else {
        "Add a project to start your first thread."
    };
    let mut actions = row().center().gap(8.0).mt(20.0);
    if workspace.project_writes {
        let blue = if palette.dark {
            oklch(0.571, 0.21, 264.0)
        } else {
            oklch(0.488, 0.217, 264.0)
        };
        actions = actions.child(action(
            "home-add-project",
            "Add project",
            &PLUS,
            compact_button_height,
            [blue, blue, Color::WHITE],
            !workspace.reconnecting,
            State::show_add_project,
        ));
    }
    if scratch {
        let pending = state.scratch_request.is_some();
        let surface = if palette.dark {
            Color::WHITE.alpha(0.0352)
        } else {
            Color::WHITE
        };
        let border = if palette.dark {
            Color::WHITE.alpha(0.11)
        } else {
            hex("#d4d4d8").expect("static secondary button border")
        };
        actions = actions.child(action(
            "home-start-scratch",
            if pending {
                "Starting…"
            } else {
                "Start without a project"
            },
            &SCRATCH,
            compact_button_height,
            [surface, border, palette.ink],
            !workspace.reconnecting && !pending,
            State::start_scratch_project,
        ));
    }
    let heading_color =
        hex(if palette.dark { "#f5f5f5" } else { "#262626" }).expect("static heading color");
    let subtitle_color = if palette.dark {
        oklch(0.708, 0.0, 0.0)
    } else {
        oklch(0.552, 0.016, 285.938)
    };
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pt(layout.header_height)
        .center()
        .child(
            col()
                .w_full()
                .max_w(512.0)
                .px(32.0)
                .items_center()
                .gap(4.0)
                .child(
                    text("What should we work on?")
                        .font(TextStyle::new(heading, 600.0).lh(heading + 6.0))
                        .color(heading_color)
                        .role("heading"),
                )
                .child(
                    text(copy)
                        .font(TextStyle::new(14.0, 400.0).lh(20.0))
                        .color(subtitle_color),
                )
                .child(actions),
        )
}

fn action(
    key: &'static str,
    label: &'static str,
    symbol: &'static Icon,
    height: f32,
    [background, border, ink]: [Color; 3],
    enabled: bool,
    press: fn(&mut State, &mut Tx),
) -> El {
    let button = row()
        .key(key)
        .label(label)
        .role("button")
        .h(height)
        .px(8.0)
        .center_y()
        .gap(6.0)
        .radius(8.0)
        .fill(background)
        .border(1.0, border)
        .child(icon(symbol, 16.0).color(ink).a11y_hidden())
        .child(
            text(label)
                .font(TextStyle::new(14.0, 500.0).lh(20.0))
                .color(ink),
        );
    if enabled {
        button
            .focusable(true)
            .focus_ring(2.0, -1.0, ink)
            .hover(background.mix(ink, 0.08))
            .on_press_tx(press)
    } else {
        button.disabled(true).opacity(0.64)
    }
}

impl State {
    pub(in crate::ui) fn start_scratch_project(&mut self, _: &mut Tx) {
        self.request_scratch_project(false);
    }

    pub(in crate::ui) fn open_fresh_scratch_draft(&mut self) {
        self.request_scratch_project(true);
    }

    pub(in crate::ui) fn can_open_scratch_draft(&self) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(Workspace::scratch_draft_available)
    }

    fn request_scratch_project(&mut self, new_thread: bool) {
        if self.scratch_request.is_some() {
            return;
        }
        let Some(workspace) = self
            .workspace
            .as_ref()
            .filter(|workspace| workspace.scratch_draft_available())
        else {
            return;
        };
        let destination = if new_thread {
            let Some(origin) = self.history_destination() else {
                return;
            };
            ProjectDestination::NewThread(origin)
        } else {
            match workspace.destination() {
                Destination::Home => ProjectDestination::Draft,
                Destination::NewThread(project) => ProjectDestination::MoveDraft(project),
                _ => return,
            }
        };
        let Some(connection) = &self.connection else {
            return;
        };
        let command_id = Uuid::new_v4().to_string();
        connection.start_scratch_project(command_id.clone());
        self.project_notice = None;
        self.scratch_request = Some(ScratchRequest {
            command_id,
            destination,
        });
    }
}
