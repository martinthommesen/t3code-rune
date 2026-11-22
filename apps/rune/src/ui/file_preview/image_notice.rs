use std::time::Duration;

use rune::color::hex;
use rune::{Color, Edge, El, Icon, TextStyle, Transition, Tx, col, icon, row, text};

use super::{COPY, Layout, Palette, State};

pub(super) const COPY_ERROR: &str = "Could not copy image";
pub(super) const SAVE_ERROR: &str = "Could not save image";
pub(super) const DECODE_ERROR: &str =
    "The browser could not decode this image for copying. Try saving it instead.";
const VISIBLE_FOR: Duration = Duration::from_secs(5);
const COPIED_FOR: Duration = Duration::from_secs(2);
const HEADING: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

static ALERT: Icon = Icon {
    name: "circle-alert",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><path d="M12 8v4m0 4h.01"/></g>"#,
};

static CLOSE: Icon = Icon {
    name: "x",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" d="m18 6-12 12M6 6l12 12"/>"#,
};

static CHECK: Icon = Icon {
    name: "check",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" d="m20 6-11 11-5-5"/>"#,
};

pub(in crate::ui) struct Notice {
    title: &'static str,
    description: String,
    expires: f64,
    hover_started: Option<f64>,
    copied_until: Option<f64>,
}

pub(super) fn show(
    state: &mut State,
    tx: &mut Tx,
    title: &'static str,
    description: impl Into<String>,
) {
    state.file_preview_image_notice = Some(Notice {
        title,
        description: description.into(),
        expires: tx.clock() + VISIBLE_FOR.as_secs_f64(),
        hover_started: None,
        copied_until: None,
    });
    tx.wake_after(VISIBLE_FOR);
}

pub(in crate::ui) fn poll(state: &mut State, clock: f64, tx: &mut Tx) -> bool {
    let Some(notice) = &mut state.file_preview_image_notice else {
        return false;
    };
    if notice.hover_started.is_none() && clock >= notice.expires {
        state.file_preview_image_notice = None;
        tx.animated();
        return true;
    }
    if notice.copied_until.is_some_and(|until| clock >= until) {
        notice.copied_until = None;
        return true;
    }
    false
}

fn hover(state: &mut State, hovered: bool, tx: &mut Tx) {
    let Some(notice) = &mut state.file_preview_image_notice else {
        return;
    };
    let now = tx.clock();
    if hovered {
        notice.hover_started.get_or_insert(now);
    } else if let Some(started) = notice.hover_started.take() {
        notice.expires += now - started;
        tx.wake_after(Duration::from_secs_f64((notice.expires - now).max(0.0)));
    }
}

fn copy(state: &mut State, tx: &mut Tx) {
    if let Some(notice) = &mut state.file_preview_image_notice {
        tx.copy(notice.description.as_str());
        notice.copied_until = Some(tx.clock() + COPIED_FOR.as_secs_f64());
        tx.wake_after(COPIED_FOR);
    }
}

fn dismiss(state: &mut State, _: &mut Tx) {
    state.file_preview_image_notice = None;
}

pub(in crate::ui) fn view(state: &State, layout: Layout, palette: Palette) -> Option<El> {
    let notice = state.file_preview_image_notice.as_ref()?;
    let [window, _] = layout.viewport;
    let inset = if window >= 640.0 { 32.0 } else { 16.0 };
    let width = (window - 2.0 * inset).clamp(1.0, 360.0);
    let color = |dark: &str, light: &str| hex(if palette.dark { dark } else { light }).unwrap();
    let fill = color("#111111", "#ffffff").alpha(0.836);
    let line = color("#292929", "#dcdce0");
    let ink = color("#f5f5f5", "#18181b");
    let muted = color("#a1a1a1", "#71717b");
    let danger = color("#fb414a", "#ff0616");

    let copied = notice.copied_until.is_some();
    let copy = row()
        .key("image-copy-error-copy")
        .role("button")
        .label(if copied { "Copied error" } else { "Copy error" })
        .focusable(true)
        .focus_ring(2.0, -2.0, palette.accent)
        .size(20.0, 20.0)
        .center()
        .on_press_tx(copy)
        .child(
            icon(if copied { &CHECK } else { &COPY }, 12.0).color(if copied {
                color("#4ade80", "#117a38")
            } else {
                muted.alpha(0.8)
            }),
        );
    let close = row()
        .key("image-copy-error-dismiss")
        .role("button")
        .label("Dismiss notification")
        .absolute(width - 19.0, -6.0)
        .focusable(true)
        .focus_ring(2.0, -2.0, palette.accent)
        .size(24.0, 24.0)
        .radius(12.0)
        .fill(color("#121212", "#ffffff"))
        .hover(color("#1c1c1c", "#f4f4f5"))
        .border(1.0, line)
        .center()
        .on_press_tx(dismiss)
        .child(icon(&CLOSE, 12.0).color(muted));

    Some(
        row()
            .key("image-copy-error")
            .role("dialog")
            .label(notice.title)
            .absolute(window - inset - width, layout.header_height + inset)
            .w(width)
            .pl(14.0)
            .pr(24.0)
            .py(12.0)
            .gap(6.0)
            .center_y()
            .radius(8.0)
            .backdrop_blur(12.0)
            .fill(fill)
            .border(1.0, line)
            .border_layout()
            .shadow(18.0, 32.0, Color::BLACK.alpha(0.25))
            .transition(
                Transition::move_edge(Edge::Trailing)
                    .combined(Transition::offset(inset, 0.0))
                    .combined(Transition::opacity()),
            )
            .on_hover_change(hover)
            .child(
                col()
                    .min_w(0.0)
                    .grow(1.0)
                    .gap(2.0)
                    .child(
                        row()
                            .gap(8.0)
                            .center_y()
                            .child(icon(&ALERT, 16.0).color(danger))
                            .child(text(notice.title).font(HEADING).color(ink).role("alert")),
                    )
                    .child(text(&notice.description).font(BODY).color(muted).wrap()),
            )
            .child(copy)
            .child(close),
    )
}
