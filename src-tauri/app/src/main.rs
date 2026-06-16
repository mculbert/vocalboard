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
use tauri::{Emitter as _, Manager as _};

/// Holds the sidecar manager once it has started, or `None` if startup failed.
struct SidecarState(Option<Arc<vb_core::SidecarManager>>);

/// App-global slot holding the single open project (Phase 1: one window ⇒ one project).
///
/// `Option` is `None` when no project is open. Replaced (old project dropped) on each
/// `new_project` or `open_project` call. The `Arc` is cloned into `spawn_blocking`
/// closures so the guard is never held across an `.await`.
struct ProjectSlot(Arc<Mutex<Option<vb_core::project::engine::ProjectState>>>);

/// App-global slot holding the [`PlaybackEngine`] for the open project.
///
/// Parallel to [`ProjectSlot`]: the engine's `cpal` stream config and ring size both
/// derive from the project's *locked* sample rate, so the engine is constructed when a
/// project opens (not at app start) and reused across all play/stop cycles. `None` when
/// no project is open **or** when the audio device could not be opened (device-open
/// failure is non-fatal — the project still opens; see [`install_playback_engine`]).
struct PlaybackSlot(Arc<Mutex<Option<vb_core::audio::playback::PlaybackEngine>>>);

/// Build a [`PlaybackEngine`](vb_core::audio::playback::PlaybackEngine) via `build` and
/// place it in `slot`, replacing any prior engine for the project just closed.
///
/// **Device-open failure is non-fatal:** if `build` returns `Err` (e.g. no audio device
/// on a headless host), the error is logged and the slot is left empty — the caller's
/// `new_project` / `open_project` still succeeds. Editing and export do not depend on the
/// engine; `play_from` / `pause` / `stop` surface the absent engine to the user.
///
/// `build` is a seam: production passes `BackendKind::Cpal`; headless lifecycle tests
/// inject `BackendKind::InMemory` or a forced error.
fn install_playback_engine(
    slot: &Arc<Mutex<Option<vb_core::audio::playback::PlaybackEngine>>>,
    build: impl FnOnce() -> Result<vb_core::audio::playback::PlaybackEngine, vb_core::audio::AudioError>,
) -> Result<(), proto::CommandError> {
    let engine = match build() {
        Ok(engine) => Some(engine),
        Err(e) => {
            tracing::warn!("playback device unavailable; project opens without audio: {e}");
            None
        }
    };
    *slot
        .lock()
        .map_err(|_| err(proto::ErrorCode::InternalError, "playback slot poisoned"))? = engine;
    Ok(())
}

/// Maps a [`vb_core::audio::AudioError`] to a typed [`CommandError`](proto::CommandError) via its
/// `error_key()` (proto carries no `vb_core` dep, so the mapping is by key string).
fn audio_to_command_error(e: vb_core::audio::AudioError) -> proto::CommandError {
    proto::CommandError {
        code: proto::ErrorCode::from_audio_error_key(e.error_key()),
        message: e.to_string(),
    }
}

/// Project + settings data the audio handlers project out of the [`ProjectSlot`] under one lock.
///
/// The handlers clone exactly what they need out of [`ProjectState`] (track metas, room tones, the
/// speech trees zipped with their `project_start_sample`, the `.vbdata` dir) plus the two derived
/// render scalars (`max_fade_samples`, `project_end`), then drop the guard — keeping with the M2
/// "no SQLite/`Db` on the audio path" invariant. `Renderer` + `CacheSourceProvider` are `'static`,
/// so everything assembled from this view outlives the lock.
struct AudioRenderInputs {
    vbdata_dir: std::path::PathBuf,
    /// Per speech track: render projection + the pre-decoded room tone.
    track_sources: Vec<vb_core::audio::source_provider::TrackSource>,
    /// Per speech track: `(track_id, project_start_sample, tree)` for `EdlCursor::build`. The
    /// tree clone is one `Arc` refcount bump (structural sharing), so cloning it out of the
    /// locked state and dropping the guard keeps the audio path off SQLite.
    edl_tracks: Vec<(
        u32,
        i64,
        vb_core::project::tree::ImplicitTimelineTree<vb_core::project::turn::Turn>,
    )>,
    /// Fade bound passed to the renderer (splice crossfade ms × project rate).
    max_fade_samples: usize,
    /// Exclusive project end: `max(project_start_sample + original_length_samples)`, 0 if empty.
    project_end: i64,
    /// Project sample rate (Hz).
    sample_rate: u32,
}

/// Gather [`AudioRenderInputs`] from the open project for `track_filter`.
///
/// `track_filter` selects which speech tracks contribute (a single id for `export_track`, all for
/// `export_mixed` / `play_from`). Returns `NoProjectOpen` when the slot is empty. Locks the slot,
/// projects out the pieces, drops the guard — the actual assembly lives in the pure
/// [`assemble_render_inputs`] (testable without a populated `ProjectState`).
fn gather_render_inputs(
    slot: &Arc<Mutex<Option<vb_core::project::engine::ProjectState>>>,
    settings: &vb_core::settings::Settings,
    track_filter: impl Fn(u32) -> bool,
) -> Result<AudioRenderInputs, proto::CommandError> {
    let guard = slot
        .lock()
        .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))?;
    let ps = guard
        .as_ref()
        .ok_or_else(|| err(proto::ErrorCode::NoProjectOpen, "no project open"))?;

    Ok(assemble_render_inputs(
        ps.vbdata_dir(),
        ps.sample_rate(),
        settings.splice_crossfade_ms,
        ps.tracks(),
        ps.trees(),
        |id| ps.room_tone(id).cloned(),
        track_filter,
    ))
}

/// Pure assembly of [`AudioRenderInputs`] from a project's projected pieces.
///
/// Split from [`gather_render_inputs`] so it is testable without a populated `ProjectState`
/// (production tracks are populated at M4 import; in M2 only this projection is exercised). Only
/// `TrackTree::Speech` trees feed the renderer; track 0 (`Labels`) is skipped. `project_end` spans
/// **all** tracks (so a full-length export pads to the longest track even when filtered to one).
fn assemble_render_inputs(
    vbdata_dir: std::path::PathBuf,
    sample_rate: u32,
    splice_crossfade_ms: f64,
    tracks: &[vb_core::project::metadata::TrackMeta],
    trees: &vb_core::project::snapshot::PerTrackTrees,
    room_tone: impl Fn(u32) -> Option<std::sync::Arc<vb_core::audio::room_tone::RoomTone>>,
    track_filter: impl Fn(u32) -> bool,
) -> AudioRenderInputs {
    use vb_core::project::snapshot::TrackTree;

    let max_fade_samples =
        vb_core::audio::zero_crossing::frames_from_ms(splice_crossfade_ms, sample_rate);

    let mut track_sources = Vec::new();
    let mut edl_tracks = Vec::new();
    let mut project_end = 0i64;

    for meta in tracks {
        project_end = project_end.max(meta.project_start_sample + meta.original_length_samples);
        if !track_filter(meta.id) {
            continue;
        }
        let Some(TrackTree::Speech(tree)) = trees.get(&meta.id) else {
            continue;
        };
        track_sources.push(vb_core::audio::source_provider::TrackSource::new(
            meta.id,
            meta.source_channels,
            meta.wet_dry_ratio,
            meta.original_length_samples,
            room_tone(meta.id),
        ));
        edl_tracks.push((meta.id, meta.project_start_sample, tree.clone()));
    }

    AudioRenderInputs {
        vbdata_dir,
        track_sources,
        edl_tracks,
        max_fade_samples,
        project_end,
        sample_rate,
    }
}

/// Assemble a [`Renderer`](vb_core::audio::render::Renderer) over `[start, end)` from
/// [`AudioRenderInputs`]. Playback and export build it identically (so a range renders the same
/// either way); the caller chooses `end` (`None` → walk to content end; `Some(project_end)` →
/// full-length export with trailing silence).
fn build_renderer(
    inputs: AudioRenderInputs,
    start: i64,
    end: Option<i64>,
) -> vb_core::audio::render::Renderer<vb_core::audio::source_provider::CacheSourceProvider> {
    let edl_refs: Vec<(
        u32,
        i64,
        &vb_core::project::tree::ImplicitTimelineTree<vb_core::project::turn::Turn>,
    )> = inputs
        .edl_tracks
        .iter()
        .map(|(id, off, tree)| (*id, *off, tree))
        .collect();
    let cursor = vb_core::audio::edl::EdlCursor::build(&edl_refs, start, end);
    let provider = vb_core::audio::source_provider::CacheSourceProvider::new(
        inputs.vbdata_dir,
        inputs.track_sources,
    );
    vb_core::audio::render::Renderer::new(
        cursor,
        provider,
        inputs.max_fade_samples,
        inputs.sample_rate,
    )
}

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
    playback: tauri::State<'_, PlaybackSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<proto::NewProjectResult, proto::CommandError> {
    if params.sample_rate < 8000 {
        return Err(err(
            proto::ErrorCode::InvalidParams,
            "sample_rate must be >= 8000",
        ));
    }
    let slot = slot.0.clone();
    let playback = playback.0.clone();
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let ps = vb_core::project::engine::ProjectState::new_project(
            Path::new(&params.path),
            params.sample_rate,
            &settings,
        )
        .map_err(to_command_error)?;
        let sample_rate = ps.sample_rate();
        let quality = settings.resampling_quality;
        *slot
            .lock()
            .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))? = Some(ps);
        // Open the playback engine at the project's locked rate (device-open non-fatal).
        install_playback_engine(&playback, || {
            vb_core::audio::playback::PlaybackEngine::new(
                sample_rate,
                vb_core::audio::playback::BackendKind::Cpal,
                quality,
            )
        })?;
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
    playback: tauri::State<'_, PlaybackSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<proto::OpenProjectResult, proto::CommandError> {
    let slot = slot.0.clone();
    let playback = playback.0.clone();
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
        let sample_rate = ps.sample_rate();
        let quality = settings.resampling_quality;
        *slot
            .lock()
            .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))? = Some(ps);
        // Open the playback engine at the project's locked rate (device-open non-fatal).
        install_playback_engine(&playback, || {
            vb_core::audio::playback::PlaybackEngine::new(
                sample_rate,
                vb_core::audio::playback::BackendKind::Cpal,
                quality,
            )
        })?;
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

/// Starts playback over `[start_sample, end_sample)` of the open project. Version 1.
///
/// Non-journaled (not a project mutation). Builds the same `CacheSourceProvider` + `Renderer` that
/// export uses, then drives the [`PlaybackEngine`] in the slot with two
/// `AppHandle`-capturing emit closures (they only `emit` — never re-enter the engine, see the
/// `playback` module's deadlock-freedom note). Guards `start_sample >= 0` (J2). Returns
/// `audio_io_error` when no audio device is open (empty playback slot).
#[tauri::command]
async fn play_from(
    params: proto::PlayFromParams,
    app: tauri::AppHandle,
    slot: tauri::State<'_, ProjectSlot>,
    playback: tauri::State<'_, PlaybackSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<(), proto::CommandError> {
    let slot = slot.0.clone();
    let playback = playback.0.clone();
    let settings = settings.inner().clone();
    let app_update = app.clone();
    let app_stopped = app;
    let emit_update = move |p: vb_core::audio::playback::PlayheadUpdate| {
        let _ = app_update.emit(
            "playhead_update",
            proto::PlayheadUpdate {
                position_samples: p.position_samples,
            },
        );
    };
    let emit_stopped = move |p: vb_core::audio::playback::PlaybackStopped| {
        let _ = app_stopped.emit(
            "playback_stopped",
            proto::PlaybackStopped {
                position_samples: p.position_samples,
            },
        );
    };
    tauri::async_runtime::spawn_blocking(move || {
        drive_play_from(
            &slot,
            &playback,
            &settings,
            &params,
            emit_update,
            emit_stopped,
        )
    })
    .await
    .map_err(|e| {
        err(
            proto::ErrorCode::InternalError,
            format!("worker join error: {e}"),
        )
    })?
}

/// Body of [`play_from`] with the emit closures injected (testable without an `AppHandle`).
///
/// Guards `start_sample >= 0` (J2), builds the export-identical renderer, and drives the
/// engine in `playback`. Returns `audio_io_error` when the playback slot is empty (no device).
/// The closures must only `emit` — never re-enter the engine (deadlock-freedom).
fn drive_play_from<EU, ES>(
    slot: &Arc<Mutex<Option<vb_core::project::engine::ProjectState>>>,
    playback: &Arc<Mutex<Option<vb_core::audio::playback::PlaybackEngine>>>,
    settings: &vb_core::settings::Settings,
    params: &proto::PlayFromParams,
    emit_update: EU,
    emit_stopped: ES,
) -> Result<(), proto::CommandError>
where
    EU: Fn(vb_core::audio::playback::PlayheadUpdate) + Send + 'static,
    ES: Fn(vb_core::audio::playback::PlaybackStopped) + Send + Sync + 'static,
{
    if params.start_sample < 0 {
        return Err(err(
            proto::ErrorCode::InvalidParams,
            "start_sample must be >= 0",
        ));
    }
    let inputs = gather_render_inputs(slot, settings, |_| true)?;
    let renderer = build_renderer(inputs, params.start_sample, params.end_sample);

    let guard = playback
        .lock()
        .map_err(|_| err(proto::ErrorCode::InternalError, "playback slot poisoned"))?;
    let engine = guard.as_ref().ok_or_else(|| {
        err(
            proto::ErrorCode::AudioIoError,
            "no audio device available for playback",
        )
    })?;
    engine
        .play_from(params.start_sample, renderer, emit_update, emit_stopped)
        .map_err(audio_to_command_error)
}

/// Pauses playback, retaining the last position (no `playback_stopped` event). Version 1.
///
/// Non-journaled. Returns `audio_io_error` when no audio device is open.
#[tauri::command]
async fn pause(
    _params: proto::PauseParams,
    playback: tauri::State<'_, PlaybackSlot>,
) -> Result<(), proto::CommandError> {
    let guard = playback
        .0
        .lock()
        .map_err(|_| err(proto::ErrorCode::InternalError, "playback slot poisoned"))?;
    let engine = guard.as_ref().ok_or_else(|| {
        err(
            proto::ErrorCode::AudioIoError,
            "no audio device available for playback",
        )
    })?;
    engine.pause();
    Ok(())
}

/// Stops playback and emits `playback_stopped` with the last position. Version 1.
///
/// Non-journaled. Returns `audio_io_error` when no audio device is open.
#[tauri::command]
async fn stop(
    _params: proto::StopParams,
    playback: tauri::State<'_, PlaybackSlot>,
) -> Result<(), proto::CommandError> {
    let guard = playback
        .0
        .lock()
        .map_err(|_| err(proto::ErrorCode::InternalError, "playback slot poisoned"))?;
    let engine = guard.as_ref().ok_or_else(|| {
        err(
            proto::ErrorCode::AudioIoError,
            "no audio device available for playback",
        )
    })?;
    engine.stop();
    Ok(())
}

/// Renders and exports a single track to `output_path`. Version 1.
///
/// The codec is resolved from the `output_path` extension (`audio_format_for`, extension wins —
/// `format` is advisory); an unrecognised extension → `export_unsupported_format` (J2). The
/// renderer is built full-length (`end = Some(project_end)`) so trailing silence pads the export.
#[tauri::command]
async fn export_track(
    params: proto::ExportTrackParams,
    slot: tauri::State<'_, ProjectSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<(), proto::CommandError> {
    let slot = slot.0.clone();
    let settings = settings.inner().clone();
    let track_id = params.track_id;
    tauri::async_runtime::spawn_blocking(move || {
        export_audio_handler(
            &slot,
            &settings,
            &params.output_path,
            params.mono,
            move |id| id == track_id,
        )
    })
    .await
    .map_err(|e| {
        err(
            proto::ErrorCode::InternalError,
            format!("worker join error: {e}"),
        )
    })?
}

/// Renders and exports the mixed output of all tracks to `output_path`. Version 1.
///
/// Same contract as [`export_track`] minus `track_id` (all speech tracks contribute); codec by
/// extension (extension wins), full-length render with trailing silence.
#[tauri::command]
async fn export_mixed(
    params: proto::ExportMixedParams,
    slot: tauri::State<'_, ProjectSlot>,
    settings: tauri::State<'_, vb_core::settings::Settings>,
) -> Result<(), proto::CommandError> {
    let slot = slot.0.clone();
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        export_audio_handler(&slot, &settings, &params.output_path, params.mono, |_| true)
    })
    .await
    .map_err(|e| {
        err(
            proto::ErrorCode::InternalError,
            format!("worker join error: {e}"),
        )
    })?
}

/// Shared body of [`export_track`] / [`export_mixed`]: resolve the codec from `output_path`, build
/// the full-length renderer for `track_filter`, and write the encoded file.
fn export_audio_handler(
    slot: &Arc<Mutex<Option<vb_core::project::engine::ProjectState>>>,
    settings: &vb_core::settings::Settings,
    output_path: &str,
    mono: bool,
    track_filter: impl Fn(u32) -> bool,
) -> Result<(), proto::CommandError> {
    let out = Path::new(output_path);
    // Extension wins (J2): an unknown extension fails before any rendering.
    let format = vb_core::audio::export::audio_format_for(out).map_err(audio_to_command_error)?;
    let inputs = gather_render_inputs(slot, settings, track_filter)?;
    let project_end = inputs.project_end;
    let renderer = build_renderer(inputs, 0, Some(project_end));
    vb_core::audio::export::export_audio(renderer, format, mono, out)
        .map_err(audio_to_command_error)
}

/// Format the project transcript from its projected pieces (pure; testable without a populated
/// `ProjectState`). The `speaker_id → name` map keys on `SpeakerMeta::id` (`u32`) widened to the
/// `u64` the turn payloads carry; only `TrackTree::Speech` trees contribute, zipped with each
/// track's `project_start_sample` so turns from different offsets merge in true global order.
fn format_project_transcript(
    tracks: &[vb_core::project::metadata::TrackMeta],
    speakers: &[vb_core::project::metadata::SpeakerMeta],
    trees: &vb_core::project::snapshot::PerTrackTrees,
    sample_rate: u32,
    format: vb_core::project::transcript::TranscriptFormat,
    include_cut_words: bool,
) -> String {
    use vb_core::project::snapshot::TrackTree;

    let speaker_map: std::collections::BTreeMap<u64, String> = speakers
        .iter()
        .map(|s| (s.id as u64, s.name.clone()))
        .collect();
    let tree_pairs: Vec<(
        i64,
        &vb_core::project::tree::ImplicitTimelineTree<vb_core::project::turn::Turn>,
    )> = tracks
        .iter()
        .filter_map(|meta| match trees.get(&meta.id) {
            Some(TrackTree::Speech(tree)) => Some((meta.project_start_sample, tree)),
            _ => None,
        })
        .collect();

    vb_core::project::transcript::format_transcript(
        &tree_pairs,
        &speaker_map,
        sample_rate,
        format,
        include_cut_words,
    )
}

/// Writes the project transcript to `output_path` as VTT or Markdown. Version 1.
///
/// The format is resolved from the `output_path` extension (`transcript_format_for`, extension
/// wins — `format` is advisory); an unrecognised extension → `export_unsupported_format` (J2).
/// Turns from tracks at different project offsets merge in true global timeline order.
#[tauri::command]
async fn export_transcript(
    params: proto::ExportTranscriptParams,
    slot: tauri::State<'_, ProjectSlot>,
) -> Result<(), proto::CommandError> {
    let slot = slot.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let out = Path::new(&params.output_path);
        let format = vb_core::project::transcript::transcript_format_for(out).ok_or_else(|| {
            err(
                proto::ErrorCode::ExportUnsupportedFormat,
                "unsupported transcript file extension",
            )
        })?;

        let guard = slot
            .lock()
            .map_err(|_| err(proto::ErrorCode::InternalError, "project slot poisoned"))?;
        let ps = guard
            .as_ref()
            .ok_or_else(|| err(proto::ErrorCode::NoProjectOpen, "no project open"))?;

        let text = format_project_transcript(
            ps.tracks(),
            ps.speakers(),
            ps.trees(),
            ps.sample_rate(),
            format,
            params.include_cut_words,
        );
        std::fs::write(out, text)
            .map_err(|e| audio_to_command_error(vb_core::audio::AudioError::Io(e)))
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
            app.manage(PlaybackSlot(Arc::new(Mutex::new(None))));

            let settings = init_settings(app)?;
            app.manage(settings);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            ping_sidecar,
            new_project,
            open_project,
            save_snapshot_now,
            play_from,
            pause,
            stop,
            export_track,
            export_mixed,
            export_transcript
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use tempfile::{tempdir, TempDir};
    use vb_core::audio::export::{export_audio, AudioFormat};
    use vb_core::audio::playback::{BackendKind, PlaybackEngine, PlaybackStopped, PlayheadUpdate};
    use vb_core::audio::render::{Renderer, SourceProvider};
    use vb_core::audio::AudioError;
    use vb_core::project::engine::ProjectState;
    use vb_core::project::metadata::{ModelUse, SourceType, SpeakerMeta, TrackMeta};
    use vb_core::project::snapshot::{PerTrackTrees, TrackTree};
    use vb_core::project::transcript::TranscriptFormat;
    use vb_core::project::tree::ImplicitTimelineTree;
    use vb_core::project::turn::{encode_turn, Splice, SpliceKind, Turn, Word, WordType};
    use vb_core::settings::{ResamplingQuality, Settings};

    use super::{
        assemble_render_inputs, drive_play_from, export_audio_handler, format_project_transcript,
        install_playback_engine, PlaybackSlot,
    };

    // `err`/`ErrorCode` for asserting handler error codes.
    use proto::ErrorCode;

    const RATE: u32 = 48_000;

    /// A track metadata stub with the fields the audio handlers project (others filled minimally).
    fn track_meta(id: u32, project_start: i64, len: i64) -> TrackMeta {
        TrackMeta {
            id,
            name: format!("track {id}"),
            source_type: SourceType::File,
            source_path_relative: String::new(),
            source_path_absolute: String::new(),
            codec: "flac".to_string(),
            source_sample_rate: RATE,
            source_channels: 1,
            project_start_sample: project_start,
            original_length_samples: len,
            cut_length_samples: 0,
            drift_ppm: 0.0,
            room_tone_hash: None,
            models_used: ModelUse::default(),
            wet_dry_ratio: 0.0,
            disfluencies_identified: false,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// One-turn speech tree covering `[0, frames)` of the track's source (no edits).
    fn speech_tree(
        turn_id: u64,
        speaker_id: Option<u64>,
        frames: i64,
        words: &[&str],
    ) -> TrackTree {
        let turn = Turn {
            id: turn_id,
            speaker_id,
            turn_duration: frames,
            post_turn_silence: 0,
            words: words
                .iter()
                .map(|t| Word {
                    word_type: WordType::Normal,
                    text: t.to_string(),
                    start_sec: 0.0,
                    end_sec: 0.0,
                    is_cut: false,
                    is_muted: false,
                    source_onset_sample: None,
                    length_samples: 0,
                })
                .collect(),
            splices: vec![Splice {
                length_samples: frames,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            }],
        };
        let (h, _) = encode_turn(&turn).unwrap();
        TrackTree::Speech(
            ImplicitTimelineTree::new()
                .insert_at(0, h, Arc::new(turn))
                .unwrap(),
        )
    }

    /// A deterministic ramp signal, distinct per track.
    fn signal(frames: usize, seed: u32) -> Vec<f32> {
        (0..frames)
            .map(|i| (((i as u32).wrapping_mul(seed) % 997) as f32 / 997.0) * 0.5 - 0.25)
            .collect()
    }

    /// Minimal in-memory [`SourceProvider`] to bootstrap a `.vbdata/resampled/<id>.flac` cache via
    /// the public [`export_audio`] (no `pub(crate)` FLAC encoder is reachable from this crate).
    struct MemSource {
        data: Vec<f32>,
    }
    impl SourceProvider for MemSource {
        fn dry(&mut self, _id: u32, from: i64, n: i64) -> Result<Vec<f32>, AudioError> {
            let mut out = vec![0.0f32; n as usize];
            for (k, slot) in out.iter_mut().enumerate() {
                let idx = from + k as i64;
                if idx >= 0 && (idx as usize) < self.data.len() {
                    *slot = self.data[idx as usize];
                }
            }
            Ok(out)
        }
        fn enhanced(
            &mut self,
            _id: u32,
            _from: i64,
            _n: i64,
        ) -> Result<Option<Vec<f32>>, AudioError> {
            Ok(None)
        }
        fn room_tone(&mut self, _id: u32) -> Result<Option<&[f32]>, AudioError> {
            Ok(None)
        }
        fn channels(&self, _id: u32) -> u16 {
            1
        }
        fn wet_ratio(&self, _id: u32) -> f32 {
            0.0
        }
        fn source_len(&self, _id: u32) -> i64 {
            self.data.len() as i64
        }
    }

    /// Write `<vbdata>/resampled/<id>.flac` holding `data` (mono) by rendering it through
    /// [`export_audio`]; returns the cache file path.
    fn write_source_flac(vbdata: &Path, id: u32, data: &[f32]) {
        use vb_core::audio::edl::EdlCursor;
        let path = vb_core::audio::cache::resampled_cache_path(vbdata, id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tree = match speech_tree(id as u64, None, data.len() as i64, &[]) {
            TrackTree::Speech(t) => t,
            _ => unreachable!(),
        };
        let cursor = EdlCursor::build(&[(id, 0, &tree)], 0, Some(data.len() as i64));
        let renderer = Renderer::new(
            cursor,
            MemSource {
                data: data.to_vec(),
            },
            0,
            RATE,
        );
        export_audio(renderer, AudioFormat::Flac, true, &path).unwrap();
    }

    fn decode_mono(path: &Path) -> Vec<f32> {
        let mut src = vb_core::audio::decode::open_source(path).unwrap();
        let channels = src.channels() as usize;
        let mut interleaved = Vec::new();
        let mut buf = vec![0.0f32; channels * 4096];
        loop {
            let frames = src.read(&mut buf).unwrap();
            if frames == 0 {
                break;
            }
            interleaved.extend_from_slice(&buf[..frames * channels]);
        }
        if channels == 1 {
            interleaved
        } else {
            interleaved.chunks(channels).map(|c| c[0]).collect()
        }
    }

    /// A live in-memory playback engine in a fresh slot, at `RATE`.
    fn engine_slot() -> Arc<Mutex<Option<PlaybackEngine>>> {
        let slot = Arc::new(Mutex::new(None));
        install_playback_engine(&slot, || {
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced)
        })
        .unwrap();
        slot
    }

    /// A project slot opened at `RATE` (empty project — no tracks; M4 populates tracks).
    fn project_slot(dir: &TempDir) -> Arc<Mutex<Option<ProjectState>>> {
        let path = dir.path().join("p.vocalboard");
        let ps = ProjectState::new_project(&path, RATE, &Settings::default()).unwrap();
        Arc::new(Mutex::new(Some(ps)))
    }

    // The engine is built at project open with the injected backend, leaving a live
    // engine in the slot whose project_rate() == the project's locked rate; a second open
    // replaces it. The in-memory backend stands in for cpal on headless hosts.
    #[test]
    fn engine_built_at_open_with_injected_backend() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let settings = Settings::default();

        // Open a project at 48 kHz, then install an in-memory engine at its locked rate.
        let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        let rate = ps.sample_rate();
        let slot = PlaybackSlot(Arc::new(Mutex::new(None)));
        install_playback_engine(&slot.0, || {
            PlaybackEngine::new(rate, BackendKind::InMemory, ResamplingQuality::Balanced)
        })
        .unwrap();
        {
            let guard = slot.0.lock().unwrap();
            let engine = guard.as_ref().expect("engine installed at open");
            assert_eq!(
                engine.project_rate(),
                48000,
                "engine project_rate matches the project's locked rate"
            );
        }

        // A second open (a different project rate) replaces the engine in the slot.
        let path2 = dir.path().join("p2.vocalboard");
        let ps2 = ProjectState::new_project(&path2, 44100, &settings).unwrap();
        let rate2 = ps2.sample_rate();
        install_playback_engine(&slot.0, || {
            PlaybackEngine::new(rate2, BackendKind::InMemory, ResamplingQuality::Balanced)
        })
        .unwrap();
        let guard = slot.0.lock().unwrap();
        assert_eq!(
            guard.as_ref().expect("engine replaced").project_rate(),
            44100,
            "the second open replaces the engine with one at the new rate"
        );
    }

    // A forced PlaybackEngine::new failure (no audio device) is non-fatal: the
    // install returns Ok, leaves the slot empty, and the open project state is unaffected.
    // (The companion case — an empty slot making `play_from` return `audio_io_error` — is
    // covered by `play_from_empty_slot_audio_io_error` below.)
    #[test]
    fn device_open_failure_is_non_fatal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let settings = Settings::default();

        // The project opens regardless of audio-device availability.
        let project_slot = Arc::new(Mutex::new(None));
        let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        *project_slot.lock().unwrap() = Some(ps);

        // Forced engine-build failure: the install must not error and must leave the slot empty.
        let slot = PlaybackSlot(Arc::new(Mutex::new(None)));
        let outcome = install_playback_engine(&slot.0, || {
            Err(AudioError::DeviceError("no device".to_string()))
        });
        assert!(
            outcome.is_ok(),
            "device-open failure must not fail the open"
        );
        assert!(
            slot.0.lock().unwrap().is_none(),
            "a failed device open leaves the playback slot empty"
        );

        // The project state is still open and usable.
        assert!(
            project_slot.lock().unwrap().is_some(),
            "the project remains open despite the absent playback engine"
        );
    }

    // ── accessor projection ─────────────────────────────────────────────────────

    // assemble_render_inputs projects exactly the speech tracks selected by the
    // filter, skips track 0 (labels) and non-Speech trees, and carries the derived scalars
    // (max_fade_samples = crossfade_ms × rate; project_end = max over ALL tracks).
    #[test]
    fn assemble_render_inputs_projects_speech_tracks() {
        let mut trees: PerTrackTrees = BTreeMap::new();
        trees.insert(0, TrackTree::Labels(ImplicitTimelineTree::new())); // labels — skipped
        trees.insert(1, speech_tree(1, None, 1000, &[]));
        trees.insert(2, speech_tree(2, None, 2000, &[]));
        let tracks = vec![track_meta(1, 0, 1000), track_meta(2, 500, 2000)];

        // crossfade_ms × rate: at 2 ms and 48 kHz that is 96 frames.
        let inputs = assemble_render_inputs(
            std::path::PathBuf::from("/x.vbdata"),
            RATE,
            2.0,
            &tracks,
            &trees,
            |_| None,
            |_| true,
        );
        assert_eq!(inputs.sample_rate, RATE);
        assert_eq!(inputs.max_fade_samples, 96, "2 ms × 48 kHz = 96 frames");
        // project_end spans the longest track: track 2 ends at 500 + 2000 = 2500.
        assert_eq!(inputs.project_end, 2500);
        assert_eq!(inputs.track_sources.len(), 2);
        assert_eq!(inputs.edl_tracks.len(), 2);

        // Filtering to track 1 still computes project_end over ALL tracks (so a single-track
        // export pads to the longest track), but only track 1 contributes a source.
        let one = assemble_render_inputs(
            std::path::PathBuf::from("/x.vbdata"),
            RATE,
            2.0,
            &tracks,
            &trees,
            |_| None,
            |id| id == 1,
        );
        assert_eq!(one.project_end, 2500, "project_end spans all tracks");
        assert_eq!(one.track_sources.len(), 1, "only track 1 contributes");
        assert_eq!(one.edl_tracks[0].0, 1);
    }

    // ── value-constraint validation (J2) ───────────────────────────────────────

    // negative start_sample is rejected with invalid_params (the >= 0 guard), not a panic.
    #[test]
    fn negative_start_sample_rejected() {
        let dir = tempdir().unwrap();
        let proj = project_slot(&dir);
        let pb = engine_slot();
        let settings = Settings::default();
        let params = proto::PlayFromParams {
            start_sample: -1,
            end_sample: None,
        };
        let e = drive_play_from(
            &proj,
            &pb,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            |_: PlaybackStopped| {},
        )
        .expect_err("negative start must be rejected");
        assert_eq!(e.code, ErrorCode::InvalidParams);
    }

    // end_sample < start_sample is accepted as an empty range (the EdlCursor emits nothing
    // and the engine stops immediately): the locked contract treats it as a no-op, not an error.
    #[test]
    fn end_before_start_is_empty_range() {
        let dir = tempdir().unwrap();
        let proj = project_slot(&dir);
        let pb = engine_slot();
        let settings = Settings::default();
        let params = proto::PlayFromParams {
            start_sample: 1000,
            end_sample: Some(500),
        };
        // No tracks ⇒ no audio anyway; the point is the inverted window does not error.
        drive_play_from(
            &proj,
            &pb,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            |_: PlaybackStopped| {},
        )
        .expect("inverted window is an empty range, not an error");
        let guard = pb.lock().unwrap();
        if let Some(e) = guard.as_ref() {
            e.stop();
        }
    }

    // an unknown export extension (.xyz) → export_unsupported_format from the audio handler
    // (codec is chosen by extension; the format param is advisory). Checked before any rendering.
    #[test]
    fn unknown_export_extension_rejected() {
        let dir = tempdir().unwrap();
        let proj = project_slot(&dir);
        let settings = Settings::default();
        let out = dir.path().join("out.xyz");
        let e = export_audio_handler(&proj, &settings, out.to_str().unwrap(), false, |_| true)
            .expect_err("unknown extension must be rejected");
        assert_eq!(e.code, ErrorCode::ExportUnsupportedFormat);

        // Transcript handler: an unknown extension also → export_unsupported_format.
        let tout = dir.path().join("t.xyz");
        let format = vb_core::project::transcript::transcript_format_for(&tout);
        assert!(
            format.is_none(),
            "unknown transcript extension is unsupported"
        );
    }

    // an mp3 export without ffmpeg on PATH → export_unsupported_format (mirrors the
    // ffmpeg-unavailable case covered for the encoder in `vb_core::audio::ffmpeg`).
    // Skipped when ffmpeg IS available (the encode path is exercised in core).
    #[test]
    fn mp3_without_ffmpeg_unsupported() {
        if vb_core::audio::ffmpeg::ffmpeg_available() {
            return;
        }
        let dir = tempdir().unwrap();
        let vbdata = dir.path().join("p.vbdata");
        write_source_flac(&vbdata, 1, &signal(1000, 7));
        let proj = project_with_track(&dir, 1, 1000);
        let settings = Settings::default();
        let out = dir.path().join("out.mp3");
        let e = export_audio_handler(&proj, &settings, out.to_str().unwrap(), false, |_| true)
            .expect_err("mp3 without ffmpeg must be rejected");
        assert_eq!(e.code, ErrorCode::ExportUnsupportedFormat);
    }

    // ── empty playback slot surfaces audio_io_error ─────────────────────────────

    // with the playback slot empty (device-open failed), play_from returns
    // audio_io_error, the locked key for "no audio device" (command-surface error table).
    #[test]
    fn play_from_empty_slot_audio_io_error() {
        let dir = tempdir().unwrap();
        let proj = project_slot(&dir);
        let empty: Arc<Mutex<Option<PlaybackEngine>>> = Arc::new(Mutex::new(None));
        let settings = Settings::default();
        let params = proto::PlayFromParams {
            start_sample: 0,
            end_sample: None,
        };
        let e = drive_play_from(
            &proj,
            &empty,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            |_: PlaybackStopped| {},
        )
        .expect_err("empty playback slot must surface an error");
        assert_eq!(
            e.code,
            ErrorCode::AudioIoError,
            "no-device play_from returns audio_io_error"
        );
    }

    // ── handlers (in-memory backend) ────────────────────────────────────────────

    // play_from over a synthetic project drives the engine: a stop afterwards emits
    // playback_stopped exactly once (end-to-end through the handler body, not just the engine).
    #[test]
    fn play_from_drives_engine() {
        let dir = tempdir().unwrap();
        let vbdata = dir.path().join("p.vbdata");
        write_source_flac(&vbdata, 1, &signal(4000, 11));
        let proj = project_with_track(&dir, 1, 4000);
        let pb = engine_slot();
        let settings = Settings::default();
        let stopped: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&stopped);

        let params = proto::PlayFromParams {
            start_sample: 0,
            end_sample: Some(4000),
        };
        drive_play_from(
            &proj,
            &pb,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            move |p: PlaybackStopped| s.lock().unwrap().push(p.position_samples),
        )
        .expect("play_from drives the engine");

        // The user-issued stop joins the pre-roll thread and emits playback_stopped once.
        let engine = pb.lock().unwrap();
        engine.as_ref().unwrap().stop();
        drop(engine);
        assert_eq!(
            stopped.lock().unwrap().len(),
            1,
            "stop emits playback_stopped exactly once"
        );
    }

    // `pause` retains position without emitting `playback_stopped`; `stop` emits it once.
    // The engine's `pause` and `stop` each tear down the live session (pause is not a "hold" — it
    // joins the producer thread and retains position), so the two behaviours are exercised as two
    // independent play sessions over the same engine, matching the engine contract: `stop` after
    // `pause` would find no live session and be a no-op. Driven through the engine in the slot (the
    // handlers are thin wrappers over these calls).
    #[test]
    fn pause_then_stop_events() {
        let dir = tempdir().unwrap();
        let vbdata = dir.path().join("p.vbdata");
        write_source_flac(&vbdata, 1, &signal(4000, 13));
        let proj = project_with_track(&dir, 1, 4000);
        let pb = engine_slot();
        let settings = Settings::default();
        let stopped: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let params = proto::PlayFromParams {
            start_sample: 0,
            end_sample: Some(4000),
        };

        // Session 1: pause must NOT emit playback_stopped.
        let s1 = Arc::clone(&stopped);
        drive_play_from(
            &proj,
            &pb,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            move |p: PlaybackStopped| s1.lock().unwrap().push(p.position_samples),
        )
        .unwrap();
        {
            let guard = pb.lock().unwrap();
            guard.as_ref().unwrap().pause();
        }
        assert!(
            stopped.lock().unwrap().is_empty(),
            "pause does not emit playback_stopped"
        );

        // Session 2: stop emits playback_stopped exactly once.
        let s2 = Arc::clone(&stopped);
        drive_play_from(
            &proj,
            &pb,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            move |p: PlaybackStopped| s2.lock().unwrap().push(p.position_samples),
        )
        .unwrap();
        {
            let guard = pb.lock().unwrap();
            guard.as_ref().unwrap().stop();
        }
        assert_eq!(
            stopped.lock().unwrap().len(),
            1,
            "stop emits playback_stopped exactly once"
        );
    }

    // export_track writes a FLAC that decodes to the expected PCM; the renderer is built via
    // EdlCursor::build (the same path playback uses).
    #[test]
    fn export_track_writes_decodable_flac() {
        let dir = tempdir().unwrap();
        let vbdata = dir.path().join("p.vbdata");
        let src = signal(3000, 17);
        write_source_flac(&vbdata, 1, &src);
        let proj = project_with_track(&dir, 1, 3000);
        let settings = Settings::default();
        let out = dir.path().join("out.flac");

        export_audio_handler(&proj, &settings, out.to_str().unwrap(), true, |id| id == 1)
            .expect("export_track writes a FLAC");
        assert!(out.exists(), "export wrote the output file");

        let decoded = decode_mono(&out);
        // A 24-bit FLAC round-trip is near-exact; compare with a tight tolerance over the source.
        assert_eq!(decoded.len(), 3000, "exported length == project length");
        let max_err = src
            .iter()
            .zip(&decoded)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_err < 1e-3,
            "FLAC round-trip within tolerance: {max_err}"
        );
    }

    // export_transcript writes VTT for a .vtt path and Markdown for a .md path; the speaker
    // map comes from the project's speakers().
    #[test]
    fn export_transcript_vtt_and_markdown() {
        let mut trees: PerTrackTrees = BTreeMap::new();
        trees.insert(0, TrackTree::Labels(ImplicitTimelineTree::new()));
        trees.insert(1, speech_tree(1, Some(7), 48_000, &["hello", "world"]));
        let tracks = vec![track_meta(1, 0, 48_000)];
        let speakers = vec![SpeakerMeta {
            id: 7,
            name: "Ada".to_string(),
            color_hint: None,
            embedding_hash: None,
            track_ids: vec![1],
        }];

        let vtt = format_project_transcript(
            &tracks,
            &speakers,
            &trees,
            RATE,
            TranscriptFormat::Vtt,
            false,
        );
        assert!(
            vtt.starts_with("WEBVTT"),
            "VTT begins with the WEBVTT header"
        );
        assert!(vtt.contains("Ada"), "speaker name from speakers() appears");
        assert!(vtt.contains("hello world"), "turn text rendered");

        let md = format_project_transcript(
            &tracks,
            &speakers,
            &trees,
            RATE,
            TranscriptFormat::Markdown,
            false,
        );
        assert!(md.contains("**Ada"), "Markdown labels the speaker");
        assert!(!md.starts_with("WEBVTT"), "Markdown is not VTT");
    }

    // play_from/pause/stop append NO journal rows (non-journaled commands). Asserted by the
    // journal row count being unchanged across a play/pause/stop cycle.
    #[test]
    fn playback_is_non_journaled() {
        let dir = tempdir().unwrap();
        let vbdata = dir.path().join("p.vbdata");
        write_source_flac(&vbdata, 1, &signal(2000, 19));
        let proj = project_with_track(&dir, 1, 2000);
        let pb = engine_slot();
        let settings = Settings::default();

        let rows_before = journal_row_count(&proj);
        let params = proto::PlayFromParams {
            start_sample: 0,
            end_sample: Some(2000),
        };
        drive_play_from(
            &proj,
            &pb,
            &settings,
            &params,
            |_: PlayheadUpdate| {},
            |_: PlaybackStopped| {},
        )
        .unwrap();
        {
            let g = pb.lock().unwrap();
            let e = g.as_ref().unwrap();
            e.pause();
            e.stop();
        }
        assert_eq!(
            journal_row_count(&proj),
            rows_before,
            "play/pause/stop append no journal rows"
        );
    }

    // ── synthetic-project test support ─────────────────────────────────────────

    /// A project slot opened at `RATE` and synthetically populated with one speech track (`id`,
    /// `len` frames) over a `<dir>/p.vbdata` cache. Drives the M4-import gap: M2 has no public
    /// track-creation command, so the timeline state is built directly and injected.
    fn project_with_track(dir: &TempDir, id: u32, len: i64) -> Arc<Mutex<Option<ProjectState>>> {
        let path = dir.path().join("p.vocalboard");
        let mut ps = ProjectState::new_project(&path, RATE, &Settings::default()).unwrap();
        ps.test_inject_speech_track(
            track_meta(id, 0, len),
            speech_tree(id as u64, None, len, &[]),
        );
        Arc::new(Mutex::new(Some(ps)))
    }

    /// Count rows across the journal table to assert non-journaled commands add none.
    fn journal_row_count(slot: &Arc<Mutex<Option<ProjectState>>>) -> u64 {
        let guard = slot.lock().unwrap();
        guard.as_ref().unwrap().test_journal_row_count()
    }
}
