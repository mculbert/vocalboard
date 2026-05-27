//! NDJSON envelope types for messages Rust sends to the Python sidecar (stdin).

use serde::{Deserialize, Serialize};

/// A message sent from Rust to the Python sidecar over stdin.
///
/// Serializes as a flat JSON object tagged by `"type"`:
/// - `{"type":"request","request_id":"…","command":"…","version":1,"payload":{…}}`
/// - `{"type":"cancel","request_id":"…"}`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToSidecar {
    /// A command request dispatched to the sidecar.
    Request(RequestEnvelope),
    /// A cancellation signal for an in-flight request.
    Cancel(CancelEnvelope),
}

/// Command request envelope sent over stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RequestEnvelope {
    /// UUIDv4 identifying this request; echoed in every response.
    pub request_id: String,
    /// Snake-case command name (e.g. `"transcribe_track"`).
    pub command: String,
    /// Command schema version; increment on breaking param changes.
    pub version: u32,
    /// Command-specific parameters as a JSON object.
    #[cfg_attr(test, ts(type = "unknown"))]
    pub payload: serde_json::Value,
}

/// Cancellation envelope sent over stdin when the user cancels a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CancelEnvelope {
    /// UUIDv4 of the request to cancel.
    pub request_id: String,
}
