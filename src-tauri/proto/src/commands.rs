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

// ── audio / transcript wire enums ──────────────────────────────────────────────

/// Audio export format on the command wire.
///
/// Mirrors `vb_core::audio::AudioFormat`. Serialises snake_case, matching the
/// command-surface `format` enum (`flac`/`wav`/`mp3`/`ogg`/`aac`). **Advisory only** — the
/// handler resolves the codec from the output-file extension (`audio_format_for`, extension
/// wins); this field carries future per-format options (e.g. a bitrate). Never mutate a
/// variant in place; add a field or bump the command version. Version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// 24-bit FLAC (lossless); the default.
    Flac,
    /// f32le WAV (lossless).
    Wav,
    /// MP3 (lossy; requires `ffmpeg` on PATH).
    Mp3,
    /// Ogg Vorbis (lossy; requires `ffmpeg` on PATH).
    Ogg,
    /// AAC (lossy; requires `ffmpeg` on PATH).
    Aac,
}

/// Wire default for [`AudioFormat`]: `flac`.
fn default_audio_format() -> AudioFormat {
    AudioFormat::Flac
}

/// Transcript export format on the command wire.
///
/// Mirrors `vb_core::project::transcript::TranscriptFormat`. Serialises snake_case, matching the
/// command-surface `format` enum (`vtt`/`markdown`). **Advisory only** — the handler resolves
/// the format from the output-file extension (`transcript_format_for`, extension wins). Version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFormat {
    /// WebVTT (one cue per turn); the default.
    Vtt,
    /// Markdown (one speaker-labelled paragraph per turn).
    Markdown,
}

/// Wire default for [`TranscriptFormat`]: `vtt`.
fn default_transcript_format() -> TranscriptFormat {
    TranscriptFormat::Vtt
}

// ── play_from ──────────────────────────────────────────────────────────────────

/// Parameters for the `play_from` command. Version 1.
///
/// Starts playback over the explicit `[start_sample, end_sample)` project-timeline range; the
/// frontend resolves the user's intent (cursor→end, current turn, selection) into the window.
/// Not journaled (not a project mutation). The handler guards `start_sample >= 0` (J2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct PlayFromParams {
    /// Project-timeline start position, in integer samples (`>= 0`, guarded at the handler).
    pub start_sample: i64,
    /// Exclusive end position; `null` (the default) plays to the end of the timeline.
    #[serde(default)]
    pub end_sample: Option<i64>,
}

// ── pause / stop ────────────────────────────────────────────────────────────────

/// Parameters for the `pause` command (no fields). Retains playback position. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct PauseParams {}

/// Parameters for the `stop` command (no fields). Moves the cursor to the last position. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct StopParams {}

// ── export_track / export_mixed ────────────────────────────────────────────────

/// Parameters for the `export_track` command. Version 1.
///
/// Renders and exports a single track. The codec is chosen from the `output_path` extension
/// (`audio_format_for`, extension wins); `format` is advisory. An unrecognised extension →
/// `export_unsupported_format` at the handler (J2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ExportTrackParams {
    /// Id of the track to render and export.
    pub track_id: u32,
    /// Absolute output-file path; its extension selects the codec.
    pub output_path: String,
    /// Advisory format (defaults to `flac`); the extension wins at the handler.
    #[serde(default = "default_audio_format")]
    pub format: AudioFormat,
    /// Collapse the stereo render to mono (mean of L + R) before encoding. Defaults to `false`.
    #[serde(default)]
    pub mono: bool,
}

/// Parameters for the `export_mixed` command. Version 1.
///
/// Renders the mixed output of all non-muted tracks. Same as [`ExportTrackParams`] minus
/// `track_id`; the codec is chosen from the `output_path` extension (extension wins).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ExportMixedParams {
    /// Absolute output-file path; its extension selects the codec.
    pub output_path: String,
    /// Advisory format (defaults to `flac`); the extension wins at the handler.
    #[serde(default = "default_audio_format")]
    pub format: AudioFormat,
    /// Collapse the stereo render to mono (mean of L + R) before encoding. Defaults to `false`.
    #[serde(default)]
    pub mono: bool,
}

// ── export_transcript ──────────────────────────────────────────────────────────

/// Parameters for the `export_transcript` command. Version 1.
///
/// Writes the project transcript. The format is chosen from the `output_path` extension
/// (`transcript_format_for`, extension wins); `format` is advisory. An unrecognised extension →
/// `export_unsupported_format` at the handler (J2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ExportTranscriptParams {
    /// Absolute output-file path; its extension selects the format.
    pub output_path: String,
    /// Advisory format (defaults to `vtt`); the extension wins at the handler.
    #[serde(default = "default_transcript_format")]
    pub format: TranscriptFormat,
    /// Include words that were cut from the timeline. Defaults to `false`.
    #[serde(default)]
    pub include_cut_words: bool,
}
