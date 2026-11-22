use rune::{
    Align, Color, El, Icon, Key, KeyEvent, ScrollSpec, Shadow, Side, TextStyle, Tx, col, el, icon,
    row, text,
};
use uuid::Uuid;

use crate::connection::{
    BranchNamingMode, BranchTextField, ProjectFileWorkspaceSettings, SubmoduleMode, WorkspaceMode,
    WorkspaceSetting, WorktreeLocation,
};
use t3_rune::providers::codex::RuntimeMode;

use super::{CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, error_text};

mod clone_layout;
mod inheritance;
pub(super) mod keyboard;
mod permissions;
pub(super) mod project_start;
mod scope;
pub(super) mod snooze;

const WIDTH: f32 = 160.0;
const TRIGGER: &str = "default-workspace";
const OPTIONS: [WorkspaceMode; 2] = [WorkspaceMode::Local, WorkspaceMode::Worktree];
const LIST: &str = "default-workspace-options";
pub(super) const ITEM: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

pub(super) static CHEVRON: Icon = Icon {
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
    permission_highlighted: Option<RuntimeMode>,
    inheritance: Option<inheritance::Setting>,
    pub(super) branch_naming_highlighted: Option<BranchNamingMode>,
    pub(super) branch_naming_typeahead: keyboard::Typeahead,
    workspace_typeahead: keyboard::Typeahead,
    permission_typeahead: keyboard::Typeahead,
    scope: scope::Scope,
    pub(super) snooze: snooze::Panel,
    pub(super) project_start: project_start::Panel,
    pub(super) submodules: super::worktrees::SubmodulesPanel,
    pending: Option<Uuid>,
    error: Option<String>,
}

pub(super) struct BranchTextView<'a> {
    pub(super) value: &'a str,
    pub(super) project_id: Option<&'a str>,
    pub(super) reset_value: Option<&'a str>,
    pub(super) writable: bool,
    pub(super) managed_by: Option<&'a str>,
}

pub(super) struct BranchNamingView<'a> {
    pub(super) mode: BranchNamingMode,
    pub(super) may_reset: bool,
    pub(super) writable: bool,
    pub(super) managed_by: Option<&'a str>,
}

pub(super) struct SubmodulesView<'a> {
    pub(super) mode: SubmoduleMode,
    pub(super) may_reset: bool,
    pub(super) writable: bool,
    pub(super) managed_by: Option<&'a str>,
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
        self.snooze.apply(None);
        self.project_start.apply(None);
        self.close_popups();
    }

    pub(super) fn stop(&mut self) {
        self.disconnect();
        self.scope = scope::Scope::default();
        self.submodules.collapse();
        self.pending = None;
        self.error = None;
    }

    pub(super) fn close_popups(&mut self) {
        self.highlighted = None;
        self.permission_highlighted = None;
        self.inheritance = None;
        self.branch_naming_highlighted = None;
        self.branch_naming_typeahead.clear();
        self.workspace_typeahead.clear();
        self.permission_typeahead.clear();
        self.scope.close();
        self.snooze.close_menu();
        self.submodules.dismiss();
    }

    pub(super) fn apply_project_file(
        &mut self,
        operation: Uuid,
        settings: ProjectFileWorkspaceSettings,
    ) {
        self.scope.apply_file(operation, settings);
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

    pub(super) fn branch_naming_view(&self, state: &State) -> Option<BranchNamingView<'_>> {
        let settings = &self.current.as_ref()?.branch_naming;
        let project_mode = self
            .scope
            .id
            .as_ref()
            .and_then(|id| settings.projects.get(id))
            .copied();
        Some(BranchNamingView {
            mode: project_mode.unwrap_or(settings.configured),
            may_reset: if self.project_selected() {
                project_mode.is_some()
            } else {
                settings.configured != BranchNamingMode::Static
            },
            writable: self.scoped_writable(state, settings.writable),
            managed_by: settings.managed_by.as_deref(),
        })
    }

    pub(super) fn branch_text_view(
        &self,
        state: &State,
        field: BranchTextField,
    ) -> Option<BranchTextView<'_>> {
        let setting = &self.current.as_ref()?.branch_naming.text[field.index()];
        let project_id = self.scope.id.as_deref();
        let project = project_id
            .and_then(|id| setting.projects.get(id))
            .map(String::as_str);
        let environment = setting.environment.as_str();
        Some(BranchTextView {
            value: project.unwrap_or(environment),
            project_id,
            reset_value: if project_id.is_some() {
                project.map(|_| environment)
            } else {
                (environment != field.default_value()).then_some(field.default_value())
            },
            writable: self.pending.is_none() && self.scoped_writable(state, setting.writable),
            managed_by: setting.managed_by.as_deref(),
        })
    }

    pub(super) fn writes_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn submodules_view(&self, state: &State) -> Option<SubmodulesView<'_>> {
        let setting = &self.current.as_ref()?.submodules;
        let project_mode = self
            .scope
            .id
            .as_ref()
            .and_then(|id| setting.projects.get(id))
            .copied();
        let configured = project_mode.or(setting.environment);
        Some(SubmodulesView {
            mode: configured
                .or(self.scope.file.submodules)
                .unwrap_or_default(),
            may_reset: if self.project_selected() {
                project_mode.is_some()
            } else {
                setting.environment.is_some()
            },
            writable: self.scoped_writable(state, setting.writable),
            managed_by: setting.managed_by.as_deref(),
        })
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
    pub(super) fn worktree_location(&self) -> Option<&WorktreeLocation> {
        if self.project_selected() {
            return None;
        }
        self.current.as_ref()?.worktree_location.as_ref()
    }

    pub(super) fn can_edit_worktree_location(&self, state: &State) -> bool {
        self.pending.is_none()
            && self
                .worktree_location()
                .is_some_and(|setting| self.scoped_writable(state, setting.writable))
    }

    fn effective(&self) -> Option<WorkspaceMode> {
        let setting = self.current.as_ref()?;
        Some(
            self.configured()
                .or(setting.configured)
                .or(self.scope.file.mode)
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

pub(super) fn submodules_inheritance_marker(state: &State, palette: Palette) -> El {
    inheritance::marker(state, inheritance::Setting::Submodules, palette)
}

pub(super) fn scope_header(state: &State, palette: Palette) -> El {
    scope::view(state, palette)
}

pub(super) fn save_branch_text(
    state: &mut State,
    field: BranchTextField,
    value: Option<String>,
) -> Option<Uuid> {
    let panel = &state.settings.workspace;
    let setting = panel.branch_text_view(state, field)?;
    if !setting.writable {
        return None;
    }
    let project = setting.project_id.map(str::to_owned);
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_branch_naming_text(project, field, value));
    start_write(&mut state.settings.workspace, operation);
    operation
}

pub(super) fn save_branch_naming(state: &mut State, mode: Option<BranchNamingMode>) {
    let panel = &state.settings.workspace;
    if panel.pending.is_some()
        || !panel
            .branch_naming_view(state)
            .is_some_and(|settings| settings.writable)
    {
        return;
    }
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_branch_naming_mode(panel.scope.id.clone(), mode));
    start_write(&mut state.settings.workspace, operation);
}

pub(super) fn save_submodules(state: &mut State, mode: Option<SubmoduleMode>) {
    let panel = &state.settings.workspace;
    if panel.pending.is_some()
        || !panel
            .submodules_view(state)
            .is_some_and(|setting| setting.writable)
    {
        return;
    }
    let project = panel.scope.id.clone();
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_worktree_submodules(project, mode));
    start_write(&mut state.settings.workspace, operation);
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

pub(super) fn save_worktree_location(state: &mut State, path: String) -> Option<Uuid> {
    let panel = &state.settings.workspace;
    if !panel.can_edit_worktree_location(state)
        || panel
            .worktree_location()
            .is_some_and(|setting| setting.path == path)
    {
        return None;
    }
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_worktree_location(path));
    start_write(&mut state.settings.workspace, operation);
    operation
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
        trigger = trigger.on_press_tx(open).on_key(trigger_input);
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
        .child(text("Workspace").font(CONTROL).color(palette.ink))
        .child(inheritance::marker(
            state,
            inheritance::Setting::Workspace,
            palette,
        ));
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
        .focusable(true)
        .tab_index(-1)
        .no_focus_ring()
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
            col()
                .w_full()
                .radius(12.0)
                .border(1.0, palette.line)
                .border_layout()
                .child(permissions::setting_row(state, palette, width))
                .child(
                    layout
                        .key("new-threads-workspace")
                        .hover_group()
                        .w_full()
                        .px(16.0)
                        .py(12.0)
                        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
                        .border_layout(),
                ),
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
                .children(project_start::section(state, palette, width))
                .child(section)
                .children(snooze::section(state, palette, width)),
        )
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    if let Some(popup) = inheritance::overlay(state, palette) {
        return Some(popup);
    }
    if let Some(options) = snooze::overlay(state, palette) {
        return Some(options);
    }
    if let Some(scope) = scope::overlay(state, palette) {
        return Some(scope);
    }
    if let Some(permissions) = permissions::overlay(state, palette) {
        return Some(permissions);
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
        .children(OPTIONS.into_iter().map(|mode| {
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
        }));
    Some([outside, panel])
}

pub(super) fn surface(palette: Palette, light: &str, dark: &str) -> Color {
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
        state.settings.workspace.close_popups();
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

fn trigger_input(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if matches!(event.key, Key::Up | Key::Down)
        || event.key == Key::Enter && event.mods == rune::Mods::default()
    {
        if !tx.key_repeat() {
            open(state, tx);
        }
        return true;
    }
    if event.key == Key::Tab {
        state.settings.workspace.workspace_typeahead.clear();
    }
    typeahead(state, event, tx)
}

fn typeahead(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let settings = &mut state.settings.workspace;
    if matches!(&event.key, Key::Text(value) if value == " ")
        && !settings.workspace_typeahead.typing()
    {
        return false;
    }
    let selected = settings.effective().unwrap_or_default();
    let index = OPTIONS
        .iter()
        .position(|&mode| mode == selected)
        .unwrap_or_default();
    let options = OPTIONS.map(|mode| Some(label(mode)));
    match settings.workspace_typeahead.input(event, index, &options) {
        keyboard::Input::Other => false,
        keyboard::Input::Typed(mode) => {
            if let Some(index) = mode {
                if settings.highlighted.is_some() {
                    settings.highlighted = Some(OPTIONS[index]);
                } else {
                    choose(state, Some(OPTIONS[index]), tx);
                }
            }
            true
        }
    }
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(active) = state.settings.workspace.highlighted else {
        return false;
    };
    if typeahead(state, event, tx) {
        return true;
    }
    match event.key {
        Key::Escape => close(state, tx),
        Key::Tab => {
            let panel = &state.settings.workspace;
            let target = if !event.mods.shift {
                if !panel.project_selected() && snooze::setting(state).is_some() {
                    inheritance::Setting::Snooze.key()
                } else {
                    "close-client-settings"
                }
            } else if panel.configured().is_some() {
                "reset-default-workspace"
            } else {
                inheritance::Setting::Workspace.key()
            };
            state.settings.workspace.highlighted = None;
            state.settings.workspace.workspace_typeahead.clear();
            tx.focus(target, true);
        }
        Key::Enter => choose(state, Some(active), tx),
        Key::Text(ref value) if value == " " => choose(state, Some(active), tx),
        _ => {
            let index = OPTIONS
                .iter()
                .position(|&mode| mode == active)
                .unwrap_or_default();
            let Some(next) = keyboard::arrow_index(&event.key, index, OPTIONS.len()) else {
                return false;
            };
            state.settings.workspace.highlighted = Some(OPTIONS[next]);
        }
    }
    true
}
