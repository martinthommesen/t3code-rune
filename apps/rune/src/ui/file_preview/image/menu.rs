use rune::color::hex;
use rune::{
    ClipboardContent, El, Family, Key, PointerPress, PopupItem, PopupMenu, TextStyle, Tx, col,
    popup_menu_supported, row, text,
};

use crate::connection::file_preview::AssetPurpose;

use super::super::{Load, State, binary::Phase};
use super::{Bounds, PICTURE, PreviewImage};

const MENU_SIZE: [f32; 2] = [161.0, 136.0];
const MENU_FONT: TextStyle = TextStyle::new(14.0, 400.0)
    .lh(20.0)
    .family(Family::Named("Cantarell, DejaVu Sans"));
const ACTIONS: [Action; 4] = [
    Action::CopyFullPath,
    Action::CopyRelativePath,
    Action::Save,
    Action::CopyImage,
];

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
            _ => true,
        }
    }
}

pub(super) fn target(image: El) -> El {
    image
        .focusable(true)
        .on_secondary_press(State::open_image_menu)
}

impl State {
    fn open_image_menu(&mut self, point: PointerPress, tx: &mut Tx) {
        let Some(preview) = &mut self.file_preview else {
            return;
        };
        let Load::Image(image) = &mut preview.load else {
            return;
        };
        if popup_menu_supported() {
            let id = preview.id;
            let menu = PopupMenu::new().items(ACTIONS.map(|action| {
                PopupItem::new(action.key(), action.label()).enabled(action.enabled(image))
            }));
            tx.popup_menu(menu, point, move |state: &mut State, picked, tx| {
                if let Some(action) = ACTIONS
                    .into_iter()
                    .find(|action| Some(action.key()) == picked)
                    && state
                        .file_preview
                        .as_ref()
                        .is_some_and(|preview| preview.id == id)
                {
                    state.select_image_action(action, tx);
                }
            });
        } else {
            image.menu = Some(point);
            tx.focus_when_shown(ACTIONS[0].key(), tx.by_keys());
        }
    }

    fn close_image_menu(&mut self, tx: &mut Tx) {
        if let Some(image) = self.preview_image_mut() {
            image.menu = None;
            tx.focus_when_shown(PICTURE, tx.by_keys());
        }
    }

    fn select_image_action(&mut self, action: Action, tx: &mut Tx) {
        self.close_image_menu(tx);
        let Some(preview) = &self.file_preview else {
            return;
        };
        let Load::Image(image) = &preview.load else {
            return;
        };
        if !action.enabled(image) {
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
            Action::CopyImage => {
                if let Some(raster) = image.ready() {
                    tx.copy_rich(ClipboardContent::default().with_image(raster.png.clone()));
                }
            }
        }
    }
}

pub(super) fn view(image: &PreviewImage, point: PointerPress, bounds: Bounds) -> El {
    let x = (point.x - bounds.x).clamp(0.0, (bounds.width - MENU_SIZE[0]).max(0.0));
    let y = (point.y - bounds.y).clamp(0.0, (bounds.height - MENU_SIZE[1]).max(0.0));
    let canvas = hex("#fcfcfc").expect("GTK menu canvas");
    let ink = hex("#2e3436").expect("GTK menu text");
    let highlight = hex("#eaeaea").expect("GTK menu highlight");
    col()
        .key("image-file-menu")
        .absolute(x, y)
        .size(MENU_SIZE[0], MENU_SIZE[1])
        .py(4.0)
        .fill(canvas)
        .role("menu")
        .label("Image actions")
        .focus_group()
        .modal_tx(true, State::close_image_menu)
        .children(ACTIONS.map(|action| {
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
                .on_press_tx(move |state: &mut State, tx| state.select_image_action(action, tx))
                .on_key(move |state: &mut State, event, tx| {
                    if event.key != Key::Enter {
                        return false;
                    }
                    if enabled && !tx.key_repeat() {
                        state.select_image_action(action, tx);
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
