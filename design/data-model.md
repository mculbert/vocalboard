# Data Model

## Project file

A Vocalboard project is a single SQLite file with the extension `.vocalboard`. It is always in [WAL mode](https://www.sqlite.org/wal.html). An associated `<project>.vbdata/` directory lives alongside it and contains derived files (see [§ Derived files](#derived-files)).

The project is persisted with a **git-style content-addressed store plus an append-only journal**: turn data (and other immutable objects) are stored once per unique version, keyed by hash, and the timeline's evolution is recorded as a journal of deltas with periodic full snapshots. This mirrors the in-memory representation, where the timeline is an immutable tree with structural sharing (see [§ Implicit timeline tree](#implicit-timeline-tree)), so taking a snapshot is cheap.

### Schema version

Schema versioning uses SQLite's built-in `PRAGMA user_version`. The application maintains a list of numbered up-migration SQL scripts (e.g., `migrations/0001_initial.sql`, `migrations/0002_*.sql`). On open, Rust reads `PRAGMA user_version`, applies any pending migrations in order, and writes the new version back. Down-migrations are not supported; older apps that encounter a `user_version` higher than their own maximum refuse to open the file with a clear error message referencing `min_app_version`.

**Migration requires explicit user consent** (M6 onward). Running a migration is one-way: a file once opened under a newer schema can no longer be opened by an older app version. So when `open_project` detects pending migrations, the engine surfaces the prior and target `user_version` values to the frontend instead of migrating; the Welcome / Open flow shows a dialog with three choices: **Cancel**, **Open read-only** (mount the file without running migrations — the engine refuses every state-mutating command for the session), or **Migrate and open** (run the migrations and proceed). M1 ships `open_project` v1 with the migrate-and-open path only; the consent flow and the read-only mode land with the M6 open flow (see the `open_project` forward note in [command-surface.md](command-surface.md#open_project-v1)).

Because blobs in the `store` are serialized with **postcard** (not self-describing — see [§ Serialization](#serialization)), schema migrations that change the shape of a serialized struct cannot rely on field names. Every blob is therefore prefixed with a one-byte **format tag** whose low nibble encodes the format version (see [§ Serialization](#serialization) for the tag layout). **Lazy migration** keeps old-format blobs readable forever: per-version deserializers stay in the codebase indefinitely; blobs are re-serialized in the new format only when their content is genuinely edited. A read-only open therefore performs zero rewrite work, and a project that has been partially edited under a new format will contain a mix of old- and new-format blobs — mixed-version stores are normal and correct. A future opt-in **compact** operation (post-M1) is the escape valve for normalizing a mixed-version store on user request.

**Two independent version axes.** Format evolution moves along two separate axes: the **SQLite `user_version`** (the table DDL below, bumped by a numbered `migrations/000N_*.sql`) and the per-blob **format-tag nibble** (the postcard shape of each `mod v1` wire struct — turn, label, splice, word, snapshot, metadata, delta). They change independently: a blob-shape change does **not** touch `user_version`, and a DDL change does not touch the blob tags. Phase 1 is expected to ride at `user_version = 1` throughout (M2–M7 add no tables — timeline and metadata live in blobs; recording-track tables are Phase 3); the blob shapes, by contrast, *do* fill in over Phase 1 (real splices in M2, real words in M4/M5).

**Pre-1.0 the shapes are not yet frozen.** All of Phase 1 ships before any public release, so no project files exist outside internal testing. Until 1.0, a wire struct whose shape proves wrong MAY be revised **in place** within its `mod v1` — *without* adding a `mod v2`, bumping the format-tag nibble, or retaining the old deserializer. Each such revision MUST regenerate the pinned wire-byte/hash tests and any committed G1 round-trip fixtures (e.g. the project fixture exercised by `src-tauri/core/tests/fixture_roundtrip.rs`), and SHOULD raise `min_app_version` so stale dev/internal files written under the old shape are refused cleanly rather than silently mis-decoded by the revised reader ([conventions.md § G2](conventions.md#g-data--persistence-integrity)). The same disposability applies to the SQLite schema: pre-1.0, prefer editing `0001_initial.sql` in place over shipping a `0002_*.sql`. **At first release the v1 shapes (and the `user_version = 1` DDL) freeze.** From then on the lazy-migration mechanism above governs every change — a new `mod v2` with the old deserializer kept indefinitely and a `From<…V2>` upgrade, while committed fixtures are *kept* (a new vN fixture added alongside), never regenerated.

### Schema DDL (Phase 1, user_version = 1)

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- ── Project metadata ─────────────────────────────────────────────────────────

CREATE TABLE project (                                  -- singleton: project-level
    id               INTEGER PRIMARY KEY CHECK (id = 1), -- session/schema state
    schema_version   INTEGER NOT NULL DEFAULT 1,
    min_app_version  TEXT    NOT NULL DEFAULT '0.1.0',   -- semver; older apps refuse this file
    sample_rate      INTEGER NOT NULL DEFAULT 48000,     -- locked at creation
    -- Monotonic ID counters: the NEXT value to assign. Persisted so that IDs
    -- stay unique across sessions. Track 0 is reserved for the labels track,
    -- so next_track_id starts at 1.
    next_track_id    INTEGER NOT NULL DEFAULT 1,
    next_speaker_id  INTEGER NOT NULL DEFAULT 1,
    next_turn_id     INTEGER NOT NULL DEFAULT 1,         -- persistent turn IDs
    next_label_id    INTEGER NOT NULL DEFAULT 1,         -- persistent label IDs (track 0)
    created_at       TEXT    NOT NULL,                   -- ISO 8601
    updated_at       TEXT    NOT NULL
);

-- ── Content-addressed blob store ─────────────────────────────────────────────
-- Git-style object store. One row per unique blob, keyed by the 128-bit BLAKE3
-- hash of its postcard-serialized payload. Turns, the global metadata object, and
-- large binaries (room tone PCM, speaker embeddings) all live here. A given
-- byte-identical payload is stored exactly once, no matter how many snapshots or
-- journal entries reference it — so an unchanged turn costs nothing across snapshots.

CREATE TABLE store (
    hash     BLOB PRIMARY KEY,   -- 16 bytes: BLAKE3 digest truncated to 128 bits
    payload  BLOB NOT NULL       -- format-tagged, postcard-serialized object
);

-- ── Edit journal ─────────────────────────────────────────────────────────────
-- Append-only. Each row is one of: a batch of timeline deltas, a full timeline
-- snapshot, or a non-timeline (metadata) change. Replayed in id order on open,
-- starting from the most recent snapshot.

CREATE TABLE journal (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- -1 = non-timeline data: payload = 16-byte hash of a global metadata blob
    --  0 = timeline delta batch: payload = delta_version:u8 || postcard Vec<Delta>
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

The **implicit timeline tree** is the in-memory data structure that represents a single track's timeline. There is one tree per track, owned by the Rust engine. Speech tracks (`track_id > 0`) carry [`Turn`](#turn-payload-speech-tracks) elements; the labels track (`track_id = 0`) carries [`Label`](#label-payload-track-0) elements (see [§ Labels (track 0)](#labels-track-0)). The tree is **generic over its element type** via the [`Tilable`](#tilable-trait) trait — same balance and augmentation machinery, two element types. "Implicit" because an element's absolute position on the project timeline is never stored — it is *implied* by its order in the sequence plus the durations of everything before it.

Formally it is a **duration-weighted order statistic tree**: a balanced binary search tree (AVL) whose ordering key is timeline position and whose augmentation is the *summed duration* of the left subtree (rather than a plain node count, as in a classic order statistic tree). This gives O(log n) "which element is at sample T?" and "what is the absolute start sample of this element?" queries.

### Immutability and structural sharing

Nodes are immutable and shared via `Arc`. An edit **path-copies**: it allocates new nodes only along the path from the touched element up to the root (plus any rotation nodes), and shares all untouched subtrees by cloning their `Arc`. Consequences:

- **Snapshot is O(1):** the "current state" is just the root `Arc`. Cloning it captures the entire timeline at that instant; the background snapshot writer serializes from that frozen root while the main thread keeps editing a new root.
- **Undo is cheap:** the previous root `Arc` is retained on the undo stack. Reverting is pointer assignment; memory cost is only the changed path.
- **No arena, no free-list, no index remapping.** Nodes are ordinary heap allocations managed by `Arc` reference counting. (This replaces the earlier arena-allocated design.)

```rust
// Immutable; shared via Arc. Editing path-copies to the root. Generic over
// element type T: speech tracks instantiate as Node<Turn>, track 0 as Node<Label>.
struct Node<T: Tilable> {
    hash:    Hash,           // on-disk content hash; surfaced by iter() and element_at_sample()
    element: Arc<T>,
    left:    Option<Arc<Node<T>>>,
    right:   Option<Arc<Node<T>>>,

    // Augmentation — DERIVED, never serialized. Recomputed along the copied path
    // on every edit/rotation.
    left_subtree_sum:  i64, // Σ(element.total_duration()) over the left subtree
    total_subtree_sum: i64, // Σ(element.total_duration()) over the whole subtree (O(1) total_duration())
    height:            u8,  // AVL balance factor
}
```

#### Tilable trait

The tree's only assumption about its element type is that each element contributes a known duration to the timeline. That single requirement is captured in a one-method trait:

```rust
// Implemented by every type that can sit at a tree node.
trait Tilable {
    /// Total contribution of this element to the timeline, in project-rate samples.
    fn total_duration(&self) -> i64;
}

impl Tilable for Turn {
    fn total_duration(&self) -> i64 { self.turn_duration + self.post_turn_silence }
}

impl Tilable for Label {
    fn total_duration(&self) -> i64 { self.post_label_silence }
}
```

Anything else that varies between element kinds — how an in-element offset is interpreted (in-speech vs. post-turn silence vs. inter-label gap), what an edit means, what its V_N wire schema looks like — lives in element-specific code, not in the trait. See [§ Temporal query](#temporal-query) for how the trait is used.

There are no parent pointers (a shared subtree may sit under many roots). Predecessor/successor — needed both for cursor movement and for computing a delta's `location` (the element that *precedes* an edit) — are found by recording the search path from the root, an O(log n) operation.

### Turn payload (speech tracks)

A `Turn` is the unit stored in the blob store for speech tracks (`track_id > 0`). It is **position-independent**: it contains nothing about where it sits on the project timeline, so an unchanged turn keeps the same hash regardless of edits elsewhere.

```rust
struct Turn {
    // Persistent ID, assigned from project.next_turn_id at creation and
    // stable across every later edit of this turn. It IS part of the hashed
    // payload: this is what distinguishes two turns that happen to carry
    // identical data but occupy different points on the timeline (their hashes
    // differ because their ids differ), which keeps the hash-keyed adjacency
    // list and delta `location` references unambiguous.
    id:                u64,
    speaker_id:        Option<u64>,   // None = the "[None]" non-speech pseudo-speaker
    // A turn BEGINS at its first word's (refined) onset — the turn origin O =
    // words[0].source_onset_sample, fixed at import (the first word is the one word
    // refined eagerly, hence O never moves later). turn_duration + post_turn_silence
    // is the gap to the NEXT turn's origin and is exact at import (consecutive
    // first-word onsets); the speech-vs-silence SPLIT between them is approximate until
    // the last word is refined lazily. No pre-roll: turn-relative position 0 == O.
    turn_duration:     i64,           // speech extent, integer samples at project rate
    post_turn_silence: i64,           // gap to the next turn, project-rate samples
    words:             Vec<Word>,
    splices:           Vec<Splice>,   // embedded, not a table reference (see below)
}

struct Word {
    word_type:          WordType,   // Normal | Disfluency | Sound
    text:               String,
    // APPROXIMATE position in the SOURCE audio file (seconds), from the WhisperX
    // forced alignment. Used for display and to seed source_onset_sample at creation.
    start_sec:          f64,
    end_sec:            f64,
    is_cut:             bool,
    is_muted:           bool,
    // Zero-crossing-accurate word ONSET as an absolute sample offset in the
    // project-rate source/cache timeline (NOT turn-relative). It is STABLE across
    // every edit — cutting a word never moves it — so a cut word still records exactly
    // where to read its audio back on uncut (this is what lets uncut/unmute restore
    // the original Source splice without "guessing" a position). The word's
    // project-timeline position is DERIVED, never stored: it is the turn origin O plus
    // (source_onset_sample - O), or equivalently the splice-offset sweep (§ Splices
    // tile the turn). None = the zero-crossing has not been refined yet.
    //
    // Refinement is LAZY and per-SEAM: a word is refined only when it sits at a
    // cut/mute boundary, at edit time — EXCEPT the first word of every turn, which is
    // refined at IMPORT because the turn's origin O (and thus the turn boundaries; see
    // the Turn comments) depends on it. length_samples is the precise word length once
    // refined and an approximation ((end_sec - start_sec) * rate) before.
    source_onset_sample: Option<i64>,
    length_samples:      i64,
}

enum WordType {
    Normal,       // ordinary transcribed speech
    Disfluency,   // filler / hesitation ("um", "uh", …)
    Sound,        // non-speech sound event (e.g. a YAMnet-labelled sound)
}

// Embedded directly in the turn (resident in RAM during playback — no table
// join, no second fetch). Coordinates are source-relative or implied by order,
// never absolute project positions, so they don't break position independence.
// This per-turn vec IS the persisted EDL fragment for the turn: it is maintained
// as cut/mute edits subdivide splices, not rebuilt by a transcript pass at play
// time (see audio-pipeline.md § Edit Decision List).
struct Splice {
    length_samples:   i64,        // span of this splice (project rate)
    fade_in_samples:  i64,        // crossfade lengths, project rate
    fade_out_samples: i64,
    kind:             SpliceKind, // source-specific fields live in the variant
}

// Kind-specific data lives INSIDE the enum so source-only fields cannot be
// constructed nonsensically (no Option<i64> juggling for non-Source splices).
enum SpliceKind {
    Source {
        // Absolute sample offset into the project-rate source/cache timeline (the
        // resampled FLAC cache is at the project rate; the renderer seeks the cache to
        // this value). Same units as Word::source_onset_sample, so uncut/unmute can
        // hand a word's onset straight to a restored Source splice.
        source_start_sample: i64,
    },
    RoomTone,
    Silence,
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

### Label payload (track 0)

A `Label` is the unit stored in the blob store for the labels track (`track_id = 0`). It is **position-independent** for the same reason `Turn` is: its absolute timeline position is implicit from the order of labels on track 0 (see [§ Temporal query](#temporal-query)).

```rust
struct Label {
    // Persistent ID, assigned from project.next_label_id at creation and
    // stable across every later edit. Part of the hashed payload for the same
    // reason as Turn.id: it disambiguates two labels carrying identical data
    // but occupying different timeline positions.
    id:                  u64,
    text:                String,
    kind:                LabelKind,
    // Gap to the next label on track 0, in project-rate samples. Labels are
    // 0-length points: the entire timeline span owned by a label is the gap
    // BETWEEN it and the next label. The first label is anchored at sample 0;
    // its post_label_silence is the gap to the second label. The total span
    // of track 0 = Σ(post_label_silence) should equal the longest speech track.
    post_label_silence:  i64,
}

enum LabelKind {
    Plain,        // ordinary timeline marker
    Section,      // section header (exports with the transcript in Phase 2)
}
```

Labels have **no splices and no audio fields**. Track 0 has no audio source — labels are pure timeline annotations. The edit operations on labels (rename / move / promote-to-section / delete) are distinct from word-level edits (cut / mute / re-align), so the two types share no command surface.

### Time representation

All durations and positions in the tree are **integer samples at the project sample rate**. Float timestamps (`start_sec`, `end_sec`) are stored only for the *approximate* original source-file positions in `Word`; everything else is integer samples. `Word::source_onset_sample` is an `Option<i64>` — `None` until its zero-crossing is refined (lazy; see above) — but is integer samples when present.

Sample fields are nominally **non-negative**; the structs use `i64` rather than `u64` so the temporal-query math (`T - left_subtree_sum`, predecessor/successor accumulation) can use natural signed arithmetic — the ≥0 invariant is enforced at command-schema boundaries (`"minimum": 0` on sample params) and constructor `debug_assert!`s, not in the type system. One bit of headroom at 48 kHz still covers ~6 million years of sample positions; the practical loss is zero.

### Temporal query

The element-finding algorithm is identical for Turn and Label trees — both use `Tilable::total_duration()` as the per-element advance step. To find the element at project time `T` (in samples), starting at the root:

1. `offset = T - node.left_subtree_sum`
2. If `offset < 0`: recurse left.
3. If `offset < node.element.total_duration()`: element found; `offset` is the in-element offset (interpretation per element kind, below).
4. Otherwise: `T -= node.left_subtree_sum + node.element.total_duration()`; recurse right.

Renderers obtain each element's start sample by iterating the tree once and accumulating `total_duration()` as they go (`iter()` yields it natively). The tree does not provide a hash-keyed inverse query — the implicit ordering is by timeline position, not by hash, so any such query would be O(n) with no in-engine consumer.

**In-element offset interpretation, per kind:**

- **`Turn`:** if `offset < turn_duration`, the cursor is inside the speech span (at position `offset` within the turn); otherwise it is inside the post-turn silence gap (at position `offset - turn_duration` into the gap). This is what distinguishes "clicked on a word" from "clicked in the gap between turns" in the UI.
- **`Label`:** `turn_duration` is implicitly 0, so the cursor is always inside the inter-label gap — the only meaningful answer is "the label whose interval `[start, start + post_label_silence)` covers `T`."

**Precondition — every reachable element has `total_duration() ≥ 1`.** Step 3's `offset < total_duration()` test can never hold when `total_duration()` is 0, so a zero-duration element is silently *skipped* by the query (its `[start, start)` interval is empty) and has fuzzy insert-boundary semantics. For turns this holds automatically — a turn has a real `turn_duration ≥ 1`, or post-turn silence spanning a gap. For **labels** it means **`post_label_silence ≥ 1`: adjacent labels must be separated by at least one sample**, so each label owns a non-empty `[start, start + post_label_silence)` interval the cursor can land in and cursor movement between labels is well-defined (two coincident labels would otherwise be indistinguishable by position). M1 never exercises this (synthetic tests use non-zero durations, and the ≥1-turn invariant assumes real duration); it becomes load-bearing when label spacing becomes user-controlled and **must be enforced at the label command-schema boundary** (the M5 label edits — `EditLabel` and siblings — plus any import path that emits labels) with a `"minimum": 1` on `post_label_silence`.

### Labels (track 0)

Timeline labels live in their own track, **track 0**, which is reserved for labels and always present. Track 0 has no audio source and no track-metadata entry; it exists only as a transcript (the `track_id = 0` tree) in snapshots and deltas, populated by [`Label`](#label-payload-track-0) blobs rather than `Turn` blobs.

Labels are **not** turns. The earlier design unified the two so they could share one tree type and one set of edits, but track 0 having no audio means turn-specific machinery (`turn_duration`, `splices`, `is_cut` / `is_muted`, word zero-crossings) is meaningless for labels. Splitting them is the cleaner shape: `Label` carries exactly its four meaningful fields (`id`, `text`, `kind`, `post_label_silence`), and `WordType` stops carrying the `Label` / `Section` variants that only applied to label "turns."

The two element types still share **timeline machinery**: the implicit tree is generic over [`Tilable`](#tilable-trait), so the AVL balance, augmentation, and temporal query are written once and instantiated for both. They share **persistence machinery**: snapshots and deltas reference 16-byte hashes regardless of what kind of blob they point to, so the [`Snapshot`](#snapshot-blob) struct and [`Delta`](#deltas) row are unchanged. The only kind-aware seam is the per-track-id load dispatch (`track_id == 0 ⇒ load_label`, else `load_turn`) called when [§ Load / replay](#load--replay) walks the adjacency list to fetch element blobs.

### Sound events

Non-speech sound events are **not** labels and do not live on track 0. Per the requirements, each detected sound gets its **own bubble** on its speech track: it is a normal turn with `speaker_id = None` (rendered as "[None]"), a real `turn_duration`, and one `WordType::Sound` word (default text "[Sound]", or a YAMnet label when available). After import these behave like any other transcribed turn.

## Blob-and-tree persistence

### Hashing and serialization {#serialization}

Objects in `store` are serialized with **postcard/Serde** and keyed by **BLAKE3 truncated to 128 bits** (16 bytes). 128 bits is ample: collision probability stays negligible well past any realistic turn count.

Content addressing requires **deterministic serialization** — the same logical object must always produce the same bytes. All hashed structs therefore use ordered collections only (`Vec`, `BTreeMap`); never `HashMap`, whose iteration order is unspecified.

Every blob in `store` is prefixed with a one-byte **format tag** that encodes both the object kind (high nibble) and the format version (low nibble): `tag = (kind << 4) | version`. The assigned kind codes are: Turn = `0x1`, Metadata = `0x2`, Snapshot = `0x3`, RoomTone = `0x4`, Embedding = `0x5`, Label = `0x6`. This gives 16 version slots per kind (versions 0–15); the two-byte tag extension is the documented escape path if either ceiling is reached. For example, a Turn at format version 1 carries tag byte `0x11`, and a Label V1 carries `0x61`. The hash covers the **full tagged bytes** (tag ++ postcard payload), so two blobs with identical postcard content but different tags produce different hashes — ensuring blobs of different kinds or versions are always distinguished.

### Turn and Label blobs

When an edit produces a new version of a `Turn` (or `Label`, on track 0), Rust serializes the new element, computes its tagged-bytes hash, and `INSERT OR IGNORE`s it into `store` (idempotent: a re-derived identical element is a no-op). The element is thereafter referred to **by hash** everywhere in the persistence layer. The `Kind::Turn` / `Kind::Label` tag byte distinguishes the two; the typed load dispatch (`load_turn` / `load_label`) is selected by `track_id` at load time, with the tag-byte mismatch surfaced as an error if the on-disk kind ever disagrees.

### Write path

A user-facing edit produces three outputs that are threaded together in a single SQLite transaction on the **main thread**:

1. `let (hash, bytes) = store_turn(&new_turn)` (or `store_label`) — the one and only serialize + hash for the edit (`postcard::to_stdvec` + `blake3::hash`, sub-millisecond for typical element sizes).
2. `store::put(tx, &hash, &bytes)` — write the new payload blob (`INSERT OR IGNORE`; no-op if a byte-identical blob already exists).
3. `journal::append(tx, type = 0, command_id, payload)` — append the delta row, whose `payload` is the postcard-serialized `Vec<Delta>` (which carries the hash, not the bytes).

After the transaction commits, the in-memory tree is updated (`tree.update_at(sample, hash, Arc::new(new_turn))` or its `insert_at` / `delete_at` siblings) and the undo entry is pushed. Metadata changes (see [§ Non-timeline data](#non-timeline-data)) follow the same pattern with a `type = -1` row.

Two consequences are worth pinning down because they are easy to mis-model otherwise:

- **Unchanged elements are never re-serialized.** Path-copy mutation (see [§ Immutability and structural sharing](#immutability-and-structural-sharing)) allocates new `Node`s along the root→edit path, but each carries an existing hash and Arc-shares its element with the prior tree. `store::put` fires once per *new* element produced by the edit — usually exactly one, occasionally a small handful for split/merge — never per surviving turn.
- **Journal writes are synchronous; only snapshot writes are background.** The transaction above is small (one row + one blob, both typically ≲ a few KB) and runs entirely on the main thread. The background snapshot writer (see [§ Snapshot trigger](#snapshot-trigger)) is for the much larger periodic [`Snapshot`](#snapshot-blob) blob, and it **serializes only the snapshot itself** (a `Vec<(track_id, Vec<Hash>)>`) — not any element payloads, which are already in `store` from edit time.

### Batched (multi-element) edits

A single command may touch many elements — e.g. disfluency removal rewrites every turn that contained a disfluency. The command computes each affected element's new content as a **pure function of its *original* content and the command parameters**, never of the element's timeline position. This is possible precisely because [`Turn`](#turn-payload-speech-tracks) / [`Label`](#label-payload-track-0) are position-independent: their content (and hash) does not depend on where they sit, so a shift elsewhere on the timeline leaves an unedited element's bytes and hash untouched. The command therefore never tracks how positions evolve as the batch is applied.

The command submits the edit as a batch of position-addressed operations with **every position expressed against the original (pre-batch) tree**. The engine applies the batch **in descending sample order** (highest position first): every already-applied operation then lies strictly to the right of the current one, so it cannot move the current operation's coordinate, and original-tree positions stay valid through the whole batch with no re-resolution. Forward deltas are recorded in application order, so journal replay — which re-applies them in that order — is correct by construction. The inverse batch is built from each operation's [delta inverse](#undo--redo) (*same location, dual op-kind*) with the batch **reversed**. Submission order to the engine is not significant (the engine sorts); a batch addresses distinct positions.

### Snapshot blob

A full timeline snapshot is the **vec-flattened transcript of each track** — i.e., for every track (including track 0) the ordered list of its elements' hashes:

```rust
struct Snapshot {
    // Per-track ordered hashes. For track_id == 0 the hashes point to Label
    // blobs; for track_id > 0 they point to Turn blobs. The snapshot itself
    // is kind-agnostic — the loader picks load_turn / load_label based on
    // track_id at fetch time.
    tracks: Vec<(u32 /* track_id */, Vec<Hash /* elements, in timeline order */>)>,
}
```

It is serialized, hashed, stored in `store`, and recorded by appending a `journal` row with `type = 1` whose `payload` is the snapshot blob's hash. (Storing the snapshot in `store` dedups byte-identical snapshots and keeps the journal row tiny.) The snapshot captures only ordering; the element blobs it references are already in `store` from edit time.

### Deltas

Instead of logical commands with parameters, each user action records the **specific tree deltas** it produced as a single `type = 0` journal row (one inline `Vec<Delta>`). The row's `command_id` is an **enum code** naming the command type that produced it (e.g. `CutWords`, `AlignTracks`) — it is metadata for inspection/telemetry, not a per-action counter and not a grouping key. A delta is one of three operations, each identified by the element that *precedes* the edit site:

| Delta | Params | Meaning |
|---|---|---|
| `InsertAfter` | `track_id`, `location`, `hash` | Insert the element `hash` immediately after `location`. |
| `UpdateAfter` | `track_id`, `location`, `hash` | Replace the element immediately after `location` with `hash`. |
| `DeleteAfter` | `track_id`, `location` | Remove the element immediately after `location`. |

`location` is either `Start` (the head of the track) or `After(hash)` — the **hash of the preceding element** (a Turn on speech tracks, a Label on track 0). Because every element's hash is unique within a track at any instant (the persistent ID in the payload guarantees it), `location` is unambiguous. Deltas within a batch are ordered and applied in sequence; since updating or inserting an element changes hashes downstream, a later delta references the hashes as they exist at its point of application. The delta itself doesn't name the element kind — that is determined by `track_id` and dispatched at load time.

```rust
struct Delta {
    track_id: u32,
    op:       DeltaOp,           // InsertAfter | UpdateAfter | DeleteAfter
    location: Location,          // Start | After(Hash)
    hash:     Option<Hash>,      // None for DeleteAfter
}
```

A delta batch is stored as the `payload` of a `type = 0` journal row: a leading `delta_version: u8` byte (M1 writes `0x01`) followed by the postcard-serialized `Vec<Delta>`. `type = -1` and `type = 1` payloads are 16-byte hash pointers into the tagged store and carry no extra version byte — the store blob's own format tag carries the version.

### Load / replay

On project open, **per track**:

1. Read the most recent `type = 1` journal row; deserialize its snapshot blob from `store` into a working **adjacency list** — a map of edges `element → next element`, keyed by hash, with a `Start → first element` edge.
2. Read all `type = 0` journal rows with `id` greater than that snapshot's `id`, in `id` order, and apply each delta to the adjacency list (insert/replace/remove the edge after `location`).
3. Walk the adjacency list from `Start`, producing the ordered hash sequence; fetch and deserialize each element blob from `store` — `load_label` for `track_id == 0`, `load_turn` otherwise.
4. Bulk-build the balanced duration-weighted order statistic tree from the ordered sequence (O(n)) — instantiated as `Node<Label>` for track 0, `Node<Turn>` elsewhere — computing `left_subtree_sum` and `height` as it builds.

Metadata is loaded separately (see below), not replayed.

**Corruption handling is asymmetric (M1).** A `type = 0` / `type = 1` row that fails to decode during replay is *recoverable*: the engine rolls the timeline back to the most recent intact snapshot, drops the undecodable tail, and surfaces the rollback through `OpenOutcome.recovery` (the caller warns the user). When this rollback fires, **metadata is loaded at the snapshot's journal `id`, not the latest `type = -1` row**, so both streams stay reconciled at the same `as_of` (a metadata write made *after* the snapshot is dropped along with the timeline tail it belonged to). A corrupt `type = -1` *metadata* blob, by contrast, is currently **fatal** — the project will not open. This asymmetry is a deliberate M1 limitation: the recovery narrative is timeline-focused, and metadata corruption is both rarer and harder to roll back usefully (a single self-contained blob, no replay chain to truncate). A general **recovery-mode** open path — iteratively backtracking the journal to the last loadable point, or surfacing the future history browser / [Time Machine](#track-reconciliation-trees--metadata) so the user picks a recovery point — is deferred until that browser exists (post-Phase-1; Phase 1 M5 ships only the read-only `command_id`-aware history *view*, not a recovery-point picker).

### Track reconciliation (trees ↔ metadata)

The transcript trees (above) and the track metadata (`Vec<TrackMeta>`, [below](#non-timeline-data)) are reconstructed from **two independent journal streams** — `type = 0` / `type = 1` rows for trees, `type = -1` rows for metadata — and nothing in the append-only log ties them together row-for-row. After both have been loaded (at the **same `as_of`**), the engine reconciles them so the in-memory `PerTrackTrees` matches the authoritative track set. **Metadata is the source of truth for which speech tracks exist;** a transcript tree is subordinate.

For every **speech track (id ≥ 1)**:

- **Tree present, no `TrackMeta`** → the track was deleted (or an add-track was undone); **silently discard the tree.** This is the *expected* outcome of `remove_track`, not an error.
- **`TrackMeta` present, tree missing or empty** → corruption (a listed track must have ≥ 1 turn — see the invariant below). Surface it through the recoverable-open channel (`OpenOutcome.recovery`), drop/flag the track, and warn — do **not** refuse the whole project for one bad track.

The **label track (track 0) is exempt.** It has no `TrackMeta` by design (it is implicit — see [§ Non-timeline data](#non-timeline-data)), so it is never reconciled against metadata and is never discarded. The label track always conceptually exists; an empty (or absent) labels tree simply means "no labels yet."

**Adding a speech track is an atomic combined edit.** The track's `TrackMeta` and its initial transcript (a complete turn delta batch) are committed in the **same `apply_batch` transaction** — there is no intermediate state where a speech track exists without at least one turn. `remove_track` is the complement: metadata-only (no delta batch emitted), with the orphaned transcript tree reconciled away at load — see below.

**Why reconcile at load rather than purge the tree at delete time.** `remove_track` removes the track's `TrackMeta`; it does **not** emit deltas deleting the track's turns, because the tree carries no independent authority. The orphaned tree is harmless journal-tail garbage: the next snapshot writes only the (reconciled) live `current.trees`, so it is not reloaded, and M5+ compaction prunes the dead `type = 0` / `type = 1` rows. This removes any need to make `remove_track` (or undo-of-add-track) a *snapshot-immediately* operation — a crash before the snapshot finishes leaves a leaked tree that the very next open discards, not a corrupt project. It is also what makes the in-memory state and a journal round-trip agree after **undo-of-add-track**: replay re-creates the track's now-empty tree, reconciliation discards it (metadata no longer lists it), and the result matches the in-memory `current` that undo rolled back to.

**Load-bearing invariant — every speech track in `tracks` has ≥ 1 turn.** A track of pure silence is one turn whose `post_turn_silence` spans the gap; cutting is non-destructive (cut splices are dropped from playback but the turns remain), so editing never empties a track. The only way to reach a zero-turn track is importing a zero-length audio file, which import refuses. This invariant is what lets "`TrackMeta` with an empty/missing tree" be treated as corruption rather than a legal state.

> Two adjacent consistency concerns the reconciliation does **not** cover, owned by the `remove_track` command (M5): (1) it must scrub the deleted id from `ProjectMeta.aligned_groups` and any `SpeakerMeta.track_ids` in the **same atomic** `Metadata` write — reconciliation only trims trees, not dangling metadata references; (2) the trees and metadata must be reconciled at the **same `as_of`**, or the two streams' track sets will be temporally inconsistent — already exercised by journal-tail recovery (above), which loads metadata at the rolled-back snapshot's `id` rather than the latest row, and relevant again once Time Machine can open at an arbitrary past point.

**Status:** specified here. As of **M1 Step 11d**, `apply_batch` **can** journal a metadata change alongside a delta batch in the same transaction — the producer capability for combined tree+metadata edits (e.g. `add_track`) is in place. What remains **M5** is the track *commands* (`add_track`, `remove_track`), the reconciliation guard itself, and their round-trip fixtures. Until the guard lands, M1's synthetic metadata tests avoid orphaning trees (e.g. they change `ProjectMeta.name` rather than dropping a `TrackMeta`). When the guard lands, speech-track tests must also populate `TrackMeta` (a speech tree with no `TrackMeta` would otherwise be discarded as an orphan), and a `remove_track` round-trip fixture (metadata drops the track; the tree persists in the journal; reopen discards it) must be added. This is a replay-*semantics* change, not a persisted-format change, so it needs a round-trip test but no migration.

## Non-timeline data

Mutable non-transcript state — project-level fields (e.g., project name; none exist in Phase 1 but the slot is reserved), track metadata, speaker metadata, model-usage records — lives in a single **global metadata object**:

```rust
struct Metadata {
    project:  ProjectMeta,           // project-scoped mutable state (first field)
    tracks:   Vec<TrackMeta>,        // sorted by id; track 0 (labels) is implicit, not listed
    speakers: Vec<SpeakerMeta>,      // sorted by id
}

// Project-scoped mutable metadata. NB: distinct from the `project` SQLite
// singleton table (which holds sample_rate + the id counters). This struct lives
// inside the global metadata blob and segregates project-level fields so future
// ones can be added without touching the schema. The parallel ProjectMeta /
// TrackMeta / SpeakerMeta naming is deliberate; the SQLite singleton is `project`.
struct ProjectMeta {
    name:           Option<String>,
    aligned_groups: Vec<Vec<u32>>,   // sets of track_ids aligned together, e.g. [[1,2,4],[5,6]];
                                      // canonical order: each group ascending, groups by first id
}

struct TrackMeta {
    id, name, source_type,                 // 'file' | 'recording' (Phase 3)
    source_path_relative, source_path_absolute,
    codec, source_sample_rate, source_channels,
    project_start_sample,
    original_length_samples,               // full track length, in project samples
    cut_length_samples,                    // length after cuts, in project samples
    drift_ppm,
    room_tone_hash:          Option<Hash>, // → store blob (RoomTone PCM, content-addressed)
    models_used:             ModelUse,     // per-role model identifier (see below)
    wet_dry_ratio:           f32,          // persisted per-track enhance mix; NOT set by enhance_track
    disfluencies_identified: bool,
    created_at, updated_at,
}
// Derived (not persisted): resampled path = resampled/<track_id>.flac (always derivable from
// track_id). Room-tone sample length = decoded from the store blob via room_tone_hash.
// Enhanced audio path = resolved at export time from settings, not stored in metadata.

// The model applied to a track per role. The role set is fixed (it mirrors the
// settings `model_paths` roles) and each model is applied to a track at most
// once, so this is a flat struct of optional model identifiers — not a list and
// not timestamped. `None` means that role's model was never run on the track.
struct ModelUse {
    transcription:        Option<String>,   // WhisperX
    vad:                  Option<String>,   // reserved; unused in Phase 1
    forced_alignment:     Option<String>,   // WhisperX alignment model
    enhancement:          Option<String>,   // MP-SENet
    sound_classification: Option<String>,   // YAMnet
    llm:                  Option<String>,   // Gemma
}

struct SpeakerMeta {
    id, name, color_hint,
    embedding_hash: Option<Hash>,   // → store blob (normalized mean embedding, f32)
    track_ids:      Vec<u32>,       // replaces the old track_speakers join table; ascending
}
```

On any metadata change, Rust builds the new `Metadata`, ensures any referenced binary blobs are in `store`, serializes and hashes the `Metadata`, `INSERT OR IGNORE`s it into `store`, and appends a `journal` row with `type = -1` whose `payload` is the metadata blob's hash.

**Large binaries are separate content-addressed blobs.** Room tone PCM (potentially seconds of f32 samples) and speaker embeddings are stored as their own `store` blobs and referenced from `Metadata` *by hash*. A change like renaming a track therefore re-serializes only the small `Metadata` object — the unchanged room-tone/embedding blobs are reused by hash, not re-stored.

**Loading metadata** needs no replay: each `type = -1` entry is a *complete* metadata object, so the current metadata is simply `store[payload]` of the most recent `type = -1` journal row (or the empty default if none exists).

## Undo / redo

Undo/redo is delta-based; there is no "snapshot inverse." When a transformation is applied:

1. Compute the forward delta batch and its **inverse** batch (the inverse of `InsertAfter h` is `DeleteAfter` at the same location; of `DeleteAfter` is `InsertAfter` of the removed element; of `UpdateAfter h_new` is `UpdateAfter h_old`). Metadata changes invert to "restore the previous metadata blob hash."
2. Push an entry bundling the **before and after project-state snapshots** together with the forward and inverse journal effects onto the **undo stack**, and clear the redo stack. The undoable state — the per-track timeline trees *and* the non-timeline [metadata](#non-timeline-data) — is held as a single immutable value behind an `Arc`, so each snapshot is one `Arc` clone (structural sharing makes it O(1) and cheap in memory: trees share subtrees, and metadata is a small struct with large binaries referenced by hash). One command produces at most one `type = 0` delta row plus, if it also touched metadata, one `type = -1` row; the entry bundles both effects, so the *journal itself needs no grouping key* to define an undoable unit — the bundling lives in memory. The undo stack is bounded by a configurable depth (the `undo_history_limit` app setting, default 50); the oldest entry is evicted when the limit is exceeded.

**Undo:** pop the undo stack, swap the current state to the retained **before** snapshot (one `Arc` assignment — timeline and metadata revert together), and **append the inverse effect to the journal in a single transaction** (a `type = 0` and/or `type = -1` row tagged with the relevant `command_id` code), then push the entry onto the redo stack. An undo is thus just another forward-recorded edit; replay on next open reproduces the post-undo state. **Redo** is symmetric (swap to the **after** snapshot, append the forward effect). The undo/redo stacks themselves are in-memory only and do not survive reopen.

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
| `enhanced/<track_id>-enhanced.flac` | MP-SENet output for a given track |
| `resampled/<track_id>.flac` | Source audio resampled to the project rate (24-bit FLAC), written at import; regenerated on open if missing |

**Derived-cache files are keyed by the stable `TrackMeta.id` (the `u32` from `next_track_id`), not the track name.** The id is unique by construction and never reassigned, so the cache survives renames without orphaning and needs no filesystem-name sanitization (distinct names can otherwise sanitize to a colliding string). Track names *are* enforced unique at the command layer (`track_name_duplicate`), but they are mutable and not filesystem-safe, so they are unsuitable as a cache key. Users who need to locate a cache file on disk find the id in the track info dialog.

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
- `room_tone_rms_ceiling`: absolute RMS ceiling (linear amplitude, default 0.0316 ≈ −30 dBFS) above which audio is never treated as room tone (see [audio-pipeline.md § Room tone detection](audio-pipeline.md#room-tone-detection))
- `room_tone_quiet_percentile`: percentile (0–100) of 100 ms block RMS forming the adaptive quiet threshold (default 5); the effective threshold is `min(room_tone_rms_ceiling, this percentile of block RMS)`
- `splice_search_window_ms`: outward search radius (ms) for cut/mute boundary refinement — the zero-crossing search scans up to this far before a word onset / after a word offset (default 20; see [audio-pipeline.md § Zero-crossing and crossfade](audio-pipeline.md#zero-crossing-and-crossfade))
- `splice_crossfade_ms`: length (ms) of the crossfade at a splice seam (recorded as the splice's fade length; rendered as a centered equal-power overlay — see [audio-pipeline.md § Zero-crossing and crossfade](audio-pipeline.md#zero-crossing-and-crossfade)); also the local-RMS analysis window of the zero-crossing search (the two are equal by design; default 2)
