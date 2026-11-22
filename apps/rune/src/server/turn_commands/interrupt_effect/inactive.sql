SELECT effect.effect_id, effect.thread_id
FROM orchestration_v2_effect_outbox AS effect
JOIN orchestration_command_receipts AS receipt ON receipt.command_id = effect.command_id
  AND receipt.aggregate_kind = 'thread' AND receipt.aggregate_id = effect.thread_id
  AND receipt.command_type = 'thread.stop' AND receipt.status = 'accepted'
JOIN orchestration_v2_projection_provider_turns AS turn ON turn.thread_id = effect.thread_id
  AND turn.provider_turn_id = json_extract(effect.payload_json, '$.providerTurnId')
  AND turn.provider_thread_id = json_extract(effect.payload_json, '$.providerThreadId')
JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.attempt_id = turn.run_attempt_id
JOIN orchestration_v2_projection_runs AS run ON run.run_id = attempt.run_id AND run.thread_id = effect.thread_id
JOIN orchestration_v2_projection_provider_threads AS provider_thread
  ON provider_thread.provider_thread_id = turn.provider_thread_id AND provider_thread.thread_id = run.thread_id
WHERE effect.thread_id = ?1 AND run.provider_instance_id = 'codex'
  AND provider_thread.driver = 'codex' AND provider_thread.provider_instance_id = 'codex'
  AND COALESCE(json_array_length(provider_thread.payload_json, '$.pendingBackgroundTasks'), 0) = 0
  AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_subagents AS task WHERE task.run_id = run.run_id)
  AND turn.status NOT IN ('pending', 'running', 'waiting') AND run.status <> 'running'
  AND effect.effect_type = 'provider-turn.interrupt' AND effect.status IN ('pending', 'running')
ORDER BY effect.rowid
