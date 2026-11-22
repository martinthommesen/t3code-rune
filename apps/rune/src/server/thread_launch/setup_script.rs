mod output;
mod shell;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::setup_progress::{Progress, ScriptInfo, StageId, Status};
use super::{LaunchError, LaunchInput};
use crate::server::terminal::{
    OpenTerminal, OutputEvent, TerminalEvent, TerminalKey, TerminalManager,
};

const QUIET_SHELL: Duration = Duration::from_secs(3);
const PROMPT_REDRAW: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Script {
    id: String,
    name: String,
    command: String,
    #[serde(rename = "async", default = "asynchronous_default")]
    pub(super) asynchronous: bool,
    #[serde(default)]
    stop_on_settle: bool,
}

fn asynchronous_default() -> bool {
    true
}

pub(super) struct Started {
    pub(super) key: TerminalKey,
    pub(super) asynchronous: bool,
    pub(super) completion: JoinHandle<Completion>,
}

#[derive(Clone, Copy)]
pub(super) enum Completion {
    Exited(Option<i32>),
    Cancelled,
}

pub(super) struct Location<'a> {
    pub(super) thread_id: &'a str,
    pub(super) project: &'a Path,
    pub(super) worktree: &'a Path,
}

impl Script {
    pub(super) fn start(
        self,
        input: &LaunchInput,
        location: Location<'_>,
        manager: Arc<TerminalManager>,
        progress: Progress,
        cancelled: watch::Receiver<bool>,
    ) -> Result<Started, LaunchError> {
        let Location {
            thread_id,
            project,
            worktree,
        } = location;
        let key = TerminalKey {
            thread_id: thread_id.to_owned(),
            terminal_id: format!("setup-{}", self.id),
        };
        let terminal = OpenTerminal {
            key: key.clone(),
            cwd: worktree.to_owned(),
            worktree_path: Some(Some(worktree.to_owned())),
            cols: None,
            rows: None,
            env: [
                (
                    "T3CODE_PROJECT_ROOT",
                    project.to_string_lossy().into_owned(),
                ),
                (
                    "T3CODE_WORKTREE_PATH",
                    worktree.to_string_lossy().into_owned(),
                ),
                ("COLORTERM", String::new()),
                ("T3CODE_TERMINAL_SHELL", String::new()),
                ("NO_COLOR", "1".to_owned()),
                ("FORCE_COLOR", "0".to_owned()),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        };
        let (snapshot, observed) = manager
            .open_observed(terminal, self.stop_on_settle)
            .map_err(|_| {
                input.fail(
                    "run-setup-script",
                    format!(
                        "Project setup script operation 'openTerminal' failed for thread '{thread_id}' in '{}'.",
                        worktree.display()
                    ),
                )
            })?;
        let shell = snapshot.shell().map(shell::CompletionShell::resolve);
        if matches!(shell, Some(shell::CompletionShell::Cmd)) {
            let _ = manager.close_idle(&key);
            return Err(input.fail(
                "run-setup-script",
                "Project script completion is not supported in cmd.exe. Install PowerShell and retry.",
            ));
        }
        let sentinel = format!("__T3_SETUP_DONE___{}:", uuid::Uuid::new_v4().simple());
        let command = shell.map(|shell| (shell, shell.wrap(&self.command, &sentinel)));
        progress.script(ScriptInfo {
            terminal_id: key.terminal_id.clone(),
            name: self.name,
            command: self.command,
        });
        let completion = tokio::spawn(run_observed(Observation {
            manager,
            key: key.clone(),
            observed,
            command,
            sentinel,
            progress,
            cancelled,
        }));
        Ok(Started {
            key,
            asynchronous: self.asynchronous,
            completion,
        })
    }
}

struct Observation {
    manager: Arc<TerminalManager>,
    key: TerminalKey,
    observed: broadcast::Receiver<TerminalEvent>,
    command: Option<(shell::CompletionShell, String)>,
    sentinel: String,
    progress: Progress,
    cancelled: watch::Receiver<bool>,
}

async fn run_observed(mut run: Observation) -> Completion {
    let result = run.monitor().await;
    if let Completion::Exited(exit) = result {
        if exit == Some(0) {
            let _ = run.manager.close_idle(&run.key);
        }
        let detail = format!(
            "exited with {}",
            exit.map_or_else(|| "no exit code".to_owned(), |code| code.to_string())
        );
        run.progress.stage(
            StageId::SetupScript,
            if exit == Some(0) {
                Status::Done
            } else {
                Status::Failed
            },
            Some(&detail),
        );
    }
    result
}

impl Observation {
    async fn monitor(&mut self) -> Completion {
        let Some((shell, command)) = self.command.as_ref() else {
            return Completion::Exited(None);
        };
        let mut output = output::Capture::new(&self.sentinel, command, &self.progress);
        let mut ready = matches!(shell, shell::CompletionShell::PowerShell);
        let mut sent = false;
        let mut deadline = Instant::now() + QUIET_SHELL;
        loop {
            if *self.cancelled.borrow() {
                return Completion::Cancelled;
            }
            if ready && !sent {
                if self
                    .manager
                    .write(&self.key, &format!(" {command}\r"), false)
                    .is_err()
                {
                    return Completion::Exited(None);
                }
                sent = true;
            }
            tokio::select! {
                biased;
                _ = self.cancelled.wait_for(|cancelled| *cancelled) => return Completion::Cancelled,
                event = self.observed.recv() => {
                    let Ok(event) = event else { return Completion::Exited(output.exit_code()); };
                    match event.output() {
                        OutputEvent::Output { data } => {
                            let had_exit = output.exit_code().is_some();
                            output.output(data);
                            ready |= output.reading();
                            if output.exit_code().is_some() {
                                if !had_exit { deadline = Instant::now() + PROMPT_REDRAW; }
                                if output.has_partial_line() { return Completion::Exited(output.exit_code()); }
                            } else if !sent {
                                deadline = Instant::now() + QUIET_SHELL;
                            }
                        }
                        OutputEvent::Exited { .. } | OutputEvent::Closed | OutputEvent::Error { .. } => {
                            return Completion::Exited(output.exit_code());
                        }
                        _ => (),
                    }
                },
                _ = tokio::time::sleep_until(deadline), if !sent || output.exit_code().is_some() => {
                    if sent {
                        return Completion::Exited(output.exit_code());
                    }
                    match self.manager.has_active_subprocess(&self.key) {
                        Ok(Some(false)) => ready = true,
                        Ok(_) => output.flush_partial_line(),
                        Err(_) => return Completion::Exited(None),
                    }
                    deadline = Instant::now() + QUIET_SHELL;
                }
            }
        }
    }
}
