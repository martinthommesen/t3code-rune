use rusqlite::Transaction;

use super::{Migration, citation_search, legacy_cutover, legacy_pull_requests, legacy_settlement};

pub(super) const BASE_HISTORY: &[(i64, &str)] = &[(38, "ProjectionThreadsPinOrderKey")];

pub(super) const PULL_REQUEST_FILES_VIEWED: Migration = Migration::sql(
    53,
    "PullRequestFilesViewed",
    r#"
CREATE TABLE IF NOT EXISTS pull_request_files_viewed (
  provider TEXT NOT NULL,
  host TEXT NOT NULL,
  repository TEXT NOT NULL,
  number INTEGER NOT NULL,
  viewer TEXT NOT NULL,
  path TEXT NOT NULL,
  revision TEXT,
  viewed_at TEXT NOT NULL,
  PRIMARY KEY (provider, host, repository, number, viewer, path)
) WITHOUT ROWID;
"#,
);

pub(super) const AUTO_SETTLE_DISABLED_AT: Migration =
    Migration::scripted(54, "ProjectionThreadsAutoSettleDisabledAt", migrate_54);

pub(super) const MIGRATIONS: &[Migration] = &[
    PULL_REQUEST_FILES_VIEWED,
    AUTO_SETTLE_DISABLED_AT,
    Migration::scripted(55, "OrchestrationV2", legacy_cutover::migrate_55),
    Migration::sql(
        56,
        "RemoveRedundantProjectionIndexes",
        r#"
DROP INDEX IF EXISTS idx_projection_threads_project_id;

DROP INDEX IF EXISTS idx_projection_thread_messages_thread_created;

DROP INDEX IF EXISTS idx_projection_thread_activities_thread_sequence;

DROP INDEX IF EXISTS idx_projection_turns_thread_requested;
"#,
    ),
    Migration::sql(
        57,
        "ScheduledTaskWebhooks",
        r#"
ALTER TABLE scheduled_tasks ADD COLUMN webhook_token TEXT;

ALTER TABLE scheduled_tasks ADD COLUMN webhook_secret TEXT;

CREATE TABLE IF NOT EXISTS scheduled_task_webhook_deliveries (
      delivery_id TEXT PRIMARY KEY,
      task_id TEXT NOT NULL,
      received_at TEXT NOT NULL,
      method TEXT NOT NULL,
      query TEXT NOT NULL,
      headers_json TEXT NOT NULL,
      body TEXT NOT NULL,
      body_bytes INTEGER NOT NULL,
      body_truncated INTEGER NOT NULL,
      outcome TEXT NOT NULL,
      signature_verified INTEGER NOT NULL,
      missing_fields_json TEXT NOT NULL,
      rendered_prompt TEXT,
      error TEXT
    );

CREATE INDEX IF NOT EXISTS idx_scheduled_task_webhook_deliveries_task
    ON scheduled_task_webhook_deliveries(task_id, received_at);
"#,
    ),
    Migration::sql(
        58,
        "WebhookRelayDeliveries",
        r#"
CREATE TABLE IF NOT EXISTS scheduled_task_webhook_relay_deliveries (
      relay_delivery_id TEXT PRIMARY KEY,
      task_id TEXT NOT NULL,
      seen_at TEXT NOT NULL
    );

CREATE INDEX IF NOT EXISTS idx_scheduled_task_webhook_relay_deliveries_seen
    ON scheduled_task_webhook_relay_deliveries(seen_at);
"#,
    ),
    Migration::sql(
        59,
        "McpAppModelContext",
        r#"
CREATE TABLE IF NOT EXISTS mcp_app_model_context (
      thread_id TEXT NOT NULL,
      item_id TEXT NOT NULL,
      server TEXT NOT NULL,
      tool TEXT NOT NULL,
      text TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      PRIMARY KEY (thread_id, item_id)
    );
"#,
    ),
    Migration::sql(
        60,
        "ThreadSnapshotWindowIndexes",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_turn_items_user_message_idx
    ON orchestration_v2_projection_turn_items(thread_id, ordinal, turn_item_id)
    WHERE type = 'user_message';

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_nodes_live_idx
    ON orchestration_v2_projection_nodes(thread_id)
    WHERE status IN ('pending', 'starting', 'running', 'waiting');
"#,
    ),
    Migration::sql(
        61,
        "PullRequestReviewGuides",
        r#"
CREATE TABLE IF NOT EXISTS pull_request_review_guides (
      host TEXT NOT NULL,
      repository TEXT NOT NULL,
      number INTEGER NOT NULL,
      guide_json TEXT NOT NULL,
      PRIMARY KEY (host, repository, number)
    ) WITHOUT ROWID;
"#,
    ),
    Migration::sql(
        62,
        "ThreadFailureLookupIndex",
        r#"
CREATE INDEX orchestration_v2_projection_turn_items_failed_error_idx
    ON orchestration_v2_projection_turn_items(
      thread_id, run_id, node_id, updated_at DESC, ordinal DESC, turn_item_id DESC
    )
    WHERE type = 'error' AND status = 'failed';
"#,
    ),
    Migration::sql(
        63,
        "DropUnusedV1Projections",
        r#"
DROP TABLE IF EXISTS projection_thread_activities;

DROP TABLE IF EXISTS projection_turns;

DROP TABLE IF EXISTS projection_thread_proposed_plans;

DROP TABLE IF EXISTS projection_pending_approvals;

DROP TABLE IF EXISTS projection_state;

DROP TABLE IF EXISTS checkpoint_diff_blobs;
"#,
    ),
    Migration::scripted(
        64,
        "CanonicalizeThreadPullRequestLinks",
        legacy_pull_requests::migrate_64,
    ),
    Migration::sql(
        65,
        "CodexUsageNativeTurnIndex",
        r#"
CREATE INDEX orchestration_v2_provider_turns_codex_native_id_idx
    ON orchestration_v2_projection_provider_turns (
      json_extract(payload_json, '$.nativeTurnRef.nativeId')
    )
    WHERE json_extract(payload_json, '$.nativeTurnRef.driver') = 'codex';
"#,
    ),
    Migration::scripted(
        66,
        "RepairClientSettlementTimestamps",
        legacy_settlement::migrate_66,
    ),
    Migration::sql(
        67,
        "WebhookDedupe",
        r#"
ALTER TABLE scheduled_tasks ADD COLUMN last_run_webhook_delivery_id TEXT;
CREATE TABLE scheduled_task_webhook_replay_keys (
  task_id TEXT NOT NULL REFERENCES scheduled_tasks(task_id) ON DELETE CASCADE,
  replay_key TEXT NOT NULL,
  delivery_id TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY (task_id, replay_key)
);
CREATE INDEX idx_webhook_replay_expiry ON scheduled_task_webhook_replay_keys(expires_at);
CREATE INDEX idx_webhook_replay_delivery ON scheduled_task_webhook_replay_keys(delivery_id);
CREATE TABLE scheduled_task_webhook_pending (
  delivery_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES scheduled_tasks(task_id) ON DELETE CASCADE,
  coalesce_key TEXT NOT NULL,
  due_at INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending',
  task_json TEXT NOT NULL,
  request_json TEXT NOT NULL,
  delivery_ids_json TEXT NOT NULL
);
CREATE INDEX idx_webhook_pending_due ON scheduled_task_webhook_pending(state, due_at);
CREATE UNIQUE INDEX idx_webhook_pending_key
ON scheduled_task_webhook_pending(task_id, coalesce_key) WHERE state = 'pending';
"#,
    ),
    Migration::sql(
        68,
        "ShellStopRequestIndex",
        r#"
CREATE INDEX orchestration_v2_projection_turn_items_stop_request_idx
ON orchestration_v2_projection_turn_items(thread_id, run_id)
WHERE type = 'run_interrupt_request';
"#,
    ),
    Migration::scripted(69, "ThreadMessageSearch", citation_search::migrate_69),
    Migration::sql(
        70,
        "PeerLinks",
        r#"
CREATE TABLE IF NOT EXISTS peer_environment_links (
  environment_id TEXT PRIMARY KEY,
  secret_name TEXT NOT NULL,
  label TEXT NOT NULL,
  urls_json TEXT NOT NULL,
  access TEXT NOT NULL,
  linked_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  last_reached_at TEXT,
  last_error TEXT
);
"#,
    ),
    Migration::sql(
        71,
        "PeerLinkFence",
        r#"
ALTER TABLE peer_environment_links ADD COLUMN fenced INTEGER NOT NULL DEFAULT 0;
"#,
    ),
    Migration::sql(
        72,
        "WaitingLinkRequestIndex",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_turn_items_waiting_link_idx
ON orchestration_v2_projection_turn_items(thread_id, updated_at DESC, turn_item_id DESC)
WHERE type = 'link_request' AND status = 'waiting';
"#,
    ),
    Migration::sql(
        73,
        "ActiveTurnItemIndexes",
        r#"
CREATE INDEX orchestration_v2_projection_turn_items_active_ordinal_idx
ON orchestration_v2_projection_turn_items(thread_id, ordinal, turn_item_id)
WHERE status IN ('pending', 'running', 'waiting');

CREATE INDEX orchestration_v2_projection_turn_items_active_run_idx
ON orchestration_v2_projection_turn_items(thread_id, run_id)
WHERE status IN ('pending', 'running', 'waiting');
"#,
    ),
    Migration::scripted(
        74,
        "ExcludeAttachmentPlaceholdersFromSearch",
        citation_search::migrate_74,
    ),
    Migration::sql(
        75,
        "SubagentDeliveryState",
        r#"
ALTER TABLE orchestration_v2_projection_subagents ADD COLUMN completion_delivery_state TEXT;

UPDATE orchestration_v2_projection_subagents
SET completion_delivery_state = json_extract(payload_json, '$.completionDelivery.state')
WHERE json_type(payload_json, '$.completionDelivery') IS NOT NULL;

CREATE INDEX orchestration_v2_projection_subagents_settled_idx
ON orchestration_v2_projection_subagents(thread_id, status, completion_delivery_state, run_id);
"#,
    ),
    Migration::sql(
        76,
        "ProjectionProjectsPinArchive",
        r#"
ALTER TABLE projection_projects ADD COLUMN pinned_at TEXT;

ALTER TABLE projection_projects ADD COLUMN archived_at TEXT;
"#,
    ),
    Migration::sql(
        77,
        "RepairBooleanSettledOverride",
        r#"
UPDATE orchestration_v2_projection_threads
SET payload_json = json_set(payload_json, '$.settledOverride',
  IIF(json_type(payload_json, '$.settledOverride') = 'true', 'settled', NULL))
WHERE json_valid(payload_json)
  AND json_type(payload_json, '$.settledOverride') IN ('true', 'false');
"#,
    ),
    Migration::sql(
        78,
        "DropOrchestrationEventCorrelationIndex",
        r#"
DROP INDEX IF EXISTS idx_orch_events_correlation_id;
"#,
    ),
    Migration::sql(
        79,
        "DelegatedRecoveryIndexes",
        r#"
CREATE INDEX orchestration_v2_projection_threads_subagent_result_idx
ON orchestration_v2_projection_threads(
  thread_id, json_extract(payload_json, '$.lineage.parentThreadId')
)
WHERE CASE WHEN json_valid(payload_json) THEN
  json_extract(payload_json, '$.lineage.relationshipToParent') = 'subagent'
  AND json_extract(payload_json, '$.lineage.parentThreadId') IS NOT NULL
  AND json_extract(payload_json, '$.forkedFrom.type') = 'node'
  ELSE 0 END;

CREATE INDEX orchestration_v2_projection_runs_delegated_delivery_idx
ON orchestration_v2_projection_runs(thread_id)
WHERE CASE WHEN json_valid(payload_json)
  THEN json_type(payload_json, '$.delegatedCompletion.delivery') = 'object'
  ELSE 0 END;
"#,
    ),
    Migration::sql(
        80,
        "ScheduledTaskThreadMode",
        r#"
ALTER TABLE scheduled_tasks ADD COLUMN thread_mode TEXT NOT NULL DEFAULT 'new_thread';

UPDATE scheduled_tasks SET thread_mode = 'continue' WHERE thread_id IS NOT NULL;
"#,
    ),
    Migration::scripted(
        81,
        "ProjectionMessageSearchText",
        citation_search::migrate_81,
    ),
    Migration::sql(
        82,
        "TurnItemConversationIndex",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_turn_items_conversation_idx
ON orchestration_v2_projection_turn_items(thread_id, ordinal, type, run_id)
WHERE type NOT IN (
  'command_execution', 'reasoning', 'file_change', 'file_search', 'web_search', 'dynamic_tool'
);
"#,
    ),
    Migration::sql(
        83,
        "CommandReadIndexes",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_nodes_settled_leaf_idx
ON orchestration_v2_projection_nodes(
  thread_id,
  (kind IN ('tool_call', 'assistant_message', 'reasoning')
    AND status IN ('completed', 'interrupted', 'failed', 'cancelled', 'rolled_back')
    AND run_id IS NOT NULL)
);

DROP INDEX IF EXISTS orchestration_v2_projection_context_transfers_source_thread_idx;

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_context_transfers_source_thread_status_idx
ON orchestration_v2_projection_context_transfers(source_thread_id, status);
"#,
    ),
    Migration::sql(
        84,
        "LastExecutedRunIndex",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_runs_last_executed_idx
ON orchestration_v2_projection_runs(
  thread_id, (completed_at IS NULL) DESC, completed_at DESC, ordinal DESC, run_id DESC
)
WHERE status <> 'queued'
  AND NOT (status = 'cancelled' AND json_extract(payload_json, '$.startedAt') IS NULL);
"#,
    ),
    Migration::sql(
        85,
        "TurnItemCounts",
        r#"
CREATE TABLE orchestration_v2_projection_turn_item_counts (
  thread_id TEXT NOT NULL,
  run_key TEXT NOT NULL,
  item_count INTEGER NOT NULL,
  PRIMARY KEY (thread_id, run_key)
) WITHOUT ROWID;

INSERT INTO orchestration_v2_projection_turn_item_counts (thread_id, run_key, item_count)
SELECT thread_id, COALESCE(run_id, ''), COUNT(*)
FROM orchestration_v2_projection_turn_items
GROUP BY thread_id, run_id;

CREATE TRIGGER orchestration_v2_turn_item_counts_insert
AFTER INSERT ON orchestration_v2_projection_turn_items
BEGIN
  INSERT INTO orchestration_v2_projection_turn_item_counts (thread_id, run_key, item_count)
  VALUES (new.thread_id, COALESCE(new.run_id, ''), 1)
  ON CONFLICT (thread_id, run_key) DO UPDATE SET item_count = item_count + 1;
END;

CREATE TRIGGER orchestration_v2_turn_item_counts_update
AFTER UPDATE OF thread_id, run_id ON orchestration_v2_projection_turn_items
WHEN old.thread_id IS NOT new.thread_id OR old.run_id IS NOT new.run_id
BEGIN
  UPDATE orchestration_v2_projection_turn_item_counts SET item_count = item_count - 1
  WHERE thread_id = old.thread_id AND run_key = COALESCE(old.run_id, '');
  DELETE FROM orchestration_v2_projection_turn_item_counts
  WHERE thread_id = old.thread_id AND run_key = COALESCE(old.run_id, '') AND item_count <= 0;
  INSERT INTO orchestration_v2_projection_turn_item_counts (thread_id, run_key, item_count)
  VALUES (new.thread_id, COALESCE(new.run_id, ''), 1)
  ON CONFLICT (thread_id, run_key) DO UPDATE SET item_count = item_count + 1;
END;

CREATE TRIGGER orchestration_v2_turn_item_counts_delete
AFTER DELETE ON orchestration_v2_projection_turn_items
BEGIN
  UPDATE orchestration_v2_projection_turn_item_counts SET item_count = item_count - 1
  WHERE thread_id = old.thread_id AND run_key = COALESCE(old.run_id, '');
  DELETE FROM orchestration_v2_projection_turn_item_counts
  WHERE thread_id = old.thread_id AND run_key = COALESCE(old.run_id, '') AND item_count <= 0;
END;
"#,
    ),
    Migration::sql(
        86,
        "QueuedTurnInputIndex",
        r#"
CREATE INDEX orchestration_v2_projection_turn_items_queued_input_idx
ON orchestration_v2_projection_turn_items(run_id)
WHERE type = 'user_message'
  AND json_valid(payload_json)
  AND json_extract(payload_json, '$.inputIntent') = 'queued_turn';
"#,
    ),
    Migration::sql(
        87,
        "ProjectionProjectsCustomTitle",
        r#"
ALTER TABLE projection_projects
ADD COLUMN has_custom_title INTEGER NOT NULL DEFAULT 0;

UPDATE projection_projects
SET has_custom_title = 1
WHERE project_id IN (
  SELECT stream_id FROM orchestration_events
  WHERE aggregate_kind = 'project'
    AND event_type = 'project.meta-updated'
    AND json_type(payload_json, '$.title') = 'text'
);
"#,
    ),
    Migration::sql(
        88,
        "TurnItemTypeStatusIndex",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_turn_items_thread_type_idx
ON orchestration_v2_projection_turn_items(thread_id, type, status);
"#,
    ),
    Migration::sql(
        89,
        "ShellThreadReadIndexes",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_threads_worktree_path_idx
ON orchestration_v2_projection_threads(
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.worktreePath') END
);

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_threads_root_thread_idx
ON orchestration_v2_projection_threads(
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.lineage.rootThreadId') END
);

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_runs_run_status_idx
ON orchestration_v2_projection_runs(run_id, status);
"#,
    ),
    Migration::sql(
        90,
        "AuthConnectDevices",
        r#"
ALTER TABLE auth_pairing_links
ADD COLUMN requires_connect_approval INTEGER NOT NULL DEFAULT 0;

ALTER TABLE auth_pairing_links
ADD COLUMN requested_client_json TEXT;

ALTER TABLE auth_sessions
ADD COLUMN proof_key_thumbprint TEXT;

CREATE TABLE IF NOT EXISTS auth_connect_devices (
  proof_key_thumbprint TEXT PRIMARY KEY,
  client_json TEXT,
  approved_at TEXT NOT NULL
);
"#,
    ),
    Migration::sql(
        91,
        "AuthConnectDeniedRequests",
        r#"
ALTER TABLE auth_pairing_links
ADD COLUMN connect_denied INTEGER NOT NULL DEFAULT 0;
"#,
    ),
    Migration::sql(
        92,
        "QueuedTurnInputIndexJsonValid",
        r#"
DROP INDEX IF EXISTS orchestration_v2_projection_turn_items_queued_input_idx;

CREATE INDEX orchestration_v2_projection_turn_items_queued_input_idx
ON orchestration_v2_projection_turn_items(run_id)
WHERE type = 'user_message'
  AND json_valid(payload_json)
  AND json_extract(payload_json, '$.inputIntent') = 'queued_turn';
"#,
    ),
    Migration::sql(
        93,
        "ProviderTurnAttemptIndex",
        r#"
CREATE INDEX orchestration_v2_projection_provider_turns_attempt_idx
ON orchestration_v2_projection_provider_turns(run_attempt_id);
"#,
    ),
    Migration::sql(
        94,
        "TurnItemMessageOrdinalIndex",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_turn_items_message_ordinal_idx
ON orchestration_v2_projection_turn_items(
  thread_id, json_extract(payload_json, '$.messageId'), ordinal
)
WHERE type IN ('user_message', 'assistant_message') AND json_valid(payload_json);
"#,
    ),
    Migration::sql(
        95,
        "TurnItemInlineWorkOutput",
        r#"
ALTER TABLE orchestration_v2_projection_turn_items
ADD COLUMN inline_work_output INTEGER NOT NULL DEFAULT 0;

UPDATE orchestration_v2_projection_turn_items
SET inline_work_output = 1
WHERE type = 'dynamic_tool' AND (
  payload_json LIKE '%htmlRender%' OR payload_json LIKE '%t3McpApp%'
  OR payload_json LIKE '%"viewedImagePath"%'
  OR payload_json LIKE '%"toolName":"%delegate_task"%'
  OR payload_json LIKE '%"toolName":"%send_to_user"%'
  OR payload_json LIKE '%sentMessage%');
"#,
    ),
    Migration::sql(
        96,
        "RepairStaleTitleRegeneration",
        r#"
UPDATE orchestration_v2_projection_threads
SET payload_json = json_set(payload_json, '$.titleRegeneration', json('null'))
WHERE json_valid(payload_json)
  AND json_type(payload_json, '$.titleRegeneration') = 'object'
  AND NOT EXISTS (
    SELECT 1 FROM orchestration_v2_effect_outbox AS job
    WHERE job.effect_type = 'thread-title.generate'
      AND job.thread_id = orchestration_v2_projection_threads.thread_id
      AND job.command_id =
        json_extract(orchestration_v2_projection_threads.payload_json, '$.titleRegeneration.requestId')
      AND job.status IN ('pending', 'running')
  );
"#,
    ),
    Migration::sql(
        97,
        "IssueTrackerBindings",
        r#"
CREATE TABLE IF NOT EXISTS issue_tracker_bindings (
  project_id TEXT PRIMARY KEY,
  tracker TEXT NOT NULL,
  scope TEXT NOT NULL,
  updated_at TEXT NOT NULL
) WITHOUT ROWID;
"#,
    ),
    Migration::sql(
        98,
        "ActiveShellIndexIndexes",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_threads_active_shell_idx
ON orchestration_v2_projection_threads(
  updated_at,
  thread_id,
  project_id,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.lineage.relationshipToParent') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.lineage.parentThreadId') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.lineage.rootThreadId') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.lineage.sideChat') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.settledOverride') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.settledAt') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.pinnedAt') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.pinOrderKey') END,
  CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '$.snoozedUntil') END,
  CASE WHEN json_valid(payload_json) THEN json_type(payload_json, '$.pullRequests[0]') END
)
WHERE deleted_at IS NULL AND archived_at IS NULL;

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_provider_threads_pending_tasks_idx
ON orchestration_v2_projection_provider_threads(thread_id)
WHERE CASE WHEN json_valid(payload_json)
  THEN json_array_length(payload_json, '$.pendingBackgroundTasks') > 0
  ELSE 0 END;
"#,
    ),
    Migration::sql(
        99,
        "BackgroundRecoveryIndexes",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_turn_items_usage_limit_idx
ON orchestration_v2_projection_turn_items(thread_id)
WHERE type = 'error' AND status = 'failed'
  AND CASE WHEN json_valid(payload_json) THEN
    json_extract(payload_json, '$.failure.class') = 'usage_limit'
    ELSE 0 END;
"#,
    ),
    Migration::scripted(100, "TurnStartReadIndexes", turn_start_indexes),
    Migration::sql(
        101,
        "DelegatedTaskReadIndexes",
        r#"
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_runs_delegated_task_idx
ON orchestration_v2_projection_runs(thread_id, json_extract(payload_json, '$.delegatedTaskId'))
WHERE json_valid(payload_json) AND json_extract(payload_json, '$.delegatedTaskId') IS NOT NULL;

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_messages_delegated_task_idx
ON orchestration_v2_projection_messages(thread_id, created_at, message_id)
WHERE role = 'user' AND json_valid(payload_json) AND (
  json_extract(payload_json, '$.notification.source.kind') = 'monitor'
  OR json_extract(payload_json, '$.delegatedCompletion') IS NOT NULL
);
"#,
    ),
    Migration::sql(
        102,
        "ThreadItemCounts",
        r#"
CREATE TABLE orchestration_v2_projection_thread_item_counts (
  thread_id TEXT PRIMARY KEY,
  item_count INTEGER NOT NULL
) WITHOUT ROWID;

INSERT INTO orchestration_v2_projection_thread_item_counts (thread_id, item_count)
SELECT counts.thread_id, SUM(counts.item_count)
FROM orchestration_v2_projection_turn_item_counts counts
WHERE counts.run_key = ''
  OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs WHERE run_id = counts.run_key)
GROUP BY counts.thread_id;

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_runs_hidden_items_idx
ON orchestration_v2_projection_runs(thread_id, status, run_id)
WHERE status IN ('rolled_back', 'queued', 'cancelled');

CREATE TRIGGER orchestration_v2_thread_item_counts_count_insert
AFTER INSERT ON orchestration_v2_projection_turn_item_counts
WHEN new.run_key = ''
  OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs WHERE run_id = new.run_key)
BEGIN
  INSERT INTO orchestration_v2_projection_thread_item_counts (thread_id, item_count)
  VALUES (new.thread_id, new.item_count)
  ON CONFLICT (thread_id) DO UPDATE SET item_count = item_count + excluded.item_count;
END;

CREATE TRIGGER orchestration_v2_thread_item_counts_count_update
AFTER UPDATE ON orchestration_v2_projection_turn_item_counts
BEGIN
  UPDATE orchestration_v2_projection_thread_item_counts
  SET item_count = item_count - old.item_count
  WHERE thread_id = old.thread_id
    AND (old.run_key = ''
      OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs WHERE run_id = old.run_key));
  INSERT INTO orchestration_v2_projection_thread_item_counts (thread_id, item_count)
  SELECT new.thread_id, new.item_count
  WHERE new.run_key = ''
    OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs WHERE run_id = new.run_key)
  ON CONFLICT (thread_id) DO UPDATE SET item_count = item_count + excluded.item_count;
  DELETE FROM orchestration_v2_projection_thread_item_counts
  WHERE thread_id = old.thread_id AND item_count <= 0;
END;

CREATE TRIGGER orchestration_v2_thread_item_counts_count_delete
AFTER DELETE ON orchestration_v2_projection_turn_item_counts
WHEN old.run_key = ''
  OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs WHERE run_id = old.run_key)
BEGIN
  UPDATE orchestration_v2_projection_thread_item_counts
  SET item_count = item_count - old.item_count
  WHERE thread_id = old.thread_id;
  DELETE FROM orchestration_v2_projection_thread_item_counts
  WHERE thread_id = old.thread_id AND item_count <= 0;
END;

CREATE TRIGGER orchestration_v2_thread_item_counts_run_insert
AFTER INSERT ON orchestration_v2_projection_runs
BEGIN
  INSERT INTO orchestration_v2_projection_thread_item_counts (thread_id, item_count)
  SELECT thread_id, COUNT(*)
  FROM orchestration_v2_projection_turn_items
  WHERE run_id = new.run_id
  GROUP BY thread_id
  ON CONFLICT (thread_id) DO UPDATE SET item_count = item_count + excluded.item_count;
END;

CREATE TRIGGER orchestration_v2_thread_item_counts_run_delete
AFTER DELETE ON orchestration_v2_projection_runs
BEGIN
  UPDATE orchestration_v2_projection_thread_item_counts
  SET item_count = item_count - (
    SELECT COUNT(*)
    FROM orchestration_v2_projection_turn_items item
    WHERE item.run_id = old.run_id
      AND item.thread_id = orchestration_v2_projection_thread_item_counts.thread_id
  )
  WHERE thread_id IN (
    SELECT thread_id FROM orchestration_v2_projection_turn_items WHERE run_id = old.run_id
  );
  DELETE FROM orchestration_v2_projection_thread_item_counts
  WHERE item_count <= 0 AND thread_id IN (
    SELECT thread_id FROM orchestration_v2_projection_turn_items WHERE run_id = old.run_id
  );
END;
"#,
    ),
    Migration::sql(
        103,
        "IssueTrackerAdditionalScopes",
        r#"
ALTER TABLE issue_tracker_bindings ADD COLUMN additional_scopes TEXT NOT NULL DEFAULT '[]';
"#,
    ),
    Migration::sql(
        104,
        "ProjectionProjectsTrackedFolders",
        r#"
ALTER TABLE projection_projects
ADD COLUMN tracked_folders_json TEXT NOT NULL DEFAULT '[]';
"#,
    ),
    Migration::sql(
        105,
        "TaskFolders",
        r#"
CREATE TABLE IF NOT EXISTS task_folders (
  root TEXT PRIMARY KEY,
  real_root TEXT NOT NULL,
  project_id TEXT NOT NULL,
  project_root TEXT NOT NULL,
  anchor TEXT NOT NULL,
  project_path TEXT NOT NULL,
  project_is_worktree INTEGER NOT NULL,
  repositories_json TEXT NOT NULL DEFAULT '[]',
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_task_folders_real_root ON task_folders (real_root);
"#,
    ),
];

fn migrate_54(database: &Transaction<'_>) -> rusqlite::Result<()> {
    let added: bool = database.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('projection_threads')
         WHERE name = 'auto_settle_disabled_at')",
        [],
        |row| row.get(0),
    )?;
    if !added {
        database.execute_batch(
            "ALTER TABLE projection_threads ADD COLUMN auto_settle_disabled_at TEXT",
        )?;
    }
    Ok(())
}

fn turn_start_indexes(database: &Transaction<'_>) -> rusqlite::Result<()> {
    let whitespace = " \t\n\r\x0b\x0c\u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
    let text = "json_extract(payload_json, '$.text')";
    database.execute_batch(&format!(
        "
CREATE INDEX IF NOT EXISTS orchestration_v2_projection_messages_deferred_command_idx
ON orchestration_v2_projection_messages(thread_id, created_at, message_id)
WHERE role = 'user' AND (
  lower(trim({text}, '{whitespace}')) = '/compact'
  OR trim({text}, '{whitespace}') GLOB '/goal*'
);

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_subagents_routable_idx
ON orchestration_v2_projection_subagents(thread_id, subagent_id)
WHERE origin = 'provider_native' AND status = 'completed';

CREATE INDEX IF NOT EXISTS orchestration_v2_projection_context_transfers_fork_idx
ON orchestration_v2_projection_context_transfers(target_thread_id)
WHERE type = 'fork';
"
    ))
}
