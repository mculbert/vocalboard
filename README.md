# Vocalboard

An open-source, cross-platform desktop app for speech-forward audio editing, geared toward podcasters, audiobook narrators, and voiceover artists.

Vocalboard presents your recordings as an editable transcript. Import audio, get an automatically transcribed and speaker-labeled timeline, then edit by working with words — cut filler, mute noise, and rearrange speech — while the app handles the audio.

## Status

Early development. See [`design/`](design/) for the technical design document.

## Tech stack

- [Tauri 2](https://tauri.app/) + [Svelte 5](https://svelte.dev/) — desktop shell and UI
- Rust — project state, audio engine, playback
- Python sidecar — ML inference (transcription, diarization, enhancement, disfluency detection)

## Development

### Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| Rust | stable (1.95+) | [rustup.rs](https://rustup.rs/) |
| Node | 26 LTS | [nodejs.org](https://nodejs.org/) or `nvm install` |
| pnpm | 11+ | `npm install -g pnpm` |
| Python | 3.11 | managed by `uv` (see below) |
| uv | 0.11+ | [docs.astral.sh/uv](https://docs.astral.sh/uv/) |
| Go | 1.22+ | [go.dev/dl](https://go.dev/dl/) — required for `pnpm run docs:build` (Hugo module resolution) |

`rust-toolchain.toml` pins the Rust channel. `.nvmrc` pins the Node version. `.python-version` pins Python 3.11 for `uv`.

### Setup

```sh
# Install frontend dependencies
pnpm install

# Install Python dependencies (uv auto-downloads Python 3.11 if needed)
uv sync --project python/

# Run the app in development mode
pnpm tauri dev
```

### Tests

```sh
cargo test --workspace          # Rust (from src-tauri/)
pytest python/tests/            # Python
pnpm check && pnpm test         # Frontend
```

### Docs

```sh
pnpm run docs:build             # Build the Hugo site (output → docs/public/)
pnpm run docs:api               # Regenerate API docs from source
```

`pnpm run docs:api --python-only` (etc.) accepts `--rust-only`, `--python-only`, `--frontend-only` flags. Python API docs require `uv pip install -e 'python/[docs]'`; Rust API docs require the nightly toolchain (`rustup toolchain install nightly`).

## License

[MIT](LICENSE)
