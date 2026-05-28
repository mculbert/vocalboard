# Command Surface

Every user-visible action in Vocalboard is a **named, versioned command**. Commands are the single unit of:
- Application of state mutations (Rust applies them to the implicit timeline tree)
- Persistence (a command produces a batch of tree **deltas**, recorded in the `journal` as one `type = 0` row whose `command_id` is the enum code for the command type — see [data-model.md § Blob-and-tree persistence](data-model.md#blob-and-tree-persistence))
- Undo/redo (the delta batch and its inverse are pushed on the undo stack — see [data-model.md § Undo / redo](data-model.md#undo--redo))
- Plugin/scripting access (Phase 6: plugins call the same surface)

The command surface is the **only** way the frontend (or a future plugin) mutates project state; it is the security boundary described in [architecture.md § Security boundaries](architecture.md#security-boundaries). Commands no longer carry a stored logical inverse: undo is delta-based, so what each command *records* is the concrete tree edit it produced, not a reverse command.

## Conventions

- `command_name`: `snake_case`
- `command_version`: integer, incremented on breaking param changes
- `params_json`: JSON object; schema in Draft-07 format. Rust validates params against the schema before applying; unknown extra fields are rejected.
- `source`: who may invoke this command — `frontend` (via Tauri command), `task_result` (Rust applies after a Python task completes), `both`

Applying a command produces zero or more timeline deltas (`InsertAfter` / `UpdateAfter` / `DeleteAfter`) and/or a metadata change. These are written as one `type = 0` row (plus, if metadata changed, one `type = -1` row), each tagged with the command-type `command_id` code; the undoable unit is the in-memory undo-stack entry that bundles them, not a journal grouping key.

Progress event shape (for ML commands dispatched to Python):
```json
{ "request_id": "...", "type": "progress", "step": "<step_name>", "step_index": 1, "step_count": 4, "pct": 42, "label": "Transcribing…" }
```

---

## Project commands

### `new_project` v1

Creates a new empty project sqlite file.

**Params:**
```json
{
  "type": "object",
  "required": ["path", "sample_rate"],
  "properties": {
    "path":        { "type": "string", "description": "Absolute path for the .vocalboard file" },
    "sample_rate": { "type": "integer", "minimum": 8000, "default": 48000, "description": "Any integer rate (e.g. 16000, 44100, 48000); locked at creation" }
  }
}
```
**Source:** `frontend`

---

### `open_project` v1

Opens an existing project sqlite file, loads the latest snapshot, and applies the remaining journal deltas.

**Params:**
```json
{ "type": "object", "required": ["path"], "properties": { "path": { "type": "string" } } }
```
**Source:** `frontend`

---

### `save_snapshot_now` v1

Triggers an immediate snapshot serialization on the background thread.

**Params:** `{}`
**Source:** `frontend`

---

## Track commands

### `import_speech_track` v1

Registers a new speech track (one source file) and kicks off the import pipeline: transcribe → diarize (Python ML) → room-tone (Rust) → non-speech detection (Rust). For multi-file imports, multiple `import_speech_track` commands are issued; **alignment is not a step here** — it is a separate `align_tracks` command that the UI queues after the imports complete. Named `import_speech_track` (not `import_track`) because later phases add non-speech track types (background music, sound effects), and a track is appended to the project timeline, not to another track.

**Params:**
```json
{
  "type": "object",
  "required": ["source_path"],
  "properties": {
    "source_path": { "type": "string" },
    "placement":   { "type": "string", "enum": ["append","prepend"], "default": "append" }
  }
}
```
**Source:** `frontend`

Progress steps: `decode_probe` → `transcribe` → `diarize` → `room_tone` → `non_speech_detect`. (Resampling the source to the project-rate cache runs as a separate background task.)

---

### `remove_track` v1

Removes a track and all its turns/words from the project.

**Params:**
```json
{ "type": "object", "required": ["track_id"], "properties": { "track_id": { "type": "integer" } } }
```
**Source:** `frontend`

---

### `rename_track` v1

Renames a track.

**Params:**
```json
{
  "type": "object",
  "required": ["track_id", "name"],
  "properties": {
    "track_id": { "type": "integer" },
    "name":     { "type": "string", "minLength": 1 }
  }
}
```
**Source:** `frontend`

---

### `align_tracks` v1

Aligns two or more tracks to each other. Runs **entirely in Rust** (FFT cross-correlation over speaking turns — alignment is not an ML operation, so there is no Python dispatch) as a background task with progress. The UI queues this command after the relevant `import_speech_track` commands complete, and the user may also trigger it manually (e.g. to align tracks that were imported separately).

**Precondition:** every listed track must have `cut_length_samples == original_length_samples` (no cuts applied); otherwise the command fails with `track_has_cuts_cannot_align`. On success the aligned set is recorded in `ProjectMeta.aligned_groups`.

**Params:**
```json
{
  "type": "object",
  "required": ["track_ids"],
  "properties": {
    "track_ids": { "type": "array", "items": { "type": "integer" }, "minItems": 2 }
  }
}
```
**Source:** `frontend`

Progress steps: `extract_turns` → `cross_correlate` → `compute_drift` (all in Rust)

---

## Speaker commands

### `rename_speaker` v1

**Params:**
```json
{
  "type": "object",
  "required": ["speaker_id", "name"],
  "properties": {
    "speaker_id": { "type": "integer" },
    "name":       { "type": "string", "minLength": 1 }
  }
}
```
**Source:** `frontend`

---

### `merge_speakers` v1 *(Phase 1 stub)*

Merges all turns from `source_speaker_id` into `target_speaker_id` and deletes the source speaker.

**Params:**
```json
{
  "type": "object",
  "required": ["source_speaker_id", "target_speaker_id"],
  "properties": {
    "source_speaker_id": { "type": "integer" },
    "target_speaker_id": { "type": "integer" }
  }
}
```
**Source:** `frontend`

---

## Editing commands

### `cut_words` v1

Marks as cut every word whose span falls within a transcript time range. `start_sample == end_sample` cuts the single word at that position; a range cuts all words in `[start_sample, end_sample]`, across tracks. The command resolves which words fall in the range.

**Params:**
```json
{
  "type": "object",
  "required": ["start_sample", "end_sample"],
  "properties": {
    "start_sample": { "type": "integer", "description": "transcript position in project samples" },
    "end_sample":   { "type": "integer", "description": "transcript position in project samples (>= start_sample)" }
  }
}
```
**Source:** `frontend`

> Validation: rejects if any word in the range overlaps a turn from another track (unless the range covers the full overlapping turns). Returns error code `overlapping_word_cannot_cut`.

> **Selection-to-range note:** the frontend resolves a user selection into `[start_sample, end_sample]`. Selection follows navigation order (overlapping turns are sequentialized by start time — see [frontend.md § Selection](frontend.md#selection)), so for a selection that spans overlapping turns the endpoints are *not* a simple timeline min/max. Reconciling that mapping (the later turn's selected end can precede the earlier turn's end in project time) is an implementation consideration flagged in frontend.md.

---

### `uncut_words` v1

Marks one or more words as not cut.

**Params:** Same schema as `cut_words`.
**Source:** `frontend`

---

### `mute_words` v1

Marks one or more words as muted.

**Params:** Same schema as `cut_words`.
**Source:** `frontend`

---

### `unmute_words` v1

Marks one or more words as not muted.

**Params:** Same schema as `cut_words`.
**Source:** `frontend`

---

## ML task commands

These commands are dispatched to the Python sidecar and applied when Python returns a result.

### `transcribe_track` v1

Runs the full WhisperX pipeline on a track. Applied as a task result; the resulting turns/words populate the track's timeline tree.

**Params:**
```json
{
  "type": "object",
  "required": ["track_id"],
  "properties": {
    "track_id":     { "type": "integer" },
    "language":     { "type": ["string","null"], "default": null }
  }
}
```
**Source:** `task_result`

> Model selection is **not** a param: Rust resolves the selected model from `model_paths.transcription` (app settings) and injects its path into the Python request payload. The same rule applies to every ML task command below (`enhance_track` → `model_paths.enhancement`, `identify_disfluencies` → `model_paths.llm`, `classify_sounds` → `model_paths.sound_classification`) — the frontend never names a model.

---

### `enhance_track` v1

Runs MP-SENet on the track audio. Records the enhanced file path in the track metadata.

**Params:**
```json
{
  "type": "object",
  "required": ["track_id", "output_path"],
  "properties": {
    "track_id":    { "type": "integer" },
    "output_path": { "type": "string" }
  }
}
```
**Source:** `task_result`

> The wet/dry ratio is **not** a parameter here: it is a playback/export mixing setting (persisted as `TrackMeta.wet_dry_ratio`), applied at audio splice-read time, not at enhancement time.

---

### `identify_disfluencies` v1

Runs Gemma on the **entire** transcript of a track and tags word types. Identification is always whole-track (no selection scope).

**Params:**
```json
{
  "type": "object",
  "required": ["track_id"],
  "properties": {
    "track_id": { "type": "integer" }
  }
}
```
**Source:** `task_result`

---

### `classify_sounds` v1

Labels Rust-detected non-speech sound events using YAMnet. Detection itself is a Rust sweep at import (not this command); this command supplies the per-event audio to Python and applies the returned top-1 label to each event's Sound word. Dispatched automatically during `import_speech_track` **only if** a `sound_classification` model is selected; if none is, events keep their default `"[Sound]"` text and this command does not run.

**Params:**
```json
{
  "type": "object",
  "required": ["track_id"],
  "properties": {
    "track_id": { "type": "integer" }
  }
}
```
**Source:** `task_result`

---

### `remove_disfluencies` v1

**Cuts** all words tagged as disfluencies in scope — either the whole track (`track_id`) or a transcript range (`start_sample`/`end_sample`) — falling back to **mute** for any disfluent word that overlaps a turn from another track (the general `overlapping_word_cannot_cut` rule). A single undo-stack entry.

**Params:**
```json
{
  "type": "object",
  "oneOf": [
    { "required": ["track_id"],
      "properties": { "track_id": { "type": "integer" } } },
    { "required": ["start_sample", "end_sample"],
      "properties": {
        "start_sample": { "type": "integer" },
        "end_sample":   { "type": "integer" }
      } }
  ]
}
```
**Source:** `frontend`

---

### `remove_sounds` v1

Applies cut to all non-speech sound events in scope — either the whole track (`track_id`) or a transcript range (`start_sample`/`end_sample`). A single undo-stack entry.

**Params:**
```json
{
  "type": "object",
  "oneOf": [
    { "required": ["track_id"],
      "properties": { "track_id": { "type": "integer" } } },
    { "required": ["start_sample", "end_sample"],
      "properties": {
        "start_sample": { "type": "integer" },
        "end_sample":   { "type": "integer" }
      } }
  ]
}
```
**Source:** `frontend`

---

## Playback commands

### `play_from` v1

Starts playback over an explicit project-timeline range. The frontend resolves whatever the user meant (cursor→end, current turn, current selection) into `start_sample`/`end_sample`; the backend simply plays `[start_sample, end_sample)`. Not journaled (not a project mutation).

**Params:**
```json
{
  "type": "object",
  "required": ["start_sample"],
  "properties": {
    "start_sample": { "type": "integer" },
    "end_sample":   { "type": ["integer","null"], "default": null, "description": "null = play to end of timeline" }
  }
}
```
**Source:** `frontend`

---

### `pause` v1 / `stop` v1

Pause (retains position) or stop (moves cursor to last position) playback. No params. Not journaled.
**Source:** `frontend`

---

## Export commands

### `export_track` v1

Renders and exports a single track.

**Params:**
```json
{
  "type": "object",
  "required": ["track_id", "output_path"],
  "properties": {
    "track_id":    { "type": "integer" },
    "output_path": { "type": "string" },
    "format":      { "type": "string", "enum": ["flac","wav","mp3","ogg","aac"], "default": "flac" },
    "mono":        { "type": "boolean", "default": false }
  }
}
```
**Source:** `frontend`

---

### `export_mixed` v1

Renders the mixed output of all non-muted tracks.

**Params:** Same as `export_track` minus `track_id`.
**Source:** `frontend`

---

### `export_transcript` v1

**Params:**
```json
{
  "type": "object",
  "required": ["output_path"],
  "properties": {
    "output_path":       { "type": "string" },
    "format":            { "type": "string", "enum": ["vtt","markdown"], "default": "vtt" },
    "include_cut_words": { "type": "boolean", "default": false }
  }
}
```
**Source:** `frontend`

---

## Task management commands

### `cancel_task` v1

Sends a cancel control message to the Python sidecar for the given request. The task queue is in-memory only (Phase 1 does not persist it); cancelling drops the in-flight task.

**Params:**
```json
{ "type": "object", "required": ["task_id"], "properties": { "task_id": { "type": "string" } } }
```
**Source:** `frontend`

---

### `list_tasks` v1

Returns the current (in-memory) task queue. Read-only; not journaled.
**Source:** `frontend`

---

## Error codes

| Code | Meaning |
|---|---|
| `overlapping_word_cannot_cut` | Word overlaps another track's turn; cannot be cut individually |
| `track_has_cuts_cannot_align` | A track in the align set has cuts applied (`cut_length_samples != original_length_samples`) |
| `last_track_cannot_remove` | Attempt to remove the only remaining track |
| `track_name_empty` | Track name may not be empty |
| `track_name_duplicate` | Track with that name already exists |
| `speaker_name_empty` | Speaker name may not be empty |
| `speaker_name_duplicate` | Speaker with that name already exists |
| `model_not_available` | Required ML model not downloaded |
| `low_confidence_transcript` | Transcription rejected; avg_logprob below threshold |
| `file_not_found` | Source audio file could not be located |
| `export_unsupported_format` | File extension not recognized |
| `cancelled` | Task was cancelled by user |
| `sidecar_not_ready` | Python sidecar did not start within timeout |
| `unknown_command` | Command name not recognized by the sidecar / unsupported message type |
| `internal_error` | Unhandled error inside a sidecar handler |
