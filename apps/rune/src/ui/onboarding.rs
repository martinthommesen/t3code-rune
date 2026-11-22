use std::time::Duration;

use rune::color::oklch;
use rune::{Color, El, Icon, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};

use crate::client_settings::ClientSettings;
use crate::connection::{ProviderMetadata, ProviderReadiness};

use super::{State, Workspace, sidebar::provider_badge};

const TITLE: TextStyle = TextStyle::new(24.0, 600.0).lh(32.0).tracking(-0.6);
const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const CONTROL: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const SMALL: TextStyle = TextStyle::new(12.0, 400.0).lh(19.5);
const START: &str = "onboarding-start";
const SKIP: &str = "onboarding-start-without-agent";
const DISCOVERY_TIMEOUT: f64 = 4.0;

static CHECK: Icon = Icon {
    name: "onboarding-check",
    view: 24.0,
    body: r#"<path d="m20 6-11 11-5-5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    App,
    Pending,
    Welcome,
    SettingsUnavailable,
}

#[derive(Default)]
pub(super) struct Onboarding {
    phase: Phase,
    since: Option<f64>,
    stalled: bool,
    save_when_confirmed: bool,
    save_failed: bool,
    details: Option<String>,
    refreshing: bool,
}

impl Onboarding {
    pub(super) fn is_active(&self) -> bool {
        self.phase != Phase::App
    }

    pub(super) fn new(settings: &ClientSettings) -> Self {
        Self {
            phase: match settings.onboarding_completed() {
                Ok(true) => Phase::App,
                Ok(false) => Phase::Pending,
                Err(_) => Phase::SettingsUnavailable,
            },
            ..Self::default()
        }
    }

    pub(super) fn poll(state: &mut State, clock: f64, tx: &mut Tx) -> bool {
        let onboarding = &mut state.onboarding;
        let shell = state
            .workspace
            .as_ref()
            .filter(|workspace| !workspace.reconnecting)
            .and_then(|workspace| workspace.shell.as_ref());
        let catalog_available = state
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.providers.is_some());
        let mut changed = false;
        if onboarding.phase == Phase::Pending {
            if let Some(shell) = shell {
                let populated = !shell.projects.is_empty() || !shell.threads.is_empty();
                let many_items = shell.projects.len() > 1 || shell.threads.len() > 1;
                if catalog_available || many_items {
                    onboarding.phase = if populated {
                        Phase::App
                    } else {
                        Phase::Welcome
                    };
                    onboarding.save_when_confirmed = populated;
                    changed = true;
                }
            }
            if onboarding.phase == Phase::Pending && !onboarding.stalled {
                let started = *onboarding.since.get_or_insert(clock);
                let remaining = DISCOVERY_TIMEOUT - (clock - started);
                if remaining <= 0.0 {
                    onboarding.stalled = true;
                    changed = true;
                } else {
                    tx.wake_after(Duration::from_secs_f64(remaining));
                }
            }
        }
        if onboarding.save_when_confirmed && catalog_available && shell.is_some() {
            let _ = state.client_settings.complete_onboarding();
            onboarding.save_when_confirmed = false;
        }
        changed
    }

    pub(super) fn finish_refresh(&mut self) {
        self.refreshing = false;
    }

    fn finish(state: &mut State, tx: &mut Tx) {
        if state.onboarding.phase != Phase::Welcome {
            return;
        }
        if state.client_settings.complete_onboarding().is_ok() {
            state.onboarding.phase = Phase::App;
            state.onboarding.save_failed = false;
            tx.blur();
        } else {
            state.onboarding.save_failed = true;
        }
    }

    fn retry_settings(state: &mut State, _: &mut Tx) {
        state.onboarding = Self::new(&state.client_settings);
    }

    fn refresh_agents(state: &mut State, _: &mut Tx) {
        if let Some(connection) = &state.connection {
            state.onboarding.refreshing = true;
            state.onboarding.since = None;
            state.onboarding.stalled = false;
            connection.refresh_provider_catalog();
        } else {
            state.connect();
        }
    }

    fn toggle_details(&mut self, instance: &str) {
        self.details = if self.details.as_deref() == Some(instance) {
            None
        } else {
            Some(instance.to_owned())
        };
    }
}

#[derive(Clone, Copy)]
struct Colors {
    background: Color,
    text: Color,
    muted: Color,
    edge: Color,
    control_edge: Color,
    hover: Color,
    control_fill: Color,
    blue: Color,
    success: Color,
    dark: bool,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let text = if dark {
            oklch(0.970, 0.0, 0.0)
        } else {
            oklch(0.210, 0.006, 285.885)
        };
        Self {
            background: if dark {
                Color::BLACK
            } else {
                oklch(0.992, 0.0, 0.0)
            },
            text,
            muted: if dark {
                oklch(0.708, 0.0, 0.0)
            } else {
                oklch(0.552, 0.016, 285.938)
            },
            edge: text.alpha(if dark { 0.048 } else { 0.10 }),
            control_edge: text.alpha(if dark { 0.15 } else { 0.14 }),
            control_fill: text.alpha(if dark { 0.0352 } else { 0.020 }),
            hover: text.alpha(0.09),
            blue: oklch(if dark { 0.571 } else { 0.541 }, 0.21, 264.0),
            success: oklch(if dark { 0.765 } else { 0.527 }, 0.177, 163.223),
            dark,
        }
    }
}

pub(super) fn view(
    state: &State,
    dark: bool,
    width: f32,
    height: f32,
    titlebar: f32,
) -> Option<El> {
    let onboarding = &state.onboarding;
    let colors = Colors::new(dark);
    let panel = match onboarding.phase {
        Phase::App => return None,
        Phase::Pending if state.connection_error.is_some() => return None,
        Phase::Pending if onboarding.stalled => recovery(
            "Still connecting",
            "T3 Code could not confirm this workspace.",
            "Retry",
            Onboarding::refresh_agents,
            colors,
        ),
        Phase::Pending => el().w_full().h_full().fill(colors.background),
        Phase::SettingsUnavailable => recovery(
            "Could not read settings",
            "Your saved settings could not be loaded.",
            "Retry",
            Onboarding::retry_settings,
            colors,
        ),
        Phase::Welcome => welcome(state, colors, width, height, titlebar),
    };
    Some(panel)
}

fn recovery(
    title: &str,
    description: &str,
    action: &str,
    press: fn(&mut State, &mut Tx),
    colors: Colors,
) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .fill(colors.background)
        .child(
            col()
                .items_center()
                .gap(8.0)
                .child(
                    text(title)
                        .font(TextStyle::new(18.0, 600.0).lh(24.0))
                        .color(colors.text)
                        .role("heading"),
                )
                .child(text(description).font(BODY).color(colors.muted))
                .child(
                    button(
                        "onboarding-retry",
                        action,
                        action,
                        false,
                        true,
                        press,
                        colors,
                    )
                    .mt(12.0),
                ),
        )
}

fn welcome(state: &State, colors: Colors, width: f32, height: f32, titlebar: f32) -> El {
    let Some(workspace) = &state.workspace else {
        return el().w_full().h_full().fill(colors.background);
    };
    let mut providers: Vec<_> = workspace
        .providers
        .as_ref()
        .into_iter()
        .flat_map(|providers| providers.values())
        .collect();
    providers.sort_unstable_by(|left, right| {
        left.readiness()
            .cmp(&right.readiness())
            .then_with(|| {
                left.display_name
                    .as_deref()
                    .unwrap_or(&left.instance_id)
                    .cmp(right.display_name.as_deref().unwrap_or(&right.instance_id))
            })
            .then_with(|| left.instance_id.cmp(&right.instance_id))
    });
    let ready = providers
        .iter()
        .any(|provider| provider.readiness() == ProviderReadiness::Ready);
    let wide = width >= 640.0;
    let footer = completion(state, ready, wide, colors);
    let agents = agents(state, workspace, &providers, wide, colors).mt(40.0);
    let label = row()
        .w_full()
        .mt(6.0)
        .center_y()
        .child(text("Setting up ").font(BODY).color(colors.muted))
        .child(
            text(format!("{}.", workspace.label))
                .font(BODY)
                .color(colors.text)
                .truncate(),
        );
    let content = col()
        .w_full()
        .max_w(768.0)
        .min_h((height - titlebar).max(0.0))
        .px(24.0)
        .pt(if wide { 80.0 } else { 48.0 })
        .pb(32.0)
        .child(
            text("Welcome to T3 Code")
                .key("onboarding-title")
                .font(TITLE)
                .color(colors.text)
                .role("heading"),
        )
        .child(label)
        .child(agents)
        .child(footer)
        .child(el().grow(1.0).min_h(48.0))
        .child(
            el().key("onboarding-privacy-policy")
                .role("link")
                .label("Privacy policy")
                .focusable(true)
                .focus_ring(1.0, 2.0, colors.blue)
                .on_press_tx(|_: &mut State, tx| {
                    let _ = tx.open_url("https://t3.codes/privacy-policy");
                })
                .child(
                    text("Privacy policy")
                        .font(TextStyle::new(11.0, 400.0).lh(17.875))
                        .color(colors.muted.alpha(0.70)),
                ),
        );
    col()
        .key("onboarding-page")
        .role("main")
        .label("Welcome")
        .w_full()
        .h_full()
        .fill(colors.background)
        .child(el().w_full().h(titlebar).window_drag())
        .child(
            col()
                .key("onboarding-scroll")
                .w_full()
                .grow(1.0)
                .items_center()
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(content),
        )
}

fn agents(
    state: &State,
    workspace: &Workspace,
    providers: &[&ProviderMetadata],
    wide: bool,
    colors: Colors,
) -> El {
    let mut list = col().grow(1.0).min_w(0.0);
    for provider in providers {
        list = list.child(agent_row(provider, state, colors));
    }
    if providers.is_empty() {
        list = list.child(
            text("No agents available")
                .font(BODY.lh(40.0))
                .color(colors.muted),
        );
    }
    let checking = state.onboarding.refreshing;
    let refresh_label = if checking {
        "Checking…"
    } else {
        "Check for agents"
    };
    list = list.child(
        el().key("onboarding-refresh-agents")
            .role("button")
            .label(refresh_label)
            .w_full()
            .h(40.0)
            .focusable(!checking && !workspace.reconnecting)
            .disabled(checking || workspace.reconnecting)
            .focus_ring(1.0, -1.0, colors.blue)
            .radius(6.0)
            .hover(colors.hover)
            .on_press_tx(Onboarding::refresh_agents)
            .child(
                row()
                    .center_y()
                    .gap(12.0)
                    .child(
                        el().size(20.0, 20.0)
                            .center()
                            .child(text("↻").font(BODY).color(colors.muted)),
                    )
                    .child(text(refresh_label).font(BODY).color(colors.muted)),
            ),
    );
    let section = if wide { row().gap(40.0) } else { col() };
    let title = text("Agents")
        .font(BODY.lh(40.0))
        .color(colors.muted)
        .role("heading");
    let title = if wide { title.w(96.0) } else { title };
    section
        .w_full()
        .py(16.0)
        .border_sides([1.0, 0.0, 1.0, 0.0], colors.edge)
        .border_layout()
        .child(title)
        .child(list)
}

fn agent_row(provider: &ProviderMetadata, state: &State, colors: Colors) -> El {
    let (name, mark) = provider_badge::identity(provider, colors.dark, colors.muted);
    let readiness = provider.readiness();
    let detail = match readiness {
        ProviderReadiness::SignIn => Some("Not authenticated"),
        ProviderReadiness::Attention => Some("Needs attention"),
        ProviderReadiness::Install => Some("Not installed"),
        ProviderReadiness::Disabled => Some("Disabled"),
        ProviderReadiness::Ready | ProviderReadiness::Checking => None,
    };
    let instance = provider.instance_id.clone();
    let mut line = row()
        .w_full()
        .min_h(40.0)
        .py(4.0)
        .gap(12.0)
        .center_y()
        .child(el().size(20.0, 20.0).center().child(mark))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(text(&name).font(CONTROL).color(colors.text).truncate())
                .children(detail.map(|status| text(status).font(SMALL).color(colors.muted))),
        );
    if readiness == ProviderReadiness::Ready {
        return line.child(
            row()
                .center_y()
                .gap(6.0)
                .child(icon(&CHECK, 14.0).color(colors.success).a11y_hidden())
                .child(
                    text("Ready")
                        .font(TextStyle::new(12.0, 500.0).lh(16.0))
                        .color(colors.success),
                ),
        );
    }
    if readiness == ProviderReadiness::Checking {
        return line.child(text("Checking…").font(SMALL).color(colors.muted));
    }
    let open = state.onboarding.details.as_deref() == Some(instance.as_str());
    let key = format!("onboarding-agent-{instance}");
    let label = format!("Set up: {name}");
    line = line.child(
        el().key(key)
            .role("button")
            .label(label)
            .expanded(open)
            .focusable(true)
            .focus_ring(1.0, 2.0, colors.blue)
            .h(24.0)
            .mt(2.0)
            .px(7.0)
            .center()
            .radius(7.0)
            .border(1.0, colors.control_edge)
            .border_layout()
            .fill(colors.control_fill)
            .hover(colors.hover)
            .child(
                text("Set up")
                    .font(TextStyle::new(12.0, 500.0).lh(16.0))
                    .color(colors.text),
            )
            .on_press_tx(move |state: &mut State, _| state.onboarding.toggle_details(&instance)),
    );
    if !open {
        return line;
    }
    let message = provider
        .message
        .as_deref()
        .unwrap_or("Check this agent’s installation and account, then check for agents again.");
    col().w_full().child(line).child(
        col()
            .w_full()
            .mt(4.0)
            .mb(4.0)
            .padding(12.0)
            .gap(8.0)
            .radius(7.0)
            .border(1.0, colors.control_edge)
            .child(text(message).font(SMALL).color(colors.muted).wrap()),
    )
}

fn completion(state: &State, ready: bool, wide: bool, colors: Colors) -> El {
    let prompt = row()
        .gap(12.0)
        .center_y()
        .children((!ready).then(|| {
            text("Set up one agent to start.")
                .font(SMALL)
                .color(colors.muted)
        }))
        .child(button(
            START,
            "Start",
            "Start",
            true,
            ready,
            Onboarding::finish,
            colors,
        ));
    let actions = if wide {
        row().w_full().justify_between().center_y()
    } else {
        col().gap(8.0)
    };
    let actions = actions
        .child(if ready {
            el().size(0.0, 0.0)
        } else {
            button(
                SKIP,
                "Start without an agent",
                "Start without an agent",
                false,
                true,
                Onboarding::finish,
                colors,
            )
        })
        .child(prompt);
    let mut result = col().w_full().mt(24.0).gap(8.0).child(actions);
    if state.onboarding.save_failed {
        result = result.child(
            text("Your settings could not be saved. Try again.")
                .font(SMALL)
                .color(colors.muted)
                .role("status"),
        );
    }
    result
}

fn button(
    key: &str,
    label: &str,
    visible: &str,
    primary: bool,
    enabled: bool,
    press: fn(&mut State, &mut Tx),
    colors: Colors,
) -> El {
    let (fill, ink) = if primary {
        (
            colors.blue.alpha(if enabled { 1.0 } else { 0.60 }),
            Color::WHITE.alpha(if enabled { 1.0 } else { 0.60 }),
        )
    } else {
        (Color::TRANSPARENT, colors.muted)
    };
    el().key(key)
        .role("button")
        .label(label)
        .focusable(enabled)
        .disabled(!enabled)
        .focus_ring(2.0, 1.0, colors.blue)
        .h(32.0)
        .px(12.0)
        .center()
        .radius(9.0)
        .fill(fill)
        .hover(if primary {
            fill.mix(Color::WHITE, 0.10)
        } else {
            colors.hover
        })
        .child(text(visible).font(CONTROL).color(ink))
        .on_press_tx(press)
}
