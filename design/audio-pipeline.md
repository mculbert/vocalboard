# Audio Pipeline

## Decoding strategy

All audio decoding in Rust produces **f32 PCM at the project sample rate**. Two decoders are used:

### Symphonia (primary)

[`symphonia`](https://github.com/pdeljanov/Symphonia) handles the common audio formats natively in pure Rust:

- WAV, AIFF, FLAC (native)
- MP3 (via the `symphonia-codec-mp3` feature)
- OGG Vorbis
- **AAC-LC** and **ALAC**, both in the MPEG-4/M4A container (shared `symphonia-format-isomp4` demuxer)

Symphonia is the **only decoder used for audio-only files** in these formats. It is linked statically; no system libraries needed.

### ffmpeg (fallback)

A bundled `ffmpeg` subprocess (or dynamically-loaded `libavcodec`) is invoked for what Symphonia cannot decode:

- **HE-AAC v1/v2** (SBR/PS extensions) — Symphonia decodes only AAC-LC
- **Opus** — Symphonia has no Opus decoder
- **AC-3 / E-AC-3, DTS** — broadcast/video audio codecs
- Video containers (MP4, MOV, MKV, WebM) carrying a **non-AAC-LC** audio stream — to demux + decode the audio. (An AAC-LC stream in such a container rides the Symphonia path; the deciding factor is the *audio codec*, not the container.)

The ffmpeg build is **LGPL** (no GPL codecs). It is called via a subprocess pipe rather than Rust bindings to avoid linking complexity; audio is extracted as raw PCM and piped to the Rust decoder.

> **Forward-looking (Phase 1):** Move some video/exotic formats to ffmpeg-only if they require codec infrastructure not worth shipping in Phase 1.

### Resampling

At **import**, the source is decoded and resampled to the project sample rate with `rubato` sinc interpolation (quality preset from `resampling_quality` in settings), then written to `<project>.vbdata/resampled/<track_id>.flac` (24-bit integer FLAC, keyed by the stable `TrackMeta.id`) by a **background task**. The resampled path (`resampled/<track_id>.flac`) is **fully derived from the track ID** — it is not stored in `TrackMeta`. Resampling the **source** on the fly during playback/export was considered and rejected — per-track decode + arbitrary-rate resampling isn't reliably fast enough for low-latency preview — so this resampled cache becomes the audio source the EDL engine reads from (decoding 24-bit FLAC back to f32 is cheap). (This is distinct from the single post-mix **output** resample to the device rate done on the pre-roll thread at playback — see [§ Output stream](#output-stream); that is one stereo pass on already-mixed audio, usually a passthrough, not per-track source resampling.) The cache trades a small f32→int24 quantization (≈ −144 dB floor, far below any speech room tone) for roughly half the disk of raw f32.

**The cache is the read source for *every* track, not only ones that needed resampling.** A source already at the project rate skips the resample computation but is still transcoded to the cache. The cache is therefore a single, cheap, seekable, **codec-agnostic** decode path (the real-time pre-roll thread never touches MP3/AAC/ffmpeg decoders), is sample-accurate to seek (lossy sources are not), and is the canonical **dry** signal for the wet/dry enhancement blend. The cost is that an already-at-rate *lossless* source (project-rate WAV/FLAC) is duplicated on disk for read-path uniformity. Reading such sources directly to avoid the duplicate is a deliberate **Phase 2** optimization, not taken in Phase 1.

> **Assumption — the cache is on local-latency storage.** The real-time pre-roll thread reads the cache with blocking I/O, relying on the ring buffer's cushion to absorb producer jitter. That cushion is sized for local SSD/HDD read latency (sub-millisecond to low-millisecond). The cache lives **inside the project bundle** (`<project>.vbdata/`), so a project kept on a **network mount or cloud-sync folder** puts the real-time read source on that medium: cold reads then cross the network on the playback path, and cloud **on-demand / placeholder** files (Windows Files On-Demand, macOS "Optimize Storage", Dropbox Smart Sync) can block a `read()` for seconds — or indefinitely when offline — while the file hydrates. Either drains the ring and produces dropouts (or, with an unbounded read, a stall). We **deliberately do not** mitigate this in Phase 1: keeping the cache in the bundle preserves project portability and on-disk visibility (the explicit tradeoff chosen over network insulation), and an editor's interactive seek/scrub rules out the fat-buffer approach a linear player would use. The accepted posture is a **user-facing documentation caveat** ("performance may suffer when projects are stored on cloud/network drives — prefer local storage if you have connectivity issues"), leaving the call to the user. A local prefetch/scratch mirror feeding the real-time path while the canonical cache stays in-bundle is a possible **Phase 2** option, not taken now.

The transcode decode→resample→FLAC-encode pipeline streams its **input** (Symphonia/ffmpeg packets are pulled chunk-by-chunk and resampled on demand, so the full-length f32/int24 PCM is never resident — see the `ensure_resampled` streaming pipeline in `src-tauri/core/src/audio/cache.rs`). The FLAC **encoder** (`flacenc`) also streams its output: each frame is written to the file as it is encoded and then dropped, so no compressed-stream buffer accumulates in RAM. To satisfy the FLAC spec (STREAMINFO `min_block_size` must equal `max_block_size` for a fixed-block stream), the encoder writes `min == max == 4096` **directly** into STREAMINFO (the final short frame is the permitted FLAC exception) — first in the placeholder header, then again when `total_samples` and the MD5 digest are backpatched after the last frame; there is no separate post-encode field rewrite. Import peak memory is therefore bounded by the resample buffer plus one encode window, not by the full compressed output.

The cache is **derived**: if `resampled/<track_id>.flac` is missing on open (deleted, or the import was interrupted before the background task finished), it is regenerated from the source — the same posture as a missing enhanced track. Regeneration requires the **source** to be present, so the open-time sweep regenerates only tracks whose source path resolves; a track whose source is *also* missing is handled by [audio file resolution](data-model.md#audio-file-resolution) (the cache regenerates on a later open, once the source has been relocated). The original source file must therefore remain accessible (for re-resampling, re-import, and enhancement).

**Determinism and invalidation contract.** The transcode is deterministic *within a build* — the same source, project rate, and quality preset produce byte-identical FLAC — but the cache is deliberately **not** content-addressed and carries **no format version**: a future `rubato`/`flacenc` change may alter the bytes, which is harmless because the cache is regenerated on demand and is never hashed, journaled, or compared across versions (the opposite posture from the content-addressed blob store, where every persisted format ships a tag-byte bump + a migration). One consequence is load-bearing: regeneration triggers on the file's **absence**, not its content — so a changed *source* is picked up via re-import, but a future change to the **cache encoding itself** (block size, bit depth, encoder version) would *not* invalidate an existing file (an old-format cache at the path is read as-is). If the cache encoding is ever changed, the open-time sweep must invalidate by bumping a cache **generation** (e.g. a `resampled-v2/` directory) or stamping a format byte the sweep checks — existence alone is not enough.

## Edit Decision List

The Edit Decision List (EDL) is **maintained incrementally**, not rebuilt by a transcript pass at play time. Each turn's `splices` vec (see [data-model.md § Turn payload](data-model.md#turn-payload-the-unit-stored-in-the-blob-store)) is the persisted EDL fragment for that turn; the playback/export EDL is the concatenation of those fragments along the timeline, merged across tracks.

### Initial EDL (at import)

Each turn starts with exactly **one** `SpliceKind::Source` splice covering the turn's speech time **plus its `post_turn_silence`**. Source audio is preserved for the time between turns; Phase 1 mixes all non-cut/non-muted audio from every track (a future version will let the user mute tracks outside their own speech turns).

### Updating on cut / mute (and uncut / unmute)

A cut or mute acts on a turn-relative **span** `[start, end)` (current-vec sample coordinates, resolved by the M5 caller from the word onsets). The four primitives (`subdivide_on_cut`, `subdivide_on_mute`, `merge_on_uncut`, `merge_on_unmute` in `audio/splice.rs`) are **coordinate-pure span operations**: the span may cross **any number of splice boundaries and any splice kinds** (so cutting/muting a multi-word selection, or restoring source across a previously edited region, is a single call). The splice containing `start` is trimmed to its **head**, the splice containing `end` to its (source-rebased) **tail**, everything between is dropped, and the new edit produces:

1. the surviving audio (any kind) **before** `start` — the head;
2. for a **mute**, one `SpliceKind::RoomTone` (or `Silence`, when the "mute to silence" preference is active) splice spanning `[start, end)`; for a **cut**, nothing — the span (including, when the cut swallows it, the following inter-word silence) is removed, shrinking the turn;
3. the surviving audio (any kind) from `end` onward — the (source-rebased) tail.

The `mute_to_room_tone: bool` flag and the `crossfade_samples: i64` integer (already converted from `splice_crossfade_ms` × project rate by the M5 caller) are passed as parameters — the audio engine reads no settings directly and does no ms→sample math internally (integer-samples invariant).

**The M5 caller owns the cut/mute interaction policy** (e.g. a muted word staying muted when later uncut; whether a cut subsumes adjacent edits) and is responsible for requesting correct spans; each primitive simply performs the span operation it is given and, on a merge, restores source audio for the requested span.

**Uncut / unmute is the inverse, done by *merging*.** A merge re-inserts a `Source` of the word's length reading the word's stored `source_onset_sample` (which a cut/muted word always has, since it was refined when first edited): `merge_on_uncut` inserts at the zero-width gap, re-growing the turn (splitting the containing splice when a prior coalesce merged a room tone over the gap — `start` need not be a boundary); `merge_on_unmute` replaces the muted span, preserving the turn length. Debug-asserted invariants: the restored source must not overlap the following splice's source, and `start` must not fall inside a `Source` splice (there is no edited region to restore mid-source).

**Coalescing keeps the vec canonical.** Every op coalesces adjacent splices that should be one: source-contiguous `Source`s (`a.source_start_sample + a.length_samples == b.source_start_sample`), adjacent `RoomTone`s, and adjacent `Silence`s — summing lengths, keeping the leftmost `fade_in`/`source_start_sample` and the rightmost `fade_out`, and dropping the interior seam fade. A cut breaks source-contiguity and a mute interposes a non-`Source` splice, so two adjacent `Source`s become contiguous *only* once the edit between them is undone — coalesce fires exactly then, and a within-splice edit followed by its inverse returns the vec to its **pre-edit shape**. (Adjacent room tones arise from muting adjacent words, or from cutting a source from between two muted words; they coalesce too.) This keeps the representation canonical (same edits ⇒ identical vec regardless of order) and maximises blob-store reuse. (Phase 2's manual per-splice fades on a `Source`–`Source` boundary would need revisiting; out of scope for Phase 1.)

The EDL is **maintained incrementally** — subdivided on cut/mute, merged on uncut/unmute — *not* recomputed from the word list, because per-boundary fade lengths (`fade_in_samples` / `fade_out_samples`) are independently editable splice state (phase-2 directly; even in Phase 1 by changing the global `splice_crossfade_ms` between edits) and would be lost by a recompute. Word onset/offset come from the zero-crossing search (below), which is where the word's `source_onset_sample` / `length_samples` get their precise values. Because turns are immutable and content-addressed, applying the edit yields a **new `Turn` version**; the touched turn's splice vec changes while untouched turns are shared unchanged.

### Building the playback / export EDL

For playback/export over `[start, end]`:

1. Walk the implicit timeline tree from `start` forward, collecting turns. Splices are embedded in each turn (resident in RAM — no table lookup). A splice's absolute project position is the turn's start sample (from the tree walk) plus the running sum of the `length_samples` of the preceding splices in that turn — splices carry **no** stored project-position offset. A `SpliceKind::Source` splice also carries no stored *decode* offset: the renderer seeks the FLAC cache to `source_start_sample` via `FrameReader::seek_to_frame` and discards the `required_ts − actual_ts` frames returned by the Symphonia seektable, so the seek-discard delta is recomputed at read time rather than persisted.
2. Concatenate each turn's splice vec. Inter-turn silence is already part of each turn's splice tiling (`post_turn_silence`), so the only gap synthesized while walking is the **lead-in** before a track's first content (when the track's `project_start_sample` is later than the walk start).
3. For multi-track projects, do this per track, then merge by project-timeline position, mixing samples. The merge resolves boundary alignment (each merged span carries one per-track contribution over the same sample range); the mix step sums those contributions.

Splice fade lengths (`fade_in_samples` / `fade_out_samples`) are integer samples at the project rate.

### Zero-crossing and crossfade

Per the requirements spec, when cutting or muting a word:

1. Search backwards up to `splice_search_window_ms` (default 20 ms) before the word's start for an acceptable zero crossing: a sample where the local RMS < `max(0.001, min(2 * room_tone_rms, room_tone_rms_ceiling))`. The ceiling reuses the same `room_tone_rms_ceiling` setting (default 0.0316 ≈ −30 dBFS) as room-tone detection; `0.001` is a fixed floor and `2 ×` a fixed multiplier.
2. Search forwards up to `splice_search_window_ms` after the word's end for the same condition.
3. Record a `splice_crossfade_ms` (default 2 ms) crossfade length on the new splice seam (as the splices' `fade_in_samples` / `fade_out_samples`). The crossfade is **applied at render time as a centered, equal-power overlay** straddling the chosen boundary — each side runs one equal-power ramp of its fade length centered on the boundary, reading its source *handle* across the seam (the material the trim removed, or the continued room-tone loop), summed so a symmetric seam is constant-power with the crossover landing exactly on the zero crossing. The splice tiling and absolute positions are unchanged; the crossfade adds no length. See [Building the playback / export EDL](#building-the-playback--export-edl) and the renderer (`src-tauri/core/src/audio/render.rs`). Fade-in and fade-out are stored separately per seam, so a future per-side custom length is representable; the engine reads no settings — the M5 caller resolves `splice_crossfade_ms` (and the room-tone gap-fade length) to samples and is responsible for clamping it to a valid range. When a handle has no source to draw from (a side at the cache's start or end), the renderer **degrades gracefully**: it shortens/zero-pads a partial handle, or, with no handle at all, falls back to a one-sided fade within that side's own extent — it never reads outside a valid source range.
4. If no zero crossing is found within the search window, use the boundary with the minimum local energy.

"Local RMS at a sample" is the RMS over a window of length `splice_crossfade_ms` (the crossfade and the analysis window are equal by design) centred on the candidate, clamped at slice ends. Search and refinement work in **frame** units (per-channel sample positions) and take the track's channel count, so a stereo cut lands on a frame boundary; the RMS sums squares across all channels (matching the mono down-mix room-tone detection uses). `splice_search_window_ms` and `splice_crossfade_ms` are app settings (see [data-model.md § App settings](data-model.md#app-settings)) stored in ms (rate-independent); the audio engine reads no settings directly — the M5/render caller resolves them, converting ms to integer frames once into a `ZeroCrossingParams` (the engine works in frames, per the integer-samples invariant).

**Renderer fade application.** The renderer (`src-tauri/core/src/audio/render.rs`) realises these centered overlays — both cut/mute seam crossfades and room-tone gap fades — with a single, project-wide fade accumulator sized to the **longest** fade currently in play, fed by a bounded look-ahead over the upcoming splice descriptors (the backward half of a centered fade needs material from across the seam). That look-ahead lives **upstream of the playback ring buffer** ([§ Ring buffer](#ring-buffer)): the ring carries already-faded frames, so its `RING_MS` depth never bounds the maximum fade or gap-fade length, and tuning `RING_MS` cannot clip a long room-tone gap fade.

Cutting a word includes cutting the inter-word silence that follows it (if the turn has more words after).

**Refinement is lazy and per-seam.** Zero-crossings are expensive and most words are never edited, so a word's `source_onset_sample` stays `None` (and `length_samples` approximate) until it is actually needed. Refinement happens at edit time and is keyed to the **seam**, not a single word: cutting word *i* extends removal to word *i+1*'s onset, so it refines word *i* (onset + offset) **and** word *i+1* (onset) — being explicit about the seam matters for very fast speech where the inter-word gap is zero. The **one** eager exception is the **first word of every turn**, refined at **import**: the turn origin O is its onset, and the turn boundaries (`turn_duration + post_turn_silence` = the gap between consecutive turn origins) are fixed from those onsets at import (see [data-model.md § Turn payload](data-model.md#turn-payload-the-unit-stored-in-the-blob-store)).

### Overlapping turns

When turn A from track 1 and turn B from track 2 overlap in project time, their EDLs are merged at sample granularity. During the overlap window, both tracks' samples are summed (mixed). The result is clamped to f32 range `[-1.0, 1.0]` after mixing.

**Cutting during overlaps:** A word from turn A may not be cut if any part of its time range overlaps with any word in turn B (from a different track). The Rust engine checks this at command validation time. Exception: if the selection covers *both* full overlapping turns, they may be cut together.

## Playback engine

The playback engine runs on a dedicated Rust thread using `cpal`.

### Architecture

```
EDL builder ──► f32 PCM frames ──► cpal output stream ──► system audio
                      │
                      └──► playhead position events ──► Tauri event bus ──► UI
```

### Output stream

A `cpal` output stream is opened **when a project opens** and kept alive for that project session (recreated on project switch; a second project window owns its own stream). It cannot be opened at application start because both its config and the ring-buffer capacity depend on the **negotiated device rate** (below), which is unknown until a project is open. It is configured for 2-channel (stereo) output. Mono tracks are up-mixed to stereo with equal gain on both channels. The `PlaybackEngine` owns the stream + ring and reuses them across all play/stop cycles within the project.

**Device-rate negotiation (two clocks).** The default output device may not open at the project's locked sample rate (e.g. a 44.1 kHz project on a 48 kHz-locked device, or Bluetooth at 16 kHz). On stream open the backend **negotiates** the device rate — it prefers a device config at the project rate and otherwise falls back to the device's default rate — and reports it back. This splits the engine into two sample-rate clocks: the **project clock** (the renderer, the EDL, `start`/`end`, and playhead *semantics*) and the **device clock** (the ring, the callback, and the `frames_played` counter). A [`StreamingResampler`](audio-pipeline.md#resampling) on the **pre-roll thread** bridges them — an identity passthrough when the rates match (the common case), so it costs nothing then; the callback never resamples (no DSP / no allocation on the real-time path). Note this adapts only the *rate* of the **default** device; device *selection* and hot-swap are post-Phase 1.

### Ring buffer

Between the EDL reader and the cpal callback, a lock-free SPSC ring buffer (`rtrb`) holds pre-rendered frames; it is split into a producer (the pre-roll thread, the only writer) and a consumer (captured by the cpal callback, the only reader), allocated together at stream open. A "pre-roll" thread renders project-rate frames from the EDL, resamples them to the device rate (passthrough when equal), and fills the buffer; the cpal callback drains it (writing silence on underrun, never blocking/allocating/locking). Buffer size: a fixed `RING_MS` (~200 ms) of audio **at the device rate** (the rate the callback drains at) — a named constant, not a user setting (an internal latency-vs-underrun tradeoff; exposing it would require a `settings.json` migration for negligible benefit).

### Playhead events

Every ~50ms (configurable), the pre-roll thread emits a Tauri event `playhead_update { position_samples }` to the UI. The UI uses this to highlight the current word and scroll the viewport. `position_samples` is the *played* (not rendered-ahead) position in **project** samples: it is derived from the device-clock `frames_played` counter (incremented by the callback only for real frames delivered, not silence padding) and converted to the project clock as `start + round(frames_played · project_rate / device_rate)` — an exact identity when no resampling is in play. The conversion happens on the pre-roll thread, never the callback.

### Room tone substitution

The head/tail loop crossfade is **pre-applied when the room tone is extracted** (see [§ Room tone detection](#room-tone-detection)), so the stored segment loops seamlessly with no per-playback fade. When a room-tone splice is encountered:
1. Read the track's room tone PCM (resampled f32 at project rate) — the `store` blob referenced by the track metadata's `room_tone_hash`.
2. Loop the stored segment directly (it is already crossfaded for looping).
3. Crossfade at the **gap boundary**, where the looped tone meets the surrounding audio — using the RoomTone splice's own stamped `fade_in` / `fade_out`, applied by the **same centered equal-power seam machinery** as any splice (see [§ Zero-crossing and crossfade](#zero-crossing-and-crossfade)). There is **no separate gap-fade mechanism and no fixed gap-fade length** in the renderer: the gap-fade length is recorded on the splice by the M5 mute command (a room-tone-gap setting, typically longer than the cut crossfade). At a speech→room-tone seam the source side supplies its forward handle and the room-tone side's backward handle is the continued loop; vice-versa at the room-tone→speech seam.

(The loop crossfade — baked into the stored blob at extraction — is a separate concern from this gap crossfade; the renderer's only room-tone-specific behaviour at playback is the looping itself.)

### Playback stops

Playback stops when:
- `end_sample` is reached (the frontend set it to the cursor/turn/selection end).
- The end of the EDL (end of timeline) is reached — the case when `end_sample` is null.
- The user presses Space or triggers Stop.

The backend does not interpret "selection" or "turn" scopes; the frontend resolves those into the `[start_sample, end_sample)` range it passes to `play_from`. When playback stops, the playhead position is reported to Rust, which maps it to the nearest word and updates the cursor.

## Room tone detection

Room tone detection runs in **Rust** at track import time — it is signal processing, not ML, so it is not part of the Python sidecar's work. RMS/stability analysis is performed on a **mono down-mix** (channel mean) of the resampled-cache PCM, read via the `FrameReader` streaming interface (no full-file decode into RAM). The extracted segment is **stored in the source channel count** (a mono recording yields mono room tone; a stereo recording yields a stereo sample, the two channels collapsed only for the RMS analysis). The algorithm:

0. **Skip entirely if the recording is shorter than 10 s** — too little material to extract a useful loop; return no room tone (the renderer falls back to digital silence, as for any track without a `room_tone_hash`).
1. Compute RMS of non-overlapping 100ms blocks across the full track (on the mono down-mix).
2. Define the **global quiet threshold** `Q = min(ceiling, Pq)`, where `ceiling` is the `room_tone_rms_ceiling` setting (default `0.0316` ≈ −30 dBFS) — the absolute "background, not signal" level (the same default the [zero-crossing search](#zero-crossing-and-crossfade) clamps to) — and `Pq` is the `room_tone_quiet_percentile`-th percentile of the 100ms-block RMS values (default 5th percentile; adapts the threshold downward on genuinely-quiet tracks). Both are [app settings](data-model.md#app-settings), resolved by the caller and passed into detection (the audio engine reads no settings directly).
3. Search for the longest contiguous window (target 5–10 seconds, minimum 2 seconds) with the lowest cumulative RMS, accepting a window only if: (a) its RMS ≤ the absolute ceiling `room_tone_rms_ceiling` (it is background, not signal); (b) peak energy ≤ 5 × RMS of the window (no transient); and (c) SD of 100ms-block RMS ≤ 15% of mean RMS (stable level). Among accepted windows prefer the **longest**, breaking ties by **lowest window RMS**, then **earliest start**.
4. If no ≥ 2 s window qualifies, search for 100–300ms quiet segments — pieces whose block RMS ≤ `Q` and whose peak ≤ 5 × the piece RMS — and stitch them with 50ms crossfades up to the 2 s minimum.
5. Apply the **loop crossfade** to the segment's head and tail so it can be looped directly at playback (no fade needed at mix time). The crossfade length scales with the segment length: **< 500ms → 50ms; 500ms–2s → 100ms; > 2s → 500ms**.
6. The resulting PCM segment (f32, project sample rate, source channel count) is stored as a content-addressed blob in `store` and referenced by hash (`room_tone_hash`) from the track's metadata. The per-channel frame count is **derived** at load time by decoding the blob — it is not stored separately in `TrackMeta`. If no window and no stitched segment qualifies (e.g. continuous loud content with no block below the ceiling), **no room tone is recorded** (`room_tone_hash` stays null).

## Non-speech sound detection

After transcription, non-speech segments are swept by Rust:

1. Iterate the gaps between recognized speech turns.
2. Apply a 20ms sliding window (10ms hop) over each gap.
3. If a window's RMS > 4 × room tone RMS: mark as sound event start; extend forward up to 150ms hold time (continue as long as sound reoccurs within 150ms).
4. Each detected sound event becomes its **own turn** (a bubble) on the speech track, with `speaker_id = None` (rendered "[None]"), a `Sound`-typed word (default text `"[Sound]"`), and a `turn_duration` matching the event's span. Sound events are **not** labels and do not go on track 0.

Detection (the sweep above) is signal processing and runs entirely in Rust. *Labeling* the events is the only ML part and is optional: if a `sound_classification` (YAMnet) model is selected, Rust dispatches a `classify_sounds` task with the per-event audio and replaces each Sound word's text with the returned label (e.g. `"[Laughter]"`); otherwise the events keep `"[Sound]"`. See [ml-pipeline.md § YAMnet sound classification](ml-pipeline.md#yamnet-sound-classification).

## Track alignment

Track alignment runs **entirely in Rust** as a background task (FFT cross-correlation over speaking turns). It is *not* an ML operation, so it is never dispatched to the Python sidecar — Python is reserved for model inference. It is its own `align_tracks` command (see [command-surface.md](command-surface.md#align_tracks-v1)): the UI queues it after the relevant `import_speech_track` commands finish, or the user triggers it manually (e.g. to align separately-imported tracks).

**Precondition:** every track in the align set must have `cut_length_samples == original_length_samples` (no cuts applied); otherwise the command fails with `track_has_cuts_cannot_align`.

### Algorithm

1. Identify speaking turns in each track (from the implicit timeline tree).
2. Compute cross-correlation (via FFT) between at least three speaker turns per track pair, spread across the duration of the recording.
3. Find the offset that maximizes cross-correlation at the head and tail independently.
4. Compute a linear clock-drift correction: the head offset sets `project_start_sample`; the drift rate spreads the head↔tail difference linearly across the track duration.
5. When aligning > 2 tracks: order tracks by descending speech time; align each successive track against the union of already-aligned tracks.

### Result application

Rust applies, per track, the computed `project_start_sample` and records `drift_ppm` (for future use), and records the aligned set in `ProjectMeta.aligned_groups`.

## Export pipeline

Audio export reuses the playback EDL engine offline, **pull-based**: a caller-built `EdlCursor` (wrapped in a `Renderer`, which is a `PcmSource`) is *pulled* by a streaming encoder. There is no separate export sink trait and no whole-buffer staging — encoding peaks at one chunk of memory, the same streaming property the import transcode has, so a feature-length export never goes resident.

### Audio export

1. **Build the cursor.** `EdlCursor::build(tracks, start, end)` assembles a merged cursor over the requested track(s) for `[start, end)`. **The cursor carries the length bound:** when `end = Some(project_end)` and the tracks exhaust early, it emits one trailing-silence slice to `project_end` then stops; `end = None` walks to the last track's content end (= project end, since no content exists past the longest track). This unifies single-track silence padding, mixed export, and bounded/partial export under one contract — there is no separate pad step and no `project_length` threaded through the export functions (`project_end = max(project_start_sample + tree_length)` over the project's tracks, data the handler already holds).
2. **Wrap in a `Renderer`** — the offline `PcmSource`. Source audio comes from each track's resampled cache (`resampled/<track_id>.flac`, regenerated from the source if missing) — no on-the-fly resampling. A multi-track cursor merges and mixes per the playback EDL rules above; single vs. mixed export differs only in how many tracks the cursor was built over (one `export_audio` entry point covers both). If mono collapse is requested, the renderer is wrapped in a `MonoSource` that reports 1 channel and averages `(L + R) / 2` per frame.
3. **Pull-encode to the output path.** `export_audio(renderer, format, mono, out)` dispatches on `format` (resolved from the extension by the caller): FLAC → `encode_flac_streaming` (the **same** 24-bit streaming encoder the import transcode uses), WAV → `encode_wav_streaming` (f32le, RIFF/data sizes back-patched at finalize), MP3 / OGG / AAC → `encode_via_ffmpeg` (pulls frames → ffmpeg stdin) when a system `ffmpeg` is available, else `export_unsupported_format`. Each encoder owns its output file and **removes a partial `out` on any failure**. **Default format: FLAC (24-bit integer, project sample rate).** WAV is **f32le** (32-bit float, IEEE 754) for a bit-exact round-trip; its header carries format code `3` (IEEE_FLOAT). Exports are **not** cached in `.vbdata/`.

Because export and playback assemble the *same* cursor and render through the *same* `Renderer`, an export over `[start, end)` is sample-for-sample identical to playback over that range.

### Transcript export

Transcript export is not an audio operation; it lives in `project/transcript.rs` (reads the implicit timeline tree, touches no audio code). Turns from all tracks stream through `MergedTurns` — a lazy k-way merge over the per-tree turn iterators emitting `(start, end, &Turn)` in global timeline order (the turn-level analog of `EdlCursor`) — and each formatter builds its output in a single pass, with no `Vec`-materialize-and-sort. Supported formats: VTT, Markdown, Word/RTF (Phase 1: VTT and Markdown; Word/RTF deferred). Both Phase-1 formats are **by-turn**: one cue (VTT) / paragraph (Markdown) per turn, speaker-labelled (VTT carries the speaker as a `<v Speaker Name>` voice tag). VTT cue extents come from the turn's project position and `turn_duration`; per-word (karaoke) timing is deferred.

### Format selection

File type is determined by the **output-file extension** — the extension always wins over any caller-supplied format parameter. `audio_format_for(path)` maps the extension to an `AudioFormat` (unrecognised → `export_unsupported_format`); `transcript_format_for(path)` maps it to an `Option<TranscriptFormat>` (the handler maps `None → export_unsupported_format`, keeping the transcript module free of `AudioError`). On an unsupported format the save dialog re-opens with the previously submitted name. Callers derive the format from the path and pass it in (the `format` wire field stays advisory); the audio encoder honors the passed `AudioFormat` rather than re-deriving it.
