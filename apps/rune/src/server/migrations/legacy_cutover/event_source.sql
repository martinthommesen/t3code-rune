ALTER TABLE orchestration_events ADD COLUMN application_event_version INTEGER NOT NULL DEFAULT 1;
CREATE INDEX idx_orchestration_events_application_sequence
ON orchestration_events(application_event_version, sequence);
ALTER TABLE orchestration_command_receipts ADD COLUMN command_type TEXT NOT NULL DEFAULT 'legacy';

WITH source AS (
  SELECT previous.*,
    ROW_NUMBER() OVER (PARTITION BY previous.thread_id ORDER BY previous.sequence) - 1
      + COALESCE((
        SELECT MAX(stream_version) + 1 FROM orchestration_events
        WHERE aggregate_kind = 'thread' AND stream_id = previous.thread_id
      ), 0) AS version
  FROM orchestration_v2_events AS previous
  WHERE NOT EXISTS (SELECT 1 FROM orchestration_events WHERE event_id = previous.event_id)
)
INSERT INTO orchestration_events (
  event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at, command_id,
  causation_event_id, correlation_id, actor_kind, payload_json, metadata_json, application_event_version
)
SELECT event_id, 'thread', thread_id, version, event_type, occurred_at, command_id,
  NULL, command_id, IIF(raw_event_id IS NULL, 'server', 'provider'), payload_json,
  json_patch(json_object('applicationEventVersion', 2), json_object(
    'runId', run_id, 'nodeId', node_id, 'driver', driver,
    'providerInstanceId', provider_instance_id, 'rawEventId', raw_event_id
  )), 2
FROM source ORDER BY sequence;

WITH projects AS MATERIALIZED (
  SELECT projection_projects.*,
    'migration:39:project:' || project_id || ':baseline' AS baseline_id,
    COALESCE((
      SELECT MAX(stream_version) + 1 FROM orchestration_events
      WHERE aggregate_kind = 'project' AND stream_id = projection_projects.project_id
    ), 0) AS version
  FROM projection_projects
),
events AS (
  SELECT project_id, created_at, baseline_id AS event_id, version, 'project.created' AS event_type,
    updated_at AS occurred_at, NULL AS causation_event_id,
    json_object(
      'projectId', project_id, 'title', title, 'workspaceRoot', workspace_root,
      'defaultModelSelection', json(default_model_selection_json), 'scripts', json(scripts_json),
      'createdAt', created_at, 'updatedAt', updated_at
    ) AS payload
  FROM projects
  UNION ALL
  SELECT project_id, created_at, 'migration:39:project:' || project_id || ':deleted',
    version + 1, 'project.deleted', deleted_at, baseline_id,
    json_object('projectId', project_id, 'deletedAt', deleted_at)
  FROM projects WHERE deleted_at IS NOT NULL
)
INSERT INTO orchestration_events (
  event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
  command_id, causation_event_id, correlation_id, actor_kind, payload_json,
  metadata_json, application_event_version
)
SELECT event_id, 'project', project_id, version, event_type, occurred_at,
  NULL, causation_event_id, NULL, 'server', payload,
  '{"applicationEventVersion":2,"migration":39}', 2
FROM events ORDER BY created_at, project_id, version;

INSERT INTO orchestration_command_receipts (
  command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, error, command_type
)
SELECT command_id, 'thread', thread_id, accepted_at,
  COALESCE((
    SELECT MAX(events.sequence) FROM orchestration_events AS events
    WHERE events.application_event_version = 2 AND events.command_id = receipts.command_id
  ), 0), status, error, command_type
FROM orchestration_v2_command_receipts AS receipts
WHERE TRUE
ON CONFLICT(command_id) DO NOTHING;
