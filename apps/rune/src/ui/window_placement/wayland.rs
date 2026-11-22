use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use rune::{App, Display, NativeDisplayId, WindowSpec, tooling::Args};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle, globals::registry_queue_init, protocol::wl_output,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_output, delegate_registry, registry_handlers};

use super::super::State;
use super::document::{Bounds, Placement};

pub(super) fn restore(
    mut app: App<State>,
    mut args: Vec<String>,
    saved: Option<Placement>,
    displays: &[Display],
) -> WindowSpec<State> {
    if let Some(saved) = saved {
        app = app.maximized(saved.maximized);
        if Args::parse_from(args.clone()).size.is_none() && fits_output(saved.bounds, displays) {
            // The compositor places the window. Pass a logical size so the first output's
            // integer buffer scale can't leak into the dimensions of a fractional-scale window.
            let Bounds { width, height, .. } = saved.bounds;
            args.extend(["--size".to_owned(), format!("{width}x{height}")]);
            app = app.args(args);
        }
    }
    WindowSpec::from_app("main", app)
}

fn fits_output(bounds: Bounds, displays: &[Display]) -> bool {
    let outputs = logical_outputs().unwrap_or_default();
    displays.iter().any(|display| {
        let Some(NativeDisplayId::Wayland(id)) = display.native_id() else {
            return false;
        };
        bounds.fits(
            outputs
                .get(&id)
                .copied()
                .unwrap_or_else(|| display.native_bounds()),
        )
    })
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
