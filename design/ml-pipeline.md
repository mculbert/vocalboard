# ML Pipeline

## Sidecar internals

The Python sidecar is a long-running process that handles all ML inference. It communicates exclusively via the NDJSON stdio protocol described in [architecture.md § IPC protocol](architecture.md#ipc-protocol).

### Startup

1. Initialize `structlog` with rotating file handler pointing to the app data dir.
2. Load the model registry manifest from the model directory (`models/manifest.json`).
3. Start the NDJSON dispatch loop: read one line from stdin, parse as JSON, route by `command` field, run in a thread pool, stream results to stdout.
4. Emit `{ "type": "log", "level": "info", "msg": "sidecar ready", "request_id": null }` to signal readiness.

Rust waits for this "sidecar ready" signal (with a 30s timeout) before proceeding with app initialization.

### Model registry

The model registry is a lazy dictionary: `Dict[str, LoadedModel]`. Models are **not loaded at startup**; they are loaded on the first request that requires them.

```python
class ModelRegistry:
    def get(self, role: str) -> LoadedModel:
        if role not in self._loaded:
            self._loaded[role] = self._load(role)
            self._last_used[role] = time.monotonic()
        return self._loaded[role]

    def _idle_unload_loop(self):
        # Background thread; runs every 60 seconds
        for role in list(self._loaded.keys()):
            if time.monotonic() - self._last_used[role] > IDLE_TIMEOUT_SECONDS:
                self._unload(role)
```

The `IDLE_TIMEOUT_SECONDS` is passed to the sidecar at startup from the settings value `model_idle_unload_seconds` (default: 300).

**Per-role model paths.** Each role's *selected* model is configured in app settings (`model_paths` keyed by role; see [data-model.md § App settings](data-model.md#app-settings)). Rust resolves the selected path for a role from settings and passes it in the task request payload; `_load(role)` loads from that path. A path may point inside `model_dir` (a built-in/downloaded model) or to an external user-supplied location, and its shape is role-specific — a **directory** for WhisperX/pyannote/MP-SENet/YAMnet, a single **`.gguf` file** for Gemma. To populate the Settings → Models picker, Rust **scans `model_dir`** to enumerate the available (downloaded) models per role; the user selects one, or supplies an external path. A role may have no model selected (`null`), which is allowed.

The six roles are: `transcription`, `vad`, `forced_alignment` (wav2vec2 word-timestamp alignment in WhisperX — distinct from Rust-side *track* alignment), `enhancement`, `sound_classification`, `llm`.

### Model directory layout

```
<model_dir>/
├── manifest.json
├── whisperx/
│   ├── base/
│   │   ├── weights.pt
│   │   └── meta.json
│   └── large-v3/
│       └── ...
├── pyannote/
│   └── speaker-diarization-3.1/
│       └── ...
├── gemma/
│   ├── gemma-3-1b-q4_k_m.gguf
│   ├── gemma-3-4b-q8.gguf
│   └── gemma-3-12b-q4_k_m.gguf
├── mp-senet/
│   └── v1/
│       └── ...
└── yamnet/
    └── v1/
        └── ...
```

### manifest.json

```json
{
  "version": 1,
  "models": [
    {
      "role": "transcription",
      "name": "whisperx-base",
      "path": "whisperx/base",
      "size_bytes": 145000000,
      "sha256": "abc123...",
      "min_ram_gb": 2,
      "bundled": true
    },
    {
      "role": "llm",
      "name": "gemma-3-12b-q4_k_m",
      "path": "gemma/gemma-3-12b-q4_k_m.gguf",
      "size_bytes": 8000000000,
      "sha256": "...",
      "min_ram_gb": 12,
      "bundled": false
    }
  ]
}
```

On model download, Rust verifies the SHA-256 of the downloaded file against the manifest before moving it to the model directory.

## WhisperX transcription pipeline

Invoked by the `transcribe_track` task.

### Pre-processing (in Python)

1. Decode source audio to f32 mono PCM at 16 kHz (WhisperX's required rate) using PyAV or `torchaudio`.
2. **Loudness normalization**: compute integrated LUFS (via `pyloudnorm`) and apply a linear gain to target −23 LUFS (ITU-R BS.1770-4).
3. Optional **high-pass filter** at 80 Hz to remove DC offset and low-frequency rumble (4th-order Butterworth, `scipy.signal`).

### Transcription quality gate

After transcription, check:
- If the mean `avg_logprob` across all segments < −1.0, **reject** and report `error` with `code: "low_confidence_transcript"`.
- If > 20% of individual segments have `avg_logprob` < −1.0, reject.
- Detect transcription despite `no_speech_prob` > 0.6: flag these segments as suspect in the result.

### WhisperX steps

1. **Transcription**: `whisperx.load_model()` + `model.transcribe(audio, ...)` — produces word-level timestamps.
2. **Alignment**: `whisperx.load_align_model()` + `whisperx.align()` — aligns whisper tokens to wav2vec2 force-aligned timestamps.
3. **Diarization**: `whisperx.DiarizationPipeline` (backed by pyannote) — assigns speaker labels to each segment.

### Result format

Python returns word-level result as part of the `result` payload:

```json
{
  "turns": [
    {
      "speaker_id_local": 0,
      "embedding_blob_b64": "...",
      "words": [
        { "text": "Hello", "start_sec": 1.24, "end_sec": 1.58, "word_type": "normal" }
      ]
    }
  ]
}
```

Room tone and non-speech sound events are **not** returned here. Both the room-tone search and the non-speech *detection* sweep are signal processing, so they run in Rust at import as part of applying `import_speech_track` (see [audio-pipeline.md § Room tone detection](audio-pipeline.md#room-tone-detection) and [§ Non-speech sound detection](audio-pipeline.md#non-speech-sound-detection)). Rust deserializes this transcription result and builds the track's implicit timeline tree. The only ML step for non-speech is *labeling*: if a YAMnet model is selected, Rust dispatches a separate `classify_sounds` task over the audio of the Rust-detected events (see [§ YAMnet sound classification](#yamnet-sound-classification)).

## Enhancement pipeline (MP-SENet)

Invoked by the `enhance_track` task.

### Processing

1. Decode source audio to f32 PCM (source sample rate preserved for processing; MP-SENet operates at 16 kHz internally).
2. Split into **2–5 second chunks** overlapping by a few milliseconds.
3. Run each chunk through MP-SENet, collecting the enhanced output.
4. **Pipeline delay compensation**: measure the group delay of the MP-SENet model once at startup (by processing an impulse and finding the peak of the cross-correlation); subtract this delay from the enhanced output when stitching chunks.
5. Stitch chunks with 50ms crossfade overlaps.
6. Resample output back to project sample rate via `torchaudio.transforms.Resample`.
7. Write enhanced audio as FLAC to `<project>.vbdata/enhanced/<track_name>-enhanced.flac`.

The wet/dry slider in the UI produces a linear blend of enhanced vs. original; blending happens at the audio splice read time in the Rust EDL engine (the splice reads from the enhanced FLAC at `wet_ratio` gain and from the **resampled cache** — `resampled/<track>.flac`, project rate — at `(1-wet_ratio)` gain, summed; both are at the project rate, so the two streams are sample-aligned). The ratio is persisted per track as `TrackMeta.wet_dry_ratio` and is a playback/export mixing setting — it is **not** a parameter of the `enhance_track` command (which only produces the enhanced FLAC).

## Disfluency detection (Gemma)

Invoked by the `identify_disfluencies` task.

### Model selection

The Gemma model is **whatever the user selected** for the `llm` role; the choice is made once at model download or in Settings, not per task. Rust resolves `model_paths.llm` (a single `.gguf` file) and passes its path in the request payload — the same "Rust injects from settings" rule used by all ML task commands. If no `llm` model is selected, or the selected file is missing, Python returns `code: "model_not_available"` (with the model name) and Rust prompts the user to download/select via the model dialog.

> The RAM tiers from the requirements (≥ 16 GB → 12B, 8–16 GB → 4B, < 8 GB → 1B) are **recommendation** guidance for the model-download dialog — matched against manifest `min_ram_gb` + detected RAM to pre-select a sensible default — **not** a runtime per-task selection.

### Batching

The transcript is batched for Gemma processing. Batch size is chosen from the **selected model's class** (derived from its context length / metadata), not from detected RAM:

| Model | Batch size | Overlap |
|---|---|---|
| Gemma 3 1B | 4 turns | 2 turns |
| Gemma 3 4B | ~2–3k words | ~2 min / ~300 words |
| Gemma 3 12B | ~4–6k words | ~2 min |

### Prompt template

Instead of returning word indices, the model **reproduces the batch's input text** and wraps each disfluency in a typed XML tag — `<filler>`, `<stutter>`, `<repetition>`, or `<repair>` (spans may cover multiple words). Structured tagging keeps the model anchored to the actual tokens (sharper than free-form index lists):

```
You are an expert transcript editor. Reproduce the transcript below EXACTLY,
adding XML tags around disfluencies, by type:
  <filler>…</filler>       filler words (um, uh, like, you know, I mean)
  <stutter>…</stutter>     stutters / partial-word repetitions (th-th-that)
  <repetition>…</repetition>  repeated words or phrases
  <repair>…</repair>       false starts / self-corrections (come- I mean)
Do not change, add, or drop any words. Do not explain.

Transcript:
I want I want to come- I mean go to th-th-that uh party

Output:
<repetition>I want</repetition> I want to <repair>come- I mean</repair> go to <stutter>th-th-</stutter>that <filler>uh</filler> party
```

### Batch overlap

The batch overlap (above) exists **only** to give the LLM linguistic context across batch boundaries — it imposes no restriction on which words may be identified or cut. A word that lands in the overlap of two batches is treated as disfluent if it is tagged in **either** batch (union). (The "mute, not cut" requirement is a *separate* rule about words that overlap a turn from another track; it is applied by Rust at `remove_disfluencies` time, not here.)

### Result

Python strips the tags, **diff-aligns** the tagged output back to the input word positions, and returns one entry per disfluent word. Because LLMs do not always echo the input verbatim, the alignment is a fuzzy/token diff (not a byte-equality check) that tolerates minor reproduction drift and maps each tagged span onto its constituent input words. Phase 1 collapses every tag type to a single disfluency mark — the subtype is used to sharpen detection but is **not** stored:

```json
{
  "disfluencies": [
    { "turn_id": 42, "word_index": 3 },
    { "turn_id": 42, "word_index": 7 }
  ]
}
```

Rust applies the result by setting each listed word's `word_type` to `Disfluency` via the `identify_disfluencies` command. The subsequent `remove_disfluencies` command **cuts** all disfluency-typed words in scope, falling back to **mute** for any word that overlaps a turn from another track (the general `overlapping_word_cannot_cut` rule).

## Speaker diarization and embeddings

Diarization runs as part of the WhisperX pipeline (`DiarizationPipeline`). Each turn gets a `speaker_id_local` (0-based index within the track) and an embedding vector.

### Embedding storage

The mean embedding for each speaker is stored as a content-addressed `store` blob (normalized f32 vector), referenced by hash from `SpeakerMeta.embedding_hash` — there is no `speakers` table under the three-table schema (see [data-model.md § Non-timeline data](data-model.md#non-timeline-data)). When a new track is imported, Python includes per-turn embeddings in the result. Rust compares each new speaker's mean embedding against existing speakers using cosine similarity. If similarity > the merge threshold, the new speaker is merged with the existing one; otherwise a new `Speaker N` entry is created.

The threshold is read from the `speaker_merge_threshold` app setting (default 0.71; see [data-model.md § App settings](data-model.md#app-settings)).

### Speaker naming

New speakers are named `Speaker 1`, `Speaker 2`, etc. (incrementing from the highest existing speaker number in the project). The user may rename any speaker at any time.

## YAMnet sound classification

YAMnet provides classification *labels* for non-speech sound events. **Detection** of the events is done in Rust (the RMS sweep — see [audio-pipeline.md § Non-speech sound detection](audio-pipeline.md#non-speech-sound-detection)); labeling is the only ML part, and it is **optional**.

The flow: Rust detects the events at import and creates each as a Sound turn whose word text defaults to `"[Sound]"`. If a `sound_classification` (YAMnet) model is selected, Rust then dispatches a `classify_sounds` task with the per-event audio; Python runs YAMnet and returns the top label per event:

```json
{ "labels": [ { "event_index": 0, "label": "[Laughter]" }, { "event_index": 1, "label": "[Music]" } ] }
```

Rust applies the labels via the `classify_sounds` command (updating each Sound word's text). If no YAMnet model is selected, the events simply keep `"[Sound]"` and no Python call is made. Phase 1 support is minimal (top-1 label only); richer classification is a Phase 6 enhancement.

## GPU detection and acceleration

By default, the sidecar runs with **CPU-only PyTorch**. The user can trigger GPU acceleration setup via **Settings → Advanced → Detect & Install GPU Acceleration**.

When triggered, Rust sends a `detect_gpu` request to the sidecar. Python runs `torchruntime.get_device_info()` (or equivalent hardware detection) and returns a device description. Rust then downloads the appropriate:

- CUDA-enabled torch wheel (Windows/Linux, NVIDIA GPU)
- ROCm-enabled torch wheel (Windows/Linux, AMD GPU)
- Metal-accelerated torch (already included in the macOS build via `mps` backend)
- CPU-optimized llama.cpp build (always available)
- CUDA/Metal/Vulkan llama.cpp build if GPU detected

The GPU torch package replaces the CPU-only package; sidecar restarts to load it.

### llama.cpp builds

The bundled llama.cpp (via `llama-cpp-python`) provides:
- `llama_cpp.LLAMA_SUPPORTS_GPU_OFFLOAD`: checked at runtime
- `n_gpu_layers = -1` (offload all layers) when GPU is available
- Fallback to CPU if GPU memory is insufficient for the selected model

## Cancellation

Per the architecture spec, Python checks a per-request cancel flag at every safe checkpoint:
- Between audio chunks in MP-SENet
- Between transcript segments in WhisperX
- Between batches in Gemma
- Between speaker turn comparisons in pyannote

For llama.cpp, cancellation sends `SIGTERM` (Unix) or `TerminateProcess` (Windows) to the llama.cpp subprocess. Python waits up to 5 seconds; if no exit, sends `SIGKILL` / `TerminateProcess(force=True)`.

On successful cancellation, Python emits:
```json
{ "request_id": "...", "type": "error", "code": "cancelled", "message": "Task cancelled by user" }
```
