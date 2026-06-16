//! IPC contract types shared between the Tauri app and the Python sidecar.
//!
//! All wire-format types are defined here. TypeScript equivalents are generated
//! by running `cargo run -p proto --features ts-export --bin gen_bindings` which
//! writes `src/lib/ipc/types.ts` in the frontend project.
#![warn(missing_docs)]

#[cfg(feature = "ts-export")]
pub mod bindings;
pub mod commands;
pub mod envelope;
pub mod error;
pub mod events;
pub mod sidecar;

pub use commands::{
    AppInfoParams, AppInfoResult, AudioFormat, ExportMixedParams, ExportTrackParams,
    ExportTranscriptParams, NewProjectParams, NewProjectResult, OpenProjectParams,
    OpenProjectResult, PauseParams, PingParams, PingResult, PlayFromParams, RecoveryReport,
    SaveSnapshotNowParams, SidecarStatus, StopParams, TranscriptFormat,
};
pub use envelope::{CancelEnvelope, RequestEnvelope, ToSidecar};
pub use error::{CommandError, ErrorCode};
pub use events::{PlaybackStopped, PlayheadUpdate};
pub use sidecar::{ErrorMsg, FromSidecar, LogLevel, LogMsg, ProgressMsg, ResultMsg};

#[cfg(test)]
mod tests {
    /// Round-trip serialisation smoke tests for the IPC envelope types.
    #[test]
    fn round_trip_request_envelope() -> Result<(), Box<dyn std::error::Error>> {
        use crate::envelope::{RequestEnvelope, ToSidecar};

        let msg = ToSidecar::Request(RequestEnvelope {
            request_id: "test-id".to_string(),
            command: "ping".to_string(),
            version: 1,
            payload: serde_json::json!({}),
        });

        let json = serde_json::to_string(&msg)?;
        let back: ToSidecar = serde_json::from_str(&json)?;

        let ToSidecar::Request(env) = back else {
            return Err("expected Request variant".into());
        };
        assert_eq!(env.command, "ping");
        assert_eq!(env.version, 1);
        Ok(())
    }

    #[test]
    fn round_trip_cancel_envelope() -> Result<(), Box<dyn std::error::Error>> {
        use crate::envelope::{CancelEnvelope, ToSidecar};

        let msg = ToSidecar::Cancel(CancelEnvelope {
            request_id: "cancel-me".to_string(),
        });

        let json = serde_json::to_string(&msg)?;
        assert!(json.contains(r#""type":"cancel""#));

        let back: ToSidecar = serde_json::from_str(&json)?;
        let ToSidecar::Cancel(env) = back else {
            return Err("expected Cancel variant".into());
        };
        assert_eq!(env.request_id, "cancel-me");
        Ok(())
    }

    #[test]
    fn round_trip_from_sidecar_messages() -> Result<(), Box<dyn std::error::Error>> {
        use crate::sidecar::{ErrorMsg, FromSidecar, LogLevel, LogMsg, ProgressMsg, ResultMsg};
        use crate::ErrorCode;

        let progress = FromSidecar::Progress(ProgressMsg {
            request_id: "req-1".to_string(),
            step: "transcribe".to_string(),
            step_index: 1,
            step_count: 4,
            pct: 25,
            label: "Transcribing…".to_string(),
        });
        let json = serde_json::to_string(&progress)?;
        assert!(json.contains(r#""type":"progress""#));
        let _: FromSidecar = serde_json::from_str(&json)?;

        let log = FromSidecar::Log(LogMsg {
            request_id: None,
            level: LogLevel::Info,
            msg: "sidecar ready".to_string(),
        });
        let json = serde_json::to_string(&log)?;
        assert!(json.contains(r#""type":"log""#));
        assert!(json.contains(r#""level":"info""#));
        let _: FromSidecar = serde_json::from_str(&json)?;

        let result = FromSidecar::Result(ResultMsg {
            request_id: "req-2".to_string(),
            payload: serde_json::json!({ "pong": true }),
        });
        let json = serde_json::to_string(&result)?;
        assert!(json.contains(r#""type":"result""#));
        let _: FromSidecar = serde_json::from_str(&json)?;

        let error = FromSidecar::Error(ErrorMsg {
            request_id: Some("req-3".to_string()),
            error: crate::error::CommandError {
                code: ErrorCode::Cancelled,
                message: "user cancelled".to_string(),
            },
        });
        let json = serde_json::to_string(&error)?;
        assert!(json.contains(r#""type":"error""#));
        assert!(json.contains(r#""code":"cancelled""#));
        let _: FromSidecar = serde_json::from_str(&json)?;

        Ok(())
    }

    #[test]
    fn command_error_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{CommandError, ErrorCode};

        let err = CommandError {
            code: ErrorCode::InternalError,
            message: "something went wrong".to_string(),
        };
        let json = serde_json::to_string(&err)?;
        assert!(json.contains(r#""code":"internal_error""#));
        assert!(json.contains(r#""message":"something went wrong""#));
        let back: CommandError = serde_json::from_str(&json)?;
        assert_eq!(back.code, ErrorCode::InternalError);
        assert_eq!(back.message, "something went wrong");
        Ok(())
    }

    #[test]
    fn open_project_result_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        use crate::commands::{OpenProjectResult, RecoveryReport};

        let with_recovery = OpenProjectResult {
            missing_tracks: vec![2, 5],
            recovery: Some(RecoveryReport {
                failed_row: 42,
                snapshot_id: 7,
            }),
        };
        let json = serde_json::to_string(&with_recovery)?;
        assert!(json.contains(r#""failed_row":42"#));
        assert!(json.contains(r#""snapshot_id":7"#));
        let back: OpenProjectResult = serde_json::from_str(&json)?;
        assert_eq!(back.missing_tracks, vec![2, 5]);
        assert!(back.recovery.is_some());

        let without_recovery = OpenProjectResult {
            missing_tracks: vec![],
            recovery: None,
        };
        let json2 = serde_json::to_string(&without_recovery)?;
        assert!(json2.contains(r#""recovery":null"#));
        let back2: OpenProjectResult = serde_json::from_str(&json2)?;
        assert!(back2.recovery.is_none());

        Ok(())
    }

    #[test]
    fn deny_unknown_fields_rejects_extra() -> Result<(), Box<dyn std::error::Error>> {
        use crate::commands::{NewProjectParams, OpenProjectParams};

        let bad_open = r#"{"path":"/tmp/x.vocalboard","extra":true}"#;
        assert!(serde_json::from_str::<OpenProjectParams>(bad_open).is_err());

        let bad_new = r#"{"path":"/tmp/x.vocalboard","sample_rate":48000,"extra":true}"#;
        assert!(serde_json::from_str::<NewProjectParams>(bad_new).is_err());

        Ok(())
    }

    // ── playback / export proto types ───────────────────────────────────────────

    /// omitted optional fields deserialise to their documented defaults.
    #[test]
    fn audio_params_defaults_deserialise() -> Result<(), Box<dyn std::error::Error>> {
        use crate::commands::{
            AudioFormat, ExportTrackParams, ExportTranscriptParams, PlayFromParams,
            TranscriptFormat,
        };

        let play: PlayFromParams = serde_json::from_str(r#"{"start_sample":0}"#)?;
        assert_eq!(play.start_sample, 0);
        assert_eq!(play.end_sample, None);

        let track: ExportTrackParams =
            serde_json::from_str(r#"{"track_id":1,"output_path":"x.flac"}"#)?;
        assert_eq!(track.track_id, 1);
        assert_eq!(track.format, AudioFormat::Flac);
        assert!(!track.mono);

        let transcript: ExportTranscriptParams =
            serde_json::from_str(r#"{"output_path":"x.vtt"}"#)?;
        assert_eq!(transcript.format, TranscriptFormat::Vtt);
        assert!(!transcript.include_cut_words);
        Ok(())
    }

    /// `deny_unknown_fields` rejects an extra key on every playback / export param struct.
    #[test]
    fn audio_params_deny_unknown_fields() {
        use crate::commands::{
            ExportMixedParams, ExportTrackParams, ExportTranscriptParams, PauseParams,
            PlayFromParams, StopParams,
        };

        assert!(serde_json::from_str::<PlayFromParams>(r#"{"start_sample":0,"bogus":1}"#).is_err());
        assert!(serde_json::from_str::<PauseParams>(r#"{"bogus":1}"#).is_err());
        assert!(serde_json::from_str::<StopParams>(r#"{"bogus":1}"#).is_err());
        assert!(serde_json::from_str::<ExportTrackParams>(
            r#"{"track_id":1,"output_path":"x.flac","bogus":1}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<ExportMixedParams>(r#"{"output_path":"x.flac","bogus":1}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<ExportTranscriptParams>(
            r#"{"output_path":"x.vtt","bogus":1}"#
        )
        .is_err());
    }

    /// format enums round-trip snake_case; unknown strings error.
    #[test]
    fn format_enums_round_trip_snake_case() -> Result<(), Box<dyn std::error::Error>> {
        use crate::commands::{AudioFormat, TranscriptFormat};

        for (s, f) in [
            ("flac", AudioFormat::Flac),
            ("wav", AudioFormat::Wav),
            ("mp3", AudioFormat::Mp3),
            ("ogg", AudioFormat::Ogg),
            ("aac", AudioFormat::Aac),
        ] {
            assert_eq!(serde_json::to_string(&f)?, format!("\"{s}\""));
            assert_eq!(serde_json::from_str::<AudioFormat>(&format!("\"{s}\""))?, f);
        }
        assert!(serde_json::from_str::<AudioFormat>(r#""flacc""#).is_err());

        for (s, f) in [
            ("vtt", TranscriptFormat::Vtt),
            ("markdown", TranscriptFormat::Markdown),
        ] {
            assert_eq!(serde_json::to_string(&f)?, format!("\"{s}\""));
            assert_eq!(
                serde_json::from_str::<TranscriptFormat>(&format!("\"{s}\""))?,
                f
            );
        }
        assert!(serde_json::from_str::<TranscriptFormat>(r#""md""#).is_err());
        Ok(())
    }

    /// `end_sample` accepts both `null` and an integer.
    #[test]
    fn play_from_end_sample_null_or_int() -> Result<(), Box<dyn std::error::Error>> {
        use crate::commands::PlayFromParams;

        let none: PlayFromParams =
            serde_json::from_str(r#"{"start_sample":10,"end_sample":null}"#)?;
        assert_eq!(none.end_sample, None);

        let some: PlayFromParams = serde_json::from_str(r#"{"start_sample":10,"end_sample":99}"#)?;
        assert_eq!(some.end_sample, Some(99));
        Ok(())
    }

    /// event payloads serialise to `{ "position_samples": n }`.
    #[test]
    fn event_payloads_serialise() -> Result<(), Box<dyn std::error::Error>> {
        use crate::events::{PlaybackStopped, PlayheadUpdate};

        let update = PlayheadUpdate {
            position_samples: 480,
        };
        assert_eq!(
            serde_json::to_string(&update)?,
            r#"{"position_samples":480}"#
        );
        let back: PlayheadUpdate = serde_json::from_str(r#"{"position_samples":7}"#)?;
        assert_eq!(back.position_samples, 7);

        let stopped = PlaybackStopped {
            position_samples: 96000,
        };
        assert_eq!(
            serde_json::to_string(&stopped)?,
            r#"{"position_samples":96000}"#
        );
        Ok(())
    }

    /// `AudioError::error_key()` strings route to command codes.
    #[test]
    fn audio_error_key_maps_to_code() {
        use crate::ErrorCode;

        // The two frontend-facing audio codes in the command-surface table.
        assert_eq!(
            ErrorCode::from_audio_error_key("export_unsupported_format"),
            ErrorCode::ExportUnsupportedFormat
        );
        assert_eq!(
            ErrorCode::from_audio_error_key("audio_io_error"),
            ErrorCode::AudioIoError
        );
        // Every other audio key folds to internal_error (no new codes beyond the table).
        for key in [
            "decode_unsupported_format",
            "decode_failed",
            "ffmpeg_unavailable",
            "ffmpeg_failed",
            "encode_failed",
            "audio_device_error",
            "something_unrecognised",
        ] {
            assert_eq!(
                ErrorCode::from_audio_error_key(key),
                ErrorCode::InternalError,
                "key {key} should fold to internal_error"
            );
        }
    }

    #[test]
    fn error_code_snake_case_serialisation() -> Result<(), Box<dyn std::error::Error>> {
        use crate::ErrorCode;

        assert_eq!(
            serde_json::to_string(&ErrorCode::OverlappingWordCannotCut)?,
            r#""overlapping_word_cannot_cut""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::SidecarNotReady)?,
            r#""sidecar_not_ready""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::ProjectFileExists)?,
            r#""project_file_exists""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::ProjectFileNotFound)?,
            r#""project_file_not_found""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::ProjectOpenFailed)?,
            r#""project_open_failed""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::NoProjectOpen)?,
            r#""no_project_open""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::InvalidParams)?,
            r#""invalid_params""#
        );
        Ok(())
    }
}
