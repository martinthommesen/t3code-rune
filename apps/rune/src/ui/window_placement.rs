use std::path::Path;
use std::time::{Duration, Instant};

use rune::{App, NativeDisplayId, RunOptions, Tx, WindowHandle, WindowSpec};

use super::State;

mod document;
mod geometry;
pub(super) mod x11;

use document::{Bounds, Placement, Store};
use geometry::{Monitor, capture};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct Tracker(Option<TrackedWindow>);

impl Tracker {
    pub(super) fn poll(&mut self, tx: &mut Tx) {
        if let Some(window) = &mut self.0 {
            let now = Instant::now();
            window.observe(now, false);
            if window
                .changed_at
                .is_some_and(|changed| now.duration_since(changed) >= DEBOUNCE)
            {
                window.persist();
            }
            tx.wake_after(POLL_INTERVAL);
        }
    }

    fn flush(&mut self, closing: bool) {
        if let Some(window) = &mut self.0 {
            window.observe(Instant::now(), closing);
            window.persist();
        }
    }
}

struct TrackedWindow {
    store: Store,
    window: WindowHandle,
    displays: Vec<Monitor>,
    normal: Option<Bounds>,
    latest: Option<Placement>,
    last_content_size: Option<(u32, u32)>,
    saved: Option<Placement>,
    changed_at: Option<Instant>,
    preserve_unrestored: bool,
    initial_maximized: bool,
    opening_maximize: Option<Instant>,
    x11: bool,
}

impl TrackedWindow {
    fn observe(&mut self, now: Instant, closing: bool) {
        let Some(mut placement) = self.window.placement() else {
            return;
        };
        if closing && self.x11 {
            // The window manager can clear the maximized flag as it unmaps the window.
            if !placement.maximized
                && self.latest.is_some_and(|latest| latest.maximized)
                && self.last_content_size == Some(placement.content_size)
            {
                return;
            }
            // The server can withdraw the frame before Winit updates its insets.
            let Some(position) = x11::frame_origin(&self.window) else {
                return;
            };
            placement.position = position;
        }
        let Some(current) = capture(placement, self.normal, &self.displays) else {
            return;
        };
        self.last_content_size = Some(placement.content_size);
        self.normal = Some(current.bounds);
        if let Some(deadline) = self.opening_maximize {
            if !closing && !current.maximized && now < deadline {
                return;
            }
            self.opening_maximize = None;
            self.initial_maximized = current.maximized;
        }
        if self.latest == Some(current) {
            return;
        }
        // An off-display saved frame remains untouched until the fallback window is changed.
        if self.latest.is_some() || current.maximized != self.initial_maximized {
            self.preserve_unrestored = false;
        }
        self.latest = Some(current);
        self.changed_at = (!self.preserve_unrestored && self.saved != self.latest).then_some(now);
    }

    fn persist(&mut self) {
        self.changed_at = None;
        if self.preserve_unrestored || self.latest == self.saved {
            return;
        }
        if let Some(current) = self.latest {
            match self.store.save(current) {
                Ok(()) => self.saved = Some(current),
                Err(error) => eprintln!("Could not save the main window placement. {error}"),
            }
        }
    }
}

pub(super) fn run(
    app: App<State>,
    rune_args: Vec<String>,
    state_dir: Option<&Path>,
) -> Result<(), String> {
    let Some(state_dir) = state_dir else {
        return app.args(rune_args).try_run();
    };
    if rune::tooling::Args::parse_from(rune_args.clone()).headless() {
        return app.args(rune_args).try_run();
    }
    let store = Store::at_state_dir(state_dir);
    let saved = match store.load() {
        Ok(placement) => placement,
        Err(error) => {
            eprintln!("Could not read the main window placement. {error}");
            return app.args(rune_args).try_run();
        }
    };
    rune::try_run_windows_with_args(RunOptions::default(), rune_args, move |displays| {
        let x11 = displays.iter().any(|display| {
            matches!(
                display.native_id(),
                Some(NativeDisplayId::X11Crtc(_) | NativeDisplayId::X11Output(_))
            )
        });
        let displays: Vec<_> = displays.iter().map(Monitor::new).collect();
        let restored = saved.and_then(|placement| {
            displays
                .iter()
                .find_map(|display| display.restore(placement.bounds))
        });
        let original_normal = saved
            .filter(|_| restored.is_some())
            .map(|placement| placement.bounds);
        let maximized = saved.is_some_and(|placement| placement.maximized);
        let maximize_after_map = maximized && x11;
        let handle = app.handle();
        let app = app
            .maximized(maximized && !maximize_after_map)
            .on_ready(move |window| {
                let Some(placement) = window.placement() else {
                    return;
                };
                // X11 needs a mapped normal frame before some window managers can restore it.
                let maximize = maximize_after_map && !placement.maximized;
                let now = Instant::now();
                let mut tracker = TrackedWindow {
                    store: store.clone(),
                    window: window.clone(),
                    displays: displays.clone(),
                    normal: original_normal,
                    latest: None,
                    last_content_size: None,
                    saved,
                    changed_at: None,
                    preserve_unrestored: saved.is_some() && restored.is_none(),
                    initial_maximized: maximized,
                    opening_maximize: maximize.then(|| now + Duration::from_secs(1)),
                    x11,
                };
                tracker.observe(now, false);
                handle.update_if(move |state, tx| {
                    state.window_placement.0 = Some(tracker);
                    if maximize {
                        tx.toggle_maximized();
                    }
                    tx.wake_after(POLL_INTERVAL);
                    false
                });
            })
            .on_close(|state, _| state.window_placement.flush(true))
            .on_quit(|state, _| state.window_placement.flush(false));
        let mut spec = WindowSpec::from_app("main", app);
        if let Some((position, size)) = restored {
            spec.options.position = Some(position);
            spec.options.physical_size = Some(size);
        }
        [spec]
    })
}
