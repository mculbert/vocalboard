//! Format round-trip fixture tests for `.vocalboard` project files (convention G1).
//!
//! Each fixture under `tests/fixtures/` represents a project file written by a
//! prior format version. Every fixture must load cleanly through the public API
//! and produce the expected observable state.

use tempfile::tempdir;
use vb_core::project::engine::ProjectState;
use vb_core::settings::Settings;

/// The committed v1 project fixture must open via the public API with the expected
/// sample rate, no recovery, and the known missing-track id (which proves that the
/// metadata blob was decoded — an empty metadata would yield no missing tracks).
#[test]
fn fixture_v1_opens() {
    let bytes = include_bytes!("fixtures/project_v1.vocalboard");
    let dir = tempdir().unwrap();
    let path = dir.path().join("p.vocalboard");
    std::fs::write(&path, bytes).unwrap();

    let settings = Settings::default();
    let (ps, outcome) = ProjectState::open_project(&path, &settings).unwrap();

    assert_eq!(ps.sample_rate(), 48000);
    assert!(
        outcome.recovery.is_none(),
        "fixture must have a clean journal"
    );
    assert_eq!(
        outcome.missing_tracks,
        vec![1],
        "track 1 has nonexistent paths — missing_tracks must be [1]"
    );
}
