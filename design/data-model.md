# Data Model

## Project file

A Vocalboard project is a single SQLite file with the extension `.vocalboard`. It is always in [WAL mode](https://www.sqlite.org/wal.html). An associated `<project>.vbdata/` directory lives alongside it and contains derived files (see [§ Derived files](#derived-files)).

The project is persisted with a **git-style content-addressed store plus an append-only journal**: turn data (and other immutable objects) are stored once per unique version, keyed by hash, and the timeline's evolution is recorded as a journal of deltas with periodic full snapshots. This mirrors the in-memory representation, where the timeline is an immutable tree with structural sharing (see [§ Implicit timeline tree](#implicit-timeline-tree)), so taking a snapshot is cheap.

### Schema version

Schema versioning uses SQLite's built-in `PRAGMA user_version`. The application maintains a list of numbered up-migration SQL scripts (e.g., `migrations/0001_initial.sql`, `migrations/0002_*.sql`). On open, Rust reads `PRAGMA user_version`, applies any pending migrations in order, and writes the new version back. Down-migrations are not supported; older apps that encounter a `user_version` higher than their own maximum refuse to open the file with a clear error message referencing `min_app_version`.

Because blobs in the `store` are serialized with **Bincode** (not self-describing — see [§ Serialization](#serialization)), schema migrations that change the shape of a serialized struct cannot rely on field names. Every blob is therefore prefixed with a one-byte **format tag**; a migration deserializes old blobs with the old struct definition (kept in the migration module) and re-serializes them with the new one.

### Schema DDL (Phase 1, user_version = 1)

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- ── Project metadata ─────────────────────────────────────────────────────────

CREATE TABLE project_meta (
    id               INTEGER PRIMARY KEY CHECK (id = 1), -- singleton row
    schema_version   INTEGER NOT NULL DEFAULT 1,
    min_app_version  TEXT    NOT NULL DEFAULT '0.1.0',   -- semver; older apps refuse this file
    sample_rate      INTEGER NOT NULL DEFAULT 48000,     -- locked at creation
    -- Monotonic ID counters: the NEXT value to assign. Persisted so that IDs
    -- stay unique across sessions. Track 0 is reserved for the labels track,
    -- so next_track_id starts at 1.
    next_track_id    INTEGER NOT NULL DEFAULT 1,
    next_speaker_id  INTEGER NOT NULL DEFAULT 1,
    next_turn_id     INTEGER NOT NULL DEFAULT 1,         -- persistent turn IDs
    created_at       TEXT    NOT NULL,                   -- ISO 8601
    updated_at       TEXT    NOT NULL
);

-- ── Content-addressed blob store ─────────────────────────────────────────────
-- Git-style object store. One row per unique blob, keyed by the 128-bit BLAKE3
-- hash of its Bincode-serialized payload. Turns, the global metadata object, and
-- large binaries (room tone PCM, speaker embeddings) all live here. A given
-- byte-identical payload is stored exactly once, no matter how many snapshots or
-- journal entries reference it — so an unchanged turn costs nothing across snapshots.

CREATE TABLE store (
    hash     BLOB PRIMARY KEY,   -- 16 bytes: BLAKE3 digest truncated to 128 bits
    payload  BLOB NOT NULL       -- format-tagged, Bincode-serialized object
);

-- ── Edit journal ─────────────────────────────────────────────────────────────
-- Append-only. Each row is one of: a batch of timeline deltas, a full timeline
-- snapshot, or a non-timeline (metadata) change. Replayed in id order on open,
-- starting from the most recent snapshot.

CREATE TABLE journal (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- -1 = non-timeline data: payload = 16-byte hash of a global metadata blob
    --  0 = timeline delta batch: payload = inline Bincode Vec<Delta>
    --  1 = full timeline snapshot: payload = 16-byte hash of a snapshot blob
    type         INTEGER NOT NULL CHECK (type IN (-1, 0, 1)),
    payload      BLOB    NOT NULL,
    command_id   INTEGER NOT NULL,   -- enum CODE for the command type that produced this
                                     -- row (e.g. CutWords, AlignTracks); NOT a counter
    applied_at   INTEGER NOT NULL    -- POSIX time in seconds, UTC
);

-- Fast "most recent snapshot" lookup and "deltas after snapshot" range scan.
CREATE INDEX journal_type_idx ON journal(type, id DESC);
```

That is the entire schema: **three tables**. Track metadata, speaker metadata, model-usage records, room tone, and embeddings are no longer relational tables; they live in the content-addressed `store` and are referenced from the journal (see [§ Non-timeline data](#non-timeline-data)).

> **Phase 1 has no persistent task queue.** The background-task queue lives only in memory. If the app is killed mid-task, the in-flight task is lost and the user re-triggers it on reopen; only *completed* operations have been written to the journal. Cross-session queue persistence ("Restore queued actions") is deferred to Phase 2, where it becomes a fourth table added by migration.

## Implicit timeline tree

The **implicit timeline tree** is the in-memory data structure that represents a single track's timeline. There is one tree per track (including track 0, the labels track), owned by the Rust engine. "Implicit" because a turn's absolute position on the project timeline is never stored — it is *implied* by the turn's order in the sequence plus the durations of everything before it.

Formally it is a **duration-weighted order statistic tree**: a balanced binary search tree (AVL) whose ordering key is timeline position and whose augmentation is the *summed duration* of the left subtree (rather than a plain node count, as in a classic order statistic tree). This gives O(log n) "which turn is playing at sample T?" and "what is the absolute start sample of this turn?" queries.

### Immutability and structural sharing

Nodes are immutable and shared via `Arc`. An edit **path-copies**: it allocates new nodes only along the path from the touched turn up to the root (plus any rotation nodes), and shares all untouched subtrees by cloning their `Arc`. Consequences:

- **Snapshot is O(1):** the "current state" is just the root `Arc`. Cloning it captures the entire timeline at that instant; the background snapshot writer serializes from that frozen root while the main thread keeps editing a new root.
- **Undo is cheap:** the previous root `Arc` is retained on the undo stack. Reverting is pointer assignment; memory cost is only the changed path.
- **No arena, no free-list, no index remapping.** Nodes are ordinary heap allocations managed by `Arc` reference counting. (This replaces the earlier arena-allocated design.)

```rust
// Immutable; shared via Arc. Editing path-copies to the root.
struct Node {
    turn:  Arc<Turn>,
    left:  Option<Arc<Node>>,
    right: Option<Arc<Node>>,

    // Augmentation — DERIVED, never serialized. Σ(turn_duration +
    // post_turn_silence) over the left subtree, in integer samples at the
    // project rate. Recomputed along the copied path on every edit/rotation.
    left_subtree_sum: i64,
    height:           u8,    // AVL balance factor
}
```

There are no parent pointers (a shared subtree may sit under many roots). Predecessor/successor — needed both for cursor movement and for computing a delta's `location` (the turn that *precedes* an edit) — are found by recording the search path from the root, an O(log n) operation.

### Turn payload (the unit stored in the blob store)

A `Turn` is the object that gets Bincode-serialized, hashed, and stored. It is **position-independent**: it contains nothing about where it sits on the project timeline, so an unchanged turn keeps the same hash regardless of edits elsewhere.

```rust
struct Turn {
    // Persistent ID, assigned from project_meta.next_turn_id at creation and
    // stable across every later edit of this turn. It IS part of the hashed
    // payload: this is what distinguishes two turns that happen to carry
    // identical data but occupy different points on the timeline (their hashes
    // differ because their ids differ), which keeps the hash-keyed adjacency
    // list and delta `location` references unambiguous.
    id:                u64,
    speaker_id:        Option<u64>,   // None = the "[None]" non-speech pseudo-speaker
    turn_duration:     i64,           // integer samples at project rate
    post_turn_silence: i64,           // gap to the next turn / spacing to next label
    words:             Vec<Word>,
    splices:           Vec<Splice>,   // embedded, not a table reference (see below)
}

struct Word {
    word_type:          WordType,   // Normal | Disfluency | Sound | Label | Section
    text:               String,
    // APPROXIMATE position in the SOURCE audio file (seconds), from the WhisperX
    // forced alignment. Used for display and to seed turn_offset_sample at creation.
    start_sec:          f64,
    end_sec:            f64,
    is_cut:             bool,
    is_muted:           bool,
    // Position and length WITHIN this turn, integer samples at the project rate.
    // At turn creation, turn_offset_sample is derived from the approximate WhisperX
    // start_sec and length_samples is 0. When a cut/mute computes a zero-crossing
    // for this word, both are updated to the precise word onset/offset (see
    // audio-pipeline.md § Zero-crossing and crossfade).
    turn_offset_sample: i64,
    length_samples:     i64,
}

// Embedded directly in the turn (resident in RAM during playback — no table
// join, no second fetch). Coordinates are source-relative or implied by order,
// never absolute project positions, so they don't break position independence.
// This per-turn vec IS the persisted EDL fragment for the turn: it is maintained
// as cut/mute edits subdivide splices, not rebuilt by a transcript pass at play
// time (see audio-pipeline.md § Edit Decision List).
struct Splice {
    kind:                 SpliceKind,  // Source | RoomTone | Silence
    length_samples:       i64,         // span of this splice (project rate)
    source_start_sample:  Option<i64>, // Source only: in source-file sample rate
    source_decode_offset: Option<i64>, // Source only: resampled samples to discard
    fade_in_samples:      i64,         // crossfade lengths, project rate
    fade_out_samples:     i64,
}
```

**Splices tile the turn.** A splice carries no stored offset: the splices of a turn
form a *gapless* sequence starting at turn offset 0, so a splice's position within the
turn is the running sum of the `length_samples` of all preceding splices. The tiling
spans `turn_duration + post_turn_silence` (the initial single splice covers the speech
plus the post-turn silence). If the splice-length total ever disagrees with
`turn_duration + post_turn_silence`, **the splices are authoritative**. The absolute
project position of a splice (or word) is computed at EDL-build time as
`turn_start_sample` (from the tree walk) `+` that running splice-offset sweep. Offsets
are accumulated naturally while iterating splices in order during playback/export, so
dropping the stored offset costs nothing on the hot path and removes a redundant field
that would otherwise have to stay consistent with the lengths (and thus with the turn's
content hash).

### Time representation

All durations and positions in the tree are **integer samples at the project sample rate**. Float timestamps (`start_sec`, `end_sec`) are stored only for the original source-file positions in `Word`; everything else is integer samples.

### Temporal query

To find the turn playing at project time `T` (in samples), starting at the root:

1. `offset = T - node.left_subtree_sum`
2. If `offset < 0`: recurse left.
3. If `offset < node.turn.turn_duration`: turn found; `offset` is the position within the turn.
4. Otherwise: `T -= node.left_subtree_sum + turn.turn_duration + turn.post_turn_silence`; recurse right.

The inverse query (absolute start sample of a given turn) accumulates `left_subtree_sum + turn_duration + post_turn_silence` contributions along the search path.

### Labels (track 0)

Timeline labels are **not** mixed into the speech tracks. They live in their own track, **track 0**, which is reserved for labels and always present. Track 0 has no audio source and no track-metadata entry; it exists only as a transcript (the `track_id = 0` tree) in snapshots and deltas.

A label is an ordinary turn on track 0:

- The label text is carried in the turn's **word list** (not in dedicated label fields). This lets a label hold rich/multi-word text and reuses the same rendering and editing machinery as speech turns.
- `post_turn_silence` encodes the **spacing between consecutive labels** on the timeline.
- The **section-header** flag (Phase 2: section labels export with the transcript) is encoded by **word type**: a plain label's words are `WordType::Label`; a section header's words are `WordType::Section`. No separate boolean is needed.

Because labels are just a track, every mechanism that operates on tracks — the delta model, snapshots, undo, navigation — applies to them uniformly.

### Sound events

Non-speech sound events are **not** labels and do not live on track 0. Per the requirements, each detected sound gets its **own bubble** on its speech track: it is a normal turn with `speaker_id = None` (rendered as "[None]"), a real `turn_duration`, and one `WordType::Sound` word (default text "[Sound]", or a YAMnet label when available). After import these behave like any other transcribed turn.

## Blob-and-tree persistence

### Hashing and serialization {#serialization}

Objects in `store` are serialized with **Bincode/Serde** and keyed by **BLAKE3 truncated to 128 bits** (16 bytes). 128 bits is ample: collision probability stays negligible well past any realistic turn count.

Content addressing requires **deterministic serialization** — the same logical object must always produce the same bytes. All hashed structs therefore use ordered collections only (`Vec`, `BTreeMap`); never `HashMap`, whose iteration order is unspecified.

### Turn blobs

When an edit produces a new version of a turn, Rust serializes the new `Turn`, computes its hash, and `INSERT OR IGNORE`s it into `store` (idempotent: a re-derived identical turn is a no-op). The turn is thereafter referred to **by hash** everywhere in the persistence layer.

### Snapshot blob

A full timeline snapshot is the **vec-flattened transcript of each track** — i.e., for every track (including track 0) the ordered list of its turns' hashes:

```rust
struct Snapshot {
    tracks: Vec<(u32 /* track_id */, Vec<Hash /* turns, in timeline order */>)>,
}
```

It is serialized, hashed, stored in `store`, and recorded by appending a `journal` row with `type = 1` whose `payload` is the snapshot blob's hash. (Storing the snapshot in `store` dedups byte-identical snapshots and keeps the journal row tiny.) The snapshot captures only ordering; the turn blobs it references are already in `store` from edit time.

### Deltas

Instead of logical commands with parameters, each user action records the **specific tree deltas** it produced as a single `type = 0` journal row (one inline `Vec<Delta>`). The row's `command_id` is an **enum code** naming the command type that produced it (e.g. `CutWords`, `AlignTracks`) — it is metadata for inspection/telemetry, not a per-action counter and not a grouping key. A delta is one of three operations, each identified by the turn that *precedes* the edit site:

| Delta | Params | Meaning |
|---|---|---|
| `InsertAfter` | `track_id`, `location`, `hash` | Insert the turn `hash` immediately after `location`. |
| `UpdateAfter` | `track_id`, `location`, `hash` | Replace the turn immediately after `location` with `hash`. |
| `DeleteAfter` | `track_id`, `location` | Remove the turn immediately after `location`. |

`location` is either `Start` (the head of the track) or the **hash of the preceding turn**. Because every turn's hash is unique within a track at any instant (the persistent ID in the payload guarantees it), `location` is unambiguous. Deltas within a batch are ordered and applied in sequence; since updating or inserting a turn changes hashes downstream, a later delta references the hashes as they exist at its point of application.

```rust
struct Delta {
    track_id: u32,
    op:       DeltaOp,            // InsertAfter | UpdateAfter | DeleteAfter
    location: Location,          // Start | Turn(Hash)
    hash:     Option<Hash>,      // None for DeleteAfter
}
```

A delta batch is serialized inline (Bincode `Vec<Delta>`) as the `payload` of a `type = 0` journal row.

### Load / replay

On project open, **per track**:

1. Read the most recent `type = 1` journal row; deserialize its snapshot blob from `store` into a working **adjacency list** — a map of edges `turn → next turn`, keyed by hash, with a `Start → first turn` edge.
2. Read all `type = 0` journal rows with `id` greater than that snapshot's `id`, in `id` order, and apply each delta to the adjacency list (insert/replace/remove the edge after `location`).
3. Walk the adjacency list from `Start`, producing the ordered hash sequence; fetch and deserialize each `Turn` blob from `store`.
4. Bulk-build the balanced duration-weighted order statistic tree from the ordered sequence (O(n)), computing `left_subtree_sum` and `height` as it builds.

Metadata is loaded separately (see below), not replayed.

## Non-timeline data

Mutable non-transcript state — project-level fields (e.g., project name; none exist in Phase 1 but the slot is reserved), track metadata, speaker metadata, model-usage records — lives in a single **global metadata object**:

```rust
struct Metadata {
    project:  ProjectMeta,           // project-scoped mutable state (first field)
    tracks:   Vec<TrackMeta>,        // sorted by id; track 0 (labels) is implicit, not listed
    speakers: Vec<SpeakerMeta>,      // sorted by id
}

// Project-scoped mutable metadata. NB: distinct from the `project_meta` SQLite
// singleton table (which holds sample_rate + the id counters). This struct lives
// inside the global metadata blob and segregates project-level fields so future
// ones can be added without touching the schema.
struct ProjectMeta {
    name:           Option<String>,
    aligned_groups: Vec<Vec<u32>>,   // sets of track_ids aligned together, e.g. [[1,2,4],[5,6]]
}

struct TrackMeta {
    id, name, source_type,                 // 'file' | 'recording' (Phase 3)
    source_path_relative, source_path_absolute,
    resampled_path:           Option<String>, // → resampled/<track>.flac; null until resample completes
    codec, source_sample_rate, source_channels,
    project_start_sample,
    original_length_samples,                  // full track length, in project samples
    cut_length_samples,                       // length after cuts, in project samples
    drift_ppm,
    room_tone_hash:           Option<Hash>,   // → store blob (resampled f32 PCM)
    room_tone_length_samples: Option<i64>,
    models_used:              Vec<ModelUse>,  // role/name/hash/used_at
    enhanced_path:            Option<String>,
    wet_dry_ratio:            f32,            // persisted per-track enhance mix; NOT set by enhance_track
    disfluencies_identified:  bool,
    created_at, updated_at,
}

struct SpeakerMeta {
    id, name, color_hint,
    embedding_hash: Option<Hash>,   // → store blob (normalized mean embedding, f32)
    track_ids:      Vec<u32>,       // replaces the old track_speakers join table
}
```

On any metadata change, Rust builds the new `Metadata`, ensures any referenced binary blobs are in `store`, serializes and hashes the `Metadata`, `INSERT OR IGNORE`s it into `store`, and appends a `journal` row with `type = -1` whose `payload` is the metadata blob's hash.

**Large binaries are separate content-addressed blobs.** Room tone PCM (potentially seconds of f32 samples) and speaker embeddings are stored as their own `store` blobs and referenced from `Metadata` *by hash*. A change like renaming a track therefore re-serializes only the small `Metadata` object — the unchanged room-tone/embedding blobs are reused by hash, not re-stored.

**Loading metadata** needs no replay: each `type = -1` entry is a *complete* metadata object, so the current metadata is simply `store[payload]` of the most recent `type = -1` journal row (or the empty default if none exists).

## Undo / redo

Undo/redo is delta-based; there is no "snapshot inverse." When a transformation is applied:

1. Compute the forward delta batch and its **inverse** batch (the inverse of `InsertAfter h` is `DeleteAfter` at the same location; of `DeleteAfter` is `InsertAfter` of the removed turn; of `UpdateAfter h_new` is `UpdateAfter h_old`). Metadata changes invert to "restore the previous metadata blob hash."
2. Push `(previous root Arc, forward rows, inverse rows)` onto the **undo stack** and clear the redo stack. One command produces at most one `type = 0` row plus, if it also touched metadata, one `type = -1` row; the undo-stack entry bundles both, so the *journal itself needs no grouping key* to define an undoable unit — the bundling lives in memory.

**Undo:** push the current state onto the redo stack, pop the undo stack, set the current root to the retained previous `Arc`, and **append the inverse rows to the journal** (as normal `type = 0`/`-1` rows tagged with the relevant `command_id` code). An undo is thus just another forward-recorded edit; replay on next open reproduces the post-undo state. **Redo** is symmetric. The undo/redo stacks themselves are in-memory only and do not survive reopen.

Because every transformation — including bulk operations like remove-track, align-tracks, and disfluency removal — now has a precise delta inverse, **full snapshots are no longer used as inverses**. Snapshots exist purely to bound journal-replay length; they are taken on the cadence in [§ Snapshot trigger](#snapshot-trigger), not per transformation.

### Snapshot trigger

A full timeline snapshot is written:

- After ~30 seconds of user inactivity (no edits) following an edit, on a background thread.
- Immediately after an "expensive op" (e.g., `import_speech_track`, `identify_disfluencies`, `remove_disfluencies`, `remove_sounds`, `align_tracks`) — to keep replay short after large journals.
- On explicit user request (`save_snapshot_now`).
- At app exit.

The background writer serializes from a frozen root `Arc` (captured by the structural-sharing clone) without blocking the main thread.

## Derived files

The `<project>.vbdata/` directory contains:

| Path pattern | Contents |
|---|---|
| `enhanced/<track_name>-enhanced.flac` | MP-SENet output for a given track |
| `resampled/<track_name>.flac` | Source audio resampled to the project rate (24-bit FLAC), written at import; regenerated on open if missing |

User-requested exports are written directly to the user-chosen path and are **not** cached in `.vbdata/` (export is infrequent and caching would just double disk use).

Path separator is always `/` in stored paths; Rust normalizes on read.

## Audio file resolution

On project open, for each `TrackMeta` with `source_type = 'file'`:

1. Resolve `source_path_relative` relative to the directory containing the `.vocalboard` file.
2. If the resolved path exists: use it.
3. Else if `source_path_absolute` exists on disk: use it; update `source_path_relative` (a metadata change → new `type = -1` journal row).
4. Else: the track has a missing source file. Collect all missing tracks and show the **Missing Files** dialog (once, after all tracks are checked). Per the requirements UX spec, the user may: remove the track, locate the file manually, or ignore (track is silently omitted from playback/export).

When a track is located by the user, both `source_path_relative` and `source_path_absolute` are updated and the track is fully loaded.

## App settings

App-level settings (not per-project) are stored via `tauri-plugin-store` in a `settings.json` file in the platform app config directory. The top-level object has a `version` integer field for forward-compatible migration. Settings include:

- `model_dir`: the app's default model directory — the download target for curated models and the directory scanned to enumerate available models in Settings → Models (default: platform app data dir)
- `model_paths`: the **selected** model per role — an object keyed by role (`transcription`, `vad`, `forced_alignment`, `enhancement`, `sound_classification`, `llm`), each value a path or `null`. A non-null value points either inside `model_dir` (a built-in/downloaded model) or to an external, user-supplied location; `null` means no model is selected for that role (allowed). Path semantics are role-specific: a directory for WhisperX/pyannote/MP-SENet/YAMnet, a single `.gguf` file for Gemma. The `vad` role is a **reserved, nullable slot**: it is carried for forward-compatibility but is unused in Phase 1 (WhisperX supplies its own internal VAD; a standalone Silero VAD flow is deferred).
- `speaker_merge_threshold`: cosine-similarity cutoff for merging a newly-imported speaker into an existing one (default 0.71; see [ml-pipeline.md § Embedding storage](ml-pipeline.md#embedding-storage))
- `default_sample_rate`: shown as default in new-project dialog (user can override)
- `gpu_enabled`: whether GPU acceleration has been installed and enabled
- `snapshot_idle_seconds`: autosave idle interval (default: 30)
- `model_idle_unload_seconds`: Python sidecar model unload timeout (default: 300)
- `resampling_quality`: `balanced` | `high` | `highest` (maps to rubato sinc parameters)
