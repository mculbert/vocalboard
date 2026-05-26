# Phase 1 — Implementation Plan

A dependency-ordered build plan for the Phase 1 (Minimum Viable Product) scope
described across the [design documents](index.md).

## Guiding principles

1. **Build the persistence/timeline core first.** Every feature mutates it, and bugs
   there corrupt projects — prove it with synthetic data before layering audio/ML on
   top.
2. **Define the cross-process contracts up front.** The command surface and IPC
   envelope are the spine; fixing them early lets the Rust, Python, and frontend
   layers develop against stable interfaces.
3. **Drive a thin end-to-end vertical slice as early as possible.** Get
   import → bubbles → playback working through a minimal cut of each layer to de-risk
   integration before broadening any one layer.

**Critical path:** M0 → M1 → M2/M3 → M4 → M5.
**Parallelizable:** M3 alongside M2; M6 from M0 onward against mocked commands;
M7's CI/settings early, packaging/docs at the end.

---

## M0 — Scaffolding & contracts

Establishes the spine all three layers code against.

- Rust workspace: `app/`, `core/`, `proto/` crates; `tauri.conf.json`, capability
  files, CSP
- `proto/`: command param/result + sidecar event types from
  [command-surface.md](command-surface.md); the NDJSON envelope
- SvelteKit static scaffold (Tailwind v4, Bits UI, Paraglide); generated TS command
  wrappers + types from the JSON schemas
- Python sidecar skeleton: `pyproject.toml`, package, NDJSON dispatch-loop stub,
  "sidecar ready" signal
- App settings schema + `tauri-plugin-store` load/migrate (needed by ML later)
- CI skeleton: `cargo fmt/clippy/test`, `pytest`, `pnpm check/test/build`

## M1 — Core persistence & timeline engine *(critical path; test heavily)*

No audio, no ML — proven with synthetic turns.

- `db/`: SQLite 3-table schema + `PRAGMA user_version` migrations
- Content-addressed blob store: BLAKE3-128, Bincode, format-tag byte, deterministic
  serialization
- Implicit timeline tree: duration-weighted order-statistic AVL, immutable `Arc`
  nodes, structural sharing, temporal queries
- `Turn` / `Word` / `Splice` structs
- Journal: delta batches (type 0), snapshots (type 1), metadata (type −1);
  load/replay (adjacency list → O(n) tree build); background snapshot writer
- Undo/redo stack (delta inverses)
- Global metadata blob (`ProjectMeta` / `TrackMeta` / `SpeakerMeta`) + file resolution
- Commands: `new_project`, `open_project`, `save_snapshot_now`

## M2 — Audio engine

Depends on M1 (tree → EDL). Decode/resample subparts can start late in M1.

- Decoding: Symphonia → f32 PCM; ffmpeg subprocess fallback (defer exotic/video
  formats if needed)
- Resample-at-import → resampled cache (rubato → 24-bit FLAC) as a background task;
  regenerate-if-missing
- EDL builder: per-turn splice tiling, prefix-sum sweep, multi-track merge
- Playback engine: cpal stream, ring buffer, pre-roll thread, `playhead_update` events
- Room-tone detection + pre-applied loop crossfade
- Export pipeline (track / mixed; transcript VTT/Markdown)
- Commands: `play_from` / `pause` / `stop`, `export_*`

## M3 — Python sidecar & ML *(parallelizable with M2 once M0 types exist)*

- SidecarManager + in-memory `TaskQueue` / `TaskDispatcher`; model registry (lazy
  load, idle unload); cancellation
- Model manifest scan + per-role path resolution from settings
- WhisperX (preproc → quality gate → transcribe/align/diarize) → result format;
  speaker-embedding merge (settings threshold)
- MP-SENet enhancement; Gemma disfluency (tagged-text prompt + **diff-align parser**);
  YAMnet `classify_sounds`; `detect_gpu`

> **First vertical slice happens here.** With minimal M1 + M2 + M3, wire
> `import_speech_track` for a single track → build the tree from turns → render
> bubbles → play. This validates IPC, tree-from-turns, and playback together before
> going wide.

## M4 — Import pipeline integration

- `import_speech_track` orchestration: probe → transcribe (Python) → build tree →
  room tone (Rust) → non-speech detection (Rust) → resample cache (bg) → speaker
  merge → journal/snapshot → optional `classify_sounds`
- `align_tracks` (Rust FFT cross-correlation, drift correction, `aligned_groups`)

## M5 — Editing commands

Depends on M1 (tree/undo) + M2 (splices/EDL) + M4 (real transcripts to edit).

- Zero-crossing search + crossfade; splice subdivision on cut/mute
- `cut_words` / `uncut_words` / `mute_words` / `unmute_words` (range-based, overlap
  validation)
- `remove_disfluencies` / `remove_sounds` (cut with mute fallback for cross-track
  overlaps); `identify_disfluencies` application
- `rename_track` / `rename_speaker` / `merge_speakers` / `remove_track`

## M6 — Frontend *(scaffolds against mocked commands from M0; matures alongside M4/M5)*

- Welcome / new / open / missing-files flows; project shell, toolbar, menus
- Timeline: bubble virtualization, overlap columns, `Bubble` / `Word` components
- Cursor & selection model (navigation order), keyboard shortcuts, playback highlight
- Dialogs (ModelDownload, AlignTracks, EnhanceAudio, TrackInfo, RenameTrack,
  Settings); progress / task-queue UI
- Accessibility (aria, focus management, live regions), i18n catalog, rune stores +
  event subscriptions

## M7 — Packaging, docs, hardening

- Nuitka sidecar build per platform; Tauri bundle; `release.yml`; model download +
  SHA-256 verification
- Logging (tracing / structlog), diagnostics bundle, crash-report URL
- Auto-generated API docs (pydoc-markdown / typedoc-plugin-markdown / rustdoc-JSON
  converter) → Hugo
- Playwright E2E (pre-release); performance pass (multi-hour transcripts, playback
  latency)
