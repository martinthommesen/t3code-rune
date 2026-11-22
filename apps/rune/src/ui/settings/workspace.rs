use rune::{
    Align, Color, El, Icon, Key, KeyEvent, ScrollSpec, Shadow, Side, TextStyle, Tx, col, el, icon,
    row, text,
};
use uuid::Uuid;

use crate::connection::{WorkspaceMode, WorkspaceSetting};

use super::{CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, error_text};

mod scope;

const WIDTH: f32 = 160.0;
const TRIGGER: &str = "default-workspace";
const LIST: &str = "default-workspace-options";
const ITEM: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static CHEVRON: Icon = Icon {
    name: "default-workspace-chevron",
    view: 24.0,
    body: r#"<path d="m6 9 6 6 6-6" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static LAYERS: Icon = Icon {
    name: "workspace-inheritance",
    view: 24.0,
    body: r#"<path d="m12 2 10 5-10 5L2 7l10-5zm-10 10 10 5 10-5m-20 5 10 5 10-5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(super) struct Panel {
    pub(super) current: Option<WorkspaceSetting>,
    pub(super) highlighted: Option<WorkspaceMode>,
    scope: scope::Scope,
    pending: Option<Uuid>,
    error: Option<String>,
}

pub(super) struct OriginView<'a> {
    pub(super) enabled: bool,
    pub(super) may_reset: bool,
    pub(super) writable: bool,
    pub(super) managed_by: Option<&'a str>,
}

impl Panel {
    pub(super) fn settle(&mut self, operation: Uuid, result: Result<(), String>) {
        if self.pending == Some(operation) {
            self.pending = None;
            self.error = result.err();
        }
    }

    pub(super) fn disconnect(&mut self) {
        self.current = None;
        self.close_popups();
    }

    pub(super) fn stop(&mut self) {
        self.disconnect();
        self.scope = scope::Scope::default();
        self.pending = None;
        self.error = None;
    }

    pub(super) fn close_popups(&mut self) {
        self.highlighted = None;
        self.scope.close();
    }

    pub(super) fn apply_project_file(&mut self, operation: Uuid, mode: Option<WorkspaceMode>) {
        self.scope.apply_file(operation, mode);
    }

    pub(super) fn refresh_project_file(state: &mut State) {
        scope::Scope::reload(state);
    }

    fn configured(&self) -> Option<WorkspaceMode> {
        let setting = self.current.as_ref()?;
        match &self.scope.id {
            Some(project) => setting.projects.get(project).copied(),
            None => setting.configured,
        }
    }

    fn scoped_writable(&self, state: &State, writable: bool) -> bool {
        writable
            && state
                .workspace
                .as_ref()
                .is_some_and(|workspace| !workspace.reconnecting)
            && self
                .scope
                .id
                .as_deref()
                .is_none_or(|id| scope::project_is_available(state, id))
    }

    fn writable(&self, state: &State) -> bool {
        self.scoped_writable(
            state,
            self.current
                .as_ref()
                .is_some_and(|setting| setting.writable),
        )
    }

    pub(super) fn project_selected(&self) -> bool {
        self.scope.id.is_some()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn origin_view(&self, state: &State) -> Option<OriginView<'_>> {
        let origin = &self.current.as_ref()?.origin;
        let project = self
            .scope
            .id
            .as_ref()
            .and_then(|id| origin.projects.get(id))
            .copied();
        Some(OriginView {
            enabled: project.unwrap_or(origin.enabled),
            may_reset: if self.project_selected() {
                project.is_some()
            } else {
                !origin.enabled
            },
            writable: self.scoped_writable(state, origin.writable),
            managed_by: origin.managed_by.as_deref(),
        })
    }
    fn effective(&self) -> Option<WorkspaceMode> {
        let setting = self.current.as_ref()?;
        Some(
            self.configured()
                .or(setting.configured)
                .or(self.scope.file_mode)
                .unwrap_or_default(),
        )
    }
}

pub(super) fn inheritance_marker(label: String, palette: Palette) -> El {
    el().size(20.0, 20.0)
        .center()
        .help(label.clone())
        .role("img")
        .label(label)
        .child(icon(&LAYERS, 12.0).color(palette.muted).a11y_hidden())
}

pub(super) fn scope_header(state: &State, palette: Palette) -> El {
    scope::view(state, palette)
}

pub(super) fn save_origin(state: &mut State, enabled: Option<bool>) {
    let panel = &state.settings.workspace;
    if panel.pending.is_some()
        || !panel
            .origin_view(state)
            .is_some_and(|origin| origin.writable)
    {
        return;
    }
    let project = panel.scope.id.clone();
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_worktree_origin(project, enabled));
    start_write(&mut state.settings.workspace, operation);
}

fn start_write(panel: &mut Panel, operation: Option<Uuid>) {
    panel.pending = operation;
    panel.error = operation
        .is_none()
        .then(|| "The environment connection is unavailable.".into());
}

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let settings = &state.settings.workspace;
    let project_scope = settings.scope.id.is_some();
    let writable = settings.writable(state);
    let expanded = settings.highlighted.is_some();
    let value = settings.effective().map_or("Unavailable", label);
    let mut trigger = row()
        .key(TRIGGER)
        .role("combobox")
        .label("Default workspace")
        .a11y_value(value)
        .expanded(expanded)
        .disabled(!writable)
        .focusable(writable)
        .focus_ring(2.0, 1.0, palette.accent)
        .size(WIDTH, 28.0)
        .min_w(WIDTH)
        .px(9.0)
        .gap(6.0)
        .items_center()
        .radius(8.0)
        .fill(surface(palette, "#fcfcfc", "#171717"))
        .border(1.0, surface(palette, "#d4d4d8", "#313131"))
        .opacity(if writable { 1.0 } else { 0.64 })
        .child(
            text(value)
                .font(ITEM)
                .color(palette.ink)
                .grow(1.0)
                .truncate(),
        )
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden());
    if let Some(mode) = settings.highlighted {
        trigger = trigger.active_descendant(option_key(mode));
    }
    if writable {
        trigger = trigger
            .on_press_tx(open)
            .on_key(|state: &mut State, event, tx| {
                if !matches!(event.key, Key::Up | Key::Down) {
                    return false;
                }
                open(state, tx);
                true
            });
    } else if let Some(organization) = settings
        .current
        .as_ref()
        .and_then(|setting| setting.managed_by.as_ref())
    {
        trigger = trigger.help(format!("Managed by {organization}."));
    } else if settings
        .current
        .as_ref()
        .is_some_and(|setting| !setting.writable)
    {
        trigger = trigger.help("This connection lacks permission to change environment settings.");
    }

    let mut title = row()
        .items_center()
        .gap(6.0)
        .child(text("Workspace").font(CONTROL).color(palette.ink));
    if project_scope {
        let inherited = if settings.configured().is_some() {
            "Overridden for this project".to_owned()
        } else if settings
            .current
            .as_ref()
            .is_some_and(|setting| setting.configured.is_some())
        {
            state
                .workspace
                .as_ref()
                .map_or("Inherited from environment".to_owned(), |workspace| {
                    format!("Inherited from {}", workspace.label)
                })
        } else if settings.scope.file_mode.is_some() {
            "Inherited from the repository’s t3.json".to_owned()
        } else {
            "Built-in default".to_owned()
        };
        title = title.child(inheritance_marker(inherited, palette));
    }
    if settings.configured().is_some() && (!project_scope || writable) {
        let reset = super::icon_button(
            "reset-default-workspace",
            if project_scope {
                "Reset Workspace to default"
            } else {
                "Reset default workspace to default"
            },
            &RESET,
            12.0,
            20.0,
            palette,
            |state, tx| choose(state, None, tx),
        );
        let reset = if project_scope {
            reset.help("Reset to inherited value")
        } else {
            reset
        };
        title = title.child(reset.disabled(!writable).focusable(writable));
    }
    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text(if project_scope {
            "Where new threads in this project start."
        } else {
            "Where new threads start. Projects and their t3.json can override it."
        })
        .font(DESCRIPTION)
        .color(palette.muted)
        .wrap(),
    );
    let layout = if width >= 560.0 {
        row().items_center().gap(32.0).child(label).child(trigger)
    } else {
        col().gap(12.0).child(label).child(trigger)
    };
    let mut section = col()
        .key("settings-new-threads")
        .w_full()
        .max_w(800.0)
        .gap(10.0)
        .child(
            text("New threads")
                .font(SECTION)
                .color(palette.ink)
                .role("heading")
                .px(16.0),
        )
        .child(
            layout
                .key("new-threads-workspace")
                .w_full()
                .px(16.0)
                .py(12.0)
                .radius(12.0)
                .border(1.0, palette.line)
                .border_layout(),
        );
    if let Some(error) = &settings.error {
        section = section.child(error_text(error, palette));
    }
    col()
        .key("settings-threads-content")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .padding(30.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .max_w(800.0)
                .gap(22.0)
                .child(scope::view(state, palette))
                .child(section),
        )
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    if let Some(scope) = scope::overlay(state, palette) {
        return Some(scope);
    }
    let settings = &state.settings.workspace;
    let active = settings.highlighted.filter(|_| settings.writable(state))?;
    let outside = el()
        .key("default-workspace-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    let panel = col()
        .key(LIST)
        .role("listbox")
        .label("Default workspace")
        .focusable(true)
        .active_descendant(option_key(active))
        .anchored_fit(TRIGGER, Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .w(WIDTH)
        .padding(4.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(surface(palette, "#ffffff", "#111111").alpha(if palette.dark { 0.84 } else { 0.97 }))
        .backdrop_blur(16.0)
        .box_shadow(Shadow {
            x: 0.0,
            y: 16.0,
            blur: 40.0,
            spread: -18.0,
            color: Color::BLACK.alpha(if palette.dark { 0.80 } else { 0.55 }),
            inset: false,
        })
        .on_key(navigate)
        .children(
            [WorkspaceMode::Local, WorkspaceMode::Worktree]
                .into_iter()
                .map(|mode| {
                    let selected = settings.effective() == Some(mode);
                    let fill = if mode == active || selected {
                        palette.ink.alpha(0.08)
                    } else {
                        Color::TRANSPARENT
                    };
                    row()
                        .key(option_key(mode))
                        .role("option")
                        .label(label(mode))
                        .focus_selected(selected)
                        .w_full()
                        .h(28.0)
                        .radius(4.0)
                        .px(8.0)
                        .items_center()
                        .fill(fill)
                        .on_hover(move |state: &mut State| {
                            state.settings.workspace.highlighted = Some(mode)
                        })
                        .on_press_tx(move |state, tx| choose(state, Some(mode), tx))
                        .child(text(label(mode)).font(ITEM).color(palette.ink))
                }),
        );
    Some([outside, panel])
}

fn surface(palette: Palette, light: &str, dark: &str) -> Color {
    rune::color::hex(if palette.dark { dark } else { light }).expect("static settings palette")
}

fn label(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Local => "Current checkout",
        WorkspaceMode::Worktree => "New worktree",
    }
}

fn option_key(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Local => "workspace-current-checkout",
        WorkspaceMode::Worktree => "workspace-new-worktree",
    }
}

fn open(state: &mut State, tx: &mut Tx) {
    if state.settings.workspace.writable(state) && state.settings.workspace.pending.is_none() {
        state.settings.workspace.highlighted = state.settings.workspace.effective();
        tx.focus_when_shown(LIST, tx.focus_visible());
    }
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.workspace.highlighted = None;
    tx.return_focus(TRIGGER, tx.focus_visible());
}

fn choose(state: &mut State, mode: Option<WorkspaceMode>, tx: &mut Tx) {
    close(state, tx);
    let settings = &state.settings.workspace;
    if !settings.writable(state)
        || settings.pending.is_some()
        || mode.is_some() && mode == settings.effective()
    {
        return;
    }
    let project_id = settings.scope.id.clone();
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_workspace_mode(project_id, mode));
    start_write(&mut state.settings.workspace, operation);
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(current) = state.settings.workspace.highlighted else {
        return false;
    };
    match event.key {
        Key::Escape => close(state, tx),
        Key::Tab => {
            close(state, tx);
            return false;
        }
        Key::Enter => choose(state, Some(current), tx),
        Key::Text(ref value) if value == " " => choose(state, Some(current), tx),
        Key::Home => state.settings.workspace.highlighted = Some(WorkspaceMode::Local),
        Key::End => state.settings.workspace.highlighted = Some(WorkspaceMode::Worktree),
        Key::Up | Key::Down => {
            state.settings.workspace.highlighted = Some(match current {
                WorkspaceMode::Local => WorkspaceMode::Worktree,
                WorkspaceMode::Worktree => WorkspaceMode::Local,
            });
        }
        _ => return false,
    }
    true
}
