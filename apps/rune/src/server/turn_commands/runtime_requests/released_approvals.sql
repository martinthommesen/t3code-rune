SELECT request.thread_id, run.run_id, node.node_id, request.payload_json, node.payload_json, card.payload_json
FROM orchestration_v2_projection_runtime_requests AS request
JOIN orchestration_v2_projection_provider_turns AS turn
  ON turn.provider_turn_id = request.provider_turn_id AND turn.thread_id = request.thread_id
JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.attempt_id = turn.run_attempt_id
JOIN orchestration_v2_projection_runs AS run ON run.run_id = attempt.run_id AND run.thread_id = request.thread_id
JOIN orchestration_v2_projection_nodes AS node ON node.node_id = request.node_id AND node.run_id = run.run_id
JOIN orchestration_v2_projection_turn_items AS card ON card.node_id = node.node_id AND card.thread_id = request.thread_id
  AND json_extract(card.payload_json, '$.requestId') = request.runtime_request_id
WHERE (?1 IS NULL OR request.thread_id = ?1)
  AND (request.kind = 'command' OR (request.kind = 'file-change' AND node.root_node_id = attempt.root_node_id))
  AND request.status = 'pending'
  AND json_extract(request.payload_json, '$.responseCapability.type') = 'live'
  AND json_extract(request.payload_json, '$.responseCapability.providerSessionId') = ?2
  AND run.status = 'interrupted' AND turn.status = 'interrupted' AND run.provider_instance_id = 'codex'
  AND attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
  AND node.kind = 'approval_request' AND node.status = 'waiting'
  AND card.type = 'approval_request' AND card.status = 'waiting'
  AND EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items AS marker
    WHERE marker.thread_id = run.thread_id AND marker.run_id = run.run_id
      AND marker.type = 'run_interrupt_request' AND marker.provider_turn_id = request.provider_turn_id)
ORDER BY request.thread_id, request.created_at, request.runtime_request_id
