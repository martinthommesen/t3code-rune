use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::super::store::Store;
use super::super::thread_commands::{CreateThread, ThreadCommands};
use super::reservation::PreparationReservation;
use super::setup_progress::{Phase, Progress, StageId, Status};
use super::setup_script::{self, Completion};
use super::{
    AfterClaim, DispatchCommit, LaunchCommit, LaunchContext, LaunchError, LaunchInput,
    LaunchResult, PendingCheckout, PreparedLaunch, Trimmed, WorkspaceStrategy, persist, prepare,
    release, worktree,
};
use crate::server::terminal::TerminalKey;

pub(super) type CommitObserver = Arc<dyn Fn(&str, bool) + Send + Sync>;

pub(in super::super) async fn launch(
    input: LaunchInput,
    context: LaunchContext<'_>,
    on_commit: impl Fn(&str, bool) + Send + Sync + 'static,
) -> Result<LaunchResult, LaunchError> {
    let on_commit: CommitObserver = Arc::new(on_commit);
    let database = context.database.to_owned();
    let scratch_root = context.scratch_root.to_owned();
    let settings_path = context.settings_path.map(std::path::Path::to_owned);
    let terminals = context.terminals;
    let codex_enabled = context.codex_enabled;
    let interrupted = input.fail("create-thread", "The launch task was interrupted.");
    tokio::spawn(async move {
        let plan = prepare(
            input,
            LaunchContext {
                database: &database,
                scratch_root: &scratch_root,
                settings_path: settings_path.as_deref(),
                codex_enabled,
                terminals,
            },
        )
        .await?;
        let interrupted = plan
            .input
            .fail("create-thread", "The launch claim was interrupted.");
        tokio::task::spawn_blocking(move || {
            let commit = commit_initial_actions(&plan, codex_enabled, on_commit.as_ref())?;
            let projection = read_projection(&plan, &commit.thread_id);
            let result = projection.and_then(|projection| plan.result(&commit, projection));
            schedule_preparation(plan, commit, codex_enabled, on_commit);
            result
        })
        .await
        .map_err(|_| interrupted)?
    })
    .await
    .map_err(|_| interrupted)?
}

pub(super) fn schedule_preparation(
    plan: PreparedLaunch,
    commit: LaunchCommit,
    codex_enabled: bool,
    on_commit: CommitObserver,
) {
    if commit.schedule_preparation
        && let Some(reservation) = PreparationReservation::new(&plan, &commit.thread_id)
    {
        tokio::spawn(prepare_in_background(
            plan,
            commit,
            codex_enabled,
            on_commit,
            reservation,
        ));
    }
}

async fn prepare_in_background(
    plan: PreparedLaunch,
    commit: LaunchCommit,
    codex_enabled: bool,
    on_commit: CommitObserver,
    reservation: PreparationReservation,
) {
    let mut preparation = WorktreePreparation {
        plan: Arc::new(plan),
        commit: Arc::new(commit),
        codex_enabled,
        on_commit,
        reservation,
        checkout: None,
        workspace_recorded: false,
        progress: None,
        setup_terminal: None,
    };
    if let Err(error) = preparation.run().await {
        let cancelled = *preparation.reservation.cancelled.borrow()
            && error.cause.name == "WorktreeSetupCancelled";
        if let Some(progress) = &preparation.progress {
            if cancelled {
                progress.finish(Phase::Cancelled, None);
            } else {
                progress.finish(Phase::Failed, Some(&error.preparation_failure_message()));
            }
        }
        if cancelled
            && let Some((terminals, key)) = preparation
                .plan
                .terminals
                .as_ref()
                .zip(preparation.setup_terminal.as_ref())
        {
            terminals.close(&key.thread_id, Some(&key.terminal_id), true);
        }
        if let Some(checkout) = preparation.checkout.take() {
            if cancelled || !preparation.workspace_recorded {
                let path = checkout.path().to_owned();
                if checkout.abort().await.is_ok() && cancelled {
                    let _ = preparation
                        .write(
                            "update-thread",
                            move |plan, commit| {
                                release::record_cancel_workspace(plan, commit, &path)
                            },
                            false,
                        )
                        .await;
                }
            } else {
                let _ = checkout.commit().await;
            }
        }
        settle_failure(
            preparation.plan,
            preparation.commit,
            error,
            preparation.on_commit,
        )
        .await;
    } else if let Some(progress) = &preparation.progress {
        progress.finish(Phase::Done, None);
    }
}

struct WorktreePreparation {
    plan: Arc<PreparedLaunch>,
    commit: Arc<LaunchCommit>,
    codex_enabled: bool,
    on_commit: CommitObserver,
    reservation: PreparationReservation,
    checkout: Option<worktree::LocalWorktree>,
    workspace_recorded: bool,
    progress: Option<Progress>,
    setup_terminal: Option<TerminalKey>,
}

impl WorktreePreparation {
    async fn run(&mut self) -> Result<(), LaunchError> {
        let Some(mut pending) = self
            .blocking("update-thread", PreparedLaunch::pending_workspace)
            .await?
        else {
            return Ok(());
        };
        let mut automatic_branch = false;
        if let Some(checkout) = pending.checkout.take() {
            let strategy = match &checkout {
                PendingCheckout::FinalizeBinding { requested, .. } => requested,
                PendingCheckout::Provision { .. } => &pending.strategy,
            };
            if let WorkspaceStrategy::Worktree {
                base_ref, branch, ..
            } = strategy
            {
                self.progress = Some(Progress::begin(
                    &self.plan.database,
                    &self.commit.thread_id,
                    branch.as_ref().map(|branch| branch.0.as_str()),
                    &base_ref.0,
                ));
            }
            self.reservation.begin_checkout();
            match checkout {
                PendingCheckout::FinalizeBinding {
                    requested,
                    path,
                    original_branch,
                } => {
                    automatic_branch = worktree::auto_naming(&requested);
                    self.workspace_recorded = true;
                    self.checkout = worktree::recorded_binding(
                        &self.plan.input,
                        &requested,
                        &pending.workspace_root,
                        &path,
                        &original_branch,
                    )
                    .await?;
                    if let Some(progress) = &self.progress {
                        progress.stage(StageId::Fetch, Status::Skipped, None);
                        progress.stage(StageId::Checkout, Status::Running, None);
                        progress.checkout(&path, &original_branch);
                    }
                }
                PendingCheckout::Provision { parent } => {
                    automatic_branch = worktree::auto_naming(&pending.strategy);
                    self.write(
                        "provision-worktree",
                        |plan, commit| {
                            release::record_progress(plan, commit, release::ProgressPhase::Worktree)
                        },
                        false,
                    )
                    .await?;
                    let checkout = worktree::LocalWorktree::acquire(
                        &self.plan.input,
                        &pending.strategy,
                        &pending.workspace_root,
                        &parent,
                        self.plan.settings_path.as_deref(),
                        &mut self.reservation.cancelled,
                        self.progress.as_ref(),
                    )
                    .await?;
                    let path = checkout.path().to_str().ok_or_else(|| {
                        self.plan.input.fail(
                            "provision-worktree",
                            "The Git worktree path is not Unicode.",
                        )
                    })?;
                    if let Some(progress) = &self.progress {
                        progress.checkout(checkout.path(), checkout.branch());
                    }
                    pending.strategy = WorkspaceStrategy::ExistingWorktree {
                        worktree_path: Trimmed(path.to_owned()),
                        branch: Some(Trimmed(checkout.branch().to_owned())),
                    };
                    self.checkout = Some(checkout);
                }
            }
        }
        self.check_cancelled()?;
        let mut preparation = self.plan.workspace_preparation(&self.commit, pending).await;
        preparation.newly_claimed_checkout = self.checkout.is_some() && !self.workspace_recorded;
        let setup_path = preparation.metadata_command["worktreePath"]
            .as_str()
            .map(std::path::PathBuf::from);
        let project_root = preparation.workspace_root.clone();
        let prepared_branch = preparation.metadata_command["branch"]
            .as_str()
            .map(str::to_owned);
        let release_run = preparation.release_command.is_some();
        self.write(
            "update-thread",
            move |plan, commit| release::record_workspace(plan, commit, &preparation),
            false,
        )
        .await?;
        self.workspace_recorded = true;
        self.check_cancelled()?;
        if setup_path.is_some() {
            self.write(
                "update-thread",
                |plan, commit| {
                    release::record_progress(plan, commit, release::ProgressPhase::Setup)
                },
                false,
            )
            .await?;
        }
        let setup = self
            .start_setup(&project_root, setup_path.as_deref())
            .await?;
        if automatic_branch && let Some(branch) = prepared_branch {
            self.rename_checkout(branch).await?;
        }
        let mut async_setup = None;
        if let Some(setup) = setup {
            if setup.asynchronous {
                async_setup = Some(setup.completion);
            } else {
                match setup.completion.await.unwrap_or(Completion::Exited(None)) {
                    Completion::Cancelled => return Err(self.plan.input.cancelled()),
                    Completion::Exited(Some(0)) => (),
                    Completion::Exited(code) => {
                        let detail =
                            code.map_or_else(|| "no exit code".to_owned(), |code| code.to_string());
                        return Err(self.plan.input.fail(
                            "run-setup-script",
                            format!("Setup script exited with {detail}."),
                        ));
                    }
                }
            }
        }
        if !self.reservation.begin_release() {
            return Err(self.plan.input.cancelled());
        }
        if let Some(progress) = &self.progress {
            progress.stage(StageId::Agent, Status::Running, None);
        }
        if let Some(checkout) = self.checkout.take() {
            checkout
                .commit()
                .await
                .map_err(|error| self.plan.input.fail("update-thread", error))?;
        }
        if release_run {
            let codex_enabled = self.codex_enabled;
            self.write(
                "release-run",
                move |plan, commit| release::record_release(plan, commit, codex_enabled),
                true,
            )
            .await?;
        }
        if let Some(progress) = &self.progress {
            progress.stage(StageId::Agent, Status::Done, None);
        }
        if let Some(setup) = async_setup {
            let _ = setup.await;
        }
        Ok(())
    }

    async fn start_setup(
        &mut self,
        project: &std::path::Path,
        worktree: Option<&std::path::Path>,
    ) -> Result<Option<setup_script::Started>, LaunchError> {
        let Some(progress) = self.progress.clone() else {
            return Ok(None);
        };
        progress.stage(StageId::SetupScript, Status::Running, None);
        if let Some((worktree, terminals)) = worktree.zip(self.plan.terminals.clone())
            && let Some(script) = self
                .blocking("run-setup-script", PreparedLaunch::automatic_setup)
                .await?
        {
            self.check_cancelled()?;
            let started = script.start(
                &self.plan.input,
                setup_script::Location {
                    thread_id: &self.commit.thread_id,
                    project,
                    worktree,
                },
                terminals,
                progress,
                self.reservation.cancelled.clone(),
            )?;
            self.setup_terminal = Some(started.key.clone());
            return Ok(Some(started));
        }
        progress.stage(StageId::SetupScript, Status::Skipped, None);
        Ok(None)
    }

    async fn rename_checkout(&mut self, recorded: String) -> Result<(), LaunchError> {
        let Some(checkout) = self.checkout.as_ref() else {
            return Ok(());
        };
        let path = checkout.path().to_owned();
        if checkout.branch() != recorded {
            let actual = checkout.branch().to_owned();
            return self
                .write(
                    "update-thread",
                    move |plan, commit| {
                        worktree::rename_events::renamed(plan, commit, &path, &recorded, &actual)
                    },
                    false,
                )
                .await;
        }
        let Some(message) = self.plan.input.initial_message.as_ref() else {
            return Ok(());
        };
        if self
            .blocking("update-thread", worktree::rename_events::settled)
            .await?
        {
            return Ok(());
        }
        let proposed = worktree::generate_name(
            &self.plan.input,
            &path,
            self.plan.settings_path.as_deref(),
            &message.text,
            &mut self.reservation.cancelled,
        )
        .await;
        let reason = match proposed {
            worktree::ResultName::Cancelled => return Err(self.plan.input.cancelled()),
            worktree::ResultName::Failed(reason) => reason,
            worktree::ResultName::Generated { branch, exact } => {
                let checkout = self
                    .checkout
                    .as_mut()
                    .expect("The worktree remains reserved during branch naming");
                match checkout.rename_branch(&branch, exact).await {
                    Ok(()) => {
                        let actual = checkout.branch().to_owned();
                        self.check_cancelled()?;
                        return self
                            .write(
                                "update-thread",
                                move |plan, commit| {
                                    worktree::rename_events::renamed(
                                        plan, commit, &path, &recorded, &actual,
                                    )
                                },
                                false,
                            )
                            .await;
                    }
                    Err(error) => error.to_string(),
                }
            }
        };
        self.check_cancelled()?;
        self.write(
            "update-thread",
            move |plan, commit| worktree::rename_events::failure(plan, commit, &recorded, &reason),
            false,
        )
        .await
    }

    fn check_cancelled(&self) -> Result<(), LaunchError> {
        if *self.reservation.cancelled.borrow() {
            Err(self.plan.input.cancelled())
        } else {
            Ok(())
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: &'static str,
        action: impl FnOnce(&PreparedLaunch, &LaunchCommit) -> Result<T, LaunchError> + Send + 'static,
    ) -> Result<T, LaunchError> {
        let (plan, commit) = (self.plan.clone(), self.commit.clone());
        tokio::task::spawn_blocking(move || action(&plan, &commit))
            .await
            .map_err(|_| {
                self.plan
                    .input
                    .fail(operation, "The workspace preparation task was interrupted.")
            })?
    }

    async fn write(
        &self,
        operation: &'static str,
        action: impl FnOnce(&PreparedLaunch, &LaunchCommit) -> Result<bool, LaunchError>
        + Send
        + 'static,
        start_provider: bool,
    ) -> Result<(), LaunchError> {
        let on_commit = self.on_commit.clone();
        self.blocking(operation, move |plan, commit| {
            if action(plan, commit)? {
                on_commit(&commit.thread_id, start_provider);
            }
            Ok(())
        })
        .await
    }
}

async fn settle_failure(
    plan: Arc<PreparedLaunch>,
    commit: Arc<LaunchCommit>,
    error: LaunchError,
    on_commit: CommitObserver,
) {
    if commit.run_id.is_none() {
        eprintln!("Thread workspace preparation failed: {error}");
        return;
    }
    let detail = Arc::new(error.preparation_failure_message());
    let mut delay = Duration::from_secs(1);
    loop {
        let result = tokio::task::spawn_blocking({
            let (plan, commit, detail) = (plan.clone(), commit.clone(), detail.clone());
            move || release::record_failure(&plan, &commit, &detail)
        })
        .await;
        match result {
            Ok(Ok(changed)) => {
                if changed {
                    on_commit(&commit.thread_id, false);
                }
                return;
            }
            Ok(Err(error)) => {
                eprintln!("Could not record workspace preparation failure: {error}");
                if !release::transient_write_error(&error) {
                    return;
                }
            }
            Err(_) => return,
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

fn commit_initial_actions(
    plan: &PreparedLaunch,
    codex_enabled: bool,
    on_commit: &dyn Fn(&str, bool),
) -> Result<LaunchCommit, LaunchError> {
    let claim = if plan.input.reuse_existing_thread == Some(true) {
        persist::reuse_claim(plan)?
    } else if plan.input.initial_message.is_some() {
        plan.persist_initial_claim(codex_enabled)?
    } else {
        let threads = ThreadCommands::open(plan.database.clone())
            .map_err(|cause| plan.input.fail("create-thread", cause))?;
        let create = CreateThread::parse_for_launch(plan.claim_command())
            .map_err(|cause| plan.input.fail("create-thread", cause))?;
        let created = threads
            .create(create)
            .map_err(|cause| plan.input.fail("create-thread", cause))?;
        DispatchCommit {
            sequence: created.sequence,
            replayed: !created.committed,
            thread_id: Some(plan.thread_id.clone()),
            run_id: None,
        }
    };
    if !claim.replayed {
        on_commit(claim.thread_id.as_deref().unwrap_or(&plan.thread_id), false);
    }
    match plan.after_claim(claim)? {
        AfterClaim::Complete(commit) => Ok(commit),
        AfterClaim::Dispatch(initial) => {
            let dispatch = persist::fallback_message(plan, &initial, codex_enabled)?;
            if !dispatch.replayed {
                on_commit(&initial.thread_id, false);
            }
            plan.after_initial_message(initial, dispatch)
        }
    }
}

fn read_projection(plan: &PreparedLaunch, thread_id: &str) -> Result<Value, LaunchError> {
    let operation = "create-thread";
    Store::open(plan.database.clone())
        .and_then(|store| store.live_thread_snapshot(thread_id))
        .map_err(|cause| plan.input.fail(operation, cause))?
        .and_then(|mut snapshot| snapshot.get_mut("projection").map(Value::take))
        .ok_or_else(|| {
            plan.input
                .fail(operation, "The accepted launch projection is unavailable.")
        })
}
