use rune::WindowHandle;

#[cfg(target_os = "linux")]
use rune::raw_window_handle::RawWindowHandle;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{ConnectionExt, Window};
#[cfg(target_os = "linux")]
use x11rb::rust_connection::RustConnection;

#[cfg(target_os = "linux")]
pub(crate) fn window_id(window: &WindowHandle) -> Option<Window> {
    window
        .with_raw_handle(|handle| match handle.as_raw() {
            RawWindowHandle::Xlib(handle) => Window::try_from(handle.window).ok(),
            RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
            _ => None,
        })
        .ok()?
}

#[cfg(target_os = "linux")]
pub(super) fn frame_origin(window: &WindowHandle) -> Option<(i32, i32)> {
    let id = window_id(window)?;
    let (connection, _) = x11rb::connect(None).ok()?;
    // A window manager may remove the frame between reading its ID and its coordinates.
    (0..2).find_map(|_| query_frame(&connection, id))
}

#[cfg(target_os = "linux")]
fn query_frame(connection: &RustConnection, mut window: Window) -> Option<(i32, i32)> {
    for _ in 0..8 {
        let info = connection.query_tree(window).ok()?.reply().ok()?;
        if info.parent == info.root {
            let position = connection
                .translate_coordinates(window, info.root, 0, 0)
                .ok()?
                .reply()
                .ok()?;
            return Some((i32::from(position.dst_x), i32::from(position.dst_y)));
        }
        window = info.parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub(super) fn frame_origin(_: &WindowHandle) -> Option<(i32, i32)> {
    None
}
