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
pub mod sidecar;

pub use commands::{
    AppInfoParams, AppInfoResult, NewProjectParams, NewProjectResult, OpenProjectParams,
    OpenProjectResult, PingParams, PingResult, RecoveryReport, SaveSnapshotNowParams,
    SidecarStatus,
};
pub use envelope::{CancelEnvelope, RequestEnvelope, ToSidecar};
pub use error::{CommandError, ErrorCode};
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
