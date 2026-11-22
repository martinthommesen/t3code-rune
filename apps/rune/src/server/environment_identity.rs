use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tempfile::{Builder, NamedTempFile};
use tokio::process::Command;
use uuid::Uuid;

pub(super) struct Identity {
    pub(super) id: String,
    pub(super) label: String,
}

impl Identity {
    pub(super) async fn local(id: String, cwd: &Path) -> Self {
        Self {
            id,
            label: environment_label(cwd).await,
        }
    }

    pub(super) fn history() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            label: "T3 Code History (read-only)".into(),
        }
    }
}

pub(super) fn load_id(state_dir: &Path) -> io::Result<String> {
    let path = state_dir.join("environment-id");
    if let Some(id) = read_id(&path)? {
        return Ok(id);
    }

    let bytes = format!("{}\n", Uuid::new_v4());
    let candidate = write_candidate(state_dir, bytes.as_bytes())?;
    link_candidate(&candidate, &path)?;
    if let Some(id) = read_id(&path)? {
        return Ok(id);
    }

    let recovery_path = state_dir.join("environment-id.recovery");
    link_candidate(&candidate, &recovery_path)?;
    let recovery = write_candidate(state_dir, &fs::read(recovery_path)?)?;
    recovery.persist(&path)?;
    read_id(&path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "The T3 Code environment ID is empty at '{}'.",
                path.display()
            ),
        )
    })
}

fn write_candidate(state_dir: &Path, value: &[u8]) -> io::Result<NamedTempFile> {
    let mut candidate = Builder::new()
        .prefix(".environment-id-")
        .tempfile_in(state_dir)?;
    candidate.write_all(value)?;
    candidate.as_file().sync_all()?;
    Ok(candidate)
}

fn link_candidate(candidate: &NamedTempFile, destination: &Path) -> io::Result<()> {
    match fs::hard_link(candidate.path(), destination) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        result => result,
    }
}

fn read_id(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(raw) => Ok(normalize(&String::from_utf8_lossy(&raw)).map(str::to_owned)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn normalize(value: &str) -> Option<&str> {
    let value = value.trim_matches(|ch| {
        matches!(ch, '\t'..='\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    });
    (!value.is_empty()).then_some(value)
}

async fn environment_label(cwd: &Path) -> String {
    if let Some(label) = std::env::var("T3CODE_ENVIRONMENT_LABEL")
        .ok()
        .as_deref()
        .and_then(normalize)
    {
        return label.to_owned();
    }
    if let Some(label) = friendly_hostname().await {
        return label;
    }
    if let Some(hostname) = hostname::get()
        .ok()
        .as_deref()
        .and_then(|name| name.to_str())
        .and_then(normalize)
    {
        let wsl = cfg!(target_os = "linux")
            && fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"));
        return if wsl {
            match std::env::var("WSL_DISTRO_NAME")
                .ok()
                .as_deref()
                .and_then(normalize)
            {
                Some(distro) => format!("{hostname} WSL ({distro})"),
                None => format!("{hostname} WSL"),
            }
        } else {
            hostname.to_owned()
        };
    }
    cwd.file_name()
        .and_then(|name| name.to_str())
        .and_then(normalize)
        .unwrap_or("T3 environment")
        .to_owned()
}

async fn friendly_hostname() -> Option<String> {
    let (executable, args): (&str, &[&str]) = match std::env::consts::OS {
        "macos" => ("scutil", &["--get", "ComputerName"]),
        "linux" => {
            if let Ok(contents) = fs::read_to_string("/etc/machine-info")
                && let Some(label) = machine_info_hostname(&contents)
            {
                return Some(label.to_owned());
            }
            ("hostnamectl", &["--pretty"])
        }
        _ => return None,
    };
    let mut command = Command::new(executable);
    command
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let result = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    if !result.status.success() {
        return None;
    }
    normalize(&String::from_utf8_lossy(&result.stdout)).map(str::to_owned)
}

fn machine_info_hostname(contents: &str) -> Option<&str> {
    let value = contents
        .lines()
        .find_map(|line| normalize(line)?.strip_prefix("PRETTY_HOSTNAME="))?;
    let value = normalize(value)?;
    normalize(
        value
            .strip_prefix('"')
            .and_then(|quoted| quoted.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|quoted| quoted.strip_suffix('\''))
            })
            .unwrap_or(value),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_persisted_t3_id_without_regenerating_it() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("environment-id");
        let original = "\u{feff} existing-profile-42\n";
        fs::write(&path, original)?;

        assert_eq!(load_id(directory.path())?, "existing-profile-42");
        assert_eq!(fs::read_to_string(path)?, original);
        Ok(())
    }

    #[test]
    fn concurrent_creators_keep_the_same_id() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| load_id(path));
            let second = scope.spawn(|| load_id(path));
            (first.join().unwrap(), second.join().unwrap())
        });
        let first = first?;
        assert_eq!(second?, first);
        assert_eq!(load_id(path)?, first);
        assert!(Uuid::parse_str(&first).is_ok());
        Ok(())
    }

    #[test]
    fn an_empty_identity_reuses_the_previous_recovery_id() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        fs::write(path.join("environment-id"), " \n")?;
        fs::write(path.join("environment-id.recovery"), "recovered-t3\n")?;

        assert_eq!(load_id(path)?, "recovered-t3");
        assert_eq!(fs::read(path.join("environment-id"))?, b"recovered-t3\n");
        assert_eq!(load_id(path)?, "recovered-t3");
        Ok(())
    }
}
