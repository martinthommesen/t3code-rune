use serde::{Deserialize, Serialize};
use serde_json::Value;

mod schedule;
pub(crate) use schedule::{Unit, Wake, resolve};

pub(crate) const MAX_OPTIONS: usize = 8;
pub(crate) const MAX_MINUTES: u16 = 30 * 24 * 60;
pub(crate) const KEY: &str = "snoozePresets";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Day {
    Today,
    Tomorrow,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum Preset {
    After { minutes: u16 },
    At { day: Day, time: String },
}

pub(crate) fn decode(raw: &Value, path: &str) -> Result<Vec<Preset>, String> {
    let entries = raw
        .as_array()
        .ok_or_else(|| failure("Expected array", path))?;
    if entries.len() > MAX_OPTIONS {
        return Err(failure("Expected a value with a length of at most 8", path));
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let item_path = format!("{path}[{index}]");
            match value.get("kind").and_then(Value::as_str) {
                Some("after") => duration(value, &item_path),
                Some("at") => scheduled(value, &item_path),
                _ => Err(failure(
                    "Expected { readonly \"kind\": \"after\", ... } | { readonly \"kind\": \"at\", ... }",
                    &item_path,
                )),
            }
        })
        .collect()
}

fn duration(value: &Value, path: &str) -> Result<Preset, String> {
    let minute_path = format!("{path}[\"minutes\"]");
    let minutes = value
        .get("minutes")
        .ok_or_else(|| failure("Missing key", &minute_path))?
        .as_f64()
        .ok_or_else(|| failure("Expected number", &minute_path))?;
    if minutes.fract() != 0.0 {
        return Err(failure("Expected an integer", &minute_path));
    }
    if !(1.0..=f64::from(MAX_MINUTES)).contains(&minutes) {
        return Err(failure(
            "Expected a value between 1 and 43200",
            &minute_path,
        ));
    }
    Ok(Preset::After {
        minutes: minutes as u16,
    })
}

fn scheduled(value: &Value, path: &str) -> Result<Preset, String> {
    let day_path = format!("{path}[\"day\"]");
    let day = value
        .get("day")
        .ok_or_else(|| failure("Missing key", &day_path))?;
    let day: Day = serde_json::from_value(day.clone()).map_err(|_| {
        failure(
            "Expected \"today\" | \"tomorrow\" | \"monday\" | \"tuesday\" | \"wednesday\" | \"thursday\" | \"friday\" | \"saturday\" | \"sunday\"",
            &day_path,
        )
    })?;
    let time_path = format!("{path}[\"time\"]");
    let time = value
        .get("time")
        .ok_or_else(|| failure("Missing key", &time_path))?
        .as_str()
        .ok_or_else(|| failure("Expected string", &time_path))?;
    if !valid_time(time) {
        return Err(failure(
            r"Expected a string matching the RegExp ^([01]\d|2[0-3]):[0-5]\d$",
            &time_path,
        ));
    }
    Ok(Preset::At {
        day,
        time: time.to_owned(),
    })
}

pub(crate) fn valid_time(value: &str) -> bool {
    let [hour, hours, b':', minute, minutes] = value.as_bytes() else {
        return false;
    };
    (matches!(hour, b'0'..=b'1') && hours.is_ascii_digit()
        || *hour == b'2' && matches!(hours, b'0'..=b'3'))
        && matches!(minute, b'0'..=b'5')
        && minutes.is_ascii_digit()
}

fn failure(message: &str, path: &str) -> String {
    format!("{message}\n  at {path}")
}

pub(crate) fn defaults() -> &'static [Preset] {
    static OPTIONS: std::sync::OnceLock<Vec<Preset>> = std::sync::OnceLock::new();
    OPTIONS.get_or_init(|| {
        let settings: Value = serde_json::from_str(include_str!("server/settings/defaults.json"))
            .expect("built-in server settings");
        decode(&settings[KEY], KEY).expect("built-in snooze options")
    })
}
