mod command_output;
pub(super) mod launch;
mod reasoning;
mod streaming;
mod terminals;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::sync::{Notify, broadcast, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use t3_rune::providers::CodexLaunch;
use t3_rune::providers::codex::{
    AssistantPhase, CodexAdapterError, CodexSession, CodexSessionClient, CodexSessionEvent,
    SettlementStatus, ThreadId, ToolLifecycle, TurnEvent, TurnKey,
};

use self::command_output::CommandOutputHub;
pub(crate) use self::command_output::{CommandColor, CommandPen, TerminalOutput};
pub(super) use self::command_output::{CommandOutputError, CommandOutputSubscription};
use self::launch::Launch;
use self::reasoning::{PartOutput, ReasoningOutput};
use self::streaming::{AssistantStreaming, Snapshot, StreamingMode};
use self::terminals::RunningTerminals;
use super::ProjectionChange;
use super::claude_worker;
use super::settings::{CodexInstanceError, SettingsError, SettingsFile};
use super::turn_commands::abort_effect::AbortReason;
use super::turn_commands::context_handoff::HistoryDelivery;
use super::turn_commands::interrupt_effect::ClaimedInterrupt;
use super::turn_commands::native_events::AssistantSnapshot;
use super::turn_commands::provider_session::SessionAttachment;
use super::turn_commands::provider_turn::NativeTurnUpdate;
use super::turn_commands::reasoning::{ReasoningPart, ReasoningSnapshot, ReasoningStatus};
use super::turn_commands::runtime_requests::{LiveRequest, ResponseValue};
use super::turn_commands::start_effect::ClaimedTurnStart;
use super::turn_commands::start_failure::StartFailure;
use super::turn_commands::start_recovery::RecoveryTrigger;
use super::turn_commands::title_effect::ClaimedTitle;
use super::turn_commands::tool_events::{command_item_id, file_approval_prompt};
use super::turn_commands::turn_text::AssistantUpdate;
use super::turn_commands::{TurnCommandError, TurnCommands};

const EFFECT_BATCH_SIZE: usize = 32;
const STARTUP_EVENT_CAPACITY: usize = 128;
const DELTA_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const STOP_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct TurnWorker {
    queue: TurnQueue,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), WorkerError>>,
}

#[derive(Clone)]
pub(super) struct TurnQueue {
    wake: Arc<Notify>,
    command_output: CommandOutputHub,
}

impl TurnQueue {
    pub(super) fn new(
        commands: TurnCommands,
        changes: broadcast::Sender<ProjectionChange>,
    ) -> Self {
        Self {
            wake: Arc::new(Notify::new()),
            command_output: CommandOutputHub::new(commands, changes),
        }
    }

    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }

    pub(super) async fn notified(&self) {
        self.wake.notified().await;
    }

    pub(super) fn subscribe_command_output(
        &self,
        thread_id: &str,
        item_id: &str,
    ) -> Result<CommandOutputSubscription, CommandOutputError> {
        self.command_output.subscribe(thread_id, item_id)
    }
}

#[derive(Debug)]
pub(super) enum WorkerError {
    Closed,
    Storage(TurnCommandError),
    Settings(SettingsError),
    ProviderSettings(CodexInstanceError),
    Provider(CodexAdapterError),
    Claude(claude_worker::Error),
    Unavailable(&'static str),
    Interrupted,
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("The Codex worker has stopped."),
            Self::Storage(error) => error.fmt(formatter),
            Self::Settings(error) => error.fmt(formatter),
            Self::ProviderSettings(error) => error.fmt(formatter),
            Self::Provider(_) => {
                formatter.write_str("The Codex app-server could not complete the turn.")
            }
            Self::Claude(error) => error.fmt(formatter),
            Self::Unavailable(reason) => formatter.write_str(reason),
            Self::Interrupted => formatter.write_str("The Codex worker stopped during this turn."),
        }
    }
}

impl Error for WorkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Settings(error) => Some(error),
            Self::ProviderSettings(error) => Some(error),
            Self::Provider(error) => Some(error),
            Self::Claude(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TurnCommandError> for WorkerError {
    fn from(error: TurnCommandError) -> Self {
        Self::Storage(error)
    }
}

impl From<CodexAdapterError> for WorkerError {
    fn from(error: CodexAdapterError) -> Self {
        Self::Provider(error)
    }
}

impl TurnWorker {
    pub(super) fn recover_before_spawn(
        commands: &TurnCommands,
        changes: &broadcast::Sender<ProjectionChange>,
    ) -> Result<(), WorkerError> {
        commands.requeue_unoffered_claude_start(None, None)?;
        for thread_id in commands.recover_lost_starts()? {
            let _ = changes.send(ProjectionChange::Thread(thread_id));
        }
        let active = commands.recover_active_provider_runs()?;
        let recovered_approvals = commands.recover_stopped_approvals()?;
        let released = commands.release_provider_session(&active, None)?;
        for thread_id in active
            .into_iter()
            .chain(recovered_approvals)
            .chain(released)
        {
            let _ = changes.send(ProjectionChange::Thread(thread_id));
        }
        commands.cancel_process_bound_interrupts()?;
        commands.recover_checkpoints()?;
        commands.cancel_lost_title_effects()?;
        for thread_id in commands.recover_queued_worktrees()? {
            let _ = changes.send(ProjectionChange::Thread(thread_id));
        }
        Ok(())
    }

    pub(super) fn spawn(
        commands: TurnCommands,
        settings: SettingsFile,
        changes: broadcast::Sender<ProjectionChange>,
    ) -> Self {
        let queue = TurnQueue::new(commands.clone(), changes.clone());
        let (stop, signal) = oneshot::channel();
        let task = tokio::spawn(
            Worker {
                commands,
                settings,
                changes,
                worker_id: format!("rune-codex:{}", Uuid::new_v4()),
            }
            .run(queue.clone(), signal),
        );
        queue.wake();
        Self { queue, stop, task }
    }

    pub(super) fn sender(&self) -> TurnQueue {
        self.queue.clone()
    }

    pub(super) async fn shutdown(self) -> Result<(), WorkerError> {
        let _ = self.stop.send(());
        self.task.await.map_err(|_| WorkerError::Closed)?
    }
}

struct Worker {
    commands: TurnCommands,
    settings: SettingsFile,
    changes: broadcast::Sender<ProjectionChange>,
    worker_id: String,
}

impl Worker {
    async fn run(
        self,
        queue: TurnQueue,
        mut stop: oneshot::Receiver<()>,
    ) -> Result<(), WorkerError> {
        let mut runtime = None;
        let mut claude = claude_worker::Runtimes::default();
        let result = self
            .consume(&queue, &mut stop, &mut runtime, &mut claude)
            .await;
        let release = self.release(&mut runtime, None).await;
        result.and(release)
    }

    async fn consume(
        &self,
        queue: &TurnQueue,
        stop: &mut oneshot::Receiver<()>,
        runtime: &mut Option<ActiveRuntime>,
        claude: &mut claude_worker::Runtimes,
    ) -> Result<(), WorkerError> {
        let mut idle_check = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                biased;
                _ = &mut *stop => return Ok(()),
                () = queue.wake.notified() => {},
                _ = idle_check.tick() => {
                    claude.release_idle().await;
                    continue;
                }
            }
            let mut cursor = 0;
            loop {
                let titles_remain = tokio::select! {
                    biased;
                    _ = &mut *stop => return Ok(()),
                    result = self.drain_titles(runtime.as_ref(), claude, queue) => result?,
                };
                let pending = self.commands.pending_effects(cursor, EFFECT_BATCH_SIZE)?;
                if pending.is_empty() {
                    if titles_remain {
                        continue;
                    }
                    break;
                }
                for (row_id, effect_id) in pending {
                    cursor = row_id;
                    let result = tokio::select! {
                        biased;
                        _ = &mut *stop => None,
                        result = self.execute(&effect_id, runtime, claude, queue) => Some(result),
                    };
                    let Some(result) = result else {
                        let release = self.release(runtime, None).await;
                        let recovery = self.reconcile_stopped(&effect_id, AbortReason::Shutdown);
                        return release.and(recovery);
                    };
                    if let Err(error) = result {
                        let detail = error.to_string();
                        self.reconcile_stopped(&effect_id, AbortReason::RuntimeFailure(&detail))?;
                        eprintln!("Provider worker: {error}");
                    }
                }
            }
        }
    }

    async fn drain_titles(
        &self,
        runtime: Option<&ActiveRuntime>,
        claude: &mut claude_worker::Runtimes,
        queue: &TurnQueue,
    ) -> Result<bool, WorkerError> {
        for _ in 0..EFFECT_BATCH_SIZE {
            let Some(claim) = self.commands.claim_title_effect(None, &self.worker_id)? else {
                return Ok(false);
            };
            let loaded = runtime.and_then(|runtime| runtime.loaded.get(&claim.thread_id));
            let client = runtime.map(|runtime| runtime.session.client());
            claude.apply_title(&claim).await;
            self.apply_title_effect(claim, loaded, client).await?;
            queue.wake();
        }
        Ok(true)
    }

    async fn apply_title_effect(
        &self,
        claim: ClaimedTitle,
        loaded: Option<&ThreadId>,
        client: Option<CodexSessionClient>,
    ) -> Result<(), WorkerError> {
        if let (Some(native), Some(client)) = (loaded, client)
            && claim.native_thread_id.as_ref() == Some(native)
        {
            client.apply_thread_name(native, &claim.title).await;
        }
        self.commands.finish_title_effect(&claim, &self.worker_id)?;
        Ok(())
    }

    fn reconcile_stopped(
        &self,
        effect_id: &str,
        reason: AbortReason<'_>,
    ) -> Result<(), WorkerError> {
        if let Some(receipt) = self.commands.abort_started_run(effect_id, reason)? {
            self.publish(&receipt.thread_id, receipt.sequence);
            self.commands
                .complete_inactive_interrupts(&receipt.thread_id)?;
            return Ok(());
        }
        self.commands
            .requeue_unoffered_claude_start(Some(effect_id), Some(&self.worker_id))?;
        if let Some(thread_id) = self
            .commands
            .recover_unaccepted_start(effect_id, RecoveryTrigger::Shutdown(&self.worker_id))?
        {
            let _ = self.changes.send(ProjectionChange::Thread(thread_id));
        }
        Ok(())
    }

    async fn execute(
        &self,
        effect_id: &str,
        runtime: &mut Option<ActiveRuntime>,
        claude: &mut claude_worker::Runtimes,
        queue: &TurnQueue,
    ) -> Result<Option<i64>, WorkerError> {
        if effect_id.starts_with("effect:checkpoint.capture:") {
            let thread_id = self.commands.checkpoint_thread(effect_id)?;
            let sequence = self
                .commands
                .capture_checkpoint(effect_id, &self.worker_id)
                .await?;
            if let Some(thread_id) = thread_id {
                self.publish_optional(&thread_id, sequence);
                if sequence.is_some() {
                    self.publish_optional(
                        &thread_id,
                        self.commands.promote_queued_worktree(&thread_id)?,
                    );
                }
            }
            return Ok(sequence);
        }
        let Some(claim) = self.commands.claim_start(effect_id, &self.worker_id)? else {
            let Some(claim) = self
                .commands
                .claim_claude_start(effect_id, &self.worker_id)?
            else {
                return Ok(None);
            };
            let result = claude
                .execute(&self.commands, &self.settings, &claim, queue, |sequence| {
                    self.publish(&claim.thread_id, sequence)
                })
                .await;
            queue.wake();
            return match result {
                Ok(sequence) => Ok(Some(sequence)),
                Err(error) => {
                    let detail = error.to_string();
                    if let Some(sequence) = self.commands.fail_claude_start(&claim, &detail)? {
                        self.publish(&claim.thread_id, sequence);
                    } else if let Some(receipt) = self.commands.abort_started_run(
                        effect_id,
                        AbortReason::ProviderFailure {
                            message: &detail,
                            code: error.code(),
                        },
                    )? {
                        self.publish(&receipt.thread_id, receipt.sequence);
                    }
                    Err(WorkerError::Claude(error))
                }
            };
        };
        let thread_id = claim
            .thread
            .app_thread_id
            .as_deref()
            .ok_or(WorkerError::Unavailable(
                "The claimed Codex turn has no local thread.",
            ))?;
        let launch = match Launch::resolve(&self.settings, &claim) {
            Ok(launch) => launch,
            Err(error) => {
                self.fail_start(&claim, thread_id, StartFailure::SessionOpen, &error)?;
                return Err(error);
            }
        };
        let _ = self.commands.capture_baseline(&claim).await;
        if self.commands.start_is_cancelled(&claim)? {
            return Ok(None);
        }
        let key = ProcessKey::from(&launch.process);
        if runtime.as_ref().is_some_and(|active| active.key != key) {
            self.release(runtime, None).await?;
        }
        let reuse = runtime.is_some();
        if !reuse {
            let session = match CodexSession::launch(launch.process).await {
                Ok(session) => session,
                Err(error) => {
                    let error = WorkerError::Provider(error);
                    self.fail_start(&claim, thread_id, StartFailure::SessionOpen, &error)?;
                    return Err(error);
                }
            };
            *runtime = Some(ActiveRuntime {
                key,
                session,
                threads: Vec::new(),
                loaded: HashMap::new(),
            });
        }
        let active = runtime.as_mut().ok_or(WorkerError::Closed)?;
        let result = self
            .run_session(&claim, thread_id, active, reuse, launch.streaming, queue)
            .await;
        if let Err(error) = &result {
            if self.commands.start_is_cancelled(&claim)? {
                return Ok(None);
            }
            self.release(runtime, Some(&error.to_string())).await?;
        }
        result.map(Some)
    }

    async fn release(
        &self,
        runtime: &mut Option<ActiveRuntime>,
        failure: Option<&str>,
    ) -> Result<(), WorkerError> {
        let Some(runtime) = runtime.take() else {
            return Ok(());
        };
        let shutdown = runtime.session.shutdown().await;
        for thread_id in self
            .commands
            .release_provider_session(&runtime.threads, failure)?
        {
            let _ = self.changes.send(ProjectionChange::Thread(thread_id));
        }
        shutdown.map_err(WorkerError::Provider)
    }

    fn fail_start(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        failure: StartFailure,
        error: &WorkerError,
    ) -> Result<(), WorkerError> {
        let sequence = self
            .commands
            .fail_provider_start(claim, failure, &error.to_string())?;
        self.publish_optional(thread_id, sequence);
        Ok(())
    }

    async fn run_session(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        runtime: &mut ActiveRuntime,
        reuse: bool,
        mode: StreamingMode,
        queue: &TurnQueue,
    ) -> Result<i64, WorkerError> {
        let Some(attachment) = self.commands.record_session_ready(claim, reuse)? else {
            return Err(WorkerError::Interrupted);
        };
        if let SessionAttachment::Attached(sequence) = attachment {
            self.publish(thread_id, sequence);
        }
        if !runtime.threads.iter().any(|attached| attached == thread_id) {
            runtime.threads.push(thread_id.to_owned());
        }
        let session = &mut runtime.session;
        let mut pending = VecDeque::new();
        let client = session.client();
        let native = async {
            match &claim.native_thread_id {
                Some(native_thread) if runtime.loaded.get(thread_id) == Some(native_thread) => {
                    Ok(native_thread.clone())
                }
                Some(native_thread) => client.resume_thread(native_thread, &claim.thread).await,
                None => client.start_thread(&claim.thread).await,
            }
        };
        let native_thread = match await_native(session, native, &mut pending).await {
            Ok(thread) => thread,
            Err(error) => {
                self.fail_start(claim, thread_id, StartFailure::ThreadLoad, &error)?;
                return Err(error);
            }
        };
        self.publish(
            thread_id,
            require_current(self.commands.bind_native_thread(claim, &native_thread)?)?,
        );
        runtime
            .loaded
            .insert(thread_id.to_owned(), native_thread.clone());
        let mut turn = Cow::Borrowed(&claim.turn);
        let mut inline_history = None;
        if let Some(history) =
            self.commands
                .prepare_unbound_history(claim, thread_id, &native_thread)?
        {
            self.publish(thread_id, history.sequence);
            let injected = await_native(
                session,
                client.inject_history(&native_thread, &history.native_items),
                &mut pending,
            )
            .await?;
            if injected {
                let sequence = self.commands.finish_history_delivery(
                    claim,
                    thread_id,
                    history,
                    HistoryDelivery::Injected,
                )?;
                self.publish(thread_id, sequence);
            } else {
                turn.to_mut().text = format!(
                    "{}\n\nUser message:\n{}",
                    history.inline_context, claim.turn.text
                );
                inline_history = Some(history);
            }
        }
        let title = self.commands.thread_title(thread_id)?;
        await_native(
            session,
            async {
                client.apply_thread_name(&native_thread, &title).await;
                Ok(())
            },
            &mut pending,
        )
        .await?;
        let accepted = await_native(
            session,
            client.start_turn(&native_thread, &turn),
            &mut pending,
        )
        .await?;
        if !self.commands.accept_provider_start(claim, &accepted)? {
            return Err(WorkerError::Interrupted);
        }
        if let Some(history) = inline_history {
            match self.commands.finish_history_delivery(
                claim,
                thread_id,
                history,
                HistoryDelivery::Inline,
            ) {
                Ok(sequence) => self.publish(thread_id, sequence),
                Err(error) => {
                    eprintln!("Could not record the accepted Codex context delivery: {error}")
                }
            }
        }
        queue.wake();

        let key = accepted.key;
        let mut output = AssistantOutput::new(mode);
        let mut reasoning = ReasoningOutput::default();
        let mut native_start_seen = false;
        let mut requests = BTreeMap::new();
        let mut file_changes = HashMap::new();
        let mut interrupt: Option<(ClaimedInterrupt, Instant)> = None;
        let mut retry = None;
        let mut applying_title: Option<std::pin::Pin<Box<_>>> = None;
        let mut running_terminals = RunningTerminals::default();
        let mut stopped_by_request = false;
        let mut terminating: Option<std::pin::Pin<Box<_>>> = None;
        loop {
            let event = if let Some(event) = pending.pop_front() {
                Some(event)
            } else {
                let deadline = output
                    .next_due()
                    .into_iter()
                    .chain(reasoning.next_due())
                    .min();
                let interrupt_due = interrupt.as_ref().map(|(_, deadline)| *deadline);
                tokio::select! {
                    event = session.next_event() => Some(event?),
                    () = wait_until(deadline), if deadline.is_some() => None,
                    () = wait_until(interrupt_due), if interrupt_due.is_some() => {
                        if let Some((effect, _)) = interrupt.take() {
                            self.publish_optional(thread_id, self.commands.mark_interrupt_unresponsive(
                                thread_id, &effect, &self.worker_id,
                            )?);
                        }
                        continue;
                    }
                    () = wait_until(retry), if retry.is_some() => {
                        retry = None;
                        queue.wake();
                        continue;
                    }
                    result = async {
                        applying_title.as_mut().expect("claimed thread title").await
                    }, if applying_title.is_some() => {
                        result?;
                        applying_title = None;
                        queue.wake();
                        continue;
                    }
                    attempts = async {
                        terminating.as_mut().expect("running terminal termination").await
                    }, if terminating.is_some() => {
                        let _ = running_terminals.record(attempts);
                        terminating = None;
                        continue;
                    }
                    () = queue.wake.notified() => {
                        if applying_title.is_some() {
                            continue;
                        }
                        if let Some(title) = self.commands.claim_title_effect(Some(thread_id), &self.worker_id)? {
                            applying_title = Some(Box::pin(self.apply_title_effect(title, Some(&key.thread_id), Some(client.clone()))));
                            continue;
                        }
                        if !requests.is_empty() {
                            self.answer_requests(thread_id, &key, &client, &mut requests).await?;
                        }
                        if interrupt.is_none()
                            && let Some(effect) = self.commands.claim_interrupt(thread_id, &key, &self.worker_id)?
                        {
                            match await_native(session, client.interrupt_turn(&key), &mut pending).await {
                                Ok(()) => {
                                    stopped_by_request = true;
                                    interrupt = Some((effect, Instant::now() + STOP_CONFIRMATION_TIMEOUT));
                                    let processes = running_terminals.pending();
                                    if !processes.is_empty() && terminating.is_none() {
                                        terminating = Some(Box::pin(terminals::terminate(&client, &key.thread_id, processes)));
                                    }
                                }
                                Err(WorkerError::Provider(CodexAdapterError::Transport(error @ (
                                    t3_rune::providers::CodexError::Remote { .. } |
                                    t3_rune::providers::CodexError::Timeout(_)
                                )))) => {
                                    retry = self.commands.retry_interrupt_effect(&effect, &self.worker_id, &error.to_string())?
                                        .map(|delay| Instant::now() + delay);
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        continue;
                    }
                }
            };
            let Some(event) = event else {
                self.flush_text(claim, thread_id, &key, &mut output)?;
                for part in reasoning.flush_due() {
                    self.write_reasoning(claim, thread_id, &key, part, ReasoningStatus::Running)?;
                }
                continue;
            };
            let Some(event) = turn_event(event, &key, thread_id, &self.commands)? else {
                continue;
            };
            match event {
                TurnEvent::Started { started_at } => {
                    let started_at = started_at.and_then(seconds);
                    let sequence = self.commands.record_provider_turn(
                        thread_id,
                        &claim.run_id,
                        &key,
                        NativeTurnUpdate::Started { started_at },
                    )?;
                    self.publish(thread_id, require_current(sequence)?);
                    native_start_seen = true;
                }
                TurnEvent::TextStarted {
                    item_id,
                    phase,
                    started_at_ms,
                } => {
                    output.started(item_id, phase, started_at_ms.and_then(milliseconds));
                }
                TurnEvent::TextDelta { item_id, delta } => {
                    output.append(item_id, &delta, Instant::now());
                }
                TurnEvent::TextCompleted {
                    item_id,
                    text,
                    phase,
                    ..
                } => {
                    let source = output.completed(&item_id, phase);
                    self.publish_optional(
                        thread_id,
                        self.commands.record_assistant(
                            thread_id,
                            &claim.run_id,
                            AssistantUpdate {
                                key: &key,
                                native_item_id: &item_id,
                                started_at: source.started_at,
                                updated_at: OffsetDateTime::now_utc(),
                                phase: source.phase,
                                snapshot: AssistantSnapshot::Completed { text: &text },
                            },
                        )?,
                    );
                }
                TurnEvent::ReasoningDelta {
                    item_id,
                    stream,
                    index,
                    delta,
                } => {
                    let part = ReasoningPart {
                        native_item_id: item_id,
                        stream,
                        index,
                    };
                    if let Some(part) = reasoning.append(part, &delta) {
                        self.commands.update_reasoning(
                            thread_id,
                            &claim.run_id,
                            &key,
                            part,
                            None,
                        )?;
                    }
                }
                TurnEvent::ReasoningCompleted {
                    item_id,
                    summary,
                    content,
                } => {
                    for part in reasoning.complete(&item_id, summary, content) {
                        self.write_reasoning(
                            claim,
                            thread_id,
                            &key,
                            &part,
                            ReasoningStatus::Completed,
                        )?;
                    }
                }
                TurnEvent::TokenUsageUpdated { usage } => {
                    let sequence = self.commands.record_provider_turn(
                        thread_id,
                        &claim.run_id,
                        &key,
                        NativeTurnUpdate::Usage(usage),
                    )?;
                    self.publish_optional(thread_id, sequence);
                }
                TurnEvent::ModelRerouted { .. } => {}
                TurnEvent::Settled {
                    status,
                    usage,
                    completed_at,
                    ..
                } => {
                    if !native_start_seen {
                        let sequence = self.commands.record_provider_turn(
                            thread_id,
                            &claim.run_id,
                            &key,
                            NativeTurnUpdate::Started {
                                started_at: accepted.started_at.and_then(seconds),
                            },
                        )?;
                        self.publish(thread_id, require_current(sequence)?);
                    }
                    let reasoning_status = if status == SettlementStatus::Completed {
                        ReasoningStatus::Completed
                    } else {
                        ReasoningStatus::Interrupted
                    };
                    for part in reasoning.finish() {
                        self.write_reasoning(claim, thread_id, &key, &part, reasoning_status)?;
                    }
                    let completed_at = completed_at.and_then(seconds);
                    let sequence = self.commands.record_provider_turn(
                        thread_id,
                        &claim.run_id,
                        &key,
                        NativeTurnUpdate::Settled {
                            status,
                            completed_at,
                            usage: &usage,
                        },
                    )?;
                    self.publish(thread_id, require_current(sequence)?);
                    if let Some(terminating) = terminating.take() {
                        let _ = running_terminals.record(terminating.await);
                    }
                    let containment_failure = if stopped_by_request {
                        let attempts = terminals::terminate(
                            &client,
                            &key.thread_id,
                            running_terminals.pending(),
                        )
                        .await;
                        running_terminals.record(attempts)
                    } else {
                        None
                    };
                    if let Some(closed) = self.commands.close_native_tools(
                        thread_id,
                        &claim.run_id,
                        &key,
                        status,
                        completed_at,
                    )? {
                        for item_id in closed.command_item_ids {
                            queue.command_output.finish(thread_id, &item_id);
                        }
                        self.publish(thread_id, closed.sequence);
                    }
                    let settled = if status == SettlementStatus::Completed {
                        require_current(self.commands.await_checkpoint(
                            thread_id,
                            &claim.run_id,
                            &key,
                        )?)?
                    } else if status == SettlementStatus::Interrupted
                        && let Some(sequence) =
                            self.commands.settle_requested_interrupt(thread_id, &key)?
                    {
                        sequence
                    } else {
                        self.commands
                            .abort_started_run(
                                &claim.effect_id,
                                AbortReason::NativeTerminal(status),
                            )?
                            .ok_or(WorkerError::Interrupted)?
                            .sequence
                    };
                    self.publish(thread_id, settled);
                    if let Some((mut effect, _)) = interrupt.take() {
                        let mut failure = containment_failure;
                        loop {
                            let Some(error) = failure else {
                                self.commands.complete_interrupt_effect(
                                    thread_id,
                                    &effect,
                                    &self.worker_id,
                                )?;
                                break;
                            };
                            let Some(delay) = self.commands.retry_interrupt_effect(
                                &effect,
                                &self.worker_id,
                                &error.to_string(),
                            )?
                            else {
                                break;
                            };
                            tokio::time::sleep(delay).await;
                            let Some(claimed) = self.commands.claim_settled_interrupt(
                                &effect,
                                thread_id,
                                &key,
                                &self.worker_id,
                            )?
                            else {
                                return Err(WorkerError::Interrupted);
                            };
                            effect = claimed;
                            let attempts = terminals::terminate(
                                &client,
                                &key.thread_id,
                                running_terminals.pending(),
                            )
                            .await;
                            failure = running_terminals.record(attempts);
                        }
                    }
                    self.commands.complete_inactive_interrupts(thread_id)?;
                    if let Some(title) = applying_title.take() {
                        title.await?;
                    }
                    if status == SettlementStatus::Failed {
                        return Ok(settled);
                    }
                    break;
                }
                TurnEvent::ApprovalRequested { request } => {
                    let file_prompt = if request.permissions().is_none()
                        && request.kind == t3_rune::providers::codex::ApprovalKind::FileChange
                    {
                        file_approval_prompt(&request, file_changes.get(&request.item_id))?
                    } else {
                        None
                    };
                    self.publish_request(
                        claim,
                        thread_id,
                        &key,
                        LiveRequest::Approval {
                            native: request,
                            file_prompt,
                        },
                        &mut requests,
                    )?;
                }
                TurnEvent::UserInputRequested { request } => {
                    self.publish_request(
                        claim,
                        thread_id,
                        &key,
                        LiveRequest::Input(request),
                        &mut requests,
                    )?;
                }
                TurnEvent::ToolChanged { item, lifecycle } if item.kind == "commandExecution" => {
                    running_terminals.update(&item, lifecycle);
                    let sequence =
                        self.commands
                            .record_native_tool(thread_id, &claim.run_id, &key, &item)?;
                    if lifecycle == ToolLifecycle::Completed && sequence.is_some() {
                        queue
                            .command_output
                            .finish(thread_id, &command_item_id(&item.id));
                    }
                    self.publish_optional(thread_id, sequence);
                }
                TurnEvent::ToolChanged {
                    mut item,
                    lifecycle,
                } if item.kind == "fileChange" => match lifecycle {
                    ToolLifecycle::Started => {
                        file_changes.insert(item.id.clone(), item);
                    }
                    ToolLifecycle::Completed => {
                        if let Some(started) = file_changes.remove(&item.id) {
                            item.started_at_ms = started.started_at_ms;
                        }
                        self.publish_optional(
                            thread_id,
                            self.commands.record_native_tool(
                                thread_id,
                                &claim.run_id,
                                &key,
                                &item,
                            )?,
                        );
                    }
                },
                TurnEvent::ToolOutputDelta { item_id, delta } => {
                    queue
                        .command_output
                        .append(thread_id, &command_item_id(&item_id), &delta);
                }
                TurnEvent::WebSearch(update) => {
                    self.publish_optional(
                        thread_id,
                        self.commands
                            .record_web_search(thread_id, &claim.run_id, &key, update)?,
                    );
                }
                TurnEvent::TodoList(update) => {
                    self.publish_optional(
                        thread_id,
                        self.commands
                            .record_todo_list(thread_id, &claim.run_id, &key, &update)?,
                    );
                }
                TurnEvent::ToolChanged { .. } => {
                    let _ = client.interrupt_turn(&key).await;
                    return Err(WorkerError::Unavailable(
                        "This Codex activity requires a different tool handler.",
                    ));
                }
            }
        }
        let checkpoint = format!("effect:checkpoint.capture:{}", claim.run_id);
        let completed = require_current(
            self.commands
                .capture_checkpoint(&checkpoint, &self.worker_id)
                .await?,
        )?;
        self.publish(thread_id, completed);
        self.publish_optional(thread_id, self.commands.promote_queued_worktree(thread_id)?);
        Ok(completed)
    }

    fn publish_request(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        key: &TurnKey,
        request: LiveRequest,
        requests: &mut BTreeMap<String, LiveRequest>,
    ) -> Result<(), WorkerError> {
        let recorded = self
            .commands
            .record_runtime_request(thread_id, &claim.run_id, key, &request)?
            .ok_or(WorkerError::Interrupted)?;
        requests.insert(recorded.id, request);
        self.publish(thread_id, recorded.sequence);
        Ok(())
    }

    async fn answer_requests(
        &self,
        thread_id: &str,
        key: &TurnKey,
        client: &CodexSessionClient,
        requests: &mut BTreeMap<String, LiveRequest>,
    ) -> Result<(), WorkerError> {
        loop {
            let mut delivered = Vec::new();
            for (request_id, request) in requests.iter() {
                let Some(effect) = self.commands.claim_runtime_response(
                    thread_id,
                    key,
                    request_id,
                    &self.worker_id,
                )?
                else {
                    continue;
                };
                let delivery = match (request, &effect.response) {
                    (
                        LiveRequest::Approval {
                            native: request, ..
                        },
                        ResponseValue::Approval { decision },
                    ) => client.answer_approval(request, *decision).await,
                    (LiveRequest::Input(request), response @ ResponseValue::Input { .. }) => {
                        client
                            .answer_user_input(
                                request,
                                response.native_answers().unwrap_or_default(),
                            )
                            .await
                    }
                    _ => {
                        self.commands.finish_runtime_response(
                            &effect,
                            &self.worker_id,
                            Some("The response does not match its live Codex request."),
                        )?;
                        return Err(WorkerError::Unavailable(
                            "The response does not match its live Codex request.",
                        ));
                    }
                };
                let failure = delivery
                    .as_ref()
                    .err()
                    .map(|_| "The Codex request ended before its response could be delivered.");
                self.commands
                    .finish_runtime_response(&effect, &self.worker_id, failure)?;
                delivery?;
                delivered.push(request_id.clone());
            }
            if delivered.is_empty() {
                return Ok(());
            }
            for request_id in delivered {
                requests.remove(&request_id);
            }
        }
    }

    fn flush_text(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        key: &TurnKey,
        output: &mut AssistantOutput,
    ) -> Result<(), WorkerError> {
        for (item_id, snapshot) in output.flush(Instant::now()) {
            let sequence = self.commands.record_assistant(
                thread_id,
                &claim.run_id,
                AssistantUpdate {
                    key,
                    native_item_id: &item_id,
                    started_at: snapshot.data.started_at,
                    updated_at: OffsetDateTime::now_utc(),
                    phase: snapshot.data.phase,
                    snapshot: AssistantSnapshot::Streaming {
                        visible_text: &snapshot.text,
                    },
                },
            )?;
            self.publish_optional(thread_id, sequence);
        }
        Ok(())
    }

    fn write_reasoning(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        key: &TurnKey,
        part: &PartOutput,
        status: ReasoningStatus,
    ) -> Result<(), WorkerError> {
        let sequence = self.commands.update_reasoning(
            thread_id,
            &claim.run_id,
            key,
            &part.part,
            Some(ReasoningSnapshot {
                text: &part.text,
                status,
            }),
        )?;
        self.publish_optional(thread_id, sequence);
        Ok(())
    }

    fn publish_optional(&self, thread_id: &str, sequence: Option<i64>) {
        if let Some(sequence) = sequence {
            self.publish(thread_id, sequence);
        }
    }

    fn publish(&self, thread_id: &str, _sequence: i64) {
        let _ = self
            .changes
            .send(ProjectionChange::Thread(thread_id.to_owned()));
    }
}

struct ActiveRuntime {
    key: ProcessKey,
    session: CodexSession,
    threads: Vec<String>,
    loaded: HashMap<String, ThreadId>,
}

#[derive(PartialEq, Eq)]
struct ProcessKey {
    executable: PathBuf,
    cwd: PathBuf,
    arguments: Vec<OsString>,
    environment: HashMap<OsString, OsString>,
}

impl From<&CodexLaunch> for ProcessKey {
    fn from(launch: &CodexLaunch) -> Self {
        Self {
            executable: launch.executable.clone(),
            cwd: launch.working_directory.clone(),
            arguments: launch.arguments.clone(),
            environment: launch.environment.clone(),
        }
    }
}

async fn await_native<T>(
    session: &mut CodexSession,
    request: impl Future<Output = Result<T, CodexAdapterError>>,
    pending: &mut VecDeque<CodexSessionEvent>,
) -> Result<T, WorkerError> {
    tokio::pin!(request);
    loop {
        tokio::select! {
            result = &mut request => return Ok(result?),
            event = session.next_event() => {
                if pending.len() == STARTUP_EVENT_CAPACITY {
                    return Err(WorkerError::Unavailable("Codex sent too many events before accepting the request."));
                }
                pending.push_back(event?);
            }
        }
    }
}

fn turn_event(
    event: CodexSessionEvent,
    expected: &TurnKey,
    app_thread_id: &str,
    commands: &TurnCommands,
) -> Result<Option<TurnEvent>, WorkerError> {
    match event {
        CodexSessionEvent::Turn { key, event } if &key == expected => Ok(Some(event)),
        CodexSessionEvent::Turn {
            key,
            event: TurnEvent::TokenUsageUpdated { .. },
        } if key.thread_id == expected.thread_id => Ok(None),
        CodexSessionEvent::Turn { key, event }
            if key.thread_id == expected.thread_id
                && match &event {
                    TurnEvent::WebSearch(_) | TurnEvent::TodoList(_) => matches!(
                        commands.native_turn_status(app_thread_id, &key)?.as_deref(),
                        Some("interrupted" | "completed" | "cancelled" | "failed"),
                    ),
                    TurnEvent::ToolOutputDelta { .. } => {
                        commands.native_turn_status(app_thread_id, &key)?.as_deref()
                            == Some("interrupted")
                    }
                    TurnEvent::ToolChanged { item, .. } if item.kind == "commandExecution" => {
                        commands.native_turn_status(app_thread_id, &key)?.as_deref()
                            == Some("interrupted")
                    }
                    _ => false,
                } =>
        {
            Ok(None)
        }
        CodexSessionEvent::Turn { .. } | CodexSessionEvent::UnsupportedRequest { .. } => Err(
            WorkerError::Unavailable("Codex sent an event outside the active turn."),
        ),
        CodexSessionEvent::ThreadModelChanged { .. }
        | CodexSessionEvent::RequestResolved { .. } => Ok(None),
    }
}

fn require_current(sequence: Option<i64>) -> Result<i64, WorkerError> {
    sequence.ok_or(WorkerError::Interrupted)
}

fn seconds(value: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(value).ok()
}

fn milliseconds(value: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1_000_000).ok()
}

async fn wait_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

#[derive(Clone)]
struct TextSource {
    started_at: OffsetDateTime,
    phase: Option<AssistantPhase>,
}

impl Default for TextSource {
    fn default() -> Self {
        Self {
            started_at: OffsetDateTime::now_utc(),
            phase: None,
        }
    }
}

#[derive(Default)]
struct Draft {
    source: TextSource,
    text: String,
    due: Option<Instant>,
}

struct AssistantOutput {
    drafts: HashMap<String, Draft>,
    filter: AssistantStreaming<TextSource>,
}

impl AssistantOutput {
    fn new(mode: StreamingMode) -> Self {
        Self {
            drafts: HashMap::new(),
            filter: AssistantStreaming::new(mode),
        }
    }

    fn started(
        &mut self,
        id: String,
        phase: Option<AssistantPhase>,
        started_at: Option<OffsetDateTime>,
    ) {
        let track = self.drafts.entry(id).or_default();
        track.source.phase = phase;
        if let Some(started_at) = started_at {
            track.source.started_at = started_at;
        }
    }

    fn append(&mut self, id: String, delta: &str, now: Instant) {
        if delta.is_empty() {
            return;
        }
        let track = self.drafts.entry(id).or_default();
        track.text.push_str(delta);
        track.due.get_or_insert(now + DELTA_FLUSH_INTERVAL);
    }

    fn completed(&mut self, id: &str, phase: Option<AssistantPhase>) -> TextSource {
        self.filter.complete(id);
        let mut source = self
            .drafts
            .remove(id)
            .map(|draft| draft.source)
            .unwrap_or_default();
        source.phase = phase.or(source.phase);
        source
    }

    fn next_due(&self) -> Option<Instant> {
        self.drafts
            .values()
            .filter_map(|draft| draft.due)
            .chain(self.filter.next_idle_flush_at())
            .min()
    }

    fn flush(&mut self, now: Instant) -> Vec<(String, Snapshot<TextSource>)> {
        let mut visible = Vec::new();
        for (id, draft) in &mut self.drafts {
            if draft.due.is_some_and(|due| due <= now) {
                draft.due = None;
                let snapshot = Snapshot {
                    text: draft.text.clone(),
                    data: draft.source.clone(),
                };
                if let Some(snapshot) = self.filter.filter(id, snapshot, now) {
                    visible.push((id.clone(), snapshot));
                }
            }
        }
        visible.extend(self.filter.take_idle_flushes(now));
        visible
    }
}

#[cfg(test)]
mod tests;
