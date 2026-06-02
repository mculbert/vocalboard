//! Database handle: open/create, connection-level pragmas, and transaction helpers.

pub(crate) mod journal;
mod migrations;
pub(crate) mod project;
pub mod store;

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;

/// Error returned by [`Db::create`] and [`Db::open`].
#[derive(Debug)]
pub enum DbOpenError {
    /// [`Db::create`] was called but the path already exists.
    AlreadyExists(PathBuf),
    /// [`Db::open`] was called but the path does not exist.
    NotFound(PathBuf),
    /// A SQLite connection or pragma error.
    Sqlite(rusqlite::Error),
    /// A schema migration error (version too new, DDL failure, etc.).
    Migration(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for DbOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists(p) => write!(f, "project file already exists: {}", p.display()),
            Self::NotFound(p) => write!(f, "project file not found: {}", p.display()),
            Self::Sqlite(e) => write!(f, "database connection error: {e}"),
            Self::Migration(e) => write!(f, "schema migration error: {e}"),
        }
    }
}

impl std::error::Error for DbOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyExists(_) | Self::NotFound(_) => None,
            Self::Sqlite(e) => Some(e),
            Self::Migration(e) => Some(&**e),
        }
    }
}

/// Wraps a `rusqlite` connection with migrations applied and connection-level
/// pragmas enforced.
pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Db {
    /// Create a new project database at `path`.
    ///
    /// Errors with [`DbOpenError::AlreadyExists`] if the file already exists, so
    /// callers need not worry about silently migrating an unrelated SQLite file.
    /// Sets WAL mode, foreign-key enforcement, and a busy timeout, then runs
    /// schema migrations.
    pub fn create(path: &Path) -> Result<Self, DbOpenError> {
        if path.try_exists().unwrap_or(false) {
            return Err(DbOpenError::AlreadyExists(path.to_owned()));
        }
        let mut conn = open_conn(path).map_err(DbOpenError::Sqlite)?;
        migrations::run(&mut conn).map_err(|e| DbOpenError::Migration(e.into()))?;
        Ok(Self {
            conn,
            path: path.to_owned(),
        })
    }

    /// Open an existing project database at `path`.
    ///
    /// Errors with [`DbOpenError::NotFound`] if the file does not exist, so callers
    /// get a clear error instead of silently creating an empty project. Sets WAL mode,
    /// foreign-key enforcement, and a busy timeout, then runs any pending schema
    /// migrations. Returns a migration error if the file's `user_version` exceeds the
    /// maximum this build supports (project was created by a newer app version).
    pub fn open(path: &Path) -> Result<Self, DbOpenError> {
        if !path.try_exists().unwrap_or(true) {
            return Err(DbOpenError::NotFound(path.to_owned()));
        }
        let mut conn = open_conn(path).map_err(DbOpenError::Sqlite)?;
        migrations::run(&mut conn).map_err(|e| DbOpenError::Migration(e.into()))?;
        Ok(Self {
            conn,
            path: path.to_owned(),
        })
    }

    /// Open a second connection to the same project file without re-running migrations.
    ///
    /// Used by [`crate::project::engine::ProjectState`]'s snapshot writer to hold a
    /// dedicated write connection. Structurally uncallable without a primary [`Db`] in
    /// hand — the primary is always the one that creates and migrates the file, so the
    /// shared connection is guaranteed to find a fully-migrated schema.
    ///
    /// The cpal real-time path never holds any connection; only a racing snapshot commit
    /// (from M5 on) can cause `busy_timeout` waits, which are bounded by the 5000 ms
    /// pragma applied by [`open_conn`].
    pub(crate) fn open_shared(&self) -> Result<Self, DbOpenError> {
        let conn = open_conn(&self.path).map_err(DbOpenError::Sqlite)?;
        Ok(Self {
            conn,
            path: self.path.clone(),
        })
    }

    /// Run `f` inside a new transaction, committing on `Ok` and rolling back
    /// on `Err`.
    #[allow(dead_code)]
    pub(crate) fn with_transaction<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    {
        let tx = self.conn.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Borrow the underlying connection for queries outside a transaction.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable borrow of the connection — needed to open a transaction.
    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Open a SQLite connection at `path` and apply connection-level pragmas.
///
/// The single place in production code where [`Connection::open`] is called. All other
/// code goes through [`Db::create`], [`Db::open`], or [`Db::open_shared`].
///
/// WAL, foreign-key enforcement, and `busy_timeout` are connection-level settings that
/// must be applied on every open. `busy_timeout = 5000` makes a writer that finds the
/// lock held wait and retry rather than fail immediately with `SQLITE_BUSY` — required
/// from M5 on, when edit commands can race the snapshot writer's separate connection.
fn open_conn(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
    )?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn db_debug_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = Db::create(&path).unwrap();
        let s = format!("{db:?}");
        assert!(s.contains("Db"), "debug output should contain Db: {s}");
    }

    #[test]
    fn create_builds_schema_at_max_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = Db::create(&path).unwrap();
        let ver: u32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, migrations::MAX_VERSION);
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        drop(Db::create(&path).unwrap());
        let db = Db::open(&path).unwrap();
        let ver: u32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, migrations::MAX_VERSION);
    }

    #[test]
    fn future_version_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        // Doctor the user_version to simulate a newer-format file.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", migrations::MAX_VERSION + 1)
                .unwrap();
        }
        let err = Db::open(&path).unwrap_err();
        assert!(
            matches!(err, DbOpenError::Migration(_)),
            "future version should produce Migration error: {err:?}"
        );
        assert!(
            err.to_string().contains("schema version"),
            "error message should cite schema version: {err}"
        );
    }

    #[test]
    fn with_transaction_commits_on_ok() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let mut db = Db::create(&path).unwrap();
        db.with_transaction(|tx| {
            tx.execute_batch(
                "INSERT INTO store (hash, payload) VALUES (X'00000000000000000000000000000001', X'01');",
            )?;
            Ok(())
        })
        .unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn with_transaction_rolls_back_on_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let mut db = Db::create(&path).unwrap();
        let _: Result<()> = db.with_transaction(|tx| {
            tx.execute_batch(
                "INSERT INTO store (hash, payload) VALUES (X'00000000000000000000000000000001', X'01');",
            )?;
            anyhow::bail!("intentional failure")
        });
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // C1 — Db::create fails when the path already exists and does not mangle the file.
    #[test]
    fn create_fails_if_file_exists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = Db::create(&path).unwrap();
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        drop(db);

        let err = Db::create(&path).unwrap_err();
        assert!(
            matches!(err, DbOpenError::AlreadyExists(_)),
            "expected AlreadyExists, got {err:?}"
        );

        // Re-open must still succeed with original row count intact.
        let db2 = Db::open(&path).unwrap();
        let rows2: i64 = db2
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows, rows2,
            "existing file must not be mangled by failed create"
        );
    }

    // C2 — Db::open fails when the path is absent.
    #[test]
    fn open_fails_if_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.vocalboard");
        let err = Db::open(&path).unwrap_err();
        assert!(
            matches!(err, DbOpenError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    // C3 — open_shared returns a working second connection that reads primary writes.
    #[test]
    fn open_shared_reads_primary_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let mut primary = Db::create(&path).unwrap();

        primary
            .conn_mut()
            .execute_batch(
                "INSERT INTO store (hash, payload) \
             VALUES (X'00000000000000000000000000000002', X'02');",
            )
            .unwrap();

        let shared = primary.open_shared().unwrap();
        let count: i64 = shared
            .conn()
            .query_row("SELECT COUNT(*) FROM store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "shared connection must see primary's writes");
    }

    // C4 — DbOpenError Display and source() are non-empty / wired for all variants.
    #[test]
    fn db_open_error_display_and_source() {
        use std::error::Error;

        let variants: Vec<DbOpenError> = vec![
            DbOpenError::AlreadyExists(PathBuf::from("/a")),
            DbOpenError::NotFound(PathBuf::from("/b")),
            DbOpenError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
            DbOpenError::Migration(Box::new(rusqlite::Error::QueryReturnedNoRows)),
        ];
        for v in &variants {
            assert!(
                !v.to_string().is_empty(),
                "Display must be non-empty: {v:?}"
            );
        }
        assert!(variants[0].source().is_none());
        assert!(variants[1].source().is_none());
        assert!(variants[2].source().is_some());
        assert!(variants[3].source().is_some());
    }
}
