use std::cell::Cell;
use std::rc::Rc;

use rune::{
    Color, Cursor, Drag, El, Icon, Key, KeyEvent, PointerPress, ScrollPos, ScrollSpec, SurfaceFit,
    TextStyle, Tx, col, el, icon, row, text,
};

use super::super::{CRUMB, Layout, Load, Palette, State};
use super::PICTURE;

const BACK: &str = "expanded-image-back";
const SCROLLER: &str = "expanded-image-canvas";
const CAPTION_SPACING: f32 = 24.0;
const BACK_TEXT: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
static ARROW: Icon = Icon {
    name: "arrow-left",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" d="m12 19-7-7 7-7M19 12H5"/>"#,
};

pub(super) struct Dialog {
    zoom: f32,
    window: [f32; 2],
    return_ring: bool,
    drag_origin: [f32; 2],
    scroll: Rc<Cell<ScrollPos>>,
    pointer: Rc<Cell<Option<PointerPress>>>,
}

impl Dialog {
    fn new(return_ring: bool) -> Self {
        Self {
            zoom: 1.0,
            window: [0.0; 2],
            return_ring,
            drag_origin: [0.0; 2],
            scroll: Rc::new(Cell::new(ScrollPos::default())),
            pointer: Rc::new(Cell::new(None)),
        }
    }
}

#[derive(Clone, Copy)]
struct Geometry {
    window: [f32; 2],
    popup: [f32; 4],
    maximum: [f32; 2],
    fitted: [f32; 2],
}

impl Geometry {
    fn new(layout: Layout, dimensions: (u32, u32)) -> Self {
        let [width, height] = layout.viewport;
        let popup_width = width * 0.92;
        let top = (layout.header_height + 24.0).min(height * 0.5);
        let popup_height = (height - top - 24.0).max(1.0).min(height * 0.92);
        let viewer_height = height - layout.header_height;
        let available_width = popup_width - if width >= 640.0 { 96.0 } else { 0.0 };
        let maximum = [
            available_width.max(1.0),
            (viewer_height * 0.86).min(viewer_height - 160.0).max(1.0),
        ];
        let natural = [dimensions.0 as f32, dimensions.1 as f32];
        let scale = 1.0_f32
            .min(maximum[0] / natural[0])
            .min(maximum[1] / natural[1]);
        Self {
            window: [width, height],
            popup: [(width - popup_width) * 0.5, top, popup_width, popup_height],
            maximum,
            fitted: natural.map(|dimension| dimension * scale),
        }
    }

    fn canvas(self, zoom: f32) -> [f32; 4] {
        let [width, height] = self.fitted.map(|dimension| dimension * zoom);
        let width = width.min(self.maximum[0]);
        let height = height.min(self.maximum[1]);
        let [x, y, w, h] = self.popup;
        [
            x + (w - width) * 0.5,
            y + (h - height - CAPTION_SPACING) * 0.5,
            width,
            height,
        ]
    }
}

impl State {
    pub(super) fn open_expanded_image(&mut self, tx: &mut Tx) {
        let Some(image) = self
            .preview_image_mut()
            .filter(|image| image.ready().is_some())
        else {
            return;
        };
        image.expanded = Some(Box::new(Dialog::new(tx.by_keys())));
        tx.focus_when_shown(BACK, tx.by_keys());
    }

    fn close_expanded_image(&mut self, tx: &mut Tx) {
        if let Some(image) = self.preview_image_mut()
            && let Some(dialog) = image.expanded.take()
        {
            tx.focus_when_shown(PICTURE, dialog.return_ring);
        }
    }

    fn active_expanded(&mut self) -> Option<&mut Dialog> {
        self.preview_image_mut()?.expanded.as_deref_mut()
    }

    fn resize_expanded(&mut self, [_, _, width, height]: [f32; 4], tx: &mut Tx) {
        let Some(dialog) = self.active_expanded() else {
            return;
        };
        if dialog.window != [width, height] {
            dialog.window = [width, height];
            dialog.zoom = 1.0;
            tx.scroll_to_instant(SCROLLER, 0.0, 0.0);
        }
    }

    fn zoom_expanded(
        &mut self,
        geometry: Geometry,
        next: f32,
        anchor: Option<PointerPress>,
        tx: &mut Tx,
    ) {
        let Some(dialog) = self.active_expanded() else {
            return;
        };
        let zoom = next.clamp(1.0, 8.0);
        if !zoom.is_finite() || dialog.zoom == zoom {
            return;
        }
        let [old_x, old_y, old_w, old_h] = geometry.canvas(dialog.zoom);
        let [new_x, new_y, _, _] = geometry.canvas(zoom);
        let anchor = anchor.unwrap_or(PointerPress {
            x: old_x + old_w * 0.5,
            y: old_y + old_h * 0.5,
        });
        let [left, top] = dialog.scroll.get().offset;
        let factor = zoom / dialog.zoom;
        let x = ((left + anchor.x - old_x) * factor - (anchor.x - new_x)).round();
        let y = ((top + anchor.y - old_y) * factor - (anchor.y - new_y)).round();
        dialog.zoom = zoom;
        tx.scroll_to_instant(SCROLLER, x, y);
    }

    fn toggle_expanded_zoom(&mut self, geometry: Geometry, tx: &mut Tx) {
        let Some(dialog) = self.active_expanded() else {
            return;
        };
        let next = if dialog.zoom > 1.0 { 1.0 } else { 2.0 };
        let point = (!tx.by_keys()).then(|| dialog.pointer.get()).flatten();
        self.zoom_expanded(geometry, next, point, tx);
    }

    fn expanded_zoom_key(&mut self, geometry: Geometry, event: &KeyEvent, tx: &mut Tx) -> bool {
        if event.mods.cmd || event.mods.ctrl || event.mods.alt || event.mods.logo {
            return false;
        }
        let Some(dialog) = self.active_expanded() else {
            return false;
        };
        let zoom = dialog.zoom;
        let (offset, max) = (dialog.scroll.get().offset, dialog.scroll.get().max);
        let pan = match event.key {
            Key::Left if max[0] > 0.0 => [-40.0, 0.0],
            Key::Right if max[0] > 0.0 => [40.0, 0.0],
            Key::Up if zoom > 1.0 => [0.0, -40.0],
            Key::Down if zoom > 1.0 => [0.0, 40.0],
            _ => [0.0, 0.0],
        };
        if pan != [0.0, 0.0] {
            tx.scroll_to_instant(SCROLLER, offset[0] + pan[0], offset[1] + pan[1]);
            return true;
        }
        let next = match &event.key {
            key if *key == Key::Enter || matches!(key, Key::Text(text) if text == " ") => {
                if !tx.key_repeat() {
                    self.toggle_expanded_zoom(geometry, tx);
                }
                return true;
            }
            Key::Text(text) if matches!(text.as_str(), "+" | "=") => zoom * 1.5,
            Key::Text(text) if text == "-" => zoom / 1.5,
            Key::Text(text) if text == "0" => 1.0,
            _ => return false,
        };
        self.zoom_expanded(geometry, next, None, tx);
        true
    }

    fn drag_expanded(&mut self, drag: &Drag, tx: &mut Tx) {
        let Some(dialog) = self.active_expanded() else {
            return;
        };
        match drag.phase {
            rune::Phase::Down => dialog.drag_origin = dialog.scroll.get().offset,
            rune::Phase::Move => tx.scroll_to_instant(
                SCROLLER,
                dialog.drag_origin[0] - drag.dx,
                dialog.drag_origin[1] - drag.dy,
            ),
            rune::Phase::Up => {}
        }
    }
}

pub(in crate::ui) fn view(state: &State, layout: Layout, palette: Palette) -> Option<El> {
    let preview = state.file_preview.as_ref()?;
    let Load::Image(image) = &preview.load else {
        return None;
    };
    let dialog = image.expanded.as_deref()?;
    let raster = image.ready()?;
    let geometry = Geometry::new(layout, raster.png.size());
    let zoom = if dialog.window == geometry.window {
        dialog.zoom
    } else {
        1.0
    };
    let [x, y, width, height] = geometry.canvas(zoom);
    let [picture_width, picture_height] = geometry.fitted.map(|value| value * zoom);
    let offset = Rc::clone(&dialog.scroll);
    let pointer = Rc::clone(&dialog.pointer);
    let mut canvas = col()
        .key(SCROLLER)
        .size(width, height)
        .role("region")
        .label(format!("{}, zoomable image", preview.target.path))
        .focusable(true)
        .focus_ring(2.0, -2.0, palette.accent)
        .cursor(if zoom > 1.0 {
            Cursor::Grab
        } else {
            Cursor::ZoomIn
        })
        .radius(8.0)
        .clip_rounded(8.0)
        .border(1.0, palette.line.alpha(0.7))
        .shadow(25.0, 50.0, Color::BLACK.alpha(0.25))
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_xy()
        .on_scroll_offset(move |position| offset.set(position))
        .on_pointer_move_local(move |_: &mut State, point, tx| {
            pointer.set(Some(point.window()));
            tx.live_only();
        })
        .on_press_tx(move |state: &mut State, tx| state.toggle_expanded_zoom(geometry, tx))
        .on_key(move |state: &mut State, event, tx| state.expanded_zoom_key(geometry, event, tx))
        .on_wheel(move |state: &mut State, event, tx| {
            if !event.mods.ctrl || event.dy == 0.0 {
                return false;
            }
            state.zoom_expanded(
                geometry,
                zoom * (-event.dy * 0.01).exp(),
                Some(PointerPress {
                    x: x + event.x,
                    y: y + event.y,
                }),
                tx,
            );
            true
        })
        .on_magnify(move |state: &mut State, event, tx| {
            state.zoom_expanded(
                geometry,
                zoom * (1.0 + event.delta),
                Some(PointerPress {
                    x: x + event.x,
                    y: y + event.y,
                }),
                tx,
            );
            true
        })
        .child(
            el().key("expanded-image-pixels")
                .size(picture_width, picture_height)
                .role("image")
                .label(&preview.target.path)
                .pixels_fit(raster.picture.pixels(), SurfaceFit::Fit),
        );
    if zoom > 1.0 {
        canvas = canvas
            .drag_threshold(4.0)
            .on_drag(|state: &mut State, drag, tx| {
                state.drag_expanded(drag, tx);
            });
    }

    let [popup_x, popup_y, popup_width, popup_height] = geometry.popup;
    let popup = el()
        .key("expanded-image-dialog")
        .absolute(popup_x, popup_y)
        .size(popup_width, popup_height)
        .role("dialog")
        .label("Expanded image preview")
        .modal_tx(true, State::close_expanded_image)
        .child(
            el().absolute(0.0, 0.0)
                .size(popup_width, popup_height)
                .a11y_hidden()
                .on_background_press_tx(State::close_expanded_image),
        )
        .child(
            col()
                .absolute(x - popup_x, y - popup_y)
                .w(width)
                .gap(8.0)
                .items_center()
                .child(canvas)
                .child(
                    text(&preview.target.path)
                        .font(CRUMB)
                        .color(Color::WHITE.alpha(0.8))
                        .truncate(),
                ),
        )
        .child(back(1.0, 1.0));
    Some(
        el().key("expanded-image-overlay")
            .absolute(0.0, 0.0)
            .size(geometry.window[0], geometry.window[1])
            .fill(Color::BLACK.alpha(0.75))
            .on_layout_tx(State::resize_expanded)
            .child(popup),
    )
}

fn back(x: f32, y: f32) -> El {
    row()
        .key(BACK)
        .absolute(x, y)
        .size(71.0, 28.0)
        .role("button")
        .label("Back, close image preview")
        .center()
        .gap(4.0)
        .radius(8.0)
        .fill(Color::BLACK.alpha(0.65))
        .hover(Color::BLACK.alpha(0.8))
        .border(1.0, Color::WHITE.alpha(0.2))
        .focusable(true)
        .focus_ring(2.0, 0.0, Color::WHITE)
        .on_press_tx(State::close_expanded_image)
        .on_key(|state: &mut State, event, tx| {
            if event.key != Key::Enter {
                return false;
            }
            if !tx.key_repeat() {
                state.close_expanded_image(tx);
            }
            true
        })
        .child(icon(&ARROW, 14.0).color(Color::WHITE).a11y_hidden())
        .child(text("Back").font(BACK_TEXT).color(Color::WHITE))
}
