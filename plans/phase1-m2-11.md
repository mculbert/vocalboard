# Phase 1 · M2 · Step 11 — Tauri wiring + contract + final pass (action plan)

Per-step action plan for Step 11 of the M2 milestone from [phase1-m2.md](phase1-m2.md) — the
**boundary + final gate**, following the M1 Step 12 pattern (`phase1-m1-12.md`). The authoritative
spec is [command-surface.md § Playback / Export commands](../design/command-surface.md#playback-commands) and
[§ Tauri command boundary — versioning (H1) / param validation (J2)](../design/command-surface.md#tauri-command-boundary--versioning-mechanism-h1).

This step exposes the Step 9–10 engine through the Tauri command surface: `proto` param/result
types + the `playhead_update` / `playback_stopped` event payloads, the `#[tauri::command]`
handlers, the **managed `PlaybackEngine` opened when a project opens**, the read-accessors on
`ProjectState` the handlers need, and regenerated TS bindings + `commands.ts` wrappers. **No new
UI** (consistent with M1). Then the full workspace gate.

**Definition of done:** the six commands (`play_from`, `pause`, `stop`, `export_track`,
`export_mixed`, `export_transcript`) and two events round-trip through Tauri with regenerated,
in-sync TS bindings; the managed `PlaybackEngine` opens its `cpal` stream **at project open** (not
app start) and is reused across play/stop cycles; `cargo run -p proto --features ts-export --bin
gen_bindings -- --check`, `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test
--workspace`, and `pnpm check && pnpm test && pnpm build` are all green.

## Decisions locked in this step

- **Proto types mirror the command-surface schemas exactly** ([command-surface.md](../design/command-surface.md)):
  - `PlayFromParams { start_sample: i64, end_sample: Option<i64> }` (default `null` = to end).
  - `PauseParams {}` / `StopParams {}` (no fields).
  - `ExportTrackParams { track_id: u32, output_path: String, format: AudioFormat (default flac),
    mono: bool (default false) }`; `ExportMixedParams` = same minus `track_id`.
  - `ExportTranscriptParams { output_path: String, format: TranscriptFormat (default vtt),
    include_cut_words: bool (default false) }`.
  - Event payloads `PlayheadUpdate { position_samples: i64 }`, `PlaybackStopped { position_samples:
    i64 }`.
- **Each param struct carries `#[serde(deny_unknown_fields)]`** and the `ts_rs::TS` derive under
  the `ts-export` feature (the existing `commands.rs` pattern); **value-constraint guards** (e.g.
  `start_sample >= 0`, valid `format` enum) validate at the handler boundary per
  [command-surface.md § J2](../design/command-surface.md#tauri-command-boundary--param-validation-j2), since
  serde alone won't enforce ranges. Format enums use `#[serde(rename_all = "snake_case")]` matching
  the schema `enum` values (`flac`/`wav`/`mp3`/`ogg`/`aac`, `vtt`/`markdown`).
- **The audio `format` param is advisory; the codec is chosen by the output-file extension.**
  [Revision 2](phase1-m2-revision2.md) replaced `export_track`/`export_mixed` with a single
  `export_audio(renderer, format, mono, out)` that **honors** the passed `format`; the handler
  resolves it via `audio_format_for(output_path)` — *extension wins*, the same contract as
  transcript. The wire `format`
  field stays in the schema (it carries future per-format options, e.g. `Mp3 { bitrate }`; never
  mutate the enum in place — add a field or bump v2). So the **value guard targets the extension**
  (`audio_format_for(output_path)` / `transcript_format_for(output_path)` → `export_unsupported_format`
  for an unknown extension), not a redundant check that `format` matches the path.
- **Versioning is by name** ([command-surface.md § H1](../design/command-surface.md#tauri-command-boundary--versioning-mechanism-h1)):
  the M2 commands are all v1; a future incompatible change registers a new command name, never an
  in-place schema change. Document each `// Version 1`.
- **The `PlaybackEngine` is constructed when a project opens, not at app start** ([phase1-m2.md](phase1-m2.md)
  "decisions"; [phase1-m2-09.md](phase1-m2-09.md)). Its `cpal` stream config and ring size both
  depend on the **negotiated device rate**, which derives from the project's *locked* sample rate —
  unknown until a project opens. So `PlaybackEngine::new(sample_rate, BackendKind::Cpal, quality)` is
  built inside the `new_project` / `open_project` handlers (alongside the `ProjectState`), held in a
  parallel managed slot, reused across all play/stop cycles, and dropped/replaced on project switch.
  The engine is a value, not a global, so a second project window (Phase 6) owns its own stream.
  - **Test seam:** the backend is injected. Production passes `BackendKind::Cpal`; headless command-
    handler unit tests construct the engine with `BackendKind::InMemory` (or `InMemoryAtRate`). The
    handlers operate on a `&PlaybackEngine` from the slot regardless of backend.
  - **Device-open is non-fatal.** A `PlaybackEngine::new` failure (no audio device) must **not** fail
    `open_project`/`new_project` — the project opens with an empty playback slot; `play_from`/`pause`/
    `stop` then return `audio_io_error` (or a no-op) until a device is available. Editing/export do
    not depend on the engine.
- **`ProjectState` must expose read-accessors for the audio handlers (new in this step).** Today
  `ProjectState` exposes only `sample_rate()` and `room_tone()`; the per-track timeline `trees`, the
  per-track `TrackMeta`/`SpeakerMeta` (private on `current: Arc<TimelineState>`), and the project
  directory (computed locally in `open_project` and dropped) are all unreachable. The export/playback
  handlers need them, so add:
  - `fn trees(&self) -> &BTreeMap<u32, Arc<ImplicitTimelineTree<Turn>>>` (or an iterator over
    `(track_id, &tree)`) — for the renderer cursors and the transcript walk.
  - `fn tracks(&self) -> &[TrackMeta]` and `fn speakers(&self) -> &[SpeakerMeta]` (or a
    `speaker_id → name` `BTreeMap` built once) — for `TrackSource` assembly and the transcript map.
  - **Store the project directory on `ProjectState`** (a `vbdata_dir()` / `project_dir()` getter) so
    the handler can resolve `<project>.vbdata/` for `CacheSourceProvider` — `open_project`/`new_project`
    already know `path`; keep its parent. (`cache::resampled_cache_path(vbdata, id)` already derives
    the per-track FLAC path from there.)
  These getters are read-only, hold no `Db`, and do not mutate; they continue the M2 "no SQLite on
  the audio path" invariant once the handler has cloned the `Arc`/`TrackMeta` it needs.
- **Building the renderer for a handler ([revision 2](phase1-m2-revision2.md) surface).** The EDL
  cursor and `Renderer` are `'static + Send` (the cursor owns its traversal via `Arc` clones;
  `TrackCursor::at` borrows the tree only during construction), so: build the `tracks` slice from
  `ProjectState`, then `EdlCursor::build(tracks, start, end)` → `Renderer::new(cursor, provider,
  max_fade_samples, project_rate)`. `EdlCursor::build` (in `edl.rs`) replaces Step 10's private
  `build_renderer`; the audio crate stays free of `ProjectState`. The `play_from` and `export_*`
  handlers assemble the renderer identically (playback == export, test X23).
  - **`max_fade_samples`** comes from the `splice_crossfade_ms` setting × project rate (the M5 fade
    bound; pass the same value playback and export use). **Length is carried by the cursor's `end`,
    not a renderer argument**: for full-length export the handler passes `end = Some(project_end)`,
    where `project_end = max(project_start_sample + original_length_samples)` over tracks (0 for an
    empty project) and the `EdlCursor` emits trailing silence to `end`. Resolve `max_fade_samples`
    and `project_end` in the handler from `Settings` + `TrackMeta`.
- **`play_from` takes a pre-built `Renderer`, not a `(start, end)` pair.** The landed signature is
  `play_from(start, renderer: Renderer<CacheSourceProvider>, emit_update, emit_stopped)`; the
  `[start, end)` window rides **inside the renderer's `EdlCursor`**. So the `play_from` handler: (1)
  builds `Vec<TrackSource>` from `TrackMeta` (`id`, `source_channels`, `wet_dry_ratio`,
  `original_length_samples`, pre-decoded `room_tone()`), constructs `CacheSourceProvider::new(vbdata_dir,
  tracks)`; (2) builds the cursor over `[start_sample, end_sample)` via `EdlCursor::build` + the
  renderer; (3) calls `engine.play_from(start_sample, renderer, emit_update, emit_stopped)`. The
  commands are **non-journaled** (the frontend resolves scope → `[start, end)`). `export_*` handlers
  build the same provider + renderer and call the single `export_audio`
  ([revision 2](phase1-m2-revision2.md)). The renderer no longer carries `natural_end`; the engine
  derives the stop position from `frames_played`.
- **Events: two emit closures, each capturing an `AppHandle`.** `play_from` takes `emit_update:
  Fn(PlayheadUpdate) + Send + 'static` and `emit_stopped: Fn(PlaybackStopped) + Send + Sync +
  'static`; both push onto the Tauri event bus via `app_handle.emit(...)` (net-new — the app emits
  nothing today). `AppHandle` is `Clone + Send + Sync`, matching the `emit_stopped` `Send + Sync`
  bound. `commands.ts` adds typed `listen` helpers for the two events alongside the wrappers.
- **The emit closures must not re-enter the engine** (load-bearing for the Step-9 deadlock-freedom
  argument). `emit_stopped` runs *inside* the pre-roll thread on the natural-stop path, so calling
  back into `pause`/`stop`/`play_from` from a closure would self-join the pre-roll thread
  (panic/hang). The closures may only `emit` (see the `playback` module doc, "Liveness /
  deadlock-freedom").
- **Errors map to the snake_case `error_key`s** the frontend surfaces through Paraglide
  ([conventions.md](../design/conventions.md) C3/D2). `AudioError::error_key()` already exists
  (`export_unsupported_format`, `audio_io_error`, …) and `proto::error` already carries
  `ExportUnsupportedFormat` — extend the M1 proto error mapping to route `AudioError`; **no new
  error codes** beyond the command-surface table.
- **No persisted-format change** in this step (no migration needed): the RoomTone blob (Step 4 /
  revision) was the only format addition and shipped its own fixture there. Room tone reaches the
  provider as the **pre-decoded `Arc<RoomTone>`** held in `ProjectState.room_tones` (loaded at open),
  not a file.

## Sub-steps

### 11a — proto types + events

- Add the six param structs + the two result/event payloads to `proto/src/commands.rs` (or a new
  `events.rs`), with `deny_unknown_fields`, `ts_rs::TS`, doc-comments, and `// Version 1`. Reuse /
  mirror the `audio::AudioFormat` / `TranscriptFormat` enums as wire enums. Extend the proto error
  mapping to route `AudioError` via its `error_key()`.

### 11b — `ProjectState` accessors + `PlaybackEngine` at-open lifecycle

- Add the read-accessors to `ProjectState` (`trees`, `tracks`, `speakers`/speaker-map, and a stored
  `project_dir`/`vbdata_dir`) — read-only, no `Db`, no mutation. Store the project directory on the
  struct (keep `path.parent()` in `new_project`/`open_project`).
- Add a parallel managed slot in `app/src/main.rs` (e.g. `PlaybackSlot(Arc<Mutex<Option<PlaybackEngine>>>)`).
  In the `new_project` / `open_project` handlers, after building `ProjectState`, construct
  `PlaybackEngine::new(sample_rate, BackendKind::Cpal, quality_from_settings)` and place it in the
  slot. **Device-open failure is non-fatal** — log + leave the slot empty; the project still opens.
- Use `edl.rs::EdlCursor::build` (the [revision-2](phase1-m2-revision2.md) cursor builder) in 11c's
  handlers; the handler builds the `tracks` slice from `ProjectState`, then wraps `Renderer::new`.

### 11c — Tauri handlers (playback + export)

- Register the six `#[tauri::command]` handlers in `app/src/main.rs` over the `ProjectSlot` +
  `PlaybackSlot` managed state.
  - `play_from`/`pause`/`stop`: build the `CacheSourceProvider` + renderer (11b builder) and drive
    the engine; pass the two `AppHandle`-capturing emit closures; map `AudioError` → `error_key`.
  - `export_track`/`export_mixed`: build provider + renderer (cursor `end = Some(project_end)` for
    full length), call `export_audio(renderer, audio_format_for(output_path)?, mono, output_path)`;
    resolve `max_fade_samples` + `project_end` from `Settings` + `TrackMeta`.
  - `export_transcript`: `transcript_format_for(output_path)` (returns `Option`; `None →
    export_unsupported_format`) → `project::transcript::format_transcript(trees, speakers, rate, fmt,
    include_cut_words)` → write the string to `output_path`. `trees` here is a slice of
    `(project_start_sample, &tree)` pairs (zip `tracks()`'s `project_start_sample` with `trees()`),
    so turns from tracks at different project offsets merge in true global order.
  - Value guards (J2): `start_sample >= 0`; unknown extension → `export_unsupported_format`.

### 11d — bindings + wrappers

- Regenerate TS bindings (`gen_bindings`) and add `commands.ts` wrappers + the two event `listen`
  helpers; `pnpm check`.

### 11e — Final pass / full gate

- `cargo run -p proto --features ts-export --bin gen_bindings -- --check`; `cargo fmt --check`;
  `cargo clippy --workspace -- -D warnings` (incl. `missing_docs`, `unwrap_used`); `cargo test
  --workspace`; `pnpm check && pnpm test && pnpm build`.
- Confirm [command-surface.md](../design/command-surface.md) and [audio-pipeline.md](../design/audio-pipeline.md) match
  the implemented surface; update in the same commit if anything was adjusted (CLAUDE.md doc-sync) —
  in particular the project-open (not app-start) engine lifecycle and the new `ProjectState`
  accessors. Re-check the **M2 exit criteria** in [phase1-m2.md](phase1-m2.md).
- One commit `1M2-11: Tauri wiring for playback/export + final pass` on `claude/1M2`, unsigned.

## Test cases (for the implementer)

Proto/boundary unit tests (in `proto` + `app`), accessor/lifecycle tests on `ProjectState`, and
handler integration tests using the Step-9 in-memory backend. Groups: P = proto serde, V =
validation, L = state/lifecycle, H = handlers, B = bindings, X = gate.

**P — proto serde / shape**

1. **Defaults deserialize.** `{"start_sample": 0}` → `PlayFromParams { start_sample: 0, end_sample:
   None }`; `{"track_id":1,"output_path":"x.flac"}` → `format == Flac`, `mono == false`; transcript
   default `Vtt`, `include_cut_words == false`.
2. **`deny_unknown_fields`.** An extra key (`{"start_sample":0,"bogus":1}`) → deserialize error
   (guards typo/contract drift).
3. **Format enum round-trips snake_case.** `"flac"/"wav"/"mp3"/"ogg"/"aac"` ↔ `AudioFormat`;
   `"vtt"/"markdown"` ↔ `TranscriptFormat`; an unknown string → error.
4. **`end_sample` null vs integer.** Both `null` and an integer parse to `None`/`Some(n)`.
5. **Event payloads serialize.** `PlayheadUpdate`/`PlaybackStopped` serialize to
   `{ "position_samples": n }`.

**V — value-constraint validation (J2)**

6. **Negative `start_sample` rejected** at the handler boundary with the right `error_key` (`>=0`
   guard), not a panic.
7. **`end_sample < start_sample`** is rejected / handled per the locked contract (document: reject
   with an error key, or treat as empty range — test the chosen behaviour).
8. **Unknown export extension** in `output_path` (`.xyz`) → `export_unsupported_format` from the
   handler (the codec is chosen by extension via `audio_format_for`; the `format` param is advisory).
9. **Unsupported audio format without ffmpeg** → `export_unsupported_format` (mirrors Step 10 F20).

**L — `ProjectState` accessors + engine lifecycle**

10. **Accessors expose read state.** `trees()` / `tracks()` / `speakers()` / `vbdata_dir()` return
    the open project's timeline trees, track metas, speaker map, and `<project>.vbdata` path; they
    hold no `Db` and do not mutate (a synthetic `ProjectState` round-trips its inputs).
11. **Engine built at project open with the injected backend.** A `new_project`/`open_project` over
    the in-memory backend leaves a live `PlaybackEngine` in the slot whose `project_rate()` ==
    the project's locked rate; a second open replaces it.
12. **Device-open failure is non-fatal.** A forced `PlaybackEngine::new` error leaves the project
    open with an empty playback slot (no panic, no `open_project` failure); a subsequent `play_from`
    returns `audio_io_error`.

**H — handlers (in-memory backend)**

13. **`play_from` drives the engine.** The handler over a synthetic project starts playback; the
    in-memory sink captures the expected frames; `playhead_update` events are emitted (end-to-end
    through the handler, not just the engine).
14. **`pause`/`stop` handlers.** `pause` retains position (no `playback_stopped`); `stop` emits
    `playback_stopped` with the last position.
15. **`export_track` handler writes a file.** Over a synthetic project + a real `SourceProvider`
    over a temp `.vbdata`, the handler writes a FLAC that decodes to the expected PCM; the renderer
    is built via `EdlCursor::build` (so it matches playback, X23).
16. **`export_transcript` handler.** A `.vtt` path writes the Step-10 pinned VTT string; a `.md`
    path writes Markdown; the speaker map comes from `ProjectState.speakers()`.
17. **Non-journaled.** `play_from`/`pause`/`stop` append **no** journal rows (assert the journal is
    unchanged) — they are not project mutations.

**B — bindings in sync**

18. **`gen_bindings -- --check` is clean.** The committed TS bindings match the regenerated output
    (the M1 CI gate; fails if a proto type changed without regenerating).
19. **`commands.ts` wrappers exist + typecheck.** `pnpm check` passes with the six new wrappers +
    two event listeners.

**X — full gate**

20. **`cargo test --workspace`** (unit + integration + fixtures) green.
21. **`cargo clippy --workspace -- -D warnings`** (incl. `missing_docs`, `unwrap_used`) clean.
22. **`cargo fmt --check`** clean.
23. **`pnpm check && pnpm test && pnpm build`** green.

## Out of scope for Step 11

- **Any playback/export UI** (transport bar, export dialogs) — M6 frontend; M2 ships the command
  surface + bindings only.
- **The `import_speech_track` orchestration** that drives decode/resample/room-tone/initial-EDL and
  populates `TrackMeta` — M4 (consumes Steps 2–4, 6). In M2 the new `ProjectState` accessors return
  whatever the (synthetic, in M2) state holds; production tracks are populated at M4 import.
- **The `cut_words`/`mute_words`/… editing commands** that call the Step 5–6 primitives — M5.
- **Bundling ffmpeg / per-platform encoders** — M7.
- **Device selection / second-window streams / hot-swap** — minimal default device here; Phase 6
  multi-window. (Default-device *sample-rate* adaptation is already in Step 9.)
