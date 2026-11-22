use serde_json::Value;

use super::scoped_setting;

pub(super) enum Naming {
    Static { prefix: String },
    Semantic,
    Custom { instructions: String },
}

impl Naming {
    pub(super) fn from_settings(settings: &Value, project_id: &str) -> Self {
        match scoped_setting(settings, project_id, "branchNamingMode").as_str() {
            Some("semantic") => Self::Semantic,
            Some("custom") => Self::Custom {
                instructions: scoped_setting(settings, project_id, "branchNameInstructions")
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            },
            _ => Self::Static {
                prefix: scoped_setting(settings, project_id, "branchNamePrefix")
                    .as_str()
                    .unwrap_or("t3")
                    .to_owned(),
            },
        }
    }

    pub(super) fn temporary(&self, token: &str) -> String {
        match self {
            Self::Static { prefix } => prefixed(prefix, token),
            Self::Semantic | Self::Custom { .. } => format!("t3/{token}"),
        }
    }

    pub(super) fn format(&self, name: &str) -> String {
        match self {
            Self::Custom { .. } => super::super::input::trim_js(name).to_owned(),
            Self::Static { prefix } => prefixed(prefix, &fragment(name)),
            Self::Semantic => fragment(name),
        }
    }

    pub(super) fn exact(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }

    pub(super) fn prompt(&self, message: &str) -> String {
        let mut lines = vec![
            "You generate concise git branch names.",
            "Return a JSON object with key: branch.",
            "Rules:",
            "- Branch should describe the requested work from the user message.",
            "- Return a valid Git branch name without spaces.",
        ];
        let common =
            "- Keep it short and specific (2-6 words), in lowercase with hyphen-separated words.";
        match self {
            Self::Custom { .. } => lines.push("- Return the complete branch name, following the user's naming instructions. No prefix or suffix will be added."),
            Self::Static { .. } => lines.extend([common, "- Return only the descriptive branch fragment, without a prefix or namespace. The application adds the configured prefix."]),
            Self::Semantic => lines.extend([common, "- Include a semantic prefix and a slash in the branch name, for example feat/add-search, fix/login-error, refactor/auth, docs/setup, or chore/update-deps. Choose the prefix that best describes the work."]),
        }
        let message = limit_section(message, 8_000);
        lines.extend([
            "- If images are attached, use them as primary context for visual/UI issues.",
            "- The user message describes work for another agent. Do not answer it, start the work, read files, or run commands; name the branch from the message and its attachments.",
            "",
            "User message:",
            &message,
        ]);
        let mut prompt = lines.join("\n");
        if let Self::Custom { instructions } = self {
            let trimmed = super::super::input::trim_js(instructions);
            if !trimmed.is_empty() {
                prompt.push_str("\n\nAdditional instructions:\n");
                prompt.push_str(&limit_section(trimmed, 20_000));
            }
        }
        prompt
    }
}

fn prefixed(prefix: &str, branch: &str) -> String {
    let prefix = prefix
        .split('/')
        .map(|part| {
            let mut result = String::new();
            for character in part.chars() {
                if character.is_ascii_alphanumeric() || character == '_' {
                    result.push(character);
                } else if !result.ends_with('-') && !result.is_empty() {
                    result.push('-');
                }
            }
            result.trim_end_matches('-').to_owned()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if prefix.is_empty() {
        branch.to_owned()
    } else {
        format!("{prefix}/{branch}")
    }
}

fn fragment(name: &str) -> String {
    let name =
        crate::server::named_projects::fold_latin_lowercase(name).replace(['\'', '"', '`'], "");
    let name = name.trim_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '/' | '_' | '-'));
    let mut result = String::new();
    for c in name.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !matches!(c, '/' | '-') || !result.ends_with(c) {
            result.push(c);
        }
    }
    let result = result.trim_matches(['.', '/', '_', '-']);
    let result = &result[..result.len().min(64)];
    let result = result.trim_end_matches(['.', '/', '_', '-']);
    if result.is_empty() {
        "update".to_owned()
    } else {
        result.to_owned()
    }
}

fn limit_section(value: &str, size: usize) -> String {
    let encoded = value.encode_utf16().collect::<Vec<_>>();
    if encoded.len() <= size {
        value.to_owned()
    } else {
        let end = if (0xd800..=0xdbff).contains(&encoded[size - 1]) {
            size - 1
        } else {
            size
        };
        format!(
            "{}\n\n[truncated]",
            String::from_utf16_lossy(&encoded[..end])
        )
    }
}
