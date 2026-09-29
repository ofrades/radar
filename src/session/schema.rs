//! Versioned SQLite schema migrations for the daemon's stores.
//!
//! `CREATE TABLE IF NOT EXISTS` creates a schema but cannot evolve it: it never
//! adds a column to a table that already exists. Each store therefore stamps
//! `PRAGMA user_version` with the schema version it is at, and applies the
//! steps it has not seen yet, in order — the same ladder the app database uses.

use anyhow::Result;
use rusqlite::Connection;

/// Apply the full `steps` ladder (index 0 is version 1) to reach `target`, then
/// stamp the version. A no-op when the stored version is already at or past
/// `target`, so the `steps` are not even parsed on later opens. Callers always
/// pass the whole ladder — a new schema change appends a step and bumps
/// `target`.
pub(crate) fn migrate(connection: &Connection, target: i64, steps: &[&str]) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= target {
        return Ok(());
    }
    for (index, step) in steps.iter().enumerate() {
        let step_version = index as i64 + 1;
        if version < step_version {
            connection.execute_batch(step)?;
        }
    }
    connection.pragma_update(None, "user_version", target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::migrate;
    use rusqlite::Connection;

    #[test]
    fn migrations_run_once_stamp_the_version_and_are_idempotent() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(
            &connection,
            2,
            &["CREATE TABLE a (x INTEGER);", "CREATE TABLE b (y INTEGER);"],
        )
        .unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);

        // Already at the target: the steps are not run at all, so even an
        // invalid step is harmless.
        migrate(&connection, 2, &["THIS WOULD FAIL;", "THIS TOO;"]).unwrap();

        // Stepping up applies only the unseen step (the full ladder is passed).
        migrate(
            &connection,
            3,
            &[
                "CREATE TABLE a (x INTEGER);",
                "CREATE TABLE b (y INTEGER);",
                "CREATE TABLE c (z INTEGER);",
            ],
        )
        .unwrap();
        assert!(connection
            .query_row("SELECT COUNT(*) FROM c", [], |row| row.get::<_, i64>(0))
            .is_ok());
    }
}
