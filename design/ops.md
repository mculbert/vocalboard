# Operations

## Repository layout

```
vocalboard/
├── design/                 Technical design documents (this folder)
├── docs/                   Hugo documentation site
│   ├── hugo.toml
│   └── content/
│       ├── reference/      Feature reference manual
│       ├── settings/       Settings reference manual
│       └── internals/      Internals reference manual
│           ├── *.md        Hand-authored overview docs (architecture, data structures)
│           └── api/        Auto-generated Markdown API docs (see § Internal API documentation)
│               ├── rust/
│               ├── python/
│               └── frontend/
│
├── src-tauri/              Tauri configuration and Rust workspace root
│   ├── Cargo.toml          Workspace manifest
│   ├── tauri.conf.json     Tauri bundle configuration
│   ├── capabilities/       Tauri 2 capability files (permission declarations)
│   │
│   ├── app/                Crate: Tauri shell (main binary)
│   │   ├── src/
│   │   │   ├── main.rs     Application entry point; sets up Tauri builder
│   │   │   ├── commands/   Tauri command handlers (one file per command group)
│   │   │   └── events.rs   Tauri event emitters
│   │   └── tests/          Crate integration tests (unit tests live inline, #[cfg(test)])
│   │
│   ├── core/               Crate: Project engine (no Tauri dependency)
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── project/    ProjectState, timeline tree, Journal, SnapshotManager
│   │   │   ├── audio/      EDL builder, playback engine, decoder, room-tone, exporter
│   │   │   ├── db/         SQLite schema, migrations, queries (rusqlite)
│   │   │   ├── task/       TaskQueue, TaskDispatcher, SidecarManager
│   │   │   └── ipc/        NDJSON types shared with Python (mirrors proto/)
│   │   └── tests/          Crate integration tests (unit tests live inline, #[cfg(test)])
│   │
│   └── proto/              Crate: Shared IPC type definitions
│       └── src/
│           ├── commands.rs     Command param/result types (Serde JSON)
│           └── events.rs       Sidecar event types
│
├── src/                    SvelteKit frontend (referenced from tauri.conf.json)
│   └── ...                 (see frontend.md; Vitest unit tests colocated as *.test.ts / *.svelte.test.ts)
│
├── e2e/                    Playwright end-to-end tests (run before releases; NOT in CI)
│
├── python/                 Python sidecar source
│   ├── pyproject.toml
│   ├── vocalboard_sidecar/
│   │   ├── __main__.py     Entry point (NDJSON dispatch loop)
│   │   ├── registry.py     Model registry + idle-unload timer
│   │   ├── tasks/
│   │   │   ├── transcribe.py
│   │   │   ├── enhance.py
│   │   │   ├── disfluency.py
│   │   │   ├── classify_sounds.py  (YAMnet labeling of Rust-detected events)
│   │   │   └── gpu_detect.py     (alignment & detection are Rust-side, not Python tasks)
│   │   └── models/
│   │       ├── whisperx.py
│   │       ├── pyannote.py
│   │       ├── mp_senet.py
│   │       ├── gemma_llama.py
│   │       └── yamnet.py
│   └── tests/
│       └── ...
│
├── scripts/                Build helpers, model manifest generator, release scripts,
│                           API-doc generators (see § Internal API documentation)
└── .github/
    └── workflows/
        ├── ci.yml          Runs on every push: tests, type checks, lints
        └── release.yml     Builds and packages on tag push
```

## Rust crate dependencies (key)

| Crate | Purpose |
|---|---|
| `tauri` (v2) | App shell, webview, IPC |
| `tauri-plugin-shell` | Sidecar process management |
| `tauri-plugin-store` | Settings JSON persistence |
| `tauri-plugin-dialog` | File open/save dialogs |
| `rusqlite` (with `bundled` feature) | SQLite, bundled static |
| `serde` / `serde_json` | Serialization |
| `symphonia` | Audio decoding (pure Rust) |
| `cpal` | Cross-platform audio output |
| `rubato` | Sinc resampling |
| `tokio` | Async runtime for sidecar I/O and task dispatch |
| `uuid` | UUIDv4 for request IDs |
| `tracing` + `tracing-appender` | Structured logging with rolling file output |
| `anyhow` | Error handling |

## Python dependencies (key)

| Package | Purpose |
|---|---|
| `whisperx` | Transcription + alignment + diarization |
| `pyannote.audio` | Speaker diarization (via WhisperX) |
| `torch` (CPU default) | ML inference backend |
| `torchaudio` | Audio I/O and resampling |
| `llama-cpp-python` | Gemma GGUF inference |
| `tensorflow-lite` or `torch` | YAMnet (TBD) |
| `scipy` | DSP utilities (HPF, room tone) |
| `pyloudnorm` | Loudness normalization (LUFS) |
| `PyAV` | FFmpeg-based audio decode for Python |
| `structlog` | Structured logging |
| `nuitka` (build-time only) | Compilation |
| `pydoc-markdown` (docs-build only) | Docstring → Markdown API docs |

## Python packaging: Nuitka

The Python sidecar is compiled with [Nuitka](https://nuitka.net/) into a standalone executable per platform. Key Nuitka considerations:

- `torch`, `whisperx`, and `pyannote` must be loaded via `importlib.import_module()` at runtime rather than top-level `import` statements, due to Nuitka's static analysis limitations with these libraries.
- The compiled binary includes a Python 3.11 interpreter and all pure-Python dependencies.
- C extensions (torch, etc.) are included as `.so`/`.dll` files in the distribution folder.
- A `build_sidecar.sh` / `build_sidecar.ps1` script in `scripts/` handles the Nuitka invocation with the correct flags.
- The output is a **one-folder** distribution (not one-file) for fast startup. The folder is placed at `src-tauri/binaries/<platform>/` and referenced in `tauri.conf.json` as the sidecar binary.

> **Note:** Nuitka build flags (optimization level, plugin list, etc.) will be finalized during initial implementation. They are not locked down in this TDD.

## Build targets

| Platform | Architecture | Notes |
|---|---|---|
| macOS | arm64 | Apple Silicon native |
| macOS | x86_64 | Intel Macs; also runs under Rosetta on arm64 |
| Windows | x64 | NSIS installer |
| Linux | x64 | AppImage + `.deb` |

macOS builds are produced as a universal binary (`lipo`-merged arm64 + x86_64) where Rust supports it. The Python sidecar is built separately for each architecture and bundled per-arch.

## Test layout

| Layer | Unit tests | Integration / E2E | Runner |
|---|---|---|---|
| Rust | Inline `#[cfg(test)] mod tests` in each `src/**/*.rs` | `tests/` dir at each crate root (`src-tauri/core/tests/`, `src-tauri/app/tests/`; `proto/` as needed) | `cargo test --workspace` |
| Frontend | Colocated `*.test.ts` / `*.svelte.test.ts` beside source under `src/lib/...` | `e2e/` (Playwright, repo root) | `pnpm test` (unit); Playwright (E2E) |
| Python | `python/tests/` | `python/tests/` | `pytest python/tests/` |

Playwright E2E tests are **not** run in CI (they drive the full Tauri app); they are run manually before releases. All other suites run on every push (see [§ CI / CD](#ci--cd)).

## CI / CD

GitHub Actions. Two workflow files:

### `ci.yml` (every push, every PR)

```yaml
jobs:
  rust-tests:
    runs-on: [ubuntu-latest, windows-latest, macos-latest]
    steps:
      - cargo fmt --check
      - cargo clippy -- -D warnings   # incl. workspace lints: unwrap/expect/panic/complexity
      - cargo test --workspace        # incl. format round-trip fixtures
      - cargo audit                   # CVE advisories
      - cargo deny check              # license + ban policy (deny.toml)

  python-tests:
    runs-on: [ubuntu-latest, windows-latest, macos-latest]
    steps:
      - pytest python/tests/ -v
      - pip-audit                     # CVE advisories

  frontend-tests:
    runs-on: [ubuntu-latest, windows-latest, macos-latest]
    steps:
      - pnpm install
      - pnpm check     (svelte-check + tsc; a11y warnings fail)
      - pnpm lint      (eslint incl. svelte/a11y-* + no-hardcoded-string check)
      - pnpm test      (vitest)
      - pnpm build     (verify static build succeeds)
```

These gates enforce the machine-checkable rules in [conventions.md](conventions.md#enforcement-summary).

E2E tests (Playwright driving Tauri) are **not in CI**; they are run manually before releases.

### `release.yml` (on `v*` tag push)

Builds the full Tauri app on each platform (macOS arm64, macOS x86_64, Windows x64, Linux x64), creates a GitHub Release, and attaches the app bundles as release assets.

Code signing and notarization are deferred until the first public release. Pre-release builds will trigger Gatekeeper/SmartScreen warnings that early-adopter users must override.

## Internal API documentation

The Internals reference manual combines **hand-authored overview docs** (architecture, data structures, the backend/frontend API contract) with **auto-generated interface/data-structure docs derived from source docstrings**. The generated docs are emitted as **Markdown** (not rendered HTML) into the Hugo content tree so Hugo renders them alongside the overviews under one navigation.

| Language | Tool | Output |
|---|---|---|
| Python | `pydoc-markdown` | `docs/content/internals/api/python/` |
| TypeScript / Svelte | `typedoc` + `typedoc-plugin-markdown` (dev deps) | `docs/content/internals/api/frontend/` |
| Rust | nightly `rustdoc --output-format json` (`-Z unstable-options`) piped through a converter script in `scripts/` | `docs/content/internals/api/rust/` |

Rust has no mature stable docstring→Markdown generator (`cargo doc` emits HTML only), so the Rust path renders rustdoc's JSON output to Markdown via a small `scripts/` converter. This is the **only** step that requires a nightly toolchain (the app itself builds on stable). The three generators are wrapped by a `scripts/` entry point (e.g. `gen_api_docs.sh` / `.ps1`) and can be re-run locally or as a docs-build step before publishing the Hugo site.

## Distribution

Phase 1 distribution is **direct download from GitHub Releases** only. No app store submissions, no package managers, no auto-update in Phase 1.

Auto-update infrastructure is reserved for Phase 6. The settings schema includes a `update_feed_url` key (initially `null`) so Phase 6 can enable it without a settings migration.

## App data directory layout

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/Vocalboard/` |
| Windows | `%APPDATA%\Vocalboard\` |
| Linux | `~/.local/share/vocalboard/` |

```
Vocalboard/
├── settings.json          App settings (tauri-plugin-store)
├── models/                Model weights (default; user-configurable)
│   └── manifest.json
├── logs/
│   ├── rust.log           Rolling log file (tracing-appender)
│   └── python.log         Rolling log file (structlog)
└── cache/                 Miscellaneous ephemeral data
```

## Logging and diagnostics

### Rust

`tracing` with `tracing-appender` rolling file appender. Log files rotate daily; last 7 days retained. Log level: `INFO` in production, `DEBUG` when `VOCALBOARD_LOG=debug` env var is set.

### Python

`structlog` configured with JSON renderer to `python.log`. Same rotation policy.

### "Copy diagnostics bundle" action

Available from **Help → Copy Diagnostics Bundle**. Assembles:
- Last 7 days of `rust.log` and `python.log`
- `settings.json` with sensitive paths redacted
- System info (OS, arch, RAM, GPU if any)
- App version

Saves as a `.zip` file to a user-chosen location, or copies to clipboard (text summary for logs ≤ 100 KB).

### One-click anonymous error report

**Help → Report a Problem** opens a pre-filled GitHub Issue URL in the user's browser:

```
https://github.com/<org>/vocalboard/issues/new?template=bug_report.md
  &title=<url-encoded-title>
  &body=<url-encoded-body>
```

The body includes: app version, OS, and last 50 lines of both log files, HTML-encoded. The user reviews the content in their browser before clicking "Submit new issue" — nothing is sent automatically.

## Settings schema (Phase 1)

`settings.json`:
```json
{
  "version": 1,
  "model_dir": null,
  "model_paths": {
    "transcription": null,
    "vad": null,
    "forced_alignment": null,
    "enhancement": null,
    "sound_classification": null,
    "llm": null
  },
  "default_sample_rate": 48000,
  "speaker_merge_threshold": 0.71,
  "resampling_quality": "balanced",
  "gpu_enabled": false,
  "snapshot_idle_seconds": 30,
  "model_idle_unload_seconds": 300,
  "update_feed_url": null,
  "recent_projects": []
}
```

`model_dir` is the default model directory (download target + the directory scanned to enumerate available models); `model_paths` records the *selected* model path per role (see [data-model.md § App settings](data-model.md#app-settings)).

On read, if `version < current`, the settings migration code runs (hand-written per-version upgrade functions) before the settings are used. Unknown keys are preserved (forward-compat).
