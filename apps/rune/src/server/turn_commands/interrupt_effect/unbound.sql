SELECT run.run_id, run.ordinal, node.node_id, provider.provider_thread_id,
       run.payload_json, attempt.payload_json, node.payload_json, preparation.payload_json
FROM orchestration_v2_projection_threads AS thread
JOIN projection_projects AS project ON project.project_id = thread.project_id
JOIN orchestration_v2_projection_runs AS run ON run.thread_id = thread.thread_id
LEFT JOIN orchestration_v2_projection_runs AS resumed_source
  ON resumed_source.thread_id = run.thread_id
 AND resumed_source.run_id = json_extract(run.payload_json, '$.manualContinuationOfRunId')
 AND resumed_source.ordinal = 1 AND resumed_source.status = 'interrupted'
 AND resumed_source.provider_thread_id = run.provider_thread_id
 AND json_extract(resumed_source.payload_json, '$.workspacePreparation.type') = 'worktree'
JOIN orchestration_v2_projection_run_attempts AS attempt
  ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
 AND attempt.run_id = run.run_id
JOIN orchestration_v2_projection_nodes AS node
  ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
JOIN orchestration_v2_projection_provider_threads AS provider
  ON provider.provider_thread_id = run.provider_thread_id AND provider.thread_id = thread.thread_id
LEFT JOIN orchestration_v2_projection_turn_items AS preparation
  ON preparation.run_id = run.run_id AND preparation.thread_id = thread.thread_id
 AND preparation.type = 'command_execution' AND preparation.status = 'running'
 AND json_extract(preparation.payload_json, '$.input') = 'Preparing workspace'
WHERE thread.thread_id = ?1 AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
  AND project.archived_at IS NULL AND project.deleted_at IS NULL
  AND COALESCE(json_array_length(project.tracked_folders_json), 0) = 0
  AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
  AND json_extract(thread.payload_json, '$.lineage.rootThreadId') = thread.thread_id
  AND json_extract(thread.payload_json, '$.forkedFrom') IS NULL
  AND (json_extract(thread.payload_json, '$.worktreePath') IS NULL
       OR json_extract(run.payload_json, '$.workspacePreparation.type') = 'worktree'
       OR resumed_source.run_id IS NOT NULL)
  AND thread.active_provider_thread_id = provider.provider_thread_id
  AND provider.driver = 'codex' AND provider.provider_instance_id = 'codex'
  AND provider.provider_session_id = 'provider-session:provider-instance:codex:shared'
  AND provider.owner_node_id IS NULL AND provider.status = 'not_loaded'
  AND json_extract(provider.payload_json, '$.nativeThreadRef') IS NULL
  AND run.ordinal > 0 AND run.provider_instance_id = 'codex'
  AND provider.last_run_ordinal = run.ordinal
  AND COALESCE(json_array_length(provider.payload_json, '$.pendingBackgroundTasks'), 0) = 0
  AND attempt.attempt_ordinal = 1 AND attempt.status = 'pending' AND attempt.provider_turn_id IS NULL
  AND attempt.provider_thread_id = provider.provider_thread_id
  AND node.kind = 'root_turn' AND node.parent_node_id IS NULL AND node.status = 'pending'
  AND node.provider_turn_id IS NULL AND node.provider_thread_id = provider.provider_thread_id
  AND (
    (run.status = 'preparing' AND json_extract(run.payload_json, '$.workspacePreparation.type') IN ('root', 'worktree')
     AND preparation.turn_item_id IS NOT NULL)
    OR (run.status = 'starting' AND EXISTS (
      SELECT 1 FROM orchestration_v2_effect_outbox AS effect
      JOIN orchestration_command_receipts AS receipt ON receipt.command_id = effect.command_id
      WHERE effect.thread_id = run.thread_id AND effect.effect_type = 'provider-turn.start'
        AND json_extract(effect.payload_json, '$.runId') = run.run_id
        AND effect.status IN ('pending', 'running')
        AND receipt.aggregate_kind = 'thread' AND receipt.aggregate_id = run.thread_id
        AND receipt.status = 'accepted'
        AND (receipt.command_type = 'message.dispatch' OR (receipt.command_type = 'prepared-run.release'
             AND json_extract(run.payload_json, '$.workspacePreparation.type') IN ('root', 'worktree')))
    ))
  )
  AND NOT EXISTS (
    SELECT 1 FROM orchestration_v2_projection_runs AS other
    WHERE other.thread_id = thread.thread_id AND other.run_id <> run.run_id
      AND (other.status IN ('preparing', 'starting', 'running', 'waiting')
           OR (other.status = 'queued'
               AND json_extract(run.payload_json, '$.workspacePreparation.type') IS NOT 'worktree'
               AND resumed_source.run_id IS NULL))
  )
  AND NOT EXISTS (
    SELECT 1 FROM orchestration_v2_projection_provider_turns AS native_turn
    WHERE native_turn.provider_thread_id = provider.provider_thread_id
  )
  AND NOT EXISTS (
    SELECT 1 FROM orchestration_v2_projection_nodes AS child
    WHERE child.run_id = run.run_id AND child.node_id <> node.node_id
      AND child.status IN ('pending', 'running', 'waiting')
  )
  AND NOT EXISTS (
    SELECT 1 FROM orchestration_v2_projection_subagents AS task WHERE task.thread_id = thread.thread_id
  )
  AND NOT EXISTS (
    SELECT 1 FROM orchestration_v2_projection_runtime_requests AS request
    WHERE request.thread_id = thread.thread_id AND request.status = 'pending'
  )
ORDER BY run.ordinal DESC LIMIT 1
