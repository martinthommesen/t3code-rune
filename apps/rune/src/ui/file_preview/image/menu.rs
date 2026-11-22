use rune::color::hex;
use rune::{
    ClipboardContent, El, Family, Key, PointerPress, PopupItem, PopupMenu, TextStyle, Tx, col,
    popup_menu_supported, row, text,
};

use crate::connection::file_preview::AssetPurpose;

use super::super::{Load, State, binary::Phase, image_notice};
use super::{Bounds, PICTURE, PreviewImage, expanded::SCROLLER};

const MENU_FONT: TextStyle = TextStyle::new(14.0, 400.0)
    .lh(20.0)
    .family(Family::Named("Cantarell, DejaVu Sans"));
const DOCKED_ACTIONS: [Action; 4] = [
    Action::CopyFullPath,
    Action::CopyRelativePath,
    Action::Save,
    Action::CopyImage,
];

const EXPANDED_ACTIONS: [Action; 2] = [Action::Save, Action::CopyImage];

#[derive(Clone, Copy)]
pub(super) enum Surface {
    Docked,
    Expanded,
}

impl Surface {
    fn actions(self) -> &'static [Action] {
        match self {
            Self::Docked => &DOCKED_ACTIONS,
            Self::Expanded => &EXPANDED_ACTIONS,
        }
    }

    fn focus(self) -> &'static str {
        match self {
            Self::Docked => PICTURE,
            Self::Expanded => SCROLLER,
        }
    }

    fn active(self, image: &PreviewImage) -> bool {
        matches!(self, Self::Expanded) == image.expanded.is_some()
    }

    fn set_menu(self, image: &mut PreviewImage, point: Option<PointerPress>) {
        match self {
            Self::Docked => image.menu = point,
            Self::Expanded => {
                if let Some(dialog) = &mut image.expanded {
                    dialog.menu = point;
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    CopyFullPath,
    CopyRelativePath,
    Save,
    CopyImage,
}

impl Action {
    fn key(self) -> &'static str {
        match self {
            Self::CopyFullPath => "image-copy-full-path",
            Self::CopyRelativePath => "image-copy-relative-path",
            Self::Save => "image-save",
            Self::CopyImage => "image-copy-pixels",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CopyFullPath => "Copy full path",
            Self::CopyRelativePath => "Copy relative path",
            Self::Save => "Save image",
            Self::CopyImage => "Copy image",
        }
    }

    fn enabled(self, image: &PreviewImage) -> bool {
        match self {
            Self::Save => matches!(image.save, Phase::Ready | Phase::SaveError(_)),
            Self::CopyImage if image.failed() => {
                matches!(image.save, Phase::Ready | Phase::SaveError(_))
            }
            Self::CopyImage => {
                image.invalid()
                    || image
                        .ready()
                        .is_some_and(|raster| raster.clipboard.is_some())
            }
            _ => true,
        }
    }
}

pub(super) fn target(image: El, surface: Surface) -> El {
    image.on_secondary_press(move |state: &mut State, point, tx| {
        state.open_image_menu(surface, point, tx);
    })
}

impl State {
    fn open_image_menu(&mut self, surface: Surface, point: PointerPress, tx: &mut Tx) {
        let Some(preview) = &mut self.file_preview else {
            return;
        };
        let Load::Image(image) = &mut preview.load else {
            return;
        };
        if !surface.active(image) {
            return;
        }
        let actions = surface.actions();
        if popup_menu_supported() {
            let id = preview.id;
            let menu = PopupMenu::new().items(actions.iter().map(|action| {
                PopupItem::new(action.key(), action.label()).enabled(action.enabled(image))
            }));
            tx.popup_menu(menu, point, move |state: &mut State, picked, tx| {
                if let Some(action) = actions.iter().find(|action| Some(action.key()) == picked)
                    && state
                        .file_preview
                        .as_ref()
                        .is_some_and(|preview| preview.id == id)
                {
                    state.select_image_action(surface, *action, tx);
                }
            });
        } else {
            surface.set_menu(image, Some(point));
            tx.focus_when_shown(actions[0].key(), tx.by_keys());
        }
    }

    fn close_image_menu(&mut self, surface: Surface, tx: &mut Tx) {
        if let Some(image) = self
            .preview_image_mut()
            .filter(|image| surface.active(image))
        {
            surface.set_menu(image, None);
            tx.focus_when_shown(surface.focus(), tx.by_keys());
        }
    }

    fn select_image_action(&mut self, surface: Surface, action: Action, tx: &mut Tx) {
        self.close_image_menu(surface, tx);
        let Some(preview) = &self.file_preview else {
            return;
        };
        let Load::Image(image) = &preview.load else {
            return;
        };
        if !surface.active(image) || !action.enabled(image) {
            return;
        }
        let relative = &preview.target.path;
        let full_path = || format!("{}/{}", preview.cwd.trim_end_matches(['/', '\\']), relative,);
        let windows =
            preview.cwd.starts_with("\\\\") || preview.cwd.as_bytes().get(1) == Some(&b':');
        let path = |value: &str| {
            if windows {
                value.replace('/', "\\")
            } else {
                value.to_owned()
            }
        };
        match action {
            Action::CopyFullPath => tx.copy(path(&full_path())),
            Action::CopyRelativePath => tx.copy(path(relative)),
            Action::Save => self.request_preview_asset(AssetPurpose::SaveImage),
            Action::CopyImage if image.failed() => {
                self.request_preview_asset(AssetPurpose::CopyImage);
            }
            Action::CopyImage => {
                if let Some(clipboard) = image.ready().and_then(|raster| raster.clipboard.clone()) {
                    tx.copy_rich(ClipboardContent::default().with_image(clipboard));
                } else {
                    image_notice::show(
                        self,
                        tx,
                        image_notice::COPY_ERROR,
                        image_notice::DECODE_ERROR,
                    );
                }
            }
        }
    }
}

pub(super) fn view(
    surface: Surface,
    image: &PreviewImage,
    point: PointerPress,
    bounds: Bounds,
) -> El {
    let actions = surface.actions();
    let width = match surface {
        Surface::Docked => 161.0,
        Surface::Expanded => 118.0,
    };
    let height = 8.0 + 32.0 * actions.len() as f32;
    let x = (point.x - bounds.x).clamp(0.0, (bounds.width - width).max(0.0));
    let y = (point.y - bounds.y).clamp(0.0, (bounds.height - height).max(0.0));
    let canvas = hex("#fcfcfc").expect("GTK menu canvas");
    let ink = hex("#2e3436").expect("GTK menu text");
    let highlight = hex("#eaeaea").expect("GTK menu highlight");
    col()
        .key("image-file-menu")
        .absolute(x, y)
        .size(width, height)
        .py(4.0)
        .fill(canvas)
        .role("menu")
        .label("Image actions")
        .focus_group()
        .modal_tx(true, move |state: &mut State, tx| {
            state.close_image_menu(surface, tx)
        })
        .children(actions.iter().copied().map(|action| {
            let enabled = action.enabled(image);
            row()
                .key(action.key())
                .role("menuitem")
                .label(action.label())
                .w_full()
                .h(32.0)
                .px(20.0)
                .center_y()
                .disabled(!enabled)
                .focusable(enabled)
                .focus_highlight(highlight)
                .on_press_tx(move |state: &mut State, tx| {
                    state.select_image_action(surface, action, tx)
                })
                .on_key(move |state: &mut State, event, tx| {
                    if event.key != Key::Enter {
                        return false;
                    }
                    if enabled && !tx.key_repeat() {
                        state.select_image_action(surface, action, tx);
                    }
                    true
                })
                .child(text(action.label()).font(MENU_FONT).color(if enabled {
                    ink
                } else {
                    ink.alpha(0.45)
                }))
        }))
}
