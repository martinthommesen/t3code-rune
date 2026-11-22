use rune::{WindowHandle, WindowPlacement};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, ButtonPressEvent, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, CreateWindowAux,
    EventMask, KeyButMask, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use super::super::window_placement::x11::window_id;
use super::CaptionLayout;

mod settings;

const WM_RESIZE_CURSORS: [u16; 8] = [134, 138, 136, 96, 14, 16, 12, 70];
const RESIZE_BORDER: f64 = 5.0;

pub(super) struct ResizeFrame {
    connection: RustConnection,
    window: Window,
    root: Window,
    moveresize: Atom,
    minimize: Atom,
    edges: [Window; 8],
    layout: Option<(u16, u16, u16)>,
    settings: Option<settings::Watcher>,
}

impl ResizeFrame {
    pub(super) fn install(window: &WindowHandle) -> Option<(Self, CaptionLayout)> {
        let window = window_id(window)?;
        let (connection, _) = x11rb::connect(None).ok()?;
        let root = connection.get_geometry(window).ok()?.reply().ok()?.root;
        let minimize = connection
            .intern_atom(false, b"WM_CHANGE_STATE")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let moveresize = connection
            .intern_atom(false, b"_NET_WM_MOVERESIZE")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let edges = edge_windows(&connection, window)?;
        let mut settings = settings::Watcher::install(&connection, root);
        let caption = settings
            .as_mut()
            .map(|watcher| watcher.load(&connection))
            .unwrap_or_default();
        Some((
            Self {
                connection,
                window,
                root,
                moveresize,
                minimize,
                edges,
                layout: None,
                settings,
            },
            caption,
        ))
    }

    pub(super) fn pointer(&self) -> Option<(i32, i32)> {
        let reply = self
            .connection
            .query_pointer(self.root)
            .ok()?
            .reply()
            .ok()?;
        reply
            .same_screen
            .then_some((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    pub(super) fn minimize(&self) -> Option<()> {
        const ICONIC_STATE: u32 = 3;
        self.request(self.minimize, [ICONIC_STATE, 0, 0, 0, 0])
    }

    pub(super) fn poll(
        &mut self,
        placement: WindowPlacement,
        caption: &mut CaptionLayout,
    ) -> Option<bool> {
        let layout = if placement.maximized || placement.full_screen || placement.minimized {
            None
        } else {
            Some((
                u16::try_from(placement.content_size.0).ok()?,
                u16::try_from(placement.content_size.1).ok()?,
                (RESIZE_BORDER * placement.scale).round().max(1.0) as u16,
            ))
        };
        if layout != self.layout {
            self.reposition(layout)?;
            self.layout = layout;
        }
        let mut settings_changed = false;
        for _ in 0..16 {
            let Some(event) = self.connection.poll_for_event().ok()? else {
                break;
            };
            if let Event::ButtonPress(event) = event
                && event.detail == 1
                && self.layout.is_some()
            {
                self.resize(event)?;
            }
            settings_changed |= self
                .settings
                .as_ref()
                .is_some_and(|watcher| watcher.changed(&event));
        }
        if settings_changed && let Some(settings) = &mut self.settings {
            let next = settings.load(&self.connection);
            if next != *caption {
                *caption = next;
                return Some(true);
            }
        }
        Some(false)
    }

    fn reposition(&self, layout: Option<(u16, u16, u16)>) -> Option<()> {
        if let Some((width, height, border)) = layout {
            for (window, (x, y, width, height)) in self
                .edges
                .into_iter()
                .zip(edge_rectangles(width, height, border))
            {
                self.connection
                    .configure_window(
                        window,
                        &ConfigureWindowAux::new()
                            .x(i32::from(x))
                            .y(i32::from(y))
                            .width(u32::from(width))
                            .height(u32::from(height)),
                    )
                    .ok()?;
                self.connection.map_window(window).ok()?;
            }
        } else {
            for window in self.edges {
                self.connection.unmap_window(window).ok()?;
            }
        }
        self.connection.flush().ok()
    }

    fn resize(&self, event: ButtonPressEvent) -> Option<()> {
        let direction = self.edges.iter().position(|&edge| edge == event.event)? as u32;
        let pointer = self
            .connection
            .query_pointer(self.root)
            .ok()?
            .reply()
            .ok()?;
        if !pointer.mask.contains(KeyButMask::BUTTON1) {
            return Some(());
        }
        // The InputOnly child owns this implicit grab. Release it before the WM grabs the drag.
        self.connection.ungrab_pointer(event.time).ok()?;
        self.request(
            self.moveresize,
            [
                i32::from(event.root_x) as u32,
                i32::from(event.root_y) as u32,
                direction,
                1,
                1,
            ],
        )
    }

    fn request(&self, message: Atom, values: [u32; 5]) -> Option<()> {
        self.connection
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                ClientMessageEvent::new(32, self.window, message, values),
            )
            .ok()?;
        self.connection.flush().ok()
    }
}

fn edge_windows(connection: &RustConnection, window: Window) -> Option<[Window; 8]> {
    let font = connection.generate_id().ok()?;
    connection.open_font(font, b"cursor").ok()?.check().ok()?;
    let mut edges = [0; 8];
    for (edge, glyph) in edges.iter_mut().zip(WM_RESIZE_CURSORS) {
        let cursor = connection.generate_id().ok()?;
        connection
            .create_glyph_cursor(
                cursor,
                font,
                font,
                glyph,
                glyph + 1,
                0,
                0,
                0,
                65535,
                65535,
                65535,
            )
            .ok()?
            .check()
            .ok()?;
        *edge = connection.generate_id().ok()?;
        connection
            .create_window(
                0,
                *edge,
                window,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new()
                    .event_mask(EventMask::BUTTON_PRESS)
                    .cursor(cursor),
            )
            .ok()?
            .check()
            .ok()?;
        connection.free_cursor(cursor).ok()?;
    }
    connection.close_font(font).ok()?;
    Some(edges)
}

fn edge_rectangles(width: u16, height: u16, border: u16) -> [(u16, u16, u16, u16); 8] {
    let right = width.saturating_sub(border);
    let bottom = height.saturating_sub(border);
    let middle_width = right.saturating_sub(border).max(1);
    let middle_height = bottom.saturating_sub(border).max(1);
    [
        (0, 0, border, border),
        (border, 0, middle_width, border),
        (right, 0, border, border),
        (right, border, border, middle_height),
        (right, bottom, border, border),
        (border, bottom, middle_width, border),
        (0, bottom, border, border),
        (0, border, border, middle_height),
    ]
}
