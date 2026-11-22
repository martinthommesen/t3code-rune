use std::fmt::Write as _;
use std::io;

use serde::Serialize;

use super::{BrowseInput, serialize_trimmed};

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(in super::super) struct BrowseError(Box<BrowseErrorPayload>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseErrorPayload {
    #[serde(rename = "_tag")]
    tag: &'static str,
    partial_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<Option<String>>,
    #[serde(flatten)]
    failure: BrowseFailure,
    message: String,
    cause: BrowseCause,
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "failure",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum BrowseFailure {
    WindowsPathUnsupported {
        platform: &'static str,
    },
    CurrentProjectRequired,
    ReadDirectoryFailed {
        #[serde(serialize_with = "serialize_trimmed")]
        parent_path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path_missing: Option<bool>,
    },
}

#[derive(Debug, Serialize)]
struct BrowseCause {
    name: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<Box<BrowseCause>>,
}

impl BrowseError {
    pub(super) fn windows_path(input: &BrowseInput, platform: &'static str) -> Self {
        Self::new(
            input,
            BrowseFailure::WindowsPathUnsupported { platform },
            BrowseCause {
                name: "WorkspaceEntriesWindowsPathUnsupportedError",
                message: format!(
                    "Windows-style workspace path '{}' is not supported on '{platform}'{}.",
                    input.partial_path,
                    cwd_context(input),
                ),
                cause: None,
            },
        )
    }

    pub(super) fn project_required(input: &BrowseInput) -> Self {
        Self::new(
            input,
            BrowseFailure::CurrentProjectRequired,
            BrowseCause {
                name: "WorkspaceEntriesCurrentProjectRequiredError",
                message: format!(
                    "A current project is required to browse relative workspace path '{}'.",
                    input.partial_path,
                ),
                cause: None,
            },
        )
    }

    pub(super) fn read_directory(
        input: &BrowseInput,
        parent_path: String,
        error: io::Error,
    ) -> Self {
        let cause = BrowseCause {
            name: "WorkspaceEntriesReadDirectoryError",
            message: format!(
                "Failed to read workspace directory '{parent_path}' while browsing '{}'{}.",
                input.partial_path,
                cwd_context(input),
            ),
            cause: Some(Box::new(directory_defect(&parent_path, &error))),
        };
        Self::new(
            input,
            BrowseFailure::ReadDirectoryFailed {
                path_missing: (error.kind() == io::ErrorKind::NotFound).then_some(true),
                parent_path,
            },
            cause,
        )
    }

    fn new(input: &BrowseInput, failure: BrowseFailure, cause: BrowseCause) -> Self {
        Self(Box::new(BrowseErrorPayload {
            tag: "FilesystemBrowseError",
            partial_path: input.partial_path.clone(),
            cwd: input.cwd.clone(),
            failure,
            message: format!(
                "Failed to browse filesystem path '{}'{}.",
                input.partial_path,
                cwd_context(input),
            ),
            cause,
        }))
    }
}

fn cwd_context(input: &BrowseInput) -> String {
    input
        .project_cwd()
        .map_or_else(String::new, |cwd| format!(" from '{cwd}'"))
}

fn directory_defect(path: &str, error: &io::Error) -> BrowseCause {
    if path.contains('\0') {
        return BrowseCause {
            name: "TypeError",
            message: format!(
                "The argument 'path' must be a string, Uint8Array, or URL without null bytes. Received {}",
                inspect_path(path),
            ),
            cause: None,
        };
    }
    let detail = match error.kind() {
        io::ErrorKind::NotFound => "ENOENT: no such file or directory".to_owned(),
        io::ErrorKind::NotADirectory => "ENOTDIR: not a directory".to_owned(),
        io::ErrorKind::InvalidFilename => "ENAMETOOLONG: name too long".to_owned(),
        io::ErrorKind::Interrupted => "EINTR: interrupted system call".to_owned(),
        io::ErrorKind::OutOfMemory => "ENOMEM: not enough memory".to_owned(),
        io::ErrorKind::InvalidInput => "EINVAL: invalid argument".to_owned(),
        _ if is_symlink_loop(error) => "ELOOP: too many symbolic links encountered".to_owned(),
        _ => error.to_string(),
    };
    BrowseCause {
        name: "Error",
        message: format!("{detail}, scandir '{path}'"),
        cause: None,
    }
}

fn is_symlink_loop(error: &io::Error) -> bool {
    // ErrorKind::FilesystemLoop is unstable.
    matches!(
        (std::env::consts::OS, error.raw_os_error()),
        ("linux" | "android", Some(40)) | ("macos", Some(62)) | ("windows", Some(1921))
    )
}

fn inspect_path(path: &str) -> String {
    let quote = if !path.contains('\'') {
        '\''
    } else if !path.contains('"') {
        '"'
    } else if !path.contains('`') && !path.contains("${") {
        '`'
    } else {
        '\''
    };
    let mut escaped = String::from(quote);
    for character in path.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{8}' => escaped.push_str("\\b"),
            '\u{b}' => escaped.push_str("\\v"),
            '\u{c}' => escaped.push_str("\\f"),
            character if character == '\\' || character == quote => {
                escaped.push('\\');
                escaped.push(character);
            }
            character if character.is_ascii_control() => {
                write!(escaped, "\\x{:02X}", u32::from(character))
                    .expect("writing to a string cannot fail");
            }
            character => escaped.push(character),
        }
    }
    escaped.push(quote);
    escaped
}
