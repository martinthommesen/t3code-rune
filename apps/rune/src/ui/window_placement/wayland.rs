use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

use rune::{App, Display, NativeDisplayId, WindowHandle, WindowSpec, tooling::Args};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle, globals::registry_queue_init, protocol::wl_output,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_output, delegate_registry, registry_handlers};

use super::super::State;
use super::document::{Bounds, Placement, Store};
use super::{Geometry, POLL_INTERVAL, TrackedWindow};

pub(super) fn restore(
    mut app: App<State>,
    mut args: Vec<String>,
    store: Store,
    saved: Option<Placement>,
    displays: &[Display],
) -> WindowSpec<State> {
    let outputs = output_bounds(displays);
    let restorable = saved.filter(|saved| outputs.iter().any(|output| saved.bounds.fits(*output)));
    let explicit_size = Args::parse_from(args.clone()).size;
    if explicit_size.is_none()
        && let Some(restored) = restorable
    {
        // Let the compositor apply its actual scale to the saved logical size.
        let Bounds { width, height, .. } = restored.bounds;
        args.extend(["--size".to_owned(), format!("{width}x{height}")]);
        app = app.args(args);
    }
    let maximized = saved.is_some_and(|saved| saved.maximized);
    let anchor = saved.map(|saved| saved.bounds);
    let normal = if let Some((width, height)) = explicit_size {
        compatible_bounds((f64::from(width), f64::from(height)), anchor, &outputs)
    } else {
        restorable.map(|saved| saved.bounds)
    };
    let preserve_unrestored = saved.is_some() && (restorable.is_none() || explicit_size.is_some());
    let handle = app.handle();
    let app = app
        .maximized(maximized)
        .on_ready(move |window| {
            let now = Instant::now();
            let mut tracker = TrackedWindow {
                store: store.clone(),
                window: window.clone(),
                geometry: Geometry::Wayland(outputs.clone()),
                normal,
                latest: None,
                last_content_size: None,
                saved,
                changed_at: None,
                preserve_unrestored,
                initial_maximized: maximized,
                opening_maximize: None,
                x11: false,
            };
            tracker.observe(now, false);
            handle.update_if(move |state, tx| {
                state.window_placement.0 = Some(tracker);
                tx.wake_after(POLL_INTERVAL);
                false
            });
        })
        .on_close(|state, _| state.window_placement.flush(true))
        .on_quit(|state, _| state.window_placement.flush(false));
    WindowSpec::from_app("main", app)
}

pub(super) fn capture(
    window: &WindowHandle,
    normal: Option<Bounds>,
    saved: Option<Placement>,
    outputs: &[[f64; 4]],
) -> Option<Placement> {
    let maximized = window.is_maximized()?;
    let bounds = if maximized || window.is_full_screen()? {
        normal?
    } else {
        let anchor = normal.or_else(|| saved.map(|saved| saved.bounds));
        compatible_bounds(window.logical_content_size()?, anchor, outputs)?
    };
    Some(Placement { bounds, maximized })
}

fn compatible_bounds(
    (width, height): (f64, f64),
    anchor: Option<Bounds>,
    outputs: &[[f64; 4]],
) -> Option<Bounds> {
    let width = width.round() as i64;
    let height = height.round() as i64;
    if let Some(anchor) = anchor {
        let reused = Bounds::new(anchor.x, anchor.y, width, height)?;
        if outputs.iter().any(|output| reused.fits(*output)) {
            return Some(reused);
        }
    }
    // Wayland supplies the content size. Retain a compatible restore anchor or use a display
    // origin that the placement reader accepts. Moving the window never changes this anchor.
    outputs.iter().find_map(|output| {
        let [x, y, ..] = *output;
        let bounds = Bounds::new(x.ceil() as i64, y.ceil() as i64, width, height)?;
        bounds.fits(*output).then_some(bounds)
    })
}

fn output_bounds(displays: &[Display]) -> Vec<[f64; 4]> {
    let outputs = logical_outputs().unwrap_or_default();
    displays
        .iter()
        .filter_map(|display| {
            let Some(NativeDisplayId::Wayland(id)) = display.native_id() else {
                return None;
            };
            Some(
                outputs
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| display.native_bounds()),
            )
        })
        .collect()
}

fn logical_outputs() -> Option<HashMap<u32, [f64; 4]>> {
    // This is a separate, read-only connection. An inherited WAYLAND_SOCKET belongs to Rune.
    let mut socket = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY")?);
    if socket.is_relative() {
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
        socket = runtime.join(socket);
    }
    let connection = Connection::from_socket(UnixStream::connect(socket).ok()?).ok()?;
    let (globals, mut queue) = registry_queue_init(&connection).ok()?;
    let mut query = OutputQuery {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &queue.handle()),
    };
    queue.roundtrip(&mut query).ok()?;
    Some(
        query
            .outputs
            .outputs()
            .filter_map(|output| {
                let output = query.outputs.info(&output)?;
                let (x, y) = output.logical_position?;
                let (width, height) = output.logical_size?;
                Some((output.id, [x, y, width, height].map(f64::from)))
            })
            .collect(),
    )
}

struct OutputQuery {
    registry: RegistryState,
    outputs: OutputState,
}

impl OutputHandler for OutputQuery {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for OutputQuery {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState];
}

delegate_output!(OutputQuery);
delegate_registry!(OutputQuery);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_only_a_restore_anchor_that_fits_the_resized_wayland_window() {
        let outputs = [[0.0, 0.0, 819.0, 614.0], [-1280.0, 0.0, 1280.0, 720.0]];
        let previous = Bounds::new(61, 43, 680, 520);
        assert_eq!(
            compatible_bounds((702.4, 536.0), previous, &outputs),
            Bounds::new(61, 43, 702, 536)
        );
        assert_eq!(
            compatible_bounds((780.0, 560.0), previous, &outputs),
            Bounds::new(0, 0, 780, 560)
        );
        assert_eq!(
            compatible_bounds((930.0, 650.0), None, &outputs),
            Bounds::new(-1280, 0, 930, 650)
        );
        assert_eq!(compatible_bounds((1320.0, 760.0), previous, &outputs), None);
    }
}
