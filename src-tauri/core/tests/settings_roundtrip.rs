//! Format round-trip fixture tests for `settings.json` (convention G1).
//!
//! Each fixture under `tests/fixtures/` represents a settings file written by a
//! prior format version.  Every fixture must load cleanly through the migration
//! path and round-trip without data loss for the fields this version knows about.

use vb_core::settings::{self, ResamplingQuality, Settings};

/// The canonical v1 fixture must deserialise to the Phase-1 defaults.
///
/// The fixture intentionally includes an unknown key (`_future_key`) to prove
/// that forward-compatible keys do not cause loading to fail.  Unknown keys are
/// preserved in the `tauri-plugin-store` because the app writes individual keys
/// rather than replacing the whole file.
#[test]
fn settings_v1_loads_with_correct_defaults() -> anyhow::Result<()> {
    let raw: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/settings_v1.json"))?;

    let s = Settings::from_json(&raw)?;

    assert_eq!(s.version, 1);
    assert_eq!(s.default_sample_rate, settings::DEFAULT_SAMPLE_RATE);
    assert!(
        (s.speaker_merge_threshold - settings::DEFAULT_SPEAKER_MERGE_THRESHOLD).abs()
            < f64::EPSILON
    );
    assert_eq!(s.resampling_quality, ResamplingQuality::Balanced);
    assert!(!s.gpu_enabled);
    assert_eq!(
        s.snapshot_idle_seconds,
        settings::DEFAULT_SNAPSHOT_IDLE_SECONDS
    );
    assert_eq!(
        s.model_idle_unload_seconds,
        settings::DEFAULT_MODEL_IDLE_UNLOAD_SECONDS
    );
    assert!(s.update_feed_url.is_none());
    assert!(s.recent_projects.is_empty());
    assert!(s.model_dir.is_none());
    assert!(s.model_paths.transcription.is_none());
    Ok(())
}

/// An empty JSON object (simulating a brand-new or missing store) must produce
/// default settings with version bumped to 1.
#[test]
fn migrate_empty_object_produces_v1_defaults() -> anyhow::Result<()> {
    let raw = serde_json::json!({});
    let s = Settings::from_json(&raw)?;
    assert_eq!(s.version, 1);
    assert_eq!(s.default_sample_rate, settings::DEFAULT_SAMPLE_RATE);
    Ok(())
}

/// A missing `version` field is treated as v0 and migrated to v1.
#[test]
fn migrate_missing_version_field() -> anyhow::Result<()> {
    let raw = serde_json::json!({ "gpu_enabled": true });
    let s = Settings::from_json(&raw)?;
    assert_eq!(s.version, 1);
    assert!(s.gpu_enabled);
    Ok(())
}

/// `migrate` is idempotent: a current-version value passes through unchanged.
#[test]
fn migrate_is_idempotent_for_current_version() -> anyhow::Result<()> {
    let defaults = Settings::default();
    let json = defaults.to_json()?;
    let migrated = settings::migrate(json.clone())?;
    let reloaded = Settings::from_json(&migrated)?;

    assert_eq!(defaults.version, reloaded.version);
    assert_eq!(defaults.default_sample_rate, reloaded.default_sample_rate);
    assert_eq!(
        defaults.speaker_merge_threshold,
        reloaded.speaker_merge_threshold
    );
    assert_eq!(
        defaults.snapshot_idle_seconds,
        reloaded.snapshot_idle_seconds
    );
    assert_eq!(defaults.gpu_enabled, reloaded.gpu_enabled);
    Ok(())
}

/// Unknown keys in the JSON (from a newer app version) do not prevent loading.
#[test]
fn unknown_keys_do_not_prevent_load() -> anyhow::Result<()> {
    let raw = serde_json::json!({
        "version": 1,
        "default_sample_rate": 44100,
        "phase_7_feature": { "enabled": true }
    });
    let s = Settings::from_json(&raw)?;
    assert_eq!(s.default_sample_rate, 44_100);
    Ok(())
}
