---
title: Settings
weight: 2
---

Full schema and migration semantics are documented in
[`design/ops.md § Settings schema`](https://github.com/mculbert/vocalboard/blob/main/design/ops.md#settings-schema-phase-1).

## Phase 1 keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `version` | integer | `1` | Settings format version; incremented on each migration |
| `model_dir` | string \| null | `null` | Default directory for model downloads and enumeration |
| `model_paths.transcription` | string \| null | `null` | Selected WhisperX model path |
| `model_paths.vad` | string \| null | `null` | Selected VAD (pyannote) model path |
| `model_paths.forced_alignment` | string \| null | `null` | Selected forced-alignment model path |
| `model_paths.enhancement` | string \| null | `null` | Selected MP-SENet enhancement model path |
| `model_paths.sound_classification` | string \| null | `null` | Selected YAMNet sound-classification model path |
| `model_paths.llm` | string \| null | `null` | Selected Gemma LLM model path |
| `default_sample_rate` | integer | `48000` | Sample rate (Hz) applied to new projects |
| `speaker_merge_threshold` | float | `0.71` | Cosine-similarity threshold for automatic speaker merging |
| `resampling_quality` | string | `"balanced"` | Audio resampling quality (`"fast"`, `"balanced"`, `"high"`) |
| `gpu_enabled` | boolean | `false` | Enable GPU acceleration for ML inference |
| `snapshot_idle_seconds` | integer | `30` | Seconds of inactivity before writing a timeline snapshot |
| `model_idle_unload_seconds` | integer | `300` | Seconds before an idle ML model is unloaded from memory |
| `update_feed_url` | string \| null | `null` | URL for the update-check feed (null = disabled) |
| `recent_projects` | array | `[]` | Ordered list of recently opened project paths |

Settings manual — full prose to be written in M7.
