//! Message types the Python sidecar emits to Rust over stdout.

use serde::{Deserialize, Serialize};

use crate::error::CommandError;

/// A message received from the Python sidecar over stdout.
///
/// Every line of stdout is one NDJSON object discriminated by `"type"`:
/// ```json
/// {"type":"ready"}
/// {"type":"progress","request_id":"…","step":"transcribe","step_index":1,"step_count":4,"pct":42,"label":"Transcribing…"}
/// {"type":"log","request_id":null,"level":"info","msg":"…"}
/// {"type":"result","request_id":"…","payload":{…}}
/// {"type":"error","request_id":"…","code":"cancelled","message":"…"}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromSidecar {
    /// Typed startup signal emitted once the sidecar is ready to accept requests.
    Ready,
    /// Progress update for an in-flight request.
    Progress(ProgressMsg),
    /// Structured log line (may be process-level, with `request_id = null`).
    Log(LogMsg),
    /// Successful completion of a request.
    Result(ResultMsg),
    /// Error response for a request or a process-level fault.
    Error(ErrorMsg),
}

/// A progress update from the Python sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct ProgressMsg {
    /// UUIDv4 of the originating request.
    pub request_id: String,
    /// Name of the current pipeline step (e.g. `"transcribe"`).
    pub step: String,
    /// Zero-based index of the current step within the pipeline.
    pub step_index: u32,
    /// Total number of steps in the pipeline.
    pub step_count: u32,
    /// Completion percentage for the current step (0–100).
    pub pct: u8,
    /// Human-readable label for the current step, suitable for display in the UI.
    pub label: String,
}

/// Severity level for a sidecar log message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Verbose diagnostic information.
    Debug,
    /// Normal operational information.
    Info,
    /// Unexpected condition that does not prevent progress.
    Warn,
    /// Condition that caused a request to fail.
    Error,
}

/// A structured log line emitted by the Python sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct LogMsg {
    /// UUIDv4 of the originating request, or `null` for process-level messages.
    pub request_id: Option<String>,
    /// Severity level.
    pub level: LogLevel,
    /// Log message text.
    pub msg: String,
}

/// A successful result from the Python sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct ResultMsg {
    /// UUIDv4 of the originating request.
    pub request_id: String,
    /// Command-specific result payload.
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub payload: serde_json::Value,
}

/// An error response from the Python sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct ErrorMsg {
    /// UUIDv4 of the originating request, or `null` for process-level errors.
    pub request_id: Option<String>,
    /// Error code and human-readable message (flattened to `{code, message}` on the wire).
    #[serde(flatten)]
    pub error: CommandError,
}
