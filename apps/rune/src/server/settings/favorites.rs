use serde::Serialize;
use serde_json::Value;

use super::{Fields, nonblank, slug};

pub(super) const KEY: &str = "favorites";
pub(super) const DELTA_KEY: &str = "setModelFavorites";

#[derive(Eq, PartialEq, Serialize)]
pub(super) struct Favorite {
    provider: String,
    model: String,
}

impl Favorite {
    fn decode(input: &Value) -> Result<Self, &'static str> {
        if !input.is_object() {
            return Err("Expected object");
        }
        let provider = input
            .get("provider")
            .ok_or("Missing key\n  at [\"provider\"]")?;
        let model = input.get("model").ok_or("Missing key\n  at [\"model\"]")?;
        let provider = nonblank(provider)
            .filter(|provider| slug(provider))
            .ok_or("Expected a provider instance ID\n  at [\"provider\"]")?;
        let model = nonblank(model).ok_or("Expected a non-blank string\n  at [\"model\"]")?;
        Ok(Self {
            provider: provider.into(),
            model: model.into(),
        })
    }
}

pub(super) struct Change {
    entry: Favorite,
    favorite: bool,
}

pub(super) fn decode_changes(input: &Value) -> Result<Vec<Change>, String> {
    let changes = input
        .as_array()
        .ok_or("Expected array\n  at [\"patch\"][\"setModelFavorites\"]")?;
    changes
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let at_entry = |error: &str| {
                let (message, field) = error.split_once("\n  at ").unwrap_or((error, ""));
                format!("{message}\n  at [\"patch\"][\"setModelFavorites\"][{index}]{field}")
            };
            let entry = Favorite::decode(input).map_err(at_entry)?;
            let favorite = input
                .get("favorite")
                .ok_or_else(|| at_entry("Missing key\n  at [\"favorite\"]"))?
                .as_bool()
                .ok_or_else(|| at_entry("Expected boolean\n  at [\"favorite\"]"))?;
            Ok(Change { entry, favorite })
        })
        .collect()
}

pub(super) fn read(input: &Value) -> Result<Option<Vec<Favorite>>, &'static str> {
    if input.is_null() {
        return Ok(None);
    }
    input
        .as_array()
        .ok_or("Expected a list of model favorites")?
        .iter()
        .map(Favorite::decode)
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub(super) fn apply(stored: &mut Fields, changes: Vec<Change>) -> Result<(), String> {
    let mut current = read(stored.get(KEY).unwrap_or(&Value::Null))?.unwrap_or_default();
    for change in changes {
        if change.favorite {
            if !current.contains(&change.entry) {
                current.push(change.entry);
            }
        } else {
            current.retain(|entry| entry != &change.entry);
        }
    }
    stored.insert(
        KEY.to_owned(),
        serde_json::to_value(current).expect("serializable favorites"),
    );
    Ok(())
}

#[cfg(test)]
mod tests;
