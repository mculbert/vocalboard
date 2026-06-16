# Phase 1 — M2 Revision 2: export coherence (pull-based encode, cursor-bounded length, transcript relocation)

Status: **Commits 1–5 complete.** Cross-cutting revision triggered by a review of landed M2 Step 10
([phase1-m2-10.md](phase1-m2-10.md)) that surfaced module-coherence and abstraction issues
spanning Steps 7–10 (EDL cursor, renderer, FLAC encode, export) plus a small Step-9 playback
change. Runs **before** Step 11 wires the Tauri handlers, so the export/cursor signatures the
handlers consume settle here first. Step 11 has not been implemented; its plan
([phase1-m2-11.md](phase1-m2-11.md)) is updated in the same revision to match.

> **Plan-doc convention note.** Second *revision* plan (re-opens already-landed steps rather
> than advancing). Named `phase1-m2-revision2.md`, continuing the `phase1-mX-revision[N].md`
> pattern from [phase1-m2-revision.md](phase1-m2-revision.md).

## Why

1. **The "streaming" FLAC export sink isn't streaming.** `export.rs::FlacSink` accumulates the
   *entire* output in a `Vec<f32>`, then `encode_flac_24` builds another full copy
   (`MemSource` + `ByteSink`) — multiple GB resident for a feature-length export, the exact
   memory blow-up [revision 1](phase1-m2-revision.md) fixed for *import*. Meanwhile a genuinely
   streaming FLAC encoder already exists (`transcode.rs::FlacPullSource` + the STREAMINFO
   back-patch loop), and `Renderer` already implements [`PcmSource`](../src-tauri/core/src/audio/mod.rs).
   The two FLAC encode paths must become one, and export must reuse the streaming one.
2. **Push/pull abstraction mismatch.** The `ExportSink` trait is *push* (`write_frames`); the
   streaming FLAC encoder is *pull*. They fight, forcing the FLAC sink to buffer. Inverting
   export to a single **pull** model (each encoder pulls from a `PcmSource`) makes all three
   formats genuinely streaming and deletes `ExportSink` / `AnySink` / `FlacSink` / `run_export`.
3. **Project length is the cursor's job, not a threaded argument.** `project_length` rides as an
   argument through `export_track`/`export_mixed`/`run_export` *and* as a `Renderer` field (for
   `natural_end()`), even though the project timeline is fully defined by the tracks
   (`max(project_start + tree_length)` — there is **no** content past the last track). Moving the
   length bound into the `EdlCursor` (emit trailing silence to an explicit `end` when tracks
   exhaust early) removes `project_length`/`pad` from the export args, removes `project_length`
   and `natural_end` from the `Renderer`, and unifies single-track padding, mixed export, and
   bounded/partial export under one cursor contract.
4. **Module incoherence.** FLAC code is spread across `flac.rs`, `export.rs`, and `transcode.rs`;
   `flac_stream.rs` is actually a bit-sink, not a FLAC stream; `FfmpegEncodeSink` and `WavSink`
   sit in `export.rs` away from `ffmpeg.rs` and a (missing) `wav.rs`; `transcode.rs` is left with
   one thin function. Related code should share a file; each file should have one purpose.
5. **The `format` param is dead.** `export_track`/`export_mixed` take `_format` (ignored) and
   re-derive the codec from `audio_format_for(out)`. The param must become load-bearing (it
   carries future per-format options, e.g. `Mp3 { bitrate }`); the **caller** resolves the format
   from the extension and passes it in.
6. **Two near-identical export entry points.** `export_track` and `export_mixed` differ only by
   one-vs-many tracks in the cursor. A single `export_audio` over a caller-built cursor unifies
   single / all / future selected-track export.
7. **Transcript export is mis-housed.** `format_transcript` & friends read the timeline tree and
   touch no audio code — the Step-10 plan itself calls it "not an audio op". It belongs in
   `project/`. And the formatters materialize **all** turns into a `Vec` and sort, where a lazy
   **merged turn iterator** (the turn-level analog of `EdlCursor`, living in `project/tree.rs`)
   would let them stream in one pass.

## Decisions locked in this revision

- **Export is pull-based.** Each encoder is a free function `encode_*(src: impl PcmSource, out, …)`
  that pulls interleaved f32 from `src` and owns its output file — **including removing a partial
  `out` on failure** (replaces the `ExportSink::Drop` cleanup and `run_export`'s `remove_file`).
  `ExportSink`, `AnySink`, `FlacSink`, and `run_export` are deleted.
- **`Renderer` is the source; `MonoSource` is the only wrapper.** `Renderer` already implements
  `PcmSource`. Mono collapse is a thin `MonoSource<P: PcmSource>` (reports 1 channel, averages L/R
  per frame). `export_audio` picks `Box<dyn PcmSource>` = renderer or `MonoSource::new(renderer)`.
  No `ExportSource`, no pad-to-length source (padding lives in the cursor).
- **One streaming FLAC encoder.** The `FlacPullSource` adapter + STREAMINFO write/back-patch loop
  move from `transcode.rs` into `flac.rs` as `encode_flac_streaming(src: impl PcmSource, out) ->
  Result<i64, AudioError>`. Both `cache::transcode_to_cache` (import) and FLAC export call it.
  `encode_flac_24` (whole-buffer `MemSource` path) loses its last non-test caller → **demote to a
  `#[cfg(test)]` helper or delete** (verify no other callers first).
- **`EdlCursor` carries the length bound.** When constructed with an explicit `end = Some(e)` and
  every track exhausts at `pos < e`, the cursor emits one final silence `MixSlice`
  (`segments: vec![]`, length `e − pos`) then stops. `end = None` is unchanged (natural track
  drop-out already reaches the last track's end = project end). An empty-segments slice renders as
  silence (the renderer zeroes the region and sums zero segments) and is allocation-free (the
  renderer chunks it internally). The export caller pads a single track to project length by
  passing `end = Some(project_end)`, where `project_end = max(project_start + tree_length)` over
  the project's tracks — data the caller already holds.
- **`Renderer` drops `project_length` and `natural_end`.** Ctor becomes
  `Renderer::new(cursor, provider, max_fade_samples, project_rate)`. The natural stop position is
  whatever the cursor reaches.
- **Playback reports the natural-stop position from `frames_played`.** The pre-roll thread's
  natural-stop branch reports `project_pos(start, frames_played, project_rate, device_rate)` — the
  *same* formula the user-stop/pause path (`stop_session`) already uses — unifying both stop paths
  and deleting the `natural_stop_pos` plumbing. (At natural stop `frames_played == produced`, so
  the value is deterministic; it may round ≤1 project sample off the mathematical EDL end —
  accepted, <21 µs at 48 kHz, and the position is informational.)
- **Cursor assembly lives in `edl.rs`; renderer wrap stays in `render.rs`.** A pure
  `EdlCursor::build(tracks: &[(u32, i64, &ImplicitTimelineTree<Turn>)], start, end)` replaces
  `export.rs::build_renderer`'s cursor half; the handler builds the `tracks` slice from
  `ProjectState` and calls it, then `Renderer::new`. The audio crate never depends on
  `ProjectState`. (Supersedes the Step-11 plan's "promote `build_renderer`" item.)
- **`format` is honored, not re-derived.** `export_audio(renderer_or_mono, format, out)` uses the
  passed `format`; the caller calls `audio_format_for(out)` to resolve it (extension wins enforced
  at the handler boundary, per [audio-pipeline.md § Format selection](../design/audio-pipeline.md#format-selection)).
- **Transcript moves to `project/transcript.rs`.** `transcript_format_for` returns
  `Option<TranscriptFormat>` (the handler maps `None → export_unsupported_format`), so the project
  module no longer borrows `AudioError`. The formatters take a turn **iterator** and build their
  output in one pass.
- **Merged turn iterator in `project/tree.rs`.** A k-way merge over per-tree turn iterators (each
  already start-ordered) emitting the globally-next `(start, end, &Turn)` in project order — the
  turn-level analog of `EdlCursor`. Single-track transcript uses `tree.iter()` directly; multi-track
  uses the merge. No `Vec`-materialize-and-sort.
- **No persisted-format change.** This revision touches only in-memory types, module layout, and
  function signatures — no blob tag, snapshot, SQLite, or `settings.json` change, so **no
  migration** (CLAUDE.md data-integrity invariant unaffected).

## Module layout (after)

```
src-tauri/core/src/audio/
  decode.rs       decode dispatcher (Symphonia → ffmpeg fallback) — UNCHANGED name
  ffmpeg.rs       ffmpeg decode fallback + encode_via_ffmpeg (pull → stdin)   [+encode]
  flac.rs         encode_flac_streaming (pull) + FlacPullSource + decode_flac  [+streaming]
  bit_sink.rs     WriteBitSink (flacenc BitSink over Write)        [renamed from flac_stream.rs]
  wav.rs          encode_wav_streaming (pull, f32le, header back-patch)        [new]
  cache.rs        resampled-cache paths + ensure_resampled + transcode_to_cache [+transcode]
  edl.rs          EdlCursor (+ trailing-silence-to-end) + EdlCursor::build     [+build, +pad]
  render.rs       Renderer (no project_length / natural_end) + MonoSource      [−len, +Mono]
  export.rs       export_audio + audio_format_for + AudioFormat                [pull rewrite]
  ...             (transcode.rs DELETED; flac_stream.rs RENAMED)
src-tauri/core/src/project/
  tree.rs         + merged turn iterator (MergedTurns)
  transcript.rs   format_transcript / fmt_vtt / fmt_markdown / transcript_format_for  [moved]
```

## Commits

Sequenced on compilable/landable seams; each leaves `cargo test -p core` green.

### Commit 1 — `EdlCursor` length bound + `Renderer`/playback simplification

The architectural core; lands the cursor contract that later commits depend on.

- **`edl.rs`**: in `EdlCursor::next`, when `end = Some(e)`, `self.tracks` is empty, and
  `self.pos < e`, emit `MixSlice { start_sample: pos, length_samples: e - pos, segments: vec![] }`
  and set `pos = e`. (`end = None` path untouched.)
- **`render.rs`**: drop the `project_length` field, ctor param, `project_length()`, and
  `natural_end()`. `Renderer::new(cursor, provider, max_fade_samples, project_rate)`.
- **`playback.rs`**: drop `natural_stop_pos` from `play_from` / `run_preroll`; the natural-stop
  branch emits `project_pos(start, frames_played, project_rate, device_rate)`.
- **Mechanical**: update every `Renderer::new` call (render.rs tests, playback.rs tests,
  source_provider.rs test) to the 4-arg form. `export.rs::build_renderer` drops the
  `project_length` arg to `Renderer::new` but **keeps** its own `project_length` → `run_export`
  padding for now (rewritten in Commit 3).

**Tests**
1. **Cursor pads to explicit end.** A single 100-frame track, `end = Some(200)` → slices tile
   `[0,200)`; the tail `[100,200)` is one empty-segments slice. `end = Some(80)` → clipped at 80,
   no silence slice. `end = None` → stops at 100 (unchanged).
2. **Renderer renders cursor-padded silence.** `Renderer` over the padded cursor yields 200 frames;
   `[100,200)` is exactly zero (replaces the export-side `a3` pad assertion at the renderer level).
3. **Multi-track early exhaustion.** Two tracks ending at 150, `end = Some(200)` → silence `[150,200)`.
4. **Playback natural stop position.** In-memory backend: a project ending at frame N reports
   `playback_stopped { position_samples ≈ N }` (within ≤1 frame), deterministic across two runs.
5. **No `project_length` references remain** (compile-level: the field/methods are gone).

### Commit 2 — FLAC consolidation + module renames

- Move `FlacPullSource` + the STREAMINFO write/back-patch encode loop from `transcode.rs` into
  `flac.rs` as `pub(crate) fn encode_flac_streaming(src: impl PcmSource, out: &Path) ->
  Result<i64, AudioError>` (returns the project-rate frame count).
- `cache.rs`: absorb the (now-thin) `transcode_to_cache` = `open_source` → `StreamingResampler`
  → `encode_flac_streaming`. Delete `transcode.rs`; update `mod.rs` and imports.
- Rename `flac_stream.rs` → `bit_sink.rs` (no API change; `WriteBitSink` stays `pub(super)` —
  re-export path updates in `flac.rs`).
- `encode_flac_24`: confirm `FlacSink` is its only non-test caller; demote to `#[cfg(test)]`
  helper or delete. The streaming round-trip / determinism / STREAMINFO tests (`SW*`, `TC*`) move
  with the code to `flac.rs`.

**Tests**: existing `transcode.rs` `TC*`/`SW*` and `flac.rs` `C*` suites relocate and stay green;
add one asserting `encode_flac_streaming` over a `BufferedSource` round-trips within the 24-bit
bound (the encoder is now exercised independent of resampling).

### Commit 3 — pull-based export rewrite

- **`render.rs`**: add `MonoSource<P: PcmSource>` (channels → 1; `read` pulls stereo from inner,
  writes `(L+R)/2`).
- **`wav.rs`**: `encode_wav_streaming(src: impl PcmSource, out)` — placeholder header, stream
  frames, back-patch sizes in finalize, remove `out` on failure. (Lift the WAV writer out of
  `export.rs`.)
- **`ffmpeg.rs`**: `encode_via_ffmpeg(src: impl PcmSource, out, codec)` — spawn ffmpeg, loop
  pull→stdin, close stdin, wait, check status; remove `out` + stderr temp on failure.
- **`edl.rs`**: `pub(crate) fn EdlCursor::build(tracks, start, end) -> EdlCursor` (cursor half of
  the old `build_renderer`).
- **`export.rs`**: delete `ExportSink`, `AnySink`, `FlacSink`, `WavSink`, `FfmpegEncodeSink`,
  `run_export`, `build_renderer`, `export_track`, `export_mixed`. Add:
  ```rust
  pub fn export_audio(
      renderer: Renderer<impl SourceProvider>,   // caller built the cursor (incl. end-pad)
      format: AudioFormat, mono: bool, out: &Path,
  ) -> Result<(), AudioError>
  ```
  builds `Box<dyn PcmSource>` (renderer or `MonoSource`), dispatches: `Flac →
  flac::encode_flac_streaming`, `Wav → wav::encode_wav_streaming`, `Mp3|Ogg|Aac` →
  ffmpeg availability check then `ffmpeg::encode_via_ffmpeg`. `audio_format_for` + `AudioFormat`
  stay in `export.rs`.

**Tests** (port the Step-10 `A*`/`F*`/`X*` audio matrix to the new entry point)
- `A1`/`A2` round-trip (FLAC bound, WAV exact) via `export_audio` over a cursor with
  `end = Some(len)`.
- `A3` silence pad now expressed through the cursor (`end = Some(200)` over a 100-frame track).
- `A4` mono via `MonoSource` (WAV + FLAC headers report 1 channel).
- `A5` mixed sum/clamp over a 2-track cursor.
- `A8` not-cached, `A9` determinism, `X21` write-failure-no-partial (per-encoder cleanup),
  `X23` renderer parity (export == direct render), `F19`/`F20` ffmpeg present/absent.
- **New**: each encoder removes a partially-written `out` on a mid-stream failure (inject a
  failing writer / unwritable path *after* header).

### Commit 4 — transcript relocation + merged turn iterator

- **`project/tree.rs`**: `MergedTurns` — k-way merge over per-tree turn iterators yielding
  `(start_sample, end_sample, &Turn)` (or `Arc<Turn>`) in project order. Each entry is
  `(project_start_sample, &tree)`: turns are positioned at `project_start_sample + tree-local
  start_sample`, so tracks beginning at different project offsets interleave in true global order
  (mirroring `EdlCursor`'s per-track offset). Single-track fast path is just `tree.iter()`.
- **`project/transcript.rs`** (moved from `audio/export.rs`): `TranscriptFormat`,
  `format_transcript` (now driving `MergedTurns`, taking `&[(project_start_sample, &tree)]`),
  `fmt_vtt`/`fmt_markdown` taking `impl Iterator<Item = (i64, i64, &Turn)>` and building output
  single-pass, `samples_to_timestamp`, `turn_words_text`, `speaker_name`.
  `transcript_format_for(path) -> Option<TranscriptFormat>`.
- Remove the transcript code + `T*` tests from `export.rs`; relocate `T10`–`T18` to
  `transcript.rs` (pinned VTT/Markdown strings unchanged). Add `MergedTurns` ordering tests in
  `tree.rs`: `MT1` (two tracks at offset 0, interleaved turn starts → globally start-ordered
  output) and `MT2` (tracks at different `project_start_sample` → ordered by project position,
  not local start).

### Commit 5 — doc-sync + gate

- Update [audio-pipeline.md § Export pipeline](../design/audio-pipeline.md#export-pipeline): pull-based
  encoders, cursor-bounded length (no `project_length`/`natural_end`), transcript in `project/`.
- Update [phase1-m2-11.md](phase1-m2-11.md) (see "Step-11 plan changes" below).
- Re-check [command-surface.md](../design/command-surface.md) export rows (wire surface unchanged — the
  `format` field stays advisory; only the internal Rust signatures changed).
- `cargo fmt --check`; `cargo clippy -p core -- -D warnings`; `cargo test -p core`.
- One commit per the above; all on `claude/1M2`, unsigned.

## Step-11 plan changes (`phase1-m2-11.md`)

Applied in this revision (Step 11 unimplemented):

- **Renderer assembly**: replace "promote `export.rs::build_renderer`" with "the handler builds the
  `tracks` slice from `ProjectState` → `EdlCursor::build(tracks, start, end)` → `Renderer::new(cursor,
  provider, max_fade_samples, project_rate)`". `project_length` is no longer a renderer arg.
- **Padding / length**: the handler resolves `project_end = max(project_start + original_length)`
  from `TrackMeta` and passes `end = Some(project_end)` to `EdlCursor::build` for full-length
  export (or the requested `[start, end)` for bounded export). No `project_length` argument to the
  export functions.
- **Export call**: `export_track`/`export_mixed` handlers both call the single `export_audio(renderer,
  audio_format_for(output_path)?, mono, output_path)`; the handler resolves the format from the
  extension (the `format` wire field stays advisory).
- **Transcript call**: `transcript_format_for(output_path)` now returns `Option`
  (`None → export_unsupported_format`); `format_transcript` lives in `project::transcript` and takes
  the project's trees (via the merged iterator) + speaker map.
- **`play_from`**: unchanged signature; note the renderer no longer carries `natural_end` — the
  engine derives the stop position from `frames_played`.

## Out of scope

- **Bundled mp3/ogg/aac encoders** — M7 (system ffmpeg only; `#[ignore]` when absent), unchanged.
- **Word/RTF transcript, section headers, per-track mute** — post-Phase-1 / Phase 2, unchanged.
- **Any Step-9 behavior beyond the natural-stop position source** — the ring/callback/resampler
  contracts and the deadlock-freedom invariants are untouched.
- **Splitting Symphonia internals out of `decode.rs`** into `symphonia.rs` — `decode.rs` is the
  decode *dispatcher* (tries Symphonia, falls back to ffmpeg), so the name is correct; an internal
  split is low-value and deferred.
