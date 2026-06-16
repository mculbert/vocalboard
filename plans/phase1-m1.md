# Phase 1 · M1 — Core Persistence & Timeline Engine (action plan)

Step-by-step plan for the M1 milestone from [phase1.md](phase1.md). The authoritative
spec is [data-model.md](../design/data-model.md); the three commands are defined in
[command-surface.md](../design/command-surface.md).

**Definition of done:** a heavily-tested Rust engine that creates a project SQLite file,
builds and queries the in-memory implicit timeline tree, journals deltas + snapshots,
**replays them on open to a byte-identical tree**, and undoes/redoes via delta inverses —
wired end-to-end through the `new_project` / `open_project` / `save_snapshot_now` Tauri
commands. **No audio, no ML**: correctness is proven against *synthetic* turns in tests.

## Decisions to lock first (recommended defaults)

- **Full Tauri wiring.** Engine logic lives in `core`; the three commands are exposed as
  `#[tauri::command]` handlers backed by a managed `ProjectState`, with generated TS
  wrappers (no UI — consistent with how M0 wired `ping` / `app_info`). Structure
  `ProjectState` so Phase 6 can instantiate a second one per
  [architecture.md § Rust process](../design/architecture.md#rust-process-tauri).
- **Snapshot writer + explicit/exit triggers only.** Build the background snapshot-writer
  mechanism wired to `save_snapshot_now` and app-exit. **Defer the 30 s idle-autosave
  timer to M5** (no command mutates the timeline in M1, so it cannot be exercised
  end-to-end). Recorded under M5 in [phase1.md](phase1.md).
- **New crate dependencies** (added to `core/Cargo.toml`; justified per
  [conventions.md](../design/conventions.md) I2): `rusqlite` with the `bundled` feature (per
  [ops.md](../design/ops.md#rust-crate-dependencies-key)), `blake3`, and `postcard` 1.x with its
  `use-std` feature (`bincode 2.x` was the original spec choice but has RUSTSEC-2025-0141
  unmaintainability; postcard is the well-maintained community successor with identical
  serde + deterministic-binary semantics). Dev-only: `tempfile` for filesystem tests.
  All three are named in [data-model.md](../design/data-model.md) as the prescribed hash +
  serialization approach.
- **Working branch:** `claude/1M1` (commits unsigned per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md)); squash-merged to `main` via PR. Split into numbered
  sub-step commits (`1M1-1 …`) mirroring M0.

## Module layout (within `src-tauri/core/`)

```
db/
  mod.rs          Db handle: open/create, WAL + foreign_keys pragmas, txn helpers
  migrations.rs   user_version runner; embeds migrations/0001_initial.sql via include_str!
  store.rs        blob store: put(tagged bytes)->Hash, get(Hash)->bytes, INSERT OR IGNORE
  journal.rs      latest-snapshot lookup + deltas-after-id scan (read side, Step 8);
                  append rows (type -1/0/1) + latest_metadata / MetaRow (write side + meta read, Step 9)
project/
  hash.rs         Hash([u8;16]); blake3-128; FormatTag byte enum; bincode encode/decode helpers
  tilable.rs      Tilable trait: total_duration() — implemented by Turn and Label
  turn.rs         Turn / Word / Splice / WordType / SpliceKind (Serde, ordered collections only)
  label.rs        Label / LabelKind (track 0 element; own blob kind 0x6)
  tree.rs         Node<T: Tilable> + ImplicitTimelineTree<T>: AVL insert/update/delete,
                  temporal queries, O(n) bulk-build, path-based predecessor/successor
  delta.rs        Delta / DeltaOp / Location (Start | After(Hash)); apply-to-adjacency-list; inverse
  snapshot.rs     Snapshot struct; flatten tree -> Snapshot; build adjacency list from Snapshot
  metadata.rs     Metadata / ProjectMeta / TrackMeta / SpeakerMeta / ModelUse; load/store;
                  source-file resolution (returns the missing-track list) (Step 9)
  command_id.rs   CommandId enum codes (bit-mask, append-only; UNDO_FLAG = 0x1) (Step 9)
  undo.rs         TimelineState { trees, metadata }; History (bounded undo VecDeque + redo Vec)
                  holding UndoEntry { before/after Arc<TimelineState>, fwd/inv journal effects }
  engine.rs       ProjectState: Db handle, current Arc<TimelineState>, History; new_project /
                  open_project / save_snapshot_now; snapshot-writer handle
```

`app/main.rs` gains a managed `ProjectState` (`Mutex<Option<ProjectState>>`) and the three
command handlers. `proto` gains `OpenProjectParams` + empty `SaveSnapshotNowParams` (and any
result types); `types.ts` is regenerated. `src/lib/ipc/commands.ts` gains the wrappers.

Build the persistence/timeline core first and prove it with synthetic data before wiring
commands (per [phase1.md](phase1.md) guiding principle 1).

---

## Step 1 — Action-plan doc, branch, dependencies

- This document; create the `claude/1M1` branch.
- Add `rusqlite` (`bundled`), `blake3`, `bincode` 2.x (`serde` feature) to
  `core/Cargo.toml`; add `tempfile` as a dev-dependency.
- **Verify:** `cargo build` green; `cargo deny check` passes for the new deps (license +
  advisory policy in `deny.toml`).

## Step 2 — `db` schema + migrations

- `core/migrations/0001_initial.sql`: the exact three-table DDL from
  [data-model.md § Schema DDL](../design/data-model.md#schema-ddl-phase-1-user_version--1) —
  `project` (singleton, `CHECK (id = 1)`), `store`, `journal` + `journal_type_idx`;
  `PRAGMA journal_mode = WAL`, `PRAGMA foreign_keys = ON`, and
  `PRAGMA busy_timeout = 5000` applied on every open. The busy timeout makes a
  writer that finds the lock held wait-and-retry instead of failing with
  `SQLITE_BUSY`; harmless in M1 (no concurrent writers) but required from M5 on,
  when edit commands race the background snapshot writer (see Step 11).
- `db/migrations.rs`: read `PRAGMA user_version`, apply pending numbered migrations in
  order inside a transaction, write the new version back. A file whose `user_version`
  exceeds our maximum is **refused** with a clear error referencing `min_app_version`
  (no down-migrations).
- **Verify:** unit tests — fresh DB ends at `user_version = 1`; re-open is a no-op; a
  doctored future-version DB is refused cleanly.

## Step 3 — Hashing + serialization (`project/hash.rs`)

See [phase1-m1-03.md](phase1-m1-03.md) for the detailed action plan.

- `Hash([u8; 16])`; BLAKE3 truncated to 128 bits (named constant for the width).
- Tag byte = `(kind << 4) | version`: high nibble is the kind (`Turn`=0x1, `Metadata`=0x2,
  `Snapshot`=0x3, `RoomTonePcm`=0x4, `Embedding`=0x5), low nibble is the format version
  (0–15 per kind). The hash covers the **full tagged bytes** (tag ++ postcard payload).
  See [data-model.md § Schema version](../design/data-model.md#schema-version) for the lazy-migration
  policy: old-format blobs stay readable via per-version deserializers; re-serialization
  only on genuine content edits.
- Generic `encode_tagged` / `decode_tagged` / `decode_tagged_as` helpers; per-kind typed
  loaders/writers live with their structs in later steps and call these for the common plumbing.
- Serialization uses `postcard::to_stdvec` / `postcard::from_bytes` — deterministic by
  default ([data-model.md § Serialization](../design/data-model.md#serialization)).
- **Verify:** see [phase1-m1-03.md](phase1-m1-03.md#3c--implement-coresrcprojecthashrs)
  for the full test list.

## Step 4 — Tree element payloads: `Turn` / `Word` / `Splice` / `Label` + `Tilable`

See [phase1-m1-04.md](phase1-m1-04.md) for the detailed action plan.

- `project/tilable.rs`: one-method `Tilable` trait (`fn total_duration(&self) -> i64`).
- `project/turn.rs`: `Turn { id, speaker_id, turn_duration, post_turn_silence, words,
  splices }`; `Word { word_type, text, start_sec, end_sec, is_cut, is_muted,
  source_onset_sample: Option<i64>, length_samples }`; `WordType` enum (`Normal | Disfluency |
  Sound` — no `Label` / `Section` variants); `Splice { length_samples, fade_in_samples,
  fade_out_samples, kind }` with `SpliceKind { Source { source_start_sample,
  source_decode_offset }, RoomTone, Silence }` (source-only fields live in the variant,
  not as `Option<i64>` on the parent). Per
  [data-model.md § Turn payload (speech tracks)](../design/data-model.md#turn-payload-speech-tracks).
- `project/label.rs`: `Label { id, text, kind, post_label_silence }` with
  `LabelKind { Plain | Section }`. Separate blob kind `Kind::Label = 0x6` with its own
  V1 wire schema, `store_label` / `load_label`, and `LATEST_LABEL_VERSION`. Per
  [data-model.md § Label payload (track 0)](../design/data-model.md#label-payload-track-0).
- Both types `impl Tilable`. **Ordered collections only** (no `HashMap`) — the
  determinism invariant.
- **Verify:** see [phase1-m1-04.md](phase1-m1-04.md) for the full test list (paired
  pinned-bytes / pinned-hash tests for Turn V1 and Label V1; round-trip + variant /
  enum coverage; tag-byte and kind-mismatch dispatch).

## Step 5 — Blob store (`db/store.rs`)

See [phase1-m1-05.md](phase1-m1-05.md) for the detailed action plan.

- `put(&Connection, &Hash, &[u8]) -> Result<bool, StoreError>`: `INSERT OR IGNORE` into
  `store` (idempotent — a re-derived identical blob is a no-op). Tagging and hashing
  already happen upstream in [`hash.rs`](phase1-m1-03.md) / [`store_turn` /
  `store_label`](phase1-m1-04.md); `put` is the insert half of the round-trip and
  trusts the supplied `(hash, bytes)` pair (`debug_assert!` guards against
  caller-side bugs). Returns `Ok(true)` when a new row was written, `Ok(false)`
  when the hash was already present.
- `get(&Connection, &Hash) -> Result<Vec<u8>, StoreError>`: fetch by hash,
  **re-hash the fetched bytes and return a typed `HashMismatch` error if the
  recomputed hash differs from the lookup key** (catches on-disk corruption —
  postcard-level errors are caught by [`hash.rs`](phase1-m1-03.md), but bit-flips
  that still parse as valid postcard only get caught here). Returns the full
  tagged blob; downstream `load_turn` / `load_label` parse the tag.
- **Verify:** see [phase1-m1-05.md](phase1-m1-05.md#test-plan) for the full test
  list (round-trip, dedup, distinct-blobs, not-found, corruption, truncation,
  transaction-scoped put/get).

## Step 6 — Implicit timeline tree (`project/tree.rs`)

- **Generic over element type:** `Node<T: Tilable>` and `ImplicitTimelineTree<T>`.
  Speech tracks instantiate as `Tree<Turn>`; track 0 as `Tree<Label>`. The trait keeps
  the tree from caring about Turn-specific or Label-specific fields.
- Immutable `Arc<Node<T>>` AVL keyed by timeline position; augmentation
  `left_subtree_sum` = Σ(`element.total_duration()`) over the left subtree, plus
  `height`; both **derived, never serialized**. Edits **path-copy** to the root
  (structural sharing); no parent pointers.
- Temporal queries per [data-model.md § Temporal query](../design/data-model.md#temporal-query):
  `element_at_sample(T)` and the inverse `start_sample_of(element_hash)`. The in-element
  offset interpretation (in-speech vs. post-turn-silence for Turn; in-the-gap for Label)
  lives in element-specific helpers in `turn.rs` / `label.rs`, not in the generic tree.
  O(n) bulk-build from an ordered `Vec<Arc<T>>`; predecessor/successor via the recorded
  search path (needed for delta `location`).
- **Verify (heavy):** for **both** `Tree<Turn>` and `Tree<Label>`: empty / single-node /
  many-node; AVL balance + augmentation invariant after random insert/delete; queries
  at `T = 0`, the last sample, and past the end; `start_sample_of` inverts
  `element_at_sample`; bulk-build == incremental insert; an edit leaves the prior root
  `Arc` unchanged. Turn-tree adds the mid-turn vs. post-turn-silence cases per
  data-model.md's in-element offset interpretation.

## Step 7 — Deltas (`project/delta.rs`)

See [phase1-m1-07.md](phase1-m1-07.md) for the detailed action plan.

- `Delta { track_id, op, location, hash }`, `DeltaOp` (`InsertAfter` / `UpdateAfter` /
  `DeleteAfter`), `Location` (`Start` | `After(Hash)`), per
  [data-model.md § Deltas](../design/data-model.md#deltas). The `After(Hash)` variant points to
  whatever element kind sits on the delta's track (Turn for `track_id > 0`, Label for
  `track_id == 0`) — the delta itself is kind-agnostic.
- An `AdjacencyList` type (`HashMap<Location, Option<Hash>>` — every legal location is a
  key, with the terminal end represented as a `Location → None` entry; the empty list is
  `{ Start: None }`) + `apply(&mut adj, &batch)` for **replay only** (consumed by Step 8).
  Forward-edit code never builds an
  `AdjacencyList`; the engine (Step 11) mutates the in-memory tree through its Step 6
  primitives and captures the inverse `Delta` at the edit site, where it already holds
  `h_old` / `h_removed` (`InsertAfter`↔`DeleteAfter`, `UpdateAfter h_new`↔`UpdateAfter
  h_old`).
- Journal payload encode/decode with `LATEST_DELTA_VERSION = 1` byte + postcard
  `Vec<Delta>`; `mod v1::DeltaV1` frozen wire schema paralleling Turn / Label.
- **Verify:** see [phase1-m1-07.md § Test plan](phase1-m1-07.md#test-plan) for the
  full test list.

## Step 8 — Snapshot + replay (`project/snapshot.rs`)

See [phase1-m1-08.md](phase1-m1-08.md) for the detailed action plan.

- `Snapshot { tracks: Vec<(u32 /*track_id*/, Vec<Hash>)> }`; flatten a tree to its ordered
  hash sequence; build an adjacency list from a snapshot. Replay per
  [data-model.md § Load / replay](../design/data-model.md#load--replay): latest `type = 1` snapshot →
  adjacency list → apply `type = 0` deltas with `id` greater than the snapshot's → walk from
  `Start` → fetch/deserialize each element (`load_label` for `track_id == 0`, `load_turn`
  otherwise) → O(n) bulk-build into the per-track tree (`Tree<Label>` for track 0,
  `Tree<Turn>` elsewhere).
- **From Step 6:** flatten is a one-liner — `tree.iter().map(|e| e.hash).collect::<Vec<_>>()`.
  The reverse (snapshot → tree) calls `ImplicitTimelineTree::from_sorted_elements(elements)`
  after loading each hash from the blob store. **Replay never calls the tree's mutation
  primitives** (`insert_at` / `update_at` / `delete_at`) — delta application runs on the
  working adjacency list; `from_sorted_elements` is the single construction sweep at the end.
- **Two `as_of`-bounded load entry points** over module-internal,
  adjacency-passing helpers (full design in [phase1-m1-08.md](phase1-m1-08.md)).
  `load_latest_snapshot(&Db, as_of) -> Result<(i64, PerTrackTrees), _>` builds
  trees from the latest snapshot at/before `as_of` with no forward-replay (the
  corrupt-journal recovery path, see Step 11); `load_and_replay(&Db, as_of) ->
  Result<PerTrackTrees, _>` then applies the `type = 0` rows up to `as_of`. The
  `as_of: Option<i64>` endpoint (inclusive journal row id; `None` ⇒ end of
  journal) supports the future project-history / time-travel feature, which
  reconstructs state at an arbitrary journal point — possibly mid-run between two
  snapshots. Both call shared internals (`snapshot_adjacency` → optional
  `replay_into` → `build_trees`) so the single tree-construction sweep happens
  once on the typical path; the rare fallback re-derives the snapshot adjacency.
- **Verify:** a synthetic snapshot + delta sequence replays to the expected ordered
  elements for **both** a speech track and track 0; the trees from replay equal the trees
  captured before the save. Use `ImplicitTimelineTree`'s `PartialEq` (sequence equality over
  `(hash, total_duration)` pairs) for the equivalence assertion — it compares element
  sequences, not AVL shape, so bulk-built and incrementally-built trees with the same content
  compare equal. Also: a "snapshot-only" load test that calls `load_latest_snapshot` against
  a project with subsequent `type = 0` rows and asserts the resulting trees match the
  snapshot, not the post-replay state — pins the recovery primitive.
- **Journal read-side lands here.** Replay needs two `as_of`-bounded journal
  queries (latest `type = 1` row at/before an id; `type = 0` rows in an id
  range), so Step 8 creates `db/journal.rs` with the read helpers
  `latest_snapshot` / `deltas_after` (served by `journal_type_idx`). Their
  `SnapshotRow` / `DeltaRow` structs carry `command_id` + `applied_at` for the
  future history view (no replay use, no efficiency cost). Step 9 extends the
  same module with the append/write side and the metadata plumbing.

## Step 9 — Journal ops + metadata (`db/journal.rs`, `project/metadata.rs`, `project/command_id.rs`)

See [phase1-m1-09.md](phase1-m1-09.md) for the detailed action plan.

- **`project/command_id.rs`** — `CommandId` enum: bit-mask category codes, append-only
  policy. Each code is a bit-mask flag; OR-ing the codes across a journal range gives the
  set of command categories touched (for the M5+ history view). Bit 0 (`0x1`) is the Undo
  flag. M1 stamps `CommandId::Unknown` (`0x0`) on every row it writes.
- **Journal write side** — `append_delta_batch` / `append_snapshot` / `append_metadata`
  (+ private `append_row`), each returning the new row id. `applied_at` is a caller-supplied
  parameter (clock-free, deterministic in tests). Also adds `latest_metadata` / `MetaRow`
  (the third read helper, mirroring `latest_snapshot`). Generalizes the
  `MalformedHashPayload` Display from "snapshot row" → "journal row" to cover `type = -1`
  rows too.
- **`project/metadata.rs`** — `Metadata { project, tracks, speakers }` (+
  `ProjectMeta`, `TrackMeta`, `SpeakerMeta`, `ModelUse`, `SourceType`) per
  [data-model.md § Non-timeline data](../design/data-model.md#non-timeline-data),
  stored as a `type = −1` blob (most-recent wins, no replay); large binaries referenced by
  hash. `ModelUse` is a flat struct of `Option<String>` role fields (one per ML role) — not
  a `Vec`. Pinned wire-bytes + hash tests (G1 invariant) included. Source-file resolution
  (relative → absolute fallback → missing list) is **pure** (no DB I/O) per
  [data-model.md § Audio file resolution](../design/data-model.md#audio-file-resolution); the
  **Missing-Files dialog is M6** — M1 only returns the list. Persisting the
  `FoundViaAbsolute` relative-path rewrite is deferred to Step 11 / M6.
- **Verify:** full test suite per [phase1-m1-09.md § Test plan](phase1-m1-09.md#test-plan)
  — command-code stability (every variant pinned), journal write round-trips,
  most-recent-wins + as-of-bounded metadata read, binary-blob reuse, pinned wire
  bytes + hash, canonical-order predicate, source-file resolution with `tempfile` dirs.
- **Cleanup deferred to Step 11.** The `#[allow(dead_code)]` attributes on
  `db::store::put`, `db::store::get`, and `Db::conn` stay until Step 11: replay
  (Step 8) and metadata (Step 9) are both `pub(crate)` and only test-reachable
  until the engine wires them, so they leave those items dead to clippy. The
  engine in Step 11 is the first genuine non-test caller — it removes the
  attributes (along with the `#[allow(dead_code)]` on the Step 8/9 plumbing).

## Step 10 — Undo / redo (`project/undo.rs`)

See [phase1-m1-10.md](phase1-m1-10.md) for the detailed action plan.

- **Unified immutable state.** The undoable project state is one immutable value behind an
  `Arc`: `TimelineState { trees: PerTrackTrees, metadata: Metadata }`. An edit builds a new
  `TimelineState` (cloning the small `BTreeMap` spine + the `Metadata` struct; tree subtrees
  stay `Arc`-shared, large metadata binaries are by-hash) and swaps the engine's
  `current: Arc<TimelineState>`. Undo/redo are a single `Arc` swap — timeline **and** metadata
  revert together, so there is no separate metadata-undo path.
- A `History` value owns the stacks. An `UndoEntry` bundles the **before** and **after**
  `Arc<TimelineState>` snapshots (cheap `Arc` clones — structural sharing) plus the forward and
  inverse **journal effects** (an optional delta `Vec<Delta>` and a `metadata_changed` flag —
  the metadata blobs are re-derived from the snapshots) and the forward `CommandId` category.
- `record` pushes an entry and clears redo, evicting the oldest entry when the stack exceeds the
  configurable limit. `undo` pops, swaps `current` to `before`, and **appends the inverse effect
  to the journal in one transaction** (`type = 0` via `append_delta_batch` and/or `type = -1` via
  `store_metadata` + `store::put` + `append_metadata`, stamped via the new `CommandId::undo_of`
  = `category | UNDO_FLAG`); `redo` is symmetric (swap to `after`, append the forward effect
  stamped with the plain category). An undo is thus just another forward-recorded edit; replay on
  reopen reproduces the post-undo state. See [data-model.md § Undo / redo](../design/data-model.md#undo--redo).
- **Bounded stack ⇒ `VecDeque`.** The undo stack is a stack at the recent end (`push_back` /
  `pop_back`) and a queue at the old end (`pop_front` to evict past the limit). The redo stack
  stays a `Vec` (cleared on every `record`, bounded by undo depth, never front-evicted).
- **Producer vs. consumer.** Step 10 builds the **consumer** — `History` + the swap-and-journal
  mechanics, operating on a `&mut Arc<TimelineState>` + `&mut Connection` passed in (no
  `ProjectState` yet, so it is unit-testable in isolation). The **producer** (`apply_batch`,
  builds an `UndoEntry` from a real edit) lands in Step 11; Step 10 tests synthesize entries.
- **Added here:** the `undo_history_limit` app setting (new `#[serde(default)]` field in
  `settings.rs` returning 50, with a round-trip test per the data-integrity invariant — additive
  defaulted field, no version bump); `DEFAULT_UNDO_HISTORY_LIMIT` const; `CommandId::undo_of`;
  `Clone` on the Step 8 `TrackTree`; `Db::conn_mut` (for the transaction).
- **Verify:** unit tests on stack transitions + eviction (record clears redo; LIFO; oldest
  evicted past the limit; empty no-ops), plus integration tests against a hand-built `Db`: a
  tree-only edit **and** a metadata-changing edit each → `record` → `undo` restores the snapshot
  and appends the inverse row(s) → `load_and_replay` + `load_current_metadata` reproduce the
  post-undo state → `redo` restores the post-edit state.

## Step 11 — `ProjectState` engine + snapshot writer (`project/engine.rs`)

- `ProjectState` holds the `Db`, the current `Arc<TimelineState>` (trees + metadata, defined in
  Step 10), and a `History` (the undo/redo stacks, sized from `settings.undo_history_limit`).
  - **Wall clock:** define a `now_posix() -> i64` helper in the engine (POSIX seconds
    UTC). The engine calls it once per command to supply `applied_at` to all
    `journal::append_*` calls in that command's transaction — keeping `journal.rs`
    clock-free and deterministic in tests (Step 9 design decision).
  - `new_project(path, sample_rate)`: create the file, run migrations, write the
    `project` singleton, and write an initial empty snapshot via
    `journal::append_snapshot(tx, CommandId::Unknown, &h, now)` (Step 9).
  - `save_snapshot_now()`: similarly calls `journal::append_snapshot(tx,
    CommandId::Unknown, &h, now)` after storing the snapshot blob — a snapshot is not
    an edit, so `CommandId::Unknown` (`0x0`) is the correct stamp here too (Step 9).
  - `open_project(path)`: run `load_and_replay(db, None)` + `metadata::load_current_metadata(&db, None)`
    + `metadata::missing_tracks(dir, &meta)` to build the missing-files list (Step 9 pure
    helpers). Persist the `FoundViaAbsolute` relative-path rewrite here (a `type = -1`
    write via `journal::append_metadata`). **On replay failure, fall back to the
    snapshot-only load path from Step 8** (`load_latest_snapshot(db, None)`, no
    forward-replay) and surface a recoverable error to the UI naming the failed journal
    row id and the snapshot id the project was rolled back to. Edits made after the
    snapshot are lost; the user is informed and can choose to keep working from that state
    (subsequent `save_snapshot_now` writes a fresh snapshot, after which the abandoned
    `type = 0` rows are journal-tail garbage that the future M5+ compact step can prune).
    Snapshot-load failure is unrecoverable — surface a fatal error and refuse to open.
    The Missing-Files dialog (M6) follows the same "open partially, surface the issue"
    pattern; M1 ships the error and the snapshot-fallback path, the dialog UI lands later.
  - **Step 11 removes all `#[allow(dead_code)]` on the Step 8/9/10 plumbing** (`store::put`,
    `store::get`, `Db::conn`, `journal::append_*`, `latest_metadata`, `MetaRow`,
    `load_current_metadata`, `missing_tracks`, `resolve_track_source`, `FileResolution`,
    and the Step 10 `undo::History` / `UndoEntry` / `TimelineState` / `CommandId::undo_of` /
    `Db::conn_mut`) since the engine provides their first genuine non-test callers — plus the
    rest of the chain the engine reaches transitively (`snapshot::{load_and_replay,
    load_latest_snapshot, snapshot_from_trees, snapshot_adjacency, replay_into, build_trees,
    build_track_tree}`, `delta::{apply, encode/decode_delta_batch, AdjacencyList}`,
    `journal::{latest_snapshot, deltas_after}`, `undo::append_effect`, `command_id::{from_code,
    UNDO_FLAG}`). A handful of **field/method-level** allows are kept for genuinely not-yet-wired
    gaps (`History::{record, can_undo, can_redo}`, the `command_id`/`applied_at` row fields,
    `FileResolution` path fields, `AdjacencyList`'s query API, `Db::with_transaction`) — the full
    list and the milestone each is owed to is in
    [phase1-m1-11.md § Dead-code cleanup](phase1-m1-11.md#remaining-allowdead_code-after-the-11a-sweep-deliberate-gaps).
- **From Step 8:** the per-track tree representation already exists — `enum TrackTree {
  Labels(ImplicitTimelineTree<Label>), Speech(ImplicitTimelineTree<Turn>) }` and
  `type PerTrackTrees = BTreeMap<u32, TrackTree>` in `snapshot.rs` (track 0 ⇒ `Labels`, others
  ⇒ `Speech`). It is wrapped in `Arc<TimelineState>` (with `Metadata`) as the engine's `current`
  state (see Step 10); `Clone` was added to `TrackTree` in Step 10. Building a new `TimelineState`
  per edit clones the small `BTreeMap` spine + `Metadata` (each `TrackTree` clone is one Arc
  refcount bump; subtrees stay shared), so it is cheap regardless of project size — and
  `save_snapshot_now()` just clones `current` to hand the frozen state to the background writer.
- **`apply_batch` (the edit applier).** This is the **producer** of the `UndoEntry` packages
  the Step 10 `History` consumes. It mutates working clones of the touched `TrackTree`s via the
  Step 6 primitives, captures each op's `Location` + `h_old` from `element_at_sample` and emits
  the forward+inverse `Delta` pair at the edit site (per Step 7), `store::put`s each new element
  blob and `append_delta_batch`es the forward batch in one transaction; then — only on commit —
  builds the new `Arc<TimelineState>`, swaps `current`, and calls `history.record` with the
  before/after snapshots + journal effects. It **applies a batch in descending sample order**
  over original-tree-coordinate positions, per
  [data-model.md § Batched (multi-element) edits](../design/data-model.md#batched-multi-element-edits). As of
  **Step 11d**, `apply_batch` also accepts an optional `Metadata` argument and journals the
  metadata change in the **same transaction** as the delta batch — the producer capability for
  combined tree+metadata edits (e.g. `add_track`) is in place. The track *commands*
  (`add_track`, `remove_track`), the reconciliation guard, and their round-trip fixtures are **M5**.
  M1 exercises `apply_batch` with synthetic edits only (no real editing command exists until M4/M5).
- **Threading note:** give the writer its own `rusqlite` connection — WAL permits a
  concurrent reader while writers serialize via SQLite locks; no edits run concurrently in
  M1, so this is safe. Document the constraint where the writer is spawned. The snapshot's
  cost is in-memory and lock-free: the O(1) `tree.clone()` handoff and the O(n) flatten +
  postcard + BLAKE3 happen with no connection held; only the final two-INSERT write
  (snapshot blob + `type = 1` row, a `Vec` of 16-byte hashes ≲ ~1 MB even for huge
  projects) takes the write lock, for single-digit ms. So no write queue is needed. From
  M5 on, when a synchronous main-thread edit write can race that snapshot commit, the
  `busy_timeout` set in Step 2 makes the loser wait-and-retry rather than hit `SQLITE_BUSY`;
  the worst case is one command handler delayed a few ms (the cpal callback never touches
  SQLite, so playback is unaffected).
- **Verify:** integration test in `core/tests/` — `new_project` → apply synthetic delta
  batches → `save_snapshot_now` → drop → `open_project` → identical trees + metadata.
  Plus a journal-corruption recovery test: after a successful snapshot, append a
  hand-doctored `type = 0` row whose payload won't decode → `open_project` returns the
  recoverable error variant carrying the failed row id and the snapshot id, and the
  resulting `ProjectState` matches the post-snapshot pre-corruption state.

## Step 12 — Tauri wiring + contract

See [phase1-m1-12.md](phase1-m1-12.md) for the detailed action plan. Split into three
sub-step commits (`1M1-12a`/`12b`/`12c`), risk-ascending like Step 11 — the first two
**back-fix M0-era contract conventions** a review surfaced, before Step 12 adds a second
command family on top of them; the third is the wiring itself:

- **12a** — binding generation hardening + ES-module exports: replace the codegen-as-test
  with a feature-gated `gen_bindings` bin (`cargo test` becomes hermetic) and emit
  `export` module types instead of ambient globals.
- **12b** — unified typed error contract: a canonical `CommandError { code, message }`
  (sidecar `ErrorMsg` embeds it), the new project-lifecycle + `invalid_params` `ErrorCode`
  variants, and migration of the M0 handlers off bare `String` errors.
- **12c** — `ProjectState` command wiring: managed `ProjectSlot`, the three
  `#[tauri::command]` handlers (`new_project` / `open_project` / `save_snapshot_now`), the
  `proto` param/result types, the TS wrappers, plus the **versioning** (version-by-command-name
  on the Tauri boundary) and **validation** (serde + `deny_unknown_fields`, value-checks)
  convention reconciliations the handlers embody.
- **Verify:** `cargo run -p proto --features ts-export --bin gen_bindings -- --check` (the new
  TS-bindings CI gate), `cargo test --workspace`, `pnpm check && pnpm test && pnpm build` green.

## Step 13 — G1 round-trip fixture + final pass

See [phase1-m1-13.md](phase1-m1-13.md) for the detailed action plan.

- Commit a v1-format `.vocalboard` fixture under `core/tests/fixtures/` and a test that opens
  it and verifies it loads (per [conventions.md](../design/conventions.md) G1 — a persisted-format
  change ships a migration **and** a fixture round-trip test). The fixture must contain a
  real `Kind::Metadata` blob (i.e. a `type = -1` journal row pointing at a stored metadata
  blob), so that opening it exercises `load_metadata` + `load_current_metadata` end-to-end
  — completing the G1 round-trip for the metadata wire format on top of Step 9's pinned-bytes
  tests (Step 9 downstream implication).
- Run the full gate.
- **Verify:** `cargo fmt --check`, `cargo clippy -- -D warnings` (incl. `missing_docs`,
  `unwrap_used`), `cargo test --workspace`, and `pnpm check / test / build` all green.

## Testing strategy (M1 is "test heavily")

- Inline `#[cfg(test)]` unit tests per module hitting the boundary cases above (empty /
  single / max / overlapping / gaps) — [conventions.md](../design/conventions.md) A1.
- Cross-cutting **integration tests** in `core/tests/`: the full
  new → edit (synthetic) → snapshot → reopen lifecycle, and a replay-equivalence test
  (incremental build == replay).
- **Determinism** tests for hashing/serialization (the content-addressing invariant).
- **G1 fixture** round-trip test (committed v1 project file) wired into `cargo test`.
- Synthetic turns are built via test-only helpers in `core` (no turn-creating command
  exists until M4).

## M1 exit criteria

- `cargo test --workspace` (unit + integration + fixture), `cargo clippy -- -D warnings`,
  and `cargo fmt --check` all green locally and in CI.
- A `.vocalboard` file created by `new_project` has the three tables, `user_version = 1`,
  and journal rows for the initial snapshot; `open_project` reconstructs identical state.
- The three commands round-trip through Tauri with regenerated, in-sync TS bindings;
  `pnpm check && pnpm build` green.
- [data-model.md](../design/data-model.md) stays authoritative — any field/behavior adjusted during
  implementation is updated there in the same commit.

> **Deferred to later milestones:** the 30 s idle-autosave timer (M5, when edits land); the
> Missing-Files dialog UI (M6); the **migration-consent dialog and read-only open mode**
> (M6 — `open_project` v1 runs migrations unconditionally; M6 adds the user-consent step
> and the engine's read-only mode per [data-model.md § Schema version](../design/data-model.md#schema-version));
> any turn-mutating command (M4/M5), and with it the **undo/redo command surface** (the engine's
> `undo`/`redo` + `History::{can_undo, can_redo}` are built and tested in M1 but have no Tauri
> command until there is an edit to undo). A concurrent-request test on the sidecar `send()` path
> (M0 retro) is added when the first overlapping command lands.
