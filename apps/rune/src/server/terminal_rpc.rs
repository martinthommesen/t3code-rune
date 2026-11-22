use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::store::Store;
use super::terminal::{
    OpenTerminal, TerminalError, TerminalEvent, TerminalKey, TerminalManager, present_worktree_path,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteInput {
    #[serde(flatten)]
    key: TerminalKey,
    data: String,
    #[serde(default)]
    binary: bool,
}

#[derive(Deserialize)]
struct ResizeInput {
    #[serde(flatten)]
    key: TerminalKey,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseInput {
    thread_id: String,
    terminal_id: Option<String>,
    #[serde(default)]
    delete_history: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachInput {
    #[serde(flatten)]
    key: TerminalKey,
    cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "present_worktree_path")]
    worktree_path: Option<Option<PathBuf>>,
    cols: Option<u16>,
    rows: Option<u16>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    provider_instance_id: Option<String>,
    #[serde(default)]
    restart_if_not_running: bool,
    #[serde(default)]
    resume: bool,
    max_history_bytes: Option<usize>,
}

pub(super) struct Subscription {
    pub key: TerminalKey,
    pub initial: Value,
    pub events: Option<broadcast::Receiver<TerminalEvent>>,
    pub max_history_bytes: Option<usize>,
}

pub(super) fn call(
    manager: &TerminalManager,
    store: &Store,
    tag: &str,
    payload: Value,
) -> Option<Result<Value, Value>> {
    Some(match tag {
        "terminal.open" => {
            let provider = payload.get("providerInstanceId").cloned();
            decode(payload).and_then(|mut input: OpenTerminal| {
                normalize_key(&mut input.key)?;
                reject_missing_provider(provider)?;
                authorize_open(store, &mut input)?;
                encode(manager.open(input).map_err(wire)?)
            })
        }
        "terminal.write" => decode(payload).and_then(|mut input: WriteInput| {
            normalize_key(&mut input.key)?;
            manager
                .write(&input.key, &input.data, input.binary)
                .map_err(wire)?;
            Ok(Value::Null)
        }),
        "terminal.resize" => decode(payload).and_then(|mut input: ResizeInput| {
            normalize_key(&mut input.key)?;
            manager
                .resize(&input.key, input.cols, input.rows)
                .map_err(wire)?;
            Ok(Value::Null)
        }),
        "terminal.clear" => decode(payload).and_then(|mut key: TerminalKey| {
            normalize_key(&mut key)?;
            manager.clear(&key).map_err(wire)?;
            Ok(Value::Null)
        }),
        "terminal.close" => decode(payload).and_then(|mut input: CloseInput| {
            normalize_id(&mut input.thread_id, usize::MAX)?;
            if let Some(id) = &mut input.terminal_id {
                normalize_id(id, 128)?;
            }
            manager.close(
                &input.thread_id,
                input.terminal_id.as_deref(),
                input.delete_history,
            );
            Ok(Value::Null)
        }),
        _ => return None,
    })
}

pub(super) fn subscribe(
    manager: &TerminalManager,
    store: &Store,
    tag: &str,
    payload: Value,
) -> Option<Result<Subscription, Value>> {
    Some(match tag {
        "terminal.observe" => decode(payload).and_then(|mut key: TerminalKey| {
            normalize_key(&mut key)?;
            observe(manager, key, None)
        }),
        "terminal.attach" => decode(payload).and_then(|mut input: AttachInput| {
            normalize_key(&mut input.key)?;
            attach(manager, store, input)
        }),
        _ => return None,
    })
}

pub(super) fn observe(
    manager: &TerminalManager,
    key: TerminalKey,
    max_history_bytes: Option<usize>,
) -> Result<Subscription, Value> {
    let (mut snapshot, events) = manager.observe(&key).map_err(wire)?;
    if let Some(bytes) = max_history_bytes {
        snapshot.retain_history(bytes);
    }
    Ok(Subscription {
        key,
        initial: json!({"type": "snapshot", "snapshot": encode(snapshot)?}),
        events: Some(events),
        max_history_bytes,
    })
}

fn attach(
    manager: &TerminalManager,
    store: &Store,
    input: AttachInput,
) -> Result<Subscription, Value> {
    let key = input.key;
    let max_history_bytes = input.max_history_bytes;
    if input.resume
        && let Some(event) = manager.closed_event(&key)
    {
        return Ok(Subscription {
            key,
            initial: encode(event)?,
            events: None,
            max_history_bytes,
        });
    }
    match manager.observe(&key) {
        Ok((snapshot, _)) => {
            if snapshot.is_running()
                || input.resume
                || !input.restart_if_not_running
                || input.cwd.is_none()
            {
                manager
                    .resize_optional(&key, input.cols, input.rows)
                    .map_err(wire)?;
                return observe(manager, key, max_history_bytes);
            }
        }
        Err(error) if input.cwd.is_none() => return Err(wire(error)),
        Err(_) => {}
    }
    let cwd = input
        .cwd
        .expect("a launch location is present when starting a session");
    reject_missing_provider(input.provider_instance_id.map(Value::String))?;
    let mut opening = OpenTerminal {
        key: key.clone(),
        cwd,
        worktree_path: input.worktree_path,
        cols: input.cols,
        rows: input.rows,
        env: input.env,
    };
    authorize_open(store, &mut opening)?;
    manager.open(opening).map_err(wire)?;
    observe(manager, key, max_history_bytes)
}

fn reject_missing_provider(provider: Option<Value>) -> Result<(), Value> {
    match provider {
        None => Ok(()),
        Some(Value::String(provider_instance_id)) if !provider_instance_id.trim().is_empty() => {
            Err(json!({
                "_tag": "TerminalProviderInstanceNotFoundError",
                "providerInstanceId": provider_instance_id,
                "message": "This host does not have that terminal provider instance."
            }))
        }
        Some(_) => Err(invalid("Invalid terminal provider instance.")),
    }
}

fn authorize_open(store: &Store, input: &mut OpenTerminal) -> Result<(), Value> {
    let workspace = store
        .terminal_workspace(&input.key.thread_id)
        .map_err(|_| invalid("Could not read the thread's terminal workspace."))?;
    let Some((project, worktree)) = workspace else {
        return Err(wire(TerminalError::Unknown(input.key.clone())));
    };
    constrain_launch(input, &project, worktree.as_deref())
}

fn constrain_launch(
    input: &mut OpenTerminal,
    project: &Path,
    worktree: Option<&Path>,
) -> Result<(), Value> {
    let registered_root = workspace_directory(worktree.unwrap_or(project))?;
    if let Some(Some(requested)) = &input.worktree_path
        && (worktree.is_none() || workspace_directory(requested)? != registered_root)
    {
        return Err(outside_workspace());
    }
    let cwd = workspace_directory(&input.cwd)?;
    if !cwd.starts_with(&registered_root) {
        return Err(outside_workspace());
    }
    input.cwd = cwd;
    input.worktree_path = Some(worktree.map(|_| registered_root));
    Ok(())
}

fn workspace_directory(path: &Path) -> Result<PathBuf, Value> {
    if !path.is_absolute() {
        return Err(outside_workspace());
    }
    let canonical = fs::canonicalize(path).map_err(|error| workspace_io_error(path, error))?;
    if canonical.to_str().is_none() {
        return Err(invalid("Terminal folder must be a UTF-8 path."));
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| workspace_io_error(path, error))?;
    if !metadata.is_dir() {
        return Err(wire(TerminalError::CwdNotDirectory(path.to_owned())));
    }
    Ok(canonical)
}

fn workspace_io_error(path: &Path, error: io::Error) -> Value {
    wire(if error.kind() == io::ErrorKind::NotFound {
        TerminalError::CwdNotFound(path.to_owned())
    } else {
        TerminalError::CwdInaccessible(path.to_owned(), error)
    })
}

fn outside_workspace() -> Value {
    json!({
        "_tag": "EnvironmentAuthorizationError",
        "requiredScope": "terminal:operate",
        "message": "Terminal folder is outside this thread's workspace."
    })
}

fn invalid(message: &str) -> Value {
    json!({"_tag": "InvalidRequest", "message": message})
}

fn wire(error: TerminalError) -> Value {
    if let TerminalError::Invalid(message) = error {
        return invalid(message);
    }
    error.wire()
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(|_| invalid("Invalid terminal request."))
}

fn encode<T: Serialize>(value: T) -> Result<Value, Value> {
    serde_json::to_value(value).map_err(|_| invalid("Could not encode the terminal state."))
}

fn normalize_key(key: &mut TerminalKey) -> Result<(), Value> {
    normalize_id(&mut key.thread_id, usize::MAX)?;
    normalize_id(&mut key.terminal_id, 128)
}

fn normalize_id(id: &mut String, max_units: usize) -> Result<(), Value> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.encode_utf16().count() > max_units {
        return Err(invalid("Invalid terminal or thread identifier."));
    }
    if trimmed != id {
        *id = trimmed.to_owned();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("t3-terminal-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn directory(&self, path: &str) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn open(cwd: PathBuf) -> OpenTerminal {
        OpenTerminal {
            key: TerminalKey {
                thread_id: "thread".to_owned(),
                terminal_id: "term-1".to_owned(),
            },
            cwd,
            worktree_path: None,
            cols: None,
            rows: None,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn started_terminals_keep_the_persisted_worktree_scope() {
        let workspace = Workspace::new();
        let main = workspace.directory("project");
        let worktree = workspace.directory("worktree");
        let nested = workspace.directory("worktree/crates");
        let neighbour = workspace.directory("worktree-copy/crates");

        let mut input = open(nested.clone());
        constrain_launch(&mut input, &main, Some(&worktree)).unwrap();
        assert_eq!(input.cwd, nested.canonicalize().unwrap());
        assert_eq!(
            input.worktree_path,
            Some(Some(worktree.canonicalize().unwrap()))
        );

        let mut input = open(neighbour.clone());
        assert_eq!(
            constrain_launch(&mut input, &main, Some(&worktree)).unwrap_err()["_tag"],
            "EnvironmentAuthorizationError"
        );

        let mut input = open(nested);
        input.worktree_path = Some(Some(neighbour));
        assert_eq!(
            constrain_launch(&mut input, &main, Some(&worktree)).unwrap_err()["_tag"],
            "EnvironmentAuthorizationError"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_under_a_project_cannot_launch_a_terminal_outside_it() {
        let workspace = Workspace::new();
        let project = workspace.directory("project");
        let outside = workspace.directory("outside");
        let link = project.join("linked-directory");
        std::os::unix::fs::symlink(outside, &link).unwrap();

        assert_eq!(
            constrain_launch(&mut open(link), &project, None).unwrap_err()["_tag"],
            "EnvironmentAuthorizationError"
        );
    }
}
