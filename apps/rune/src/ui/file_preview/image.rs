pub(super) mod expanded;
mod menu;

use rune::{
    Align, ClipboardImage, El, Icon, Image, ImageCache, ScrollSpec, SurfaceFit, col, el, row, text,
};

use crate::connection::file_preview::AssetOutcome;
use crate::workspace_image::{self, Source};

use super::{
    CRUMB, Colors, ERROR, Layout, Load, Palette, Preview, State, binary::Phase, control, filename,
};

pub(super) const PICTURE: &str = "file-preview-image";
const TOOLBAR_HEIGHT: f32 = 40.0;
const IMAGE_INSET: f32 = 16.0;
const SCROLLBAR_GUTTER: f32 = if cfg!(target_os = "linux") { 15.0 } else { 0.0 };

macro_rules! glyph {
    ($name:literal, $body:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(
                r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#,
                $body,
                "</g>"
            ),
        }
    };
}

static MINUS: Icon = glyph!("minus", r#"<path d="M5 12h14"/>"#);
static PLUS: Icon = glyph!("plus", r#"<path d="M5 12h14M12 5v14"/>"#);
static FIT: Icon = glyph!(
    "scan",
    r#"<path d="M4 7V6a2 2 0 0 1 2-2h1m10 0h1a2 2 0 0 1 2 2v1m0 10v1a2 2 0 0 1-2 2h-1m-10 0H6a2 2 0 0 1-2-2v-1"/>"#
);
static ACTUAL: Icon = glyph!(
    "maximize",
    r#"<path d="M8 3H5a2 2 0 0 0-2 2v3m13-5h3a2 2 0 0 1 2 2v3m0 8v3a2 2 0 0 1-2 2h-3m-8 0H5a2 2 0 0 1-2-2v-3"/>"#
);

pub(super) struct PreviewImage {
    contents: Contents,
    zoom: Option<f32>,
    menu: Option<rune::PointerPress>,
    expanded: Option<Box<expanded::Dialog>>,
    pub(super) save: Phase,
}

enum Contents {
    Loading,
    Decoding(Raster),
    Ready(Raster),
    Failed,
}

#[derive(Clone)]
struct Raster {
    picture: Image,
    size: (u32, u32),
    clipboard: Option<ClipboardImage>,
}

impl PreviewImage {
    pub(super) fn new() -> Self {
        Self {
            contents: Contents::Loading,
            zoom: None,
            menu: None,
            expanded: None,
            save: Phase::Ready,
        }
    }

    pub(super) fn save_name(&self, path: &str) -> String {
        if self.expanded.is_some() {
            path.replace(['/', '\\'], "_")
        } else {
            filename(path).to_owned()
        }
    }

    pub(super) fn save_focus(&self) -> &'static str {
        if self.expanded.is_some() {
            expanded::SCROLLER
        } else {
            PICTURE
        }
    }

    pub(super) fn loading(&self) -> bool {
        matches!(self.contents, Contents::Loading)
    }

    pub(super) fn loaded(&mut self, outcome: AssetOutcome, cache: Option<&ImageCache>) {
        self.contents = match (outcome, cache) {
            (AssetOutcome::Image(Source::Png(source)), Some(cache)) => {
                let Some(clipboard) = ClipboardImage::from_png(source.clone()) else {
                    self.contents = Contents::Failed;
                    return;
                };
                Contents::Decoding(Raster {
                    picture: cache.load(source),
                    size: clipboard.size(),
                    clipboard: Some(clipboard),
                })
            }
            (AssetOutcome::Image(Source::Decoded(raster)), Some(cache)) => {
                Contents::Decoding(Raster {
                    picture: cache.rgba(raster.width, raster.height, raster.pixels),
                    size: (raster.width, raster.height),
                    clipboard: raster.clipboard,
                })
            }
            (AssetOutcome::Image(Source::Animated(animation)), Some(cache)) => {
                Contents::Decoding(Raster {
                    picture: cache.load(animation.bytes),
                    size: animation.size,
                    clipboard: animation.clipboard,
                })
            }
            _ => Contents::Failed,
        }
    }

    fn ready(&self) -> Option<&Raster> {
        match &self.contents {
            Contents::Ready(raster) => Some(raster),
            _ => None,
        }
    }
}

pub(super) fn supports(path: &str) -> bool {
    workspace_image::mime_type(path).is_some()
}

pub(in crate::ui) fn expanded_is_open(state: &State) -> bool {
    state.file_preview.as_ref().is_some_and(
        |preview| matches!(&preview.load, Load::Image(image) if image.expanded.is_some()),
    )
}

impl State {
    fn preview_image_mut(&mut self) -> Option<&mut PreviewImage> {
        match &mut self.file_preview.as_mut()?.load {
            Load::Image(image) => Some(image),
            _ => None,
        }
    }

    pub(in crate::ui) fn poll_image_preview(&mut self) -> bool {
        let Some(image) = self.preview_image_mut() else {
            return false;
        };
        let Contents::Decoding(raster) = &image.contents else {
            return false;
        };
        if !raster.picture.is_ready() {
            return false;
        }
        image.contents = if raster.picture.size().is_some() {
            Contents::Ready(raster.clone())
        } else {
            Contents::Failed
        };
        true
    }

    fn preview_zoom(&mut self, zoom: Option<f32>) {
        if let Some(image) = self.preview_image_mut() {
            image.zoom = zoom;
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Bounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Bounds {
    pub(super) fn new(width: f32, layout: Layout, writable: bool) -> Self {
        let x = layout.viewport[0] - width + 1.0;
        let y = layout.header_height + 28.0 + 12.0 + if writable { 0.0 } else { 29.0 };
        Self {
            x,
            y,
            width: (width - 1.0).max(0.0),
            height: (layout.viewport[1] - y).max(1.0),
        }
    }

    fn canvas_height(self) -> f32 {
        (self.height - TOOLBAR_HEIGHT).max(1.0)
    }
}

fn fit(bounds: Bounds, width: f32, height: f32) -> f32 {
    let room = 2.0 * IMAGE_INSET;
    1.0_f32
        .min((bounds.width - room).max(1.0) / width)
        .min((bounds.canvas_height() - room).max(1.0) / height)
}

fn content_size(bounds: Bounds, scaled: [f32; 2]) -> [f32; 2] {
    let content = scaled.map(|dimension| dimension + 2.0 * IMAGE_INSET);
    let height = bounds.canvas_height();
    let gutter = |scrolling| if scrolling { SCROLLBAR_GUTTER } else { 0.0 };
    let width = bounds.width - gutter(content[1] > height - gutter(content[0] > bounds.width));
    let height = height - gutter(content[0] > width);
    [width.max(content[0]), height.max(content[1])]
}

pub(super) fn view(
    preview: &Preview,
    image: &PreviewImage,
    bounds: Bounds,
    colors: Colors,
    palette: Palette,
) -> El {
    let surface = col()
        .key("file-preview-image-surface")
        .w_full()
        .grow(1.0)
        .min_h(0.0);
    let Some(raster) = image.ready() else {
        return surface.center().child(match image.contents {
            Contents::Failed => text("Unable to load workspace image.")
                .role("alert")
                .font(ERROR)
                .color(colors.error)
                .align(Align::Center),
            _ => text("Loading image…")
                .role("status")
                .font(CRUMB)
                .color(colors.muted),
        });
    };
    let (width, height) = raster.size;
    let scale = image
        .zoom
        .unwrap_or_else(|| fit(bounds, width as f32, height as f32));
    let scaled = [width as f32 * scale, height as f32 * scale];
    let body = content_size(bounds, scaled);
    let picture = menu::target(
        el().key(PICTURE)
            .focusable(true)
            .size(scaled[0], scaled[1])
            .role("button")
            .label(format!("Zoom {}", preview.target.path))
            .cursor(rune::Cursor::ZoomIn)
            .pixels_fit(raster.picture.pixels(), SurfaceFit::Fit)
            .on_press_tx(State::open_expanded_image)
            .on_key(|state: &mut State, event, tx| {
                if event.key != rune::Key::Enter {
                    return false;
                }
                if !tx.key_repeat() {
                    state.open_expanded_image(tx);
                }
                true
            }),
        menu::Surface::Docked,
    );
    let canvas = col()
        .w_full()
        .min_h(0.0)
        .grow(1.0)
        .key("file-preview-image-canvas")
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_xy()
        .child(col().size(body[0], body[1]).center().child(picture));
    let mut surface = surface
        .child(toolbar([width, height], scale, image.zoom, colors, palette))
        .child(canvas);
    if let Phase::SaveError(message) = &image.save {
        surface = surface.child(
            text(format!("Could not save image. {message}"))
                .role("alert")
                .font(ERROR)
                .color(colors.error)
                .padding(12.0)
                .wrap(),
        );
    }
    surface.children(
        image
            .menu
            .map(|point| menu::view(menu::Surface::Docked, image, point, bounds)),
    )
}

fn toolbar(
    [width, height]: [u32; 2],
    scale: f32,
    zoom: Option<f32>,
    colors: Colors,
    palette: Palette,
) -> El {
    let out = move |state: &mut State, _: &mut rune::Tx| {
        state.preview_zoom(Some((scale / 1.25).clamp(0.05, 8.0)));
    };
    let zoom_in = move |state: &mut State, _: &mut rune::Tx| {
        state.preview_zoom(Some((scale * 1.25).clamp(0.05, 8.0)));
    };
    let fit = |state: &mut State, _: &mut rune::Tx| state.preview_zoom(None);
    let actual = |state: &mut State, _: &mut rune::Tx| state.preview_zoom(Some(1.0));
    row()
        .key("file-preview-image-toolbar")
        .w_full()
        .h(TOOLBAR_HEIGHT)
        .px(12.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], colors.separator)
        .border_layout()
        .child(
            text(format!("{width} × {height}"))
                .font(CRUMB)
                .color(colors.muted)
                .grow(1.0),
        )
        .child(
            row()
                .gap(4.0)
                .center_y()
                .child(
                    control(
                        "file-image-zoom-out",
                        "Zoom out",
                        &MINUS,
                        colors,
                        palette,
                        out,
                    )
                    .disabled(scale <= 0.05),
                )
                .child(
                    el().w(40.0).h(16.0).center().child(
                        text(format!("{}%", (scale * 100.0).round()))
                            .font(CRUMB)
                            .color(colors.muted),
                    ),
                )
                .child(
                    control(
                        "file-image-zoom-in",
                        "Zoom in",
                        &PLUS,
                        colors,
                        palette,
                        zoom_in,
                    )
                    .disabled(scale >= 8.0),
                )
                .child(
                    control("file-image-fit", "Fit image", &FIT, colors, palette, fit)
                        .checked(zoom.is_none())
                        .fill(if zoom.is_none() {
                            colors.accent
                        } else {
                            rune::Color::TRANSPARENT
                        }),
                )
                .child(
                    control(
                        "file-image-actual",
                        "Actual image size",
                        &ACTUAL,
                        colors,
                        palette,
                        actual,
                    )
                    .checked(zoom == Some(1.0))
                    .fill(if zoom == Some(1.0) {
                        colors.accent
                    } else {
                        rune::Color::TRANSPARENT
                    }),
                ),
        )
}
