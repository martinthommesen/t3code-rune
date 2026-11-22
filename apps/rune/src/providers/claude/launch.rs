use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tokio::process::Command;
use uuid::Uuid;

pub struct Launch {
    pub executable: PathBuf,
    pub working_directory: PathBuf,
    pub environment: HashMap<OsString, OsString>,
    pub target: SessionTarget,
    pub model: String,
    pub title: Option<String>,
    pub append_system_prompt: String,
    pub permission: PermissionMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTarget {
    New(Uuid),
    Resume {
        session_id: Uuid,
        resume_at: Option<Uuid>,
    },
}

impl SessionTarget {
    pub fn id(self) -> Uuid {
        match self {
            Self::New(id) | Self::Resume { session_id: id, .. } => id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionMode {
    Default,
    FullAccess,
}

const INHERITED_SESSION_VARIABLES: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_DESKTOP_APP_VERSION",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
    "CLAUDE_AGENT_SDK_VERSION",
    "ELECTRON_RUN_AS_NODE",
];

impl Launch {
    pub(super) fn command(&self) -> Command {
        let mut command = command(&self.executable, &self.working_directory, &self.environment);
        command
            .args([
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--thinking=adaptive",
                "--thinking-display=summarized",
            ])
            .arg(format!("--model={}", self.model));
        if self.permission == PermissionMode::Default {
            command.arg("--permission-prompt-tool=stdio");
        }
        if let SessionTarget::Resume { session_id, .. } = self.target {
            command.arg(format!("--resume={session_id}"));
        }
        command.arg("--tools=default");
        match self.permission {
            PermissionMode::Default => command.arg("--permission-mode=default"),
            PermissionMode::FullAccess => command.args([
                "--permission-mode=bypassPermissions",
                "--allow-dangerously-skip-permissions",
            ]),
        };
        command.arg("--include-partial-messages");
        match self.target {
            SessionTarget::New(id) => {
                command.arg(format!("--session-id={id}"));
            }
            SessionTarget::Resume {
                resume_at: Some(id),
                ..
            } => {
                command.arg(format!("--resume-session-at={id}"));
            }
            SessionTarget::Resume {
                resume_at: None, ..
            } => {}
        }
        command.args([
            "--thinking-display",
            "summarized",
            "--settings",
            r#"{"showThinkingSummaries":true}"#,
        ]);
        command
    }
}

pub(super) fn command(
    executable: &Path,
    cwd: &Path,
    environment: &HashMap<OsString, OsString>,
) -> Command {
    let mut command = Command::new(executable);
    command.current_dir(cwd);
    for name in INHERITED_SESSION_VARIABLES {
        command.env_remove(name);
    }
    command
        .env("CLAUDE_CODE_ENTRYPOINT", "t3-code")
        .envs(environment);
    command
}
