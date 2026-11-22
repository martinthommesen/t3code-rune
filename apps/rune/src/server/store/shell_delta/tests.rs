use super::*;

fn database() -> Connection {
    let database = Connection::open_in_memory().unwrap();
    database.execute_batch(
        "CREATE TABLE orchestration_events (
            sequence INTEGER PRIMARY KEY, aggregate_kind TEXT, stream_id TEXT,
            event_type TEXT, application_event_version INTEGER, payload_json TEXT
        );
        CREATE INDEX idx_orchestration_events_application_high_water
        ON orchestration_events(sequence)
        WHERE aggregate_kind = 'project' OR (application_event_version = 2 AND aggregate_kind = 'thread');
        CREATE TABLE orchestration_v2_projection_metadata (
            projection_name TEXT PRIMARY KEY, last_sequence INTEGER
        );",
    ).unwrap();
    database
}

#[test]
fn catches_up_past_unknown_and_subagent_transcript_rows() {
    let database = database();
    database.execute_batch(
        "INSERT INTO orchestration_events VALUES
          (1, 'thread', 'child', 'thread.created', 2, '{\"lineage\":{\"relationshipToParent\":\"subagent\"}}'),
          (2, 'thread', 'child', 'node.updated', 2, '{}'),
          (3, 'thread', 'child', 'message.updated', 2, '{}'),
          (4, 'thread', 'child', 'turn-item.updated', 2, '{\"type\":\"link_request\"}'),
          (5, 'thread', 'child', 'thread.deleted', 2, '{}'),
          (6, 'thread', 'child', 'provider-turn.updated', 2, '{}'),
          (7, 'thread', 'parent', 'message.updated', 2, '{}'),
          (8, 'thread', 'parent', 'message.updated', 1, '{}'),
          (9, 'thread', 'parent', 'future.event', 2, '{}'),
          (10, 'project', 'parent', 'project.deleted', 1, '{}'),
          (11, 'thread', 'child', 'message.updated', 2, '{}');",
    ).unwrap();

    let mut subagents = HashSet::new();
    let events = read_events(&database, 0, 10, &mut subagents).unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [1, 4, 5, 7, 10]
    );
    assert_eq!(subagents, HashSet::from(["child".to_owned()]));
    let batch = read(&database, 10, 11, Some(&subagents)).unwrap().unwrap();
    assert_eq!(batch.sequence, 11);
    assert!(batch.items.is_empty());
    assert_eq!(batch.subagent_thread_ids, subagents);
}

#[test]
fn replay_budgets_include_expanded_prompts_and_respect_pruning() {
    let database = database();
    let prompt = "x".repeat(REPLAY_MAX_BYTES as usize);
    database
        .execute(
            "INSERT INTO orchestration_events VALUES (1, 'thread', 'parent', 'subagent.updated', 2, ?1)",
            [json!({"prompt": prompt}).to_string()],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO orchestration_events VALUES
              (2, 'thread', 'parent', 'subagent.updated', 2, '{\"promptSequence\":1}')",
            [],
        )
        .unwrap();
    assert!(!can_resume(&database, 1, 2).unwrap());
    assert!(!can_resume(&database, 0, REPLAY_MAX_GAP + 1).unwrap());
    assert!(!can_resume(&database, 3, 2).unwrap());

    database.execute(
        "INSERT INTO orchestration_v2_projection_metadata VALUES ('event-log-pruned-through', 4)",
        [],
    ).unwrap();
    assert!(!can_resume(&database, 3, 5).unwrap());
    assert!(can_resume(&database, 4, 5).unwrap());
    assert!(can_resume(&database, 2, 2).unwrap());
}
