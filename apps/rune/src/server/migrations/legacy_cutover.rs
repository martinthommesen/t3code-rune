use rusqlite::Transaction;

pub(super) fn migrate_55(database: &Transaction<'_>) -> rusqlite::Result<()> {
    database.execute_batch(include_str!("legacy_cutover/base.sql"))?;
    database.execute_batch(include_str!("legacy_cutover/event_source.sql"))?;
    database.execute_batch(include_str!("legacy_cutover/finalize.sql"))
}
