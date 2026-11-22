use super::{Migration, add_column};

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration::sql(
        24,
        "BackfillProjectionThreadShellSummary",
        include_str!("legacy_pre30/thread_shell_summary.sql"),
    ),
    Migration::sql(
        25,
        "CleanupInvalidProjectionPendingApprovals",
        include_str!("legacy_pre30/invalid_pending_approvals.sql"),
    ),
    Migration::sql(
        26,
        "CanonicalizeModelSelectionOptions",
        include_str!("legacy_pre30/model_selection_options.sql"),
    ),
    Migration::scripted(27, "ProviderSessionRuntimeInstanceId", |db| {
        add_column(
            db,
            "provider_session_runtime",
            "provider_instance_id",
            "TEXT",
        )?;
        db.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_provider_session_runtime_instance
             ON provider_session_runtime(provider_instance_id)",
        )
    }),
    Migration::scripted(28, "ProjectionThreadSessionInstanceId", |db| {
        add_column(
            db,
            "projection_thread_sessions",
            "provider_instance_id",
            "TEXT",
        )?;
        db.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_projection_thread_sessions_instance
             ON projection_thread_sessions(provider_instance_id)",
        )
    }),
    Migration::sql(
        29,
        "ProjectionThreadDetailOrderingIndexes",
        include_str!("legacy_pre30/thread_detail_ordering.sql"),
    ),
    Migration::sql(
        30,
        "ProjectionThreadShellArchiveIndexes",
        include_str!("legacy_pre30/thread_shell_archive.sql"),
    ),
];
