# Vocalboard — Technical Design Document

> Phase 1 (Minimum Viable Product) — with forward-looking notes for Phases 2–6.
> Last updated: 2026-05-24.

## System overview

Vocalboard is a cross-platform desktop audio editor built on Tauri 2 (Rust shell), Svelte 5 (webview UI), and a long-running Python sidecar that handles all machine-learning inference. The user works in a **transcript-first** interface: imported speech audio is transcribed, diarized, and presented as a timeline of speech-turn-level "bubbles." Edits (cuts, mutes, silence compression) operate on words and turns in the transcript; a Rust-native audio engine translates those edits into a real-time Edit Decision List (EDL) used for both preview playback and export. The project state is persisted in a local SQLite file using a git-style content-addressed blob store and an append-only delta journal with periodic snapshots.

## Design area index

| Area | File | What it covers |
|------|------|----------------|
| Architecture | [architecture.md](architecture.md) | Component diagram, process model, IPC protocol, security boundaries |
| Data model | [data-model.md](data-model.md) | SQLite schema, implicit timeline tree, blob-and-tree persistence, snapshot/replay, file resolution |
| Audio pipeline | [audio-pipeline.md](audio-pipeline.md) | Decoding, EDL, playback engine, room tone, track alignment, export |
| ML pipeline | [ml-pipeline.md](ml-pipeline.md) | Sidecar internals, model catalog, WhisperX/Gemma/MP-SENet pipelines |
| Frontend | [frontend.md](frontend.md) | Svelte 5 layout, state, virtualization, cursor model, accessibility, i18n |
| Operations | [ops.md](ops.md) | Build & packaging, CI, distribution, settings, logging, app data layout |
| Command surface | [command-surface.md](command-surface.md) | Inventory of named commands with JSON schemas |
| Sequence diagrams | [sequence-diagrams.md](sequence-diagrams.md) | Key cross-process flows (import, playback, snapshot, cancellation, etc.) |
| Conventions | [conventions.md](conventions.md) | Development norms: testing, error handling, a11y, i18n, docs, data integrity; enforced vs reviewed |

## Implementation plans

| Plan | File | What it covers |
|------|------|----------------|
| Phase 1 roadmap | [phase1.md](phase1.md) | Dependency-ordered build plan for Phase 1 (milestones M0–M7) |
| M0 action plan | [phase1-m0.md](phase1-m0.md) | Step-by-step scaffolding & contracts plan for the M0 milestone |

## Key architectural decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Rust vs Python scope | Rust owns state + sqlite + audio; Python is ML only | Keeps the hot path (playback, edit, UI) entirely in Rust; no round-trip latency for interactive ops |
| Python sidecar lifecycle | Long-running process, lazy model load, idle-unload | Avoids 5–30 s cold-start for repeated ML tasks while managing RAM on resource-constrained machines |
| IPC transport | Tauri stdio NDJSON with stdin control channel | Zero port-collision risk, Tauri-native lifetime management, simple to log and replay |
| Audio playback | Rust native: Symphonia + cpal + rubato | Deterministic latency; same engine for preview and export; no webview audio fragility |
| Audio decoding | Symphonia for audio; bundled ffmpeg for video/AAC fallback | Pure-Rust fast path for the common case; ffmpeg only loaded when needed |
| Project persistence | SQLite with PRAGMA user_version migrations + min_app_version | Industry-proven file format; versioned migrations; older apps refuse incompatible projects cleanly |
| Timeline persistence | Git-style content-addressed blob store + append-only delta journal, with periodic snapshots | An unchanged turn is stored once across all snapshots; mirrors the in-memory structural-sharing tree, so snapshots are cheap |
| Serialization | Bincode/Serde for stored blobs | Compact and fast for the many snapshots/blobs the journal produces; content-addressing already precludes eyeballing blobs, so JSON's inspectability buys nothing here |
| Undo / redo | Each edit records its tree delta + inverse on the undo stack and in the journal | O(1) undo; bulk ops (remove track, align, disfluency removal) invert precisely via deltas — no snapshot-as-inverse needed |
| Command surface | Named, versioned, JSON-schema-documented operations | Enables Phase 6 scripting/plugin hooks to call the same surface; enforces the "no raw scripts from frontend" security invariant |
| i18n | Paraglide-js (compile-time, typed) | Type-safe message functions; integrates cleanly with Svelte 5 runes; English-only v1 but trivially extensible |
| Bubble virtualization | Bubble-level windowing, all words rendered inside visible bubbles | Handles long transcripts without sacrificing per-word focus/selection/accessibility |
| Track schema | `source_type` column from day one | Phase 3 recording tracks become a sibling metadata table; no migration touches existing projects |
| Task queue | In-memory in Phase 1 | A killed app loses in-flight tasks (user re-triggers on reopen); only completed ops are journaled. Cross-session persistence ("Restore queued actions") is deferred to Phase 2 |

## Reading order for new contributors

1. **This file** — orientation
2. [architecture.md](architecture.md) — understand the three-process model and IPC
3. [data-model.md](data-model.md) — understand the SQLite schema and the implicit timeline tree
4. [command-surface.md](command-surface.md) — understand the named-operation contract before writing any code that crosses a process boundary
5. [conventions.md](conventions.md) — the development norms all code follows (testing, errors, a11y, i18n, docs, data integrity)
6. [audio-pipeline.md](audio-pipeline.md) or [ml-pipeline.md](ml-pipeline.md) — depending on your work area
7. [frontend.md](frontend.md) — UI layer
8. [sequence-diagrams.md](sequence-diagrams.md) — verify your mental model of the key flows
9. [ops.md](ops.md) — build, test, ship

## Glossary

**audio splice** — A record describing one contiguous segment of playable audio: either a range from a source file, a loop of the track's room tone, or silence. Splices are embedded directly in their turn (no separate table) and are resident in RAM during playback. They **tile the turn** contiguously, so a splice's position is the running sum of the preceding splices' lengths — no offset is stored. The per-turn splice list is the persisted EDL, maintained as edits are made. See [data-model.md § Turn payload](data-model.md#turn-payload-the-unit-stored-in-the-blob-store).

**blob store** — The content-addressed `store` table: a git-style object store mapping a 128-bit BLAKE3 hash to a Bincode-serialized payload (turns, the global metadata object, room tone, embeddings). A byte-identical payload is stored exactly once. See [data-model.md § Blob-and-tree persistence](data-model.md#blob-and-tree-persistence).

**bubble** — The visual representation of one speech turn (or one non-speech event) in the UI, rendered as a styled `<section>`. Multiple bubbles in the same time range appear in adjacent columns.

**command** — A named, versioned, JSON-schema-documented user action applied to the project state. Applying it produces a batch of tree deltas recorded as one journal row whose `command_id` is an **enum code for the command type** (not a counter); undo is delta-based and tracked on an in-memory stack, not a stored reverse command and not grouped by `command_id`. See [command-surface.md](command-surface.md).

**delta** — A single recorded edit to a track's turn sequence: `InsertAfter`, `UpdateAfter`, or `DeleteAfter`, located by the preceding turn (`Start` or a turn hash). Recorded per user action as one `type = 0` journal row; the unit of replay. (Undo bundles a command's rows on an in-memory stack, not via a journal grouping key.) See [data-model.md § Deltas](data-model.md#deltas).

**duration-weighted order statistic tree** — The formal data structure behind the implicit timeline tree: a balanced BST keyed by timeline position whose nodes are augmented with the *summed duration* of their left subtree (instead of a plain subtree node count), giving O(log n) position↔turn queries. See [data-model.md § Implicit timeline tree](data-model.md#implicit-timeline-tree).

**EDL** — Edit Decision List. Maintained **incrementally** as the per-turn splice lists (updated as cut/mute edits subdivide splices), not rebuilt by a transcript pass. The playback/export EDL concatenates those per-turn lists along the timeline and merges tracks. See [audio-pipeline.md § EDL](audio-pipeline.md#edit-decision-list).

**implicit timeline tree** — The in-memory tree structure (one per track) that represents turns and their relative timeline positions; formally a duration-weighted order statistic tree. "Implicit" because a turn's absolute position is never stored — it is implied by sequence order plus preceding durations. Immutable nodes shared via `Arc` (structural sharing); supports O(log n) temporal queries. See [data-model.md § Implicit timeline tree](data-model.md#implicit-timeline-tree).

**journal** — The append-only `journal` table recording the project's evolution: timeline delta batches (type 0), full snapshots (type 1), and non-timeline metadata changes (type -1). Replayed in order from the latest snapshot on open. See [data-model.md § Blob-and-tree persistence](data-model.md#blob-and-tree-persistence).

**label** — A timeline marker (e.g., a chapter boundary). Labels live on **track 0** (the reserved labels track) as ordinary turns whose text is carried in the word list; `post_turn_silence` sets the spacing between labels, and `WordType::Section` marks a section header. See [data-model.md § Labels (track 0)](data-model.md#labels-track-0).

**locus** — A cursor position. May be a word, a track-start marker, or (conceptually) the boundary between two elements. Not a range.

**persistent turn ID** — A stable integer assigned to a turn at creation (from `project_meta.next_turn_id`) and unchanged across edits. It is part of the turn's hashed payload, so it disambiguates turns with identical data at different timeline points and keeps turn hashes unique within a track. The cursor and command params refer to a turn by this ID. See [data-model.md § Turn payload](data-model.md#turn-payload-the-unit-stored-in-the-blob-store).

**project sample rate** — The integer sample rate (default 48 kHz; any integer allowed) at which all audio in the project is represented. Set at project creation; locked thereafter. All source audio is resampled to a per-track cache at import.

**room tone** — The ambient background noise of a recording environment. Detected at import; stored as a resampled segment. Used to fill gaps left by muted words.

**sidecar** — The long-running Python process that handles all ML inference. Managed by Tauri's sidecar facility.

**snapshot** — A Bincode serialization of each track's vec-flattened transcript (the ordered list of its turns' hashes), stored content-addressed in the blob store and recorded as a `type = 1` journal row. Bounds journal-replay length: the journal replays only the deltas after the latest snapshot. See [data-model.md § Snapshot blob](data-model.md#snapshot-blob).

**structural sharing** — The immutable-tree technique where an edit allocates new nodes only along the path to the root and shares all untouched subtrees via `Arc`. Makes in-memory snapshots and undo O(1) in pointer terms. See [data-model.md § Immutability and structural sharing](data-model.md#immutability-and-structural-sharing).

**turn** — One contiguous span of speech from a single speaker bounded by silence (also: a non-speech sound event, or a label on track 0). A node in the implicit timeline tree, carrying a persistent ID and an ordered list of words. Displayed as a bubble.

**VBDATA** — The `<project>.vbdata/` directory co-located with the project's `.vocalboard` sqlite file. Contains derived files: enhanced audio FLACs and the resampled-source cache.

**word** — A transcribed token with *approximate* source-audio start/end timestamps (from WhisperX), project-rate `turn_offset_sample` / `length_samples` (refined to the precise zero-crossing on cut/mute), a text label, and boolean cut/muted flags. Each word is its own `<span tabindex="-1">` in the UI.
