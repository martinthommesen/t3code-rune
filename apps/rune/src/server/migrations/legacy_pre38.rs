use super::{Migration, add_column};

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration::sql(
        31,
        "AuthAuthorizationScopes",
        include_str!("legacy_pre38/auth_authorization_scopes.sql"),
    ),
    Migration::scripted(32, "AuthPairingProofKeyThumbprint", |db| {
        add_column(db, "auth_pairing_links", "proof_key_thumbprint", "TEXT")
    }),
    Migration::scripted(33, "ProjectionThreadsSettled", |db| {
        add_column(db, "projection_threads", "settled_override", "TEXT")?;
        add_column(db, "projection_threads", "settled_at", "TEXT")
    }),
    Migration::scripted(34, "ProjectionThreadsSnoozed", |db| {
        add_column(db, "projection_threads", "snoozed_until", "TEXT")?;
        add_column(db, "projection_threads", "snoozed_at", "TEXT")
    }),
    Migration::scripted(35, "ProjectionThreadTitleRegeneration", |db| {
        add_column(
            db,
            "projection_threads",
            "title_regeneration_request_id",
            "TEXT",
        )?;
        add_column(
            db,
            "projection_threads",
            "title_regeneration_started_at",
            "TEXT",
        )
    }),
    Migration::scripted(36, "ProjectionThreadsPinned", |db| {
        add_column(db, "projection_threads", "pinned_at", "TEXT")
    }),
    Migration::sql(
        37,
        "ProjectionTurnsKeysetIndex",
        "CREATE INDEX IF NOT EXISTS idx_projection_turns_thread_keyset
         ON projection_turns(thread_id, requested_at, turn_id)",
    ),
    Migration::scripted(38, "ProjectionThreadsPinOrderKey", |db| {
        add_column(db, "projection_threads", "pin_order_key", "TEXT")
    }),
];

#[test]
fn scope_cutover_discards_role_records_and_proof_column_preserves_scopes() {
    use rusqlite::Connection;

    let mut database = Connection::open_in_memory().unwrap();
    database
        .execute_batch(
            "CREATE TABLE auth_pairing_links (id TEXT PRIMARY KEY, role TEXT NOT NULL);
         INSERT INTO auth_pairing_links VALUES ('legacy-link', 'owner');
         CREATE TABLE auth_sessions (session_id TEXT PRIMARY KEY, role TEXT NOT NULL);
         INSERT INTO auth_sessions VALUES ('legacy-session', 'owner');",
        )
        .unwrap();
    let transaction = database.transaction().unwrap();
    MIGRATIONS[0].apply(&transaction).unwrap();
    let remaining: i64 = transaction.query_row(
        "SELECT (SELECT count(*) FROM auth_pairing_links) + (SELECT count(*) FROM auth_sessions)",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(remaining, 0);
    let scopes = "[\"orchestration:read\"]";
    transaction.execute(
        "INSERT INTO auth_pairing_links (id, credential, method, scopes, subject, created_at, expires_at)
         VALUES ('scoped-link','fixture-credential','desktop-bootstrap',?1,'fixture-subject',
                 '2026-06-20T00:00:00.000Z','2026-06-21T00:00:00.000Z')",
        [scopes],
    ).unwrap();
    MIGRATIONS[1].apply(&transaction).unwrap();
    let (stored, proof): (String, Option<String>) = transaction
        .query_row(
            "SELECT scopes, proof_key_thumbprint FROM auth_pairing_links WHERE id='scoped-link'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((stored.as_str(), proof), (scopes, None));
}
