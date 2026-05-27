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
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PingParams {}

/// Result of the `ping` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PingResult {
    /// Always `true`; confirms the sidecar round-trip succeeded.
    pub pong: bool,
}

// ── app_info ─────────────────────────────────────────────────────────────────

/// Parameters for the `app_info` command (no fields).
///
/// Returns the application version and sidecar status. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct AppInfoParams {}

/// Current lifecycle state of the Python sidecar process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct NewProjectParams {
    /// Absolute filesystem path for the `.vocalboard` project file.
    pub path: String,
    /// Project sample rate in Hz, locked at creation (e.g. 48000).
    ///
    /// Any integer rate is accepted; 48000 is the default in the UI.
    pub sample_rate: u32,
}
