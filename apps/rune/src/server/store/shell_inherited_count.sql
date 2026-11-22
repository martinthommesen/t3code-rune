SELECT COALESCE(SUM(
    CASE WHEN run.run_id IS NULL THEN counts.item_count ELSE counts.item_count - CASE
        WHEN run.status IN ('queued', 'cancelled') OR (
            run.status = 'rolled_back' AND EXISTS (
                SELECT 1 FROM orchestration_v2_projection_run_attempts AS attempt
                WHERE attempt.run_id = run.run_id AND attempt.status = 'cancelled'
            )
        ) THEN (
            SELECT COUNT(*) FROM orchestration_v2_projection_turn_items AS held
                INDEXED BY orchestration_v2_projection_turn_items_queued_input_idx
            WHERE held.run_id = run.run_id AND held.type = 'user_message'
              AND json_valid(held.payload_json)
              AND json_extract(held.payload_json, '$.inputIntent') = 'queued_turn'
        ) ELSE 0 END
    END
), 0)
FROM orchestration_v2_projection_runs AS boundary
JOIN orchestration_v2_projection_turn_item_counts AS counts ON counts.thread_id = boundary.thread_id
LEFT JOIN orchestration_v2_projection_runs AS run ON run.run_id = counts.run_key
WHERE boundary.thread_id = ?1 AND boundary.run_id = ?2
  AND ((counts.run_key = '' AND ?4) OR (
    run.ordinal <= boundary.ordinal AND (
      run.status <> 'rolled_back' OR run.completed_at IS NULL OR julianday(run.completed_at) >= julianday(?3)
    )
  ))
