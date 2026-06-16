# Phase 1 · M2 · Step 3 — Resample + FLAC cache (action plan)

Per-step action plan for Step 3 of the M2 milestone from [phase1-m2.md](phase1-m2.md).
The authoritative spec is [audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling).
This step turns the Step-2 decoded f32 PCM into the project's **resampled cache** — the single,
uniform, codec-agnostic read source every later step (EDL render, playback, export, wet/dry
blend) reads from.

Three modules: `resample.rs` (rubato sinc → project rate, identity fast-path), `flac.rs` (24-bit
integer FLAC encode + decode), and `cache.rs` (cache path + `ensure_resampled`, the
regenerate-if-missing fill). It consumes Step 2's `decode()` / `DecodedAudio` / `AudioError`.

**No command, no metadata journaling, no import/open orchestration.** `ensure_resampled`
*produces the cache file and returns the outcome*; the M4 import command and open-time sweep are
the callers that use the result and run it on a background thread. Step 3
builds and tests the callable synchronously and directly. The cache *read* feeds the real-time
pre-roll thread (Step 8/9), but seekable/ranged reading is layered there; Step 3's `decode_flac`
is a whole-file decode for correctness tests.

**Definition of done:** `core/src/audio/resample.rs`, `flac.rs`, and `cache.rs` exist with the
surface below; the cache round-trips f32 within the 24-bit quantization bound; `ensure_resampled`
writes `<vbdata>/resampled/<track_id>.flac` and regenerates it when missing; the test matrix below
passes; `cargo test -p core audio::`, `cargo clippy -p core -- -D warnings`, `cargo fmt --check`
green.

## Decisions locked in this step

- **The identity-rate path still transcodes to the cache.** When `source_rate == project_rate`,
  skip the rubato pass (output is bit-exact to the decoded input) but **still** encode the FLAC
  cache — the cache is the uniform read source for *every* track, not only resampled ones
  ([audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling)). Avoiding the duplicate for
  already-at-rate lossless sources is a Phase 2 revisit.
- **The resampled FLAC is *derived and regenerable*, NOT content-addressed.** Unlike the
  room-tone blob (Step 4, hashed + pinned-bytes), the cache carries no stable cross-version hash:
  if a future rubato/`flacenc` version changes the bytes, the file is simply regenerated on the
  next open. So **cross-version determinism is not required**, but **within-build determinism
  is** (same input + rate + quality + build → identical bytes) so tests and reproducible export
  are stable. This is the key contrast with every content-addressed format in the project.
- **24-bit integer FLAC; clamp f32→i24 at encode.** Scale f32 by `2^23 − 1` to signed 24-bit.
  Sinc resampling can ring **past ±1.0** even for in-range input, so **clamp to [−1, 1] at the
  encode boundary** (not in `resample()`); the ≈ −144 dB quantization floor sits far below speech
  room tone ([audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling)).
- **rubato runs planar; we de-interleave → resample per channel → re-interleave.** Each channel
  is resampled independently (no cross-talk). The resampler's internal delay + the final partial
  chunk must be **flushed/drained** so no samples are dropped or duplicated and the output length
  matches `round(in_frames * to/from)` within the resampler's bounded rounding.
- **`ResamplingQuality` → rubato params mapping lives in code with a `why` comment**, not pinned
  as a format (the cache is derived). Representative: `Balanced` → shorter sinc / linear interp;
  `High` / `Highest` → longer sinc / higher oversampling / cubic interp + a higher-order window.
  Tune empirically; tests assert *behaviour* (length scaling, frequency preservation, low
  aliasing), not exact parameters.
- **`ensure_resampled` produces the file and returns a `CacheOutcome` — it does NOT journal metadata and
  holds no `Db` connection.** The resampled path (`resampled/<track_id>.flac`) is **fully derived
  from the track ID** and not persisted in `TrackMeta`. The M4 import caller uses `CacheOutcome.length_samples`
  to set `original_length_samples`. Keeping the callable connection-free makes it trivially testable
  and keeps the real-time/background concerns out of M2.
- **Cache path = `<vbdata>/resampled/<track_id>.flac`** per
  [data-model.md](../design/data-model.md#derived-files), keyed by the stable `TrackMeta.id`. The id is a
  `u32` — always filesystem-safe (no sanitization) and never reassigned — so **rename → orphaned
  cache** and **sanitized-name collision** cannot arise; the two cases a name-keyed scheme would
  have deferred to M4 simply don't exist. The caller supplies the id (allocated from
  `next_track_id` at M4 import). Track *names* are mutable and not filesystem-safe, so they are
  unsuitable as the key even though they are command-layer-unique.
- **FLAC decode reuses Step 2's Symphonia path; encode is `flacenc`** (locked in Step 1). Add a
  single `AudioError::EncodeFailed(String)` variant (`error_key()` → `"encode_failed"`) for
  encoder failures; everything else reuses the Step-2 `AudioError` variants.

## Module surface

```rust
// audio/resample.rs
/// Resample interleaved f32 from `from_rate` to `to_rate`, preserving channel count and
/// interleave order. Bit-exact identity when `from_rate == to_rate` (no rubato pass).
/// Output may briefly exceed [-1, 1] (sinc overshoot); callers clamp at the encode boundary.
pub fn resample(
    samples: &[f32],
    channels: u16,
    from_rate: u32,
    to_rate: u32,
    quality: ResamplingQuality,
) -> Result<Vec<f32>, AudioError>;
```

```rust
// audio/flac.rs
/// Encode interleaved f32 (clamped to [-1, 1]) as 24-bit integer FLAC at `out`.
pub fn encode_flac_24(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    out: &Path,
) -> Result<(), AudioError>;

/// Decode a FLAC file to interleaved f32 (delegates to the Step-2 Symphonia path).
pub fn decode_flac(path: &Path) -> Result<DecodedAudio, AudioError>;
```

```rust
// audio/cache.rs
/// `<vbdata_dir>/resampled/<track_id>.flac` (id is always filesystem-safe; no sanitization).
pub fn resampled_cache_path(vbdata_dir: &Path, track_id: u32) -> PathBuf;

/// Ensure the resampled cache exists for a track: if the file is missing, decode the source,
/// resample to `project_sample_rate`, and write the 24-bit FLAC. Returns the relative path
/// (`resampled/<track_id>.flac`, derivable from the track ID), whether it (re)wrote the file,
/// and the project-rate frame count. Does NOT journal metadata. Requires the source to exist.
pub fn ensure_resampled(
    source: &Path,
    vbdata_dir: &Path,
    track_id: u32,
    project_sample_rate: u32,
    quality: ResamplingQuality,
) -> Result<CacheOutcome, AudioError>;

pub struct CacheOutcome {
    pub relative_path: String,   // '/'-separated, relative to the project dir
    pub regenerated: bool,       // true if it (re)wrote the file; false if already present
    pub length_samples: i64,     // project-rate frame count (feeds original_length_samples at M4)
}
```

## Sub-steps

### 3a — `audio/resample.rs`: rubato sinc + identity fast-path

- `from_rate == to_rate` → return `samples.to_vec()` (bit-exact; skip rubato).
- Else: de-interleave into `channels` planar buffers; build a `SincFixedIn<f32>` (or chunked
  resampler) with params from `quality`; process in the resampler's fixed chunk size; **flush**
  the tail (process the final partial chunk + drain internal delay) so the total output length is
  `round(in_frames * to/from)` ± the resampler's bounded rounding; re-interleave.
- Map the `ResamplingQuality` preset to rubato params in one place with a `why` comment.
- No clamp here — overshoot is the encoder's concern (documented).

### 3b — `audio/flac.rs`: 24-bit encode + decode

- `encode_flac_24`: clamp each f32 to `[-1, 1]`, scale to signed 24-bit (`* (2^23 − 1)`, round),
  feed `flacenc` (bits_per_sample = 24, the given channels + rate, fixed block size), write the
  encoded bytes to `out`. Encoder error → `AudioError::EncodeFailed`.
- `decode_flac`: delegate to the Step-2 Symphonia decode (FLAC is on that path), returning
  `DecodedAudio`. Whole-file decode; ranged/seekable reads are Step 8.

### 3c — `audio/cache.rs`: path + `ensure_resampled`

- `resampled_cache_path`: join `vbdata_dir/resampled/<track_id>.flac` (id rendered as decimal; no
  sanitization needed).
- `ensure_resampled`: resolve the path; if the file exists → `regenerated = false`, probe it for
  `length_samples`, return. Else create `resampled/` (mkdir -p), `decode(source)` (Step 2),
  `resample(...)` to `project_sample_rate`, `encode_flac_24` to the path, `regenerated = true`,
  `length_samples` = output frames. On any failure **after** a partial write, remove the partial
  file so a later open retries cleanly (no zero-byte/corrupt artifact left behind).
- Missing source → propagate `AudioError::Io(NotFound)`; write nothing.

### 3d — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings` (incl. `missing_docs`,
  `unwrap_used`); `cargo test -p core audio::`.
- Confirm [audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling) still matches; update in
  the same commit if behaviour was adjusted (CLAUDE.md doc-sync rule).
- One commit `1M2-03: resample + 24-bit FLAC cache` on `claude/1M2`, unsigned per the
  GPG-by-branch policy.

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests` per module ([conventions.md](../design/conventions.md) A1), plus a
`core/tests/` integration test for the end-to-end source→cache round-trip. Groups: R = resample,
C = FLAC codec, E = cache/`ensure_resampled`, X = cross-cutting.

**R — `resample.rs`**

1. **Identity fast-path is bit-exact.** `from == to` → output `==` input exactly (and the rubato
   pass is skipped).
2. **Upsample length.** 24 kHz → 48 kHz roughly doubles frame count (`out_frames ≈ in*2` within
   the resampler's bounded rounding).
3. **Downsample length.** 48 kHz → 24 kHz roughly halves.
4. **Non-integer ratio.** 44.1 kHz → 48 kHz: `out_frames ≈ in * 48000/44100` within a few samples.
5. **Channel preservation, no cross-talk.** Stereo with distinct L/R sines resamples each channel
   independently; output is interleaved, `channels` unchanged, and L≠R content is preserved (no
   channel bleed).
6. **Frequency preservation.** A sine at `f` resampled to a new rate keeps its peak at `f` (FFT
   bin check); out-of-band energy (aliasing) stays below a loose threshold.
7. **No drop/duplicate at chunk seams.** A linear ramp longer than the rubato chunk resamples to a
   monotonic ramp with no discontinuity at chunk boundaries (proves the flush/drain is correct)
   and the expected total length.
8. **All quality presets run.** `Balanced` / `High` / `Highest` each resample without panic and
   produce the expected length; (loose) higher preset ≤ aliasing of lower.
9. **Within-build determinism.** Same input + rates + quality → byte-identical output.
10. **Degenerate inputs.** Empty input → empty output; an input shorter than the sinc length →
    no panic, sensible (possibly empty/short) output.
11. **Overshoot passes through.** A full-scale impulse/square resampled produces samples with
    `abs > 1.0` (sinc ringing) — `resample` does **not** clamp (documents why the encoder must).

**C — `flac.rs`**

12. **24-bit round-trip bound.** f32 ramp/sine in `[-1, 1]` → `encode_flac_24` → `decode_flac` →
    max abs error ≤ ~`2^-23` (≈ 1.2e-7); quantization-bounded.
13. **Full-scale endpoints.** ±1.0 round-trips to ≈ ±1.0 within the bound (no wrap/overflow at the
    i24 extremes).
14. **Overshoot is clamped at encode.** Input from R11 (`abs > 1.0`) → encode clamps to ±full
    scale, no panic; decoded values are ≈ ±1.0.
15. **Channels + rate fidelity.** Stereo 48 kHz encode→decode preserves `channels == 2`,
    `sample_rate == 48000`, and interleave order (L/R distinct).
16. **Cache file probes as FLAC.** `probe()` (Step 2) on the written file → `codec == "flac"`,
    correct rate/channels, `length_frames == Some(frames)`.
17. **Within-build determinism.** Same f32 input → byte-identical FLAC file.
18. **No length surprise.** Decoded frame count `==` encoded frame count (no gapless trim on our
    own FLAC).

**E — `cache.rs` / `ensure_resampled`**

19. **Path resolution.** `resampled_cache_path(vbdata, 1)` ends with
    `resampled/1.flac`; `CacheOutcome.relative_path` uses `/` separators.
20. **End-to-end at a different rate.** 44.1 kHz source + project 48 kHz → writes
    `resampled/<track_id>.flac` at 48 kHz; `regenerated == true`; decode of the cache has the
    resampled length; `length_samples` == project-rate frame count.
21. **Identity-rate source still cached.** 48 kHz source + project 48 kHz → file written,
    `regenerated == true`, decoded PCM bit-exact to the source decode (identity path).
22. **Idempotence.** Two calls in a row → first `regenerated == true`, second
    `regenerated == false` (file present, not rewritten); same path + `length_samples` both times.
23. **Regenerate after deletion.** Write, delete the file, call again → `regenerated == true`; the
    regenerated cache decodes to the same PCM as the first (deterministic regen).
24. **Missing source.** Source path absent → `AudioError::Io(NotFound)`; **no** partial/zero-byte
    cache file is left on disk.
25. **Creates the subdir.** `resampled/` is created when absent (mkdir -p).
26. **Length without regen.** A pre-existing cache → `length_samples` is still correct (probed
    from the existing file, not recomputed by resampling).

**X — cross-cutting**

27. **No metadata write / no `Db` connection.** Asserted by the signature + a test that
    `ensure_resampled` neither opens a connection nor mutates any `TrackMeta` — it only produces
    the file and returns the `CacheOutcome`. The resampled path is derived from track ID, not
    persisted in metadata.
28. **`error_key()` for the new variant.** `AudioError::EncodeFailed(_).error_key() ==
    "encode_failed"` (extends the Step-2 mapping table-test).

## Out of scope for Step 3

- **Seekable / ranged cache reads** for the real-time pre-roll — Step 8 (render source readers).
  `decode_flac` here is whole-file.
- **Running `ensure_resampled` on the background import thread and the open-time sweep** — M4
  (the callers of `ensure_resampled`).
- **Track-id assignment** — the caller passes the id; allocating it from `next_track_id` is M4
  import's concern. (Keying the cache by id, rather than name, means rename-orphaning and
  sanitized-name collisions don't arise — see the path decision above.)
- **The room-tone blob** (content-addressed, pinned-bytes) — Step 4; contrast with this
  derived/regenerable cache.
- **Enhanced-FLAC / wet-dry sources** — produced by M3 enhancement; blended at render (Step 8).
