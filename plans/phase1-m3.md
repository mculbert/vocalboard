# Phase 1 · M3 — Python Sidecar & ML (action plan)

Step-by-step plan for the M3 milestone from [phase1.md](phase1.md). The authoritative
specs are [ml-pipeline.md](../design/ml-pipeline.md) and [architecture.md § IPC](../design/architecture.md#ipc-protocol);
the ML task commands are in [command-surface.md § ML task commands](../design/command-surface.md#ml-task-commands).
M3 runs **in parallel with M2** once the M0 `proto` types exist
([phase1.md](phase1.md) critical path: M0 → M1 → M2/M3 → M4 → M5).

**Definition of done:** a Python sidecar that runs the real ML pipelines (WhisperX
transcribe/align/diarize, MP-SENet enhancement, Gemma disfluency tagging, YAMnet sound
labelling, GPU detection) behind a lazy-loading, idle-unloading model registry; an
**in-memory `TaskQueue` / `TaskDispatcher`** on the Rust side that dispatches these tasks
over the M0 NDJSON channel, streams progress to the UI, applies cancellation, and parses
typed results; and a **non-blocking startup** that opens the window immediately and
surfaces sidecar readiness as a status. The pure-logic pieces (diff-align parser, quality
gate, speaker-merge math, result parsing) are heavily unit-tested **without** loading real
models; real-model behaviour is covered by gated/manual smoke tests.

## Boundary with M4 / M5 (what M3 does *not* do)

M3 builds the **pipelines, the result schemas, the dispatch/progress/cancel plumbing, and
the Rust-side typed result parsing**. It does **not** apply most results to project state:

- **Building the timeline tree from `transcribe_track` turns** is the `import_speech_track`
  orchestration → **M4**.
- **Applying `identify_disfluencies` (set `word_type = Disfluency`) and `remove_disfluencies`
  / `remove_sounds`** → **M5** (editing commands).
- **`classify_sounds` relabelling** and **speaker-merge application** happen during import → **M4**
  (M3 provides the Python labels + the Rust speaker-merge *function*, tested standalone).
- **`enhance_track`** is self-contained (produce the enhanced FLAC at the derived
  `enhanced/<track_id>-enhanced.flac` path — not a stored field; "was enhancement run" is
  `models_used.enhancement.is_some()`) and lands here; the wet/dry *mix* at playback is M2.

This split lets M3 proceed in parallel with M2 against synthetic/canned data, with the
first real **vertical slice** (import → tree → bubbles → play) assembled in M4.

## Decisions to lock first (recommended defaults)

- **Heavy ML deps go in a non-default `[dependency-groups] ml`** (PEP 735) in
  `python/pyproject.toml` — *not* `[project.optional-dependencies]`. The ML stack is
  **required** by the shipped app (M7 Nuitka bakes it into the bundle); "optional" here is
  purely an *install-time/per-environment* concern, and the sidecar is a bundled application
  never `pip install`ed by a consumer, so a published "extra" buys nothing. A
  dependency-group (like the existing `dev = ["pytest"]`) expresses "a heavy environment
  assembled only for real runs + packaging," omitted on the fast lane, without mislabeling
  core functionality optional. Add `torch` (CPU), `whisperx`, `pyannote.audio`, `torchaudio`,
  `llama-cpp-python`, `scipy`, `pyloudnorm`, `PyAV`, and the YAMnet backend (`tensorflow`-lite
  *or* `torch` — TBD, decide at Step 9) to that group. Per
  [ops.md § Python dependencies](../design/ops.md#python-dependencies-key).
  - The default `uv sync` (base `structlog` + the `dev` group) stays tiny, so lint + the
    pure-logic/contract tests run in seconds with no multi-GB wheel install. `uv sync
    --group ml` pulls the heavy stack only where models actually run: dev model work, the
    gated smoke-test lane, and the M7 build. What *enables* the base-only lane is that the
    fast-lane tests never import `torch`/`whisperx` at module load — handler modules import
    them lazily behind the registry (the same `importlib` shape the Nuitka constraint
    requires anyway).
- **`importlib.import_module` for `torch` / `whisperx` / `pyannote`** from day one — the
  Nuitka constraint ([ops.md § Nuitka](../design/ops.md#python-packaging-nuitka)) — even though the
  Nuitka build itself is M7. Keeps the import shape correct so M7 packaging doesn't churn the code.
- **Test strategy without models:** the **diff-align parser**, **transcription quality gate**,
  **prompt/batching builder**, and **speaker-merge math** are pure functions with full unit
  coverage. The model registry is tested with a **fake `LoadedModel`**. Rust-side dispatch +
  result parsing are tested against **canned NDJSON result fixtures**. Real-model runs are
  `@pytest.mark.slow` / `#[ignore]` smoke tests, run manually or in a gated CI lane — never on
  the default path.
- **Non-blocking startup:** replace the M0 behaviour where the Tauri window blocks on
  `rx.recv()` until the sidecar signals ready. M3 opens the window immediately; sidecar
  readiness is surfaced via `get_app_info` + a `sidecar_status` event and the `SidecarStatus`
  enum; ML commands issued before ready return `sidecar_not_ready`.
- **Concurrency:** the dispatcher must support **multiple in-flight requests** (e.g. room-tone
  on two tracks). This is the trigger to add the **concurrent-request test on `send()`** flagged
  in the M0 retro (M0 only ever had one in-flight `ping`).
- **Cancellation** follows [architecture.md § Cancellation](../design/architecture.md#cancellation-semantics):
  per-request cancel flag checked at safe checkpoints; llama.cpp subprocess gets `SIGTERM` →
  5 s → `SIGKILL`. Cancel does **not** unload models.
- **Working branch:** `claude/1M3` (commits unsigned per [CLAUDE.md](../CLAUDE.md)); numbered
  sub-step commits (`1M3-01 …`).

## Module layout

**Python** (`python/vocalboard_sidecar/`, mirroring [ops.md § repository layout](../design/ops.md#repository-layout)):

```
registry.py        ModelRegistry: lazy Dict[role, LoadedModel]; _load(role, path);
                   idle-unload loop (model_idle_unload_seconds); manifest.json scan
dispatch.py        NDJSON loop (M0) extended: route to handlers, per-request cancel flags,
                   thread-pool, progress emit (≤ 1/s), typed result/error envelopes
handlers/
  whisperx.py      preproc (16k mono, LUFS, HPF) + quality gate + transcribe/align/diarize
  pyannote.py      diarization wiring (via WhisperX) + per-turn embeddings
  mp_senet.py      enhancement: chunk / delay-comp / stitch / resample / write FLAC
  gemma_llama.py   batching + prompt template + tagged generation + diff-align parser
  yamnet.py        classify_sounds top-1 labelling
  gpu.py           detect_gpu (torchruntime device info)
```

**Rust** (`src-tauri/core/task/`, building on the M0 `SidecarManager`):

```
task/
  mod.rs           SidecarManager (M0) — extend send() for concurrency + cancel control
  queue.rs         TaskQueue (in-memory): Task { id, kind, status, progress }; enqueue/list
  dispatch.rs      TaskDispatcher: build request payload (inject model_paths), send, route
                   progress → Tauri events, parse typed result, mark status
  models.rs        model_dir scan (enumerate downloaded models per role) + per-role path
                   resolution from settings.model_paths
  results.rs       typed result structs + parsers for each task (turns/words/embeddings,
                   disfluency word positions, sound labels, device info) + speaker-merge math
```

`proto` gains the ML task param/result types + the progress / `sidecar_status` event
payloads; `app/main.rs` gains the `cancel_task` / `list_tasks` / `detect_gpu` handlers and
the non-blocking startup wiring.

---

## Step 1 — Action-plan doc, branch, ML dependencies

- This document; create the `claude/1M3` branch.
- **Complete the model resource-management & scheduling plan (blocking prerequisite).** Flesh
  out the placeholder in [ml-pipeline.md § Model resource management & scheduling](../design/ml-pipeline.md#model-resource-management--scheduling)
  into a finished policy — the heavy-unit lock + `unload_to_fit` eviction, the same-unit
  batching queue order, and the precise Rust↔Python ownership protocol (Phase 1 = option A,
  the correctness floor). This must be settled **before** Steps 2–3 are implemented, because it
  reshapes the registry (capacity-aware load) and the TaskQueue (model-affinity scheduler, not
  FIFO). Reflect the finished scope back into Steps 2 and 3 here.
- Add the `[dependency-groups] ml` group to `python/pyproject.toml` (see the decision above);
  keep the default install light. Add any Rust deps the dispatcher needs (likely none beyond
  M0's `tokio` / `uuid` / `serde_json`).
- **Verify:** `uv sync` (default — base + `dev`, no `ml`) stays fast and `pytest` green;
  `uv sync --group ml` resolves the heavy stack on a dev machine; `cargo build` green;
  `pip-audit` / `cargo deny`
  policies pass.

## Step 2 — Model registry + manifest + path resolution (`registry.py`, `task/models.rs`)

*Novelty: medium (scope grows once the Step-1 scheduling plan lands — see below).*

> **Scope pending:** the Step-1 resource-management plan adds a capacity-aware load path +
> `unload_to_fit` pre-emptive eviction to the registry (beyond the idle-unload below). Detail
> lands when that plan is finished; the bullets here are the baseline.

- **Python `registry.py`:** lazy `Dict[role, LoadedModel]`; `get(role)` loads on first use
  and stamps `last_used`; `_load(role, path)` loads from the path **supplied in the request
  payload** (directory for WhisperX/pyannote/MP-SENet/YAMnet; `.gguf` file for Gemma); an
  idle-unload background thread (interval 60 s, timeout `model_idle_unload_seconds`, default
  300). Parse `models/manifest.json` ([ml-pipeline.md § manifest.json](../design/ml-pipeline.md#manifestjson)).
- **Rust `task/models.rs`:** scan `model_dir` to enumerate available (downloaded) models per
  role for the Settings picker; resolve `settings.model_paths.<role>` → the path injected into
  each request payload (the frontend never names a model — [command-surface.md](../design/command-surface.md)).
  A role may be `null` (allowed) → `model_not_available` when a task needs it.
- **Verify:** registry loads via a **fake `LoadedModel`** and unloads after the idle window;
  manifest parse handles the [ml-pipeline.md](../design/ml-pipeline.md) shape; Rust scan enumerates a
  temp model dir's roles; path resolution returns the right path or `null`.

## Step 3 — TaskQueue + TaskDispatcher (`task/queue.rs`, `task/dispatch.rs`, `dispatch.py`)

*Novelty: high — in-memory task lifecycle + concurrent IPC + progress routing + the
**model-affinity scheduler** (not FIFO). Full sub-step plan doc (`phase1-m3-03.md`); likely
pre-split (queue/lifecycle vs. the scheduling policy) once the Step-1 plan sets the scope.*

> **Scope pending:** the Step-1 resource-management plan makes this a **model-affinity +
> priority scheduler behind a heavy-unit lock** (batch same-unit tasks; serialize heavy work),
> not the plain FIFO the baseline bullets describe. Detail lands with that plan.

- **Rust:** `TaskQueue` (in-memory; a killed app loses in-flight tasks per
  [index.md](../design/index.md) — only completed ops are journaled). `TaskDispatcher` builds the
  request payload (injecting the resolved model path), calls `SidecarManager::send`, routes
  `progress` lines to a `task_progress` Tauri event, parses the typed `result`, and marks task
  status. Wire `cancel_task` / `list_tasks` (read-only, non-journaled). **Extend `send()` for
  the concurrent case and add the M0-retro concurrent-request test.**
- **Python `dispatch.py`:** extend the M0 NDJSON loop to route by `command` to the Step 5–9
  handlers in a thread pool, hold a **per-request cancel flag**, and emit progress (≤ 1 line/s
  per request per [architecture.md § Back-pressure](../design/architecture.md#back-pressure)).
- **Verify:** two overlapping requests both resolve to the correct `request_id` (no drop —
  closes the M0 retro gap); progress lines surface as ordered events; `cancel_task` flips
  status; `list_tasks` reflects the queue. Tested with a stub command (no models).

## Step 4 — Non-blocking startup (`task/mod.rs`, `app/main.rs`, `proto`)

*Novelty: medium — refactors M0's blocking path; touches the concurrency the M0 retro warned
about (reason through both orderings).*

- Replace the blocking `rx.recv()` window gate (M0): spawn the sidecar, **open the window
  immediately**, and surface readiness through `SidecarStatus` (`NotStarted` → `Starting` →
  `Ready` / `Error`) via `get_app_info` + a `sidecar_status` event. ML commands before `Ready`
  return `sidecar_not_ready`.
- **Verify:** `get_app_info` returns a non-ready status before the sidecar signals, then a
  `sidecar_status` event flips it to `Ready`; an ML command issued early returns
  `sidecar_not_ready`; the existing M0 ready-signal + route-line tests still pass.

## Step 5 — WhisperX pipeline (`handlers/whisperx.py`, `task/results.rs`)

*Novelty: high — the largest ML surface. Full sub-step plan doc (`phase1-m3-05.md`);
pre-split into 5a / 5b.*

- **5a — preprocessing + quality gate.** Decode source → f32 mono 16 kHz (PyAV/torchaudio);
  integrated-LUFS normalize to −23 LUFS (`pyloudnorm`); optional 80 Hz HPF (`scipy`). After
  transcription, the **quality gate**: reject (`low_confidence_transcript`) if mean
  `avg_logprob < −1.0` or > 20% of segments below it; flag `no_speech_prob > 0.6` segments as
  suspect. Per [ml-pipeline.md § Pre-processing / quality gate](../design/ml-pipeline.md#pre-processing-in-python).
- **5b — transcribe / align / diarize + result format.** `whisperx` transcribe →
  wav2vec2 align → pyannote diarization; assemble the [ml-pipeline.md § Result format](../design/ml-pipeline.md#result-format):
  `turns[]` with `speaker_id_local`, `embedding_blob_b64`, and `words[]` (`text`, `start_sec`,
  `end_sec`, `word_type`). Room tone + non-speech detection are **not** here (Rust). Add the
  Rust `task/results.rs` parser → typed `TranscribeResult` (consumed by M4 import).
- **Verify:** quality gate and preproc are **pure-function unit tests** (synthetic
  segments/PCM); result parsing round-trips a **canned `transcribe_track` JSON fixture** into
  the typed struct; a real-model end-to-end run is a gated `@pytest.mark.slow` smoke test on a
  short clip.

## Step 6 — Speaker embeddings + merge math (`handlers/pyannote.py`, `task/results.rs`)

*Novelty: medium — the merge math is pure and load-bearing for M4.*

- Python returns per-turn embeddings + a per-speaker mean (normalized f32, base64) in the
  transcribe result. Rust `task/results.rs` gets the **speaker-merge function**: cosine
  similarity vs existing speakers, merge when `> speaker_merge_threshold` (default 0.71),
  else `Speaker N` (incrementing) — per [ml-pipeline.md § Speaker diarization](../design/ml-pipeline.md#speaker-diarization-and-embeddings).
  The **application** (writing `SpeakerMeta`, the `Embedding` 0x5 blob) happens at M4 import;
  M3 ships the pure decision function + the embedding-blob encode/decode.
- **Verify:** cosine-merge picks the right existing speaker above threshold and allocates a
  new one below it; naming increments correctly; the normalized-embedding blob round-trips
  (pinned bytes + hash for the `Kind::Embedding` 0x5 V1 blob, G1).

## Step 7 — MP-SENet enhancement (`handlers/mp_senet.py`)

*Novelty: medium.*

- Implement [ml-pipeline.md § Enhancement](../design/ml-pipeline.md#enhancement-pipeline-mp-senet):
  decode → 2–5 s overlapping chunks → MP-SENet → **group-delay compensation** (impulse
  measured once at load) → 50 ms crossfade stitch → resample to project rate → write
  `<project>.vbdata/enhanced/<track_id>-enhanced.flac` (keyed by the stable `TrackMeta.id`).
  The path is **derived** from the track id (not stored in `TrackMeta`); enhancement is recorded
  via `models_used.enhancement` (Rust side). Wet/dry mixing is M2's renderer, **not** a param here.
- **Verify:** chunking/stitch/delay-comp are unit-tested on synthetic audio (output length +
  continuity at chunk seams); a gated smoke test runs the real model on a short clip; the
  written FLAC is decodable by the M2 cache reader.

## Step 8 — Gemma disfluency (`handlers/gemma_llama.py`, `task/results.rs`)

*Novelty: high — the diff-align parser is novel and fully pure-testable. Full sub-step plan
doc (`phase1-m3-08.md`); pre-split into 8a / 8b.*

- **8a — batching + prompt + tagged generation.** Batch the transcript by the selected model's
  class ([ml-pipeline.md § Batching](../design/ml-pipeline.md#batching)); build the tagged-text prompt
  (`<filler>/<stutter>/<repetition>/<repair>`); run llama.cpp (`llama-cpp-python`, `n_gpu_layers`
  per GPU availability); collect tagged output. Batch overlap is **context-only** (union of
  tags across overlapping batches).
- **8b — strip + diff-align parser.** Strip tags, **fuzzy/token diff-align** the tagged output
  back to input word positions (tolerant of LLM reproduction drift — *not* byte-equality), emit
  `{ turn_id, word_index }` per disfluent word, collapsing all four subtypes to a single mark
  (subtype not stored). Per [ml-pipeline.md § Result](../design/ml-pipeline.md#result). Rust `results.rs`
  parses this into a typed list (applied by M5's `identify_disfluencies` command).
- **Verify:** the diff-align parser is the **heavily-tested pure unit** — verbatim echo,
  dropped/added/substituted tokens, multi-word spans, overlapping-batch union, and a tag that
  fails to align (handled gracefully). Gated smoke test for real Gemma generation.

## Step 9 — YAMnet + GPU detection (`handlers/yamnet.py`, `handlers/gpu.py`)

*Novelty: low–medium.*

- **`classify_sounds`:** run YAMnet over the per-event audio Rust supplies; return top-1
  label per event ([ml-pipeline.md § YAMnet](../design/ml-pipeline.md#yamnet-sound-classification)).
  Detection is Rust (M2); this only labels. Decide the YAMnet backend here (tf-lite vs torch).
- **`detect_gpu`:** `torchruntime.get_device_info()` → device description ([ml-pipeline.md
  § GPU detection](../design/ml-pipeline.md#gpu-detection-and-acceleration)); Rust `detect_gpu` command
  surfaces it (the wheel-download + sidecar-restart is M7 packaging — M3 returns the device info).
- **Verify:** label parsing round-trips a canned `classify_sounds` result into the typed
  per-event list; `detect_gpu` returns a structured device descriptor (mocked detector in CI;
  real detection a gated smoke test).

## Step 10 — Cancellation (`dispatch.py` checkpoints, `task/dispatch.rs`)

*Novelty: medium — concurrency + subprocess signalling.*

- Insert cancel-flag checks at the [architecture.md](../design/architecture.md#cancellation-semantics)
  safe checkpoints in each handler (between MP-SENet chunks, WhisperX segments, Gemma batches,
  pyannote turn comparisons). For llama.cpp: `SIGTERM` → wait 5 s → `SIGKILL` /
  `TerminateProcess`. On cancel emit `{ type: error, code: "cancelled" }` and return to ready
  **without unloading models**; Rust marks the task `cancelled`.
- **Verify:** a long stub handler honours a mid-run cancel and emits `cancelled` promptly; the
  Rust dispatcher transitions the task to `cancelled` and frees the queue slot; models stay
  loaded across a cancel.

## Step 11 — Contract tests + final pass

*Novelty: medium.*

- Wire the `proto` ML task param/result types + the progress / `sidecar_status` events; ensure
  `#[serde(deny_unknown_fields)]` + value guards + version-by-name (H1/J2). Regenerate TS
  bindings. Add **Rust↔Python contract tests** asserting each request/response envelope
  round-trips against the typed structs (the cross-OS coverage the M0 retro flagged remains a
  `pytest`-on-all-platforms + Linux-only Rust-integration split).
- Run the full gate.
- **Verify:** `cargo run -p proto --features ts-export --bin gen_bindings -- --check`,
  `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, `pytest`
  (default, no `ml`), and `pnpm check && pnpm test && pnpm build` all green.

## Testing strategy (pure-logic heavy; models gated)

- **Pure-function unit tests** carry the weight: diff-align parser, quality gate, batching/prompt
  builder, speaker-merge math, all result parsers — no model load required.
- **Fake `LoadedModel` + canned NDJSON fixtures** for registry, dispatch, and Rust result
  parsing. Concurrent-request test on `send()` (M0 retro).
- **Gated/manual smoke tests** (`@pytest.mark.slow` / `#[ignore]`) run real WhisperX / MP-SENet /
  Gemma / YAMnet on short clips — excluded from the default CI path (heavy wheels stay off it).
- **Cross-OS sidecar contract** via `pytest` on all three platforms; the Rust↔Python integration
  test stays Linux-only (M0 retro gap, unchanged here).

## M3 exit criteria

- `pytest` (default), `cargo test --workspace`, `cargo clippy -- -D warnings`,
  `cargo fmt --check`, and `pnpm check && pnpm build` all green locally and in CI.
- The sidecar runs all five pipelines behind the lazy/idle-unload registry; the Rust
  `TaskDispatcher` dispatches, streams progress, cancels, and parses typed results for every ML
  task command; startup is non-blocking with a surfaced `SidecarStatus`.
- `detect_gpu` / `cancel_task` / `list_tasks` round-trip through Tauri with in-sync TS bindings.
- [ml-pipeline.md](../design/ml-pipeline.md) + [architecture.md](../design/architecture.md) stay authoritative —
  any field/behaviour adjusted during implementation is updated there in the same commit.

> **First vertical slice is assembled in M4, not here.** With minimal M1 + M2 + M3 in place,
> M4 wires `import_speech_track` for a single track (probe → transcribe → build tree → render →
> play) to de-risk IPC + tree-from-turns + playback together. M3 makes that possible by
> delivering the transcribe pipeline + dispatcher; the orchestration command itself is M4.

> **Deferred to later milestones:** applying transcribe turns to the tree + speaker-merge +
> `classify_sounds` relabel (M4 import); `identify_disfluencies` / `remove_disfluencies` /
> `remove_sounds` **application** to project state (M5); the GPU wheel download + sidecar
> restart and the Nuitka sidecar build (M7); cross-session task-queue persistence ("Restore
> queued actions" — Phase 2).
</content>
