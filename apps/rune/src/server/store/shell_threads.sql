WITH compaction_messages AS NOT MATERIALIZED (
    SELECT message_id
    FROM orchestration_v2_projection_messages
    WHERE role = 'user'
      AND lower(trim(json_extract(payload_json, '$.text'), char(
          9, 10, 11, 12, 13, 32, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197,
          8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288, 65279
      ))) = '/compact'
      AND coalesce(json_array_length(payload_json, '$.attachments'), 0) = 0
      AND (
          json_extract(payload_json, '$.idleCompaction') IS NOT NULL
          OR (
              json_extract(payload_json, '$.createdBy') = 'agent'
              AND json_extract(payload_json, '$.creationSource') = 'server'
          )
      )
), displayed_runs AS NOT MATERIALIZED (
    SELECT run.*
    FROM orchestration_v2_projection_runs AS run
    WHERE NOT (
        run.status <> 'failed'
        AND EXISTS (
            SELECT 1 FROM compaction_messages AS message
            WHERE message.message_id = json_extract(run.payload_json, '$.userMessageId')
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_turn_items AS compaction
            WHERE compaction.thread_id = run.thread_id AND compaction.run_id = run.run_id
              AND compaction.type = 'compaction' AND compaction.status = 'failed'
        )
    )
)
SELECT
  t.thread_id,
  t.payload_json,
  presented.run_id AS latest_run_id,
  presented.status AS latest_run_status,
  presented.requested_at AS latest_run_requested_at,
  json_extract(presented.payload_json, '$.startedAt') AS latest_run_started_at,
  presented.completed_at AS latest_run_completed_at,
  EXISTS (
    SELECT 1 FROM orchestration_v2_projection_turn_items stop_request
    WHERE stop_request.thread_id = t.thread_id AND stop_request.run_id = presented.run_id
      AND stop_request.type = 'run_interrupt_request'
  ) AS latest_run_stop_requested,
  EXISTS (
      SELECT 1 FROM orchestration_v2_projection_messages wake
      WHERE wake.message_id = json_extract(presented.payload_json, '$.userMessageId')
        AND json_extract(wake.payload_json, '$.notification.source.kind')
          IN ('background_task', 'background_command', 'command', 'monitor')
        AND json_extract(wake.payload_json, '$.notification.source.work') IS NULL
        AND json_extract(wake.payload_json, '$.notification.outcome') = 'updated'
        AND json_extract(wake.payload_json, '$.delegatedCompletion') IS NULL
      ) AND NOT EXISTS (
      SELECT 1 FROM orchestration_v2_projection_messages steer
      WHERE steer.run_id = presented.run_id
        AND json_extract(steer.payload_json, '$.createdBy') = 'user'
      ) AS latest_run_background_wake,
  active.run_id AS active_run_id,
  active.status AS active_run_status,
  (
    SELECT r.status
    -- Read active runs by status. Walking the ordinal index to skip this sort
    -- reads every run of a thread with none active.
    FROM displayed_runs r
    WHERE r.thread_id = t.thread_id
      AND r.status IN ('preparing', 'starting', 'running', 'waiting')
    ORDER BY r.ordinal DESC, r.run_id DESC
    LIMIT 1
  ) AS activity_run_status,
  (
    -- Mirrors orchestrationV2RunWorkStartedAt.
    SELECT COALESCE(
      json_extract(r.payload_json, '$.workStartedAt'),
      json_extract(r.payload_json, '$.startedAt'),
      r.requested_at
    )
    FROM displayed_runs r
    WHERE r.thread_id = t.thread_id
      AND r.status IN ('preparing', 'starting', 'running', 'waiting')
    ORDER BY r.ordinal DESC, r.run_id DESC
    LIMIT 1
  ) AS activity_run_started_at,
  (
    SELECT json_extract(session.payload_json, '$.lastError')
    -- Keep bindings outermost: one provider instance can hold every session.
    FROM orchestration_v2_projection_provider_session_bindings binding
    CROSS JOIN orchestration_v2_projection_provider_sessions session
      ON session.provider_session_id = binding.provider_session_id
    WHERE binding.thread_id = t.thread_id
      AND session.provider_instance_id = t.provider_instance_id
    ORDER BY session.updated_at DESC, session.provider_session_id DESC
    LIMIT 1
  ) AS last_error,
  (
    SELECT item.payload_json
    FROM orchestration_v2_projection_turn_items item
    WHERE presented.status = 'failed'
      AND item.thread_id = t.thread_id
      AND item.run_id = presented.run_id
      AND item.type = 'error' AND item.status = 'failed'
      AND item.node_id IS json_extract(presented.payload_json, '$.rootNodeId')
    ORDER BY item.updated_at DESC, item.ordinal DESC, item.turn_item_id DESC
    LIMIT 1
  ) AS terminal_failure_payload_json,
  blocked.run_id AS blocking_run_id,
  blocked.requested_at AS blocking_run_requested_at,
  json_extract(blocked.payload_json, '$.startedAt') AS blocking_run_started_at,
  blocked.completed_at AS blocking_run_completed_at,
  EXISTS (
    SELECT 1 FROM orchestration_v2_projection_turn_items stop_request
    WHERE stop_request.thread_id = t.thread_id AND stop_request.run_id = blocked.run_id
      AND stop_request.type = 'run_interrupt_request'
  ) AS blocking_run_stop_requested,
  EXISTS (
      SELECT 1 FROM orchestration_v2_projection_messages wake
      WHERE wake.message_id = json_extract(blocked.payload_json, '$.userMessageId')
        AND json_extract(wake.payload_json, '$.notification.source.kind')
          IN ('background_task', 'background_command', 'command', 'monitor')
        AND json_extract(wake.payload_json, '$.notification.source.work') IS NULL
        AND json_extract(wake.payload_json, '$.notification.outcome') = 'updated'
        AND json_extract(wake.payload_json, '$.delegatedCompletion') IS NULL
      ) AND NOT EXISTS (
      SELECT 1 FROM orchestration_v2_projection_messages steer
      WHERE steer.run_id = blocked.run_id
        AND json_extract(steer.payload_json, '$.createdBy') = 'user'
      ) AS blocking_run_background_wake,
  (
    SELECT item.payload_json
    FROM orchestration_v2_projection_turn_items item
    WHERE item.thread_id = t.thread_id
      AND item.run_id = blocked.run_id
      AND item.type = 'error' AND item.status = 'failed'
      AND item.node_id IS json_extract(blocked.payload_json, '$.rootNodeId')
    ORDER BY item.updated_at DESC, item.ordinal DESC, item.turn_item_id DESC
    LIMIT 1
  ) AS blocking_failure_payload_json,
  (
    SELECT request.payload_json
    FROM orchestration_v2_projection_runtime_requests request
    WHERE request.thread_id = t.thread_id
      AND request.status = 'pending'
    ORDER BY (request.kind = 'user_input') ASC,
      request.created_at DESC, request.runtime_request_id DESC
    LIMIT 1
  ) AS pending_request_payload_json,
  (
    SELECT secret.payload_json
    -- Keep runs outermost so completed history is never scanned for a pending secret.
    FROM orchestration_v2_projection_runs r
      INDEXED BY orchestration_v2_projection_runs_thread_status_idx
    CROSS JOIN orchestration_v2_projection_turn_items secret
      INDEXED BY orchestration_v2_projection_turn_items_thread_run_idx
    WHERE r.thread_id = t.thread_id
      AND r.status IN ('preparing', 'starting', 'running', 'waiting')
      AND secret.thread_id = t.thread_id AND secret.run_id = r.run_id
      AND secret.type = 'secret_request' AND secret.status = 'waiting'
    ORDER BY secret.updated_at DESC, secret.turn_item_id DESC
    LIMIT 1
  ) AS pending_secret_request_payload_json,
  (
    SELECT link.payload_json
    FROM orchestration_v2_projection_turn_items link
      INDEXED BY orchestration_v2_projection_turn_items_waiting_link_idx
    WHERE link.thread_id = t.thread_id
      AND link.type = 'link_request' AND link.status = 'waiting'
    ORDER BY link.updated_at DESC, link.turn_item_id DESC
    LIMIT 1
  ) AS pending_link_request_payload_json,
  (
    SELECT message.updated_at
    FROM orchestration_v2_projection_messages message
    WHERE message.thread_id = t.thread_id
      AND message.role = 'user'
      AND NOT EXISTS (SELECT 1 FROM compaction_messages AS idle WHERE idle.message_id = message.message_id)
    ORDER BY message.updated_at DESC, message.message_id DESC
    LIMIT 1
  ) AS latest_user_message_at,
  (
    SELECT message.updated_at
    FROM orchestration_v2_projection_messages message
    WHERE message.thread_id = t.thread_id
      AND message.role = 'user'
      AND json_extract(message.payload_json, '$.createdBy') = 'user'
      AND NOT EXISTS (SELECT 1 FROM compaction_messages AS idle WHERE idle.message_id = message.message_id)
    ORDER BY message.updated_at DESC, message.message_id DESC
    LIMIT 1
  ) AS latest_user_authored_message_at,
  EXISTS (
    SELECT 1
    FROM orchestration_v2_projection_plans plan
    WHERE plan.thread_id = t.thread_id
      AND plan.kind = 'proposed_plan'
      AND plan.status = 'active'
  ) AS has_actionable_proposed_plan
FROM orchestration_v2_projection_threads t
LEFT JOIN orchestration_v2_projection_runs active ON active.run_id = (
  SELECT r.run_id
  FROM displayed_runs r
  WHERE r.thread_id = t.thread_id
    AND r.status IN ('preparing', 'starting', 'running')
  ORDER BY r.ordinal DESC, r.run_id DESC
  LIMIT 1
)
LEFT JOIN orchestration_v2_projection_runs presented
  ON presented.run_id = (
      SELECT candidate.run_id
      FROM displayed_runs candidate
      WHERE candidate.thread_id = t.thread_id
        AND NOT (
          candidate.status = 'queued'
          AND json_extract(candidate.payload_json, '$.queueHeld') IS 1
        )
        AND NOT (
          candidate.status = 'cancelled'
          AND json_extract(candidate.payload_json, '$.startedAt') IS NULL
          AND candidate.completed_at IS NOT NULL
          AND EXISTS (
            SELECT 1 FROM displayed_runs earlier
            WHERE earlier.thread_id = candidate.thread_id
              AND earlier.ordinal < candidate.ordinal
              AND earlier.status <> 'queued'
              AND (
                earlier.completed_at IS NULL
                OR julianday(earlier.completed_at) >= julianday(candidate.completed_at)
              )
          )
        )
      ORDER BY candidate.ordinal DESC, candidate.run_id DESC
      LIMIT 1
    )
LEFT JOIN orchestration_v2_projection_runs blocked ON blocked.run_id = (
  SELECT candidate.run_id
  FROM orchestration_v2_projection_runs candidate
  WHERE candidate.thread_id = t.thread_id
    AND candidate.status <> 'queued'
    AND NOT (
      candidate.status = 'cancelled'
      AND json_extract(candidate.payload_json, '$.startedAt') IS NULL
    )
  -- latestExecutedRun: the run that ended last (runRanAfter).
  ORDER BY candidate.completed_at IS NULL DESC, candidate.completed_at DESC,
    candidate.ordinal DESC, candidate.run_id DESC
  LIMIT 1
) AND blocked.status = 'failed'
