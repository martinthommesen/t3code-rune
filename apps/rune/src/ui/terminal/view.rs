use rune::color::hex;
use rune::{Color, El, Icon, Key, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};

use super::super::{Palette, State, button};
use super::{
    Pane, TerminalStatus,
    tabs::{MAX_TERMINALS_PER_GROUP, SplitDirection, Tabs},
    viewport,
};

const GROUP_TEXT: TextStyle = TextStyle::new(11.0, 400.0).lh(14.0);
const TAB_TEXT: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const DIALOG_TITLE: TextStyle = TextStyle::new(20.0, 600.0).lh(27.5);
const DIALOG_BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
pub(super) const CANCEL_CLOSE_KEY: &str = "cancel-terminal-close";
const PRIMARY_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Ctrl"
};

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $path, "</g>"),
        }
    };
}

static PLUS: Icon = glyph!("plus", r#"<path d="M5 12h14M12 5v14"/>"#);
static CLOSE: Icon = glyph!("x", r#"<path d="m18 6-12 12M6 6l12 12"/>"#);
static TRASH: Icon = glyph!(
    "trash-2",
    r#"<path d="M3 6h18M19 6v14a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V6m3 0V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2m-6 5v6m4-6v6"/>"#
);
static SQUARE: Icon = glyph!(
    "square",
    r#"<rect width="18" height="18" x="3" y="3" rx="2"/>"#
);
static SPLIT_HORIZONTAL: Icon = glyph!(
    "square-split-horizontal",
    r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M12 3v18"/>"#
);
static SPLIT_VERTICAL: Icon = glyph!(
    "square-split-vertical",
    r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 12h18"/>"#
);
static SQUARE_TERMINAL: Icon = glyph!(
    "square-terminal",
    r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="m7 11 2 2-2 2m5 0h4"/>"#
);

pub(super) fn drawer(
    terminal: &Pane,
    peers: &[Pane],
    tabs: Option<&Tabs>,
    palette: Palette,
    enabled: bool,
) -> El {
    let group = tabs.and_then(Tabs::active_group);
    let direction = group.map_or(SplitDirection::Horizontal, |group| group.direction);
    let ordered: Vec<_> = group.map_or_else(
        || vec![terminal],
        |group| {
            group
                .members
                .iter()
                .filter_map(|id| {
                    std::iter::once(terminal)
                        .chain(peers)
                        .find(|pane| &pane.terminal_id == id)
                })
                .collect()
        },
    );
    let mut drawer = row()
        .key("terminal-drawer")
        .w_full()
        .h(280.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
        .role("region")
        .label(
            if terminal.status == TerminalStatus::Running && terminal.has_running_subprocess {
                format!("Terminal: {}. Process running.", terminal.label)
            } else {
                format!("Terminal: {}", terminal.label)
            },
        )
        .child(pane_group(
            &ordered,
            terminal.attachment,
            direction,
            palette,
        ));
    let border = palette.canvas.mix(palette.line, 0.7);
    let at_limit = ordered.len() >= MAX_TERMINALS_PER_GROUP;
    let (side_label, stack_label) = if at_limit {
        (
            format!("Split Terminal Horizontally (max {MAX_TERMINALS_PER_GROUP} per group)"),
            format!("Split Terminal Vertically (max {MAX_TERMINALS_PER_GROUP} per group)"),
        )
    } else {
        (
            "Split Terminal Horizontally".to_owned(),
            "Split Terminal Vertically".to_owned(),
        )
    };
    let actions = row()
        .h(22.0)
        .child(action(
            "terminal-split-horizontal",
            &side_label,
            (!at_limit).then_some("D"),
            &SPLIT_HORIZONTAL,
            palette,
            enabled && !at_limit,
            |state, tx| state.split_terminal(SplitDirection::Horizontal, tx),
        ))
        .child(el().w(1.0).h_full().fill(border))
        .child(action(
            "terminal-split-vertical",
            &stack_label,
            (!at_limit).then_some("Shift+D"),
            &SPLIT_VERTICAL,
            palette,
            enabled && !at_limit,
            |state, tx| state.split_terminal(SplitDirection::Vertical, tx),
        ))
        .child(el().w(1.0).h_full().fill(border))
        .child(action(
            "terminal-new",
            "New Terminal",
            Some("N"),
            &PLUS,
            palette,
            enabled,
            State::new_terminal_tab,
        ))
        .child(el().w(1.0).h_full().fill(border))
        .child(action(
            "terminal-close",
            "Close Terminal",
            Some("W"),
            &TRASH,
            palette,
            enabled,
            State::request_selected_terminal_close,
        ));
    if let Some(tabs) = tabs.filter(|tabs| tabs.entries.len() > 1) {
        drawer = drawer
            .gap(6.0)
            .child(tab_sidebar(tabs, actions, palette, enabled));
    } else {
        drawer = drawer.child(
            actions
                .fill(palette.canvas)
                .radius(6.0)
                .border(1.0, palette.line)
                .pin(None, Some(8.0), Some(8.0), None),
        );
    }
    drawer
}

fn pane_group(panes: &[&Pane], active: u64, direction: SplitDirection, palette: Palette) -> El {
    let mut surface = match direction {
        SplitDirection::Horizontal => row(),
        SplitDirection::Vertical => col(),
    }
    .key("terminal-split-panes")
    .grow(1.0)
    .min_w(0.0)
    .min_h(0.0)
    .h_full();
    for (index, pane) in panes.iter().enumerate() {
        let selected = pane.attachment == active;
        let child = col()
            .key(format!("terminal-surface-{}", pane.attachment))
            .grow(1.0)
            .min_w(0.0)
            .min_h(0.0)
            .child(viewport(pane, selected, palette));
        let (child, edges) = match direction {
            SplitDirection::Horizontal => (child.w(0.0).h_full(), [0.0, 0.0, 0.0, 1.0]),
            SplitDirection::Vertical => (child.w_full().h(0.0), [1.0, 0.0, 0.0, 0.0]),
        };
        surface = surface.child(if index == 0 {
            child
        } else {
            let border = if selected {
                palette.line
            } else {
                palette.canvas.mix(palette.line, 0.7)
            };
            child.border_sides(edges, border)
        });
    }
    surface
}

fn action(
    key: &str,
    label: &str,
    shortcut: Option<&str>,
    glyph: &'static Icon,
    palette: Palette,
    enabled: bool,
    press: fn(&mut State, &mut Tx),
) -> El {
    let action = el()
        .key(key)
        .role("button")
        .label(label)
        .w(22.0)
        .h_full()
        .center()
        .child(
            icon(glyph, 13.0)
                .color(if enabled { palette.ink } else { palette.muted })
                .a11y_hidden(),
        );
    let action = match shortcut {
        Some(shortcut) => action.shortcut_hint(format!("{PRIMARY_SHORTCUT}+{shortcut}")),
        None => action,
    };
    if enabled {
        action
            .focusable(true)
            .focus_ring(1.0, 2.0, palette.accent)
            .hover(palette.hover)
            .on_press_tx(press)
    } else {
        action.disabled(true)
    }
}

fn tab_sidebar(tabs: &Tabs, actions: El, palette: Palette, enabled: bool) -> El {
    let border = palette.canvas.mix(palette.line, 0.7);
    let active_fill = palette.canvas.mix(palette.ink, 0.07);
    let group_fill = palette.canvas.mix(palette.ink, 0.045);
    let mut list = col().w_full().gap(2.0).px(4.0).py(4.0).scroll(ScrollSpec {
        stick_bottom: false,
    });
    for group in &tabs.groups {
        let selected = tabs.selected();
        let active = group.members.contains(&selected.id);
        let group_tab = if active {
            selected.id.clone()
        } else {
            group.members[0].clone()
        };
        let (title, glyph) = if group.members.len() == 1 {
            ("Single", &SQUARE)
        } else {
            match group.direction {
                SplitDirection::Horizontal => ("Side by side", &SPLIT_HORIZONTAL),
                SplitDirection::Vertical => ("Stacked", &SPLIT_VERTICAL),
            }
        };
        let ink = if active { palette.ink } else { palette.muted };
        let heading = row()
            .key(format!("terminal-group-{}", group.id))
            .role("button")
            .w_full()
            .h(22.0)
            .items_center()
            .px(6.0)
            .gap(4.0)
            .radius(4.0)
            .fill(if active {
                group_fill
            } else {
                Color::TRANSPARENT
            })
            .child(icon(glyph, 12.0).color(ink).a11y_hidden())
            .child(
                el().grow(1.0)
                    .child(text(title).font(GROUP_TEXT).color(ink)),
            )
            .child(
                text(group.members.len().to_string())
                    .font(GROUP_TEXT)
                    .color(palette.muted),
            )
            .disabled(!enabled)
            .focusable(enabled)
            .hover(palette.hover)
            .on_press_tx(move |state: &mut State, tx| state.select_terminal_tab(&group_tab, tx));
        let mut group_view = col().w_full().child(heading);
        for tab in group.members.iter().filter_map(|id| tabs.tab(id)) {
            let active = tab.id == selected.id;
            let tab_id = tab.id.clone();
            let tab_label = tab.label();
            let ink = if active { palette.ink } else { palette.muted };
            group_view = group_view.child(
                row()
                    .w_full()
                    .h(24.0)
                    .items_center()
                    .px(6.0)
                    .gap(2.0)
                    .radius(6.0)
                    .hover_group()
                    .fill(if active {
                        active_fill
                    } else {
                        Color::TRANSPARENT
                    })
                    .hover(active_fill)
                    .child(tab_close_button(&tab.id, &tab_label, ink, palette, enabled))
                    .child(
                        row()
                            .key(format!("terminal-tab-{}", tab.id))
                            .role("button")
                            .checked(active)
                            .grow(1.0)
                            .h_full()
                            .items_center()
                            .disabled(!enabled)
                            .focusable(enabled)
                            .child(text(tab_label).font(TAB_TEXT).color(ink))
                            .on_press_tx(move |state: &mut State, tx| {
                                state.select_terminal_tab(&tab_id, tx)
                            }),
                    ),
            );
        }
        list = list.child(group_view);
    }
    col()
        .key("terminal-tabs")
        .role("group")
        .label("Terminals")
        .w(144.0)
        .min_w(144.0)
        .h_full()
        .fill(palette.sidebar.mix(palette.canvas, 0.9))
        .border(1.0, border)
        .child(
            row()
                .w_full()
                .h(22.0)
                .justify_end()
                .border_sides([0.0, 0.0, 1.0, 0.0], border)
                .child(actions),
        )
        .child(list.grow(1.0).min_h(0.0))
}

fn tab_close_button(id: &str, label: &str, ink: Color, palette: Palette, enabled: bool) -> El {
    let terminal = icon(&SQUARE_TERMINAL, 12.0).color(ink).a11y_hidden();
    if !enabled {
        return terminal;
    }
    let terminal_id = id.to_owned();
    el().key(format!("terminal-close-tab-{id}"))
        .label(format!("Close {label}"))
        .role("button")
        .w(14.0)
        .h(14.0)
        .center()
        .focusable(true)
        .focus_ring(1.0, 2.0, palette.accent)
        .on_press_tx(move |state: &mut State, tx| state.request_terminal_close(&terminal_id, tx))
        .child(terminal.group_opacity(1.0, 0.0))
        .child(
            icon(&CLOSE, 12.0)
                .color(ink)
                .a11y_hidden()
                .absolute(1.0, 1.0)
                .group_opacity(0.0, 1.0),
        )
}

pub(super) fn close_dialog(label: &str, palette: Palette) -> El {
    let title = format!("Close terminal \"{label}\"?");
    let color = |value| hex(value).expect("terminal close palette");
    let palette = if palette.dark {
        palette
    } else {
        Palette {
            canvas: color("#fcfcfc"),
            line: color("#e4e4e7"),
            ink: color("#18181b"),
            muted: color("#71717a"),
            ..palette
        }
    };
    let destructive = Palette {
        accent: color(if palette.dark { "#fb414a" } else { "#fb2c36" }),
        on_accent: Color::WHITE,
        ..palette
    };
    let cancel = Palette {
        line: if palette.dark {
            palette.line
        } else {
            color("#d4d4d8")
        },
        ..palette
    };
    col()
        .key("terminal-close-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .padding(16.0)
        .center()
        .hit()
        .fill(Color::BLACK.alpha(0.4))
        .child(
            col()
                .key("terminal-close-dialog")
                .role("alertdialog")
                .label(&title)
                .described_by("terminal-close-description")
                .w_full()
                .max_w(512.0)
                .radius(16.0)
                .clip_rounded(16.0)
                .border(1.0, palette.line)
                .fill(palette.canvas)
                .child(
                    col()
                        .px(24.0)
                        .py(26.0)
                        .gap(8.0)
                        .child(text(title).font(DIALOG_TITLE).color(palette.ink).wrap())
                        .child(
                            text("This stops the running process and clears its history.")
                                .key("terminal-close-description")
                                .font(DIALOG_BODY)
                                .color(palette.muted)
                                .wrap(),
                        ),
                )
                .child(
                    row()
                        .w_full()
                        .justify_end()
                        .gap(8.0)
                        .px(24.0)
                        .py(16.0)
                        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
                        .fill(if palette.dark {
                            palette.sidebar.mix(palette.canvas, 0.28)
                        } else {
                            color("#fafafa")
                        })
                        .child(
                            button(
                                CANCEL_CLOSE_KEY,
                                "Cancel",
                                cancel,
                                false,
                                true,
                                State::cancel_terminal_close,
                            )
                            .h(32.0)
                            .radius(8.0)
                            .fill(if palette.dark {
                                palette.field
                            } else {
                                Color::WHITE
                            }),
                        )
                        .child(
                            button(
                                "confirm-terminal-close",
                                "Confirm",
                                destructive,
                                true,
                                true,
                                State::confirm_terminal_close,
                            )
                            .h(32.0)
                            .radius(8.0),
                        ),
                )
                .on_key(|state: &mut State, event, tx| {
                    if event.key == Key::Escape {
                        state.cancel_terminal_close(tx);
                        return true;
                    }
                    state.matches_terminal_toggle(event, tx)
                        || event.is_command('n')
                        || event.is_command('w')
                }),
        )
}
