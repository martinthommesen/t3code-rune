use super::Migration;

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration::sql(
        2,
        "OrchestrationCommandReceipts",
        include_str!("legacy_pre15/command_receipts.sql"),
    ),
    Migration::sql(
        3,
        "CheckpointDiffBlobs",
        include_str!("legacy_pre15/checkpoint_diff_blobs.sql"),
    ),
    Migration::sql(
        4,
        "ProviderSessionRuntime",
        include_str!("legacy_pre15/provider_session_runtime.sql"),
    ),
    Migration::sql(
        5,
        "Projections",
        include_str!("legacy_pre15/projections.sql"),
    ),
    Migration::sql(
        6,
        "ProjectionThreadSessionRuntimeModeColumns",
        r#"
ALTER TABLE projection_thread_sessions
ADD COLUMN runtime_mode TEXT NOT NULL DEFAULT 'full-access';

UPDATE projection_thread_sessions
SET runtime_mode = 'full-access'
WHERE runtime_mode IS NULL;
"#,
    ),
    Migration::sql(
        7,
        "ProjectionThreadMessageAttachments",
        r#"
ALTER TABLE projection_thread_messages
ADD COLUMN attachments_json TEXT;
"#,
    ),
    Migration::sql(
        8,
        "ProjectionThreadActivitySequence",
        r#"
ALTER TABLE projection_thread_activities
ADD COLUMN sequence INTEGER;

CREATE INDEX IF NOT EXISTS idx_projection_thread_activities_thread_sequence
ON projection_thread_activities(thread_id, sequence);
"#,
    ),
    // Migration 9 only recorded a receipt.
    Migration::sql(9, "ProviderSessionRuntimeMode", ""),
    Migration::sql(
        10,
        "ProjectionThreadsRuntimeMode",
        r#"
ALTER TABLE projection_threads
ADD COLUMN runtime_mode TEXT NOT NULL DEFAULT 'full-access';

UPDATE projection_threads
SET runtime_mode = 'full-access'
WHERE runtime_mode IS NULL;
"#,
    ),
    Migration::sql(
        11,
        "OrchestrationThreadCreatedRuntimeMode",
        r#"
UPDATE orchestration_events
SET payload_json = json_set(payload_json, '$.runtimeMode', 'full-access')
WHERE event_type = 'thread.created'
  AND json_type(payload_json, '$.runtimeMode') IS NULL;
"#,
    ),
    Migration::sql(
        12,
        "ProjectionThreadsInteractionMode",
        r#"
ALTER TABLE projection_threads
ADD COLUMN interaction_mode TEXT NOT NULL DEFAULT 'default';
"#,
    ),
    Migration::sql(
        13,
        "ProjectionThreadProposedPlans",
        r#"
CREATE TABLE IF NOT EXISTS projection_thread_proposed_plans (
  plan_id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  turn_id TEXT,
  plan_markdown TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_projection_thread_proposed_plans_thread_created
ON projection_thread_proposed_plans(thread_id, created_at);
"#,
    ),
    Migration::sql(
        14,
        "ProjectionThreadProposedPlanImplementation",
        r#"
ALTER TABLE projection_thread_proposed_plans
ADD COLUMN implemented_at TEXT;

ALTER TABLE projection_thread_proposed_plans
ADD COLUMN implementation_thread_id TEXT;
"#,
    ),
    Migration::sql(
        15,
        "ProjectionTurnsSourceProposedPlan",
        r#"
ALTER TABLE projection_turns
ADD COLUMN source_proposed_plan_thread_id TEXT;

ALTER TABLE projection_turns
ADD COLUMN source_proposed_plan_id TEXT;
"#,
    ),
];
