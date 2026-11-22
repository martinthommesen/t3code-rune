CREATE TABLE orchestration_v2_effect_outbox_next (
  effect_id TEXT PRIMARY KEY,
  command_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  effect_type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  status TEXT NOT NULL CHECK (
    status IN ('pending', 'running', 'succeeded', 'failed', 'cancelled')
  ),
  attempt_count INTEGER NOT NULL DEFAULT 0,
  available_at TEXT NOT NULL,
  lease_owner TEXT,
  lease_expires_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  completed_at TEXT,
  last_error TEXT
);

INSERT INTO orchestration_v2_effect_outbox_next (
  effect_id,
  command_id,
  thread_id,
  effect_type,
  payload_json,
  status,
  attempt_count,
  available_at,
  lease_owner,
  lease_expires_at,
  created_at,
  updated_at,
  completed_at,
  last_error
)
SELECT
  effect_id,
  command_id,
  thread_id,
  effect_type,
  payload_json,
  status,
  attempt_count,
  available_at,
  lease_owner,
  lease_expires_at,
  created_at,
  updated_at,
  completed_at,
  last_error
FROM orchestration_v2_effect_outbox;

DROP TABLE orchestration_v2_effect_outbox;

ALTER TABLE orchestration_v2_effect_outbox_next
RENAME TO orchestration_v2_effect_outbox;

CREATE INDEX orchestration_v2_effect_outbox_claim_idx
ON orchestration_v2_effect_outbox(status, available_at, lease_expires_at, created_at);

CREATE INDEX orchestration_v2_effect_outbox_command_idx
ON orchestration_v2_effect_outbox(command_id, effect_id);

CREATE INDEX orchestration_v2_effect_outbox_thread_status_idx
ON orchestration_v2_effect_outbox(thread_id, status, effect_type);

CREATE TABLE IF NOT EXISTS scheduled_tasks (
  task_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  prompt TEXT NOT NULL,
  enabled INTEGER NOT NULL,
  schedule_json TEXT NOT NULL,
  project_id TEXT NOT NULL,
  thread_id TEXT,
  workspace_strategy_json TEXT NOT NULL,
  model_selection_json TEXT NOT NULL,
  runtime_mode TEXT NOT NULL,
  interaction_mode TEXT NOT NULL,
  created_by TEXT NOT NULL,
  creation_source TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  next_run_at TEXT,
  last_run_at TEXT,
  last_run_status TEXT NOT NULL,
  last_run_error TEXT,
  run_count INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_due
ON scheduled_tasks(enabled, next_run_at)
WHERE enabled = 1 AND next_run_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_project
ON scheduled_tasks(project_id, updated_at);

CREATE TABLE orchestration_v2_legacy_imports (
  thread_id TEXT PRIMARY KEY,
  source_updated_at TEXT NOT NULL,
  shell_imported_at TEXT NOT NULL,
  transcript_imported_at TEXT,
  imported_message_count INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);

CREATE INDEX orchestration_v2_legacy_imports_pending_transcript_idx
ON orchestration_v2_legacy_imports(transcript_imported_at, shell_imported_at, thread_id);

CREATE INDEX idx_orchestration_events_application_high_water
ON orchestration_events(sequence)
WHERE aggregate_kind = 'project'
  OR (application_event_version = 2 AND aggregate_kind = 'thread');

CREATE INDEX idx_orchestration_events_agent_stream_sequence
ON orchestration_events(stream_id, sequence)
WHERE application_event_version = 2 AND aggregate_kind = 'thread';

CREATE INDEX orchestration_events_v2_created_threads_idx
ON orchestration_events(stream_id)
WHERE application_event_version = 2
  AND aggregate_kind = 'thread'
  AND event_type = 'thread.created';

CREATE INDEX orchestration_v2_projection_runs_recovery_idx
ON orchestration_v2_projection_runs(status, thread_id)
WHERE status IN ('queued', 'preparing', 'starting', 'running', 'waiting');

CREATE INDEX orchestration_v2_projection_requests_recovery_idx
ON orchestration_v2_projection_runtime_requests(thread_id)
WHERE status = 'pending';

CREATE INDEX orchestration_v2_projection_turn_items_recovery_idx
ON orchestration_v2_projection_turn_items(thread_id)
WHERE type IN ('command_execution', 'dynamic_tool', 'subagent')
  AND status IN ('pending', 'running', 'waiting');

CREATE INDEX orchestration_v2_projection_turn_items_thread_run_idx
ON orchestration_v2_projection_turn_items(thread_id, run_id);

CREATE INDEX orchestration_v2_projection_turn_items_shell_pending_idx
ON orchestration_v2_projection_turn_items(thread_id, run_id)
WHERE type IN ('command_execution', 'dynamic_tool', 'subagent')
  AND status NOT IN ('completed', 'interrupted', 'failed', 'cancelled');

CREATE INDEX orchestration_v2_projection_messages_latest_user_idx
ON orchestration_v2_projection_messages(thread_id, updated_at DESC, message_id DESC)
WHERE role = 'user';
