use std::fmt::Write;
use std::path::Path;

#[derive(Clone, Copy)]
pub(super) enum CompletionShell {
    Posix,
    Fish,
    Nushell,
    PowerShell,
    Cmd,
}

impl CompletionShell {
    pub(super) fn resolve(path: &Path) -> Self {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Self::Posix;
        };
        let name = name.to_ascii_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "fish" => Self::Fish,
            "nu" => Self::Nushell,
            "pwsh" | "powershell" => Self::PowerShell,
            "cmd" => Self::Cmd,
            _ => Self::Posix,
        }
    }

    pub(super) fn wrap(&self, command: &str, sentinel: &str) -> String {
        let command = command.replace("\r\n", "\n");
        match self {
            Self::Posix => wrap_posix(&command, sentinel),
            Self::Fish => format!(
                "begin\r{}\rend; printf '\\n{sentinel}%s\\n' $status",
                command.replace('\n', "\r")
            ),
            Self::PowerShell => format!(
                "$global:LASTEXITCODE = $null; & {{\r{}\r}}; if ($null -ne $LASTEXITCODE) {{ $__t3c = $LASTEXITCODE }} elseif ($?) {{ $__t3c = 0 }} else {{ $__t3c = 1 }}; Write-Host \"{sentinel}$__t3c\"",
                command.replace('\n', "\r")
            ),
            Self::Nushell => wrap_nushell(&command, sentinel),
            Self::Cmd => unreachable!("The setup runner refuses cmd.exe before wrapping a command"),
        }
    }
}

fn append_posix(target: &mut String, character: char) {
    match character {
        '\'' => target.push_str("'\\''"),
        '\\' | '\u{0}'..='\u{1f}' | '\u{7f}' => {
            write!(target, "\\0{:03o}", u32::from(character)).unwrap();
        }
        _ => target.push(character),
    }
}

fn wrap_posix(command: &str, sentinel: &str) -> String {
    // Keep each entered line below the canonical TTY's input limit.
    const LINE_BYTES: usize = 1023;
    const APPEND: &str = "__t3_command=\"$__t3_command\"";
    let evaluate = |payload: &str| {
        format!(
            "( __t3_command=$(printf '%b_' {payload}); eval \"${{__t3_command%_}}\" ); printf '\\n{sentinel}%s\\n' \"$?\""
        )
    };
    let mut encoded = String::new();
    for character in command.chars() {
        append_posix(&mut encoded, character);
    }
    let simple = evaluate(&format!("'{encoded}'"));
    if simple.len() + 2 <= LINE_BYTES {
        return simple;
    }

    let capacity = LINE_BYTES - APPEND.len() - 4;
    let mut prefix = "__t3_command=";
    let mut lines = Vec::new();
    let mut chunk = String::new();
    let mut encoded = String::new();
    for character in command.chars() {
        encoded.clear();
        append_posix(&mut encoded, character);
        if chunk.len() + encoded.len() > capacity {
            lines.push(format!("{prefix}'{chunk}'"));
            prefix = APPEND;
            chunk.clear();
        }
        chunk.push_str(&encoded);
    }
    lines.push(format!("{prefix}'{chunk}'"));
    lines.push(format!(
        "{}; unset __t3_command",
        evaluate("\"$__t3_command\"")
    ));
    lines.join("\r ")
}

fn wrap_nushell(command: &str, sentinel: &str) -> String {
    let mut escaped = String::new();
    for character in command.chars() {
        match character {
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\u{0}'..='\u{1f}' => {
                write!(escaped, "\\u{{{:x}}}", u32::from(character)).unwrap();
            }
            _ => escaped.push(character),
        }
    }
    [
        "$env.LAST_EXIT_CODE = 0".to_owned(),
        "let __t3f = try { mktemp --tmpdir-path $env.T3CODE_WORKTREE_PATH --suffix .nu .t3-setup-XXXXXXXX } catch {|e| print -e $e.msg; null }".to_owned(),
        format!(
            concat!(
                "if $__t3f == null {{ $env.LAST_EXIT_CODE = 1 }} else {{ try {{ ",
                "[$\"rm --permanent ($__t3f | to nuon)\" \"{escaped}\"] ",
                "| str join (char newline) | save --force --raw $__t3f; ",
                "^$nu.current-exe --login --commands $\"cd $env.T3CODE_WORKTREE_PATH; source ($__t3f | to nuon)\" ",
                "}} catch {{|e| if $e != null and $e.exit_code? == null {{ print -e $e.msg }}; ",
                "if $env.LAST_EXIT_CODE == 0 {{ $env.LAST_EXIT_CODE = 1 }} }} }}",
            ),
            escaped = escaped,
        ),
        "let __t3c = $env.LAST_EXIT_CODE".to_owned(),
        "try { if $__t3f != null and ($__t3f | path exists) { rm --permanent $__t3f } } catch {|e| print -e $e.msg }".to_owned(),
        format!("print $\"\\n{sentinel}($__t3c)\""),
    ].join("; ")
}
