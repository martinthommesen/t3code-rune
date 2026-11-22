-- Only the thread's hidden runs are visited, so settled history costs nothing here.
SELECT
  totals.item_count - COALESCE((
    SELECT SUM(CASE WHEN r.status = 'rolled_back' THEN counts.item_count ELSE (
      SELECT COUNT(*)
      FROM orchestration_v2_projection_turn_items held
      INDEXED BY orchestration_v2_projection_turn_items_queued_input_idx
      WHERE held.run_id = counts.run_key
        AND held.type = 'user_message'
        AND json_valid(held.payload_json)
        AND json_extract(held.payload_json, '$.inputIntent') = 'queued_turn'
    ) END)
    FROM orchestration_v2_projection_runs r
      INDEXED BY orchestration_v2_projection_runs_hidden_items_idx
    JOIN orchestration_v2_projection_turn_item_counts counts
      ON counts.thread_id = r.thread_id AND counts.run_key = r.run_id
    WHERE r.thread_id = totals.thread_id
      AND r.status IN ('rolled_back', 'queued', 'cancelled')
  ), 0)
FROM orchestration_v2_projection_thread_item_counts totals
WHERE totals.thread_id = ?1
