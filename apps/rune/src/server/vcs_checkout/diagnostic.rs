use serde::Serialize;

use super::CHECKOUT;
use super::git::Output;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FailureReason {
    AuthenticationFailed,
    BranchAlreadyExists,
    BranchCheckedOutInWorktree,
    ConfigLocked,
    HostKeyUnverified,
    NotARepository,
    PathAlreadyExists,
    RefNameConflict,
    RefNotFound,
    RemoteUnreachable,
    TagWouldBeClobbered,
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(in crate::server) struct GitCommandError(Box<ErrorFields>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorFields {
    #[serde(rename = "_tag")]
    tag: &'static str,
    operation: &'static str,
    command: &'static str,
    cwd: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    argument_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<FailureReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<Defect>,
}

#[derive(Debug, Serialize)]
struct Defect {
    name: &'static str,
    message: String,
}

impl GitCommandError {
    pub(super) fn new(
        operation: &'static str,
        command: &'static str,
        cwd: &str,
        detail: impl Into<String>,
    ) -> Self {
        Self(Box::new(ErrorFields {
            tag: "GitCommandError",
            operation,
            command,
            cwd: cwd.to_owned(),
            detail: detail.into(),
            argument_count: None,
            exit_code: None,
            stdout_length: None,
            stderr_length: None,
            reason: None,
            cause: None,
        }))
    }

    pub(super) fn not_a_repository(operation: &'static str, cwd: &str) -> Self {
        let mut error = Self::new(
            operation,
            "vcs-route",
            cwd,
            "Failed to resolve the VCS driver for this Git command.",
        );
        error.0.cause = Some(Defect {
            name: "VcsUnsupportedOperationError",
            message: format!(
                "VCS operation is unsupported for unknown in VcsDriverRegistry.resolve: No supported VCS repository was detected at {cwd}."
            ),
        });
        error
    }

    pub(super) fn process(
        operation: &'static str,
        cwd: &str,
        args: &[&str],
        detail: &'static str,
    ) -> Self {
        let mut error = Self::new(operation, "git", cwd, detail);
        error.0.argument_count = Some(args.len());
        error
    }

    pub(super) fn failed(
        operation: &'static str,
        cwd: &str,
        args: &[&str],
        output: &Output,
        detail: Option<&str>,
    ) -> Self {
        let reason = classify(&output.stderr);
        let details = detail.unwrap_or(match reason {
            Some(FailureReason::ConfigLocked) => "Another Git process kept the repository's config locked. Wait for it to finish, then retry.",
            _ => "Git command exited with a non-zero status.",
        });
        let mut error = Self::new(operation, "git", cwd, details);
        error.0.argument_count = Some(args.len());
        error.0.exit_code = output.status.code();
        error.0.stdout_length = Some(output.stdout.encode_utf16().count());
        error.0.stderr_length = Some(output.stderr.encode_utf16().count());
        error.0.reason = reason;
        error
    }

    pub(super) fn reason(&self) -> Option<FailureReason> {
        self.0.reason
    }
}

pub(super) fn checkout_failure(cwd: &str, args: &[&str], output: &Output) -> GitCommandError {
    let details = if output.stderr.lines().any(|line| {
        line.starts_with(
            "error: Your local changes to the following files would be overwritten by checkout:",
        )
    }) {
        "Your local changes would be overwritten by switching branches. Commit, stash, or discard them, then switch again."
    } else {
        "git checkout failed"
    };
    GitCommandError::failed(CHECKOUT, cwd, args, output, Some(details))
}

fn classify(stderr: &str) -> Option<FailureReason> {
    let diagnostics = stderr
        .lines()
        .map(str::to_ascii_lowercase)
        .filter(|line| {
            line.starts_with("fatal:")
                || line.starts_with("error:")
                || line.trim_start().starts_with("! ")
                || (!line.starts_with("remote:")
                    && (line.contains("permission denied")
                        || line.contains("host key verification failed")))
        })
        .collect::<Vec<_>>();
    let any = |pattern: &str| diagnostics.iter().any(|line| line.contains(pattern));
    if any("not a git repository") || any("gitfile does not point to a valid repository") {
        return Some(FailureReason::NotARepository);
    }
    if any("would clobber existing tag") {
        return Some(FailureReason::TagWouldBeClobbered);
    }
    if any("is already used by worktree at") || any("is already checked out at") {
        return Some(FailureReason::BranchCheckedOutInWorktree);
    }
    if diagnostics
        .iter()
        .any(|line| line.contains("a branch named ") && line.contains(" already exists"))
    {
        return Some(FailureReason::BranchAlreadyExists);
    }
    if any("' exists; cannot create 'refs/") && any("'refs/") {
        return Some(FailureReason::RefNameConflict);
    }
    if diagnostics
        .iter()
        .any(|line| line.contains("could not lock config file ") && line.contains(": file exists"))
    {
        return Some(FailureReason::ConfigLocked);
    }
    if any("authentication failed")
        || any("could not read username")
        || any("could not read password")
        || any("permission denied (")
        || any("permission denied, please try again")
    {
        return Some(FailureReason::AuthenticationFailed);
    }
    if any("host key verification failed") {
        return Some(FailureReason::HostKeyUnverified);
    }
    if any("could not read from remote repository")
        || any("does not appear to be a git repository")
        || diagnostics
            .iter()
            .any(|line| line.contains("repository ") && line.contains(" not found"))
    {
        return Some(FailureReason::RemoteUnreachable);
    }
    if diagnostics.iter().any(|line| {
        (line.starts_with("fatal: '") && line.contains("' already exists"))
            || (line.contains("destination path ") && line.contains(" already exists"))
    }) {
        return Some(FailureReason::PathAlreadyExists);
    }
    if !diagnostics.iter().any(|line| {
        line.starts_with("error:")
            && (line.contains(", not a commit") || line.contains("expected commit type"))
    }) && diagnostics.iter().any(|line| {
        [
            "fatal: invalid reference: ",
            "fatal: not a valid object name",
            "fatal: needed a single revision",
            "fatal: bad revision ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
            || (line.starts_with("fatal: '")
                && line.contains("' is not a commit and a branch ")
                && line.contains("cannot be created from it"))
    }) {
        return Some(FailureReason::RefNotFound);
    }
    None
}
