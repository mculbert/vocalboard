# Audio Pipeline

## Decoding strategy

All audio decoding in Rust produces **f32 PCM at the project sample rate**. Two decoders are used:

### Symphonia (primary)

[`symphonia`](https://github.com/pdeljanov/Symphonia) handles the common audio formats natively in pure Rust:

- WAV, AIFF, FLAC, ALAC (in MPEG-4), native AIFF
- MP3 (via the `symphonia-codec-mp3` feature)
- OGG Vorbis
- FLAC (native)

Symphonia is the **only decoder used for audio-only files** in these formats. It is linked statically; no system libraries needed.

### ffmpeg (fallback)

A bundled `ffmpeg` subprocess (or dynamically-loaded `libavcodec`) is invoked for:

- Video containers: MP4, MOV, MKV, WebM — to demux the audio stream
- AAC / M4A (MPEG-4 AAC)
- Any format Symphonia cannot handle

The ffmpeg build is **LGPL** (no GPL codecs). It is called via a subprocess pipe rather than Rust bindings to avoid linking complexity; audio is extracted as raw PCM and piped to the Rust decoder.

> **Forward-looking (Phase 1):** Move some video/exotic formats to ffmpeg-only if they require codec infrastructure not worth shipping in Phase 1.

### Resampling

At **import**, the source is decoded and resampled to the project sample rate with `rubato` sinc interpolation (quality preset from `resampling_quality` in settings), then written to `<project>.vbdata/resampled/<track_name>.flac` (24-bit integer FLAC) by a **background task**. `TrackMeta.resampled_path` is `null` until that task completes. Resampling on the fly during playback/export was considered and rejected — it isn't reliably fast enough for low-latency preview — so this resampled cache becomes the audio source the EDL engine reads from (decoding 24-bit FLAC back to f32 is cheap). The cache trades a small f32→int24 quantization (≈ −144 dB floor, far below any speech room tone) for roughly half the disk of raw f32.

The cache is **derived**: if `resampled/<track>.flac` is missing on open (deleted, or the import was interrupted before the background task finished), it is regenerated from the source — the same posture as a missing enhanced track. The original source file must therefore remain accessible (for re-resampling, re-import, and enhancement).

## Edit Decision List

The Edit Decision List (EDL) is **maintained incrementally**, not rebuilt by a transcript pass at play time. Each turn's `splices` vec (see [data-model.md § Turn payload](data-model.md#turn-payload-the-unit-stored-in-the-blob-store)) is the persisted EDL fragment for that turn; the playback/export EDL is the concatenation of those fragments along the timeline, merged across tracks.

### Initial EDL (at import)

Each turn starts with exactly **one** `SpliceKind::Source` splice covering the turn's speech time **plus its `post_turn_silence`**. Source audio is preserved for the time between turns; Phase 1 mixes all non-cut/non-muted audio from every track (a future version will let the user mute tracks outside their own speech turns).

### Updating on cut / mute

A cut or mute edits a word, and the **splice containing that word is subdivided** into two or three splices:

1. the surviving `Source` audio **before** the word's onset;
2. for a **mute**, a `SpliceKind::RoomTone` (or `Silence`, per the "mute to silence" setting) splice spanning the word; for a **cut**, nothing — the word's span (and, if later words follow in the turn, the following inter-word silence) is removed, shrinking the turn;
3. the surviving `Source` audio from the **next word's onset** onward.

Word onset/offset come from the zero-crossing search (below), which is where the word's `turn_offset_sample` / `length_samples` also get their precise values. Because turns are immutable and content-addressed, applying the edit yields a **new `Turn` version** whose `splices` vec is the recomputed result — a deterministic function of the words and their cut/mute flags plus the zero-crossing offsets. "The EDL is updated as edits are made" means exactly this: the touched turn's splice vec changes while untouched turns are shared unchanged.

### Building the playback / export EDL

For playback/export over `[start, end]`:

1. Walk the implicit timeline tree from `start` forward, collecting turns. Splices are embedded in each turn (resident in RAM — no table lookup). A splice's absolute project position is the turn's start sample (from the tree walk) plus the running sum of the `length_samples` of the preceding splices in that turn — splices carry **no** stored offset.
2. Concatenate each turn's splice vec; emit silence or room tone for inter-turn gaps as needed.
3. For multi-track projects, do this per track, then merge by project-timeline position, mixing samples.

Splice fade lengths (`fade_in_samples` / `fade_out_samples`) are integer samples at the project rate.

### Zero-crossing and crossfade

Per the requirements spec, when cutting or muting a word:

1. Search backwards up to 20ms before the word's start for an acceptable zero crossing: a sample where the local RMS < `max(0.001, min(2 * room_tone_rms, 0.0316))`.
2. Search forwards up to 20ms after the word's end for the same condition.
3. Apply a 2ms linear crossfade at the cut boundary.
4. If no zero crossing is found within the search window, use the boundary with the minimum local energy.

Cutting a word includes cutting the inter-word silence that follows it (if the turn has more words after).

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

A `cpal` output stream is opened once at application start and kept alive. It is configured for the project sample rate and 2-channel (stereo) output. Mono tracks are up-mixed to stereo with equal gain on both channels.

### Ring buffer

Between the EDL reader and the cpal callback, a lock-free ring buffer (e.g., `ringbuf` crate) holds pre-rendered frames. A "pre-roll" thread fills the buffer from the EDL; the cpal callback drains it. Buffer size: ~200ms of audio at project sample rate.

### Playhead events

Every ~50ms (configurable), the pre-roll thread emits a Tauri event `playhead_update { position_samples }` to the UI. The UI uses this to highlight the current word and scroll the viewport.

### Room tone substitution

The head/tail loop crossfade is **pre-applied when the room tone is extracted** (see [§ Room tone detection](#room-tone-detection)), so the stored segment loops seamlessly with no per-playback fade. When a room-tone splice is encountered:
1. Read the track's room tone PCM (resampled f32 at project rate) — the `store` blob referenced by the track metadata's `room_tone_hash`.
2. Loop the stored segment directly (it is already crossfaded for looping).
3. Apply a 50ms crossfade only at the **gap boundary**, where the looped tone meets the surrounding audio.

### Playback stops

Playback stops when:
- `end_sample` is reached (the frontend set it to the cursor/turn/selection end).
- The end of the EDL (end of timeline) is reached — the case when `end_sample` is null.
- The user presses Space or triggers Stop.

The backend does not interpret "selection" or "turn" scopes; the frontend resolves those into the `[start_sample, end_sample)` range it passes to `play_from`. When playback stops, the playhead position is reported to Rust, which maps it to the nearest word and updates the cursor.

## Room tone detection

Room tone detection runs in **Rust** at track import time — it is signal processing, not ML, so it is not part of the Python sidecar's work. The algorithm:

1. Compute RMS of non-overlapping 100ms blocks across the full track.
2. Search for the longest contiguous window (target 5–10 seconds, minimum 2 seconds) with the lowest cumulative RMS.
3. Accept only if: peak energy ≤ 5 × RMS of the window, and SD of 100ms-block RMS ≤ 15% of mean RMS.
4. If no 2s window qualifies, search for 100–300ms quiet segments and stitch them with 50ms crossfades.
5. Apply the **loop crossfade** to the segment's head and tail so it can be looped directly at playback (no fade needed at mix time). The crossfade length scales with the segment length: **< 500ms → 50ms; 500ms–2s → 100ms; > 2s → 500ms**.
6. The resulting PCM segment (f32, project sample rate) is stored as a content-addressed blob in `store` and referenced by hash (`room_tone_hash`) from the track's metadata.

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

The export pipeline uses the same EDL engine as playback.

### Track export

1. Build the EDL for the requested track from `project_start_sample` to the track end.
2. Pad with silence to the project total length.
3. Read source audio from the track's resampled cache (`resampled/<track>.flac`, regenerated from the source if missing) — no on-the-fly resampling.
4. Write f32 PCM directly to the user-chosen output path via an encoder sink (default: FLAC at project sample rate). Exports are **not** cached in `.vbdata/`.
5. If mono collapse is requested (setting), sum channels and divide by 2.

### Mixed export

Same as track export, but merge EDLs from all non-muted tracks at the mix step.

### Transcript export

Transcript export is not an audio operation; it is handled by the Rust engine reading the implicit timeline tree directly and formatting output. Supported formats: VTT, Markdown, Word/RTF (Phase 1: VTT and Markdown; Word/RTF deferred).

### Format selection

File type is determined by the extension the user provides in the save dialog. Unsupported extensions return an error; the save dialog re-opens with the previously submitted name.
