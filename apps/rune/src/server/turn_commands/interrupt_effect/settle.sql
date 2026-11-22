SELECT run.run_id, node.node_id, run.ordinal, json_extract(node.payload_json, '$.checkpointScopeId'),
       run.payload_json, attempt.payload_json, node.payload_json, provider_thread.payload_json,
       request.payload_json
FROM orchestration_v2_projection_provider_turns AS turn
JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.attempt_id = turn.run_attempt_id
JOIN orchestration_v2_projection_runs AS run ON run.run_id = attempt.run_id AND run.thread_id = turn.thread_id
  AND attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id AND node.node_id = turn.node_id
JOIN orchestration_v2_projection_provider_threads AS provider_thread
  ON provider_thread.provider_thread_id = turn.provider_thread_id AND provider_thread.thread_id = run.thread_id
JOIN orchestration_v2_projection_turn_items AS request ON request.thread_id = run.thread_id AND request.run_id = run.run_id
  AND request.type = 'run_interrupt_request'
  AND json_extract(request.payload_json, '$.providerTurnId') = turn.provider_turn_id
WHERE run.thread_id = ?1 AND turn.provider_turn_id = ?2
  AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?3
  AND json_extract(turn.payload_json, '$.nativeTurnRef.nativeId') = ?4
  AND turn.status = 'interrupted' AND run.provider_instance_id = 'codex'
  AND run.status = 'running' AND attempt.status = 'running' AND node.status = 'running'
  AND provider_thread.status = 'active' AND provider_thread.last_run_ordinal = run.ordinal
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_subagents AS task WHERE task.run_id = run.run_id)
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_runtime_requests AS runtime_request
    WHERE runtime_request.provider_turn_id = turn.provider_turn_id AND runtime_request.status = 'pending'
      AND NOT (json_extract(runtime_request.payload_json, '$.responseCapability.type') IS 'live'
        AND json_extract(runtime_request.payload_json, '$.responseCapability.providerSessionId') IS provider_thread.provider_session_id
        AND ((runtime_request.kind IS 'command' AND EXISTS (SELECT 1 FROM orchestration_v2_projection_nodes AS approval
          JOIN orchestration_v2_projection_turn_items AS command ON command.node_id = approval.parent_node_id
            AND command.thread_id = approval.thread_id AND command.provider_turn_id = turn.provider_turn_id
            AND command.type = 'command_execution' AND command.status = 'interrupted'
          WHERE approval.node_id = runtime_request.node_id AND approval.thread_id = run.thread_id
            AND approval.kind = 'approval_request' AND approval.status = 'waiting'))
          OR (runtime_request.kind IS 'file-change' AND EXISTS (
            SELECT 1 FROM orchestration_v2_projection_nodes AS approval
            WHERE approval.node_id = runtime_request.node_id AND approval.thread_id = run.thread_id
              AND approval.kind = 'approval_request' AND approval.status = 'waiting'
              AND approval.run_id = run.run_id AND approval.root_node_id = node.node_id
              AND approval.provider_turn_id = turn.provider_turn_id)))))
