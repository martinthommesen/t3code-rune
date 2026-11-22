use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

const SCHEMA: &str = include_str!("bootstrap/schema.sql");
const JS_WHITESPACE_SUFFIX: &str = "\u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

pub(super) fn snapshot_legacy_database(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.file_name().is_none_or(|name| name != "statev2.sqlite") || path.try_exists()? {
        return Ok(());
    }
    let directory = path.parent().expect("the state database path is absolute");
    let legacy = directory.join("state.sqlite");
    if !legacy.try_exists()? {
        return Ok(());
    }
    let scratch = tempfile::Builder::new()
        .prefix(&format!(".v2-import-{}-", std::process::id()))
        .tempdir_in(directory)?;
    let snapshot = scratch.path().join("snapshot.sqlite");
    let filename = snapshot.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "The SQLite snapshot path is not valid UTF-8.",
        )
    })?;
    let source = Connection::open_with_flags(&legacy, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::from_secs(5))?;
    source.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    source.execute("VACUUM INTO ?1", [filename])?;
    source.close().map_err(|(_, error)| error)?;
    match fs::hard_link(snapshot, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn initialize(path: &Path) -> rusqlite::Result<()> {
    let mut database = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    database.busy_timeout(Duration::from_secs(5))?;
    if has_schema(&database)? {
        return Ok(());
    }

    database.pragma_update(None, "foreign_keys", true)?;
    // Persistent modes must be set before the transaction. The host holds the ownership lock.
    database.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    database.pragma_update(None, "journal_mode", "WAL")?;
    let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !has_schema(&transaction)? {
        let schema = SCHEMA
            .replace(
                "__T3_SQL_TRIM_WHITESPACE__",
                &format!(" \t\n\r\x0b\x0c{JS_WHITESPACE_SUFFIX}"),
            )
            .replace(
                "__T3_SQL_SEARCH_WHITESPACE__",
                &format!("\t\n\x0b\x0c\r {JS_WHITESPACE_SUFFIX}"),
            );
        transaction.execute_batch(&schema)?;
    }
    transaction.commit()
}

fn has_schema(database: &Connection) -> rusqlite::Result<bool> {
    database.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_schema)", [], |row| {
        row.get(0)
    })
}

#[cfg(test)]
mod tests;
