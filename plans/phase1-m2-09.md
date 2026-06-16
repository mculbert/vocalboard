# Phase 1 · M2 · Step 9 — Playback engine (action plan)

Per-step action plan for Step 9 of the M2 milestone from [phase1-m2.md](phase1-m2.md) — the
**trickiest M2 step**: real-time, lock-free, `cpal` callback constraints. The authoritative spec is
[audio-pipeline.md § Playback engine](../design/audio-pipeline.md#playback-engine) (output stream, ring
buffer, playhead events, stop conditions) and [command-surface.md § Playback commands](../design/command-surface.md#playback-commands)
(`play_from` / `pause` / `stop`).

It builds the **concrete `SourceProvider`** over the project's FLAC cache (folded in here from
Step 8 — see [phase1-m2-08.md](phase1-m2-08.md) "Out of scope"), then drives the Step-8 `Renderer`
through a **lock-free SPSC ring buffer** to a `cpal` output stream, emitting `playhead_update` and
`playback_stopped` events. **Real-time discipline is a hard invariant** ([CLAUDE.md](../CLAUDE.md)):
the cpal callback only *drains* a pre-allocated ring buffer — **no allocation, no locking, no
blocking I/O**. All rendering + FLAC decode happen on the pre-roll thread.

> **Planning note (implementer split).** The concurrency/real-time spine (the ring + backend trait
> and its no-alloc contract, the pre-roll thread + shutdown/join protocol, and the
> `play_from`/`pause`/`stop` state machine) is **designed in detail in this doc** (§ Detailed
> design) so that **Sonnet can implement every sub-step from the spec**. The three "Detailed design"
> subsections are the Opus-authored contract; implement them literally rather than re-deriving the
> concurrency model. The remaining sub-steps (concrete provider, playhead events, final pass) are
> routine plumbing over existing, tested primitives.

**Definition of done:** `core/src/audio/source_provider.rs` provides the real `SourceProvider` over
the resampled cache + enhanced FLAC + pre-loaded room tone; `core/src/audio/playback.rs` opens/owns
the cpal stream lifecycle (scoped to the open project, sized to the negotiated device rate), runs the
pre-roll thread feeding a lock-free ring buffer drained by a no-alloc callback — resampling the
renderer's project-rate output to the device rate when they differ — emits the playhead/stopped
events, and implements `play_from` / `pause` / `stop`; integration tests drive playback over a
synthetic project with an in-memory backend (including a forced device rate) and assert the frame
sequence + event cadence + stop conditions; `cargo test -p core audio::`, `cargo clippy`, `cargo fmt
--check` green.

## Decisions locked in this step

- **The concrete `SourceProvider` (`CacheSourceProvider`) is a thin adapter, not new DSP.** Its
  ranged-decode primitive already exists: [`frame_reader.rs`](../src-tauri/core/src/audio/frame_reader.rs)
  exposes `SymphoniaFrameReader::open(path)` + `read_range(start, n)` (sample-accurate seek+decode,
  proven by test `FR4`), which is exactly `dry()` / `enhanced()`. Room tone is **already decoded into
  memory at project open** (`engine.rs` loads `room_tones: BTreeMap<u32, Arc<RoomTone>>` from the
  store with `Db` in scope), so the provider is handed pre-decoded `Arc<RoomTone>` and `room_tone()`
  just returns the slice — **no SQLite/`Db` on the provider** (preserves the RT-path invariant).
  `channels`/`wet_ratio`/`source_len` come from `TrackMeta` (`source_channels`, `wet_dry_ratio`,
  `original_length_samples`); the dry path is `cache::resampled_cache_path(vbdata, id)`. `enhanced()`
  returns `None` while no enhanced file exists (it is an M3 product).
- **The `cpal` output stream is scoped to the open project, not app start** ([audio-pipeline.md §
  Output stream](../design/audio-pipeline.md#output-stream)). The stream config and the ring capacity both
  depend on the **negotiated device rate** (see the next decision), which is unknown until a project
  opens; the `PlaybackEngine` (which owns stream + ring) is therefore constructed at project open,
  reused across all play/stop cycles within the project, and recreated on project switch. The engine
  is a value, not a global, so a second project window (Phase 6) owns its own stream.
- **Output runs on two sample-rate clocks; the pre-roll thread bridges them.** The default device
  may not open at the project's locked rate (e.g. a 44.1 kHz project on a 48 kHz-locked device, or
  audio routed through Bluetooth at 16 kHz). Playing on the wrong rate is not "polish" — it is
  *broken playback on common hardware* — so Step 9 **adapts the default device's rate** (it does
  **not** add device selection or hot-swap, which stay post-Phase 1). The boundary splits into two
  clocks: the **project clock** (renderer input, EDL, `start`/`end`/`total`, playhead *semantics*)
  upstream of a resampler, and the **device clock** (the ring, the callback, `frames_played`, the
  drain-wait) downstream of it. The bridge is a [`StreamingResampler`](../src-tauri/core/src/audio/resample.rs)
  on the **pre-roll thread** (the only RT-safe place — the callback must never allocate or run DSP).
  It is an **identity passthrough when the rates match** (built into `StreamingResampler`), so the
  common case and every in-memory test cost nothing. **Negotiation prefers an exact-rate device
  config** and only falls back to the device default rate (+ resample) when none matches.
- **The ring is sized at the negotiated *device* rate, so negotiation happens before allocation.**
  The SPSC ring (`rtrb`, locked at Step 1) is split into a **producer** (held by the engine, written
  only by the pre-roll thread) and a **consumer** (captured by the cpal data callback). Both halves
  are allocated once when the stream opens, sized to **`RING_MS` (= 200) ms of stereo frames at the
  device rate** (the callback drains into a device buffer running at that rate), and live as long as
  the stream. Because the capacity depends on the device rate, `Backend::negotiate(project_rate) ->
  device_rate` runs **before** the ring is allocated (two-phase: `negotiate`, then `start(consumer,
  shared)`); `PlaybackEngine` stores both rates (`project_rate()` / `device_rate()`). `RING_MS` is a
  **named `const`, not a setting**: it is an internal latency-vs-underrun tradeoff with a sensible
  default; exposing it would make it a `settings.json` field requiring a migration + round-trip test
  ([CLAUDE.md](../CLAUDE.md) persisted-format invariant) for negligible user value. Revisit as a
  setting only if real-world underruns appear.
- **The callback only drains.** The cpal data callback pops frames from the SPSC consumer into the
  output buffer; on **underrun it writes silence** and never blocks/allocates/locks. The producer
  (pre-roll thread) is the only writer. See § Detailed design A for the exact no-alloc contract.
- **The pre-roll thread owns the `Renderer`.** It loops: render a `RENDER_CHUNK_MS` (~10 ms) chunk
  via Step 8, push to the ring's producer when space allows (back-pressure by parking briefly when
  full), and track the **render position**. All file I/O / decode lives here. It exits on
  end-of-EDL, reaching `end_sample`, or a stop signal. See § Detailed design B.
- **Playhead position is the *played* position, not the *rendered* position.** The ring holds up to
  `RING_MS` of already-rendered-but-unplayed audio; `playhead_update` must report what the user is
  hearing. Derive it from frames actually consumed by the callback (an atomic `frames_played` the
  callback increments **only for real frames it pops**, not for silence padding). `frames_played` is
  in **device frames**; map to project samples on the pre-roll thread as `start_sample +
  round(frames_played · project_rate / device_rate)` (an exact identity when the rates match). The
  natural-stop reported positions (`end_sample`, project end) come straight from the **project
  clock**, not from `frames_played`, so they stay exact. Emit `playhead_update { position_samples }`
  every `PLAYHEAD_INTERVAL_MS` (= 50) from the pre-roll thread reading that atomic — **never from the
  callback** (no Tauri emit / no work beyond the drain on the RT path; the rate conversion is on the
  pre-roll thread, off the RT path).
- **Stop conditions** ([audio-pipeline.md § Playback stops](../design/audio-pipeline.md#playback-stops)):
  (1) `end_sample` reached; (2) end-of-EDL when `end_sample == None`; (3) user `stop`. On any stop,
  the pre-roll thread stops producing, the callback flushes the ring (see § Detailed design A), the
  thread is joined, and `playback_stopped { position_samples }` is emitted with the **last played**
  position. See § Detailed design C.
- **`play_from` / `pause` / `stop` are non-journaled** (not project mutations). `play_from` joins any
  prior session, then starts a fresh pre-roll thread over a pre-built `Renderer` for
  `[start_sample, end_sample)`; the frontend resolves scope → range. `pause` stops the pre-roll
  thread but **retains position** (a subsequent `play_from` resumes from there per the frontend's
  range); `stop` reports the last position and the frontend maps it to the nearest word/cursor.
- **Testability via an injected backend.** Abstract the output behind a `Backend` (the consumer
  side): the production impl drives the cpal callback; the test impl is an in-memory consumer driven
  synchronously (pull N frames, record them, advance `frames_played`, honour the same playing/flush
  semantics). This lets the integration tests assert the exact frame sequence and event cadence
  **without audio hardware** and without a real cpal stream (CI has no audio device). See § Detailed
  design A.
- **No allocation on the callback path** is asserted by **construction + review** (the
  CLAUDE.md/audio-pipeline review gate, not CI-enforceable): the callback closure captures only the
  ring consumer + the atomics; all buffers are pre-allocated at open.

## Detailed design (Opus-authored — implement literally)

### A. Ring + `Backend` trait + the no-alloc callback contract

The output side is one `rtrb` ring split at stream open into `Producer<f32>` (engine) and
`Consumer<f32>` (callback). Shared control state, all `Arc`-wrapped and `Send + Sync`:

```rust
struct Shared {
    frames_played: AtomicU64,  // real stereo frames the callback has delivered this session
    playing: AtomicBool,       // false ⇒ callback discards (flush) instead of delivering
}
```

The callback (and the in-memory backend's `pull`) obey **one contract** so tests and production
agree:

1. Repeatedly read the largest available chunk from the consumer (`rtrb` `read_chunk`).
2. If `playing`: `copy_from_slice` real frames into the output slice and `frames_played +=
   frames_copied` (interleaved stereo ⇒ frames = samples / 2). If **not** `playing`: pop and
   `commit` the chunk **without copying** (this is the inter-session flush — see C).
3. When the output slice is not yet full and the ring is empty: fill the remainder with `0.0`
   (silence). This is the underrun path when `playing`, and the steady state between sessions.
4. **Never** allocate, lock, block, or emit on this path. The closure captures only `Consumer<f32>`
   and `Arc<Shared>`; all scratch is the caller-provided output slice.

`Backend` abstracts steps 1–4 so the engine is hardware-independent. It is **two-phase** because
the ring is sized to the device rate, which `negotiate` resolves *before* the ring (hence the
consumer) exists:

```rust
pub trait Backend: Send {
    /// Resolve the device rate given the project's desired rate. Runs BEFORE the ring is
    /// allocated. cpal: prefer an exact-rate config, else fall back to the device default rate.
    /// in-memory: return `requested` (passthrough) unless a rate was forced for testing.
    fn negotiate(&mut self, requested: u32) -> Result<u32, AudioError>;
    /// Hand the consumer + shared state to the backend and start delivering (cpal: build & play
    /// the stream; in-memory: store them for synchronous `pull`). Called once, after `negotiate`.
    fn start(&mut self, consumer: Consumer<f32>, shared: Arc<Shared>) -> Result<(), AudioError>;
}

pub enum BackendKind { Cpal, InMemory, InMemoryAtRate(u32) }
```

- **`CpalBackend`** (sub-step 9c): `negotiate` queries `supported_output_configs()`, prefers a config
  at the project rate and otherwise picks the device default rate; `start` builds the output stream
  and moves `consumer` + `shared` into the data callback implementing the contract above, calls
  `stream.play()`, and holds the stream alive.
- **`InMemoryBackend`** (sub-step 9b, **landed**): `negotiate` returns the requested rate (or a
  forced rate for testing); `start` stores `consumer` + `shared`; `pull(&mut self, frames: usize) ->
  Vec<f32>` runs the same contract synchronously and returns exactly what a device buffer of that
  size would have received. Tests drive the whole engine through `pull`. `BackendKind::InMemoryAtRate(r)`
  forces `negotiate` to return `r ≠ project_rate`, exercising the resampling (two-clock) path with no
  audio hardware.

### B. Pre-roll thread, `Send`/`'static` renderer, shutdown/join

The pre-roll thread is the **sole producer**. It owns the `Renderer` and the `Producer<f32>`.

**`Send`/`'static` prerequisite (do this first, it gates the thread).** `Renderer<'a, P>` currently
*borrows* the timeline via `EdlCursor<'a>`, so it cannot be moved into a spawned thread that
outlives `play_from`. The timeline tree is already an immutable `Arc` structure, so add an
**owned/`Arc`-backed cursor constructor** such that the built `Renderer` is `'static + Send` (the
provider `CacheSourceProvider` is owned and `Send`; verify the renderer's internal maps contain no
`Rc`/non-`Send` types). The Step-11 handler `Arc`-clones the timeline snapshot when building the
renderer; Step 9's `play_from` receives an already-`'static` renderer. If the minimal change is a
`EdlCursor::owned(Arc<…>)` next to the existing borrowing constructor, make it here and note it in
the Step-8/EDL docs (doc-sync).

**Resampler bridge (project clock → device clock).** The pre-roll thread does **not** push renderer
output to the ring directly when `project_rate ≠ device_rate`. The `Renderer` **is itself** the
project-clock `PcmSource` (`sample_rate = project_rate`, `channels = 2`); `read` fills the caller's
buffer via the shared `read_frames` core, and `render(n) -> Vec` is a thin allocating convenience
over that same path. The `[start, end)` play window is owned by the renderer's `EdlCursor` (the
Step-11 handler builds the cursor over that range), so `read` reports `is_exhausted()` once the
cursor is spent — end_sample **or** end-of-EDL — with **no** separate frame-count cap. Feed the
renderer to `StreamingResampler::new(renderer, device_rate, quality)`; the pre-roll thread then reads
**device-rate** chunks from the resampler and pushes them. `StreamingResampler` is an identity
passthrough when the rates are equal (so the in-memory / matched-rate path is unchanged), trims its
startup-delay so output frame 0 aligns to project frame `start`, and computes a deterministic flush
length from the source's exhaustion — giving an exact device-frame count for `[start, end)`. Build a
**fresh resampler per `play_from`** (like the renderer). The `quality` (`ResamplingQuality`, default
`Balanced`) is **stream-lifetime**, so it is a `quality` argument to `PlaybackEngine::new`
(`new(sample_rate, backend, quality)`, stored on the engine and read by the pre-roll thread —
**landed in 9b**); do **not** add it to `play_from` (keep that signature focused on `start` +
renderer + emit; the window rides in the renderer's cursor). One consequence to note (not a blocker): a `pause` → resume builds a new
resampler with cold filter state, so there is a sub-perceptible warmup at the resume seam —
acceptable for a user-initiated gap, and delay-trim keeps frame-0 aligned.

**Loop.** The play window (`start`, `end`) is in **project frames**, owned by the renderer's
`EdlCursor`; `start` is also kept on the engine side for playhead reporting. The ring, `produced`,
and the drain-wait count **device frames**:

1. `chunk = device-rate frames` via `resampler.read(RENDER_CHUNK_MS-worth)`; an empty return =
   end-of-stream (the renderer's cursor hit end_sample or end-of-EDL and the resampler has flushed).
2. The `[start, end)` truncation lives in the renderer's `EdlCursor` (it stops yielding slices at
   `end`); the resampler emits the matching device-frame count — no separate device-side cap.
3. Push to the producer; if the ring is full, `thread::park_timeout(small)` and retry (back-pressure
   — bounded memory, no spin-burn). Re-check the stop flag each iteration.
4. Stop producing when: the stop flag is set (user stop/pause), or step 1 returned empty (end_sample
   reached via the cursor's `end` bound, or end-of-EDL).
5. **Drain wait:** after producing the last frame for an end_sample/end-of-EDL stop, wait until
   `frames_played == produced` (both **device** frames — the ring has been delivered) before emitting
   `playback_stopped`, so the reported position is what was actually heard. The reported position is
   the **project**-clock value (`end_sample` / project end), not a back-converted `frames_played`.
   (For a user stop, skip the drain — report `start + round(frames_played · project_rate /
   device_rate)`; see C.)

**Shutdown/join.** The engine holds the `JoinHandle` in a control-side `Mutex<Option<JoinHandle>>`
(this Mutex is on the *control* path — `play_from`/`pause`/`stop` — never the audio callback, so it
does not violate the RT invariant). A `stop_requested: AtomicBool` is the signal. Joining the prior
thread is mandatory before starting a new one so there is **only ever one producer** (SPSC
invariant). `park_timeout` (not `park`) guarantees the thread observes `stop_requested` promptly even
when the ring is full.

### C. `play_from` / `pause` / `stop` state machine

Single owned session at a time; the engine is otherwise idle with the callback emitting silence.

- **`play_from(start, end, renderer, emit)`**: (1) if a session is live, perform the `stop`
  teardown below (join); (2) `frames_played = 0`, `stop_requested = false`, record `start`/`total`;
  (3) `playing = true`; (4) spawn the pre-roll thread (§ B) capturing `producer`, `renderer`,
  `emit`, and the playhead timer.
- **Inter-session flush.** Between sessions the callback runs with `playing = false`, so any frames
  left in the ring from a stopped session are popped-and-discarded (contract step 2) and never
  played. `play_from` sets `playing = true` only after resetting `frames_played`; the producer then
  fills the (now-draining-to-empty) ring with fresh content. This satisfies test 18 (ring reused, no
  per-play reopen) without the control thread ever touching the consumer.
  All control-side positions below are the **project**-clock value `pos(frames_played) = start +
  round(frames_played · project_rate / device_rate)` (an exact identity at matched rates).
- **`pause() -> i64`**: set `stop_requested`, set `playing = false`, join the thread, return
  `pos(frames_played)` (the retained position). **No `playback_stopped` emit.**
- **`stop() -> i64`**: same teardown as pause, then emit `playback_stopped { pos(frames_played) }`
  exactly once. **Idempotent:** a `stop` with no live session is a no-op that returns the last
  position and does not re-emit.
- **Natural stop (end_sample / end-of-EDL):** the pre-roll thread itself does the drain-wait (§ B
  step 5), emits `playback_stopped`, sets `playing = false`, and exits; the engine reaps the handle
  on the next control call. For end_sample the reported position is `end_sample`; for end-of-EDL it
  is the project/tree end.

## Module surface

```rust
// audio/source_provider.rs  (sub-step 9a; reused by Step 10 export + Step 11 handlers)

/// The real `SourceProvider` over a project's `.vbdata` cache. Built off the RT path (room tone
/// pre-decoded by the caller); opens dry/enhanced FLAC readers lazily on the pre-roll thread.
pub struct CacheSourceProvider { /* vbdata dir, per-track readers + meta + Arc<RoomTone> */ }

impl CacheSourceProvider {
    /// One entry per track: id, source_channels, wet_dry_ratio, source_len (original_length_samples),
    /// and the pre-decoded room tone (None when `room_tone_hash` was null).
    pub fn new(vbdata_dir: PathBuf, tracks: Vec<TrackSource>) -> Self;
}
pub struct TrackSource {
    pub id: u32, pub channels: u16, pub wet_ratio: f32,
    pub source_len: i64, pub room_tone: Option<Arc<RoomTone>>,
}
// impl SourceProvider for CacheSourceProvider { … }  // dry/enhanced via SymphoniaFrameReader::read_range

// audio/playback.rs  (sub-steps 9b–9f)

/// Owns the cpal output stream + ring buffer; created at project open. Stores `project_rate`
/// (renderer output) and the negotiated `device_rate` (ring / callback / `frames_played`).
pub struct PlaybackEngine { /* project_rate, device_rate, backend, producer, Arc<Shared>, Mutex<Option<JoinHandle>>, stop flag, last_pos */ }

impl PlaybackEngine {
    /// Negotiate the device rate, then open the output stream (device rate, stereo, RING_MS ring)
    /// and keep it alive. `sample_rate` is the project rate; `quality` is the sinc preset for the
    /// per-`play_from` project→device resampler (ignored when the rates match). `BackendKind::InMemory`
    /// substitutes the synchronous consumer for the cpal callback; `InMemoryAtRate(r)` forces a rate.
    pub fn new(sample_rate: u32, backend: BackendKind, quality: ResamplingQuality)
        -> Result<Self, AudioError>;    // LANDED (9b)
    pub fn project_rate(&self) -> u32;  // LANDED (9b)
    pub fn device_rate(&self) -> u32;   // LANDED (9b) — == project_rate unless resampling

    /// Start playback of `renderer` (already `'static + Send`); the `[start, end)` window rides in
    /// the renderer's `EdlCursor`, and `start` here is only the project-clock origin for playhead
    /// reporting. Drives the renderer (itself a `PcmSource`) through `StreamingResampler(device_rate)`
    /// (passthrough when equal), spawns the pre-roll thread; non-journaled. `emit` is the Tauri sink.
    /// The natural-stop position is read from `renderer.natural_end()` (cursor `end`, else project end).
    pub fn play_from<E: Fn(PlayheadUpdate) + Send + 'static>(
        &self, start: i64, renderer: Renderer<'static, CacheSourceProvider>, emit: E,
    ) -> Result<(), AudioError>;

    pub fn pause(&self) -> i64;  // stop pre-roll, retain position, no playback_stopped
    pub fn stop(&self) -> i64;   // stop + emit playback_stopped; idempotent
}

/// Events (proto mirror added in Step 11).
pub struct PlayheadUpdate { pub position_samples: i64 }
pub struct PlaybackStopped { pub position_samples: i64 }

const RING_MS: u64 = 200;
const RENDER_CHUNK_MS: u64 = 10;
const PLAYHEAD_INTERVAL_MS: u64 = 50;
```

## Sub-steps

### 9a — concrete `CacheSourceProvider`

- New `audio/source_provider.rs`. Implement `SourceProvider` for `CacheSourceProvider`: lazily open a
  `SymphoniaFrameReader` per track over `resampled/<id>.flac` for `dry`, an optional reader over the
  enhanced FLAC for `enhanced` (return `None` when the file is absent), return the pre-loaded
  `Arc<RoomTone>` slice for `room_tone`, and serve `channels`/`wet_ratio`/`source_len` from
  `TrackSource`. **No `Db`.** Independent of the RT machinery — lands first and de-risks 9b–9f;
  Steps 10/11 reuse it. (Routine; Sonnet.)

### 9b — `Backend` trait + ring + in-memory backend + no-alloc contract — **LANDED**

- Defined `Shared`, the two-phase `Backend` trait (`negotiate` + `start`), `InMemoryBackend` (with
  `pull` + forced-rate `negotiate`), `BackendKind::{Cpal, InMemory, InMemoryAtRate}`, and the SPSC
  ring split at `PlaybackEngine::new(sample_rate, backend, quality)` **sized to the negotiated device
  rate**. The drain/flush/silence contract (§ Detailed design A) is implemented once, shared by both
  backends. `PlaybackEngine` stores the `ResamplingQuality` (for the 9d resampler) and exposes
  `project_rate()` / `device_rate()`. No cpal yet.

### 9c — cpal output stream + device-rate negotiation

- `CpalBackend`: select default device; **`negotiate`** queries `supported_output_configs()`, prefers
  a stereo config at the project rate, and falls back to the device default rate when none matches
  (returns the chosen rate to size the ring). **`start`** builds the output stream at that rate with
  the §A contract in the data callback + an error callback, `stream.play()`, holds it alive. Gated
  behind the running app (headless/CI uses `InMemoryBackend`). Device *selection* / hot-swap stays
  out of scope (post-Phase 1); rate *adaptation* is in scope (it gates correct playback). (Sonnet.)

### 9d — pre-roll thread + resampler bridge + `Send`/`'static` renderer + shutdown/join

- Add the owned/`Arc` cursor constructor so the `Renderer` is `'static + Send` (§ B prerequisite;
  doc-sync the EDL/Step-8 doc). The `Renderer` itself implements `PcmSource` (project rate, stereo;
  `read` fills the caller's buffer, `is_exhausted` once its `EdlCursor` is spent at end_sample /
  end-of-EDL — the window rides in the cursor, no feed cap); feed it straight into
  `StreamingResampler(device_rate)` (passthrough when equal; `quality` from the engine's stored
  `ResamplingQuality`). Implement the
  pre-roll loop over the resampler output, back-pressure via `park_timeout`, device-frame `produced`
  tracking, the drain-wait (`frames_played == produced`, both device frames), and the
  `JoinHandle`/`stop_requested` shutdown. (Spec'd in § B; Sonnet implements literally — do not
  re-derive the concurrency model or the two-clock contract.)

### 9e — playhead position + events

- The callback bumps `frames_played` (device frames) for real frames only; the pre-roll thread emits
  `playhead_update` every `PLAYHEAD_INTERVAL_MS` of *played* audio from that atomic, converting device
  → project samples (`start + round(frames_played · project_rate / device_rate)`); assert
  played-not-rendered, no-emit-from-callback, and correct conversion under a forced device rate.
  (Routine; Sonnet.)

### 9f — `play_from` / `pause` / `stop` state + lifecycle

- Implement the state machine in § Detailed design C: session start with prior-join, inter-session
  flush via `playing`, pause-retains-position, stop-emits-once + idempotent, natural stop at
  end_sample/end-of-EDL, stream/ring reuse across cycles, clean `Drop`. (Spec'd in § C; Sonnet.)

### 9g — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings`; `cargo test -p core audio::`.
- Doc-sync: update [audio-pipeline.md § Output stream](../design/audio-pipeline.md#output-stream) to
  project-scoped (not app-start) + `RING_MS` fixed const + the device-rate negotiation / two-clock
  resampling model; confirm § Playback engine matches; record the played-vs-rendered-position +
  two-clock + injected-backend + concrete-provider decisions ([CLAUDE.md](../CLAUDE.md) doc-sync).
  Also update [phase1-m2-08.md](phase1-m2-08.md) "Out of scope" to point the concrete `SourceProvider`
  at 9a (done when this doc landed).
- One commit `1M2-09: playback engine (cache provider, cpal, ring buffer, events)` on `claude/1M2`,
  unsigned.

## Test cases (for the implementer)

Integration tests in `core/tests/` (driving `play_from` end to end) + inline unit tests, all over
the **in-memory backend** (no audio device). Groups map to sub-steps: V = provider (9a),
P = pre-roll/ring (9b/9d), E = events (9e), S = stop/pause (9f), R = real-time invariant, X = cross.

**V — concrete provider (9a)**

1. **Dry range == whole-file decode.** `dry(track, from, n)` equals the corresponding slice of a
   full decode of `resampled/<id>.flac` (reuse the `FR4` pattern) — sample-accurate.
2. **Enhanced absent → None.** With no enhanced file, `enhanced()` returns `None` (renderer uses dry).
3. **Room tone served from memory.** `room_tone(track)` returns the pre-loaded `Arc<RoomTone>` PCM;
   `None` when `room_tone_hash` was null. No `Db` touched.
4. **Metadata getters.** `channels`/`wet_ratio`/`source_len` match `TrackSource`; mono reports 1.
5. **End-to-end through the renderer.** A Step-8 `Renderer` over `CacheSourceProvider` for a synthetic
   `.vbdata` produces the expected PCM (ties 9a to the real pipeline before playback).

**P — pre-roll + ring buffer (9b/9d)**

6. **Frame sequence matches the renderer.** `play_from` over a synthetic single-track project, the
   in-memory backend drained to completion → captured frames equal the Step-8 `Renderer` output for
   the same range.
7. **Multi-track playback mixes.** Two overlapping tracks → captured frames == the mixed/clamped
   renderer output.
8. **Underrun → silence, never block.** Starve the producer and `pull` → silence (zeros), no block/
   panic, and playback resumes cleanly when the producer catches up.
9. **Ring is bounded / back-pressure.** With a slow consumer the producer does not grow unbounded
   (ring stays ≤ capacity; producer parks when full).
10. **Exact length.** Playing `[start, end)` feeds the renderer exactly `end − start` **project**
    frames before stop (no frames past `end`). At matched rates the device output is also `end −
    start` frames; under resampling it is the resampled count (see D-tests), with the stop position
    mapping back to `end_sample`.

**E — events (9e)**

11. **Playhead cadence.** `playhead_update` fires ~every `PLAYHEAD_INTERVAL_MS` of *played* audio;
    positions are monotonic and start at `start_sample`.
12. **Played, not rendered, position.** With `RING_MS` buffered ahead, the first `playhead_update`
    reports ≈ `start_sample`, not `start_sample + RING_MS` — proves the `frames_played` derivation.
13. **No event from the callback.** By construction/review + a test that the emit sink is only called
    from the pre-roll thread.

**S — stop / pause (9f)**

14. **Stop at `end_sample`.** Stops exactly when the played position reaches `end_sample`;
    `playback_stopped { position_samples == end_sample }` emitted once (after drain-wait).
15. **Stop at end-of-EDL.** `end == None` → stops at tree/project end; `playback_stopped` reports it.
16. **User stop mid-playback.** `stop()` halts promptly and emits `playback_stopped` with the last
    *played* position (not the rendered position).
17. **Pause retains position.** `pause()` returns the last played position and does **not** emit
    `playback_stopped`; a following `play_from` continues the frame sequence seamlessly.
18. **Stop is idempotent.** A second `stop()` does not double-emit or panic.
19. **Inter-session flush.** After a `stop` mid-stream, a new `play_from` plays only the new session's
    frames — no stale frames from the stopped ring leak in (proves the `playing`-flag flush).

**R — real-time invariant**

20. **Callback allocates nothing.** Structural/review assertion; optionally a custom-allocator
    counter around the drain path asserts **zero allocations** during a `pull`.
21. **Producer/consumer are SPSC.** Only the pre-roll thread writes; only the callback reads
    (type-enforced by `rtrb`'s split handles).

**X — cross-cutting**

22. **No SQLite on the audio path.** The pre-roll thread holds a `Renderer` + `CacheSourceProvider`
    only; no `Db`/journal in scope (continues the Step-7/8 invariant onto the RT path).
23. **Determinism.** The same project + range through the in-memory backend yields the same frame
    sequence + event positions, twice.
24. **Stream lifecycle.** `PlaybackEngine::new` opens once; repeated `play_from`/`stop` cycles reuse
    the same stream/ring (no per-play reopen); `Drop` closes cleanly.

**D — device-rate negotiation / resampling (9b/9c/9d, two-clock)**

25. **Passthrough negotiation.** `BackendKind::InMemory` ⇒ `device_rate() == project_rate()`; the
    ring is sized at that rate; the frame sequence is unchanged from P6 (no resampler artifacts).
    *(landed in 9b: `engine_in_memory_negotiates_passthrough`.)*
26. **Forced rate sizes ring at device rate.** `InMemoryAtRate(d)` with `d ≠ project_rate` ⇒
    `device_rate() == d` and the ring capacity follows `d`, not the project rate.
    *(landed in 9b: `engine_forced_rate_sizes_ring_at_device_rate`.)*
27. **Resampled frame count.** Playing `[start, end)` under a forced `d ≠ project_rate` delivers the
    `StreamingResampler` contract length (`≈ ceil((end−start) · d / project_rate)`) device frames —
    equal to draining `StreamingResampler(renderer, d)` over a renderer windowed to the same range.
28. **Playhead reports project samples under resampling.** With a forced `d`, the first
    `playhead_update` reports ≈ `start_sample` and subsequent ones advance in **project** samples
    (the device→project conversion), monotonically to `end_sample` at stop.
29. **Determinism under resampling.** Same project + range + forced `d` yields the same device-frame
    sequence + project-sample event positions, twice.

## Out of scope for Step 9

- **The proto `PlayheadUpdate`/`PlaybackStopped` payloads, `play_from`/`pause`/`stop` Tauri
  handlers, and constructing the `PlaybackEngine` + `CacheSourceProvider` at project open** — Step 11
  (Tauri wiring). Step 9 exposes the engine + provider + event structs; Step 11 mirrors them into
  `proto`, registers handlers, and `Arc`-clones the timeline snapshot to build the `'static`
  renderer.
- **Producing the enhanced FLAC** and **detecting/repairing a missing enhanced cache at project
  open** — M3 enhancement / open-time sweep.
- **Export** (offline render to file) — Step 10 reuses the Step-8 renderer + `CacheSourceProvider`
  directly, not the ring/stream.
- **Device *selection* and hot-swap, multi-device routing, sample-*format* (bit-depth) negotiation
  polish, and bundled-audio concerns** — post-Phase 1. Step 9 uses the **default** output device
  only. **In scope**, by contrast, is adapting to the default device's *sample rate* (negotiate +
  resample on the pre-roll thread) — without it, playback is broken on common hardware (44.1 kHz
  project on a 48 kHz device, Bluetooth at 16 kHz), so it is a correctness requirement, not polish.
