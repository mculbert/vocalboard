# Phase 1 · M2 — Audio Engine (action plan)

Step-by-step plan for the M2 milestone from [phase1.md](phase1.md). The authoritative
spec is [audio-pipeline.md](../design/audio-pipeline.md); playback/export touch
[command-surface.md](../design/command-surface.md) (`play_from` / `pause` / `stop`, `export_*`)
and the timeline tree from [data-model.md](../design/data-model.md). M2 runs **in parallel with
M3** ([phase1.md](phase1.md) critical path: M0 → M1 → M2/M3 → M4 → M5).

**Definition of done:** a Rust audio engine that (1) decodes source files to f32 PCM and
resamples them to a project-rate FLAC cache; (2) detects room tone and stores it as a
content-addressed blob; (3) builds a playback/export EDL by walking the M1 timeline tree
and tiling each turn's splice vec; (4) plays that EDL through `cpal` with a lock-free
ring buffer and emits `playhead_update` events; (5) exports track / mixed audio and
VTT / Markdown transcripts; and (6) provides the **low-level cut/mute primitives**
(zero-crossing search, crossfade, splice subdivision) that M5's editing commands will
call. Correctness is proven against synthetic turns + small committed audio fixtures —
**no ML, no import command** (that orchestration is M4).

## Scope note — low-level edit primitives move here from M5

Per the M2 planning decision (recorded in [phase1.md § M5](phase1.md#m5--editing-commands)):
the **zero-crossing search + crossfade** and the **splice-subdivision** transform are
low-level functions of the audio engine, not editing commands. They are signal-processing
code and belong with the rest of the audio engine, so M2 builds and tests them (Steps 6–7
below) against synthetic turns and PCM. **M5 owns only the `cut_words` / `mute_words` /
… *commands*** that call these primitives at the edit site. This keeps M5 focused on the
command surface, undo stamping, and overlap validation, with the DSP already proven.

## Decisions to lock first (recommended defaults)

- **New crate dependencies** (added to `core/Cargo.toml`; justified per
  [conventions.md](../design/conventions.md) I2; named in [ops.md § Rust crate dependencies](../design/ops.md#rust-crate-dependencies-key)):
  - `symphonia` with the codec/format features for the [audio-pipeline.md § Symphonia](../design/audio-pipeline.md#symphonia-primary)
    list (`wav`, `aiff`, `flac`, `isomp4` + `aac`/`alac`, `mp3`, `vorbis`/`ogg`) — primary decoder
    (AAC-LC and ALAC share the `isomp4` demuxer) **and** the FLAC decoder for the resampled
    cache on the playback read path.
  - `rubato` — sinc resampling (quality preset from `resampling_quality`).
  - A **FLAC encoder** for the 24-bit cache + default export: recommend `flacenc`
    (pure-Rust). Alternative `libflac-sys` if `cargo deny` rejects `flacenc`. Lock the
    choice at Step 1 against `deny.toml`.
  - `cpal` — output stream; a **lock-free SPSC ring buffer** crate (`rtrb`, or `ringbuf`).
  - Dev-only: small committed audio fixtures (WAV/FLAC/MP3, plus an AAC-LC `.m4a`) under `core/tests/fixtures/audio/`.
- **ffmpeg fallback is a subprocess; bundling is M7.** In dev/CI, detect a system `ffmpeg`
  on `PATH`; if absent, the fallback path is skipped (and its tests are `#[ignore]`d with a
  note). Bundling the LGPL build + `tauri.conf.json` wiring stays in M7
  ([ops.md](../design/ops.md)). The fallback is reserved for what Symphonia cannot decode —
  HE-AAC/Opus/AC-3/DTS and video carrying a non-AAC-LC audio stream; **AAC-LC/M4A
  decodes on the Symphonia path in M2** ([audio-pipeline.md § ffmpeg](../design/audio-pipeline.md#ffmpeg-fallback)).
- **The resample cache is filled by a Rust background task**, *not* the M3 ML
  `TaskQueue` (that queue is for Python dispatch only). Use a spawned thread / `tokio`
  task, mirroring the M1 snapshot writer's own-connection posture. M2 builds and tests
  the resample function + cache read/write + regenerate-if-missing directly; M4 wires the
  import-time and open-time callers (see Step 3).
- **The FLAC cache is the uniform EDL read source for *every* track, not just resampled
  ones** (per [audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling)): one cheap,
  seekable, codec-agnostic, sample-accurate decode path off the real-time pre-roll thread,
  and the canonical *dry* signal for the wet/dry blend. At-rate sources skip the resample
  compute (identity fast-path) but are still transcoded. Avoiding the duplicate-disk cost
  for already-at-rate lossless sources is a **Phase 2** revisit, not Phase 1.
- **The `cpal` output stream is opened when a project opens** (not app start) and kept alive
  for that project session as Tauri managed state (per [audio-pipeline.md § Output
  stream](../design/audio-pipeline.md#output-stream)), configured for the project rate, 2-ch — config and
  ring size both depend on the locked rate. Structure it so a second project window (Phase 6)
  owns its own stream.
- **Real-time discipline is a hard invariant** ([CLAUDE.md](../CLAUDE.md)): the `cpal`
  callback only *drains* the pre-allocated ring buffer — no allocation, locking, or
  blocking I/O. All EDL rendering + FLAC decode happen on the pre-roll thread. Document
  this at every callback/handoff boundary; it is a review gate, not CI-enforceable.
- **No SQLite schema / Turn / TrackMeta format change in M2** (all fixed in M1), so no
  `user_version` bump. **Exception:** the **room-tone PCM blob** is the first use of the
  reserved `Kind::RoomTone = 0x4` format tag — it ships a V1 wire schema with pinned
  bytes + a G1 fixture round-trip ([conventions.md](../design/conventions.md) G1), exactly like the
  M1 Turn/Label/Metadata blobs.
- **Audio export formats:** ship **FLAC** (default, `flacenc`) and **WAV** (native) in M2.
  `mp3` / `ogg` / `aac` from the `export_track` schema route through the ffmpeg subprocess
  when available, else return `export_unsupported_format`; revisit bundled-encoder support
  in M7. Lock this at Step 1.
- **Working branch:** `claude/1M2` (commits unsigned per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md)); squash-merged to `main` via PR. Numbered sub-step commits
  (`1M2-01 …`) mirroring M0/M1.

## Module layout (within `src-tauri/core/`)

```
audio/
  mod.rs          re-exports; AudioError (typed, message-key errors per conventions)
  decode.rs       Symphonia decode → f32 PCM @ source rate; probe (codec/rate/channels/len)
  ffmpeg.rs       ffmpeg subprocess PCM pipe + availability detection (fallback decoder)
  resample.rs     rubato sinc resample → project rate; quality preset from settings
  flac.rs         24-bit FLAC encode (cache + export); FLAC decode via symphonia
  cache.rs        resampled cache path + background fill + regenerate-if-missing
  room_tone.rs    RMS sweep, window search, stitch fallback, loop-crossfade tiers;
                  RoomTone V1 blob (store/load, kind 0x4)
  zero_crossing.rs  frame-based backward/forward zero-crossing + local-RMS search; min-energy
                  fallback; inline crossfade gain (low-level edit primitive — M5 consumer)
  splice.rs       subdivide_on_cut / subdivide_on_mute (forward) + merge_on_uncut /
                  merge_on_unmute (inverse, coalescing) → new splice vec  (low-level edit
                  primitive — M5 consumer; initial 1-Source EDL built inline at M4 import)
  edl.rs          EdlSegment (slim/vertical: track_id + pristine splice + offset_in_splice);
                  MixSlice (horizontal: start_sample/length_samples + one segment per active
                  track); TrackCursor — a seekable per-track walk (prefix-sum tiling, lead-in
                  gap) over the timeline tree via iter_from; EdlCursor — merges TrackCursors
                  into a position-ordered stream of sample-aligned MixSlices. Transient
                  (operative only while a consumer pulls); NOT a persisted structure — the
                  per-turn splice vecs remain the single source of truth.
  render.rs       EdlCursor (MixSlices) + source readers → f32 frames; room-tone loop; centered
                  equal-power seam crossfades (handles + shared fade accumulator); wet/dry blend;
                  multi-track mix + clamp to [-1,1]  (shared by playback+export)
  playback.rs     cpal stream lifecycle; lock-free ring buffer; pre-roll thread;
                  playhead_update / playback_stopped events; play_from / pause / stop
  export.rs       track/mixed via render → encoder sink; transcript VTT/Markdown; export_* cmds
```

`app/main.rs` gains the managed `cpal` output stream (opened at startup) and the
playback/export command handlers. `proto` gains the `play_from` / `export_*` param/result
types and the `playhead_update` / `playback_stopped` event payloads; `types.ts` is
regenerated and `src/lib/ipc/commands.ts` gains the wrappers (no UI — consistent with M1).

Build the read-side EDL + render core before the real-time playback wrapper, and prove
each DSP function against synthetic data before wiring commands.

---

## Step 1 — Action-plan doc, branch, dependencies

- This document; create the `claude/1M2` branch.
- Add the Step-0 crates to `core/Cargo.toml` (symphonia + features, rubato, FLAC encoder,
  cpal, ring buffer); add audio fixtures as dev assets. Lock the FLAC-encoder and
  export-format decisions above.
- **Verify:** `cargo build` green; `cargo deny check` passes for the new deps (license +
  advisory policy in `deny.toml`).

## Step 2 — Decoding + probe (`audio/decode.rs`, `audio/ffmpeg.rs`)

*Novelty: medium — mechanical, but format edge cases. Sub-step plan doc (`phase1-m2-02.md`).*

- Symphonia decode of the [audio-pipeline.md § Symphonia](../design/audio-pipeline.md#symphonia-primary)
  formats to **interleaved f32 PCM at the source rate**, plus a `probe()` returning codec,
  source sample rate, channel count, and length (feeds `TrackMeta` at M4 import).
- `ffmpeg.rs`: subprocess pipe extracting raw PCM for HE-AAC/Opus/AC-3/unsupported formats
  (**not** AAC-LC, which Symphonia handles); a cheap `ffmpeg_available()` probe. Fallback is
  invoked only when Symphonia rejects the file.
- **Verify:** decode each committed fixture (WAV/FLAC/MP3/AAC-LC `.m4a`) to PCM of the expected
  length / channel count; probe returns correct metadata; an unsupported file routes to the
  ffmpeg path (test `#[ignore]`d when no system ffmpeg). Decode is deterministic for a given input.

## Step 3 — Resample + FLAC cache (`audio/resample.rs`, `audio/flac.rs`, `audio/cache.rs`)

*Novelty: medium. Sub-step plan doc (`phase1-m2-03.md`).*

- `resample.rs`: rubato sinc resampling source-rate → project-rate, quality preset from the
  `resampling_quality` setting; **identity fast-path when rates match** (skip the rubato pass
  but still transcode to the cache — the cache is the uniform read source for *all* tracks,
  not only resampled ones; see [audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling)).
- `flac.rs`: 24-bit integer FLAC **encode** (cache write + default export) and **decode**
  (via symphonia) back to f32. Document the ≈ −144 dB quantization trade per
  [audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling).
- `cache.rs`: resolve `<project>.vbdata/resampled/<track_id>.flac` (keyed by the stable
  `TrackMeta.id`) and a callable
  **`ensure_resampled(track, dir, settings)`** — the **background fill** (Rust thread/`tokio`,
  not the M3 ML queue) that decodes → resamples → writes the cache and **returns** the derived
  cache path (`resampled/<id>.flac`, not stored in `TrackMeta`), **regenerating it when the file
  is missing** (same posture as a missing enhanced track). The callable itself holds no `Db` connection and does
  **not** journal the metadata — the M4 caller does (mirroring the M1 snapshot writer's
  own-connection background posture). M2 owns this *callable* and tests it directly; the
  **open-time trigger** that sweeps every track and the **import-time** call are wired in M4.
- **Verify:** resample a fixture to a different rate (length scales correctly; identity
  path is bit-exact); FLAC encode→decode round-trips f32 within the 24-bit quantization
  bound; cache writes the expected path and regenerates after deletion.

## Step 4 — Room-tone detection (`audio/room_tone.rs`)

*Novelty: medium-high — a multi-criterion algorithm + a new persisted blob. Full sub-step
plan doc (`phase1-m2-04.md`).*

- Implement [audio-pipeline.md § Room tone detection](../design/audio-pipeline.md#room-tone-detection)
  exactly: skip recordings < 10 s; 100 ms RMS blocks on the **mono down-mix**; quiet threshold
  `Q = min(rms_ceiling, Pq)` from the configurable `room_tone_rms_ceiling` (default 0.0316) and
  `room_tone_quiet_percentile` (default 5) settings, passed in as `RoomToneParams`; longest low-RMS
  window (target 5–10 s, min 2 s) accepted iff window RMS ≤ `rms_ceiling` **and** peak ≤ 5× window
  RMS **and** block-RMS SD ≤ 15% of mean; stitch
  100–300 ms quiet (≤ `Q`) segments with 50 ms crossfades when no 2 s window qualifies; apply the
  loop crossfade by length tier (**< 500 ms → 50 ms; 500 ms–2 s → 100 ms; > 2 s → 500 ms**) so the
  stored segment loops seamlessly. The extracted segment **preserves the source channel count**
  (mono→mono, stereo→stereo).
- Store the resulting f32 PCM as the **`Kind::RoomTone` (0x4) V1 blob** (the first use of
  that reserved tag): `encode_room_tone` / `decode_room_tone` (serialization only, Turn pattern —
  no store/load wrapper), `LATEST_ROOM_TONE_VERSION`, frozen `v1` wire schema
  `{ sample_rate, channels, samples }`. The **M4** import caller writes the blob + journals
  `TrackMeta.room_tone_hash` (the per-channel frame count is derived from the loaded blob, not
  stored); **Step 8** reads it back lazily at render time.
- **Verify:** synthetic signals exercise each branch (clean 2 s window accepted; noisy window
  rejected → stitch fallback; each crossfade tier); the looped segment is C⁰-continuous at the
  wrap; **pinned-bytes + pinned-hash** tests for the RoomTone V1 blob and a **G1 fixture
  round-trip** (per [conventions.md](../design/conventions.md) G1).

## Step 5 — Zero-crossing search + crossfade (`audio/zero_crossing.rs`) — *moved from M5*

*Novelty: medium — well-specified, but load-bearing for M5. Sub-step plan doc (`phase1-m2-05.md`).*

- Implement [audio-pipeline.md § Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade):
  backward search ≤ `splice_search_window_ms` (default 20 ms) before word onset and forward search
  after word offset for a frame where local RMS `< max(0.001, min(2 * room_tone_rms,
  room_tone_rms_ceiling))`; **min-local-energy fallback** when no crossing qualifies; a configurable
  (`splice_crossfade_ms`, default 2 ms) crossfade at the boundary — recorded as the seam fade length
  and rendered as a **centered equal-power** overlay in Step 8 via the `equal_power_gain` helper
  (shared with the room-tone stitch + loop fold — all fades in the engine are equal-power; there is
  no linear fade-gain helper). Timings (ms) are
  converted to integer frames once into a `ZeroCrossingParams` the caller resolves from settings +
  the project rate; the ceiling reuses `room_tone_rms_ceiling`.
- Pure functions over an interleaved f32 PCM slice + a candidate boundary, taking the track's
  `channels`; they return the refined onset/offset **frame indices** (which become the word's
  precise `source_onset_sample` / `length_samples` — lazily at M5 edit time, eagerly at M4 import for
  each turn's first word). `refine_onset` / `refine_offset`
  wrap one direction-parameterised `refine_boundary`; the seam crossfade gain is the inline
  `equal_power_gain` (no allocated ramp). No tree or DB coupling.
- **Verify:** finds the expected crossing in a synthetic tone-in-noise slice; honours the
  search-window bound; falls back to min-energy when no crossing exists; a stereo slice returns a
  frame-aligned index; the crossfade gain is equal-power (constant power). These tests are the contract
  M5 depends on.

## Step 6 — Splice subdivision + merge (`audio/splice.rs`) — *moved from M5*

*Novelty: high — recomputes immutable Turn splice vecs; the write side of the per-turn EDL.
Full sub-step plan doc (`phase1-m2-06.md`).*

- `subdivide_on_cut(splices, start, end, crossfade_samples)` / `subdivide_on_mute(splices, start,
  end, mute_to_room_tone, crossfade_samples)`: given the splice vec and a caller-resolved removal/
  mute **span** (in current-vec coordinates) plus the seam crossfade length **in samples**, return a
  new splice vec per [audio-pipeline.md § Updating on cut / mute](../design/audio-pipeline.md#updating-on-cut--mute-and-uncut--unmute)
  — cut removes the span shrinking the turn; mute replaces it with a `RoomTone` (or `Silence`) splice.
  The span may cross any boundaries/kinds: the splice containing `start` is trimmed to its head, the
  splice containing `end` to its (source-rebased) tail, and the new seam(s) carry the crossfade.
- `merge_on_uncut(...)` / `merge_on_unmute(...)`: the **inverse** — re-insert a `Source` splice
  (reading the word's stored `source_onset_sample`) over the shared `merge_span` helper, **coalescing
  inline** source-contiguous `Source`s and adjacent `RoomTone`/`Silence` runs (single sweep), which
  returns a within-splice edit's vec to its pre-edit shape (canonical form ⇒ blob reuse).
- These are **pure functions over scalars + `&[Splice]`** (no `Word`, no settings, no `rate`); the
  caller (M5) re-stores the new immutable `Turn`. The initial 1-`Source` EDL is built inline at M4
  import. **Prerequisite:** the `Word::turn_offset_sample → source_onset_sample: Option<i64>` rename
  (data-model.md) + V1-wire/pinned-fixture regeneration (pre-1.0).
- **The splices stay self-consistent with `turn_duration` + `post_turn_silence`** (the tiling
  invariant; splices carry no stored offset).
- **Verify:** cut/mute first / middle / last word; cut-with-following-silence; mute-to-silence vs
  mute-to-room-tone; uncut/unmute round-trip back to the identical pre-edit vec; the tiling-sum
  invariant after each op; a multi-cut sequence converges to the expected splice vec. (No command,
  no journaling here — that is M5.)

## Step 7 — EDL cursor (`audio/edl.rs`)

*Novelty: high — the read-side core of both playback and export. Full sub-step plan doc
(`phase1-m2-07.md`); pre-split into 7a / 7b.*

> **The cursor is a transient iterator, not a persisted structure.** The per-turn splice
> vecs remain the single source of truth for edit decisions; the cursor is a seekable,
> pull-based view that *resolves* them into render-ready descriptors only while a consumer is
> reading. It does three things the splices alone don't: resolves the implicit positions to
> absolute project samples, synthesizes a **lead-in** gap before a track's first content
> (inter-turn silence is already part of each turn's splice tiling, so it is not
> re-synthesized), and **merges all tracks into sample-aligned mix slices**. It yields
> lazily (no materialized whole-timeline list) so it scales to multi-hour projects and feeds
> the real-time pre-roll thread cheaply. The data model splits **vertical** (`EdlSegment` =
> one track's windowed reference to one pristine splice) from **horizontal** (`MixSlice` =
> one span of the merged timeline, carrying one `EdlSegment` per active track).

- **7a — per-track walk.** `EdlSegment` (slim/vertical: `{ track_id, splice, offset_in_splice }`
  — a window into one **pristine** splice, no position/length); `tree.iter_from(sample)` adds a
  **seeking** in-order tree walk (the per-track walk can't build a mid-tree iterator from
  outside `project::tree`), and a `TrackCursor::at(tree, track_id, project_start_sample, start)`
  yields the track's segments in order by **prefix-summing the turn's splice `length_samples`**
  (splices carry no offset), emitting a synthetic lead-in `Silence` when `project_start_sample
  > start`. Per [audio-pipeline.md § Building the EDL](../design/audio-pipeline.md#building-the-playback--export-edl).
- **7b — merge.** `EdlCursor::new(tracks, start, end)` advances all `TrackCursor`s together
  from one shared running position in **sample-aligned lockstep**: each `next()` emits a
  `MixSlice` over `[pos, pos + min-run-length)` (clamped to `end`) carrying one `EdlSegment`
  per active track (ascending `track_id`). The cursor owns **boundary alignment**; `render.rs`
  owns the f32 sum. Honour each track's `project_start_sample`.
- **Verify:** a single-track `EdlCursor` over a synthetic tree yields the expected ordered
  `MixSlice`s and total length; seeking to a mid-splice `start` stamps `start_sample`/
  `offset_in_splice` correctly and a bounded `[start, end)` walk clamps the final slice;
  overlapping turns across two tracks merge into position-ordered slices with one segment per
  track; empty range / start-past-end / single-turn boundary cases. The cursor holds **no
  SQLite connection** while iterating.

## Step 8 — EDL renderer (`audio/render.rs`)

*Novelty: high — turns cursor segments into samples; shared by playback and export. Full
sub-step plan doc (`phase1-m2-08.md`).*

- A pull-based renderer: given an `EdlCursor` (Step 7) + lazily-opened source readers (resampled
  cache FLAC; room-tone blob; enhanced FLAC for wet/dry), produce f32 frames for a requested
  range by consuming segments from the cursor on demand.
- Room-tone splices **loop the pre-crossfaded stored segment**; the gap crossfade where the tone
  meets its neighbours is **the RoomTone splice's own stamped fade**, applied by the same seam
  machinery as any splice (no separate gap-fade mechanism, no fade constant) — per
  [audio-pipeline.md § Room tone substitution](../design/audio-pipeline.md#room-tone-substitution).
- **Centered equal-power seam crossfades**, recorded as splice `fade_in`/`fade_out` (the edit
  crossfade and the room-tone gap fade alike): a **render-time overlay** that reads source *handles*
  across the seam (the tiling/positions stay authoritative) and is carried by a single, shared
  project-wide fade accumulator. **All** fade lengths are splice data — no renderer constants — and
  the renderer degrades gracefully when a handle is short/absent (per
  [audio-pipeline.md § Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade)).
- **Full wet/dry blend** at splice-read time: enhanced FLAC × `wet_ratio` + resampled cache ×
  `(1 - wet_ratio)`, both at project rate (per [ml-pipeline.md § Enhancement](../design/ml-pipeline.md#enhancement-pipeline-mp-senet)).
  Implemented now (not stubbed); a missing enhanced file ⇒ dry **even when `wet_ratio > 0`** (no
  inline regen — that is an open-time/M3 concern). Today `wet_ratio` is always 0 and `enhanced()`
  always `None`, but the renderer is coded for any ratio and for cache hits.
- **Multi-track mix**: sum overlapping tracks (+ the seam-fade accumulator), **clamp to [−1, 1]**
  after mixing; mono→stereo up-mix with equal gain.
- **Verify:** render a known segment sequence to PCM and assert sample-exact output for
  Source / Silence / RoomTone segments; a symmetric seam crossfade is **constant-power** (no dip) with
  handles read across the seam; the room-tone gap fade is the splice fade (no 50 ms constant); two
  overlapping tracks sum + clamp as expected; wet/dry blend at ratio 0 / 0.5 / 1 matches the formula
  and a missing enhanced file falls back to dry. Renderer holds **no SQLite connection** while
  producing frames (sets up the real-time invariant for Step 9).

## Step 9 — Playback engine (`audio/playback.rs`)

*Novelty: high — real-time, lock-free, `cpal` callback constraints (the trickiest M2 step).
Full sub-step plan doc (`phase1-m2-09.md`); split into 9a–9g. The concurrency/RT spine (ring +
backend contract, pre-roll thread + shutdown/join, stop state machine) is **designed in detail in
that doc** so Sonnet implements every sub-step from the spec; also folds in the concrete
`SourceProvider`, which Step 8 left to "Step 9/10".*

- **9a — concrete `CacheSourceProvider`.** The real `SourceProvider` over the `.vbdata` cache: a thin
  adapter over the existing seekable `SymphoniaFrameReader` (`dry`/`enhanced`) plus pre-decoded room
  tone; no `Db`. Reused by Steps 10/11.
- **9b — `Backend` trait + ring + no-alloc contract.** Pre-allocated lock-free **SPSC** ring
  (`rtrb`) split producer/consumer; the drain/flush/silence callback contract; an in-memory backend
  for headless tests.
- **9c — cpal output stream.** Opened **when a project opens** (project rate, stereo, fixed
  `RING_MS` ≈ 200 ms buffer per [audio-pipeline.md § Ring buffer](../design/audio-pipeline.md#ring-buffer)) —
  *not* app start, since config + ring size depend on the locked rate. Callback **only drains** (no
  alloc / lock / blocking I/O); underrun → silence, never block.
- **9d — pre-roll thread + shutdown.** A `Send`/`'static` renderer (owned/`Arc` cursor) on a
  pre-roll thread that fills the ring with back-pressure and tracks render position; clean
  shutdown/join protocol.
- **9e — playhead events.** `playhead_update { position_samples }` every ~50 ms from the pre-roll
  thread, derived from *played* (not rendered) frames; never emitted from the callback.
- **9f — `play_from`/`pause`/`stop` state.** Stop on `end_sample` / end-of-EDL / user stop (per
  [audio-pipeline.md § Playback stops](../design/audio-pipeline.md#playback-stops)); `playback_stopped`
  emitted once; pause retains position; idempotent stop; stream/ring reused across cycles. The
  commands are **non-journaled** (frontend resolves scope → `[start, end)`).
- **9g — final pass.** fmt/clippy/test + doc-sync.
- **Verify:** integration tests drive `play_from` over a synthetic project with the in-memory
  backend and assert the rendered frame sequence + `playhead_update` cadence; stop conditions fire at
  the right sample; pause retains position, stop reports the last position; inter-session flush leaks
  no stale frames. Assert (by construction + review) the callback path performs no allocation.

## Step 10 — Export (`audio/export.rs`)

*Novelty: medium — reuses the Step-8 renderer + a tree read for transcript. Sub-step plan doc (`phase1-m2-10.md`).*

- **Audio export** (`export_track` / `export_mixed`): build the EDL for the requested
  track(s), pad with silence to project length, render via Step 8, write to the user path via
  an encoder sink — **FLAC** (default) / **WAV** in M2; `mp3`/`ogg`/`aac` via ffmpeg when
  available, else `export_unsupported_format`. Optional mono collapse (sum / 2). Exports are
  **not** cached in `.vbdata/`. Per [audio-pipeline.md § Export pipeline](../design/audio-pipeline.md#export-pipeline).
- **Transcript export** (`export_transcript`): read the timeline tree directly and format
  **VTT** + **Markdown** (Word/RTF deferred), honouring `include_cut_words`. Format chosen by
  output-file extension; unknown extension → `export_unsupported_format`.
- **Verify:** exported FLAC/WAV decode back to the renderer's expected PCM; mono collapse
  halves channels; VTT/Markdown match a pinned expected string for a synthetic transcript;
  unsupported extension returns the right error code.

## Step 11 — Tauri wiring + contract + final pass

*Novelty: medium — follows the M1 Step 12 pattern. Sub-step plan doc (`phase1-m2-11.md`).*

- Add the `proto` param/result types for `play_from` / `pause` / `stop` / `export_track` /
  `export_mixed` / `export_transcript` and the `playhead_update` / `playback_stopped` event
  payloads (`#[serde(deny_unknown_fields)]`, value-constraint guards, version-by-name per
  [command-surface.md § H1/J2](../design/command-surface.md#tauri-command-boundary--versioning-mechanism-h1)).
  Add the `ProjectState` read-accessors (trees / track + speaker metadata / vbdata dir) the
  handlers need; register the `#[tauri::command]` handlers; construct the managed `PlaybackEngine`
  **when a project opens** (not app start — its stream config + ring size depend on the project's
  locked rate), with a non-fatal device-open path. Regenerate TS bindings + `commands.ts` wrappers.
- Run the full gate.
- **Verify:** `cargo run -p proto --features ts-export --bin gen_bindings -- --check`,
  `cargo fmt --check`, `cargo clippy -- -D warnings` (incl. `missing_docs`, `unwrap_used`),
  `cargo test --workspace`, `pnpm check && pnpm test && pnpm build` all green.

## Testing strategy (synthetic + small fixtures; no ML)

- Inline `#[cfg(test)]` unit tests per `audio/` module hitting boundary cases (empty / single
  / overlapping / gaps / each algorithm branch) — [conventions.md](../design/conventions.md) A1.
- **Committed audio fixtures** (tiny WAV/FLAC/MP3) under `core/tests/fixtures/audio/` for
  decode/resample/cache round-trips; an ffmpeg-fallback test `#[ignore]`d when no system
  ffmpeg.
- **Cross-cutting integration tests** in `core/tests/`: EDL → render → (in-memory sink)
  frame-exactness; a `play_from` drive test with a mock sink; export round-trips.
- **Determinism** tests for decode/resample/render (same input → same bytes) — the
  content-addressing and reproducible-export posture.
- **G1 fixture** round-trip for the new RoomTone blob (Step 4).
- Synthetic turns/PCM are built via test-only helpers (no import command exists until M4).

## M2 exit criteria

- `cargo test --workspace` (unit + integration + fixtures), `cargo clippy -- -D warnings`,
  `cargo fmt --check` all green locally and in CI.
- `play_from` / `pause` / `stop` and `export_track` / `export_mixed` / `export_transcript`
  round-trip through Tauri with regenerated, in-sync TS bindings; `pnpm check && pnpm build`
  green.
- The zero-crossing, crossfade, and splice-subdivision primitives are implemented and tested,
  ready for M5's editing commands to call.
- [audio-pipeline.md](../design/audio-pipeline.md) stays authoritative — any field/behaviour adjusted
  during implementation is updated there in the same commit.

> **Deferred to later milestones:** bundling the ffmpeg binary + per-platform encoder support
> for `mp3`/`ogg`/`aac` (M7); Word/RTF transcript export (post-Phase 1); the
> `import_speech_track` orchestration that *drives* decode/resample/room-tone/initial-EDL
> (M4); the `cut_words` / `mute_words` / … **commands** that call the Step 5–6 primitives at
> the edit site, with undo stamping + overlap validation (M5).
</content>
</invoke>
