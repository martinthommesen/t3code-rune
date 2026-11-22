use rune::{
    Align, Color, El, Icon, Key, ScrollSpec, Shadow, Side, TextStyle, Tx, col, el, icon, row, text,
};
use t3_rune::providers::codex::RuntimeMode;

use crate::connection::{Project, ResetWorkspaceOverride, SubmoduleMode, WorkspaceSetting};
use crate::snooze;

use super::super::archive::press;
use super::{DESCRIPTION, ITEM, LAYERS, Palette, State, scope, start_write, surface};

const POPUP: &str = "thread-setting-inheritance-popup";
const RESET: &str = "reset-thread-setting-project-overrides";
const OPTIONS_COUNT: [&str; 9] = [
    "0 items", "1 item", "2 items", "3 items", "4 items", "5 items", "6 items", "7 items",
    "8 items",
];
const CHECK: Icon = Icon {
    name: "thread-setting-inheritance-check",
    view: 24.0,
    body: r#"<path d="m20 6-11 11-5-5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Setting {
    Permissions,
    Workspace,
    Submodules,
    Snooze,
    ProjectBase,
}

impl Setting {
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Permissions => "default-permissions-inheritance",
            Self::Workspace => "default-workspace-inheritance",
            Self::Submodules => "worktree-submodules-inheritance",
            Self::Snooze => "snooze-options-inheritance",
            Self::ProjectBase => "add-project-base-inheritance",
        }
    }

    fn control(self) -> &'static str {
        match self {
            Self::Permissions => "default-permissions",
            Self::Workspace => "default-workspace",
            Self::Submodules => "worktree-submodules",
            Self::Snooze => super::snooze::ADD,
            Self::ProjectBase => super::project_start::INPUT,
        }
    }

    fn reset(self) -> &'static str {
        match self {
            Self::Permissions => "reset-default-permissions",
            Self::Workspace => "reset-default-workspace",
            Self::Submodules => "reset-worktree-submodules",
            Self::Snooze => "reset-snooze-options",
            Self::ProjectBase => super::project_start::RESET_INPUT,
        }
    }

    fn project_value(self, settings: &WorkspaceSetting, id: &str) -> Option<&'static str> {
        match self {
            Self::Permissions => settings
                .permission
                .projects
                .get(id)
                .copied()
                .map(permission),
            Self::Workspace => settings.projects.get(id).copied().map(super::label),
            Self::Submodules => settings
                .submodules
                .projects
                .get(id)
                .copied()
                .map(SubmoduleMode::label),
            Self::Snooze | Self::ProjectBase => None,
        }
    }

    fn environment_value(self, settings: &WorkspaceSetting) -> Option<&str> {
        match self {
            Self::Permissions => {
                let mode = settings.permission.environment;
                (mode != RuntimeMode::FullAccess).then(|| permission(mode))
            }
            Self::Workspace => settings.configured.map(super::label),
            Self::Submodules => settings.submodules.environment.map(SubmoduleMode::label),
            Self::Snooze => settings
                .snooze
                .as_ref()
                .filter(|setting| setting.presets != snooze::defaults())
                .map(|setting| OPTIONS_COUNT[setting.presets.len()]),
            Self::ProjectBase => settings
                .add_project_base
                .as_ref()
                .map(|setting| setting.path.as_str())
                .filter(|path| !path.is_empty()),
        }
    }

    fn default(self) -> &'static str {
        match self {
            Self::Permissions => permission(RuntimeMode::FullAccess),
            Self::Workspace => super::label(crate::connection::WorkspaceMode::Local),
            Self::Submodules => SubmoduleMode::Recursive.label(),
            Self::Snooze => OPTIONS_COUNT[snooze::defaults().len()],
            Self::ProjectBase => "Empty",
        }
    }
}

fn permission(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "approval-required",
        RuntimeMode::AutoAcceptEdits => "auto-accept-edits",
        RuntimeMode::Auto => "auto",
        RuntimeMode::FullAccess => "full-access",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Project,
    Environment,
    Repository,
    BuiltIn,
}

struct Layer<'a> {
    source: Source,
    label: &'static str,
    value: Option<&'a str>,
}

struct Inheritance<'a> {
    machine: &'a str,
    layers: Vec<Layer<'a>>,
    overrides: Vec<(&'a Project, &'static str)>,
    may_clear: bool,
}

impl Inheritance<'_> {
    fn winner(&self) -> Option<Source> {
        self.layers
            .iter()
            .find(|layer| layer.value.is_some())
            .map(|layer| layer.source)
    }

    fn summary(&self) -> String {
        let project = self
            .layers
            .first()
            .is_some_and(|layer| layer.source == Source::Project);
        let summary = match self.winner() {
            Some(Source::Project) => "Overridden for this project".into(),
            Some(Source::Repository) => "Inherited from the repository's t3.json".into(),
            _ if project => format!("Inherited from {}", self.machine),
            Some(Source::Environment) => "Set on the environment".into(),
            _ => "Built-in default".into(),
        };
        match self.overrides.len() {
            0 => summary,
            1 => format!("{summary} · 1 project override"),
            count => format!("{summary} · {count} project overrides"),
        }
    }
}

fn resolve_inheritance(state: &State, setting: Setting) -> Option<Inheritance<'_>> {
    let panel = &state.settings.workspace;
    let current = panel.current.as_ref()?;
    if setting == Setting::Snooze && (panel.project_selected() || current.snooze.is_none()) {
        return None;
    }
    if setting == Setting::ProjectBase && current.add_project_base.is_none() {
        return None;
    }
    let machine = state.workspace.as_ref()?.label.as_str();
    let environment = setting.environment_value(current);
    let project = panel
        .scope
        .id
        .as_deref()
        .filter(|_| {
            matches!(
                setting,
                Setting::Permissions | Setting::Workspace | Setting::Submodules
            )
        })
        .map(|id| setting.project_value(current, id));
    let mut layers = Vec::with_capacity(4);
    if let Some(value) = project {
        layers.push(Layer {
            source: Source::Project,
            label: "Project",
            value,
        });
    }
    layers.push(Layer {
        source: Source::Environment,
        label: "Environment",
        value: environment,
    });
    if matches!(setting, Setting::Workspace | Setting::Submodules) && project.is_some() {
        layers.push(Layer {
            source: Source::Repository,
            label: "t3.json",
            value: if project.flatten().is_none() && environment.is_none() {
                match setting {
                    Setting::Workspace => panel.scope.file.mode.map(super::label),
                    Setting::Submodules => panel.scope.file.submodules.map(SubmoduleMode::label),
                    _ => None,
                }
            } else {
                None
            },
        });
    }
    layers.push(Layer {
        source: Source::BuiltIn,
        label: "Default",
        value: Some(setting.default()),
    });
    let overrides = if project.is_some() {
        Vec::new()
    } else {
        current
            .override_order
            .iter()
            .filter_map(|id| {
                let value = setting.project_value(current, id)?;
                let project = scope::projects(state).iter().find(|p| &p.id == id)?;
                Some((project, value))
            })
            .collect()
    };
    let writable = match setting {
        Setting::Permissions => current.permission.writable,
        Setting::Workspace => current.writable,
        Setting::Submodules => current.submodules.writable,
        Setting::Snooze => current
            .snooze
            .as_ref()
            .is_some_and(|setting| setting.writable),
        Setting::ProjectBase => current
            .add_project_base
            .as_ref()
            .is_some_and(|setting| setting.writable),
    };
    Some(Inheritance {
        machine,
        layers,
        overrides,
        may_clear: panel.pending.is_none() && panel.scoped_writable(state, writable),
    })
}

pub(super) fn marker(state: &State, setting: Setting, palette: Palette) -> El {
    let frame = el().size(20.0, 20.0).center().radius(4.0);
    let Some(details) = resolve_inheritance(state, setting) else {
        return frame.a11y_hidden();
    };
    let active = state.settings.workspace.inheritance == Some(setting);
    let summary = details.summary();
    let color = if details.winner() == Some(Source::Project) {
        palette.accent
    } else {
        palette.muted
    };
    let frame = frame
        .key(setting.key())
        .role("button")
        .label(format!("{summary}. Show where this value comes from"))
        .help(summary)
        .expanded(active)
        .focusable(true)
        .focus_ring(2.0, 1.0, palette.accent)
        .hover(palette.ink.alpha(0.08));
    let frame = press(frame, move |state: &mut State, tx| {
        if state.settings.workspace.inheritance == Some(setting) {
            close(state, setting, tx);
            return;
        }
        let Some(details) = resolve_inheritance(state, setting) else {
            return;
        };
        let initial = initial_focus(&details);
        state.settings.workspace.close_popups();
        state.settings.workspace.inheritance = Some(setting);
        tx.focus_when_shown(initial, tx.focus_visible());
    })
    .child(icon(&LAYERS, 12.0).color(color).a11y_hidden());
    let project = details
        .layers
        .iter()
        .any(|layer| layer.source == Source::Project);
    if !active && !project && details.overrides.is_empty() {
        frame.group_opacity(0.0, 1.0)
    } else {
        frame
    }
}

fn project_key(project: &Project) -> String {
    format!("thread-setting-override-{}", project.id)
}

fn initial_focus(details: &Inheritance<'_>) -> String {
    if details.overrides.is_empty() {
        POPUP.into()
    } else if details.may_clear {
        RESET.into()
    } else {
        project_key(details.overrides[0].0)
    }
}

pub(super) fn overlay(state: &State, palette: Palette) -> Option<[El; 2]> {
    let setting = state.settings.workspace.inheritance?;
    let details = resolve_inheritance(state, setting)?;
    let winner = details.winner();
    let outside = el()
        .key("thread-setting-inheritance-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(move |state, tx| close(state, setting, tx));
    let layers = col()
        .role("list")
        .children(details.layers.iter().map(|layer| {
            let effective = winner == Some(layer.source);
            let color = if effective {
                palette.ink
            } else {
                palette.muted
            };
            let value_color = if layer.value.is_some() {
                color
            } else {
                color.alpha(0.60)
            };
            row()
                .role("listitem")
                .items_center()
                .justify_between()
                .gap(12.0)
                .h(28.0)
                .px(8.0)
                .radius(6.0)
                .fill(if effective {
                    palette.ink.alpha(0.06)
                } else {
                    Color::TRANSPARENT
                })
                .child(
                    text(layer.label)
                        .font(if effective { super::CONTROL } else { ITEM })
                        .color(color),
                )
                .child(
                    row()
                        .gap(6.0)
                        .items_center()
                        .child(
                            text(layer.value.unwrap_or("Inherits"))
                                .font(ITEM)
                                .color(value_color)
                                .max_w(128.0)
                                .truncate(),
                        )
                        .child(if effective {
                            icon(&CHECK, 14.0).color(palette.accent).a11y_hidden()
                        } else {
                            el().size(14.0, 14.0).a11y_hidden()
                        }),
                )
        }));
    let mut content = col()
        .w_full()
        .px(12.0)
        .py(10.0)
        .child(
            text(details.machine)
                .role("heading")
                .font(TextStyle::new(12.0, 500.0).lh(16.0))
                .color(palette.muted)
                .pb(6.0),
        )
        .child(layers);
    if !details.overrides.is_empty() {
        content = content.child(override_list(setting, &details, palette));
    }
    let popup = col()
        .key(POPUP)
        .role("dialog")
        .label("Setting inheritance")
        .focusable(true)
        .no_focus_ring()
        .anchored_fit(setting.key(), Side::Bottom, Align::Start, 4.0)
        .anchored_round()
        .w(320.0)
        .max_h(360.0)
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
        .on_key(move |state, event, tx| match event.key {
            Key::Escape => {
                close(state, setting, tx);
                true
            }
            Key::Tab => tab(state, setting, event.mods.shift, tx),
            _ => false,
        })
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(content);
    Some([outside, popup])
}

fn override_list(setting: Setting, details: &Inheritance<'_>, palette: Palette) -> El {
    let mut header = row()
        .h(16.0)
        .px(8.0)
        .items_center()
        .justify_between()
        .child(text("Overridden by").font(DESCRIPTION).color(palette.muted));
    if details.may_clear {
        let label = if details.overrides.len() == 1 {
            "Reset it"
        } else {
            "Reset all"
        };
        let button = el()
            .key(RESET)
            .role("button")
            .label(label)
            .focusable(true)
            .focus_ring(2.0, 1.0, palette.accent)
            .child(text(label).font(DESCRIPTION).color(palette.ink));
        header = header.child(press(button, move |state, tx| clear(state, setting, tx)));
    }
    let projects = col()
        .role("list")
        .children(details.overrides.iter().map(|(project, value)| {
            let id = project.id.clone();
            let link = el()
                .key(project_key(project))
                .role("button")
                .label(&project.name)
                .focusable(true)
                .focus_ring(2.0, 1.0, palette.accent)
                .grow(1.0)
                .min_w(0.0)
                .child(text(&project.name).font(ITEM).color(palette.ink).truncate());
            row()
                .role("listitem")
                .h(28.0)
                .px(8.0)
                .items_center()
                .gap(12.0)
                .child(press(link, move |state: &mut State, tx| {
                    state.settings.workspace.inheritance = None;
                    scope::choose(state, Some(id.clone()), tx);
                }))
                .child(
                    text(*value)
                        .font(ITEM)
                        .color(palette.muted)
                        .max_w(128.0)
                        .truncate(),
                )
        }));
    col()
        .mt(8.0)
        .pt(8.0)
        .gap(2.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
        .border_layout()
        .child(header)
        .child(projects)
}

fn close(state: &mut State, setting: Setting, tx: &mut Tx) {
    state.settings.workspace.inheritance = None;
    tx.return_focus(setting.key(), tx.focus_visible());
}

fn tab(state: &mut State, setting: Setting, backwards: bool, tx: &mut Tx) -> bool {
    let Some(details) = resolve_inheritance(state, setting) else {
        return false;
    };
    let last = details
        .overrides
        .last()
        .map(|(project, _)| project_key(project));
    let current = tx.focused();
    let first = initial_focus(&details);
    let exit = backwards && current == Some(first.as_str())
        || !backwards && (last.is_none() || current == last.as_deref());
    if !exit {
        return false;
    }
    let panel = &state.settings.workspace;
    let target = if backwards {
        setting.key()
    } else if match setting {
        Setting::Permissions => super::permissions::resettable(state),
        Setting::Workspace => panel.configured().is_some() && panel.writable(state),
        Setting::Submodules => panel
            .submodules_view(state)
            .is_some_and(|setting| setting.writable && setting.may_reset),
        Setting::Snooze => super::snooze::can_reset(state),
        Setting::ProjectBase => super::project_start::resettable(state),
    } {
        setting.reset()
    } else {
        setting.control()
    };
    close(state, setting, tx);
    if target == super::snooze::ADD {
        super::snooze::focus_editor(state, tx);
    } else if target == super::project_start::INPUT {
        super::project_start::focus_control(state, tx);
    } else {
        tx.focus(target, true);
    }
    true
}

fn clear(state: &mut State, setting: Setting, tx: &mut Tx) {
    let Some(details) = resolve_inheritance(state, setting).filter(|details| details.may_clear)
    else {
        return;
    };
    let projects = details
        .overrides
        .iter()
        .map(|(project, _)| project.id.clone())
        .collect();
    let setting = match setting {
        Setting::Permissions => ResetWorkspaceOverride::Permissions,
        Setting::Workspace => ResetWorkspaceOverride::Mode,
        Setting::Submodules => ResetWorkspaceOverride::WorktreeSubmodules,
        Setting::Snooze | Setting::ProjectBase => return,
    };
    let operation = state
        .connection
        .as_ref()
        .and_then(|connection| connection.reset_workspace_overrides(projects, setting));
    start_write(&mut state.settings.workspace, operation);
    tx.focus(POPUP, tx.focus_visible());
}
