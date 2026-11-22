use std::time::{Duration, Instant};

use rune::{Color, El, Icon, TextEdit, Tx, col, icon, row, text, text_input};
use uuid::Uuid;

use crate::connection::AddProjectBaseDirectory;

use super::super::{
    CLOSE, CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, icon_button, path_input,
};
use super::{ITEM, inheritance, start_write};

pub(super) const INPUT: &str = "add-project-base-directory";
pub(super) const RESET_INPUT: &str = "reset-add-project-base-directory";
const ROW: &str = "add-project-base-setting";
const PROJECT_SCOPE_WARNING: &str =
    "This setting is environment-wide and cannot be overridden by a project.";
const WARNING_DURATION: Duration = Duration::from_secs(5);
static WARNING: Icon = Icon {
    name: "triangle-alert",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.73 18 13.73 4a2 2 0 0 0-3.46 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3ZM12 9v4m0 4h.01"/></g>"#,
};

#[derive(Default)]
pub(in crate::ui::settings) struct Panel {
    saved: Option<String>,
    draft: TextEdit,
    previous: String,
    edited: bool,
    pending: Option<Uuid>,
    warning_until: Option<Instant>,
}

impl Panel {
    pub(in crate::ui::settings) fn apply(&mut self, setting: Option<&AddProjectBaseDirectory>) {
        let value = setting.map(|setting| setting.path.as_str());
        if self.saved.as_deref() == value {
            return;
        }
        self.saved = value.map(str::to_owned);
        if value.is_none() {
            self.pending = None;
            self.edited = false;
        }
        if !self.edited {
            self.replace(self.saved.clone().unwrap_or_default());
        }
    }

    pub(in crate::ui::settings) fn settle(&mut self, operation: Uuid, success: bool) {
        if self.pending == Some(operation) {
            self.pending = None;
            if !success && !self.edited {
                self.replace(self.saved.clone().unwrap_or_default());
            }
        }
    }

    fn replace(&mut self, value: String) {
        self.draft.replace_text(&value);
        self.previous = value;
        self.edited = false;
    }

    fn change(&mut self) {
        let next = path_input::normalize(&self.previous, &self.draft.text);
        if next != self.draft.text {
            self.draft.replace_text(&next);
        }
        self.previous = next;
        self.edited = true;
    }
}

fn current(state: &State) -> Option<&AddProjectBaseDirectory> {
    state
        .settings
        .workspace
        .current
        .as_ref()?
        .add_project_base
        .as_ref()
}

fn environment_writable(state: &State) -> bool {
    state.settings.workspace.scoped_writable(
        state,
        current(state).is_some_and(|setting| setting.writable),
    )
}

fn writable(state: &State) -> bool {
    !state.settings.workspace.project_selected() && environment_writable(state)
}

pub(super) fn focus_control(state: &State, tx: &mut Tx) {
    tx.focus(if writable(state) { INPUT } else { ROW }, true);
}

pub(super) fn resettable(state: &State) -> bool {
    environment_writable(state) && current(state).is_some_and(|value| !value.path.is_empty())
}

fn commit(state: &mut State) -> bool {
    if !writable(state) || state.settings.workspace.writes_pending() {
        return false;
    }
    let field = &mut state.settings.workspace.project_start;
    if !field.edited {
        return false;
    }
    field.edited = false;
    let path = field
        .draft
        .text
        .trim_matches(path_input::whitespace)
        .to_owned();
    field.replace(path.clone());
    if field.saved.as_deref() != Some(path.as_str()) {
        let operation = state
            .connection
            .as_ref()
            .and_then(|connection| connection.update_add_project_base_directory(path));
        let field = &mut state.settings.workspace.project_start;
        field.pending = operation;
        if operation.is_none() {
            field.replace(field.saved.clone().unwrap_or_default());
        }
        start_write(&mut state.settings.workspace, operation);
    }
    true
}

pub(in crate::ui) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let field = &mut state.settings.workspace.project_start;
    let mut changed = false;
    if let Some(until) = field.warning_until {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            field.warning_until = None;
            changed = true;
        } else {
            tx.wake_after(remaining);
        }
    }
    if tx.focused() != Some(INPUT) && (tx.focused() != Some(RESET_INPUT) || tx.focus_visible()) {
        changed |= commit(state);
    }
    changed
}

fn reset(state: &mut State, tx: &mut Tx) {
    if !resettable(state) {
        return;
    }
    tx.focus(ROW, tx.focus_visible());
    let panel = &mut state.settings.workspace;
    if panel.project_selected() {
        panel.project_start.warning_until = Some(Instant::now() + WARNING_DURATION);
        tx.wake_after(WARNING_DURATION);
        return;
    }
    panel.project_start.replace(String::new());
    panel.project_start.edited = true;
    commit(state);
}

pub(in crate::ui) fn warning(state: &State, palette: Palette, width: f32) -> Option<El> {
    state.settings.workspace.project_start.warning_until?;
    let inset = if width >= 640.0 { 32.0 } else { 16.0 };
    let width = (width - 2.0 * inset).clamp(0.0, 360.0);
    let amber = rune::color::hex("#f59e0b").expect("static warning color");
    Some(
        col()
            .key("add-project-base-scope-warning")
            .role("status")
            .pin(None, Some(inset + 40.0), Some(inset), None)
            .w(width)
            .px(14.0)
            .py(11.0)
            .gap(2.0)
            .radius(8.0)
            .fill(palette.field)
            .border(1.0, palette.line)
            .border_layout()
            .shadow(12.0, 18.0, Color::BLACK.alpha(0.30))
            .child(
                row()
                    .items_center()
                    .gap(8.0)
                    .child(icon(&WARNING, 16.0).color(amber).a11y_hidden())
                    .child(text("Setting not saved").font(CONTROL).color(palette.ink)),
            )
            .child(
                text(PROJECT_SCOPE_WARNING)
                    .font(ITEM)
                    .color(palette.muted)
                    .wrap(),
            )
            .child(
                icon_button(
                    "dismiss-add-project-base-warning",
                    "Dismiss notification",
                    &CLOSE,
                    12.0,
                    22.0,
                    palette,
                    |state, _| state.settings.workspace.project_start.warning_until = None,
                )
                .pin(None, Some(-5.0), Some(-5.0), None)
                .radius(11.0)
                .fill(palette.field)
                .border(1.0, palette.line),
            ),
    )
}

pub(super) fn section(state: &State, palette: Palette, width: f32) -> Option<El> {
    let setting = current(state)?;
    let field = &state.settings.workspace.project_start;
    let write = writable(state);
    let mut input = text_input(&field.draft, ITEM)
        .key(INPUT)
        .label("Add project base directory")
        .placeholder("~/", palette.muted)
        .single_line()
        .color(palette.ink)
        .h(28.0)
        .px(10.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .focus_ring(1.0, 1.0, palette.accent);
    if write {
        input = input
            .on_edit(|state: &mut State| &mut state.settings.workspace.project_start.draft)
            .on_input_change(|state: &mut State, _| state.settings.workspace.project_start.change())
            .on_submit(|state: &mut State, tx| {
                commit(state);
                tx.blur();
            });
    } else {
        let help = if state.settings.workspace.project_selected() {
            "Environment-wide setting. Select an environment to change it.".into()
        } else if let Some(organization) = setting.managed_by.as_deref() {
            format!("Managed by {organization}.")
        } else {
            "This connection lacks permission to change environment settings.".into()
        };
        input = input.disabled(true).read_only(true).opacity(0.5).help(help);
    }
    let mut title = row()
        .items_center()
        .gap(6.0)
        .child(
            text("Add project starts in")
                .font(CONTROL)
                .color(palette.ink),
        )
        .child(inheritance::marker(
            state,
            inheritance::Setting::ProjectBase,
            palette,
        ));
    if resettable(state) {
        title = title.child(
            super::super::archive::press(
                icon_button(
                    RESET_INPUT,
                    "Reset add project base directory to default",
                    &RESET,
                    12.0,
                    20.0,
                    palette,
                    reset,
                ),
                reset,
            )
            .help("Reset to default"),
        );
    }
    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text("Leave empty to use \"~/\" when the Add Project browser opens.")
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let content = if width >= 560.0 {
        row()
            .items_center()
            .gap(32.0)
            .child(label)
            .child(input.w(288.0))
    } else {
        col().gap(12.0).child(label).child(input.w_full())
    };
    Some(
        col()
            .key("settings-new-projects")
            .w_full()
            .max_w(800.0)
            .gap(10.0)
            .child(
                text("New projects")
                    .role("heading")
                    .font(SECTION)
                    .color(palette.ink)
                    .px(16.0),
            )
            .child(
                content
                    .key(ROW)
                    .w_full()
                    .role("group")
                    .label("Add project starts in")
                    .focusable(true)
                    .tab_index(-1)
                    .no_focus_ring()
                    .hover_group()
                    .px(16.0)
                    .py(12.0)
                    .radius(12.0)
                    .border(1.0, palette.line)
                    .border_layout(),
            ),
    )
}
