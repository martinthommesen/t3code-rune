use std::sync::OnceLock;

use rune::{Chord, ChordMod, KeyEvent, Tx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop};

#[derive(Default)]
pub(crate) struct Keybindings {
    bindings: Vec<Binding>,
    pub(crate) writable: bool,
}

pub(crate) const TERMINAL_TOGGLE: &str = "terminal.toggle";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Rule {
    pub(crate) command: String,
    pub(crate) key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) when: Option<String>,
}

pub(crate) enum Edit {
    Upsert { rule: Rule, replace: Option<Rule> },
    Remove(Rule),
}

pub(super) struct PendingEdit {
    request: String,
    operation: Uuid,
    started: Instant,
}

pub(crate) struct Binding {
    pub(crate) rule: Rule,
    pub(crate) display_keys: Vec<String>,
    shortcut: Option<Chord>,
    condition: Option<Condition>,
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
    fn expression(&self) -> String {
        match self {
            Self::Identifier { name } => name.clone(),
            Self::Not { node } => format!("!{}", node.operand()),
            Self::And { left, right } => format!("{} && {}", left.operand(), right.operand()),
            Self::Or { left, right } => format!("{} || {}", left.operand(), right.operand()),
        }
    }

    fn operand(&self) -> String {
        match self {
            Self::Identifier { .. } | Self::Not { .. } => self.expression(),
            Self::And { .. } | Self::Or { .. } => format!("({})", self.expression()),
        }
    }

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
    fn input(&self) -> String {
        let mut parts = self.modifiers();
        parts.push(match self.key.as_str() {
            " " => "space",
            "escape" => "esc",
            key => key,
        });
        parts.join("+")
    }

    fn modifiers(&self) -> Vec<&'static str> {
        [
            (self.mod_key, "mod"),
            (self.meta_key, "meta"),
            (self.ctrl_key, "ctrl"),
            (self.alt_key, "alt"),
            (self.shift_key, "shift"),
        ]
        .into_iter()
        .filter_map(|(held, name)| held.then_some(name))
        .collect()
    }

    fn display_keys(&self) -> Vec<String> {
        let is_mac = cfg!(target_os = "macos");
        let mut keys = Vec::new();
        for modifier in self.modifiers() {
            let label = match (modifier, is_mac) {
                ("mod" | "meta", true) => "⌘",
                ("mod" | "ctrl", false) => "Ctrl",
                ("ctrl", true) => "⌃",
                ("alt", true) => "⌥",
                ("alt", false) => "Alt",
                ("meta", false) => "Meta",
                _ => "⇧",
            };
            if !keys.iter().any(|key| key == label) {
                keys.push(label.to_owned());
            }
        }
        keys.push(match self.key.as_str() {
            " " => "Space".to_owned(),
            key if key.len() == 1 => key.to_ascii_uppercase(),
            key => key.to_owned(),
        });
        keys
    }

    fn chord(&self) -> Option<Chord> {
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
            rule: Rule {
                command: binding.command,
                key: binding.shortcut.input(),
                when: binding.when_ast.as_ref().map(Condition::expression),
            },
            display_keys: binding.shortcut.display_keys(),
            shortcut: binding.shortcut.chord(),
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
            Self {
                bindings: defaults
                    .iter()
                    .filter_map(|rule| Binding::from_wire(&rule["resolved"]))
                    .collect(),
                writable: false,
            }
        })
    }

    pub(crate) fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    pub(super) fn config_events(values: &[Value], writable: bool) -> Option<Self> {
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
            Some(Self {
                bindings: bindings.iter().filter_map(Binding::from_wire).collect(),
                writable,
            })
        })
    }

    pub(crate) fn command<'a>(
        &'a self,
        event: &KeyEvent,
        tx: &Tx,
        context: impl Fn(&str) -> bool + Copy,
    ) -> Option<&'a str> {
        self.bindings.iter().rev().find_map(|binding| {
            if !binding
                .condition
                .as_ref()
                .is_none_or(|condition| condition.evaluate(context))
            {
                return None;
            }
            let chord = binding.shortcut?;
            if !chord.matches_tx(event, tx) {
                return None;
            }
            Some(binding.rule.command.as_str())
        })
    }
}

impl Connection {
    pub(crate) fn edit_keybinding(&self, edit: Edit) -> Option<Uuid> {
        let operation = Uuid::new_v4();
        self.commands
            .send(Command::EditKeybinding { edit, operation })
            .ok()?;
        Some(operation)
    }
}

impl Session {
    pub(super) async fn edit_keybinding(
        &mut self,
        edit: Edit,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if !self.keybindings_writable || self.pending_keybinding.is_some() {
            let message = if self.keybindings_writable {
                "A keybinding is already being saved."
            } else {
                "This connection lacks permission to change keybindings."
            };
            settled(events, operation, Err(message.into()));
            return Ok(());
        }

        let (tag, payload) = match edit {
            Edit::Upsert { rule, replace } => {
                let mut payload = json!(rule);
                if let Some(replace) = replace {
                    payload["replace"] = json!(replace);
                }
                ("server.upsertKeybinding", payload)
            }
            Edit::Remove(rule) => ("server.removeKeybinding", json!(rule)),
        };
        let request = self.allocate_id();
        self.pending_keybinding = Some(PendingEdit {
            request: request.clone(),
            operation,
            started: Instant::now(),
        });
        self.send_request(&request, tag, payload).await
    }

    pub(super) fn keybinding_write_matches(&self, request: &str) -> bool {
        self.pending_keybinding
            .as_ref()
            .is_some_and(|pending| pending.request == request)
    }

    pub(super) fn keybinding_write_result(
        &mut self,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        if let Some(pending) = self.pending_keybinding.take() {
            // The config stream supplies the ordered catalog, including edits from other clients.
            let result = result.and_then(|value| {
                value["keybindings"]
                    .as_array()
                    .map(|_| ())
                    .ok_or_else(|| "The environment returned an invalid keybinding result.".into())
            });
            settled(events, pending.operation, result);
        }
    }

    pub(super) fn expire_keybinding_write(&mut self, events: &EventSender<impl Fn()>) {
        if self
            .pending_keybinding
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= REQUEST_TIMEOUT)
        {
            self.fail_keybinding_write(
                events,
                "The environment did not confirm the keybinding change in time.",
            );
        }
    }

    pub(super) fn fail_keybinding_write(&mut self, events: &EventSender<impl Fn()>, message: &str) {
        if let Some(pending) = self.pending_keybinding.take() {
            settled(events, pending.operation, Err(message.into()));
        }
    }
}

fn settled(events: &EventSender<impl Fn()>, operation: Uuid, result: Result<(), String>) {
    events.send(Event::KeybindingSettled { operation, result });
}
