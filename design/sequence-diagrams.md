# Sequence Diagrams

## 1. Import speech track

A user selects one or more audio files to import. For each file, an `import_speech_track` flow runs. Alignment is **not** part of import — for a multi-file import the UI queues a separate `align_tracks` command (Rust-side) after the per-file imports complete (and the user can trigger alignment manually later).

The `model_path` in the `transcribe_track` request is **not** supplied by the frontend: the command carries no model name. Rust resolves the selected model from `model_paths.transcription` (app settings) and injects its path into the Python request.

```
User          Svelte UI          Rust (Tauri)            Python sidecar
  │                │                    │                       │
  ├─ clicks        │                    │                       │
  │  "Add Track"   │                    │                       │
  │─────────────►  │                    │                       │
  │                │  open_file_dialog  │                       │
  │                ├───────────────────►│                       │
  │                │                    │  dialog result        │
  │  selects       │◄───────────────────┤                       │
  │  file(s)       │                    │                       │
  │                │  invoke            │                       │
  │                │  import_speech_    │                       │
  │                │  track(path)       │                       │
  │                ├───────────────────►│                       │
  │                │                    │ enqueue task (in mem) │
  │                │                    │ probe audio metadata  │
  │                │                    │                       │
  │                │                    │──────── request ─────►│
  │                │                    │  transcribe_track     │
  │                │                    │  { track_id, path,    │
  │                │                    │    model_path }       │
  │                │                    │                       │
  │                │◄──task_progress────│◄── progress ──────────│
  │  progress bar  │  step="transcribe" │    step=transcribe    │
  │  updates       │  pct=30            │    pct=30             │
  │                │                    │                       │
  │                │                    │◄── progress ──────────│
  │                │                    │    step=diarize pct=60│
  │                │◄──task_progress────│                       │
  │                │                    │                       │
  │                │                    │◄── result ────────────│
  │                │                    │   { turns, speakers } │
  │                │                    │                       │
  │                │                    │ Rust applies          │
  │                │                    │ import_speech_track:  │
  │                │                    │  - write track meta   │
  │                │                    │  - build timeline     │
  │                │                    │    tree from turns    │
  │                │                    │  - detect+store room  │
  │                │                    │    tone (Rust)        │
  │                │                    │  - detect non-speech  │
  │                │                    │    events (Rust) →    │
  │                │                    │    Sound turns        │
  │                │                    │  - resample to cache  │
  │                │                    │    (bg task)          │
  │                │                    │  - merge speakers     │
  │                │                    │  - journal delta(s)   │
  │                │                    │  - trigger snapshot   │
  │                │                    │                       │
  │                │◄──project_state────│                       │
  │  timeline      │   _updated         │                       │
  │  renders       │                    │  if YAMnet selected:  │
  │  bubbles       │                    │  dispatch classify_   │
  │                │                    │  sounds task → relabel│
```

---

## 2. Playback start from cursor

User presses Space to begin playback from the current cursor word.

```
User          Svelte UI          Rust (audio thread)      cpal
  │                │                    │                    │
  ├─ Space         │                    │                    │
  │─────────────►  │                    │                    │
  │                │  invoke            │                    │
  │                │  play_from(start_  │                    │
  │                │  sample,end_       │                    │
  │                │  sample?)          │                    │
  │                ├───────────────────►│                    │
  │                │                    │                    │
  │                │                    │  build EDL: walk   │
  │                │                    │  tree over [start, │
  │                │                    │  end]; concat each │
  │                │                    │  turn's splice vec │
  │                │                    │  (cut/mute already │
  │                │                    │  baked in), merge  │
  │                │                    │  tracks            │
  │                │                    │                    │
  │                │                    │  open cpal stream  │
  │                │                    ├───────────────────►│
  │                │                    │  start pre-roll    │
  │                │                    │  thread            │
  │                │                    │                    │
  │  every ~50ms:  │◄──playhead_update──│                    │
  │  current word  │  position_samples  │ (pre-roll fills    │
  │  highlighted   │                    │  ring buffer;      │
  │                │                    │  cpal drains it)   │
  │                │                    │                    │
  ├─ Space again   │                    │                    │
  │─────────────►  │                    │                    │
  │                │  invoke pause      │                    │
  │                ├───────────────────►│                    │
  │                │                    │  drain ring buffer │
  │                │                    │  close cpal stream │
  │                │                    ├──────────────────x │
  │                │                    │                    │
  │                │◄──playback_stopped─│                    │
  │                │  position_samples  │                    │
  │                │                    │                    │
  │  cursor set    │                    │                    │
  │  to last word  │                    │                    │
```

---

## 3. Snapshot on idle

No user action for 30 seconds triggers an automatic snapshot.

```
Rust main thread    Idle timer       Background thread       SQLite
       │                │                    │                  │
       │                │                    │                  │
       │  user edits    │                    │                  │
       │  last command  │                    │                  │
       │────────────────────────────────────────────────────►   │
       │  writes        │                    │                  │
       │  journal row   │                    │                  │
       │                │                    │                  │
       │   (30s pass)   │                    │                  │
       │                ├── timer fires      │                  │
       │                │                    │                  │
       │◄───────────────┤                    │                  │
       │  trigger       │                    │                  │
       │  snapshot      │                    │                  │
       │                │                    │                  │
       │  clone root    │                    │                  │
       │  Arc (cheap)   │                    │                  │
       ├────────────────────────────────────►│                  │
       │  (main thread  │                    │                  │
       │   continues    │                    │  serialize the   │
       │   normally)    │                    │  snapshot blob   │
       │                │                    │  (bincode)       │
       │                │                    │                  │
       │                │                    ├─────────────────►│
       │                │                    │  store new turn  │
       │                │                    │  blobs; append   │
       │                │                    │  snapshot row    │
       │                │                    │  to journal      │
       │                │                    │  (type = 1)      │
       │                │                    │◄─────────────────┤
       │                │                    │  done            │
```

---

## 4. Open project with missing audio file

```
User          Svelte UI          Rust engine              SQLite
  │                │                    │                    │
  ├─ Open project  │                    │                    │
  │─────────────►  │                    │                    │
  │                │  invoke            │                    │
  │                │  open_project(path)│                    │
  │                ├───────────────────►│                    │
  │                │                    │  open sqlite       │
  │                │                    ├───────────────────►│
  │                │                    │  check user_version│
  │                │                    │  run migrations    │
  │                │                    │  load latest       │
  │                │                    │  snapshot          │
  │                │                    │◄───────────────────┤
  │                │                    │                    │
  │                │                    │  for each track:   │
  │                │                    │  resolve path      │
  │                │                    │  [track 1: OK]     │
  │                │                    │  [track 2: MISS]   │
  │                │                    │                    │
  │                │◄── missing_files ──│                    │
  │                │  [{id:2,           │                    │
  │  Missing Files │   name:"Bob mic",  │                    │
  │  dialog opens  │   last_path:"..."}]│                    │
  │                │                    │                    │
  ├─ clicks        │                    │                    │
  │  "Locate File" │                    │                    │
  │─────────────►  │                    │                    │
  │                │  open_file_dialog  │                    │
  │                ├───────────────────►│                    │
  │  selects file  │                    │                    │
  │                │  resolve_track_    │                    │
  │                │  file(track_id:2,  │                    │
  │                │   new_path)        │                    │
  │                ├───────────────────►│                    │
  │                │                    │  update track      │
  │                │                    │  meta blob (jrnl)  │
  │                │                    ├───────────────────►│
  │                │                    │◄───────────────────┤
  │                │                    │  apply deltas,     │
  │                │                    │  build trees       │
  │                │◄── project_state ──│                    │
  │  timeline      │    _updated        │                    │
  │  renders       │                    │                    │
```

---

## 5. Disfluency removal (bulk)

```
User          Svelte UI          Rust              Python sidecar       SQLite
  │                │                 │                    │                 │
  ├─ "Clean Up     │                 │                    │                 │
  │   Speech"      │                 │                    │                 │
  │─────────────►  │                 │                    │                 │
  │                │  invoke         │                    │                 │
  │                │  identify_      │                    │                 │
  │                │  disfluencies   │                    │                 │
  │                ├────────────────►│                    │                 │
  │                │                 │  enqueue task      │                 │
  │                │                 │  (in-memory)       │                 │
  │                │                 │                    │                 │
  │                │                 │──── request ──────►│                 │
  │                │                 │  identify_         │                 │
  │                │                 │  disfluencies      │                 │
  │                │                 │  {track_id,        │                 │
  │                │                 │   transcript}      │                 │
  │                │◄─task_progress──│◄── progress ───────│                 │
  │  progress bar  │  step=batch 1/4 │    pct=25          │                 │
  │                │◄─task_progress──│◄── progress ───────│                 │
  │                │  step=batch 4/4 │    pct=100         │                 │
  │                │                 │◄── result ─────────│                 │
  │                │                 │   {disfluencies:   │                 │
  │                │                 │    [{turn,word}]}  │                 │
  │                │                 │                    │                 │
  │                │                 │  apply             │                 │
  │                │                 │  identify_         │                 │
  │                │                 │  disfluencies cmd: │                 │
  │                │                 │  tag word types    │                 │
  │                │                 │  trigger snapshot  │                 │
  │                │                 │                    │                 │
  │  notify ready  │◄─task_completed─│                    │                 │
  │  dialog: apply?│                 │                    │                 │
  │                │                 │                    │                 │
  ├─ "Apply"       │                 │                    │                 │
  │─────────────►  │                 │                    │                 │
  │                │  invoke         │                    │                 │
  │                │  remove_        │                    │                 │
  │                │  disfluencies   │                    │                 │
  │                ├────────────────►│                    │                 │
  │                │                 │  compute deltas +  │                 │
  │                │                 │  inverse deltas    │                 │
  │                │                 │  apply cut_words   │                 │
  │                │                 │  / mute_words in   │                 │
  │                │                 │  batch (progress   │                 │
  │                │                 │  events to UI)     │                 │
  │                │                 │  push to undo;     │                 │
  │                │                 │  journal deltas    │                 │
  │                │◄─project_state──│                    │                 │
  │  transcript    │   _updated      │                    │                 │
  │  shows         │                 │                    │                 │
  │  strikethrough │                 │                    │                 │
```

---

## 6. Task cancellation mid-run

```
User          Svelte UI          Rust              Python sidecar
  │                │                 │                    │
  │                │                 │  task running      │
  │                │◄─task_progress──│◄── progress ───────│
  │  progress bar  │  pct=45         │                    │
  │                │                 │                    │
  ├─ clicks Cancel │                 │                    │
  │─────────────►  │                 │                    │
  │                │  invoke         │                    │
  │                │  cancel_task    │                    │
  │                │  (task_id)      │                    │
  │                ├────────────────►│                    │
  │                │                 │  mark task in the  │
  │                │                 │  in-memory queue   │
  │                │                 │  'cancelled'       │
  │                │                 │                    │
  │                │                 │──── cancel ───────►│
  │                │                 │  {type:"cancel",   │
  │                │                 │   request_id}      │
  │                │                 │                    │
  │                │                 │  (Python finishes  │
  │                │                 │   current chunk,   │
  │                │                 │   checks cancel    │
  │                │                 │   flag)            │
  │                │                 │                    │
  │                │                 │◄── error ──────────│
  │                │                 │   code="cancelled" │
  │                │                 │                    │
  │                │◄─task_error─────│                    │
  │  progress bar  │  code=cancelled │                    │
  │  clears        │                 │                    │
  │                │                 │  (models remain    │
  │                │                 │   loaded; sidecar  │
  │                │                 │   ready for next   │
  │                │                 │   request)         │
```

---

## 7. App startup and sidecar initialization

```
OS            Tauri (Rust)         Python sidecar      Webview (Svelte)
  │                │                    │                    │
  ├─ launch app    │                    │                    │
  ├──────────────► │                    │                    │
  │                │  load settings.json│                    │
  │                │  open log files    │                    │
  │                │                    │                    │
  │                │  spawn sidecar     │                    │
  │                ├───────────────────►│                    │
  │                │                    │  init structlog    │
  │                │                    │  load manifest.json│
  │                │                    │  start NDJSON loop │
  │                │                    │──── log "ready" ──►│
  │                │                    │                    │ (Rust listens)
  │                │◄─── "sidecar ready"│                    │
  │                │                    │                    │
  │                │  show webview      │                    │
  │                ├───────────────────────────────────────►│
  │                │                    │                    │  render welcome
  │                │                    │                    │  screen
  │                │                    │                    │
  │  (if recent    │                    │                    │
  │   project      │                    │                    │
  │   exists,      │                    │                    │
  │   show in UI)  │                    │                    │
```
