use rune::color::oklch;
use rune::{El, Key, Tx, col, row, text};

use crate::connection::CloneLayoutSetting;

use super::super::{CONTROL, DESCRIPTION, Palette, RESET, State, icon_button, switch_track};
use super::{inheritance, project_start, start_write};

pub(super) const SWITCH: &str = "clone-into-host-folders";
pub(super) const RESET_SWITCH: &str = "reset-clone-layout";
const ROW: &str = "clone-layout-setting";
const LABEL: &str = "Clone into host folders";

fn current(state: &State) -> Option<&CloneLayoutSetting> {
    state
        .settings
        .workspace
        .current
        .as_ref()?
        .clone_layout
        .as_ref()
}

fn environment_writable(state: &State) -> bool {
    state
        .settings
        .workspace
        .scoped_writable(state, current(state).is_some_and(|value| value.writable))
}

fn writable(state: &State) -> bool {
    !state.settings.workspace.project_selected() && environment_writable(state)
}

pub(super) fn resettable(state: &State) -> bool {
    environment_writable(state) && current(state).is_some_and(|value| value.enabled)
}

pub(super) fn focus_control(state: &State, tx: &mut Tx) {
    tx.focus(if writable(state) { SWITCH } else { ROW }, true);
}

fn save(state: &mut State, enabled: bool) {
    if !writable(state) || state.settings.workspace.writes_pending() {
        return;
    }
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.update_clone_layout(enabled));
    start_write(&mut state.settings.workspace, operation);
}

fn toggle(state: &mut State, _: &mut Tx) {
    if let Some(setting) = current(state) {
        save(state, !setting.enabled);
    }
}

fn reset(state: &mut State, tx: &mut Tx) {
    if !resettable(state) {
        return;
    }
    tx.focus(ROW, tx.focus_visible());
    if state.settings.workspace.project_selected() {
        project_start::warn_project_scope(state, tx);
    } else {
        save(state, false);
    }
}

pub(super) fn setting_row(state: &State, palette: Palette, width: f32) -> Option<El> {
    let setting = current(state)?;
    let can_edit = writable(state);
    let blue = oklch(
        if palette.dark { 0.571 } else { 0.488 },
        if palette.dark { 0.21 } else { 0.217 },
        264.0,
    );
    let mut switch = switch_track(setting.enabled, false, palette, blue)
        .key(SWITCH)
        .label(LABEL)
        .disabled(!can_edit)
        .focusable(can_edit)
        .opacity(if can_edit { 1.0 } else { 0.5 });
    if can_edit {
        switch = switch
            .on_press_tx(toggle)
            .on_key(|state: &mut State, event, tx| {
                if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ")
                {
                    toggle(state, tx);
                    true
                } else {
                    false
                }
            });
    } else {
        let message = if let Some(organization) = &setting.managed_by {
            format!("Managed by {organization}.")
        } else if state.settings.workspace.project_selected() {
            "Environment-wide setting. Select an environment to change it.".into()
        } else {
            "This connection lacks permission to change environment settings.".into()
        };
        switch = switch.help(message);
    }
    let mut title = row()
        .items_center()
        .gap(6.0)
        .child(text(LABEL).font(CONTROL).color(palette.ink))
        .child(inheritance::marker(
            state,
            inheritance::Setting::CloneLayout,
            palette,
        ));
    if resettable(state) {
        title = title.child(
            super::super::archive::press(
                icon_button(
                    RESET_SWITCH,
                    "Reset clone into host folders to default",
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
        text("Clones go to github.com/owner/repo under the folder above, like ghq.")
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let content = if width >= 560.0 {
        row().items_center().gap(32.0).child(label).child(switch)
    } else {
        col().gap(12.0).child(label).child(switch)
    };
    Some(
        content
            .key(ROW)
            .label(LABEL)
            .role("group")
            .focusable(true)
            .tab_index(-1)
            .no_focus_ring()
            .hover_group()
            .w_full()
            .px(16.0)
            .py(12.0)
            .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
            .border_layout(),
    )
}
