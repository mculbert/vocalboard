# Architecture

## Component diagram

```
┌─────────────────────────────────────────────────────────────┐
│  Tauri application process (Rust)                           │
│                                                             │
│  ┌──────────────────┐      ┌──────────────────────────────┐ │
│  │   Webview (UI)   │      │        Rust core             │ │
│  │                  │      │                              │ │
│  │  Svelte 5 +      │      │  ProjectState  Journal       │ │
│  │  SvelteKit       │◄────►│  Turn trees   Snapshot mgr   │ │
│  │  Bits UI         │ IPC  │  Blob store   Task queue     │ │
│  │  Tailwind        │      │  Audio engine Sidecar mgr    │ │
│  │  Paraglide-js    │      │               File resolver  │ │
│  └──────────────────┘      └──────────────┬───────────────┘ │
│                                           │                 │
└───────────────────────────────────────────┼─────────────────┘
                                            │ Tauri sidecar stdio
                                            │ (NDJSON)
                                            │
                              ┌─────────────▼───────────────┐
                              │  Python sidecar (ML)        │
                              │                             │
                              │  Model registry  (lazy)     │
                              │  WhisperX        Pyannote   │
                              │  MP-SENet        YAMnet     │
                              │  Gemma (llama.cpp/GGUF)     │
                              │  Silero VAD                 │
                              └─────────────────────────────┘

 ──────  Filesystem (SQLite project DB, .vbdata/, model dir)
```

## Process model

The application consists of exactly **two OS processes** at runtime:

### Rust process (Tauri)

The Tauri process is the application. It owns:

- The Tauri webview window (Chromium-based; hosts the Svelte UI)
- The Rust engine: project state (implicit timeline trees, content-addressed blob store + edit journal), SQLite I/O (via `rusqlite`), audio decoding (`symphonia` + optional `ffmpeg` subprocess), audio playback (`cpal`), rubato resampling (at import, into the resampled cache), room-tone detection, track alignment (FFT cross-correlation), EDL builder
- The task queue scheduler (in-memory in Phase 1; dispatches to Python)
- The sidecar manager (spawns and communicates with the Python process)
- The snapshotting background thread

In Phase 1 there is one Tauri process and one project window. The design should be structured so that Phase 6 can add a second window by instantiating a second `ProjectState` with its own SQLite handle, rather than rearchitecting global state.

### Python sidecar process

Spawned at application start by Tauri via the sidecar mechanism. It runs for the lifetime of the app. It is:

- A long-running service process
- Stateless with respect to the project (Rust sends all context it needs per request)
- ML-model-aware: it maintains a registry of loaded models, loaded lazily on first use, unloaded after a configurable idle timeout (default: 5 minutes)
- **Strictly ML inference.** Non-ML signal processing — resampling, room-tone detection, non-speech sound *detection*, and track alignment (FFT cross-correlation) — runs in Rust, not here. (For sound events, only the optional YAMnet *labeling* of already-detected events is ML and runs here.)

The Python binary is a **Nuitka-compiled** executable bundled inside the Tauri app package. It uses `importlib` for importing PyTorch and WhisperX at runtime (required by Nuitka's model for these libraries). Per-platform builds: macOS arm64, macOS x86_64, Windows x64, Linux x64.

## IPC protocol

Communication between the Rust process and the Python sidecar uses **newline-delimited JSON (NDJSON)** over the process's stdio streams:

- **stdout** (Python → Rust): response/progress stream
- **stdin** (Rust → Python): request and control stream
- **stderr** (Python → Rust): unstructured logs and unexpected crash output; not part of the protocol

### Envelope format

Every message (in both directions) is a single JSON object on a single line, terminated by `\n`.

**Rust → Python (stdin):**

```json
{
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "request",
  "command": "transcribe_track",
  "version": 1,
  "payload": { ... }
}
```

```json
{
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "cancel"
}
```

**Python → Rust (stdout):**

```json
{ "request_id": "...", "type": "progress", "step": "transcribe", "step_index": 1, "step_count": 4, "pct": 42, "label": "Transcribing…" }
{ "request_id": "...", "type": "log", "level": "info", "msg": "loaded whisperx model" }
{ "request_id": "...", "type": "result", "payload": { ... } }
{ "request_id": "...", "type": "error", "code": "model_load_failed", "message": "..." }
```

### Multiplexing

The protocol *transport* multiplexes: multiple requests may be in-flight on the channel simultaneously (e.g. a quick `classify_sounds` while a longer task progresses). Each message carries a `request_id` (UUIDv4); Python tags every response with the matching ID, and Rust routes incoming stdout lines by `request_id` to the appropriate task future. **Transport multiplexing is not unconstrained model concurrency, however:** heavy inference units (WhisperX, the LLM) cannot co-reside on consumer hardware, so the scheduler serializes heavy work behind a lock and the registry evicts pre-emptively — see [ml-pipeline.md § Model resource management & scheduling](ml-pipeline.md#model-resource-management--scheduling).

### Back-pressure

Python writes at most one progress line per second per request unless explicitly needed for finer granularity. Rust does not apply flow-control on stdin writes; all requests are small JSON objects.

### Cancellation semantics

When Rust sends `{"type":"cancel","request_id":"..."}`, Python's task loop checks a per-request cancel flag at every safe checkpoint (e.g., between audio chunks, between model calls). If the underlying model is a llama.cpp subprocess, Python sends `SIGTERM` (or `TerminateProcess` on Windows) to the child. On receiving a cancel, Python emits `{"type":"error","code":"cancelled","request_id":"..."}` and returns to its ready state; it does **not** unload models. Rust marks the corresponding task as `cancelled` in its in-memory task queue.

### Model idle unload

Python tracks the last-use timestamp of each loaded model. A background timer (default 5-minute idle) unloads any model not used in that window by calling its cleanup routine and releasing the reference. This frees GPU/CPU memory between sessions without requiring a sidecar restart.

## Lifecycle

```
App start
  │
  ├─ Rust: open/create settings.json (tauri-plugin-store)
  ├─ Rust: spawn Python sidecar (Tauri sidecar API)
  │         Python: start NDJSON loop, load model registry manifest
  │
  ├─ Show welcome screen  ──────────► New project flow
  │                                       │
  │                                       └─ Open SQLite, write project row
  │
  └──────────────────────────────────► Open project flow
                                           │
                                           ├─ Load latest snapshot from SQLite
                                           ├─ Apply journal deltas after snapshot
                                           ├─ Resolve audio file paths
                                           ├─ Construct PlaybackEngine (open cpal stream at
                                           │    the locked rate; device-open is non-fatal)
                                           └─ Render UI

Running
  ├─ User edit → Rust applies command → records delta(s) in journal → updates timeline tree → emits Tauri event → UI updates
  ├─ Idle timer fires → Rust clones the root Arc (structural sharing) → background thread serializes snapshot → writes to store + journal
  ├─ User triggers ML task → Rust enqueues task (in-memory) → dispatches to Python via stdin → Python streams progress → Rust applies result commands

App exit
  ├─ Cancel any in-flight Python tasks
  ├─ Write final snapshot
  └─ SQLite WAL checkpoint
```

The `PlaybackEngine` is **managed state constructed at project open** (a playback slot alongside the project state), so its `cpal` stream is opened once at the negotiated device rate and reused across every play/stop cycle (see [audio-pipeline.md § Output stream](audio-pipeline.md#output-stream)). **Device-open failure is non-fatal:** the project still opens (with an empty playback slot), and `play_from` / `pause` / `stop` return `audio_io_error` until a device is available. A second project window owns its own engine and stream.

## Security boundaries

The requirements document mandates: *"Frontend can only trigger explicitly allowed backend events through the established API (no sending raw scripts or commands)."* This section specifies how that invariant is enforced.

### Trust levels

| Component | Trust level | Can do |
|-----------|-------------|--------|
| Webview (JS/Svelte) | Untrusted | Invoke named Tauri commands; listen to Tauri events |
| Rust core | Trusted | All local I/O; spawn Python sidecar; apply project mutations |
| Python sidecar | Limited trust | Read audio data provided by Rust; return ML results; no direct filesystem access to project sqlite |

### Webview → Rust

The webview may only call **explicitly registered Tauri commands** (defined with `#[tauri::command]`). There is no mechanism for the frontend to:
- Execute arbitrary shell commands
- Access the filesystem outside of Tauri's dialog-scoped paths
- Communicate directly with the Python sidecar

Each Tauri command corresponds to exactly one entry in the [command surface](command-surface.md) and performs JSON-schema validation of its parameters before acting. Unknown or malformed parameters are rejected with an error code; no partial execution occurs.

### Rust → Python

Rust communicates with the Python sidecar only via the NDJSON stdio protocol. Python does not receive:
- Raw filesystem paths to the project sqlite file (it never reads/writes the DB directly)
- Shell commands or scripts
- Arbitrary Python code to evaluate

The Python sidecar is granted read access to source audio files (path passed in the request payload) and write access to the `.vbdata/` directory (for enhanced audio output). It has no other filesystem scope.

### Content Security Policy

The webview has a strict CSP (enforced via `tauri.conf.json`):
- `default-src 'self'`
- `script-src 'self'` (no `'unsafe-eval'`, no `'unsafe-inline'` beyond Tauri's injected bootstrap)
- `connect-src 'self' ipc: tauri:`
- No external network requests from the UI

### Python sidecar isolation

Because Python is a compiled Nuitka binary, it cannot be trivially replaced by a malicious third-party process. However:
- On first launch, the sidecar binary's hash is verified against the embedded manifest
- The sidecar is spawned with a restricted environment (no `HOME`-writable scripts path changes)
- Stdout is the only channel Python uses to influence Rust state; each message is parsed against a strict schema before any action is taken
