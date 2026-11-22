SELECT effect.effect_id, effect.attempt_count
FROM orchestration_v2_effect_outbox AS effect
JOIN orchestration_v2_projection_provider_turns AS turn ON turn.thread_id = effect.thread_id
  AND turn.provider_turn_id = json_extract(effect.payload_json, '$.providerTurnId')
JOIN orchestration_v2_projection_provider_threads AS provider_thread
  ON provider_thread.thread_id = turn.thread_id
  AND provider_thread.provider_thread_id = turn.provider_thread_id
  AND provider_thread.provider_thread_id = json_extract(effect.payload_json, '$.providerThreadId')
JOIN orchestration_v2_projection_provider_sessions AS session
  ON session.provider_session_id = provider_thread.provider_session_id AND session.status = 'ready'
  AND session.provider_session_id = json_extract(effect.payload_json, '$.providerSessionId')
WHERE effect.thread_id = ?1 AND turn.provider_turn_id = ?2 AND turn.status = 'running'
  AND provider_thread.status = 'active'
  AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?3
  AND json_extract(turn.payload_json, '$.nativeTurnRef.nativeId') = ?4
  AND effect.effect_type = 'provider-turn.interrupt' AND effect.status = 'pending'
  AND effect.available_at <= ?5
ORDER BY effect.rowid LIMIT 1
