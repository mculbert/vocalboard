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
  typed `Ready` startup signal
- App settings schema + `tauri-plugin-store` load/migrate (needed by ML later)
- Docs skeleton: Hugo site + auto-gen wiring (pydoc-markdown / typedoc / rustdoc stub);
  placeholder content tree; `pnpm docs:build` works now; hand-authored content in M7
- CI skeleton: `cargo fmt/clippy/test`, `pytest`, `pnpm check/test/build`

### M0 retro (complete)

Delivered green; two review rounds remediated (`phase1-m0-review.md`, plus a second
pass R1–R3). Notes to carry forward:

- **Reason through both orderings when touching concurrency.** Review round 1's
  `pending`-map fix (insert after write) silently created a response-drop race that
  round 2 had to fix — it traded a benign leak for a timing bug. The "obvious" fix was
  the wrong one.
- **`send()` has no concurrent-request test.** M0 only issues one in-flight `ping`; the
  race is fixed but untested under overlap. Add a multi-request test in M1 when the
  first real command lands on that path.
- **CSP is an M2 tripwire.** `default-src 'self'` with no `style-src` will block the
  inline styles popover/menu libs emit — surfaces only when overlay UI arrives (M6
  dialogs / M2-era components). Revisit `style-src` against
  [architecture.md](architecture.md) then.
- **Cross-OS sidecar coverage gap.** The Rust↔Python integration test runs Linux-only;
  Windows/macOS rely on `pytest` for the contract. Won't catch venv/path issues until
  the app actually runs on those platforms.
- **Settings round-trip is half-tested.** The fixture proves unknown keys don't *block*
  load; the *preservation* path lives in `init_settings` (untested). Give it a real
  through-the-store test once settings hold user data.
- **Doc-sync discipline earned its keep** — tracked deferrals (this retro, M3 startup,
  M6 D2) made the review tractable. Keep updating downstream milestones as shortcuts land.

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
- **Deferred from the M1 Step 11a dead-code sweep** (allows kept, owed to a later milestone):
  `History::{record (Step 11b), can_undo/can_redo (M4/M5 undo/redo command)}`, the journal-row
  `command_id`/`applied_at` fields (Step 12+ history view), `FileResolution` path fields (M6
  Missing-Files dialog), and `AdjacencyList`'s test/diagnostics query API (Step 11b). Full list:
  [phase1-m1-11.md § Dead-code cleanup](phase1-m1-11.md#remaining-allowdead_code-after-the-11a-sweep-deliberate-gaps).

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
  load, idle unload); cancellation; **replace the M0 blocking startup** (window
  currently blocks on `rx.recv()` until sidecar is ready — M3 should open the window
  immediately and surface a loading state via `get_app_info` / `SidecarStatus`)
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
- **Generate command schemas from Rust types (`schemars`):** use `schemars` to derive
  Draft-07 JSON schemas from Rust command param/result types, replacing the hand-authored
  prose schemas in `command-surface.md` with a single source of truth. Expose the schemas
  for Phase-6 plugin introspection. Deferred from M1 Step 12 — M1 keeps prose schemas
  authoritative and typed Rust structs as the runtime contract.

## M5 — Editing commands

Depends on M1 (tree/undo) + M2 (splices/EDL) + M4 (real transcripts to edit).

- Zero-crossing search + crossfade; splice subdivision on cut/mute
- `cut_words` / `uncut_words` / `mute_words` / `unmute_words` (range-based, overlap
  validation)
- `remove_disfluencies` / `remove_sounds` (cut with mute fallback for cross-track
  overlaps); `identify_disfluencies` application
- `rename_track` / `rename_speaker` / `merge_speakers` / `remove_track`
- **Track reconciliation guard (trees ↔ metadata):** `remove_track` removes the track's
  `TrackMeta` but does **not** emit deltas purging its turns — the orphaned transcript tree is
  harmless garbage discarded at the next open. Implement the load-time reconciliation specified in
  [data-model.md § Track reconciliation](data-model.md#track-reconciliation-trees--metadata):
  after replay, drop any speech-track (id ≥ 1) tree with no `TrackMeta`, treat a `TrackMeta` with an
  empty/missing tree as recoverable corruption, and leave track 0 (labels) exempt. This also closes
  the undo-of-add-track divergence noted in M1 Step 11 (replay re-creates an emptied track that
  in-memory undo dropped). `remove_track` must additionally scrub the id from `aligned_groups` /
  `SpeakerMeta.track_ids` atomically. Add a `remove_track` round-trip fixture; when the guard lands,
  update M1's synthetic multi-track engine tests to populate `TrackMeta` for their speech tracks.
  **Note:** as of M1 Step 11d, `apply_batch` already supports combined tree+metadata edits in one
  transaction (the producer half); M5 owns the `add_track`/`remove_track` *commands* and the
  reconciliation guard itself.
- **From M1 Step 6 — edit-time tree API pattern:** each turn-mutating command (1) receives
  an edit sample `s` from the UI, (2) calls `tree.element_at_sample(s)` to get the affected
  element's hash and its predecessor's hash, (3) computes new element(s) and their hashes via
  `store_turn` / `store_label`, (4) calls `tree.insert_at` / `update_at` / `delete_at` to
  produce the new tree, (5) records `Vec<Delta>` for the journal using `Location::Start` or
  `Location::After(predecessor_hash)` from step 2. Steps 4 and 5 are sibling outputs of the
  same edit pass — neither drives the other.
- **From M1 Step 2 — `busy_timeout` concurrency test:** M5 introduces the first real second
  writer (a synchronous main-thread edit-command journal write racing the background snapshot
  writer's separate connection). Add a test that exercises that race and asserts the loser
  *waits and succeeds* rather than returning `SQLITE_BUSY` — i.e. that the `busy_timeout = 5000`
  pragma set in `Db::open` is doing its job. M1 set the pragma but deliberately deferred the
  test (no concurrent writers exist until now).
- **From M1 — labels must be spaced ≥ 1 sample apart:** the temporal query skips any element with
  `total_duration() == 0` (the `offset < total_duration()` test can never hold), so a label with
  `post_label_silence == 0` is unreachable by a position click and breaks cursor movement — see
  [data-model.md § Temporal query](data-model.md#temporal-query). The label create/move/edit commands
  (`EditLabel` and siblings) must enforce `post_label_silence ≥ 1` at their command-schema boundary
  (`"minimum": 1`), as must any import path that emits labels. M1 doesn't exercise this (no label
  commands yet; synthetic tests use non-zero durations).
- **From M1 Step 10 — metadata-undo already works:** M1's undo snapshots the whole
  `TimelineState` (trees **and** metadata) and journals both effects, so the metadata-editing
  commands here (`rename_track` / `rename_speaker` / `merge_speakers`) are undoable with no
  further undo-machinery work — they just produce an `UndoEntry` whose forward/inverse effect
  sets `metadata_changed` (and an empty delta batch for metadata-only edits). The work in M5 is
  the commands themselves and stamping the right `CommandId` category.
- **From M1 Step 9 — `CommandId` category bits:** every journal row written by an editing
  command must be stamped with the appropriate `CommandId` variant (e.g. `cut_words` /
  `uncut_words` → `CommandId::Cut` / `CommandId::UndoCut`; `rename_track` /
  `rename_speaker` → `CommandId::EditLabel` / `CommandId::UndoEditLabel`; etc.). The
  category bits were locked in Step 9 specifically so their on-disk codes are permanent
  before any category-bearing command writes its first row — the mapping from command name
  to `CommandId` variant is the contract to honour here. Also add the
  **`command_id`-aware history view** in M5: OR-fold the `command_id` values across a
  journal range to produce the set of command categories touched between two project states,
  displayed in the timeline history panel.

## M6 — Frontend *(scaffolds against mocked commands from M0; matures alongside M4/M5)*

- Welcome / new / open / missing-files flows; project shell, toolbar, menus
- **Migration-consent dialog** in the open flow: when `open_project` reports a
  pending `user_version` upgrade, the user picks **Cancel** / **Open read-only**
  (no migration runs; engine refuses mutations for the session) / **Migrate and
  open** before the engine commits to either path. Extends `open_project`
  (a `mode` param or a paired `probe_project` command — TBD at M6 design time)
  and adds an engine read-only mode. See [data-model.md § Schema version](data-model.md#schema-version).
- Timeline: bubble virtualization, overlap columns, `Bubble` / `Word` components
- Cursor & selection model (navigation order), keyboard shortcuts, playback highlight
- Dialogs (ModelDownload, AlignTracks, EnhanceAudio, TrackInfo, RenameTrack,
  Settings); progress / task-queue UI
- Accessibility (aria, focus management, live regions), i18n catalog, rune stores +
  event subscriptions
- No-hardcoded-string CI gate (D2 in conventions.md): add a custom ESLint rule or
  script that flags literal markup text in `.svelte` files and wire it into the
  `frontend-tests` CI job (no clean off-the-shelf rule existed at M0 scaffolding time)

## M7 — Packaging, docs, hardening

- Nuitka sidecar build per platform; Tauri bundle; `release.yml`; model download +
  SHA-256 verification
- Logging (tracing / structlog), diagnostics bundle, crash-report URL
- Complete auto-generated API docs (M0 wired the tools; M7 fills in real docstrings
  and finishes the `rustdoc_to_md.py` converter); wire `docs:build` into CI
- Playwright E2E (pre-release); performance pass (multi-hour transcripts, playback
  latency)
