use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use super::CaptionLayout;

const SCHEMA: &str = "org.gnome.desktop.wm.preferences";
const KEY: &str = "button-layout";

pub(super) fn open() -> (Option<Watch>, CaptionLayout) {
    let watch = Watch::start();
    let current = read().unwrap_or_default();
    (watch, current)
}

fn read() -> Option<CaptionLayout> {
    let output = Command::new("gsettings")
        .args(["get", SCHEMA, KEY])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(std::str::from_utf8(&output.stdout).ok()?)
}

fn parse(value: &str) -> Option<CaptionLayout> {
    let value = value.trim().strip_prefix('\'')?.strip_suffix('\'')?;
    Some(CaptionLayout::from_desktop_order(value))
}

pub(super) struct Watch {
    child: Child,
    pending: Arc<Mutex<Option<CaptionLayout>>>,
}

impl Watch {
    fn start() -> Option<Self> {
        let child = Command::new("gsettings")
            .args(["monitor", SCHEMA, KEY])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut watch = Self {
            child,
            pending: Arc::default(),
        };
        let stdout = watch.child.stdout.take()?;
        let pending = Arc::clone(&watch.pending);
        std::thread::Builder::new()
            .name("t3-wayland-caption".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(layout) = event_layout(&line)
                        && let Ok(mut pending) = pending.lock()
                    {
                        *pending = Some(layout);
                    }
                }
            })
            .ok()?;
        Some(watch)
    }

    pub(super) fn latest(&self) -> Option<CaptionLayout> {
        self.pending.try_lock().ok()?.take()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn event_layout(line: &str) -> Option<CaptionLayout> {
    let (key, value) = line.split_once(':')?;
    if key != KEY {
        return None;
    }
    parse(value)
}

#[cfg(test)]
mod tests {
    use super::super::super::CaptionButton::{Close, Maximize};
    use super::*;

    #[test]
    fn live_events_keep_explicit_empty_and_split_layouts() {
        let split = event_layout("button-layout: 'close:maximize'").unwrap();
        assert!(split.leading == [Close]);
        assert!(split.trailing == [Maximize]);

        let empty = event_layout("button-layout: ':'").unwrap();
        assert!(empty.leading.is_empty() && empty.trailing.is_empty());
        assert!(event_layout("other-setting: 'close:'").is_none());
        assert!(event_layout("button-layout: malformed").is_none());
    }
}
