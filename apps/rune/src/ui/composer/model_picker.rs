use rune::color::hex;
use rune::{
    Color, El, Icon, Key, KeyEvent, TextEdit, TextStyle, Tx, col, edit_access, el, icon, row, text,
    text_input,
};

use crate::connection::ProviderModel;

use super::super::{Palette, State};

pub(super) const LABEL: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
pub(super) const DETAILS: TextStyle = TextStyle::new(12.0, 400.0).lh(17.0);
const TRIGGER: &str = "claude-model-picker";
const QUERY: &str = "claude-model-search";
const EFFORT: &str = "claude-model-effort";

static CLAUDE: Icon = Icon {
    name: "claude",
    view: 24.0,
    body: include_str!("../sidebar/provider_badge/claude.svg"),
};

pub(super) static SEARCH: Icon = Icon {
    name: "model-search",
    view: 24.0,
    body: r#"<circle cx="11" cy="11" r="8" fill="none" stroke="currentColor" stroke-width="2"/><path d="m21 21-4.35-4.35" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
};

pub(super) static CHECK: Icon = Icon {
    name: "model-check",
    view: 24.0,
    body: r#"<path d="m5 12 4 4 10-10" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Clone)]
pub(super) enum Target {
    Thread(String),
}

impl Target {
    fn picker_mut<'a>(&self, state: &'a mut State) -> Option<&'a mut Option<TextEdit>> {
        match self {
            Self::Thread(id) => state
                .conversation_mut(id)
                .map(|entry| &mut entry.model_picker),
        }
    }
}

pub(super) fn trigger(target: Target, model: &ProviderModel, open: bool, palette: Palette) -> El {
    let name = model.short_name.as_deref().unwrap_or(&model.name);
    let color = Colors::new(palette.dark);
    row()
        .key(TRIGGER)
        .role("button")
        .label(format!("{}, High · 200k", model.name))
        .help("Models")
        .expanded(open)
        .focusable(true)
        .focus_ring(1.0, -1.0, palette.accent)
        .w(183.0)
        .shrink()
        .h(28.0)
        .px(8.0)
        .center_y()
        .gap(4.0)
        .radius(9.0)
        .fill(color.control)
        .hover(color.hover)
        .on_press_tx(move |state: &mut State, tx| {
            if let Some(picker) = target.picker_mut(state)
                && picker.take().is_none()
            {
                *picker = Some(TextEdit::default());
                tx.focus_when_shown(QUERY, false);
            }
        })
        .child(claude(16.0))
        .child(text(name).font(LABEL).color(color.ink).truncate())
        .child(text("High").font(LABEL).color(color.muted))
        .child(text("200k").font(DETAILS).color(color.ink))
}

pub(super) fn popup(
    target: Target,
    model: &ProviderModel,
    query: &TextEdit,
    lane: f32,
    palette: Palette,
) -> El {
    let edit_target = target.clone();
    let input_target = target.clone();
    let color = Colors::new(palette.dark);
    let menu_width = popup_width(lane);
    let columns = menu_width >= 420.0;
    let mut list = col().min_w(0.0).grow(1.0).gap(9.0).padding(10.0).child(
        row()
            .h(32.0)
            .center_y()
            .gap(6.0)
            .border_sides([0.0, 0.0, 1.0, 0.0], palette.accent)
            .child(icon(&SEARCH, 14.0).color(color.muted).a11y_hidden())
            .child(
                text_input(query, LABEL)
                    .key(QUERY)
                    .label("Search models")
                    .w_full()
                    .h(27.0)
                    .color(color.ink)
                    .single_line()
                    .placeholder("Search models...", color.muted)
                    .on_edit(edit_access(move |state: &mut State| {
                        edit_target
                            .picker_mut(state)
                            .expect("open model picker")
                            .as_mut()
                            .expect("model search")
                    }))
                    .on_input_key(move |state: &mut State, event, tx| {
                        escape(state, &input_target, event, tx)
                    }),
            ),
    );
    let needle = query.text.trim().to_lowercase();
    if model.name.to_lowercase().contains(&needle) || model.slug.to_lowercase().contains(&needle) {
        list = list.child(
            row()
                .key("claude-model-sonnet")
                .role("option")
                .label(&model.name)
                .checked(true)
                .w_full()
                .h(32.0)
                .px(8.0)
                .gap(8.0)
                .center_y()
                .radius(5.0)
                .focusable(true)
                .focus_ring(1.0, -1.0, palette.accent)
                .hover(color.hover)
                .on_press_tx(move |_: &mut State, tx| {
                    tx.focus_when_shown(EFFORT, tx.focus_visible());
                })
                .child(claude(16.0))
                .child(
                    text(&model.name)
                        .font(LABEL)
                        .color(color.ink)
                        .truncate()
                        .grow(1.0),
                )
                .child(icon(&CHECK, 12.0).color(color.ink).a11y_hidden()),
        );
    } else {
        list = list.child(text("No matching models").font(DETAILS).color(color.muted));
    }
    let side = col()
        .w(44.0)
        .h_full()
        .padding(4.0)
        .border_sides([0.0, 1.0, 0.0, 0.0], color.line)
        .child(
            el().size(36.0, 36.0)
                .center()
                .label("Claude")
                .radius(5.0)
                .fill(color.control)
                .child(claude(20.0)),
        );
    let details = claude_details(
        model,
        if columns { 192.0 } else { menu_width - 45.0 },
        palette,
        EFFORT,
        QUERY,
    );
    let body = if columns {
        row()
            .w_full()
            .grow(1.0)
            .child(side)
            .child(list)
            .child(details.border_sides([0.0, 0.0, 0.0, 1.0], color.line))
    } else {
        row()
            .w_full()
            .grow(1.0)
            .child(side)
            .child(col().grow(1.0).child(list).child(details))
    };
    popup_frame("claude-model-popup", "Claude models", lane, palette, body)
        .focus_group()
        .on_key(move |state: &mut State, event, tx| escape(state, &target, event, tx))
        .child(footer(Some("Full access"), None, color))
}

pub(super) fn popup_width(lane: f32) -> f32 {
    554.0_f32.min((lane - 16.0).max(220.0))
}

pub(super) fn popup_frame(
    key: &'static str,
    name: &str,
    lane: f32,
    palette: Palette,
    body: El,
) -> El {
    let color = Colors::new(palette.dark);
    let width = popup_width(lane);
    col()
        .key(key)
        .role("dialog")
        .label(name)
        .w(width)
        .h(if width >= 420.0 { 393.0 } else { 260.0 })
        .on_background_press_tx(|_: &mut State, tx| tx.live_only())
        .radius(8.0)
        .fill(color.panel)
        .border(1.0, color.line)
        .border_layout()
        .paint_order(3)
        .shadow(4.0, 20.0, Color::BLACK.alpha(0.25))
        .pin(
            None,
            None,
            Some(8.0_f32.max((lane - width).min(84.0))),
            Some(42.0),
        )
        .child(body)
}

pub(super) fn footer(permissions: Option<&str>, speed: Option<El>, color: Colors) -> El {
    let mut footer = row()
        .w_full()
        .h(44.0)
        .pl(if speed.is_some() { 12.0 } else { 15.0 })
        .pr(15.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], color.line);
    if let Some(speed) = speed {
        footer = footer.child(speed);
    }
    footer = footer.child(el().grow(1.0));
    if let Some(label) = permissions {
        footer = footer.child(
            text(label)
                .label(format!("Permissions: {label}"))
                .font(DETAILS)
                .color(color.muted),
        );
    }
    footer
}

pub(super) fn claude_details(
    model: &ProviderModel,
    width: f32,
    palette: Palette,
    effort_key: &'static str,
    query_key: &'static str,
) -> El {
    let color = Colors::new(palette.dark);
    let model_name = model.short_name.as_deref().unwrap_or(&model.name);
    col()
        .gap(10.0)
        .padding(12.0)
        .w(width)
        .child(
            row()
                .w_full()
                .center_y()
                .gap(6.0)
                .child(text("Reasoning").font(LABEL).color(color.muted).grow(1.0))
                .child(text(model_name).font(DETAILS).color(color.muted)),
        )
        .child(
            row()
                .key(effort_key)
                .role("radio")
                .label("High, Default")
                .checked(true)
                .focusable(true)
                .focus_ring(1.0, -1.0, palette.accent)
                .on_key(move |_: &mut State, event, tx| {
                    if event.key != Key::Left {
                        return false;
                    }
                    tx.focus_when_shown(query_key, tx.focus_visible());
                    true
                })
                .radius(5.0)
                .h(32.0)
                .px(4.0)
                .center_y()
                .gap(8.0)
                .child(text("High").font(LABEL).color(color.ink))
                .child(text("Default").font(DETAILS).color(color.muted).grow(1.0))
                .child(icon(&CHECK, 12.0).color(color.ink).a11y_hidden()),
        )
        .child(
            col()
                .w_full()
                .pt(8.0)
                .border_sides([1.0, 0.0, 0.0, 0.0], color.line)
                .child(text("Context Window").font(DETAILS).color(color.muted))
                .child(text("200k").font(LABEL).color(color.ink)),
        )
}

pub(super) fn dismissal(target: Target, viewport: [f32; 2]) -> El {
    el().key("claude-model-dismiss")
        .absolute(-viewport[0], -viewport[1])
        .size(2.0 * viewport[0], 2.0 * viewport[1])
        .paint_order(2)
        .a11y_hidden()
        .on_background_press_tx(move |state: &mut State, tx| close(state, &target, tx))
}

fn escape(state: &mut State, target: &Target, event: &KeyEvent, tx: &mut Tx) -> bool {
    if event.key != Key::Escape {
        return false;
    }
    close(state, target, tx);
    true
}

fn close(state: &mut State, target: &Target, tx: &mut Tx) {
    if let Some(picker) = target.picker_mut(state) {
        *picker = None;
    }
    tx.focus_when_shown(TRIGGER, tx.focus_visible());
}

pub(super) fn claude(size: f32) -> El {
    icon(&CLAUDE, size)
        .color(hex("#d97757").expect("Claude color"))
        .a11y_hidden()
}

#[derive(Clone, Copy)]
pub(super) struct Colors {
    panel: Color,
    pub(super) line: Color,
    pub(super) control: Color,
    pub(super) hover: Color,
    pub(super) ink: Color,
    pub(super) muted: Color,
}

impl Colors {
    pub(super) fn new(dark: bool) -> Self {
        let [panel, line, control, hover, ink, muted] = if dark {
            [
                "#121212", "#292929", "#171717", "#222222", "#f1f1f1", "#a3a3a3",
            ]
        } else {
            [
                "#ffffff", "#e5e5e5", "#f7f7f7", "#f0f0f0", "#18181b", "#71717b",
            ]
        }
        .map(|color| hex(color).expect("model picker palette"));
        Self {
            panel,
            line,
            control,
            hover,
            ink,
            muted,
        }
    }
}
