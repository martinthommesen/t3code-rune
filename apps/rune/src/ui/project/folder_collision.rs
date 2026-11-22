use std::time::{Duration, Instant};

use rune::{Color, El, Icon, Key, TextStyle, Tx, col, el, icon, row, text};

use super::super::{Palette, State};

const TITLE: &str = "Failed to add project";
const DESCRIPTION: &str = "Failed to mutate project.";
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

pub(in crate::ui) struct Notice {
    expires: Instant,
    copied_until: Option<Instant>,
}

pub(in crate::ui) fn show(state: &mut State, command_id: &str, tx: &mut Tx) {
    let Some(form) = state.add_project.as_mut().filter(|form| {
        form.pending.as_deref() == Some(command_id)
            && matches!(form.step, super::ProjectStep::Folder)
    }) else {
        return;
    };
    form.pending = None;
    form.error = None;
    state.project_folder_collision = Some(Notice {
        expires: Instant::now() + LIFETIME,
        copied_until: None,
    });
    tx.wake_after(LIFETIME);
}

pub(in crate::ui) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let Some(notice) = &mut state.project_folder_collision else {
        return false;
    };
    let now = Instant::now();
    let remaining = notice.expires.saturating_duration_since(now);
    if remaining.is_zero() {
        state.project_folder_collision = None;
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
    let Some(notice) = &mut state.project_folder_collision else {
        return;
    };
    tx.copy(DESCRIPTION);
    notice.copied_until = Some(Instant::now() + COPIED_LIFETIME);
    tx.wake_after(COPIED_LIFETIME);
}

fn dismiss(state: &mut State, _: &mut Tx) {
    state.project_folder_collision = None;
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

pub(in crate::ui) fn overlay(state: &State, palette: Palette, width: f32) -> Option<El> {
    let notice = state.project_folder_collision.as_ref()?;
    let copied = notice.copied_until.is_some();
    let inset = if width >= 640.0 { 32.0 } else { 16.0 };
    let width = (width - 2.0 * inset).clamp(0.0, 360.0);
    Some(
        row()
            .key("add-project-folder-error")
            .role("alert")
            .label(TITLE)
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
                            .child(text(TITLE).font(TITLE_STYLE).color(palette.ink)),
                    )
                    .child(
                        text(DESCRIPTION)
                            .font(DESCRIPTION_STYLE)
                            .color(palette.muted),
                    ),
            )
            .child(icon_button(
                "copy-add-project-folder-error",
                if copied { "Copied error" } else { "Copy error" },
                if copied { &CHECK } else { &COPY },
                palette,
                copy,
            ))
            .child(
                icon_button(
                    "dismiss-add-project-folder-error",
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
