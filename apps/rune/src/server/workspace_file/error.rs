use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::Input;

pub(super) enum Failure {
    OutsideWorkspace,
    Escapes {
        path: PathBuf,
        root: PathBuf,
    },
    NotFile(PathBuf),
    Binary(PathBuf),
    Operation {
        action: &'static str,
        target: PathBuf,
        at: PathBuf,
        source: io::Error,
    },
}

impl Failure {
    pub(super) fn operation(
        action: &'static str,
        target: &Path,
        at: &Path,
        source: io::Error,
    ) -> Self {
        Self::Operation {
            action,
            target: target.to_owned(),
            at: at.to_owned(),
            source,
        }
    }

    pub(super) fn wire(self, input: &Input) -> Value {
        let Input { cwd, path, .. } = input;
        let cause = |name: &str, message: String| json!({"name": name, "message": message});
        let mut error = json!({
            "_tag": "ProjectReadFileError",
            "cwd": cwd,
            "relativePath": path,
            "message": format!("Failed to read workspace file '{path}' in '{cwd}'."),
        });
        let (kind, detail) = match self {
            Self::OutsideWorkspace => (
                "workspace_path_outside_root",
                cause(
                    "WorkspacePathOutsideRootError",
                    format!("Workspace file path must be relative to the project root: {path}"),
                ),
            ),
            Self::Escapes {
                path: resolved,
                root,
            } => {
                let resolved = wire_path(&resolved);
                error["resolvedPath"] = json!(resolved);
                error["resolvedWorkspaceRoot"] = json!(wire_path(&root));
                (
                    "resolved_path_outside_root",
                    cause(
                        "WorkspaceFilePathEscapeError",
                        format!(
                            "Workspace file '{path}' resolves outside workspace root '{cwd}': {resolved}"
                        ),
                    ),
                )
            }
            Self::NotFile(resolved) => {
                let resolved = wire_path(&resolved);
                error["resolvedPath"] = json!(resolved);
                (
                    "path_not_file",
                    cause(
                        "WorkspacePathNotFileError",
                        format!("Workspace path '{path}' in '{cwd}' is not a file: {resolved}"),
                    ),
                )
            }
            Self::Binary(resolved) => {
                error["resolvedPath"] = json!(wire_path(&resolved));
                (
                    "binary_file",
                    cause(
                        "WorkspaceBinaryFileError",
                        format!(
                            "Workspace file '{path}' in '{cwd}' is binary and cannot be previewed as text."
                        ),
                    ),
                )
            }
            Self::Operation {
                action,
                target,
                at,
                source,
            } => {
                let target = wire_path(&target);
                let at = wire_path(&at);
                error["resolvedPath"] = json!(target);
                error["operation"] = json!(action);
                error["operationPath"] = json!(at);
                let mut detail = cause(
                    "WorkspaceFileSystemOperationError",
                    format!(
                        "Workspace file operation '{action}' failed at '{at}' for resolved path '{target}' (requested as '{path}' in '{cwd}')."
                    ),
                );
                let syscall = if action.starts_with("realpath-") {
                    "realpath"
                } else {
                    action
                };
                detail["cause"] = os_defect(syscall, &at, &source);
                ("operation_failed", detail)
            }
        };
        error["failure"] = json!(kind);
        error["cause"] = detail;
        error
    }
}

fn wire_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        if let Some(network) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{network}");
        }
        if let Some(local) = path.strip_prefix(r"\\?\") {
            return local.to_owned();
        }
    }
    path.into_owned()
}

fn os_defect(syscall: &str, path: &str, source: &io::Error) -> Value {
    let error = match source.kind() {
        io::ErrorKind::NotFound => "ENOENT: no such file or directory",
        io::ErrorKind::NotADirectory => "ENOTDIR: not a directory",
        io::ErrorKind::InvalidFilename => "ENAMETOOLONG: name too long",
        io::ErrorKind::Interrupted => "EINTR: interrupted system call",
        io::ErrorKind::OutOfMemory => "ENOMEM: not enough memory",
        io::ErrorKind::PermissionDenied => "EACCES: permission denied",
        io::ErrorKind::InvalidInput => "EINVAL: invalid argument",
        _ if matches!(
            (std::env::consts::OS, source.raw_os_error()),
            ("linux", Some(40)) | ("macos", Some(62)) | ("windows", Some(1921))
        ) =>
        {
            "ELOOP: too many symbolic links encountered"
        }
        _ => return json!({"name": "Error", "message": format!("{source}, {syscall} '{path}'")}),
    };
    json!({"name": "Error", "message": format!("{error}, {syscall} '{path}'")})
}
