# Phase 1 · M2 · Step 4 — Room-tone detection (action plan)

Per-step action plan for Step 4 of the M2 milestone from [phase1-m2.md](phase1-m2.md).
The authoritative spec is [audio-pipeline.md § Room tone detection](../design/audio-pipeline.md#room-tone-detection)
and [§ Room tone substitution](../design/audio-pipeline.md#room-tone-substitution). This step lands the
**room-tone analysis** — find the cleanest stretch of near-silence in a track, pre-crossfade it
for seamless looping, and store it as a content-addressed blob — plus the **first use of the
reserved `Kind::RoomTone = 0x4` format tag** ([data-model.md § Hashing and serialization](../design/data-model.md#serialization)).

Room tone is **signal processing, not ML** ([CLAUDE.md](../CLAUDE.md) invariant), so it lives in
Rust. It runs on the import/background path (M4 wires the caller), **never** the real-time cpal
callback, so the no-alloc/no-lock invariant does not bind here. This step has **no command, no DB
coupling, and no metadata journaling**: `detect_room_tone` is a pure DSP function returning the
analysed PCM, and `encode_room_tone` serializes it to the blob + content hash (mirroring
`encode_turn`). Writing the blob to `store` and journaling `TrackMeta.room_tone_hash`
is the **M4 import caller's** job; reading it back at render time is
**Step 8's** job (see *Persistence wiring (deferred)* below).

**Definition of done:** `core/src/audio/room_tone.rs` exists with the surface below; the detection
algorithm implements every branch of the spec (longest low-RMS window with the accept criteria;
stitch fallback; length-tiered loop crossfade); the `RoomTone` V1 blob has frozen wire bytes
with **pinned-bytes + pinned-hash** tests and a **G1 fixture round-trip** ([conventions.md](../design/conventions.md) G1);
the test matrix below passes; `cargo test -p core audio::`, `cargo clippy -p core -- -D warnings`,
and `cargo fmt --check` are green.

## Decisions locked in this step

- **Channel count is preserved; analysis is on the mono down-mix.** Detection consumes interleaved
  f32 at the project rate (from Step 3's cache / `decode_flac`). All RMS / peak / stability analysis
  and the window search run on the **channel-mean** mono buffer, but the extracted segment is stored
  in the **source channel count**: a mono recording yields **mono** room tone, a stereo recording
  yields a **stereo** sample (per-channel content preserved, collapsed only for the RMS math). This
  overrides the earlier "mono storage" draft — recorded in [audio-pipeline.md](../design/audio-pipeline.md).
  All lengths below are in **frames** (per-channel samples); interleaved sample count = `frames × channels`.
- **Skip extraction when the recording is < 10 s → `RoomTone::None`.** Guard at the top of
  `detect_room_tone` (frame count `< 10 × rate`): too little material to extract a useful loop;
  return `None` *as if no window passed*, before any analysis. The renderer falls back to digital
  silence. (Empty input is the degenerate case of this guard.)
- **100 ms blocks are non-overlapping and floor-aligned.** Block size = `round(0.1 * rate)`; the
  trailing partial block (< 100 ms) is dropped from the RMS sweep (it cannot anchor a window).
  Block RMS is `sqrt(mean(x²))` over the block (on the mono down-mix).
- **Global quiet threshold `Q = min(rms_ceiling, Pq)`, both configurable.** `rms_ceiling`
  (`room_tone_rms_ceiling` setting, default `0.0316` ≈ −30 dBFS) is the absolute "background, not
  signal" ceiling — the same default the Step-5 zero-crossing search clamps "quiet" to
  (`min(2 * room_tone_rms, 0.0316)`). `Pq` is the `room_tone_quiet_percentile`-th percentile
  (default 5) of the 100 ms block-RMS values (adapts `Q` downward on genuinely-quiet tracks; compute
  it deterministically — sort a copy of `br[]` and take the nearest-rank element — for reproducible
  content-addressed output). Both are **app settings**
  ([settings.rs](../src-tauri/core/src/settings.rs) / [data-model.md § App settings](../design/data-model.md#app-settings));
  `detect_room_tone` takes them as a `RoomToneParams` argument the M4 caller resolves from settings
  (the audio engine reads no settings directly, mirroring `resampling_quality`). `Q` gates
  **stitch-piece selection** and (with the main-window gate) the **degenerate → `None`**
  determination. **The main window's level gate is `rms_ceiling` alone** (`window_rms ≤ rms_ceiling`),
  *not* `Q`: a multi-second window's *mean* sits near the quiet region's median, well above its
  percentile, so gating the window on `Pq` would reject genuine room tone.
- **"Longest contiguous window with the lowest cumulative RMS" is resolved as:** scan contiguous
  block-runs whose duration is in `[2 s, 10 s]`; among those that **pass the accept criteria** prefer
  the **longest**, breaking ties by **lowest window RMS**, then by **earliest start** (deterministic).
  Target 5–10 s is an upper clamp, not a requirement — a 2 s window that passes is accepted if
  nothing longer does. See the **sweep algorithm** in sub-step 4a for the efficient O(1)-per-test
  traversal.
- **Accept criteria, applied to a candidate window:** (0) **level** — `window_rms ≤
  rms_ceiling` (it is background, not signal; this is what makes a uniformly-loud track
  return `None` — the peak and SD criteria alone do *not* reject a full-scale sine, whose
  `peak = 1.0 ≤ 5 × 0.707 = 3.54` and whose block-RMS SD ≈ 0); (1) **peak** — `|x|` within the
  window ≤ `5 × window_rms` (no transient); (2) **stability** — **SD of the 100 ms block-RMS values**
  ≤ `15% of their mean`. All three must hold. (`window_rms` is the RMS over the whole window, not
  the block mean.)
- **Stitch fallback when no ≥ 2 s window passes:** collect 100–300 ms pieces that qualify as quiet —
  every block's RMS ≤ `Q` **and** the piece peak ≤ `5 × the piece RMS` (the level + peak criteria;
  the SD/stability criterion is **not applied** at 1–3 blocks, where it is degenerate). **Select**
  pieces in **ascending-RMS order** (quietest first) until the 10 s target is reached; then
  **assemble** the selected pieces in their **original audio-file order** (ascending block index) and
  concatenate with **50 ms equal-power crossfades** between adjacent pieces (or all qualifying quiet
  material is exhausted — then use what there is, down to a documented floor).
- **Loop crossfade by length tier** (applied to head↔tail so the stored segment loops with no
  per-playback fade): **< 500 ms → 50 ms; 500 ms–2 s → 100 ms; > 2 s → 500 ms**. The crossfade is
  applied by mixing the segment's **tail ramp** into its **head** (**equal-power**, via the shared
  `equal_power_gain`) so that wrapping tail→head is C⁰-continuous. Tail and head are different
  windows of the same room-tone noise (uncorrelated), so equal-power keeps the floor level — a
  linear ramp would dip ~3 dB at every loop boundary. This baked loop fold is independent of the
  render-time **seam** crossfade (which Step 8 applies, also equal-power, with source handles). The
  stored length is the segment length **after** the wrap fold (the crossfaded region is consumed
  once per loop, not duplicated).
- **Degenerate: no usable quiet material → `None`.** When **no window** passes the accept criteria
  (criterion 0 in particular — no run stays under `rms_ceiling`) **and no stitch piece**
  qualifies (no block ≤ `Q`), e.g. continuous loud content, yield `RoomToneOutcome::None`; the caller leaves
  `room_tone_hash = None` and the renderer (Step 8) falls back to **digital silence** for RoomTone
  splices. This must never panic or fabricate tone from loud audio. (The < 10 s and empty-input
  guards also return `None`.)
- **`RoomTone` V1 wire schema is frozen + pinned**, exactly like the M1 Turn/Label blobs
  (`turn.rs` pattern): a `mod v1` `RoomToneV1 { sample_rate: u32, channels: u16, rms: f32, samples: Vec<f32> }`
  (`samples` interleaved), `encode_room_tone` always emitting `(Kind::RoomTone,
  LATEST_ROOM_TONE_VERSION = 1)` via `encode_tagged`, `decode_room_tone` dispatching on the version
  nibble through `From<…V1>`. Tag byte is `0x41`. The hash covers the full tagged bytes. **f32
  samples are bit-pinned** (IEEE little-endian via postcard) so a pinned-bytes test is stable across
  builds.

## Module surface

```rust
// audio/room_tone.rs

/// Outcome of room-tone analysis over a track's resampled PCM.
pub enum RoomToneOutcome {
    /// A usable, pre-crossfaded loop segment (interleaved f32, source channel count, project rate).
    Found(RoomTone),
    /// No stretch of the track was quiet/stable enough to serve as room tone.
    None,
}

pub struct RoomTone {
    pub samples: Vec<f32>,   // interleaved, source channel count, project rate, loop-crossfade applied
    pub channels: u16,       // 1 for a mono source, 2 for stereo, … (matches the source)
    pub sample_rate: u32,
    pub rms: f32,            // RMS of the extracted segment
}
// frame count (per-channel length) = samples.len() / channels as usize

/// Tunable detection thresholds, resolved from app settings by the M4 caller
/// (the audio engine reads no settings directly). `Default` matches the settings
/// defaults (`DEFAULT_ROOM_TONE_RMS_CEILING` / `DEFAULT_ROOM_TONE_QUIET_PERCENTILE`).
pub struct RoomToneParams {
    /// Absolute RMS ceiling (linear, ≈ −30 dBFS default): audio above this is never room tone.
    pub rms_ceiling: f32,
    /// Percentile (0–100) of block RMS forming the adaptive quiet threshold `Q = min(rms_ceiling, Pq)`.
    pub quiet_percentile: f64,
}

/// Analyse a track's resampled PCM via a `FrameReader` and extract a loopable room-tone
/// segment per audio-pipeline.md § Room tone detection. The RMS/peak/stability analysis runs
/// on the channel-mean down-mix; the returned segment preserves the source channel count.
/// Returns `None` if the recording is < 10 s or no quiet material qualifies. Deterministic
/// for a given input + `params`.
pub fn detect_room_tone(
    reader: &mut impl FrameReader,
    params: &RoomToneParams,
) -> Result<RoomToneOutcome, AudioError>;

/// Format version emitted by every new `encode_room_tone`.
pub const LATEST_ROOM_TONE_VERSION: u8 = 1;

/// Encode a room-tone segment as the latest `Kind::RoomTone` wire format
/// (tag 0x41). Returns the content hash and tagged bytes, ready for `store::put`.
pub fn encode_room_tone(seg: &RoomTone) -> Result<(Hash, Vec<u8>), postcard::Error>;

/// Decode a `Kind::RoomTone` blob back to a segment (verifies tag, dispatches version).
pub fn decode_room_tone(bytes: &[u8]) -> Result<RoomTone, DecodeError>;
```

### Persistence wiring (deferred — context for the implementer)

There is **no `store_room_tone` / `load_room_tone` helper in this step**, and none is needed: the M1
store has no per-kind store/load wrappers. Turns are encoded by the *caller* (`encode_turn` → carried
as `NewElement` bytes) and written by `apply_batch` via `store::put`; only the metadata blob is
encoded *inside* `apply_batch` (`encode_metadata`). Room tone follows the **Turn pattern** — the
caller encodes it — so Step 4 ships `encode_room_tone` / `decode_room_tone` only.

- **Write (M4 import).** On the background import thread: `detect_room_tone(reader, &params)` →
  `RoomToneOutcome::Found(rt)` → `encode_room_tone(&rt)` → `(hash, bytes)`; set `TrackMeta.room_tone_hash
  = Some(hash)`; persist via `apply_batch(&[], Some(updated_metadata), CommandId::…)`. The blob must reach `store` **in or before**
  that transaction so the journaled `room_tone_hash` is never dangling. Recommended M4 work:
  **either** add an optional `side_blobs: &[(Hash, Vec<u8>)]` parameter to `apply_batch` (puts the
  room-tone blob in the *same* transaction as the metadata that references it — also resolves the
  encode-asymmetry by letting the caller pre-encode, like Turns), **or** `store::put` the blob in a
  prior transaction and rely on content-addressing (a crash between the two leaves a harmless orphan
  blob, never a dangling reference). Either way this is **M4 scope**, flagged here per the
  doc-sync rule; it does not land in Step 4.
- **Read (Step 8 render).** `render.rs` resolves `room_tone_hash` lazily at splice-read time
  (`store::get` → `decode_room_tone`), exactly as it opens the resampled FLAC cache. The room-tone
  **bytes never enter the reconstructed `TimelineState`** — only the hash lives in `TrackMeta`, so the
  project open / journal-replay path is unchanged.

## Sub-steps

### 4a — RMS sweep + window search + accept criteria

- **Guard:** if `frames < 10 * rate` (or `samples` empty) → `RoomTone::None` immediately.
- Down-mix to a mono analysis buffer (channel mean) if `channels > 1`.
- Compute non-overlapping 100 ms block stats and the O(1)-per-test machinery:
  - `e[b] = Σx²`, `bp[b] = max|x|`, `br[b] = sqrt(e[b] / block_len)` per block.
  - prefix sums `prefixE` (window energy → `window_rms`), `prefixR` and `prefixR2` (block-RMS sum
    and sum-of-squares → block-RMS mean and SD), all O(1) range queries.
  - a **range-max** structure over `bp[]` (sparse table, build `O(n log n)`, query `O(1)`) for the
    window peak; window peak is *not* a prefix-sum (max is not invertible).
  - `Pq` = `params.quiet_percentile`-th percentile of `br[]` (nearest-rank on a sorted copy);
    `rms_ceiling = params.rms_ceiling`; `Q = min(rms_ceiling, Pq)`.
- **Window sweep (efficient traversal).** Track only the best window so far as
  `(start, len, rms)`. Acceptance is **non-monotonic in length** (a longer run can fail on a
  transient while a shorter one passes), so at each start we scan length **descending** and the
  first acceptance is the *longest* acceptance at that start. Prune everything shorter than the
  current best — a shorter accepted window can never beat it (length precedence), and equal length
  only matters for the RMS tie-break:

  ```
  min_blk = ceil(2*rate / block_len);  max_blk = floor(10*rate / block_len)
  best = None  // (start, len_blocks, rms)
  for s in 0 ..= (n_blocks - min_blk):
      lower = max(min_blk, best.len if best else min_blk)   // never search shorter than best
      for L in min(max_blk, n_blocks - s) ..= lower  (descending):
          rms = window_rms(s, L)
          if rms <= rms_ceiling                    // (0) level
             and window_peak(s, L) <= 5*rms                  // (1) peak
             and block_rms_sd(s, L) <= 0.15*block_rms_mean(s, L):  // (2) stability
              if best is None or L > best.len
                 or (L == best.len and rms < best.rms):       // longer, else lower-RMS
                  best = (s, L, rms)
              break        // first (=longest) acceptance at this start; advance s
  ```

  Earliest-start is the implicit third tie-break: starts are visited ascending and `best` is
  replaced only on a *strictly* better key (longer, or equal-length-lower-RMS), so an exact
  (length, RMS) tie keeps the earlier start. The 10 s cap bounds the inner loop to ≤ 100 blocks,
  so the whole sweep is `O(n_blocks)` after the `O(n log n)` sparse-table build.
- If `best` is `Some`, slice the selected window's frames across **all source channels**
  (interleaved) from the original PCM → hand to 4c. Else → 4b.

### 4b — Stitch fallback

- When no contiguous window passes, gather 100–300 ms pieces that qualify as quiet (every block's
  RMS ≤ `Q` **and** piece peak ≤ `5 × piece RMS`; the SD/stability criterion is **not** applied at
  1–3 blocks). **Select** pieces in **ascending-RMS order** (quietest first) up to the 10 s target;
  **assemble** the selected pieces in **original audio-file order** (ascending block index) and
  concatenate (interleaved, all source channels) with 50 ms equal-power crossfades. If quiet material is
  exhausted before 10 s, use what exists (documented floor); if there is none, return `RoomTone::None`.

### 4c — Loop crossfade + segment assembly

- Apply the length-tiered head/tail loop crossfade, operating on **frames** (the fade ramp is
  identical across channels; fold the tail ramp into the head per channel). Crossfade length in
  frames = `round(fade_ms * rate)`. Produce the final
  `RoomTone { samples, channels, sample_rate, rms }` (interleaved, source channel count).

### 4d — `RoomTone` V1 blob

- `mod v1 { RoomToneV1 { sample_rate, channels, rms, samples } }`; `encode_room_tone` /
  `decode_room_tone` per the `turn.rs` pattern; `LATEST_ROOM_TONE_VERSION = 1`; `From` conversions;
  pinned-bytes + pinned-hash consts with the `#[ignore] capture_pinned_values` helper.

### 4e — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings` (incl. `missing_docs`, `unwrap_used`);
  `cargo test -p core audio::`.
- Confirm [audio-pipeline.md § Room tone detection](../design/audio-pipeline.md#room-tone-detection) still
  matches the implemented thresholds/tiers; update it in the same commit if any behaviour was
  adjusted (CLAUDE.md doc-sync rule). Confirm the channel-preservation, < 10 s skip, and quiet
  threshold (`Q = min(rms_ceiling, Pq)`, both configurable) notes are present.
- One commit `1M2-04: room-tone detection + RoomTone V1 blob` on `claude/1M2`, unsigned per the
  GPG-by-branch policy.

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests` in `room_tone.rs` ([conventions.md](../design/conventions.md) A1). Synthetic
signals are built by test helpers (white-noise floor at a chosen RMS via the seeded xorshift RNG
already used in `tree.rs` tests; sine bursts for "loud" spans). Groups: D = detection algorithm,
L = loop crossfade, B = blob format, X = cross-cutting.

**D — detection algorithm**

1. **Clean 2 s window accepted.** 10 s signal: 8 s of loud sine + a 2 s stable low-noise tail →
   `Found`; the returned segment's source range lies within the quiet tail; `window_rms` is ~the
   noise floor.
2. **Length precedence beats lower RMS.** Two quiet stretches both passing — a **2 s** window that
   is *quieter* (lower RMS) and a **6 s** window that is slightly louder → the **6 s** window is
   selected. This exercises the precedence order: length wins even though the shorter window has the
   lower RMS (so it is not the RMS tie-break deciding it).
3. **Upper clamp at 10 s.** A 30 s uniformly-quiet signal → the selected window is ≤ 10 s (target
   range respected), not the whole track.
4. **Peak-energy rejection.** A quiet window containing one transient sample `> 5 × window_rms`
   fails criterion 1; if it is the only candidate → falls through to stitch/`None`, never accepted.
5. **Variance rejection.** A "quiet" window whose block-RMS SD `> 15%` of mean (e.g. a slow swell)
   fails criterion 2 and is not accepted.
6. **Tie-break 1 — lower RMS at equal length.** Two **equal-length** windows with **different**
   RMS (no longer window available) → the **lower-RMS** one is chosen.
6b. **Tie-break 2 — earliest start at equal length & RMS.** Two equal-length, equal-RMS windows →
   the **earliest** is chosen; the choice is identical across repeated runs (no HashMap iteration
   leakage).
7. **Stitch fallback engaged.** A signal with no contiguous 2 s quiet window but several scattered
   100–300 ms quiet gaps → `Found` via stitching; the stitched length ≥ 2 s (or the documented
   floor), pieces are assembled in original audio order, and adjacent pieces are joined (no hard
   discontinuity at the 50 ms seams — assert the max abs first-difference at seam samples is
   bounded).
8. **No usable quiet material → `None`.** A continuous full-scale sine → `RoomTone::None` —
   specifically because every window's RMS (≈ 0.707) exceeds `rms_ceiling` (criterion 0),
   even though it passes the peak (`1.0 ≤ 3.54`) and SD (≈ 0) criteria; **no panic**, no fabricated
   tone. (Guards the regression that accept criteria 1–2 alone would wrongly accept loud tone.)
9. **Stereo analysis on the down-mix.** A stereo input whose quiet region is identical in both
   channels detects the same window (and same frame-length) as the mono equivalent.
9b. **Channel count preserved.** A **mono** source yields `channels == 1`; a **stereo** source whose
   two channels carry *distinct* quiet content yields `channels == 2` with **both** channels'
   samples preserved in the stored segment (not collapsed to mono); the per-channel frame count
   matches the mono case for the same quiet region.
10. **Block alignment / trailing partial.** A signal whose length is not a multiple of 100 ms still
    analyses (the trailing < 100 ms is dropped) and finds the expected window.
10b. **Recording < 10 s → `None`.** A 9 s signal containing an otherwise-perfect quiet window →
    `RoomTone::None` (the length guard fires before analysis); a 10 s signal with the same window is
    `Found` (boundary check).
11. **Determinism.** `detect_room_tone` on the same input twice returns byte-identical `samples`
    (for a fixed `params`).
11b. **Thresholds are honored (configurability).** A signal whose quiet region sits at RMS ≈ `r`
    (with `r` between two test ceilings) is `Found` when `params.rms_ceiling > r` but `None` when
    `params.rms_ceiling < r` — proving the ceiling gates detection. A companion case varies
    `quiet_percentile` and asserts the stitch-selected set (and thus `Q`) shifts accordingly. Guards
    that the M4-resolved settings actually drive behaviour (not hard-coded constants).

**L — loop crossfade tiers**

12. **< 500 ms → 50 ms fade.** A short detected segment uses a 50 ms head/tail crossfade.
13. **500 ms–2 s → 100 ms fade.** Mid-length segment uses 100 ms.
14. **> 2 s → 500 ms fade.** Long segment uses 500 ms.
15. **Seamless wrap (C⁰ continuity).** Concatenating the stored segment with itself
    (`[seg, seg]`) has a bounded max abs first-difference at the wrap boundary (checked per channel)
    — i.e. tail→head is continuous (the pre-applied fold means playback needs no further fade).
16. **Fade region not duplicated.** The stored **frame** count equals (raw window frames − crossfade
    frames) so looping consumes the fade exactly once per cycle (no doubled frames); for a stereo
    segment the interleaved `samples.len()` is `2 ×` that.

**B — RoomTone V1 blob**

17. **Round-trip.** `encode_room_tone` → `decode_room_tone` reproduces the segment exactly
    (f32 bit-equal; `sample_rate`, `channels`, and `rms` preserved). Cover both a mono and a stereo
    segment.
18. **Tag byte is `0x41`.** First byte of the encoded blob `== tag_byte(Kind::RoomTone, 1)`.
19. **Pinned wire bytes.** A hand-built `RoomToneV1` (stereo, small fixed interleaved vector,
    e.g. `channels = 2`, 4 samples = 2 frames) encodes to a pinned `[u8; N]` (regenerate via
    `capture_pinned_values`); guards postcard-shape drift / silent data loss
    ([conventions.md](../design/conventions.md) G1).
20. **Pinned hash.** The same fixture hashes to a pinned `[u8; 16]`.
21. **Hash determinism + sensitivity.** Same segment → same hash twice; a one-sample change → a
    different hash; a `sample_rate` change → a different hash; **a `channels` change → a different
    hash** (tag/content all covered).
22. **Kind mismatch.** `decode_room_tone` on a `Kind::Turn` blob → `DecodeError::KindMismatch
    { expected: RoomTone, found: Turn }`.
23. **Unknown version.** Tag `0x4F` → `DecodeError::UnknownVersion { kind: RoomTone, version: 0xF }`.
24. **Empty / truncated input.** `&[]` → `DecodeError::Empty`; `&[0x41]` → `DecodeError::Postcard`.
25. **G1 fixture round-trip.** A committed `room_tone_v1.blob` fixture (the bytes of a known
    segment) decodes to the expected `RoomTone` — proves an *older on-disk* blob still loads
    (the format-change-ships-a-fixture invariant).

**X — cross-cutting**

26. **No DB / no journaling.** Asserted by signature + a test that detection and encoding neither
    open a connection nor mutate metadata — the M4 caller journals `room_tone_hash`.
27. **Empty input.** `detect_room_tone` on an empty reader → `RoomToneOutcome::None`, no panic.

## Fixtures to add (under `core/tests/fixtures/`)

- `room_tone_v1.blob` — the committed G1 fixture for test 25 (a few-sample `RoomTone` V1 blob).
  All detection inputs are generated in-test (no committed audio needed for this step).

## Out of scope for Step 4

- **Writing the room-tone blob to `store` and journaling `TrackMeta.room_tone_hash`**, plus
  running detection on a background thread at import — M4 (the
  caller of `detect_room_tone` + `encode_room_tone` + `store::put`; see *Persistence wiring* above).
- **Using room tone at render time** (looping the stored segment + the splice-recorded gap crossfade,
  i.e. the RoomTone splice's own stamped fade applied as a centered equal-power seam overlay) —
  Step 8 (`render.rs`).
- **`room_tone_rms`** as an input to the zero-crossing search — Step 5 reads it from the
  `RoomTone.rms` field; the value is produced here but consumed there.
- **Non-speech sound detection** ([audio-pipeline.md § Non-speech sound detection](../design/audio-pipeline.md#non-speech-sound-detection))
  — a separate Rust sweep, scheduled in M4/M5, not part of room-tone analysis.
