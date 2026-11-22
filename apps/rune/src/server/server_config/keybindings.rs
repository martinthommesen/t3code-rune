use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Value, json};

const MAX_RULES: usize = 256;
const MAX_WHEN_DEPTH: usize = 64;
const INVALID_RULE: &str = "SchemaError: Invalid keybinding rule";
const LATE_DEFAULT: &str = "composer.sendBackground:mod+enter";
const BACKGROUND_COMMAND: &str = "composer.sendBackground";
const BACKGROUND_CONTEXT: &str = "composerFocus && draftThreadRoute";

#[derive(Deserialize)]
struct Asset {
    commands: Vec<String>,
    defaults: Vec<DefaultRule>,
}

#[derive(Deserialize)]
struct DefaultRule {
    raw: Value,
    resolved: Value,
}

struct Rule {
    resolved: Value,
    when: Option<String>,
}

impl Rule {
    fn command(&self) -> &str {
        self.resolved["command"]
            .as_str()
            .expect("typed keybinding command")
    }

    fn shortcut_is(&self, shortcut: &Value, when: Option<&str>) -> bool {
        self.when.as_deref() == when && &self.resolved["shortcut"] == shortcut
    }
}

fn stock() -> &'static Asset {
    static STOCK: OnceLock<Asset> = OnceLock::new();
    STOCK.get_or_init(|| {
        serde_json::from_str(include_str!("keybindings/defaults.json"))
            .expect("generated stock keybindings")
    })
}

pub(super) fn initial(path: &Path) -> Result<(Value, Value), Value> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(defaults(vec![])),
        Err(_) => {
            return Err(json!({
                "_tag": "KeybindingsConfigParseError",
                "configPath": path,
                "detail": "failed to read keybindings config",
            }));
        }
    };
    let raw = String::from_utf8_lossy(&bytes);
    let entries = match crate::server::parse_settings_json(&raw) {
        Some(Value::Array(entries)) => entries,
        Some(_) => return Ok(malformed("Expected array")),
        None => return Ok(malformed("Expected a valid JSON string")),
    };
    let mut custom = Vec::new();
    let mut issues = Vec::new();
    for (index, raw) in entries.iter().enumerate() {
        match compile(raw) {
            Ok(rule) => custom.push(rule),
            Err(message) => issues.push(json!({
                "kind": "keybindings.invalid-entry",
                "index": index,
                "message": message,
            })),
        }
    }

    if issues.is_empty() {
        backfill(path, &mut custom);
    }
    let custom = tail(custom.into_iter().map(|rule| rule.resolved).collect());
    let mut merged: Vec<_> = stock()
        .defaults
        .iter()
        .filter(|rule| {
            !custom
                .iter()
                .any(|entry| entry["command"] == rule.resolved["command"])
        })
        .map(|rule| rule.resolved.clone())
        .collect();
    merged.extend(custom);
    Ok((json!(tail(merged)), json!(issues)))
}

fn defaults(issues: Vec<Value>) -> (Value, Value) {
    let bindings: Vec<_> = stock().defaults.iter().map(|rule| &rule.resolved).collect();
    (json!(bindings), json!(issues))
}

fn malformed(detail: &str) -> (Value, Value) {
    defaults(vec![json!({
        "kind": "keybindings.malformed-config",
        "message": format!("expected JSON array (SchemaError: {detail})"),
    })])
}

fn tail<T>(mut rules: Vec<T>) -> Vec<T> {
    if rules.len() > MAX_RULES {
        rules.drain(..rules.len() - MAX_RULES);
    }
    rules
}

fn backfill(path: &Path, custom: &mut Vec<Rule>) {
    let slots = MAX_RULES.saturating_sub(custom.len());
    if slots == 0 {
        return;
    }
    let mut missing: Vec<Rule> = stock()
        .defaults
        .iter()
        .filter_map(|rule| {
            let command = rule.resolved["command"].as_str()?;
            let when = rule.raw.get("when").and_then(Value::as_str);
            let present = custom.iter().any(|entry| {
                entry.command() == command || entry.shortcut_is(&rule.resolved["shortcut"], when)
            });
            (!present).then(|| Rule {
                resolved: rule.resolved.clone(),
                when: when.map(str::to_owned),
            })
        })
        .take(slots)
        .collect();

    if missing.len() < slots && late_default_pending(path) {
        let alongside = background_rule("mod+alt+enter", BACKGROUND_CONTEXT);
        let late = background_rule("mod+enter", BACKGROUND_CONTEXT);
        let suppressed = background_rule("mod+enter", "false");
        if custom
            .iter()
            .any(|entry| entry.resolved == alongside.resolved)
            && !custom.iter().any(|entry| {
                entry.resolved == suppressed.resolved
                    || entry.shortcut_is(&late.resolved["shortcut"], late.when.as_deref())
            })
        {
            missing.push(late);
        }
    }
    missing.append(custom);
    *custom = missing;
}

fn late_default_pending(path: &Path) -> bool {
    fs::read_to_string(path.with_file_name("keybindings-migrations")).map_or(true, |contents| {
        !contents.split('\n').any(|line| line == LATE_DEFAULT)
    })
}

fn background_rule(key: &str, when: &str) -> Rule {
    compile(&json!({"command": BACKGROUND_COMMAND, "key": key, "when": when}))
        .expect("stock late composer binding")
}

fn compile(raw: &Value) -> Result<Rule, String> {
    let fields = raw.as_object().ok_or("SchemaError: Expected object")?;
    let key = string_field(fields.get("key"), "key", 64, false)?;
    let command = fields
        .get("command")
        .ok_or_else(|| field_error("Missing key", "command"))?;
    let command = command
        .as_str()
        .filter(|command| valid_command(command))
        .ok_or_else(|| command_error(command.is_string()))?;
    let when = fields
        .get("when")
        .map(|value| string_field(Some(value), "when", 256, true))
        .transpose()?;
    let shortcut = shortcut(key).ok_or(INVALID_RULE)?;
    let mut resolved = json!({"command": command, "shortcut": shortcut});
    if let Some(when) = when {
        resolved["whenAst"] = expression(when).ok_or(INVALID_RULE)?;
    }
    Ok(Rule {
        resolved,
        when: when.map(str::to_owned),
    })
}

fn valid_command(command: &str) -> bool {
    if stock().commands.iter().any(|value| value == command) {
        return true;
    }
    let Some(script) = command
        .strip_prefix("script.")
        .and_then(|tail| tail.strip_suffix(".run"))
    else {
        return false;
    };
    let valid_slug = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    script.len() <= 24
        && script.bytes().next().is_some_and(valid_slug)
        && script.bytes().all(|byte| valid_slug(byte) || byte == b'-')
}

fn command_error(is_string: bool) -> String {
    let literals = stock()
        .commands
        .iter()
        .map(|command| json!(command).to_string())
        .collect::<Vec<_>>()
        .join(" | ");
    if !is_string {
        return field_error(&format!("Expected {literals} | string"), "command");
    }
    format!(
        "{}\nExpected a string matching template literal parts\n  at [\"command\"]",
        field_error(&format!("Expected {literals}"), "command")
    )
}

fn field_error(message: &str, field: &str) -> String {
    format!("SchemaError: {message}\n  at [\"{field}\"]")
}

fn string_field<'a>(
    value: Option<&'a Value>,
    field: &str,
    limit: usize,
    optional: bool,
) -> Result<&'a str, String> {
    let text = value
        .ok_or_else(|| field_error("Missing key", field))?
        .as_str()
        .ok_or_else(|| {
            let expected = if optional {
                "Expected string | undefined"
            } else {
                "Expected string"
            };
            field_error(expected, field)
        })?
        .trim_matches(js_whitespace);
    if text.is_empty() {
        return Err(field_error(
            "Expected a value with a length of at least 1",
            field,
        ));
    }
    if text.encode_utf16().count() > limit {
        return Err(field_error(
            &format!("Expected a value with a length of at most {limit}"),
            field,
        ));
    }
    Ok(text)
}

fn js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn shortcut(value: &str) -> Option<Value> {
    let lower = value.to_lowercase();
    let mut tokens: Vec<_> = lower
        .split('+')
        .map(|token| token.trim_matches(js_whitespace))
        .collect();
    if tokens.last() == Some(&"") {
        while tokens.last() == Some(&"") {
            tokens.pop();
        }
        tokens.push("+");
    }
    let mut key = None;
    let mut meta = false;
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut platform = false;
    for token in tokens {
        match token {
            "" => return None,
            "cmd" | "meta" => meta = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" => alt = true,
            "mod" => platform = true,
            _ if key.is_some() => return None,
            "space" => key = Some(" "),
            "esc" => key = Some("escape"),
            _ => key = Some(token),
        }
    }
    Some(json!({
        "key": key?,
        "metaKey": meta,
        "ctrlKey": ctrl,
        "shiftKey": shift,
        "altKey": alt,
        "modKey": platform,
    }))
}

fn expression(input: &str) -> Option<Value> {
    let mut parser = WhenParser { remaining: input };
    let node = parser.parse_or(0)?;
    parser
        .remaining
        .trim_matches(js_whitespace)
        .is_empty()
        .then_some(node)
}

struct WhenParser<'a> {
    remaining: &'a str,
}

impl WhenParser<'_> {
    fn consume(&mut self, literal: &str) -> bool {
        self.remaining = self.remaining.trim_start_matches(js_whitespace);
        let Some(rest) = self.remaining.strip_prefix(literal) else {
            return false;
        };
        self.remaining = rest;
        true
    }

    fn parse_or(&mut self, depth: usize) -> Option<Value> {
        let mut left = self.parse_and(depth)?;
        while self.consume("||") {
            left = json!({"type": "or", "left": left, "right": self.parse_and(depth)?});
        }
        Some(left)
    }

    fn parse_and(&mut self, depth: usize) -> Option<Value> {
        let mut left = self.parse_unary(depth)?;
        while self.consume("&&") {
            left = json!({"type": "and", "left": left, "right": self.parse_unary(depth)?});
        }
        Some(left)
    }

    fn parse_unary(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_WHEN_DEPTH {
            return None;
        }
        let mut negations = 0;
        while self.consume("!") {
            negations += 1;
            if negations > MAX_WHEN_DEPTH {
                return None;
            }
        }
        let mut node = if self.consume("(") {
            let nested = self.parse_or(depth + 1)?;
            self.consume(")").then_some(nested)?
        } else {
            self.identifier()?
        };
        for _ in 0..negations {
            node = json!({"type": "not", "node": node});
        }
        Some(node)
    }

    fn identifier(&mut self) -> Option<Value> {
        let name = self.remaining;
        let first = name.bytes().next()?;
        if !first.is_ascii_alphabetic() && first != b'_' {
            return None;
        }
        let length = name
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
            .count();
        self.remaining = &name[length..];
        Some(json!({"type": "identifier", "name": &name[..length]}))
    }
}
