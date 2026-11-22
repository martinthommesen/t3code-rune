use rune::{
    Align, Color, El, Icon, Key, KeyEvent, ScrollSpec, Shadow, Side, TextStyle, Tx, col, el, icon,
    row, text,
};
use uuid::Uuid;

use crate::connection::{WorkspaceMode, WorkspaceSetting};

use super::{CONTROL, DESCRIPTION, Palette, RESET, SECTION, State, error_text};

const WIDTH: f32 = 160.0;
const TRIGGER: &str = "default-workspace";
const LIST: &str = "default-workspace-options";
const ITEM: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static CHEVRON: Icon = Icon {
    name: "default-workspace-chevron",
    view: 24.0,
    body: r#"<path d="m6 9 6 6 6-6" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(super) struct Panel {
    pub(super) current: Option<WorkspaceSetting>,
    pub(super) highlighted: Option<WorkspaceMode>,
    pending: Option<Uuid>,
    error: Option<String>,
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
        self.highlighted = None;
    }

    pub(super) fn stop(&mut self) {
        self.disconnect();
        self.pending = None;
        self.error = None;
    }

    fn writable(&self, state: &State) -> bool {
        self.current
            .as_ref()
            .is_some_and(|setting| setting.writable)
            && state
                .workspace
                .as_ref()
                .is_some_and(|workspace| !workspace.reconnecting)
    }

    fn effective(&self) -> Option<WorkspaceMode> {
        self.current
            .as_ref()
            .map(|setting| setting.configured.unwrap_or_default())
    }
}

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let settings = &state.settings.workspace;
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
        .gap(8.0)
        .child(text("Workspace").font(CONTROL).color(palette.ink));
    if settings
        .current
        .as_ref()
        .is_some_and(|setting| setting.configured.is_some())
    {
        let reset = super::icon_button(
            "reset-default-workspace",
            "Reset default workspace to default",
            &RESET,
            12.0,
            20.0,
            palette,
            |state, tx| choose(state, None, tx),
        );
        title = title.child(reset.disabled(!writable).focusable(writable));
    }
    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text("Where new threads start. Projects and their t3.json can override it.")
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
        .child(section)
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
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
    state.settings.workspace.error = None;
    state.settings.workspace.pending = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_workspace_mode(mode));
    if state.settings.workspace.pending.is_none() {
        state.settings.workspace.error = Some("The environment connection is unavailable.".into());
    }
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
