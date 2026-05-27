//! Application-level settings: schema, defaults, versioned migration, and JSON I/O.
//!
//! Settings are persisted via `tauri-plugin-store` in `settings.json` in the
//! platform app-config directory.  This module is intentionally free of any Tauri
//! dependency so it can be exercised in unit and fixture-based tests without a
//! running Tauri runtime.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Settings format version written by this build.
pub const CURRENT_VERSION: u32 = 1;

/// Default project sample rate (Hz), shown in the new-project dialog.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Default cosine-similarity cutoff for auto-merging imported speakers.
pub const DEFAULT_SPEAKER_MERGE_THRESHOLD: f64 = 0.71;

/// Default seconds of user inactivity before an auto-snapshot is written.
pub const DEFAULT_SNAPSHOT_IDLE_SECONDS: u64 = 30;

/// Default seconds of model inactivity before the sidecar unloads it.
pub const DEFAULT_MODEL_IDLE_UNLOAD_SECONDS: u64 = 300;

/// Sinc-resampler quality preset (maps to rubato interpolation parameters).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResamplingQuality {
    /// Balanced speed/quality trade-off.
    #[default]
    Balanced,
    /// Higher quality, slower processing.
    High,
    /// Maximum quality, slowest processing.
    Highest,
}

/// Per-role model file or directory paths selected by the user.
///
/// Each field is either a path inside `Settings::model_dir` (a downloaded model),
/// an external user-supplied path, or `None` (no model selected for that role).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPaths {
    /// WhisperX transcription model directory.
    pub transcription: Option<PathBuf>,
    /// Reserved slot for a standalone VAD model (unused in Phase 1; WhisperX
    /// supplies its own internal VAD).
    pub vad: Option<PathBuf>,
    /// Forced-alignment model (if separate from `transcription`).
    pub forced_alignment: Option<PathBuf>,
    /// MP-SENet speech-enhancement model directory.
    pub enhancement: Option<PathBuf>,
    /// YAMnet sound-classification model directory.
    pub sound_classification: Option<PathBuf>,
    /// Gemma `.gguf` file path for LLM-assisted disfluency suggestions.
    pub llm: Option<PathBuf>,
}

/// Application-level settings persisted in `settings.json`.
///
/// Use [`Settings::from_json`] to load (and migrate) a raw JSON value, and
/// [`Settings::to_json`] to produce the JSON object to write back.  The caller
/// MUST write individual keys to the store rather than replacing the whole file
/// so that unknown keys written by a newer app version survive the round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Format version; bumped on every structural change to enable migration.
    pub version: u32,
    /// Default model directory (download target and scan root).
    /// `None` means the platform app-data directory is used.
    #[serde(default)]
    pub model_dir: Option<PathBuf>,
    /// Per-role selected model paths.
    #[serde(default)]
    pub model_paths: ModelPaths,
    /// Default sample rate (Hz) shown in the new-project dialog.
    #[serde(default = "default_sample_rate")]
    pub default_sample_rate: u32,
    /// Cosine-similarity threshold for auto-merging imported speakers.
    #[serde(default = "default_speaker_merge_threshold")]
    pub speaker_merge_threshold: f64,
    /// Sinc-resampler quality preset.
    #[serde(default)]
    pub resampling_quality: ResamplingQuality,
    /// Whether GPU acceleration is installed and enabled.
    #[serde(default)]
    pub gpu_enabled: bool,
    /// Seconds of user inactivity before writing an auto-snapshot.
    #[serde(default = "default_snapshot_idle_seconds")]
    pub snapshot_idle_seconds: u64,
    /// Seconds of model inactivity before the sidecar unloads it from memory.
    #[serde(default = "default_model_idle_unload_seconds")]
    pub model_idle_unload_seconds: u64,
    /// Auto-update feed URL; `None` until Phase 6 enables auto-update.
    #[serde(default)]
    pub update_feed_url: Option<String>,
    /// Paths to recently-opened project files, most-recent first.
    #[serde(default)]
    pub recent_projects: Vec<PathBuf>,
}

fn default_sample_rate() -> u32 {
    DEFAULT_SAMPLE_RATE
}
fn default_speaker_merge_threshold() -> f64 {
    DEFAULT_SPEAKER_MERGE_THRESHOLD
}
fn default_snapshot_idle_seconds() -> u64 {
    DEFAULT_SNAPSHOT_IDLE_SECONDS
}
fn default_model_idle_unload_seconds() -> u64 {
    DEFAULT_MODEL_IDLE_UNLOAD_SECONDS
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            model_dir: None,
            model_paths: ModelPaths::default(),
            default_sample_rate: DEFAULT_SAMPLE_RATE,
            speaker_merge_threshold: DEFAULT_SPEAKER_MERGE_THRESHOLD,
            resampling_quality: ResamplingQuality::Balanced,
            gpu_enabled: false,
            snapshot_idle_seconds: DEFAULT_SNAPSHOT_IDLE_SECONDS,
            model_idle_unload_seconds: DEFAULT_MODEL_IDLE_UNLOAD_SECONDS,
            update_feed_url: None,
            recent_projects: vec![],
        }
    }
}

impl Settings {
    /// Parse and migrate a raw JSON value (e.g. from `tauri-plugin-store` entries)
    /// into a typed `Settings`.
    ///
    /// Applies all pending version migrations in sequence, then deserializes.
    /// Missing fields fall back to their defaults.  Unknown fields are silently
    /// ignored — they remain in the caller's store and survive the round-trip
    /// provided the caller writes individual keys rather than replacing the file.
    pub fn from_json(raw: &serde_json::Value) -> Result<Self> {
        let migrated = migrate(raw.clone())?;
        serde_json::from_value(migrated).context("deserialize settings after migration")
    }

    /// Serialize to a JSON object for writing back to `tauri-plugin-store`.
    ///
    /// Only the fields this version of the app knows about are present.
    /// The caller must write individual keys to preserve unknown forward-compat keys.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).context("serialize Settings")
    }
}

/// Apply all pending migrations to bring `raw` up to [`CURRENT_VERSION`].
///
/// Versions are read from the JSON itself and migrations run in sequence, so
/// this function is idempotent: calling it on an already-current value is a no-op.
pub fn migrate(raw: serde_json::Value) -> Result<serde_json::Value> {
    let mut value = raw;
    loop {
        let ver = version_of(&value);
        if ver >= CURRENT_VERSION {
            break;
        }
        value = apply_migration(value, ver)?;
    }
    Ok(value)
}

fn version_of(value: &serde_json::Value) -> u32 {
    value
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32
}

fn apply_migration(
    value: serde_json::Value,
    from_version: u32,
) -> Result<serde_json::Value> {
    match from_version {
        // v0 → v1: initial persisted format.  No structural change; just stamp version.
        // (There was no on-disk "v0"; this branch handles missing or zero version fields.)
        0 => {
            let mut obj = match value {
                serde_json::Value::Object(m) => m,
                // Corrupt or missing root object: start fresh; serde will fill defaults.
                _ => serde_json::Map::new(),
            };
            obj.insert(
                "version".to_owned(),
                serde_json::Value::from(CURRENT_VERSION),
            );
            Ok(serde_json::Value::Object(obj))
        }
        // Unhandled version inside the migration range — should not happen if
        // apply_migration is only called for ver < CURRENT_VERSION and all versions
        // in [0, CURRENT_VERSION) have explicit arms above.
        _ => Ok(value),
    }
}
