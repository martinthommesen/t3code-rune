use serde_json::Value;

use super::*;
use crate::server::turn_commands::tests::{Fixture, THREAD};

#[test]
fn detaches_once_and_recovers_the_effect_without_touching_its_neighbor() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let database = fixture.connection();
    let id = "detach:owner";
    let another_id = "detach:again";
    let neighbor = "thread:attached-neighbor";
    let native = "provider-thread:idle-owner";
    let when = "2026-11-22T20:47:20.429Z";
    database.execute(
        "INSERT INTO orchestration_v2_projection_provider_sessions
            (provider_session_id, thread_id, provider, driver, provider_instance_id, status, updated_at, payload_json)
         VALUES (?1, ?2, 'codex', 'codex', 'codex', 'ready', ?3, '{}')",
        params![SHARED_CODEX_SESSION, THREAD, when],
    ).unwrap();
    database
        .execute(
            "INSERT INTO orchestration_v2_projection_provider_session_bindings
             (provider_session_id, thread_id) VALUES (?1, ?2), (?1, ?3)",
            params![SHARED_CODEX_SESSION, THREAD, neighbor],
        )
        .unwrap();
    database.execute(
        "INSERT INTO orchestration_v2_projection_provider_threads
            (provider_thread_id, thread_id, provider, driver, provider_instance_id, provider_session_id,
             status, updated_at, payload_json)
         VALUES (?1, ?2, 'codex', 'codex', 'codex', ?3, 'idle', ?4, ?5)",
        params![native, THREAD, SHARED_CODEX_SESSION, when, json!({
            "nativeThreadRef": {"nativeId": "native-owner", "strength": "strong", "driver": "codex"}
        }).to_string()],
    ).unwrap();
    database.execute(
        "UPDATE orchestration_v2_projection_threads SET active_provider_thread_id = ?2 WHERE thread_id = ?1",
        params![THREAD, native],
    ).unwrap();
    let command = Detach {
        command_id: id.into(),
        thread_id: THREAD.into(),
        provider_session_id: SHARED_CODEX_SESSION.into(),
        reason: Some("client-requested".into()),
    };
    database.execute(
        "UPDATE orchestration_v2_projection_provider_threads SET status = 'running' WHERE provider_thread_id = ?1",
        [native],
    ).unwrap();
    assert!(matches!(
        commands.detach_provider_session(&command),
        Err(TurnCommandError::Unsupported(_))
    ));
    database.execute(
        "UPDATE orchestration_v2_projection_provider_threads SET status = 'idle' WHERE provider_thread_id = ?1",
        [native],
    ).unwrap();
    let (sequence, committed) = commands.detach_provider_session(&command).unwrap();
    assert!(committed);
    assert_eq!(
        commands.detach_provider_session(&command).unwrap(),
        (sequence, false)
    );
    assert!(matches!(
        commands.detach_provider_session(&Detach {
            command_id: another_id.into(),
            ..command
        }),
        Err(TurnCommandError::ProviderSessionNotBound { .. })
    ));
    let row = database
        .query_row(
            "SELECT event_type, actor_kind, command_id, correlation_id, occurred_at, payload_json,
                metadata_json, application_event_version
         FROM orchestration_events WHERE sequence = ?1",
            [sequence],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        (&*row.0, &*row.1, &*row.2, &*row.3, row.7),
        (EVENT, "server", id, id, 2)
    );
    assert_eq!(
        serde_json::from_str::<Value>(&row.5).unwrap(),
        json!({
            "providerSessionId": SHARED_CODEX_SESSION, "reason": "client-requested", "detachedAt": row.4,
        })
    );
    assert_eq!(
        serde_json::from_str::<Value>(&row.6).unwrap(),
        json!({"driver": "codex", "providerInstanceId": "codex"})
    );
    let scope = database.query_row(
        "SELECT binding.thread_id, session.status FROM orchestration_v2_projection_provider_session_bindings AS binding
          JOIN orchestration_v2_projection_provider_sessions AS session USING (provider_session_id)
          WHERE binding.provider_session_id = ?1",
        [SHARED_CODEX_SESSION],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).unwrap();
    assert_eq!(scope, (neighbor.into(), "ready".into()));
    let effect = format!("effect:{id}:{COMMAND}:{SHARED_CODEX_SESSION}");
    assert_eq!(commands.pending_effects(0, 16).unwrap().len(), 1);
    assert_eq!(
        commands
            .claim_provider_detach(&effect, "owner")
            .unwrap()
            .as_deref(),
        Some(THREAD)
    );
    assert!(
        commands
            .claim_provider_detach(&effect, "second")
            .unwrap()
            .is_none()
    );
    assert!(commands.requeue_provider_detaches(None).unwrap());
    assert_eq!(
        commands
            .claim_provider_detach(&effect, "restarted")
            .unwrap()
            .as_deref(),
        Some(THREAD)
    );
    commands
        .finish_provider_detach(&effect, "restarted")
        .unwrap();
    let (status, attempts, payload) = database.query_row(
        "SELECT status, attempt_count, payload_json FROM orchestration_v2_effect_outbox WHERE effect_id = ?1",
        [&effect],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, String>(2)?)),
    ).unwrap();
    assert_eq!((&*status, attempts), ("succeeded", 2));
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap(),
        json!({
            "type": COMMAND, "providerSessionId": SHARED_CODEX_SESSION, "detail": "client-requested"
        })
    );
}
