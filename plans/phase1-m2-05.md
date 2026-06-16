# Phase 1 · M2 · Step 5 — Zero-crossing search + crossfade (action plan)

Per-step action plan for Step 5 of the M2 milestone from [phase1-m2.md](phase1-m2.md) — a
primitive **moved from M5** into the audio engine because it is signal processing, not a command.
The authoritative spec is [audio-pipeline.md § Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade).

This step lands the **boundary-refinement primitive** the editing commands (M5 `cut_words` /
`mute_words`) will call: given a word's approximate onset/offset (a **frame index**) in a track's
interleaved PCM, find a clean low-energy boundary near it and provide the linear crossfade gain
applied at a splice seam. These are **pure functions over an f32 slice + candidate boundary** — no
tree, no DB, no command, no journaling. M5 owns the commands that call them and write the refined
offsets into the word's `source_onset_sample` (`Option<i64>`, an absolute source/cache offset) /
`length_samples`. Refinement is **lazy and per-seam** (a cut refines word *i* and the following
word's onset), so most words stay `source_onset_sample == None`; the **one eager exception is each
turn's first word, refined at M4 import** to fix the turn origin/boundaries (see
[audio-pipeline.md § Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade)).
These pure search functions are unchanged by that policy — they return refined frame indices to
whichever caller (M4 import or M5 edit) invokes them.

**Definition of done:** `core/src/audio/zero_crossing.rs` exposes `refine_onset` /
`refine_offset` (thin wrappers over one direction-parameterised `refine_boundary`, returning
refined **frame** indices) plus `frames_from_ms` + an inline `crossfade_gain` helper; all implement
the spec exactly (configurable search window + crossfade/local-RMS window, the RMS threshold, the
min-energy fallback, the unity-summing ramp); the timing thresholds come from settings via a
`ZeroCrossingParams` the caller resolves (the audio engine reads no settings directly, mirroring
Step 4's `RoomToneParams`); the test matrix below passes; `cargo test -p core audio::`,
`cargo clippy -p core -- -D warnings`, `cargo fmt --check` green. **These tests are the contract
M5 depends on.**

## Decisions locked in this step

- **The acceptance threshold is the spec formula**, parameterised by the track's room-tone RMS:
  `thresh = max(0.001, min(2.0 * room_tone_rms, params.rms_ceiling))`. The caller passes
  `room_tone_rms` (computed from the Step-4 segment, or `0.0` when the track has no room tone →
  threshold floors at `0.001`). `params.rms_ceiling` **reuses the existing `room_tone_rms_ceiling`
  setting** (default `0.0316` ≈ −30 dBFS) — Step 4 already documents this ceiling as "the same
  constant the zero-crossing search clamps quiet to," so no new ceiling setting is introduced. The
  `0.001` floor and the `2.0` multiplier remain **fixed constants** (only the ceiling and the two
  timings are configurable).
- **The two timings are app settings stored in ms, converted to integer frames once.** The
  persisted keys `splice_search_window_ms` (default 20) and `splice_crossfade_ms` (default 2) live in
  [settings.rs](../src-tauri/core/src/settings.rs) / [data-model.md § App settings](../design/data-model.md#app-settings)
  — ms is the right *persisted* form (rate-independent and human-meaningful). But `ZeroCrossingParams`
  carries **integer frame counts** (`search_window_frames`, `crossfade_frames`), per the
  all-time-is-integer-samples invariant: `ZeroCrossingParams::from_settings(rms_ceiling,
  search_window_ms, crossfade_ms, rate)` rounds `frames_from_ms` **once** at resolve time (the
  project rate is locked at create), so no float math or `× rate` happens in the search loop. The
  caller (M5/render) resolves it from settings + the project rate (the audio engine reads no settings
  directly, mirroring `RoomToneParams` / `resampling_quality`).
- **"Local RMS at a sample"** is the RMS over a short window centred on the candidate, of length
  `crossfade_frames` — **the same length as the crossfade, driven by the one setting** (the spec
  leaves "local RMS" unpinned; this couples the two deliberately). Clamp the window at slice ends.
- **Frame-based API + `channels` parameter (mono/stereo).** `pcm` is interleaved at the source
  channel count; `approx_*` inputs and the returned index are **frame indices** (per-channel sample
  positions), matching the frame-based word offsets M5 reads/writes. Because the search steps in
  frames, the result is inherently frame-aligned — a stereo cut lands on a frame boundary with no
  "even index" bookkeeping at the call site. Local RMS over a frame window sums squares across **all
  channels** (`crossfade_frames × channels` interleaved samples), matching the mono down-mix Step-4
  detection uses. The search radius is `params.search_window_frames`; bounds are clamped to
  `[0, n_frames - 1]` where `n_frames = pcm.len() / channels`.
- **Onset search direction:** search **backwards** from the word's approximate onset frame, up to
  `search_window_frames` earlier, for the **first** frame whose local RMS `< thresh`; that frame
  becomes the refined onset. **Offset search:** search **forwards** for the first qualifying frame.
  (Searching outward keeps the cut from clipping into speech.) One private
  `refine_boundary(pcm, channels, approx_frame, room_tone_rms, params, dir)` implements the scan
  (no `rate` — the window sizes are already frames); `refine_onset` / `refine_offset` are thin
  public wrappers passing `SearchDir::Backward` / `Forward` — the algorithm is written **once**.
- **Min-energy fallback:** when no frame in the search window satisfies the threshold, return the
  frame **within the window** with the **minimum local RMS** (not the window edge) — the least-bad
  boundary. Never fail; never return a position outside the search window.
- **The crossfade is an inline gain, not an allocated ramp.** Splices happen a *lot*, so instead of
  materialising `Vec<f32>` ramps, expose an `#[inline]` `crossfade_gain(i, len) -> f32` that returns
  the fade-**in** weight `i / (len - 1)` at a single index (`len` = `crossfade_frames`); the renderer
  (Step 8) / M5 compute `fade_out = 1.0 - gain` and fold both into their seam loop with zero
  allocation. Unity-sum (`gain + (1 - gain) == 1.0`) is then **exact**, not within-epsilon. Handle
  the `len <= 1` degenerate (very low rate) without divide-by-zero (return `1.0`). The ms→frames
  conversion is the shared `frames_from_ms(ms, rate)` helper, used by `from_settings` (here) and by
  Step 6 to stamp `fade_*_samples` — so the rounding rule lives in one place.
  - **Superseded (M2 room-tone / renderer work).** `crossfade_gain` was renamed to the equal-power
    `equal_power_gain` (still inline in `audio/mod.rs`, with its unit tests co-located there): all
    engine crossfades — edit seams *and* the room-tone stitch + loop fold — are now equal-power
    (`sin`/`cos`, constant power for uncorrelated material), and the linear form plus its
    unity-sum / equal-step tests (5c and cases 14–17 below) were removed. The Step-5 deliverable (the
    zero-crossing **search**) is unchanged; the linear specifics in this bullet, in *5c*, and in the
    module surface describe the original form and are kept for the historical record.
- **All offsets are frame indices into the supplied slice.** The caller maps them to source/turn
  coordinates; this primitive has no notion of turns.
- **No allocation concerns** — pure compute on borrowed slices; the search returns `usize` and
  `crossfade_gain` returns a scalar, so nothing allocates on the edit/render path.

## Module surface

```rust
// audio/zero_crossing.rs

/// Boundary-refinement + crossfade timings in **integer frames**, resolved once from
/// app settings (ms) + the project rate by the M5/render caller — the audio engine
/// reads no settings directly. No `Default`: frame counts depend on the project rate,
/// so callers (and tests) build it via `from_settings`.
pub struct ZeroCrossingParams {
    /// Hard RMS ceiling for the "quiet" threshold; reuses `room_tone_rms_ceiling`.
    pub rms_ceiling: f32,
    /// Outward search radius in frames (from `splice_search_window_ms`).
    pub search_window_frames: usize,
    /// Crossfade length in frames; also the local-RMS window (from `splice_crossfade_ms`).
    pub crossfade_frames: usize,
}

impl ZeroCrossingParams {
    /// Resolve from the raw settings (ms) + project sample rate, rounding both
    /// timings to whole frames once (`frames_from_ms`).
    pub fn from_settings(
        rms_ceiling: f32,
        search_window_ms: f64,
        crossfade_ms: f64,
        rate: u32,
    ) -> Self;
}

/// Convert a millisecond duration to whole frames at `rate`: `round(ms / 1000 * rate)`.
/// The single home for the ms→frames rule (used by `from_settings` and Step 6's
/// `fade_*_samples` stamping). Returns `0` for a non-positive `ms`.
pub fn frames_from_ms(ms: f64, rate: u32) -> usize;

/// Search direction for `refine_boundary`. `Backward` for onsets, `Forward` for offsets.
enum SearchDir { Backward, Forward }

/// Refine a word onset: search backwards from `approx_onset_frame` up to
/// `params.search_window_frames` for the first frame whose local RMS < threshold(room_tone_rms);
/// else the minimum-local-RMS frame in the window. `pcm` is interleaved at `channels`;
/// indices are frame indices. Thin wrapper over `refine_boundary(.., Backward)`.
pub fn refine_onset(
    pcm: &[f32],
    channels: u16,
    approx_onset_frame: usize,
    room_tone_rms: f32,
    params: &ZeroCrossingParams,
) -> usize;

/// Refine a word offset: forward search; same threshold/fallback. Wrapper over
/// `refine_boundary(.., Forward)`.
pub fn refine_offset(
    pcm: &[f32],
    channels: u16,
    approx_offset_frame: usize,
    room_tone_rms: f32,
    params: &ZeroCrossingParams,
) -> usize;

/// Fade-**in** gain at ramp index `i` of `len` frames: `i / (len - 1)` (so fade-out is
/// `1.0 - crossfade_gain(i, len)`, unity-summing exactly). `len <= 1` → `1.0`
/// (no divide-by-zero). Inline so the renderer folds it into its seam loop.
#[inline]
pub fn crossfade_gain(i: usize, len: usize) -> f32;
```

## Sub-steps

### 5a — `frames_from_ms` + `ZeroCrossingParams::from_settings` + local-RMS/threshold helpers

- `frames_from_ms(ms, rate)` rounds a ms duration to whole frames (the one ms→frames site);
  `from_settings` builds the params, converting both timings once.
- `local_rms(pcm, channels, center_frame, win_frames)` over a clamped window, summing squares
  across all channels; `threshold(room_tone_rms, params.rms_ceiling)` per the formula. Internal, but
  unit-tested via the public functions.

### 5b — `refine_boundary` + `refine_onset` / `refine_offset`

- One private `refine_boundary(pcm, channels, approx_frame, room_tone_rms, params, dir)`: derive
  `n_frames = pcm.len() / channels`; scan up to `params.search_window_frames` in the `dir` direction
  over the clamped window, computing each frame's `local_rms` over a `params.crossfade_frames`
  window; first-below-threshold wins; track the running minimum-RMS frame for the fallback. No
  `rate` argument — the window sizes are already frames. `refine_onset` / `refine_offset` are
  one-line wrappers selecting `SearchDir`.
- Clamp the search window to `[0, n_frames - 1]` (a word at frame 0 has no backward room).

### 5c — `crossfade_gain`

- Inline fade-in weight `i / (len - 1)`, handling `len <= 1` (return `1.0`) without
  divide-by-zero. No allocation; the renderer derives `fade_out = 1.0 - gain`.

### 5d — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings` (incl. `missing_docs`, `unwrap_used`);
  `cargo test -p core audio::` and `cargo test -p core settings::` (the new settings keys).
- Confirm [audio-pipeline.md § Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade)
  and [data-model.md § App settings](../design/data-model.md#app-settings) still match — the
  configurable-timing, frame-based, and search-direction decisions are recorded there (done in this
  refinement; verify on implement). The `splice_search_window_ms` / `splice_crossfade_ms` keys and
  their round-trip / defaults tests land in [settings.rs](../src-tauri/core/src/settings.rs).
- One commit `1M2-05: zero-crossing search + crossfade primitives` on `claude/1M2`, unsigned.

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests`. Synthetic slices: a low-noise "room tone" region and a louder
"speech" region (sine burst), so a clear energy boundary exists. Unless noted, tests use mono
(`channels = 1`) and params built via `ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING,
20.0, 2.0, 48_000)` (→ `search_window_frames = 960`, `crossfade_frames = 96`, ceiling `0.0316`). All
`approx_*` inputs and returned indices are **frame** indices. Groups: Z = zero-crossing search,
C = crossfade, X = cross-cutting.

**Z — onset/offset search**

1. **Finds the boundary in tone-in-noise.** Slice = `[noise floor | speech burst]`; `refine_onset`
   at the approximate burst start (a few ms into the burst) lands on a frame at or before the
   burst whose local RMS < threshold (i.e. in the quiet lead-in), not inside the burst.
2. **Offset symmetric.** `[speech burst | noise floor]`; `refine_offset` at the approximate burst
   end lands at or after the burst, in the quiet tail.
3. **Honours the backward search bound.** With the nearest qualifying frame >
   `search_window_frames` before `approx_onset`, the search does **not** reach it; the result stays
   within `[approx_onset − search_window_frames, approx_onset]`.
4. **Honours the forward search bound** (offset analogue).
5. **First-qualifying, not best.** When several frames in the window are below threshold,
   `refine_onset` returns the **first** encountered scanning backward (nearest to `approx_onset`
   that still qualifies, per "search backwards … for an acceptable crossing").
6. **Min-energy fallback.** A window with **no** frame below threshold (all loud) → the returned
   index is the **minimum-local-RMS** frame within the window, and lies inside the window bounds.
7. **Threshold formula — floor.** With `room_tone_rms = 0.0`, threshold == `0.001`; a frame at
   RMS `0.0005` qualifies and one at `0.002` does not.
8. **Threshold formula — ceiling (from params).** With a large `room_tone_rms` (e.g. `0.1`),
   threshold clamps to `params.rms_ceiling` (default `0.0316`, not `0.2`); a frame at RMS `0.05`
   does **not** qualify. A companion case with a *raised* `rms_ceiling` accepts that same `0.05`
   frame — proving the ceiling is the setting, not a hard-coded constant.
9. **Threshold formula — linear region.** `room_tone_rms = 0.005` → threshold `0.01`; a frame at
   `0.008` qualifies, `0.012` does not.
9b. **Configurable search window.** With a qualifying frame ~15 ms out, params from a
   `splice_search_window_ms = 10` (→ 480 frames) do **not** reach it (fall back to min-RMS) while
   `= 20` (→ 960 frames) does — proving the setting drives the radius and is applied as frames.
10. **Word at slice start.** `approx_onset_frame = 0` (or < search window in) → no backward room;
    returns a valid in-bounds index (clamped window), never a negative/underflow.
11. **Word at slice end.** `approx_offset_frame` within the search window of `n_frames` → forward
    window clamps; valid in-bounds index.
12. **Determinism.** Same inputs → same index, twice.
12b. **Stereo, frame-aligned.** A 2-channel interleaved slice whose quiet lead-in is identical in
    both channels: `refine_onset` returns the **same frame index** as the mono equivalent, and that
    index is a valid frame (multiplying by `channels` indexes a channel-0 sample). A companion case
    with the quiet region only in the down-mix (channels differ) confirms RMS sums across channels.

**C — crossfade gain + frame conversion**

13. **ms→frames conversion.** `frames_from_ms(2.0, 48000) == round(0.002 * 48000) == 96`; a larger
    `ms` yields a proportionally larger count; `frames_from_ms(0.0, _)` and a negative `ms` → `0`.
    `from_settings(_, 20.0, 2.0, 48000)` populates `search_window_frames = 960`,
    `crossfade_frames = 96`.
14. **Endpoints.** `crossfade_gain(0, len) == 0.0` (fade-in start), `crossfade_gain(len-1, len) ==
    1.0`; fade-out `1.0 - gain` mirrors (`1.0` → `0.0`).
15. **Unity sum is exact.** For every `i`, `crossfade_gain(i, len) + (1.0 - crossfade_gain(i, len))
    == 1.0` exactly (the fold guarantees it; constant signal crossfades to itself with no dip).
16. **Monotonic + linear.** `crossfade_gain` strictly increasing in `i`, equal step `1/(len-1)`
    between adjacent indices.
17. **Degenerate.** `crossfade_gain(0, 1)` (and `len = 0`) returns `1.0` with no
    divide-by-zero/panic; a rate where `frames_from_ms` rounds the crossfade to `<= 1` is handled.

**X — cross-cutting**

18. **No allocation anywhere.** `refine_onset` / `refine_offset` return `usize` and `crossfade_gain`
    returns a scalar; neither allocates (review-asserted; they only read the borrowed slice). No
    `Vec` ramp is ever built.
19. **Refined offsets bracket the word.** `refine_onset(...) <= approx_onset_frame` and
    `refine_offset(...) >= approx_offset_frame` always hold (outward refinement), so the cut never
    eats into the word — the property M5's cut/mute depends on.

## Out of scope for Step 5

- **The `cut_words` / `mute_words` commands**, undo stamping, and overlap validation — M5; they
  call these functions at the edit site and write the refined indices into the word fields.
- **Recomputing the splice vec** from a cut/mute — Step 6 (`splice.rs`) consumes the refined
  offsets these functions return.
- **Applying the crossfade during rendering** — Step 8 (`render.rs`); this step only builds/tests
  the ramp.
- **Sourcing `room_tone_rms`** — produced by Step 4; passed in by the caller (M4/M5).
