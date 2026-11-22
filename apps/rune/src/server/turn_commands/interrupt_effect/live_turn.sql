WITH waiting_commands(request_id, tool_node_id, thread_id, provider_turn_id, session_id) AS (
    SELECT request.runtime_request_id, command.node_id, request.thread_id, request.provider_turn_id,
           json_extract(request.payload_json, '$.responseCapability.providerSessionId')
    FROM orchestration_v2_projection_runtime_requests AS request
    JOIN orchestration_v2_projection_nodes AS approval ON approval.node_id = request.node_id
      AND approval.thread_id = request.thread_id AND approval.kind = 'approval_request' AND approval.status = 'waiting'
    JOIN orchestration_v2_projection_turn_items AS command ON command.node_id = approval.parent_node_id
      AND command.thread_id = request.thread_id AND command.provider_turn_id = request.provider_turn_id
      AND command.type = 'command_execution' AND command.status = 'running'
    WHERE request.thread_id = ?1 AND request.kind = 'command' AND request.status = 'pending'
      AND json_extract(request.payload_json, '$.responseCapability.type') IS 'live'
)
SELECT run.run_id, node.node_id, session.provider_session_id,
       provider_thread.provider_thread_id, turn.provider_turn_id, run.ordinal
FROM orchestration_v2_projection_threads AS thread
JOIN orchestration_v2_projection_runs AS run ON run.thread_id = thread.thread_id
JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.run_id = run.run_id
  AND attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id
JOIN orchestration_v2_projection_provider_threads AS provider_thread
  ON provider_thread.thread_id = run.thread_id
  AND provider_thread.provider_thread_id = attempt.provider_thread_id
  AND provider_thread.provider_thread_id = thread.active_provider_thread_id
JOIN orchestration_v2_projection_provider_turns AS turn ON turn.run_attempt_id = attempt.attempt_id
  AND turn.thread_id = run.thread_id AND turn.node_id = node.node_id
  AND turn.provider_thread_id = provider_thread.provider_thread_id
JOIN orchestration_v2_projection_provider_sessions AS session
  ON session.provider_session_id = provider_thread.provider_session_id
JOIN orchestration_v2_projection_provider_session_bindings AS binding
  ON binding.provider_session_id = session.provider_session_id AND binding.thread_id = run.thread_id
WHERE thread.thread_id = ?1 AND thread.deleted_at IS NULL AND thread.archived_at IS NULL
  AND (json_extract(thread.payload_json, '$.worktreePath') IS NULL OR EXISTS (
    SELECT 1 FROM orchestration_v2_projection_checkpoint_scopes AS scope
    WHERE scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId')
      AND scope.run_id = run.run_id
      AND scope.thread_id = thread.thread_id AND scope.node_id = node.node_id
      AND scope.kind = 'root_run'
      AND json_extract(scope.payload_json, '$.cwd') = json_extract(thread.payload_json, '$.worktreePath')
  ))
  AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
  AND json_extract(thread.payload_json, '$.lineage.rootThreadId') = thread.thread_id
  AND json_extract(thread.payload_json, '$.forkedFrom') IS NULL
  AND run.provider_instance_id = 'codex' AND run.status = 'running'
  AND attempt.status = 'running' AND node.status = 'running' AND session.status = 'ready'
  AND session.driver = 'codex' AND provider_thread.status = 'active' AND turn.status = 'running'
  AND provider_thread.driver = 'codex' AND provider_thread.provider_instance_id = 'codex'
  AND provider_thread.provider_session_id = 'provider-session:provider-instance:codex:shared'
  AND provider_thread.last_run_ordinal = run.ordinal
  AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.strength') = 'strong'
  AND json_extract(turn.payload_json, '$.nativeTurnRef.strength') = 'strong'
  AND COALESCE(json_array_length(provider_thread.payload_json, '$.pendingBackgroundTasks'), 0) = 0
  AND EXISTS (SELECT 1 FROM orchestration_v2_effect_outbox AS effect
    WHERE effect.thread_id = run.thread_id AND effect.effect_type = 'provider-turn.start'
      AND effect.status = 'succeeded' AND json_extract(effect.payload_json, '$.runId') = run.run_id)
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_runs AS other
    WHERE other.thread_id = run.thread_id AND other.run_id <> run.run_id
      AND other.status IN ('queued', 'preparing', 'starting', 'running'))
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_subagents AS task
    WHERE task.thread_id = run.thread_id)
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_runtime_requests AS request
    WHERE request.thread_id = run.thread_id AND request.status = 'pending'
      AND NOT (request.provider_turn_id IS turn.provider_turn_id
        AND json_extract(request.payload_json, '$.responseCapability.type') IS 'live'
        AND json_extract(request.payload_json, '$.responseCapability.providerSessionId') IS session.provider_session_id
        AND (request.kind IS 'user_input' OR EXISTS (SELECT 1 FROM waiting_commands
          WHERE request_id = request.runtime_request_id))))
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items AS item
    WHERE item.run_id = run.run_id AND item.type IN ('command_execution', 'dynamic_tool', 'file_change', 'subagent')
      AND item.status IN ('pending', 'running', 'waiting')
      AND NOT EXISTS (SELECT 1 FROM waiting_commands WHERE tool_node_id = item.node_id
        AND waiting_commands.thread_id = run.thread_id AND waiting_commands.provider_turn_id = turn.provider_turn_id
        AND waiting_commands.session_id = session.provider_session_id)
      AND NOT (item.type = 'command_execution' AND item.status = 'running'
        AND item.provider_turn_id = turn.provider_turn_id
        AND json_extract(item.payload_json, '$.nativeItemRef.strength') = 'strong'
        AND EXISTS (SELECT 1 FROM orchestration_v2_projection_nodes AS tool
          WHERE tool.node_id = item.node_id AND tool.run_id = run.run_id
            AND tool.kind = 'tool_call' AND tool.status = 'running'
            AND tool.parent_node_id = node.node_id AND tool.root_node_id = node.node_id
            AND tool.provider_thread_id = provider_thread.provider_thread_id
            AND tool.provider_turn_id = turn.provider_turn_id)))
ORDER BY run.ordinal DESC LIMIT 1
