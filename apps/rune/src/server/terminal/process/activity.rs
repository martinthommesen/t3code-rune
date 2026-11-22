use std::path::Path;
use std::{fs, io};

pub(super) fn inspect(pid: u32, selected_shell: &Path, foreground: Option<i32>) -> Option<bool> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let stat = String::from_utf8_lossy(&stat);
    let (root_command, _) = identity(&stat)?;
    let selected_name = selected_shell.file_name()?.to_str()?;
    if root_command != selected_name && !interactive_shell(root_command) {
        return Some(true);
    }
    let foreground = foreground
        .and_then(|foreground| u32::try_from(foreground).ok())
        .filter(|foreground| *foreground > 0);
    if foreground.is_some_and(|foreground| foreground != pid) || has_child(pid)? {
        return Some(true);
    }
    // Unknown foreground groups do not establish that a terminal is idle.
    foreground.map(|_| false)
}

fn interactive_shell(command: &str) -> bool {
    matches!(
        command.trim_start_matches('-'),
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "mksh" | "tcsh" | "csh" | "nu"
    )
}

fn identity(stat: &str) -> Option<(&str, u32)> {
    let (_, name_and_fields) = stat.split_once('(')?;
    let (name, fields) = name_and_fields.rsplit_once(')')?;
    let parent = fields.split_ascii_whitespace().nth(1)?.parse().ok()?;
    Some((name, parent))
}

fn has_child(pid: u32) -> Option<bool> {
    for process in fs::read_dir("/proc").ok()? {
        let process = process.ok()?;
        if process
            .file_name()
            .to_str()
            .is_none_or(|name| name.parse::<u32>().is_err())
        {
            continue;
        }
        let stat = match fs::read(process.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let stat = String::from_utf8_lossy(&stat);
        if identity(&stat)?.1 == pid {
            return Some(true);
        }
    }
    Some(false)
}
