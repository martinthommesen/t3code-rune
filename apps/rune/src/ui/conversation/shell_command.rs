use std::borrow::Cow;

pub(super) fn heading(input: &str) -> String {
    let trimmed = input.trim();
    visible_controls(&shell_body(trimmed).map_or_else(|| Cow::Borrowed(trimmed), Cow::Owned))
}

pub(super) fn visible_controls(input: &str) -> String {
    input
        .chars()
        .map(|character| match character {
            '\0'..='\x08' | '\x0b' | '\x0c' | '\x0e'..='\x1f' => {
                char::from_u32(0x2400 + u32::from(character)).expect("control picture")
            }
            '\x7f' => '␡',
            _ => character,
        })
        .collect()
}

pub(super) fn program(input: &str) -> Option<String> {
    let compact = heading(input);
    let token = compact.split_whitespace().next()?.trim_matches(['\'', '"']);
    let name = basename(token);
    (!matches!(
        name,
        "" | "for"
            | "while"
            | "until"
            | "if"
            | "case"
            | "function"
            | "foreach"
            | "begin"
            | "repeat"
            | "switch"
            | "cd"
            | "test"
            | "true"
            | "false"
    ) && name
        .chars()
        .all(|ch| ch.is_alphanumeric() || "._-".contains(ch)))
    .then(|| name.to_owned())
}

pub(super) fn language(input: &str) -> &'static str {
    let executable = input.split_whitespace().next().unwrap_or_default();
    let executable = basename(executable.trim_matches(['\'', '"'])).to_ascii_lowercase();
    match executable.trim_end_matches(".exe") {
        "powershell" | "pwsh" => "powershell",
        _ => "bash",
    }
}

fn shell_body(input: &str) -> Option<String> {
    let tokens = tokenize(input)?;
    let executable = basename(tokens.first()?);
    let shell = executable.trim_end_matches(".exe");
    let mut flags = tokens[1..].iter().peekable();
    while let Some(flag) = flags.next() {
        let script = match shell {
            "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" | "fish" => match flag.as_str() {
                "--" => return None,
                "-o" | "-O" | "--rcfile" | "--init-file" => {
                    flags.next()?;
                    continue;
                }
                "--command" => true,
                _ => {
                    let letters = flag.strip_prefix('-')?;
                    letters.contains('c') && letters.bytes().all(|byte| byte.is_ascii_alphabetic())
                }
            },
            "cmd" => flag.eq_ignore_ascii_case("/c"),
            "powershell" | "pwsh" => {
                matches!(flag.to_ascii_lowercase().as_str(), "-c" | "-command")
            }
            _ => return None,
        };
        if script {
            let body = flags.next()?.trim();
            return (flags.peek().is_none() && !body.is_empty()).then(|| body.to_owned());
        }
    }
    None
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or_default()
}

fn tokenize(input: &str) -> Option<Vec<String>> {
    let mut output = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut has_word = false;
    let mut input = input.chars();
    while let Some(character) = input.next() {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => {
                quote = Some(character);
                has_word = true;
            }
            (None | Some('"'), '\\') => {
                let escaped = input.next()?;
                if quote == Some('"') && !matches!(escaped, '"' | '\\' | '$' | '`' | '\n') {
                    word.push(character);
                }
                if escaped != '\n' {
                    word.push(escaped);
                }
                has_word = true;
            }
            (None, '\n' | '\r' | ';' | '&' | '|' | '<' | '>' | '(' | ')') => return None,
            (None, space) if space.is_whitespace() => {
                if has_word {
                    output.push(std::mem::take(&mut word));
                    has_word = false;
                }
            }
            _ => {
                word.push(character);
                has_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_word {
        output.push(word);
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_the_recorded_script_and_preserves_outer_shell_structure() {
        assert_eq!(
            heading("/bin/bash -lc 'cat package.json && cat tsconfig.json'"),
            "cat package.json && cat tsconfig.json"
        );
        assert_eq!(
            heading("bash -c 'echo $1' shell value"),
            "bash -c 'echo $1' shell value"
        );
        assert_eq!(heading("bash -c 'date' | wc"), "bash -c 'date' | wc");
        assert_eq!(heading("echo \x1b"), "echo ␛");
        assert_eq!(
            program("zsh -lc 'for i in 1 2; do printf %s $i; done'"),
            None
        );
        assert_eq!(program("zsh -lc 'node -e test'"), Some("node".into()));
    }
}
