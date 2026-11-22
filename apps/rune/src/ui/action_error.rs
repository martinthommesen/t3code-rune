use std::time::{Duration, Instant};

use rune::{Color, El, Icon, Key, TextStyle, Tx, col, el, icon, row, text};

use super::{Palette, State};

const LIFETIME: Duration = Duration::from_secs(5);
const COPIED_LIFETIME: Duration = Duration::from_secs(2);
const TITLE_STYLE: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const DESCRIPTION_STYLE: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static ERROR: Icon = Icon {
    name: "circle-alert",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 8v4m0 4h.01"/></g>"#,
};
static CLOSE: Icon = Icon {
    name: "x",
    view: 24.0,
    body: r#"<path d="m18 6-12 12M6 6l12 12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
};
static COPY: Icon = Icon {
    name: "copy",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 4H6a2 2 0 0 0-2 2v10"/></g>"#,
};
static CHECK: Icon = Icon {
    name: "check",
    view: 24.0,
    body: r#"<path d="m20 6-11 11-5-5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(super) enum Kind {
    AddProjectFolder,
    CreateRef,
    StorageCleanup(String),
}

impl Kind {
    fn presentation(&self) -> (&'static str, &str) {
        match self {
            Self::AddProjectFolder => ("add-project-folder-error", "Failed to add project"),
            Self::CreateRef => ("create-ref-error", "Failed to create and switch ref."),
            Self::StorageCleanup(title) => ("storage-cleanup-error", title),
        }
    }
}

pub(super) struct Notice {
    kind: Kind,
    description: String,
    expires: Instant,
    copied_until: Option<Instant>,
}

pub(super) fn show(state: &mut State, kind: Kind, description: impl Into<String>, tx: &mut Tx) {
    state.action_error = Some(Notice {
        kind,
        description: description.into(),
        expires: Instant::now() + LIFETIME,
        copied_until: None,
    });
    tx.wake_after(LIFETIME);
}

pub(super) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let Some(notice) = &mut state.action_error else {
        return false;
    };
    let now = Instant::now();
    let remaining = notice.expires.saturating_duration_since(now);
    if remaining.is_zero() {
        state.action_error = None;
        return true;
    }
    tx.wake_after(remaining);
    if let Some(until) = notice.copied_until {
        let remaining = until.saturating_duration_since(now);
        if remaining.is_zero() {
            notice.copied_until = None;
            return true;
        }
        tx.wake_after(remaining);
    }
    false
}

fn copy(state: &mut State, tx: &mut Tx) {
    let Some(notice) = &mut state.action_error else {
        return;
    };
    tx.copy(notice.description.as_str());
    notice.copied_until = Some(Instant::now() + COPIED_LIFETIME);
    tx.wake_after(COPIED_LIFETIME);
}

fn dismiss(state: &mut State, _: &mut Tx) {
    state.action_error = None;
}

fn icon_button(
    key: &str,
    label: &str,
    glyph: &'static Icon,
    palette: Palette,
    action: fn(&mut State, &mut Tx),
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .help(label)
        .focusable(true)
        .size(22.0, 22.0)
        .center()
        .radius(6.0)
        .hover(palette.hover)
        .focus_ring(1.0, 1.0, palette.accent)
        .on_press_tx(action)
        .on_key(move |state: &mut State, event, tx| {
            if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ") {
                action(state, tx);
                true
            } else {
                false
            }
        })
        .child(icon(glyph, 12.0).color(palette.muted).a11y_hidden())
}

pub(super) fn overlay(state: &State, palette: Palette, width: f32) -> Option<El> {
    let notice = state.action_error.as_ref()?;
    let (key, title) = notice.kind.presentation();
    let copied = notice.copied_until.is_some();
    let inset = if width >= 640.0 { 32.0 } else { 16.0 };
    let width = (width - 2.0 * inset).clamp(0.0, 360.0);
    Some(
        row()
            .key(key)
            .role("alert")
            .label(title)
            .pin(None, Some(inset + 40.0), Some(inset), None)
            .w(width)
            .center_y()
            .gap(8.0)
            .px(14.0)
            .py(11.0)
            .radius(8.0)
            .fill(palette.field)
            .border(1.0, palette.line)
            .border_layout()
            .shadow(12.0, 18.0, Color::BLACK.alpha(0.30))
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(2.0)
                    .child(
                        row()
                            .items_center()
                            .gap(8.0)
                            .child(icon(&ERROR, 16.0).color(palette.error).a11y_hidden())
                            .child(text(title).font(TITLE_STYLE).color(palette.ink)),
                    )
                    .child(
                        text(&notice.description)
                            .font(DESCRIPTION_STYLE)
                            .color(palette.muted)
                            .wrap(),
                    ),
            )
            .child(icon_button(
                &format!("copy-{key}"),
                if copied { "Copied error" } else { "Copy error" },
                if copied { &CHECK } else { &COPY },
                palette,
                copy,
            ))
            .child(
                icon_button(
                    &format!("dismiss-{key}"),
                    "Dismiss notification",
                    &CLOSE,
                    palette,
                    dismiss,
                )
                .pin(None, Some(-5.0), Some(-5.0), None)
                .radius(11.0)
                .fill(palette.field)
                .border(1.0, palette.line),
            ),
    )
}
