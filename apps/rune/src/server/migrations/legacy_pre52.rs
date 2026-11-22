use rusqlite::{Transaction, params, types::ValueRef};
use serde_json::Value;

use super::{Migration, add_column, legacy_pull_requests};

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration::scripted(39, "ProjectionProjectsDefaultThreadEnvMode", |db| {
        add_column(db, "projection_projects", "default_thread_env_mode", "TEXT")
    }),
    Migration::scripted(40, "ProjectionProjectFaviconPath", |db| {
        add_column(db, "projection_projects", "favicon_path", "TEXT")
    }),
    Migration::scripted(41, "AuthSessionClientConnection", |db| {
        add_column(db, "auth_sessions", "client_surface", "TEXT")?;
        add_column(db, "auth_sessions", "client_app_version", "TEXT")
    }),
    Migration::scripted(42, "ProjectionThreadLinkedPullRequest", |db| {
        add_column(db, "projection_threads", "linked_pull_request_json", "TEXT")
    }),
    Migration::scripted(43, "ProjectionThreadsUnsettledAt", |db| {
        add_column(db, "projection_threads", "unsettled_at", "TEXT")
    }),
    Migration::sql(
        44,
        "ClearAutomaticProjectModelDefaults",
        include_str!("legacy_pre52/model_defaults.sql"),
    ),
    Migration::scripted(45, "ProjectionProjectsAutoPull", |db| {
        add_column(
            db,
            "projection_projects",
            "auto_pull",
            "INTEGER NOT NULL DEFAULT 0",
        )
    }),
    Migration::sql(
        46,
        "RepairAutomaticSettlementTimestamps",
        include_str!("legacy_pre52/automatic_settlements.sql"),
    ),
    Migration::scripted(47, "ProjectionProjectIcon", |db| {
        add_column(db, "projection_projects", "project_icon_json", "TEXT")
    }),
    Migration::scripted(48, "ProjectionThreadBranchPullRequest", |db| {
        add_column(db, "projection_threads", "branch_pull_request_json", "TEXT")
    }),
    Migration::scripted(49, "ProjectionThreadsActiveOrderKey", |db| {
        add_column(db, "projection_threads", "active_order_key", "TEXT")
    }),
    Migration::scripted(50, "ProjectionThreadPullRequests", migrate_50),
    Migration::scripted(51, "ProjectionThreadMessageContext", |db| {
        add_column(db, "projection_thread_messages", "context_json", "TEXT")
    }),
    Migration::sql(
        52,
        "ProjectionThreadTitleState",
        "ALTER TABLE projection_threads ADD COLUMN title_state_json TEXT",
    ),
];

fn migrate_50(db: &Transaction<'_>) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS projection_thread_pull_requests (
            thread_id TEXT NOT NULL,
            host TEXT NOT NULL,
            repository TEXT NOT NULL,
            number INTEGER NOT NULL,
            url TEXT NOT NULL,
            source TEXT NOT NULL,
            linked_at TEXT NOT NULL,
            snapshot_json TEXT,
            stack_json TEXT,
            PRIMARY KEY (thread_id, host, repository, number)
        );
        CREATE INDEX IF NOT EXISTS idx_projection_thread_pull_requests_pr
        ON projection_thread_pull_requests(host, repository, number);",
    )?;
    let mut select = db.prepare(
        "SELECT thread_id, updated_at, linked_pull_request_json
         FROM projection_threads WHERE linked_pull_request_json IS NOT NULL",
    )?;
    let mut rows = select.query([])?;
    let mut insert = db.prepare(
        "INSERT OR IGNORE INTO projection_thread_pull_requests (
            thread_id, host, repository, number, url, source, linked_at, snapshot_json, stack_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, 'manual', ?6, NULL, NULL)",
    )?;
    while let Some(row) = rows.next()? {
        let ValueRef::Text(raw) = row.get_ref(2)? else {
            continue;
        };
        let Ok(link) = serde_json::from_slice::<Value>(raw) else {
            continue;
        };
        let (Some(repository), Some(number), Some(url)) = (
            link["repository"].as_str(),
            link["number"].as_f64(),
            link["url"].as_str(),
        ) else {
            continue;
        };
        if legacy_pull_requests::trim(repository).is_empty()
            || legacy_pull_requests::trim(url).is_empty()
            || number < 1.0
            || number.fract() != 0.0
        {
            continue;
        }
        let comparable_number =
            (number <= legacy_pull_requests::MAX_SAFE_NUMBER as f64).then_some(number as u64);
        let (host, repository) =
            legacy_pull_requests::legacy_key(repository, url, comparable_number);
        insert.execute(params![
            row.get::<_, String>(0)?,
            host,
            repository,
            number,
            url,
            row.get::<_, String>(1)?,
        ])?;
    }
    Ok(())
}
