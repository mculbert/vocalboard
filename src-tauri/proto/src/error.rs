//! Error codes for all commands and sidecar responses.

use serde::{Deserialize, Serialize};

/// Machine-readable error codes returned by Tauri commands and the Python sidecar.
///
/// Every error response carries one of these codes so the frontend can react
/// programmatically without parsing human-readable strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A word overlaps another track's turn and cannot be cut individually.
    OverlappingWordCannotCut,
    /// A track in the align set has cuts applied; alignment requires uncut tracks.
    TrackHasCutsCannotAlign,
    /// The last remaining track cannot be removed.
    LastTrackCannotRemove,
    /// Track name may not be empty.
    TrackNameEmpty,
    /// A track with that name already exists in the project.
    TrackNameDuplicate,
    /// Speaker name may not be empty.
    SpeakerNameEmpty,
    /// A speaker with that name already exists in the project.
    SpeakerNameDuplicate,
    /// Required ML model is not downloaded or configured.
    ModelNotAvailable,
    /// Transcription rejected because the average log-probability was below threshold.
    LowConfidenceTranscript,
    /// Source audio file could not be located at the stored path.
    FileNotFound,
    /// The specified export file extension is not supported.
    ExportUnsupportedFormat,
    /// Task was cancelled by the user.
    Cancelled,
    /// Python sidecar did not respond to the ready handshake within the startup timeout.
    SidecarNotReady,
    /// Command name not recognized by the sidecar, or unsupported message type.
    UnknownCommand,
    /// Unhandled error inside a sidecar handler.
    InternalError,
    /// An error code emitted by a newer component than this build understands.
    #[serde(other)]
    Unknown,
}
