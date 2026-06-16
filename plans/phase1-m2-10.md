# Phase 1 · M2 · Step 10 — Export (action plan)

Per-step action plan for Step 10 of the M2 milestone from [phase1-m2.md](phase1-m2.md). The
authoritative spec is [audio-pipeline.md § Export pipeline](../design/audio-pipeline.md#export-pipeline) and
[command-surface.md § Export commands](../design/command-surface.md#export-commands) (`export_track` /
`export_mixed` / `export_transcript`). Export **reuses the Step-8 renderer** for audio and reads
the M1 timeline tree directly for transcript.

This step is the **offline counterpart to playback**: same EDL + renderer, but rendered to a file
through an encoder sink rather than the real-time ring. Plus a non-audio transcript formatter
(VTT / Markdown). It defines the export logic; the proto/command wiring + the Tauri handlers land
in Step 11 (this step exposes the callable functions and their error codes).

**Definition of done:** `core/src/audio/export.rs` exports a track / mixed output to FLAC or WAV
(mp3/ogg/aac via ffmpeg when available, else `export_unsupported_format`), with optional mono
collapse and silence-padding to project length; and exports a transcript to VTT / Markdown honoring
`include_cut_words`, format chosen by file extension. The test matrix below passes; `cargo test -p
core audio::`, `cargo clippy`, `cargo fmt --check` green.

## Decisions locked in this step

- **Audio export reuses the Step-8 renderer offline.** Build the merged EDL cursor for the
  requested track(s), wrap a `Renderer`, and pull the whole range in chunks to an encoder sink — no
  ring buffer, no real-time constraint. **Pad with silence to the project total length**
  ([audio-pipeline.md § Track export](../design/audio-pipeline.md#track-export)) so all exports share a
  common length.
- **`export_track` exports one track; `export_mixed` merges all non-muted tracks** at the mix step
  (same renderer, a cursor over the full track set). Track-level "muted" exclusion for
  `export_mixed` is a track property (Phase 1 mixes all non-cut/non-muted audio); document the
  Phase-1 set as "all tracks" until per-track mute lands.
- **Formats: FLAC (default) + WAV native in M2.** FLAC reuses Step 3's `encode_flac_24` (24-bit,
  project rate). **WAV** is written natively (32-bit float or 24-bit int — lock **f32 WAV** for a
  lossless, exact round-trip in tests; document). `mp3` / `ogg` / `aac` route through the **ffmpeg
  subprocess** (pipe f32 PCM in, encode out) when `ffmpeg_available()`, else return
  `AudioError` → `export_unsupported_format`. (Bundled encoders are M7.)
- **Encoder sink is streaming.** A `ExportSink` trait (`write_frames(&[f32])` + `finalize()`) lets
  FLAC/WAV/ffmpeg share the chunked render loop and keeps peak memory bounded for long projects.
- **Mono collapse** (the `mono` param): sum channels and **divide by 2** ([audio-pipeline.md § Track
  export](../design/audio-pipeline.md#track-export)); applied after render, before the sink.
- **Exports are not cached** in `.vbdata/` — written directly to the user path
  ([data-model.md § Derived files](../design/data-model.md#derived-files)).
- **Transcript export reads the tree directly** (not an audio op). Format is chosen by the
  **output-file extension** (`.vtt` → VTT, `.md`/`.markdown` → Markdown), overriding the param if
  they disagree (extension wins, per [audio-pipeline.md § Format selection](../design/audio-pipeline.md#format-selection));
  an unknown extension → `export_unsupported_format`. **Both formats are by-turn**, like the
  Markdown export: **one cue / paragraph per turn**, not per word. **VTT** emits one cue per turn
  spanning `[turn project start, turn project start + turn_duration)` — both endpoints come from the
  tree's prefix-sum walk + `turn_duration` (no per-word position: a `Word`'s `source_onset_sample`
  lives in the *source/cache* timeline and is `Option`, so per-word **project** timestamps are not
  cheaply available, whereas turn-level ones are exact) — formatted `HH:MM:SS.mmm` at the project
  rate, with the speaker name carried as a WebVTT voice tag (`<v Speaker Name>…`); **Markdown** emits
  the same turns as speaker-labelled paragraphs. Each cue/paragraph's text is the turn's words joined;
  `include_cut_words` toggles whether `is_cut` words appear (default false → omit). Word-level
  (karaoke) timing and Word/RTF are deferred (post-Phase 1).
- **Speaker names come from metadata.** The transcript formatter takes the speaker-id→name map
  (from `SpeakerMeta`); `speaker_id == None` renders "[None]". The caller (Step 11/M-time) supplies
  it; the formatter is pure over (tree, speaker map, options).
- **Format-specific options ride through `format`; no work now.** Per-format encoder options
  (bitrate, compression level) are *not* in Phase-1 scope, but the design already accommodates them
  for free: `AudioFormat` elaborates from unit variants to struct variants later (`Mp3 { bitrate }`,
  `Flac { compression }`) and the `ExportSink` dispatches per-format, so no separate hook/parameter is
  needed. The one constraint is at the **command schema**: `export_track`'s `format` string-enum is
  not mutated in place when options arrive — add a compatible optional field or bump to v2 (per
  [CLAUDE.md](../CLAUDE.md) "never change a command's schema in place"). Nothing to build in Step 10/11.
- **Error codes** reuse the existing surface: `export_unsupported_format` (unknown extension /
  unavailable encoder), `audio_io_error` (write failure). No new format-tag / schema change.

## Module surface

```rust
// audio/export.rs

pub enum AudioFormat { Flac, Wav, Mp3, Ogg, Aac }
// Later milestones elaborate variants with format-specific options (e.g. Mp3 { bitrate },
// Flac { compression }); the ExportSink already dispatches per-format, so that rides through
// the existing `format` parameter with no new hook. See "Decisions locked" below.

/// Render `[0, project_length)` for one track to `out` via the chosen encoder.
/// `mono` collapses to one channel (sum/2). `max_fade_samples` + `project_rate` MUST match the
/// playback caller's so the export is sample-for-sample identical to playback (test X23).
/// Unsupported/unavailable encoder → AudioError → export_unsupported_format.
pub fn export_track(
    tree: &ImplicitTimelineTree<Turn>, provider: impl SourceProvider,
    track_id: u32, project_start_sample: i64, project_length: i64,
    max_fade_samples: usize, project_rate: u32,
    format: AudioFormat, mono: bool, out: &Path,
) -> Result<(), AudioError>;

/// Mixed export of all (non-muted) tracks. Same options, no single track_id.
pub fn export_mixed(
    trees: &[(u32, i64, &ImplicitTimelineTree<Turn>)], provider: impl SourceProvider,
    project_length: i64, max_fade_samples: usize, project_rate: u32,
    format: AudioFormat, mono: bool, out: &Path,
) -> Result<(), AudioError>;
// Both build the EdlCursor (TrackCursor::at → EdlCursor::new) and wrap a Renderer
// (Renderer::new(cursor, provider, project_length, max_fade_samples, project_rate)) exactly as the
// Step-11 play_from caller does — factor that cursor+renderer assembly into one shared builder so
// playback and export render identically. `provider` is generic (tests pass MockProvider); Step 11
// supplies the production CacheSourceProvider (over the `.vbdata` cache).

pub enum TranscriptFormat { Vtt, Markdown }

/// Format the transcript from the timeline tree(s). `speakers` maps id→name.
/// `include_cut_words` keeps is_cut words. Returns the formatted string.
pub fn format_transcript(
    tracks: &[(u32, &ImplicitTimelineTree<Turn>)],
    speakers: &BTreeMap<u64, String>, sample_rate: u32,
    format: TranscriptFormat, include_cut_words: bool,
) -> String;

/// Pick the format from the output extension, or Err(unsupported) for unknown ones.
pub fn transcript_format_for(path: &Path) -> Result<TranscriptFormat, AudioError>;
pub fn audio_format_for(path: &Path) -> Result<AudioFormat, AudioError>;
```

## Sub-steps

### 10a — `ExportSink` + FLAC/WAV native encoders

- The streaming sink trait; FLAC (via `encode_flac_24`) and native f32 WAV implementations.
  Note the file-ownership asymmetry the trait spans: the native FLAC/WAV sinks have Rust write the
  bytes at `out`; the ffmpeg sink (10c) hands `out` to the subprocess, which owns the file write.
  Both take `out: &Path`; only who writes the file differs.

### 10b — `export_track` / `export_mixed` (render + pad + mono)

- Build cursor(s) + `Renderer` via the **shared cursor+renderer builder** (same assembly the Step-11
  `play_from` caller uses; pass `max_fade_samples` + `project_rate` matching playback); chunked pull
  to the sink; silence-pad to `project_length`; mono collapse; write to `out`.

### 10c — ffmpeg encode path

- mp3/ogg/aac: pipe f32 PCM to an `ffmpeg` subprocess (`-f f32le … -c:a <codec> out`). **ffmpeg
  writes the output file directly** — we feed stdin in `write_frames`, close stdin + wait in
  `finalize`, and never capture/read back encoded data. On `!ffmpeg_available()` return the
  unsupported error (test `#[ignore]`d when no system ffmpeg).

### 10d — transcript (VTT / Markdown) + extension routing

- Walk the tree(s); build cues/paragraphs; honour `include_cut_words`; `transcript_format_for` /
  `audio_format_for` by extension.

### 10e — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings`; `cargo test -p core audio::`.
- Confirm [audio-pipeline.md § Export pipeline](../design/audio-pipeline.md#export-pipeline) matches; record
  the f32-WAV + extension-wins decisions (CLAUDE.md doc-sync).
- One commit `1M2-10: export (audio FLAC/WAV + transcript VTT/Markdown)` on `claude/1M2`, unsigned.

## Test cases (for the implementer)

Inline unit tests + a `core/tests/` round-trip module (export → decode → compare). In-memory
`SourceProvider` as in Step 8. Groups: A = audio export, T = transcript, F = ffmpeg
(`#[ignore]` when absent), X = cross-cutting.

**A — audio export**

1. **FLAC round-trip.** Export a synthetic track to FLAC, decode it back (Step 2/3) → PCM equals
   the renderer's expected output within the 24-bit bound; length == `project_length`.
2. **WAV round-trip (exact).** Export to f32 WAV, decode → **bit-exact** to the renderer output
   (lossless); correct rate/channels/length header.
3. **Silence padding to project length.** A track shorter than `project_length` exports a file of
   `project_length` frames, trailing samples == silence.
4. **Mono collapse.** `mono = true` → output has 1 channel and each sample == `(L+R)/2` of the
   stereo render; FLAC/WAV header reports 1 channel.
5. **Mixed export sums tracks.** `export_mixed` over two overlapping tracks → decoded PCM == the
   clamped sum (matches the Step-8 mix), length == `project_length`.
6. **Default format is FLAC.** `audio_format_for("out.flac") == Flac`; a `.wav` path → `Wav`.
7. **Unknown audio extension.** `audio_format_for("out.xyz")` → `AudioError` →
   `export_unsupported_format`.
8. **Not cached.** Export writes only to `out`; nothing is created under `.vbdata/`
   (assert no `resampled/`/`enhanced/` side effects).
9. **Determinism.** Same project → byte-identical export file, twice (reproducible export).

**T — transcript**

10. **VTT structure.** A synthetic 2-turn, 2-speaker transcript → a pinned VTT string: `WEBVTT`
    header, **one cue per turn** with `HH:MM:SS.mmm -->` timestamps spanning
    `[turn start, turn start + turn_duration)` (from the prefix-sum walk) and the speaker name as a
    `<v Speaker Name>` voice tag.
11. **Markdown structure.** Same transcript → a pinned Markdown string with speaker-labelled
    paragraphs.
12. **`include_cut_words = false` omits cut words.** A turn with one `is_cut` word → that word's
    text is absent from both formats; timestamps/positions of remaining words are unaffected.
13. **`include_cut_words = true` keeps them.** The cut word appears.
14. **[None] speaker.** A turn with `speaker_id == None` renders "[None]".
15. **Timestamp conversion.** The pure formatter: a project sample `N` at rate `R` → `N/R` seconds
    formatted `HH:MM:SS.mmm` (boundary: 0 → `00:00:00.000`; rounding pinned). Independent of turn/word.
16. **Extension routing.** `transcript_format_for("t.vtt") == Vtt`, `.md`/`.markdown` → Markdown,
    `.txt` → `export_unsupported_format`.
17. **Extension overrides param** (extension wins, per the format-selection spec): a `format = Vtt`
    arg with a `.md` path produces Markdown (or the caller errors per the chosen contract —
    lock and test one).
18. **Empty transcript.** A project with an empty/whitespace transcript → a valid header-only
    VTT/Markdown, no panic.

**F — ffmpeg formats (`#[ignore]` when `!ffmpeg_available()`)**

19. **mp3 export.** Export to `.mp3` via ffmpeg → the file exists, decodes (Step 2 fallback or
    Symphonia mp3) to ~the expected length (lossy tolerance), samples in `[−1, 1]`.
20. **No ffmpeg → unsupported.** With `ffmpeg_available()` forced false, `.mp3`/`.ogg`/`.aac`
    export returns `export_unsupported_format` deterministically, no subprocess spawn attempt.

**X — cross-cutting**

21. **Write failure → io error.** An unwritable `out` path → `AudioError::Io` →
    `audio_io_error`; no partial/zero-byte file left behind (or removed on failure).
22. **No SQLite connection.** Export consumes a tree + `SourceProvider`; no `Db` in scope.
23. **Renderer parity.** The exported audio equals `playback`'s rendered frames for the same range
    (same Step-8 renderer) — playback and export agree sample-for-sample (FLAC within bound, WAV
    exact).

## Fixtures to add

- Pinned expected VTT + Markdown strings live inline as test constants (no committed files).

## Out of scope for Step 10

- **The proto `Export*Params`, the `export_*` Tauri handlers, and TS bindings** — Step 11.
- **Bundled mp3/ogg/aac encoders** — M7; M2 uses a system ffmpeg, `#[ignore]` when absent.
- **Word / RTF transcript** — post-Phase 1.
- **Section headers in the transcript** (Phase 2) and per-track mute selection for `export_mixed`
  (Phase 1 mixes all non-cut/non-muted audio).
- **Sourcing the speaker map** end-to-end — Step 11 / command layer supplies it from `SpeakerMeta`.
