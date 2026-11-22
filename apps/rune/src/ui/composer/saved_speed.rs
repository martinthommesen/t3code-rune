use rune::color::hex;
use rune::{Color, El, Icon, Key, icon, row, text};

use t3_rune::providers::codex::{ReasoningEffort, ServiceTier};

use crate::connection::ProviderModel;

use super::super::{Palette, State};
use super::model_picker::{Colors, DETAILS};

static BOLT: Icon = Icon {
    name: "fast-service-tier",
    view: 24.0,
    body: r#"<path fill="currentColor" stroke="currentColor" stroke-width="2" stroke-linejoin="round" d="M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"/>"#,
};

pub(super) fn bolt(color: Color) -> El {
    icon(&BOLT, 12.0).color(color).a11y_hidden()
}

pub(super) struct SavedSpeed<'a> {
    current: ServiceTier,
    standard: (&'a str, Option<&'a str>),
    fast: (&'a str, Option<&'a str>),
}

impl<'a> SavedSpeed<'a> {
    pub(super) fn for_model(
        model: &'a ProviderModel,
        current: Option<ServiceTier>,
    ) -> Option<Self> {
        if model.slug != ReasoningEffort::SOL_MODEL {
            return None;
        }
        Some(Self {
            current: current.unwrap_or(ServiceTier::Standard),
            standard: model.service_tier_choice(ServiceTier::Standard)?,
            fast: model.service_tier_choice(ServiceTier::Priority)?,
        })
    }

    pub(super) fn fast_label(&self) -> Option<&str> {
        (self.current == ServiceTier::Priority).then_some(self.fast.0)
    }

    pub(super) fn view(&self, thread_id: &str, palette: Palette) -> El {
        let color = Colors::new(palette.dark);
        let [surface, selection] = if palette.dark {
            ["#262626", "#414141"]
        } else {
            ["#ededee", "#dbdbde"]
        }
        .map(|color| hex(color).expect("service tier palette"));
        let mut group = row()
            .key("saved-codex-speed")
            .role("radiogroup")
            .label("Service Tier")
            .h(28.0)
            .padding(2.0)
            .radius(8.0)
            .fill(surface);
        for (tier, (label, description)) in [
            (ServiceTier::Standard, self.standard),
            (ServiceTier::Priority, self.fast),
        ] {
            let selected = self.current == tier;
            let fast = tier == ServiceTier::Priority;
            let key = format!("saved-codex-speed-{}", tier.as_str());
            let thread = thread_id.to_owned();
            let keyboard_thread = thread.clone();
            let focus_key = key.clone();
            let foreground = if selected { color.ink } else { color.muted };
            let width =
                rune::text::measure(label, &[], DETAILS, None).0 + if fast { 32.0 } else { 16.0 };
            let mut segment = row()
                .key(key)
                .role("radio")
                .label(label)
                .checked(selected)
                .focusable(true)
                .focus_ring(2.0, -2.0, palette.accent)
                .size(width, 24.0)
                .px(8.0)
                .gap(4.0)
                .center_y()
                .corners(if fast {
                    [0.0, 6.0, 6.0, 0.0]
                } else {
                    [6.0, 0.0, 0.0, 6.0]
                })
                .fill(if selected {
                    selection
                } else {
                    Color::TRANSPARENT
                })
                .on_press_tx(move |state: &mut State, tx| {
                    state.choose_saved_speed(&thread, tier);
                    tx.focus_when_shown(focus_key.clone(), tx.focus_visible());
                })
                .on_key(move |state: &mut State, event, tx| {
                    if event.key != Key::Enter {
                        return false;
                    }
                    if !tx.key_repeat() {
                        state.choose_saved_speed(&keyboard_thread, tier);
                    }
                    true
                });
            if let Some(description) = description {
                segment = segment.help(description);
            }
            if fast {
                segment = segment.child(bolt(foreground));
            }
            group = group.child(segment.child(text(label).font(DETAILS).color(foreground)));
        }
        group
    }
}
