//! Schema migration runner: applies numbered SQL scripts in order.
//!
//! Each entry in `MIGRATIONS` is a SQL script that advances the schema by one
//! version. The index of an entry equals `user_version` before the migration
//! runs (so index 0 is the 0→1 migration). Every migration is wrapped in a
//! transaction; `user_version` is updated atomically with the DDL changes.

use anyhow::{bail, Result};
use rusqlite::Connection;

/// SQL scripts embedded at compile time; index N advances from version N to N+1.
static MIGRATIONS: &[&str] = &[include_str!("../../migrations/0001_initial.sql")];

/// Maximum `user_version` this build supports.
pub(super) const MAX_VERSION: u32 = MIGRATIONS.len() as u32;

/// Apply any pending migrations to `conn`.
///
/// Returns an error if the file's `user_version` exceeds `MAX_VERSION` —
/// this means the project was created by a newer build and cannot be safely
/// opened here.
pub(super) fn run(conn: &mut Connection) -> Result<()> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    if current > MAX_VERSION {
        // Best-effort: read min_app_version from the project table for a helpful
        // message. Falls back gracefully if the table layout has changed beyond
        // recognition.
        let min_ver = conn
            .query_row(
                "SELECT min_app_version FROM project WHERE id = 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "a newer version".to_owned());
        bail!(
            "This project file requires {min_ver} of Vocalboard \
             (schema version {current}; this build supports up to {MAX_VERSION})."
        );
    }

    for i in current..MAX_VERSION {
        let sql = MIGRATIONS[i as usize];
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // user_version is stored in the database header; the update is
        // transactional in SQLite, so a mid-migration crash leaves version
        // unchanged and the migration will be retried on next open.
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::tempdir;

    fn open_in_tempdir() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vocalboard");
        let conn = Connection::open(&path).unwrap();
        (dir, conn)
    }

    #[test]
    fn fresh_db_reaches_max_version() {
        let (_dir, mut conn) = open_in_tempdir();
        run(&mut conn).unwrap();
        let ver: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, MAX_VERSION);
    }

    #[test]
    fn reopen_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vocalboard");

        {
            let mut conn = Connection::open(&path).unwrap();
            run(&mut conn).unwrap();
        }
        {
            let mut conn = Connection::open(&path).unwrap();
            // Second run must not fail and must leave version unchanged.
            run(&mut conn).unwrap();
            let ver: u32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(ver, MAX_VERSION);
        }
    }

    #[test]
    fn future_version_is_refused() {
        let (_dir, mut conn) = open_in_tempdir();
        // Simulate a project file from a future build.
        conn.pragma_update(None, "user_version", MAX_VERSION + 1)
            .unwrap();
        let err = run(&mut conn).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("schema version"),
            "error should mention schema version: {msg}"
        );
    }
}
