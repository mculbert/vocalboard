//! Tauri event payloads emitted by the playback engine to the frontend.
//!
//! Unlike command params/results these travel on the Tauri **event bus** (not the request/response
//! path): the `play_from` handler captures an `AppHandle` and emits these as playback advances and
//! when it stops. The frontend subscribes with typed `listen` helpers (`commands.ts`). Both payloads
//! carry a single integer-sample project-timeline position (all time is integer samples).

use serde::{Deserialize, Serialize};

/// Payload of the `playhead_update` event. Version 1.
///
/// Emitted periodically while playback advances so the UI can move the playhead. `position_samples`
/// is the current project-timeline position in integer samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct PlayheadUpdate {
    /// Current playhead position on the project timeline, in integer samples.
    pub position_samples: i64,
}

/// Payload of the `playback_stopped` event. Version 1.
///
/// Emitted once when playback stops — whether by reaching the natural end of the range or by an
/// explicit `stop`. `position_samples` is the final project-timeline position in integer samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct PlaybackStopped {
    /// Final playhead position on the project timeline, in integer samples.
    pub position_samples: i64,
}
