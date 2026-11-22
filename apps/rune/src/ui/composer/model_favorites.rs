use rune::color::hex;
use rune::{El, Icon, Key, el, icon, text};
use uuid::Uuid;

use crate::connection::{FavoriteSetting, ModelFavorite};

use super::super::sidebar::project_badge::lookup_icon;
use super::super::{Palette, State};
use super::model_picker::{Colors, DETAILS};

static FILLED_STAR: Icon = Icon {
    name: "model-favorite-star",
    view: 24.0,
    body: r#"<path fill="currentColor" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z"/>"#,
};

#[derive(Default)]
pub(in crate::ui) struct Favorites {
    setting: FavoriteSetting,
    pending: Option<PendingFavorite>,
    error: Option<String>,
}

struct PendingFavorite {
    operation: Uuid,
    model: ModelFavorite,
    favorite: bool,
    received_settings: bool,
    acknowledged: bool,
}

impl Favorites {
    pub(in crate::ui) fn apply(&mut self, setting: Option<FavoriteSetting>) {
        let Some(setting) = setting else {
            self.setting = FavoriteSetting::default();
            self.pending = None;
            return;
        };
        self.setting = setting;
        if let Some(pending) = &mut self.pending {
            pending.received_settings |=
                self.setting.models.contains(&pending.model) == pending.favorite;
            if pending.acknowledged && pending.received_settings {
                self.pending = None;
            }
        }
    }

    pub(in crate::ui) fn settle(&mut self, operation: Uuid, result: &Result<(), String>) {
        let Some(pending) = self
            .pending
            .as_mut()
            .filter(|pending| pending.operation == operation)
        else {
            return;
        };
        if result.is_ok() && !pending.received_settings {
            pending.acknowledged = true;
        } else {
            self.pending = None;
            self.error = result.as_ref().err().cloned();
        }
    }

    pub(super) fn contains(&self, provider: &str, model: &str) -> bool {
        self.setting
            .models
            .iter()
            .any(|favorite| favorite.provider == provider && favorite.model == model)
    }

    pub(super) fn error(&self, colors: Colors) -> Option<El> {
        self.error.as_ref().map(|message| {
            text(format!("Favorites not saved. {message}"))
                .role("alert")
                .font(DETAILS)
                .color(colors.muted)
        })
    }

    pub(super) fn action(
        &self,
        provider: &str,
        model: &str,
        shelf: bool,
        palette: Palette,
        colors: Colors,
    ) -> El {
        let saved = self.contains(provider, model);
        let label = if saved {
            "Remove from favorites"
        } else {
            "Add to favorites"
        };
        let model = ModelFavorite {
            provider: provider.into(),
            model: model.into(),
        };
        let ready = self.setting.writable;
        let artwork = if saved {
            icon(&FILLED_STAR, 12.0).color(hex("#f59e0b").expect("favorite star color"))
        } else {
            icon(lookup_icon("Star").expect("favorite star"), 12.0).color(colors.muted)
        };
        let key = format!("draft-model-favorite-{}-{}", model.provider, model.model);
        let save = move |state: &mut State| {
            let Some(workspace) = state.workspace.as_mut() else {
                return;
            };
            let favorites = &mut workspace.model_favorites;
            if favorites.setting.writable
                && favorites.pending.is_none()
                && let Some(connection) = state.connection.as_ref()
                && let Some(operation) = connection.update_model_favorite(model.clone(), !saved)
            {
                favorites.error = None;
                favorites.pending = Some(PendingFavorite {
                    operation,
                    model: model.clone(),
                    favorite: !saved,
                    received_settings: false,
                    acknowledged: false,
                });
            }
        };
        let save_with_key = save.clone();
        let button = el()
            .key(key)
            .role("button")
            .label(label)
            .help(label)
            .size(24.0, 24.0)
            .center()
            .radius(5.0)
            .disabled(!ready)
            .focusable(ready)
            .focus_ring(1.0, -1.0, palette.accent)
            .hover(colors.hover)
            .on_press_tx(move |state: &mut State, _| save(state))
            .on_key(move |state: &mut State, event, tx| {
                if event.key != Key::Enter {
                    return false;
                }
                if !tx.key_repeat() {
                    save_with_key(state);
                }
                true
            })
            .child(artwork.a11y_hidden());
        if saved && !shelf {
            button
        } else {
            button.group_opacity(0.0, 1.0)
        }
    }
}

pub(super) fn tab_icon(colors: Colors) -> El {
    icon(&FILLED_STAR, 18.0).color(colors.ink).a11y_hidden()
}
