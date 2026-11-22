use rune::{El, Icon, Key, Tx, col, el, icon, row, text};

use crate::client_settings::BrowserLinkTarget;

use super::{CONTROL, DESCRIPTION, Palette, SECTION, State};

pub(super) const INTEGRATIONS: Icon = glyph!(
    "plug",
    r#"<path d="M9 2v5m6-5v5M6 7h12v3a6 6 0 0 1-6 6m0 0v6m0-6a6 6 0 0 1-6-6"/>"#
);
const CHEVRON: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);
const BUTTON: &str = "settings-browser-link-target";
const APP: &str = "settings-browser-link-app";
const SYSTEM: &str = "settings-browser-link-system";

pub(super) fn page(state: &State, palette: Palette, width: f32) -> El {
    let selected = state.client_settings.browser_link_target();
    let mut choice = col().w(202.0).gap(4.0).child(
        row()
            .key(BUTTON)
            .role("combobox")
            .label("Open links in")
            .expanded(state.settings.browser_picker)
            .w_full()
            .h(32.0)
            .center_y()
            .px(10.0)
            .gap(10.0)
            .radius(6.0)
            .border(1.0, palette.accent)
            .focusable(true)
            .hover(palette.accent)
            .on_press_tx(|state: &mut State, tx| {
                state.settings.browser_picker = !state.settings.browser_picker;
                if state.settings.browser_picker {
                    tx.focus_when_shown(
                        option_key(state.client_settings.browser_link_target()),
                        false,
                    );
                }
            })
            .child(
                text(selected.label())
                    .font(CONTROL)
                    .color(palette.ink)
                    .grow(1.0),
            )
            .child(icon(&CHEVRON, 14.0).color(palette.muted)),
    );
    if state.settings.browser_picker {
        let mut options = col()
            .key("settings-browser-link-options")
            .role("listbox")
            .label("Open links in")
            .w_full()
            .padding(4.0)
            .radius(6.0)
            .border(1.0, palette.accent);
        for target in [BrowserLinkTarget::System, BrowserLinkTarget::App] {
            options = options.child(
                el().key(option_key(target))
                    .role("option")
                    .label(target.label())
                    .focus_selected(selected == target)
                    .w_full()
                    .h(30.0)
                    .px(7.0)
                    .center_y()
                    .radius(4.0)
                    .hover(palette.accent)
                    .focusable(true)
                    .on_press_tx(move |state: &mut State, tx| {
                        if let Err(error) =
                            state.client_settings.remember_browser_link_target(target)
                        {
                            state.settings.error =
                                Some(format!("Could not remember how to open links. {error}"));
                        }
                        state.settings.browser_picker = false;
                        tx.focus_when_shown(BUTTON, false);
                    })
                    .on_key(|state: &mut State, event, tx: &mut Tx| match event.key {
                        Key::Up => {
                            tx.focus_when_shown(SYSTEM, true);
                            true
                        }
                        Key::Down => {
                            tx.focus_when_shown(APP, true);
                            true
                        }
                        Key::Escape => {
                            state.settings.browser_picker = false;
                            tx.focus_when_shown(BUTTON, false);
                            true
                        }
                        _ => false,
                    })
                    .child(text(target.label()).font(CONTROL).color(palette.ink)),
            );
        }
        choice = choice.child(options);
    }
    col()
        .w_full()
        .max_w(width)
        .gap(12.0)
        .child(text("Integrations").font(SECTION).color(palette.ink))
        .child(text("Browser").font(SECTION).color(palette.ink))
        .child(
            row()
                .w_full()
                .min_h(44.0)
                .gap(16.0)
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(3.0)
                        .child(text("Open links in").font(CONTROL).color(palette.ink))
                        .child(
                            text("Where links in the chat and terminal open. Hold ⌘ or Ctrl while clicking a link to open it in your default browser either way.")
                                .font(DESCRIPTION)
                                .color(palette.muted),
                        ),
                )
                .child(choice),
        )
}

fn option_key(target: BrowserLinkTarget) -> &'static str {
    match target {
        BrowserLinkTarget::App => APP,
        BrowserLinkTarget::System => SYSTEM,
    }
}
