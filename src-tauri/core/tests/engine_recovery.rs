//! Integration tests: `ProjectState` corrupt-journal recovery.
//!
//! Tests here verify the open-recovery contract: a corrupt `type = 0` journal row
//! causes `open_project` to fall back to the latest snapshot, and a corrupt
//! snapshot causes `open_project` to return a fatal error.

use tempfile::tempdir;
use vb_core::project::engine::{EngineError, ProjectState};
use vb_core::settings::Settings;

// R1 — corrupt delta row triggers recovery: project opens via snapshot fallback.
#[test]
fn corrupt_delta_row_recovers_to_snapshot() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings::default();

    // Create a project and write an explicit snapshot.
    let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
    ps.save_snapshot_now().unwrap();
    drop(ps);

    // Inject a corrupt type=0 row (payload won't decode as a delta batch).
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 1000;")
            .unwrap();
        conn.execute(
            "INSERT INTO journal (type, payload, command_id, applied_at) \
             VALUES (0, X'DEADBEEFCAFE', 0, 0)",
            [],
        )
        .unwrap();
    }

    // Query the corrupt row's id before opening so we can assert the exact value.
    let corrupt_row_id: i64 = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 1000;")
            .unwrap();
        conn.query_row("SELECT MAX(id) FROM journal WHERE type = 0", [], |r| {
            r.get(0)
        })
        .unwrap()
    };

    // Open: load_and_replay fails on the corrupt row; fallback to load_latest_snapshot.
    let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
    let ri = outcome
        .recovery
        .expect("recovery must be Some after corrupt delta");
    assert_eq!(
        ri.failed_row, corrupt_row_id,
        "failed_row must match the injected corrupt row id"
    );
    assert!(
        ri.snapshot_id > 0,
        "snapshot_id must be a valid row id: {}",
        ri.snapshot_id
    );
    assert_eq!(ps2.sample_rate(), 48000);
    assert!(outcome.missing_tracks.is_empty());
}

// R2 — corrupt snapshot payload is a fatal error (recovery also fails).
#[test]
fn corrupt_snapshot_payload_is_fatal() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings::default();

    // Create project (writes initial type=1 row).
    let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
    drop(ps);

    // Corrupt the snapshot row's payload to be < 16 bytes (not a valid hash pointer).
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 1000;")
            .unwrap();
        conn.execute("UPDATE journal SET payload = X'DEAD' WHERE type = 1", [])
            .unwrap();
    }

    // Open must fail fatally (load_and_replay fails, AND load_latest_snapshot fails).
    let result = ProjectState::open_project(&path, &settings);
    assert!(
        result.is_err(),
        "corrupt snapshot should cause fatal open error"
    );

    // Must be the dedicated RecoveryFailed variant, not a plain Replay error.
    match result.unwrap_err() {
        EngineError::RecoveryFailed(_) => {}
        other => panic!("expected RecoveryFailed, got {other:?}"),
    }
}

// R3 — corrupt delta followed by corrupt snapshot: both recovery paths fail → fatal.
#[test]
fn corrupt_delta_and_snapshot_both_fail_fatally() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.vocalboard");
    let settings = Settings::default();

    let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
    drop(ps);

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 1000;")
            .unwrap();
        // Corrupt snapshot row (< 16 byte payload) AND add a bad delta row.
        conn.execute("UPDATE journal SET payload = X'DEAD' WHERE type = 1", [])
            .unwrap();
        conn.execute(
            "INSERT INTO journal (type, payload, command_id, applied_at) \
             VALUES (0, X'DEADBEEF', 0, 0)",
            [],
        )
        .unwrap();
    }

    let result = ProjectState::open_project(&path, &settings);
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), EngineError::RecoveryFailed(_)),
        "corrupt-snapshot recovery must return RecoveryFailed"
    );
}
