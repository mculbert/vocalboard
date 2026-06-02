//! Command parameter and result types for the M0 command surface.
//!
//! Only the commands needed for the M0 end-to-end smoke test are defined here.
//! Additional commands are added in later milestones.

use serde::{Deserialize, Serialize};

// ── ping ──────────────────────────────────────────────────────────────────────

/// Parameters for the `ping` command (no fields).
///
/// Used to verify the sidecar is responsive. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct PingParams {}

/// Result of the `ping` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct PingResult {
    /// Always `true`; confirms the sidecar round-trip succeeded.
    pub pong: bool,
}

// ── app_info ─────────────────────────────────────────────────────────────────

/// Parameters for the `app_info` command (no fields).
///
/// Returns the application version and sidecar status. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct AppInfoParams {}

/// Current lifecycle state of the Python sidecar process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SidecarStatus {
    /// Sidecar started and responded to the startup handshake within the timeout.
    Ready,
    /// Sidecar has not been spawned yet (pre-launch window).
    NotStarted,
    /// Sidecar process exited unexpectedly or failed to start.
    Error,
}

/// Result of the `app_info` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct AppInfoResult {
    /// Application version string (semver, from `Cargo.toml`).
    pub version: String,
    /// Current lifecycle state of the Python sidecar.
    pub sidecar_status: SidecarStatus,
}

// ── new_project ───────────────────────────────────────────────────────────────

/// Parameters for the `new_project` command. Version 1.
///
/// Creates a new empty project SQLite file at `path` and locks the sample rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct NewProjectParams {
    /// Absolute filesystem path for the `.vocalboard` project file.
    pub path: String,
    /// Project sample rate in Hz, locked at creation (e.g. 48000).
    ///
    /// Any integer rate is accepted; 48000 is the default in the UI.
    pub sample_rate: u32,
}

// ── open_project ──────────────────────────────────────────────────────────────

/// Parameters for the `open_project` command. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct OpenProjectParams {
    /// Absolute path to the `.vocalboard` file to open.
    pub path: String,
}

// ── save_snapshot_now ─────────────────────────────────────────────────────────

/// Parameters for the `save_snapshot_now` command (no fields). Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct SaveSnapshotNowParams {}

// ── project results ───────────────────────────────────────────────────────────

/// Result of the `new_project` command: echoes the locked sample rate as confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct NewProjectResult {
    /// Sample rate locked at project creation (Hz).
    pub sample_rate: u32,
}

/// Wire mirror of the engine's `RecoveryInfo` — details of a corrupt-journal rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct RecoveryReport {
    /// Row id of the journal row that failed to decode.
    pub failed_row: i64,
    /// Row id of the snapshot the project was rolled back to.
    pub snapshot_id: i64,
}

/// Result of the `open_project` command.
///
/// Non-fatal facts the frontend must act on: missing source files and, when
/// `recovery` is `Some`, a corrupt-journal rollback — the UI **must** warn the user
/// because post-snapshot edits were silently lost (M6 warning dialog).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct OpenProjectResult {
    /// Track ids whose source file could not be located (Missing-Files dialog is M6).
    pub missing_tracks: Vec<u32>,
    /// `Some` iff a corrupt-journal recovery rolled the project back to a snapshot.
    pub recovery: Option<RecoveryReport>,
}
