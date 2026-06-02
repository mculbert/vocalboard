//! Project-singleton-row helpers: read/write the single `project` table row.
//!
//! Every project database has exactly one row in the `project` table (`CHECK (id = 1)`
//! enforced by the migration). These helpers keep raw SQL for that table out of
//! `engine.rs`.

use rusqlite::Connection;

/// Write the initial `project` singleton row.
///
/// Called once by [`crate::project::engine::ProjectState::new_project`] inside the
/// creation transaction. Timestamps are set to `datetime('now')` by SQLite.
pub(crate) fn insert_project_row(conn: &Connection, sample_rate: u32) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO project (id, sample_rate, created_at, updated_at) \
         VALUES (1, ?1, datetime('now'), datetime('now'))",
        rusqlite::params![sample_rate],
    )?;
    Ok(())
}

/// Read the project sample rate from the singleton `project` row.
///
/// Called by [`crate::project::engine::ProjectState::open_project`].
pub(crate) fn read_sample_rate(conn: &Connection) -> rusqlite::Result<u32> {
    conn.query_row("SELECT sample_rate FROM project WHERE id = 1", [], |r| {
        r.get(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use tempfile::tempdir;

    // P1 — insert_project_row + read_sample_rate round-trip on a fresh Db.
    #[test]
    fn project_row_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let mut db = Db::create(&path).unwrap();
        let tx = db.conn_mut().transaction().unwrap();
        insert_project_row(&tx, 44100).unwrap();
        tx.commit().unwrap();
        let sr = read_sample_rate(db.conn()).unwrap();
        assert_eq!(sr, 44100);
    }

    // P2 — read_sample_rate reflects the value written.
    #[test]
    fn read_sample_rate_48k() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let mut db = Db::create(&path).unwrap();
        let tx = db.conn_mut().transaction().unwrap();
        insert_project_row(&tx, 48000).unwrap();
        tx.commit().unwrap();
        assert_eq!(read_sample_rate(db.conn()).unwrap(), 48000);
    }
}
