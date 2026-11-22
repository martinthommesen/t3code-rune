mod error;

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::filesystem_browse::absolute_path;
use super::thread_commands::trim_js;
use error::Failure;

const MAX_BYTES: usize = 1024 * 1024;
const MAX_PATH_LENGTH: usize = 512;

pub(in crate::server) struct Input {
    cwd: String,
    path: String,
    allow_missing: bool,
}

impl Input {
    pub(in crate::server) fn decode(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("Expected object")?;
        let invalid = |key: &str, message: &str| format!("{message}\n  at [\"{key}\"]");
        let string = |key| {
            object
                .get(key)
                .ok_or_else(|| invalid(key, "Missing key"))?
                .as_str()
                .ok_or_else(|| invalid(key, "Expected string"))
        };
        let cwd = trim_js(string("cwd")?);
        if cwd.is_empty() {
            return Err(invalid("cwd", "Expected a non-blank string"));
        }
        let path = string("relativePath")?;
        if path.is_empty() {
            return Err(invalid(
                "relativePath",
                "Expected a value with a length of at least 1",
            ));
        }
        if path.encode_utf16().count() > MAX_PATH_LENGTH {
            return Err(invalid(
                "relativePath",
                &format!("Expected a value with a length of at most {MAX_PATH_LENGTH}"),
            ));
        }
        let allow_missing = match object.get("allowMissing") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err(invalid("allowMissing", "Expected boolean | null")),
        };
        Ok(Self {
            cwd: cwd.into(),
            path: path.into(),
            allow_missing,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct FileText {
    relative_path: String,
    contents: String,
    byte_length: u64,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exists: Option<bool>,
}

impl FileText {
    fn missing(relative_path: String) -> Self {
        Self {
            relative_path,
            contents: String::new(),
            byte_length: 0,
            truncated: false,
            exists: Some(false),
        }
    }
}

pub(in crate::server) async fn read(input: Input, state_dir: PathBuf) -> Result<FileText, Value> {
    let path = host_path(&input.path).await;
    tokio::task::spawn_blocking(move || {
        read_file(&input, &path, &state_dir).map_err(|error| error.wire(&input))
    })
    .await
    .expect("workspace text reader")
}

fn read_file(input: &Input, host_path: &Path, state_dir: &Path) -> Result<FileText, Failure> {
    let (relative_path, resolved) = resolve(input, host_path)?;
    let Some(resolved) = resolved else {
        return Ok(FileText::missing(relative_path));
    };
    if super::internal_paths::protects(state_dir, &resolved) {
        return Err(Failure::OutsideWorkspace);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    if resolved.is_dir() {
        return Err(Failure::NotFile(resolved));
    }
    let file = match options.open(&resolved) {
        Ok(file) => file,
        Err(error) if input.allow_missing && error.kind() == io::ErrorKind::NotFound => {
            return Ok(FileText::missing(relative_path));
        }
        Err(error) => return Err(Failure::operation("open", &resolved, &resolved, error)),
    };
    read_text(file, resolved, relative_path)
}

fn read_text(file: File, path: PathBuf, relative_path: String) -> Result<FileText, Failure> {
    let metadata = file
        .metadata()
        .map_err(|error| Failure::operation("stat", &path, &path, error))?;
    if !metadata.is_file() {
        return Err(Failure::NotFile(path));
    }
    let byte_length = metadata.len();
    let limit = byte_length.min(MAX_BYTES as u64);
    let mut bytes = Vec::with_capacity(limit as usize);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| Failure::operation("read", &path, &path, error))?;
    let truncated = byte_length > MAX_BYTES as u64;
    let contents = decode_text(&bytes, truncated).ok_or(Failure::Binary(path))?;
    Ok(FileText {
        relative_path,
        contents: contents.into(),
        byte_length,
        truncated,
        exists: None,
    })
}

fn decode_text(bytes: &[u8], truncated: bool) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    // A preview limit can split the last UTF-8 character. Other invalid bytes reject the preview.
    let text = match str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    Some(text.strip_prefix('\u{feff}').unwrap_or(text))
}

fn resolve(input: &Input, host_path: &Path) -> Result<(String, Option<PathBuf>), Failure> {
    let cwd = PathBuf::from(&input.cwd);
    let absolute = |path: PathBuf| absolute_path(path).map_err(|_| Failure::OutsideWorkspace);
    if network_path(host_path) {
        let target = absolute(host_path.to_owned())?;
        if !within_root(&target, &absolute(cwd.clone())?) {
            return Err(Failure::OutsideWorkspace);
        }
        if host_path.is_absolute() {
            return resolve_absolute(input, &target);
        }
    }
    if host_path.is_absolute() {
        return resolve_absolute(input, host_path);
    }
    let root = absolute(cwd.clone())?;
    // POSIX backslashes are file name characters. Use the incoming path for relative names.
    let target = absolute(root.join(&input.path))?;
    let relative = target
        .strip_prefix(&root)
        .map_err(|_| Failure::OutsideWorkspace)?;
    if relative.as_os_str().is_empty() {
        return Err(Failure::OutsideWorkspace);
    }
    let relative = relative.to_string_lossy();
    let relative = if cfg!(windows) {
        relative.replace('\\', "/")
    } else {
        relative.into_owned()
    };
    let canonical_root = match root.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) if input.allow_missing && error.kind() == io::ErrorKind::NotFound => {
            return Ok((relative, None));
        }
        Err(error) => {
            return Err(Failure::operation(
                "realpath-workspace-root",
                &target,
                &cwd,
                error,
            ));
        }
    };
    let (canonical_target, exists) = resolve_ancestor(&target, input.allow_missing)
        .map_err(|error| Failure::operation("realpath-target", &target, &target, error))?;
    if !within_root(&canonical_target, &canonical_root) {
        return Err(Failure::Escapes {
            path: canonical_target,
            root: canonical_root,
        });
    }
    Ok((relative, exists.then_some(canonical_target)))
}

fn resolve_absolute(input: &Input, path: &Path) -> Result<(String, Option<PathBuf>), Failure> {
    match path.canonicalize() {
        Ok(canonical) => Ok((input.path.clone(), Some(canonical))),
        Err(error) if input.allow_missing && error.kind() == io::ErrorKind::NotFound => {
            Ok((input.path.clone(), None))
        }
        Err(error) => Err(Failure::operation("realpath-target", path, path, error)),
    }
}

fn resolve_ancestor(target: &Path, allow_missing: bool) -> io::Result<(PathBuf, bool)> {
    let mut candidate = target;
    loop {
        match candidate.canonicalize() {
            Ok(canonical) if candidate == target => return Ok((canonical, true)),
            Ok(canonical) => {
                let remainder = target.strip_prefix(candidate).expect("ancestor of target");
                return Ok((canonical.join(remainder), false));
            }
            Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => {
                match candidate.symlink_metadata() {
                    Err(probe) if probe.kind() == io::ErrorKind::NotFound => {
                        candidate = candidate.parent().ok_or(error)?;
                    }
                    Err(probe) => return Err(probe),
                    // An existing dangling link is a resolution error even when a missing file is allowed.
                    Ok(_) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn network_path(path: &Path) -> bool {
    matches!(
        path.as_os_str().as_encoded_bytes(),
        [b'/' | b'\\', b'/' | b'\\', ..]
    )
}

fn within_root(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let path = path.to_string_lossy().to_lowercase().replace('/', "\\");
        let root = root.to_string_lossy().to_lowercase().replace('/', "\\");
        return path == root
            || path
                .strip_prefix(root.trim_end_matches('\\'))
                .is_some_and(|part| part.starts_with('\\'));
    }
    path.starts_with(root)
}

async fn host_path(request: &str) -> PathBuf {
    if let Some(tail) = request
        .strip_prefix("~/")
        .or_else(|| request.strip_prefix("~\\"))
        && let Some(home) = std::env::home_dir()
    {
        let home_path = home.join(tail.trim_start_matches(std::path::is_separator));
        return absolute_path(home_path.clone()).unwrap_or(home_path);
    }
    if cfg!(windows)
        && let [b'/', drive, rest @ ..] = request.as_bytes()
        && drive.is_ascii_alphabetic()
        && (rest.is_empty() || rest.first() == Some(&b'/'))
    {
        let rest = request.get(3..).unwrap_or_default();
        return PathBuf::from(format!("{}:/{rest}", drive.to_ascii_uppercase() as char));
    }
    if cfg!(target_os = "linux")
        && let [drive, b':', rest @ ..] = request.as_bytes()
        && drive.is_ascii_alphabetic()
        && matches!(rest.first(), None | Some(b'/' | b'\\'))
        && !request.contains('\0')
    {
        // WSL's drive mount root is configurable. Let its path translator resolve the drive.
        let mut command = tokio::process::Command::new("wslpath");
        command
            .args(["-u", request])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(1), command.output()).await
            && output.status.success()
            && let Ok(path) = str::from_utf8(&output.stdout)
            && path.starts_with('/')
        {
            return PathBuf::from(path.strip_suffix('\n').unwrap_or(path));
        }
    }
    PathBuf::from(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_regular_relative_file_and_keeps_unresolved_ancestor_suffix() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let file = root.join("note.txt");
        std::fs::write(file, "note\n").unwrap();
        let input = Input {
            cwd: root.to_string_lossy().into_owned(),
            path: "ref/../note.txt".into(),
            allow_missing: true,
        };
        let text = read_file(&input, Path::new(&input.path), root)
            .unwrap_or_else(|error| panic!("{}", error.wire(&input)));
        assert_eq!(text.relative_path, "note.txt");
        assert_eq!(text.contents, "note\n");
        let missing = root.join("sub/missing.txt");
        let (target, exists) = resolve_ancestor(&missing, true).unwrap();
        assert_eq!(target, root.canonicalize().unwrap().join("sub/missing.txt"));
        assert!(!exists);
    }
}
