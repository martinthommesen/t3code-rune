use std::sync::OnceLock;

use rune::{Chord, ChordMod, KeyEvent, Tx};
use serde::Deserialize;
use serde_json::Value;

#[derive(Default)]
pub(crate) struct Keybindings(Vec<Binding>);

pub(crate) struct Binding {
    pub(crate) command: String,
    pub(crate) shortcut: Chord,
    pub(crate) condition: Option<Condition>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Condition {
    Identifier {
        name: String,
    },
    Not {
        node: Box<Condition>,
    },
    And {
        left: Box<Condition>,
        right: Box<Condition>,
    },
    Or {
        left: Box<Condition>,
        right: Box<Condition>,
    },
}

impl Condition {
    fn evaluate(&self, context: impl Fn(&str) -> bool + Copy) -> bool {
        match self {
            Self::Identifier { name } => match name.as_str() {
                "true" => true,
                "false" => false,
                flag => context(flag),
            },
            Self::Not { node } => !node.evaluate(context),
            Self::And { left, right } => left.evaluate(context) && right.evaluate(context),
            Self::Or { left, right } => left.evaluate(context) || right.evaluate(context),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBinding {
    command: String,
    shortcut: WireShortcut,
    when_ast: Option<Condition>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireShortcut {
    key: String,
    #[serde(default)]
    mod_key: bool,
    #[serde(default)]
    meta_key: bool,
    #[serde(default)]
    ctrl_key: bool,
    #[serde(default)]
    alt_key: bool,
    #[serde(default)]
    shift_key: bool,
}

impl WireShortcut {
    fn chord(self) -> Option<Chord> {
        let name = if self.key == " " { "space" } else { &self.key };
        let chord = Chord::parse(name).ok()?;
        let flags = [
            (self.mod_key, ChordMod::Primary),
            (self.meta_key, ChordMod::Super),
            (self.ctrl_key, ChordMod::Ctrl),
            (self.alt_key, ChordMod::Alt),
            (self.shift_key, ChordMod::Shift),
        ];
        Some(
            flags
                .into_iter()
                .filter(|(held, _)| *held)
                .fold(chord, |chord, (_, key)| chord.with(key)),
        )
    }
}

impl Binding {
    fn from_wire(value: &Value) -> Option<Self> {
        let binding = WireBinding::deserialize(value).ok()?;
        Some(Self {
            command: binding.command,
            shortcut: binding.shortcut.chord()?,
            condition: binding.when_ast,
        })
    }
}

impl Keybindings {
    pub(crate) fn stock() -> &'static Self {
        static DEFAULTS: OnceLock<Keybindings> = OnceLock::new();
        DEFAULTS.get_or_init(|| {
            let data: Value = serde_json::from_str(include_str!(
                "../server/server_config/keybindings/defaults.json"
            ))
            .expect("bundled shortcut defaults");
            let defaults = data["defaults"]
                .as_array()
                .expect("shortcut defaults array");
            Self(
                defaults
                    .iter()
                    .filter_map(|rule| Binding::from_wire(&rule["resolved"]))
                    .collect(),
            )
        })
    }

    pub(super) fn config_events(values: &[Value]) -> Option<Self> {
        values.iter().rev().find_map(|event| {
            if event["version"] != 1 {
                return None;
            }
            let config = match event["type"].as_str()? {
                "snapshot" => &event["config"],
                "keybindingsUpdated" => &event["payload"],
                _ => return None,
            };
            let bindings = config["keybindings"].as_array()?;
            Some(Self(
                bindings.iter().filter_map(Binding::from_wire).collect(),
            ))
        })
    }

    pub(crate) fn command<'a>(
        &'a self,
        event: &KeyEvent,
        tx: &Tx,
        context: impl Fn(&str) -> bool + Copy,
    ) -> Option<&'a str> {
        self.0.iter().rev().find_map(|binding| {
            if !binding
                .condition
                .as_ref()
                .is_none_or(|condition| condition.evaluate(context))
            {
                return None;
            }
            let chord = binding.shortcut;
            if !chord.matches_tx(event, tx) {
                return None;
            }
            Some(binding.command.as_str())
        })
    }
}
