pub(in crate::server) mod citation_search;
mod legacy_cutover;
mod legacy_pre38;
mod legacy_pre52;
mod legacy_pull_requests;
mod legacy_settlement;
mod sql;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

const MINIMUM_HEAD: i64 = 30;
const PIN_ORDER_HEAD: i64 = 38;
const TITLE_STATE_HEAD: i64 = 52;

struct Migration {
    id: i64,
    name: &'static str,
    action: MigrationAction,
}

enum MigrationAction {
    Sql(&'static str),
    Script(fn(&Transaction<'_>) -> rusqlite::Result<()>),
}

impl Migration {
    const fn sql(id: i64, name: &'static str, statements: &'static str) -> Self {
        Self {
            id,
            name,
            action: MigrationAction::Sql(statements),
        }
    }

    const fn scripted(
        id: i64,
        name: &'static str,
        action: fn(&Transaction<'_>) -> rusqlite::Result<()>,
    ) -> Self {
        Self {
            id,
            name,
            action: MigrationAction::Script(action),
        }
    }

    fn apply(&self, transaction: &Transaction<'_>) -> rusqlite::Result<()> {
        match self.action {
            MigrationAction::Sql(statements) => transaction.execute_batch(statements),
            MigrationAction::Script(script) => script(transaction),
        }
    }
}

#[derive(Debug)]
pub(super) enum MigrationError {
    ReadHistory(rusqlite::Error),
    UnsupportedBase {
        head: i64,
    },
    UnsupportedPreview,
    InvalidReceipt {
        id: i64,
        expected: &'static str,
        recorded: Option<String>,
    },
    Apply {
        id: i64,
        name: &'static str,
        source: rusqlite::Error,
    },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadHistory(source) => write!(
                formatter,
                "Could not read the T3 database migration history: {source}"
            ),
            Self::UnsupportedBase { head } => write!(
                formatter,
                "T3 database migration {head} requires an earlier upgrade. The Rust host upgrades databases starting with migration {MINIMUM_HEAD}."
            ),
            Self::UnsupportedPreview => {
                formatter.write_str("Cannot upgrade V2 preview with unexpected later migrations.")
            }
            Self::InvalidReceipt {
                id,
                expected,
                recorded,
            } => {
                let recorded = recorded.as_deref().unwrap_or("no receipt");
                write!(
                    formatter,
                    "T3 database migration {id} requires {expected}, recorded {recorded}."
                )
            }
            Self::Apply { id, name, source } => write!(
                formatter,
                "Could not apply T3 database migration {id}_{name}: {source}"
            ),
        }
    }
}

impl Error for MigrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadHistory(error) | Self::Apply { source: error, .. } => Some(error),
            _ => None,
        }
    }
}

pub(super) fn migrate(path: &Path) -> Result<(), MigrationError> {
    let mut database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(MigrationError::ReadHistory)?;
    database
        .busy_timeout(Duration::from_secs(5))
        .map_err(MigrationError::ReadHistory)?;
    database
        .pragma_update(None, "foreign_keys", true)
        .map_err(MigrationError::ReadHistory)?;
    reconcile_preview(&mut database)?;
    let mut head = validated_head(&database)?;

    for migration in migrations() {
        if migration.id <= head {
            continue;
        }
        let failed = |source| MigrationError::Apply {
            id: migration.id,
            name: migration.name,
            source,
        };
        let transaction = database
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failed)?;
        head = validated_head(&transaction)?;
        if migration.id <= head {
            continue;
        }
        migration.apply(&transaction).map_err(failed)?;
        transaction
            .execute(
                "INSERT INTO effect_sql_migrations (migration_id, name) VALUES (?1, ?2)",
                params![migration.id, migration.name],
            )
            .map_err(failed)?;
        transaction.commit().map_err(failed)?;
        head = migration.id;
    }
    Ok(())
}

fn migrations() -> impl Iterator<Item = &'static Migration> {
    legacy_pre38::MIGRATIONS
        .iter()
        .chain(legacy_pre52::MIGRATIONS)
        .chain(sql::MIGRATIONS)
}

fn add_column(
    db: &Transaction<'_>,
    table: &'static str,
    column: &'static str,
    definition: &'static str,
) -> rusqlite::Result<()> {
    let exists: bool = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    )?;
    if !exists {
        db.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

fn preview_id(database: &Connection) -> rusqlite::Result<Option<i64>> {
    database
        .query_row(
            "SELECT migration_id FROM effect_sql_migrations
         WHERE migration_id IN (53, 54) AND name = 'OrchestrationV2'
         ORDER BY migration_id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
}

fn reconcile_preview(database: &mut Connection) -> Result<(), MigrationError> {
    if preview_id(database)
        .map_err(MigrationError::ReadHistory)?
        .is_none()
    {
        return Ok(());
    }
    let failed = |source| MigrationError::Apply {
        id: 55,
        name: "OrchestrationV2",
        source,
    };
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(failed)?;
    let Some(id) = preview_id(&transaction).map_err(failed)? else {
        return Ok(());
    };
    let unexpected: bool = transaction
        .query_row(
            "SELECT EXISTS (
           SELECT 1 FROM effect_sql_migrations WHERE migration_id >= 53
           AND NOT (migration_id = ?1 AND name = 'OrchestrationV2')
           AND NOT (?1 = 54 AND (
               (migration_id = 53 AND name = 'PullRequestFilesViewed') OR
               (migration_id = 55 AND name = 'RemoveRedundantProjectionIndexes')
           ))
         )",
            [id],
            |row| row.get(0),
        )
        .map_err(failed)?;
    if unexpected {
        return Err(MigrationError::UnsupportedPreview);
    }
    if id == 53 {
        sql::PULL_REQUEST_FILES_VIEWED
            .apply(&transaction)
            .map_err(failed)?;
    }
    sql::AUTO_SETTLE_DISABLED_AT
        .apply(&transaction)
        .map_err(failed)?;
    transaction
        .execute_batch(
            "UPDATE effect_sql_migrations SET migration_id = 56
         WHERE migration_id = 55 AND name = 'RemoveRedundantProjectionIndexes';",
        )
        .map_err(failed)?;
    transaction
        .execute(
            "UPDATE effect_sql_migrations SET migration_id = 55
         WHERE migration_id = ?1 AND name = 'OrchestrationV2'",
            [id],
        )
        .map_err(failed)?;
    if id == 53 {
        transaction.execute_batch(
            "INSERT INTO effect_sql_migrations (migration_id, name) VALUES (53, 'PullRequestFilesViewed');",
        ).map_err(failed)?;
    }
    transaction.execute_batch(
        "INSERT INTO effect_sql_migrations (migration_id, name) VALUES (54, 'ProjectionThreadsAutoSettleDisabledAt');",
    ).map_err(failed)?;
    transaction.commit().map_err(failed)
}

fn validated_head(database: &Connection) -> Result<i64, MigrationError> {
    let mut query = database
        .prepare("SELECT migration_id, name FROM effect_sql_migrations")
        .map_err(MigrationError::ReadHistory)?;
    let history: BTreeMap<i64, String> = query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(MigrationError::ReadHistory)?
        .collect::<rusqlite::Result<_>>()
        .map_err(MigrationError::ReadHistory)?;
    let head = history.last_key_value().map_or(0, |(id, _)| *id);
    if head < MINIMUM_HEAD {
        return Err(MigrationError::UnsupportedBase { head });
    }

    let required = if head >= TITLE_STATE_HEAD {
        TITLE_STATE_HEAD
    } else if head >= PIN_ORDER_HEAD {
        PIN_ORDER_HEAD
    } else {
        MINIMUM_HEAD
    };
    let expected = sql::BASE_HISTORY
        .iter()
        .copied()
        .chain(migrations().map(|migration| (migration.id, migration.name)))
        .skip_while(|(id, _)| *id < required);
    for (id, name) in expected.take_while(|(id, _)| *id <= head) {
        let recorded = history.get(&id);
        if recorded.is_none_or(|recorded| recorded != name) {
            return Err(MigrationError::InvalidReceipt {
                id,
                expected: name,
                recorded: recorded.cloned(),
            });
        }
    }
    Ok(head)
}

#[cfg(test)]
mod tests;
