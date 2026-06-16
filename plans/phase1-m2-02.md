# Phase 1 · M2 · Step 2 — Decoding + probe (action plan)

Per-step action plan for Step 2 of the M2 milestone from [phase1-m2.md](phase1-m2.md).
The authoritative spec is [audio-pipeline.md § Decoding strategy](../design/audio-pipeline.md#decoding-strategy).
This step lands the **read-front of the audio engine**: turning a source file on disk into
**interleaved f32 PCM at the source's native rate**, plus a cheap `probe()` for the codec /
rate / channel / length metadata that feeds `TrackMeta` at M4 import.

It is pure decode — **no resample, no downmix, no cache, no command, no import orchestration**.
Resampling is Step 3 (rubato is the *only* resampler in the system, for determinism + the dry
signal); downmix is a render/export concern (Steps 8/10); the `import_speech_track` command that
*drives* this is M4. Decode runs on the import/background path, **never** the real-time cpal
callback, so the no-alloc/no-lock invariant does not bind here.

> **Revised (M2 revision, Commits 1a/4/5):** the *production* decode path no longer
> materializes the whole file. `decode.rs` exposes a streaming `open_source` →
> `SymphoniaSource`, with an ffmpeg-streaming `FfmpegSource` fallback; the whole-buffer
> `decode()` / `decode_via_ffmpeg()` described below now survive only as **test-support**
> oracles for the streaming-reader tests. See
> [phase1-m2-revision.md](phase1-m2-revision.md) (§1, Commits 4–5).

**Definition of done:** `core/src/audio/decode.rs` decodes the Symphonia-supported formats
([audio-pipeline.md § Symphonia](../design/audio-pipeline.md#symphonia-primary)) to interleaved f32 and
exposes `probe()`; `core/src/audio/ffmpeg.rs` provides the subprocess fallback + an
`ffmpeg_available()` check; `core/src/audio/mod.rs` exposes the shared `AudioError`; all are
covered by the test matrix below (ffmpeg-leg tests `#[ignore]`d when no system ffmpeg);
`cargo test -p core audio::`, `cargo clippy -p core -- -D warnings`, and `cargo fmt --check`
are green.

## Decisions locked in this step

- **Output is interleaved f32 at the *source* native rate and *source* channel count.**
  No resample, no downmix in this step. Symphonia decodes planar; we interleave via
  `SampleBuffer<f32>`. Keeping rate/channels native here makes rubato (Step 3) the single
  resampler in the system — a determinism + wet/dry-dry-signal invariant
  ([audio-pipeline.md § Resampling](../design/audio-pipeline.md#resampling)).
- **`decode()` owns the authoritative frame count; `probe()` length is best-effort.** Some
  formats (e.g. a header-less MP3) do not carry an exact frame count in metadata, so
  `probe()` returns `length_frames: Option<i64>` (`None` when unknown). The exact length used
  for `TrackMeta` comes from the full decode at M4 (decode also resamples; the project-rate
  `original_length_samples` is computed there). This step never decodes-to-count just to fill
  a probe.
- **ffmpeg outputs native rate + native channel layout too.** When the fallback is invoked,
  `ffmpeg`/`ffprobe` report the source rate/channels/codec and we pipe `-f f32le` at the
  **native** rate (no `-ar`/`-ac` rewrite) — so the fallback path produces the same shape as
  the Symphonia path and rubato still owns resampling.
- **Fallback routing is reject-driven, not extension-driven.** `decode()` always tries
  Symphonia first; only a Symphonia *format/unsupported* rejection (not an I/O error on a
  recognized format, and not a mid-stream decode error) routes to ffmpeg. If ffmpeg is
  unavailable, return a typed `FfmpegUnavailable` rather than guessing. This keeps AAC-LC/M4A
  on the in-process Symphonia path (M2 decision) and reserves the subprocess for
  HE-AAC/Opus/AC-3/DTS/video-with-non-AAC-audio.
- **`AudioError` is a typed core error now; message-key mapping is M4.** Per
  [conventions.md](../design/conventions.md) C3/D2, the frontend surfaces a snake_case `error_key`.
  Step 2 has no command, so it only defines the typed variants (with the suggested key names
  below); the proto-boundary mapping lands when M4 wires `import_speech_track`. Variants and
  their messages follow the hand-rolled `Display`/`Error` pattern used by `MetadataLoadError`
  / `EngineError` (no `thiserror` in this workspace). Any path embedded in an ffmpeg-stderr
  message is **redacted** per the local-first invariant ([CLAUDE.md](../CLAUDE.md)).
- **Prefer generated WAV fixtures over committed binaries.** WAV is raw PCM + a header, so the
  test harness writes WAV fixtures programmatically (deterministic, zero repo weight, every
  sample format covered). Only the *encoded* formats that need an external encoder — FLAC,
  MP3, AAC-LC `.m4a`, and the ffmpeg-only Opus/HE-AAC — are committed under
  `core/tests/fixtures/audio/`, each kept to a few hundred ms.

## Module surface

```rust
// audio/mod.rs
pub mod decode;
pub mod ffmpeg;

/// Decoded audio: interleaved f32 in [-1.0, 1.0] at the source's native rate.
pub struct DecodedAudio {
    pub samples: Vec<f32>,   // interleaved; len == frames * channels
    pub sample_rate: u32,    // source native rate
    pub channels: u16,       // source native channel count
}
impl DecodedAudio {
    pub fn frames(&self) -> usize;   // samples.len() / channels
}

/// Lightweight metadata read without decoding all samples (feeds TrackMeta at M4).
pub struct AudioProbe {
    pub codec: String,             // pinned identifier, e.g. "pcm_s16le" / "flac" / "mp3" / "aac"
    pub sample_rate: u32,
    pub channels: u16,
    pub length_frames: Option<i64>, // None when the format carries no exact frame count
}

/// Typed audio-engine error. `error_key()` gives the snake_case key the M4 command boundary
/// surfaces through Paraglide (conventions.md C3/D2).
pub enum AudioError {
    Io(std::io::Error),                       // -> "audio_io_error"
    UnsupportedFormat { codec: Option<String> }, // -> "decode_unsupported_format"
    DecodeFailed(String),                     // -> "decode_failed" (corrupt/truncated supported file)
    FfmpegUnavailable,                        // -> "ffmpeg_unavailable"
    FfmpegFailed { detail: String },          // -> "ffmpeg_failed" (path-redacted)
}
impl AudioError { pub fn error_key(&self) -> &'static str; }
```

```rust
// audio/decode.rs
/// Decode a source file to interleaved f32 PCM at its native rate, trying Symphonia first
/// and falling back to ffmpeg only on a format rejection.
pub fn decode(path: &Path) -> Result<DecodedAudio, AudioError>;

/// Read codec/rate/channel/length metadata without decoding all packets.
pub fn probe(path: &Path) -> Result<AudioProbe, AudioError>;
```

```rust
// audio/ffmpeg.rs
/// Cheap, cached check that a system `ffmpeg` (and `ffprobe`) is on PATH.
pub fn ffmpeg_available() -> bool;

/// Decode via the ffmpeg subprocess (native rate + channels, `-f f32le`).
pub(crate) fn decode_via_ffmpeg(path: &Path) -> Result<DecodedAudio, AudioError>;
pub(crate) fn probe_via_ffmpeg(path: &Path) -> Result<AudioProbe, AudioError>;
```

## Sub-steps

### 2a — `audio/decode.rs`: Symphonia decode + probe

- Open the file, hand the stream + extension hint to `symphonia::default::get_probe()`, select
  the default audio track, build the decoder. Decode loop: `format.next_packet()` →
  `decoder.decode(packet)` → `SampleBuffer::<f32>::copy_interleaved_ref` → append to `samples`.
- **End/error handling inside the loop:** `IoError(UnexpectedEof)` and the end-of-stream
  sentinel terminate cleanly (not an error); `ResetRequired` rebuilds the decoder and continues
  (chained streams); a `DecodeError` on a packet maps to `AudioError::DecodeFailed` (do not
  silently drop packets — a corrupt supported file is an error, not partial success).
- Convert **every** Symphonia sample format (u8/i16/i24/i32/f32/f64) to f32 in `[-1, 1]` via
  `SampleBuffer<f32>`; full-scale ints map to ≈ ±1.0.
- `probe()` reads only the format/track metadata: codec → a **pinned** identifier string,
  `sample_rate`, `channels`, and `n_frames` → `Some(i64)` or `None`.
- No `unwrap`/`expect`/`panic` without a justifying comment; `pub` items doc-commented.

### 2b — `audio/ffmpeg.rs`: subprocess fallback + availability

- `ffmpeg_available()`: run `ffmpeg -version` (and `ffprobe -version`), success → true; cache
  the result in a `OnceLock<bool>` so repeated decode calls don't re-spawn.
- `probe_via_ffmpeg()`: parse `ffprobe -show_streams -select_streams a:0` (JSON) for codec,
  rate, channels, and duration → frames.
- `decode_via_ffmpeg()`: spawn `ffmpeg -i <path> -map a:0 -f f32le -acodec pcm_f32le -` (native
  rate/channels — no `-ar`/`-ac`), read stdout fully, reinterpret little-endian bytes as
  interleaved f32. Non-zero exit / parse failure → `AudioError::FfmpegFailed { detail }` with
  the **path redacted** from any captured stderr.
- Wire the routing in `decode()`/`probe()`: Symphonia first; on `UnsupportedFormat` only, if
  `ffmpeg_available()` then delegate, else return `FfmpegUnavailable`.

### 2c — Fixtures + tests

- Add generated-WAV helpers (write header + interleaved PCM at a chosen sample format) and the
  committed encoded fixtures to `core/tests/fixtures/audio/`. See the test matrix below.

### 2d — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings` (incl. `missing_docs`,
  `unwrap_used`); `cargo test -p core audio::`.
- Cross-reference: confirm [audio-pipeline.md § Decoding strategy](../design/audio-pipeline.md#decoding-strategy)
  still matches the implemented codec list + fallback split; update it in the same commit if
  any behaviour was adjusted (CLAUDE.md doc-sync rule).
- One commit `1M2-02: audio decode + probe (Symphonia + ffmpeg fallback)` on `claude/1M2`,
  unsigned per the GPG-by-branch policy.

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests` in `decode.rs` / `ffmpeg.rs` per [conventions.md](../design/conventions.md)
A1, plus a small `core/tests/` integration module where a cross-format fixture is involved.
Group T = Symphonia happy path, E = error/edge, F = ffmpeg fallback (each `#[ignore]`d with a
note when `!ffmpeg_available()`), X = cross-cutting.

**T — Symphonia decode + probe (happy path)**

1. **WAV s16 mono, exact PCM.** Generated 48 kHz mono 16-bit WAV with known sample values →
   `decode()` returns `channels == 1`, `sample_rate == 48000`, `samples.len() == frames`, and
   each f32 equals the source PCM exactly (lossless int→f32 is exact within ≤ 1 LSB).
2. **WAV s16 stereo, interleaving + channel separation.** Distinct L/R content (e.g. L=+0.5,
   R=−0.5) → `channels == 2`, `samples.len() == frames * 2`, `samples[0]≈+0.5`, `samples[1]≈−0.5`
   (proves interleave order and that channels are **not** collapsed).
3. **Sample-format coverage.** 16-bit, 24-bit, and 32-bit-float WAV of the same tone all decode
   to f32 in `[-1, 1]`; full-scale `i16::MIN`/`i16::MAX` map to ≈ −1.0/+1.0; the f32 source is
   bit-exact.
4. **FLAC == WAV (lossless cross-format).** A FLAC fixture encoded from the *same* source as the
   mono WAV decodes to a PCM vector equal (within ≤ 1 LSB) to the WAV decode. Doubles as the
   Step-3 cache round-trip seed.
5. **MP3 decode (lossy, tolerance).** MP3 fixture → correct `sample_rate`/`channels`; frame
   count within the encoder-delay/gapless tolerance of expected; all samples finite and in
   `[-1, 1]` (no exact-value assert — lossy).
6. **AAC-LC `.m4a` decode.** Confirms `isomp4 + aac` features are wired and AAC-LC rides
   Symphonia (no ffmpeg): correct rate/channels, length within tolerance, samples in range.
   Assert (via the spy in F4) that the ffmpeg path was **not** taken.
7. **Empty-but-valid file.** A WAV with a valid header and 0 frames → `decode()` returns an
   empty `samples` vec (`frames() == 0`), not an error; `probe().length_frames == Some(0)`.
8. **`probe()` metadata matches decode.** For every happy-path fixture, `probe().sample_rate`
   and `.channels` equal the decoded values, and where `length_frames` is `Some`, it equals
   `decode().frames()`.
9. **Pinned codec identifiers.** `probe().codec` equals the exact expected string per format
   (`"pcm_s16le"`, `"flac"`, `"mp3"`, `"aac"`, …) — pins the value flowing into
   `TrackMeta.codec`, so a Symphonia upgrade can't silently rename it.
10. **`probe()` length is best-effort for headerless MP3.** A CBR MP3 without a Xing/Info
    header: Symphonia estimates `length_frames` from the file size and bitrate
    (`Some(estimate)` that may differ from the exact decoded count), not `None` as initially
    expected. `decode().frames()` is still the authoritative count.  The test verifies
    `probe()` succeeds, `decode()` returns a non-zero count, and the estimate (when present)
    is within a few MPEG frames (documents the best-effort-probe / authoritative-decode split).
11. **Determinism.** Decoding the same fixture twice yields byte-identical `samples` (supports
    content-addressing / reproducible export).

**E — error & boundary cases (no panics; typed errors)**

12. **Nonexistent path** → `AudioError::Io` (NotFound); `error_key() == "audio_io_error"`.
13. **Zero-byte file** → typed error (`UnsupportedFormat` or `DecodeFailed`), never a panic.
14. **Garbage with an audio extension** (text bytes in `bad.wav`) → Symphonia rejects →
    `UnsupportedFormat` (when ffmpeg absent) / routes to F (when present); no panic.
15. **Truncated supported file** (valid header, body cut mid-stream — truncate a good fixture's
    bytes in-test) → `AudioError::DecodeFailed`, not a partial-success silent return and not a
    panic.
16. **`error_key()` mapping is total.** Table-test every `AudioError` variant → its expected
    snake_case key (guards the M4 boundary mapping against an unmapped variant).

**F — ffmpeg fallback (`#[ignore]` when `!ffmpeg_available()`)**

17. **`ffmpeg_available()` is side-effect-free + cached.** Returns a `bool` without panicking;
    a second call doesn't re-spawn (assert via the `OnceLock` being set, or a call-count spy).
18. **Opus decodes via ffmpeg.** An `.opus` (or HE-AAC) fixture Symphonia can't handle decodes
    through the fallback → correct rate/channels, length within tolerance, samples in `[-1, 1]`.
19. **Fallback preserves native rate/channels.** The F18 decode's `sample_rate` equals the
    fixture's true source rate (no ffmpeg-side `-ar` resample) and channels are preserved —
    proving rubato stays the sole resampler.
20. **ffmpeg nonzero exit / bad input** → `AudioError::FfmpegFailed`, and the `detail` string
    contains **no absolute source path** (local-first redaction).

**E/F boundary — fallback unavailable**

21. **No ffmpeg + unsupported format** → with `ffmpeg_available()` forced false (PATH stub or an
    injected predicate), an unsupported file returns `AudioError::FfmpegUnavailable`
    deterministically, with **no subprocess spawn attempt** and no panic.

**X — routing / cross-cutting**

22. **Supported formats never invoke ffmpeg.** Via a spy (a test-only hook recording whether
    `decode_via_ffmpeg` was called) or by running with ffmpeg removed from PATH, assert
    WAV/FLAC/MP3/AAC-LC decode entirely on the Symphonia path — the fallback is reject-driven,
    not extension-driven.
23. **(Optional, fixture-dependent, `#[ignore]`-able) Audio-in-video container.** A small file
    with a non-AAC audio stream in a video container extracts its audio stream via the fallback;
    an AAC-LC-in-MP4 stream rides Symphonia (the audio codec, not the container, decides).

## Fixtures to add (under `core/tests/fixtures/audio/`)

Generated in-test (no commit): the WAV set for T1–T3, T7 (`sine_mono_48k_s16`,
`sine_stereo_48k_s16`, `ramp_44100_s24`, `tone_48k_f32`, `empty_48k`) and the truncation/garbage
inputs (E13–E15). Committed (need an encoder, keep ≤ a few hundred ms):
`sine_mono_48k.flac` (same source as the mono WAV, T4), `sine_mono_44100.mp3` (T5),
a header-less MP3 variant (T10), `sine_mono_44100_aac.m4a` (T6), and the ffmpeg-only
`sine_mono_48k.opus` / HE-AAC (F18, `#[ignore]`).

## Out of scope for Step 2

- **Resampling and the identity fast-path** — Step 3 (`resample.rs`); rubato is the only
  resampler.
- **Downmix / up-mix** — render/export (Steps 8, 10). Decode keeps native channel count.
- **The FLAC cache + `ensure_resampled`** — Step 3 (`flac.rs` / `cache.rs`).
- **The proto/command boundary + `error_key`→Paraglide mapping** — M4 (`import_speech_track`).
  Step 2 only defines the typed variants and suggested keys.
- **Bundling the ffmpeg binary** — M7. Dev/CI use a system `ffmpeg`; its tests `#[ignore]`
  when absent.
- **TrackMeta population** (`original_length_samples` at project rate, `codec`, …) — M4 import
  consumes `probe()`/`decode()` to fill it.
