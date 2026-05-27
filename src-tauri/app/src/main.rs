// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(missing_docs)]

//! Vocalboard desktop application entry point.
//!
//! Builds the Tauri runtime, registers plugins and commands, and initialises
//! structured logging.  All project state and business logic live in the
//! `core` crate; IPC contract types live in `proto`.

use std::sync::Arc;

use anyhow::Context as _;
use tauri::Manager as _;

/// Holds the sidecar manager once it has started, or `None` if startup failed.
struct SidecarState(Option<Arc<vb_core::SidecarManager>>);

/// Returns application version and sidecar lifecycle status.
///
/// Used by the frontend as a smoke test on startup.
#[tauri::command]
async fn get_app_info(
    state: tauri::State<'_, SidecarState>,
) -> Result<proto::AppInfoResult, String> {
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
/// Returns an error string if the sidecar is unavailable.
#[tauri::command]
async fn ping_sidecar(state: tauri::State<'_, SidecarState>) -> Result<proto::PingResult, String> {
    let mgr = state
        .0
        .clone()
        .ok_or_else(|| "sidecar not available".to_string())?;
    mgr.ping().await.map_err(|e| e.to_string())
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

            let settings = init_settings(app)?;
            app.manage(settings);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_app_info, ping_sidecar])
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
