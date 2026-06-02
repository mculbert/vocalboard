// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(missing_docs)]

//! Vocalboard desktop application entry point.
//!
//! Builds the Tauri runtime, registers plugins and commands, and initialises
//! structured logging.  All project state and business logic live in the
//! `core` crate; IPC contract types live in `proto`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tauri::Manager as _;

/// Holds the sidecar manager once it has started, or `None` if startup failed.
struct SidecarState(Option<Arc<vb_core::SidecarManager>>);

/// App-global slot holding the single open project (Phase 1: one window ⇒ one project).
///
/// `Option` is `None` when no project is open. Replaced (old project dropped) on each
/// `new_project` or `open_project` call. The `Arc` is cloned into `spawn_blocking`
/// closures so the guard is never held across an `.await`.
struct ProjectSlot(Arc<Mutex<Option<vb_core::project::engine::ProjectState>>>);

/// Maps an [`EngineError`](vb_core::project::engine::EngineError) to a typed
/// [`CommandError`](proto::CommandError) for the frontend.
fn to_command_error(e: vb_core::project::engine::EngineError) -> proto::CommandError {
    use vb_core::project::engine::EngineError;
    let code = match &e {
        EngineError::ProjectFileExists { .. } => proto::ErrorCode::ProjectFileExists,
        EngineError::ProjectFileNotFound { .. } => proto::ErrorCode::ProjectFileNotFound,
        EngineError::RecoveryFailed(_) | EngineError::OpenDb(_) => {
            proto::ErrorCode::ProjectOpenFailed
        }
        _ => proto::ErrorCode::InternalError,
    };
    proto::CommandError {
        code,
        message: e.to_string(),
    }
}

/// Constructs a [`CommandError`](proto::CommandError) from a code and message.
fn err(code: proto::ErrorCode, message: impl Into<String>) -> proto::CommandError {
    proto::CommandError {
        code,
        message: message.into(),
    }
}

/// Returns application version and sidecar lifecycle status.
///
/// Used by the frontend as a smoke test on startup.
#[tauri::command]
async fn get_app_info(
    state: tauri::State<'_, SidecarState>,
) -> Result<proto::AppInfoResult, proto::CommandError> {
    let mgr: Option<Arc<vb_core::SidecarManager>> = state.0.clone();
    let sidecar_status = match mgr {
        Some(m) => m.status().await,
        None => proto::SidecarStatus::Error,
    };
    Ok(proto::AppInfoResult {
        version: env!("CARGO_PKG_VERSION").to_string(),
        sidecar_status,
    })
}

/// Sends a `ping` to the Python sidecar and returns `{pong: true}`.
///
/// Returns `SidecarNotReady` if the sidecar is unavailable or does not respond.
#[tauri::command]
async fn ping_sidecar(
    state: tauri::State<'_, SidecarState>,
) -> Result<proto::PingResult, proto::CommandError> {
    let mgr = state.0.clone().ok_or_else(|| proto::CommandError {
        code: proto::ErrorCode::SidecarNotReady,
        message: "sidecar not available".to_string(),
    })?;
    mgr.ping().await.map_err(|e| proto::CommandError {
        code: proto::ErrorCode::SidecarNotReady,
        message: e.to_string(),
    })
}

/// Creates a new empty project at `params.path` locked to `params.sample_rate`.
///
/// Replaces any currently open project in the slot. Returns
/// `InvalidParams` if `sample_rate < 8000`, `ProjectFileExists` if a file
/// already exists at the path.
#[tauri::command]
async fn new_project(
    params: proto::NewProjectParams,
    slot: tauri::State<'_, ProjectSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<proto::NewProjectResult, proto::CommandError> {
    if params.sample_rate < 8000 {
        return Err(err(
            proto::ErrorCode::InvalidParams,
            "sample_rate must be >= 8000",
        ));
    }
    let slot = slot.0.clone();
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let ps = vb_core::project::engine::ProjectState::new_project(
            Path::new(&params.path),
            params.sample_rate,
            &settings,
        )
        .map_err(to_command_error)?;
        let sample_rate = ps.sample_rate();
        *slot
            .lock()
            .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))? = Some(ps);
        Ok(proto::NewProjectResult { sample_rate })
    })
    .await
    .map_err(|e| {
        err(
            proto::ErrorCode::InternalError,
            format!("worker join error: {e}"),
        )
    })?
}

/// Opens an existing project at `params.path`.
///
/// Replaces any currently open project. Returns `ProjectFileNotFound` if the path
/// does not exist, `ProjectOpenFailed` for unrecoverable open errors. The result
/// carries missing-track ids and, when `recovery` is `Some`, a corrupt-journal
/// rollback warning — the frontend **must** surface this to the user (M6 dialog).
#[tauri::command]
async fn open_project(
    params: proto::OpenProjectParams,
    slot: tauri::State<'_, ProjectSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<proto::OpenProjectResult, proto::CommandError> {
    let slot = slot.0.clone();
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (ps, outcome) = vb_core::project::engine::ProjectState::open_project(
            Path::new(&params.path),
            &settings,
        )
        .map_err(to_command_error)?;
        let result = proto::OpenProjectResult {
            missing_tracks: outcome.missing_tracks,
            recovery: outcome.recovery.map(|r| proto::RecoveryReport {
                failed_row: r.failed_row,
                snapshot_id: r.snapshot_id,
            }),
        };
        *slot
            .lock()
            .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))? = Some(ps);
        Ok(result)
    })
    .await
    .map_err(|e| {
        err(
            proto::ErrorCode::InternalError,
            format!("worker join error: {e}"),
        )
    })?
}

/// Writes the current project state as a new snapshot immediately.
///
/// Returns `NoProjectOpen` if no project is currently loaded.
#[tauri::command]
async fn save_snapshot_now(
    _params: proto::SaveSnapshotNowParams,
    slot: tauri::State<'_, ProjectSlot>,
) -> Result<(), proto::CommandError> {
    let slot = slot.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut guard = slot
            .lock()
            .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))?;
        let ps = guard
            .as_mut()
            .ok_or_else(|| err(proto::ErrorCode::NoProjectOpen, "no project open"))?;
        ps.save_snapshot_now().map_err(to_command_error)
    })
    .await
    .map_err(|e| {
        err(
            proto::ErrorCode::InternalError,
            format!("worker join error: {e}"),
        )
    })?
}

/// Load app settings from `tauri-plugin-store`, apply any pending migrations,
/// and write Phase-1 defaults for keys not yet present in the store (first launch).
///
/// Unknown keys already in the store (written by a newer app version) are preserved:
/// we set only the keys we know about, never deleting anything.
fn init_settings(
    app: &tauri::App,
) -> Result<vb_core::settings::Settings, Box<dyn std::error::Error>> {
    use tauri_plugin_store::StoreExt as _;

    let store = app.store("settings.json").context("open settings store")?;

    // Collect all store entries into a JSON object and run migration + parse.
    let raw = serde_json::Value::Object(store.entries().into_iter().collect());
    let settings = vb_core::settings::Settings::from_json(&raw).unwrap_or_else(|e| {
        tracing::warn!("settings load/migration failed, using defaults: {e}");
        vb_core::settings::Settings::default()
    });

    // Write defaults for keys not yet present in the store (first launch or new key).
    let json = settings
        .to_json()
        .context("serialize settings for default write")?;
    if let serde_json::Value::Object(map) = json {
        for (key, value) in map {
            if !store.has(&key) {
                store.set(key, value);
            }
        }
    }
    store.save().context("persist settings to disk")?;

    Ok(settings)
}

fn main() -> tauri::Result<()> {
    let _tracing_guard = init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Resolve the Python interpreter: $VOCALBOARD_PYTHON or fall back to "python".
            let python_bin =
                std::env::var("VOCALBOARD_PYTHON").unwrap_or_else(|_| "python".to_owned());

            // Spawn the sidecar startup on Tauri's async runtime, then block the
            // main thread (rx.recv) until it resolves — the window won't open until
            // this returns.  The sidecar typically starts in ~150 ms so users won't
            // notice, but a later milestone should open the window immediately and
            // surface a loading state while the sidecar warms up.
            let (tx, rx) = std::sync::mpsc::channel();
            let bin = python_bin.clone();
            tauri::async_runtime::spawn(async move {
                let result =
                    vb_core::SidecarManager::start(&bin, &["-m", "vocalboard_sidecar"]).await;
                let _ = tx.send(result);
            });

            let sidecar = match rx.recv() {
                Ok(Ok(mgr)) => {
                    tracing::info!("sidecar started successfully");
                    Some(mgr)
                }
                Ok(Err(e)) => {
                    tracing::error!("sidecar failed to start: {e}");
                    None
                }
                Err(e) => {
                    tracing::error!("sidecar init channel error: {e}");
                    None
                }
            };

            app.manage(SidecarState(sidecar));
            app.manage(ProjectSlot(Arc::new(Mutex::new(None))));

            let settings = init_settings(app)?;
            app.manage(settings);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            ping_sidecar,
            new_project,
            open_project,
            save_snapshot_now
        ])
        .run(tauri::generate_context!())
}

/// Initialises a non-blocking stdout subscriber with an `RUST_LOG`-aware
/// filter (defaults to `info`).
///
/// The returned [`WorkerGuard`] must live for the duration of the process;
/// dropping it flushes and shuts down the background logging thread.
/// File-based logging (via `tracing-appender` rolling appender) will be
/// added in a later milestone once the Tauri app data directory is available.
///
/// [`WorkerGuard`]: tracing_appender::non_blocking::WorkerGuard
fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stdout());

    // Ignore error: the only failure case is a second call (not possible here).
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_writer(non_blocking))
        .with(filter)
        .try_init();

    guard
}
