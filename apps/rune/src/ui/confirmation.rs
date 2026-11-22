use rune::{Color, El, Key, TextStyle, Tx, col, el, row, text};

use super::{Palette, State, sidebar_header};

const TITLE_FONT: TextStyle = TextStyle::new(20.0, 600.0).lh(28.0);
const BODY_FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const BUTTON_FONT: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);

pub(super) struct Dialog<'a> {
    pub(super) overlay: &'static str,
    pub(super) key: &'static str,
    pub(super) cancel: &'static str,
    pub(super) confirm: &'static str,
    pub(super) title: &'a str,
    pub(super) detail: Option<(&'static str, &'static str)>,
    pub(super) destructive: bool,
}

pub(super) fn view(
    dialog: Dialog<'_>,
    palette: Palette,
    cancel: fn(&mut State, &mut Tx),
    confirm: fn(&mut State, &mut Tx),
) -> El {
    let neutral = |hex| rune::color::hex(hex).expect("static dialog palette");
    let action = if dialog.destructive {
        neutral(if palette.dark { "#fb414a" } else { "#fb2c36" })
    } else {
        neutral(if palette.dark { "#407df7" } else { "#1b4ed8" })
    };
    let scrim = if palette.dark {
        Color::BLACK.alpha(0.55)
    } else {
        Color::WHITE.alpha(0.62)
    };
    let mut panel = col()
        .key(dialog.key)
        .role("alertdialog")
        .label(dialog.title)
        .w_full()
        .max_w(512.0)
        .radius(16.0)
        .clip_rounded(16.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(palette.canvas)
        .shadow(9.0, 24.0, Color::BLACK.alpha(0.28))
        .hit()
        .on_key(move |state: &mut State, event, tx| {
            if event.key != Key::Escape {
                return false;
            }
            cancel(state, tx);
            true
        });
    let mut body = col().w_full().px(24.0).py(24.0).gap(8.0).child(
        text(dialog.title)
            .font(TITLE_FONT)
            .color(palette.ink)
            .wrap(),
    );
    if let Some((key, detail)) = dialog.detail {
        panel = panel.described_by(key);
        body = body.child(
            text(detail)
                .key(key)
                .font(BODY_FONT)
                .color(palette.muted)
                .wrap(),
        );
    }
    let buttons = row()
        .w_full()
        .h(64.0)
        .px(24.0)
        .items_center()
        .justify_end()
        .gap(8.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
        .fill(if palette.dark {
            palette.sidebar.mix(palette.canvas, 0.28)
        } else {
            neutral("#f8f8f8")
        })
        .child(press(
            dialog_button(
                dialog.cancel,
                "Cancel",
                70.0,
                palette.field,
                palette.ink,
                palette.line,
                palette,
            ),
            cancel,
        ))
        .child(press(
            dialog_button(
                dialog.confirm,
                "Confirm",
                78.0,
                action,
                Color::WHITE,
                action,
                palette,
            ),
            confirm,
        ));
    col()
        .key(dialog.overlay)
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .padding(16.0)
        .center()
        .hit()
        .backdrop_blur(4.0)
        .fill(scrim)
        .child(panel.child(body).child(buttons))
}

fn press(button: El, action: fn(&mut State, &mut Tx)) -> El {
    button
        .on_press_tx(action)
        .on_key(move |state: &mut State, event, tx| {
            sidebar_header::activate_on_return(state, event, tx, action)
        })
}

fn dialog_button(
    key: &str,
    label: &str,
    width: f32,
    fill: Color,
    ink: Color,
    border: Color,
    palette: Palette,
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .size(width, 32.0)
        .radius(8.0)
        .center()
        .fill(fill)
        .border(1.0, border)
        .hover(fill.mix(ink, 0.10))
        .focusable(true)
        .focus_ring(1.0, 2.0, palette.accent)
        .child(text(label).font(BUTTON_FONT).color(ink))
}
