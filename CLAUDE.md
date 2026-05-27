# Vocalboard

Open-source, cross-platform desktop app for speech-forward (transcript-based) audio
editing — podcasters, audiobooks, voiceover. Runs entirely locally. Tauri 2 (Rust
shell) + Svelte 5 webview + a long-running Python ML sidecar.

## Status
Design complete; pre-implementation. The source tree is not yet scaffolded —
**M0 (scaffolding & contracts) is next** (`design/phase1-m0.md`).

## Source of truth
- **`design/` is authoritative.** Start at `design/index.md`; per-area docs cover
  architecture, data-model, command-surface, audio-pipeline, ml-pipeline, frontend,
  ops, sequence-diagrams.
- **`requirements.md` is the original brainstorm and is NOT kept in sync** with later
  decisions — defer to `design/` on any conflict.
- Roadmap: `design/phase1.md` (milestones M0–M7).
- When a decision changes, **update the relevant `design/` doc** instead of letting
  docs drift, and keep cross-doc references consistent.
- When implementation leaves a deliberate shortcut or stub, **immediately update the
  affected downstream milestone in the relevant `design/phase*.md`**
  before closing the commit — don't rely on code comments alone to surface it later.

## Architectural invariants (do not violate)
- **Rust owns all state, persistence, and audio; Python is ML inference ONLY.** Signal
  processing (resampling, room-tone, sound *detection*, track alignment) stays in Rust.
  Only model inference (WhisperX, pyannote, MP-SENet, Gemma, YAMnet) crosses to Python.
- **The command surface is the only way the frontend mutates state** — named, versioned,
  JSON-schema-validated commands (`command-surface.md`); no raw scripts from the webview.
  The frontend never names ML models; Rust resolves them from settings `model_paths`.
- **Persistence = content-addressed blob store + append-only journal (3 SQLite tables)**
  with periodic snapshots; immutable `Arc` timeline tree with structural sharing. Blobs
  are Bincode + BLAKE3-128 with a format-tag byte; hashed structs use ordered
  collections only (`Vec`/`BTreeMap`, never `HashMap`) for deterministic serialization.
- **Any persisted-format change ships a migration + a round-trip test.** Touching the SQLite
  schema, blob format-tag byte, snapshot encoding, or `settings.json` requires a migration
  AND a test that loads a prior-format fixture — a format change without one is silent data loss.
- **All time is integer samples at the project sample rate** (set at create, locked).
  The only floats are approximate source-seconds on `Word`.
- **IPC = NDJSON over Tauri sidecar stdio**, routed by `request_id`; stdin = control
  (cancel), stdout = progress/log/result/error.
- **Local-first: nothing leaves the machine without explicit user action** — no telemetry or
  background network; redact file paths and PII in logs and the diagnostics bundle.
- **No allocation, locking, or blocking I/O on the cpal audio callback / real-time path**
  (pre-allocate; lock-free hand-off) — protects deterministic playback latency.

## Conventions
- Commands: `snake_case`, integer `version`, Draft-07 schemas; one command = one journal
  delta batch tagged with a `command_id` enum code (NOT a counter, NOT an undo grouping
  key). Undo is delta-based on an in-memory stack. Never change a command's schema in
  place — add fields compatibly or bump `version` and keep the old one handled.
- All UI strings go through Paraglide (i18n); backend errors travel as message keys.
- Rust: `cargo fmt` + `clippy -D warnings`; tests inline `#[cfg(test)]` + per-crate
  `tests/`. Frontend: Svelte 5 runes; Vitest colocated; Playwright in `e2e/` (not in CI).
  Python: 3.11 (pinned for ML-wheel + Nuitka compat); structlog.
- Bug fixes start with a failing regression test (fails before the fix, passes after).
- No `unwrap`/`expect`/`panic` in non-test Rust without a justifying comment (clippy-gated);
  `pub` items carry doc-comments (`#![warn(missing_docs)]` is a hard CI gate).
- Minimal comments (non-obvious "why" only); no speculative abstractions.
- **Full development norms live in `design/conventions.md`** (testing, error handling, a11y,
  i18n, docs, data integrity, supply chain) with each rule tagged enforced-in-CI vs reviewed.

## Build / test / run (once M0 scaffolds the workspace)
- Rust: `cargo build`, `cargo test --workspace` (from `src-tauri/`)
- Frontend: `pnpm check`, `pnpm test`, `pnpm build`
- Python: `pytest python/tests/`
- App: `pnpm tauri dev`

## Workflow
- Ignore `notes/` unless explicitly referenced.
- **GPG signing by branch type:**
  - `claude/*` working branches: commits are NOT signed — always use `--no-gpg-sign`.
  - `main` and all other development branches: commits ARE signed — never bypass with
    `--no-gpg-sign`. If signing fails, the gpg-agent may need unlocking — ask.
- **Merges from `claude/*` to main or any development branch are squash merges** unless
  otherwise specified.
- Don't push unless asked.
- **Pre-commit checklist:** before running `git commit`, confirm: (1) if this diff
  leaves any shortcut, stub, or deferred item, the relevant `design/phase*.md` is
  also staged with the downstream milestone updated.
