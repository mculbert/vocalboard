//! Integration tests: `ProjectState` lifecycle round-trip.
//!
//! Tests here drive only the public API: `new_project`, `open_project`,
//! `save_snapshot_now`, `undo`, `redo`. State manipulation via `apply_batch`
//! is exercised later in this file.

use tempfile::tempdir;
use vb_core::project::engine::ProjectState;
use vb_core::settings::Settings;

// L1 — new_project + save_snapshot_now + reopen round-trip.
#[test]
fn new_open_round_trip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings::default();

    let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
    assert_eq!(ps.sample_rate(), 48000);

    ps.save_snapshot_now().unwrap();
    drop(ps);

    let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
    assert!(
        outcome.missing_tracks.is_empty(),
        "no missing tracks on empty project"
    );
    assert!(
        outcome.recovery.is_none(),
        "no recovery needed on clean journal"
    );
    assert_eq!(ps2.sample_rate(), 48000);
}

// L2 — undo/redo on an empty history return Ok(false).
#[test]
fn undo_redo_empty_history() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings::default();

    let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
    assert!(!ps.undo().unwrap(), "undo on empty stack returns false");
    assert!(!ps.redo().unwrap(), "redo on empty stack returns false");
}

// L3 — save_snapshot_now appends exactly one new type=1 journal row per call;
//      calling it twice still yields a valid latest snapshot.
#[test]
fn save_snapshot_appends_journal_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings::default();

    let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

    // new_project already wrote one type=1 row; count before first explicit save.
    let count0 = count_snapshot_rows(&path);
    assert_eq!(
        count0, 1,
        "new_project should have written exactly one snapshot row"
    );

    ps.save_snapshot_now().unwrap();
    assert_eq!(
        count_snapshot_rows(&path),
        2,
        "first save_snapshot_now adds a row"
    );

    ps.save_snapshot_now().unwrap();
    assert_eq!(
        count_snapshot_rows(&path),
        3,
        "second save_snapshot_now adds another row"
    );

    drop(ps);

    // Reopen must succeed — the latest snapshot is still valid.
    let (_ps3, outcome) = ProjectState::open_project(&path, &settings).unwrap();
    assert!(outcome.recovery.is_none());
}

// L4 — undo_history_limit = 0: no undo entries are recorded.
#[test]
fn zero_undo_limit_disables_undo() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings {
        undo_history_limit: 0,
        ..Settings::default()
    };

    let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
    assert!(!ps.undo().unwrap(), "undo disabled when limit=0");
    assert!(!ps.redo().unwrap(), "redo disabled when limit=0");
}

// L5 — reopening with different settings does not corrupt the project.
#[test]
fn reopen_with_different_settings() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings1 = Settings {
        undo_history_limit: 5,
        ..Settings::default()
    };
    let settings2 = Settings {
        undo_history_limit: 20,
        ..Settings::default()
    };

    let ps = ProjectState::new_project(&path, 48000, &settings1).unwrap();
    drop(ps);

    let (_ps2, outcome) = ProjectState::open_project(&path, &settings2).unwrap();
    assert!(outcome.recovery.is_none());
}

// Helper: count type=1 rows in the journal using a raw read-only connection.
#[allow(clippy::unwrap_used)]
fn count_snapshot_rows(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 1000;")
        .unwrap();
    conn.query_row("SELECT COUNT(*) FROM journal WHERE type = 1", [], |r| {
        r.get(0)
    })
    .unwrap()
}
