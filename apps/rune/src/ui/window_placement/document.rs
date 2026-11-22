use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::client_settings::{read_document, resolve_file_target, write_document};

const FILE_NAME: &str = "desktop-settings.json";
const BOUNDS: &str = "mainWindowBounds";
const MAXIMIZED: &str = "mainWindowMaximized";
const MIN_WIDTH: i64 = 400;
const MIN_HEIGHT: i64 = 360;
const DISPLAY_TOLERANCE: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Bounds {
    pub(super) x: i64,
    pub(super) y: i64,
    pub(super) width: i64,
    pub(super) height: i64,
}

impl Bounds {
    fn decode(value: &Value) -> Option<Self> {
        let number = |key: &str| {
            let value = value.get(key)?;
            value.as_i64().or_else(|| {
                let value = value.as_f64()?;
                (value.fract() == 0.0 && value >= i64::MIN as f64 && value < i64::MAX as f64)
                    .then_some(value as i64)
            })
        };
        Self::new(
            number("x")?,
            number("y")?,
            number("width")?,
            number("height")?,
        )
    }

    pub(super) fn new(x: i64, y: i64, width: i64, height: i64) -> Option<Self> {
        (width >= MIN_WIDTH && height >= MIN_HEIGHT).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub(super) fn fits(self, display: [f64; 4]) -> bool {
        let [x, y, width, height] = display;
        self.x as f64 >= x - DISPLAY_TOLERANCE
            && self.y as f64 >= y - DISPLAY_TOLERANCE
            && self.x as f64 + self.width as f64 <= x + width + DISPLAY_TOLERANCE
            && self.y as f64 + self.height as f64 <= y + height + DISPLAY_TOLERANCE
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placement {
    pub(super) bounds: Bounds,
    pub(super) maximized: bool,
}

impl Placement {
    fn decode(document: &Map<String, Value>) -> Option<Self> {
        let bounds = Bounds::decode(document.get(BOUNDS)?)?;
        let maximized = document
            .get(MAXIMIZED)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(Self { bounds, maximized })
    }
}

#[derive(Clone)]
pub(super) struct Store(PathBuf);

impl Store {
    pub(super) fn at_state_dir(state_dir: &Path) -> Self {
        Self(state_dir.join(FILE_NAME))
    }

    pub(super) fn load(&self) -> io::Result<Option<Placement>> {
        Ok(Placement::decode(&read_document(&self.0)?))
    }

    pub(super) fn save(&self, placement: Placement) -> io::Result<()> {
        let path = resolve_file_target(&self.0)?;
        let mut document = read_document(&path)?;
        if Placement::decode(&document) == Some(placement) {
            return Ok(());
        }
        document.insert(BOUNDS.to_owned(), serde_json::to_value(placement.bounds)?);
        if placement.maximized {
            document.insert(MAXIMIZED.to_owned(), Value::Bool(true));
        } else {
            document.remove(MAXIMIZED);
        }
        write_document(&path, &document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unrelated_properties_and_omits_a_cleared_maximize_flag() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{
            "mainWindowBounds": {"x":40.0,"y":70,"width":1100,"height":780},
            "mainWindowMaximized": true,
            "updateChannel": "nightly",
            "future": {"setting":1},
        }"#,
        )
        .unwrap();
        let store = Store::at_state_dir(root.path());
        let mut placement = store.load().unwrap().unwrap();
        assert_eq!(placement.bounds.x, 40);
        assert!(placement.maximized);
        placement.maximized = false;
        store.save(placement).unwrap();
        assert_eq!(store.load().unwrap(), Some(placement));
        let document = read_document(&path).unwrap();
        assert!(!document.contains_key(MAXIMIZED));
        assert_eq!(document["updateChannel"], "nightly");
        assert_eq!(document["future"]["setting"], 1);
    }
}
