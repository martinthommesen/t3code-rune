mod active;

use std::path::PathBuf;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use t3_rune::providers::claude::SessionTarget;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    CLAUDE_INSTANCE, NativeProvider, ThreadProjection, TurnCommandError, TurnCommands, TurnIds,
    timestamp, validate_claude_turn,
};

pub(in crate::server) struct Claim {
    pub(in crate::server) effect_id: String,
    pub(in crate::server) worker_id: String,
    pub(in crate::server) thread_id: String,
    pub(in crate::server) project_id: String,
    pub(in crate::server) checkpoint_scope_id: String,
    pub(in crate::server) workspace: PathBuf,
    pub(in crate::server) ordinal: i64,
    pub(in crate::server) model: String,
    pub(in crate::server) title: String,
    pub(in crate::server) text: String,
    pub(in crate::server) target: SessionTarget,
    thread: ThreadProjection,
    ids: TurnIds,
}

impl TurnCommands {
    pub(in crate::server) fn requeue_unoffered_claude_start(
        &self,
        effect_id: Option<&str>,
        worker_id: Option<&str>,
    ) -> Result<usize, TurnCommandError> {
        let db = self.connect()?;
        let now = timestamp(OffsetDateTime::now_utc());
        Ok(db.execute("UPDATE orchestration_v2_effect_outbox AS effect
            SET status='pending',lease_owner=NULL,available_at=?1,updated_at=?1,last_error=NULL
            WHERE effect_type='provider-turn.start' AND status='running'
              AND (?2 IS NULL OR effect_id=?2) AND (?3 IS NULL OR lease_owner=?3)
              AND EXISTS (SELECT 1 FROM orchestration_v2_projection_runs AS run
                WHERE run.thread_id=effect.thread_id AND run.run_id=json_extract(effect.payload_json,'$.runId')
                  AND run.provider_instance_id='claudeAgent' AND run.status='starting')",
            params![now,effect_id,worker_id])?)
    }

    pub(in crate::server) fn fail_claude_start(
        &self,
        claim: &Claim,
        detail: &str,
    ) -> Result<Option<i64>, TurnCommandError> {
        self.fail_starting_run(
            &claim.effect_id,
            &claim.worker_id,
            &claim.ids.run_id,
            CLAUDE_INSTANCE,
            super::start_failure::StartFailure::SessionOpen,
            detail,
        )
    }

    pub(in crate::server) fn claim_claude_start(
        &self,
        effect_id: &str,
        worker_id: &str,
    ) -> Result<Option<Claim>, TurnCommandError> {
        if worker_id.trim().is_empty() {
            return Err(TurnCommandError::Unsupported(
                "A provider start requires a worker ID.",
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp(OffsetDateTime::now_utc());
        let Some((
            thread_id,
            project_id,
            thread,
            ordinal,
            run,
            scope,
            workspace,
            message_id,
            text,
            provider_thread_id,
            provider_session_id,
            session_id,
            context,
        )) = transaction
            .query_row(READY, params![effect_id, now], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            })
            .optional()?
        else {
            return Ok(None);
        };
        let thread: Value = serde_json::from_str(&thread)?;
        let projected: ThreadProjection = serde_json::from_value(thread.clone())?;
        let Some(model) = projected.model_selection["model"].as_str() else {
            return Ok(None);
        };
        let Some(title) = thread["title"].as_str() else {
            return Ok(None);
        };
        let ids = TurnIds::for_provider(
            &thread_id,
            &message_id,
            ordinal,
            Some(&provider_thread_id),
            NativeProvider::Claude,
            Some(&provider_session_id),
        );
        if ids.run_id != run || ids.checkpoint_scope_id != scope {
            return Ok(None);
        }
        let target = match session_id {
            Some(id) => SessionTarget::Resume {
                session_id: Uuid::parse_str(&id).map_err(|_| {
                    TurnCommandError::Unsupported("The Claude Code session ID is invalid.")
                })?,
                resume_at: None,
            },
            None => SessionTarget::New(Uuid::new_v4()),
        };
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'running', attempt_count = attempt_count + 1, lease_owner = ?2,
                 updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1",
            params![effect_id, worker_id, now],
        )?;
        transaction.commit()?;
        Ok(Some(Claim {
            effect_id: effect_id.to_owned(),
            worker_id: worker_id.to_owned(),
            thread_id,
            project_id,
            checkpoint_scope_id: scope,
            workspace: workspace.into(),
            ordinal,
            model: model.into(),
            title: title.into(),
            text: crate::composer_context::provider_text(text, context.as_deref()),
            target,
            thread: projected,
            ids,
        }))
    }
}

impl Claim {
    pub(in crate::server) fn config(
        &self,
        settings: &super::super::settings::SettingsSnapshot,
    ) -> Result<super::super::settings::ClaudeInstance, TurnCommandError> {
        let config = settings
            .claude_instance(Some(&self.project_id), CLAUDE_INSTANCE)
            .ok_or(TurnCommandError::Unsupported(
                "The selected Claude Code instance is unavailable.",
            ))?;
        validate_claude_turn(&self.thread, &self.text, &config)?;
        Ok(config)
    }
}

const READY: &str = "
    SELECT thread.thread_id, thread.project_id, thread.payload_json, run.ordinal, run.run_id,
           scope.scope_id, json_extract(scope.payload_json, '$.cwd'), message.message_id,
           json_extract(message.payload_json, '$.text'), provider.provider_thread_id,
           provider.provider_session_id, json_extract(provider.payload_json, '$.nativeThreadRef.nativeId'),
           json_extract(message.payload_json, '$.context')
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_command_receipts AS receipt
      ON receipt.command_id = effect.command_id AND receipt.aggregate_kind = 'thread'
         AND receipt.aggregate_id = effect.thread_id
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = effect.thread_id
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
      AND run.run_id = json_extract(effect.payload_json, '$.runId')
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId') AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    JOIN orchestration_v2_projection_checkpoint_scopes AS scope
      ON scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId')
    JOIN orchestration_v2_projection_messages AS message
      ON message.message_id = json_extract(run.payload_json, '$.userMessageId') AND message.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider
      ON provider.provider_thread_id = run.provider_thread_id AND provider.thread_id = thread.thread_id
    WHERE effect.effect_id = ?1 AND effect.available_at <= ?2
      AND effect.status = 'pending' AND effect.effect_type = 'provider-turn.start'
      AND json_extract(effect.payload_json, '$.type') = 'provider-turn.start'
      AND receipt.status = 'accepted'
      AND (receipt.command_type = 'message.dispatch'
           OR (receipt.command_type = 'prepared-run.release'
               AND run.ordinal = 1
               AND json_extract(run.payload_json, '$.workspacePreparation.type') = 'root'
               AND EXISTS (
                   SELECT 1 FROM orchestration_command_receipts AS initial
                   WHERE receipt.command_id = initial.command_id || ':release'
                     AND initial.aggregate_kind = 'thread' AND initial.aggregate_id = thread.thread_id
                     AND initial.command_type = 'thread.create' AND initial.status = 'accepted'
               )))
      AND effect.effect_id = 'effect:' || receipt.command_id || ':provider-turn.start:' || run.run_id
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND project.archived_at IS NULL AND project.deleted_at IS NULL
      AND COALESCE(json_array_length(project.tracked_folders_json), 0) = 0
      AND json_extract(thread.payload_json, '$.worktreePath') IS NULL
      AND json_extract(thread.payload_json, '$.forkedFrom') IS NULL
      AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
      AND json_extract(thread.payload_json, '$.lineage.rootThreadId') = thread.thread_id
      AND json_extract(thread.payload_json, '$.providerInstanceId') = 'claudeAgent'
      AND thread.active_provider_thread_id = provider.provider_thread_id
      AND run.status = 'starting' AND run.provider_instance_id = 'claudeAgent' AND run.ordinal > 0
      AND attempt.attempt_ordinal = 1 AND attempt.status = 'pending' AND node.status = 'pending'
      AND scope.kind = 'root_run' AND json_extract(scope.payload_json, '$.cwd') = project.workspace_root
      AND message.role = 'user' AND COALESCE(json_array_length(message.payload_json, '$.attachments'), 0) = 0
      AND provider.driver = 'claudeAgent' AND provider.provider_instance_id = 'claudeAgent'
      AND ((run.ordinal = 1 AND provider.status = 'not_loaded' AND json_extract(provider.payload_json, '$.nativeThreadRef') IS NULL)
           OR (run.ordinal > 1 AND provider.status = 'idle'
               AND json_extract(provider.payload_json, '$.nativeThreadRef.strength') = 'strong'))
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_effect_outbox AS active
        WHERE active.thread_id = effect.thread_id AND active.effect_type != 'thread-title.generate'
          AND (active.status = 'running' OR (active.status = 'pending' AND active.rowid < effect.rowid))
      )
";

#[cfg(test)]
mod tests {
    use super::super::tests::{COMMAND, Fixture, THREAD, command_payload};
    use super::*;
    use crate::server::settings::SettingsFile;
    use serde_json::json;
    use std::fs;

    #[test]
    fn records_claude_intent_and_binds_the_start_to_one_worker() {
        let fixture = Fixture::new();
        let db = fixture.connection();
        db.execute("UPDATE orchestration_v2_projection_threads
            SET provider_instance_id='claudeAgent', default_provider='claudeAgent', runtime_mode='full-access',
              payload_json=json_set(payload_json, '$.providerInstanceId', 'claudeAgent',
                '$.modelSelection.instanceId', 'claudeAgent', '$.modelSelection.model', 'claude-sonnet-4-6',
                '$.runtimeMode', 'full-access')", []).unwrap();
        let settings = SettingsFile::new(fixture.directory.join("settings.json"));
        let initial = settings.read().unwrap();
        let stock = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/multi_turn_restart/claude_transcript.ndjson"
        );
        let first_prompt = stock
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|entry| entry["label"] == "prompt.offer:1")
            .unwrap();
        let text = first_prompt["frame"]["message"]["message"]["content"]
            .as_str()
            .unwrap();
        let mut command = command_payload();
        command["text"] = json!(text);
        let commands = fixture.commands();
        let result = commands
            .dispatch_with_settings(
                serde_json::from_value(command.clone()).unwrap(),
                Some(&initial),
            )
            .unwrap();
        assert!(result.committed);
        assert!(
            !commands
                .dispatch_with_settings(serde_json::from_value(command).unwrap(), Some(&initial))
                .unwrap()
                .committed
        );
        assert_eq!(db.query_row("SELECT driver, provider_session_id FROM orchestration_v2_projection_provider_threads",[],|row| {
            let driver: String = row.get(0)?;
            let id: String = row.get(1)?;
            Ok((driver, id.starts_with("provider-session:provider-instance:claudeAgent:thread:")))
        }).unwrap(), ("claudeAgent".into(), true));
        let effect_id = format!("effect:{COMMAND}:provider-turn.start:{}", result.run_id);
        assert!(
            commands
                .claim_start(&effect_id, "codex-worker")
                .unwrap()
                .is_none()
        );
        let claim = commands
            .claim_claude_start(&effect_id, "claude-worker")
            .unwrap()
            .unwrap();
        assert_eq!(claim.thread_id, THREAD);
        assert_eq!(claim.text, text);
        assert_eq!(claim.workspace, fixture.directory);
        assert_eq!(claim.model, "claude-sonnet-4-6");
        assert!(matches!(claim.target, SessionTarget::New(_)));
        assert_eq!(claim.config(&initial).unwrap().binary_path, "claude");
        assert!(
            commands
                .claim_claude_start(&effect_id, "other-worker")
                .unwrap()
                .is_none()
        );
        fs::write(
            settings.path(),
            r#"{"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false}}}"#,
        )
        .unwrap();
        assert!(claim.config(&settings.read().unwrap()).is_err());
        assert_eq!(
            commands
                .requeue_unoffered_claude_start(Some(&effect_id), Some("other-worker"))
                .unwrap(),
            0
        );
        assert_eq!(
            commands.requeue_unoffered_claude_start(None, None).unwrap(),
            1
        );
        let recovered = commands
            .claim_claude_start(&effect_id, "replacement-worker")
            .unwrap()
            .unwrap();
        assert!(
            commands
                .bind_claude_turn(&recovered, None)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            commands.requeue_unoffered_claude_start(None, None).unwrap(),
            0
        );
        assert_eq!(commands.recover_active_provider_runs().unwrap(), [THREAD]);
        let states: (String,String) = db.query_row("SELECT run.status, effect.status
            FROM orchestration_v2_projection_runs AS run JOIN orchestration_v2_effect_outbox AS effect
             ON run.run_id=json_extract(effect.payload_json,'$.runId') WHERE effect.effect_id=?1", [&effect_id],
            |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(states, ("cancelled".into(), "cancelled".into()));
        assert!(
            commands
                .claim_claude_start(&effect_id, "third-worker")
                .unwrap()
                .is_none()
        );
    }
}
