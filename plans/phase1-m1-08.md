# Phase 1 · M1 · Step 8 — Snapshot + replay (`project/snapshot.rs`) (action plan)

Per-step action plan for Step 8 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md § Snapshot blob](../design/data-model.md#snapshot-blob),
[§ Load / replay](../design/data-model.md#load--replay), and
[§ Hashing and serialization](../design/data-model.md#serialization). This step closes the
persistence loop: it lays down the **`Snapshot` blob** (the vec-flattened
per-track transcript), the **replay path** that reconstructs the in-memory
per-track trees from the latest snapshot plus the deltas journaled after it, and
the **journal read-side** queries that feed replay. It is the first step where a
project on disk round-trips back into live timeline trees.

**Definition of done:** `core/src/project/snapshot.rs` exposes `Snapshot`,
`store_snapshot` / `load_snapshot` (+ `LATEST_SNAPSHOT_VERSION` and a frozen
`mod v1`), the `TrackTree` / `PerTrackTrees` engine state types, `snapshot_from_trees`
(flatten), `ReplayError`, and the two `as_of`-bounded load entry points
`load_latest_snapshot` / `load_and_replay` (backed by module-internal
adjacency-passing helpers). `core/src/db/journal.rs` is created with the
read-side helpers `latest_snapshot` and `deltas_after` (Step 9 extends
this same module with the append/write side + metadata). Both modules are
re-exported from their parents. Full unit + integration coverage: snapshot
round-trip, pinned wire format + pinned wire hash (G1 — a new persisted format),
replay equivalence for a speech track and track 0, the snapshot-only recovery
primitive, and the replay error paths. `cargo test -p core`, `cargo clippy -p
core -- -D warnings`, and `cargo fmt --check` are all green.

## Context

This step sits on top of everything Steps 3–7 shipped and is consumed by the
engine (Step 11):

- [Step 3](phase1-m1-03.md) — `hash.rs`: `Hash`, `Kind` (incl. `Kind::Snapshot
  = 0x3`), `encode_tagged` / `decode_tagged_as`, `parse_tag`, `hash_tagged`,
  `DecodeError`. Snapshot blobs reuse this tagged-bytes plumbing exactly like
  Turn / Label.
- [Step 4](phase1-m1-04.md) — `turn.rs` / `label.rs`: `store_turn` /
  `load_turn`, `store_label` / `load_label`, both keyed on the tag byte. Replay
  dispatches `load_label` for `track_id == 0` and `load_turn` otherwise.
- [Step 5](phase1-m1-05.md) — `db/store.rs`: `store::get(conn, &hash)` (returns
  the tagged blob, re-hashing for bit-rot detection) and `store::put`. Replay
  fetches every element blob — and the snapshot blob itself — through `get`.
- [Step 6](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers) —
  `tree.rs`: `ImplicitTimelineTree<T: Tilable>`. Flatten is the one-liner
  `tree.iter().map(|e| e.hash).collect::<Vec<Hash>>()`; the reverse is
  `ImplicitTimelineTree::from_sorted_elements(Vec<(Hash, Arc<T>)>)`. Tree
  `PartialEq` is sequence-equality over `(hash, total_duration)` pairs, so a
  bulk-built tree compares equal to an incrementally-built one with the same
  content — this is the assertion replay tests use.
- [Step 7](phase1-m1-07.md) — `delta.rs`: `AdjacencyList` (`from_sequence`,
  `iter`, `successor`, …), `decode_delta_batch`, and `apply(&mut adj, &[Delta])`.
  Replay is **the only consumer** of `AdjacencyList` and `apply`, exactly as
  that step anticipated.

The data flow this step implements (per track), from
[data-model.md § Load / replay](../design/data-model.md#load--replay):

```
latest type=1 row ─► snapshot hash ─► store::get ─► load_snapshot ─► Vec<(track_id, Vec<Hash>)>
                                                                            │
                              ┌─────────────────────────────────────────────┘
                              ▼  (per track)
   AdjacencyList::from_sequence(snapshot hashes)
                              │   apply each type=0 batch (id > snapshot id), in id order
                              ▼
   adj.iter().collect::<Vec<Hash>>()  ─►  store::get + load_turn/load_label per hash
                              │
                              ▼
   ImplicitTimelineTree::from_sorted_elements(elements)   ── the single construction sweep
```

## Decisions locked in this step

### Snapshot blob mirrors the Turn / Label tagged-blob pattern

`Snapshot` is a store-resident, content-addressed blob (`Kind::Snapshot = 0x3`,
already in the `Kind` enum). It gets the **same** four-part treatment as Turn /
Label:

```rust
pub const LATEST_SNAPSHOT_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Per-track ordered element hashes, in timeline order. `track_id == 0`
    /// entries point to Label blobs; `track_id > 0` to Turn blobs. The snapshot
    /// itself is kind-agnostic — the loader picks `load_turn` / `load_label`
    /// from `track_id` at fetch time.
    pub tracks: Vec<(u32, Vec<Hash>)>,
}

pub fn store_snapshot(snap: &Snapshot) -> Result<(Hash, Vec<u8>), postcard::Error>;
pub fn load_snapshot(bytes: &[u8]) -> Result<Snapshot, DecodeError>;

pub mod v1 { pub struct SnapshotV1 { pub tracks: Vec<(u32, Vec<Hash>)> } }
// total identity-shaped From<&Snapshot> for v1::SnapshotV1 and From<v1::SnapshotV1> for Snapshot
```

`store_snapshot` is `encode_tagged(Kind::Snapshot, LATEST_SNAPSHOT_VERSION,
&v1::SnapshotV1::from(snap))`; `load_snapshot` is a byte-for-byte copy of
`load_turn` with `Kind::Snapshot` substituted. This is a **new persisted
format**, so per the data-integrity invariant ([conventions.md](../design/conventions.md)
G1 / [CLAUDE.md](../CLAUDE.md)) it ships pinned-bytes + pinned-hash tests in this
step, and the Step 13 G1 fixture round-trips a real snapshot blob by
construction.

`tracks` is a `Vec<(u32, Vec<Hash>)>`, an **ordered collection** (the
determinism invariant — no `HashMap` in a hashed struct). The engine produces it
sorted by `track_id` via `snapshot_from_trees`; replay does not depend on the
ordering being sorted, but pinning it keeps byte-identical snapshots
deduplicating in `store`.

### `TrackTree` enum + `PerTrackTrees` alias live here, not in the engine

Replay must return per-track trees of two different element types in one
collection. Track 0 holds `ImplicitTimelineTree<Label>`; every other track holds
`ImplicitTimelineTree<Turn>`. The natural wrapper (anticipated by
[phase1-m1.md Step 11](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectenginers)):

```rust
pub enum TrackTree {
    Labels(ImplicitTimelineTree<Label>),
    Speech(ImplicitTimelineTree<Turn>),
}

pub type PerTrackTrees = std::collections::BTreeMap<u32, TrackTree>;
```

`TrackTree` carries:
- `pub fn hashes(&self) -> Vec<Hash>` — flatten, dispatching on the variant
  (`tree.iter().map(|e| e.hash).collect()`). Used by `snapshot_from_trees`.
- a `PartialEq` impl: `Labels == Labels` and `Speech == Speech` defer to the
  inner tree's sequence-equality `PartialEq`; mixed variants are never equal.
  `#[derive(PartialEq)]` does exactly this, so derive it.

`PerTrackTrees` lives here (not the engine) because replay is its constructor and
`snapshot_from_trees` is its consumer; Step 11 holds one inside `ProjectState`
and adds the `BTreeMap` mutation wiring. The track-kind rule — **`track_id == 0`
⇒ `Labels`, else `Speech`** — is global and is the single source of the Turn /
Label dispatch throughout this module.

### Point-in-history load: the `as_of` endpoint

Replay is not only "latest snapshot → end of journal." A future
project-history / time-travel feature (M5+) needs to reconstruct project state
**as of an arbitrary journal point**, which can fall in the middle of the delta
run between two snapshots — so both the snapshot we start from and the last
delta we apply must be parameterizable. Both public load entry points therefore
take an inclusive endpoint:

```rust
as_of: Option<i64>   // inclusive journal row id; None ⇒ absolute end of journal
```

`as_of` is one value threaded through the whole reconstruction:

- **Snapshot selection:** load the most recent `type = 1` row with `id <= as_of`
  (or the absolute latest when `None`) — **not** unconditionally the latest
  snapshot. If `as_of` names a `type = 1` row, that snapshot is loaded and no
  deltas follow it within the bound (so it's returned as-is).
- **Replay bound:** apply exactly the `type = 0` rows with
  `snapshot_id < id <= as_of` (when `None`, all rows after the snapshot).

Semantics are **inclusive** (`<= as_of`): the state returned is "the project
after every journal row through `as_of` has been applied." (This realizes the
`before` parameter sketched in the design discussion; it is named `as_of`
because inclusive "before" is ambiguous.) The typical open-project call is
`as_of = None`, recovering today's "latest snapshot, all deltas" behaviour.
`NoSnapshot` now means "no `type = 1` row at or before `as_of`" — e.g. an
`as_of` earlier than the initial snapshot `new_project` writes.

### Two public load entry points over adjacency-returning internals

The public surface is **two** functions (replay-to-trees is never needed
standalone — see the next decision):

```rust
/// Reconstruct per-track trees from the latest snapshot at or before `as_of`,
/// WITHOUT applying any deltas. The snapshot-only recovery / inspection path.
/// Returns the chosen snapshot's journal row id (the engine needs it for the
/// recoverable-error message). Errors `NoSnapshot` if none exists at/before `as_of`.
pub(crate) fn load_latest_snapshot(db: &Db, as_of: Option<i64>)
    -> Result<(i64, PerTrackTrees), ReplayError>;

/// Reconstruct per-track trees as of `as_of`: latest snapshot at/before `as_of`,
/// then every `type = 0` batch with `snapshot_id < id <= as_of`. The happy path.
pub(crate) fn load_and_replay(db: &Db, as_of: Option<i64>)
    -> Result<PerTrackTrees, ReplayError>;
```

Both are thin wrappers over **module-internal helpers that pass the working
`AdjacencyList`s explicitly**, so the single tree-construction sweep happens
once, on whichever path is taken, with **no flatten-and-rebuild round-trip on
the typical path**:

```rust
type AdjLists = BTreeMap<u32, AdjacencyList>;

/// Latest snapshot at/before `as_of` → per-track adjacency lists (seeded from
/// the snapshot's hash sequences via `AdjacencyList::from_sequence`). No element
/// blobs fetched, no trees built. Returns the snapshot row id too.
fn snapshot_adjacency(db: &Db, as_of: Option<i64>) -> Result<(i64, AdjLists), ReplayError>;

/// Apply the `type = 0` rows with `snapshot_id < id <= as_of`, in id order, to
/// the adjacency lists in place. Routes each delta to its track's list.
fn replay_into(db: &Db, snapshot_id: i64, as_of: Option<i64>, adj: &mut AdjLists)
    -> Result<(), ReplayError>;

/// Walk each adjacency list to its ordered `Vec<Hash>`, fetch + decode every
/// element blob, and bulk-build the per-track trees. The single construction sweep.
fn build_trees(conn: &Connection, adj: &AdjLists) -> Result<PerTrackTrees, ReplayError>;
```

```rust
// load_latest_snapshot: snapshot adjacency → trees (no deltas)
let (snapshot_id, adj) = snapshot_adjacency(db, as_of)?;
Ok((snapshot_id, build_trees(db.conn(), &adj)?))

// load_and_replay: snapshot adjacency → apply deltas → trees
let (snapshot_id, mut adj) = snapshot_adjacency(db, as_of)?;
replay_into(db, snapshot_id, as_of, &mut adj)?;
build_trees(db.conn(), &adj)
```

**Where the (rare) duplicate work lands.** `load_and_replay` does **not** call
`load_latest_snapshot`; both call `snapshot_adjacency`. So the engine's happy
path (`load_and_replay`) builds the snapshot adjacency exactly once and never
builds a throwaway tree. Only the **failure** path pays twice: when
`load_and_replay` errors mid-replay and the engine falls back to
`load_latest_snapshot`, the snapshot adjacency is reconstructed a second time.
That is the rare branch, which is where we want the redundancy — the inverse of
the earlier compose-the-publics sketch. This honours the Step 7 contract
unchanged: **delta application runs on the `AdjacencyList`, never on the tree's
`insert_at` / `update_at` / `delete_at`;** `from_sorted_elements` (inside
`build_trees`) is the one construction sweep.

### `replay_into` routes deltas per track in journal order; it is module-internal

`replay_into` is **not** part of the crate-visible surface: its only caller is
`load_and_replay` (the engine never needs replay-onto-existing-trees on its
own — open is always snapshot-then-replay, and forward edits go through the tree
primitives per Step 7). Keeping it private shrinks the surface and removes the
public `replay_deltas_after` from the earlier draft.

It reads the bounded `type = 0` rows in ascending `id` order (one query, served
by `journal_type_idx`) and walks them, routing each delta to its track's list:

```rust
for row in deltas_after(conn, snapshot_id, as_of)? {     // ascending id, id <= as_of
    let batch = decode_delta_batch(&row.payload)
        .map_err(|e| ReplayError::DeltaDecode { row_id: row.id, source: e })?;
    for d in &batch {                                    // intra-batch order preserved
        let list = adj.entry(d.track_id).or_insert_with(AdjacencyList::new);
        delta::apply(list, std::slice::from_ref(d))
            .map_err(|e| ReplayError::DeltaApply { row_id: row.id, source: e })?;
    }
}
```

- A track already present in `adj` (from the snapshot) keeps its seeded list; a
  track first seen in a delta gets a fresh `AdjacencyList::new()` (a track born
  entirely from post-snapshot deltas — M1 has no command that does this, but
  replay stays correct if one ever lands, and tests exercise it synthetically).
- Cross-track deltas are independent (separate lists), so per-row, per-delta
  routing preserves the only ordering that matters (within-track id order then
  within-batch position). Applying one delta at a time via `from_ref` keeps the
  row id attached to any apply error.
- **Invariant: `snapshot_id <= as_of` always holds at this call site.**
  `snapshot_adjacency` selects the latest snapshot with `id <= as_of`, and
  `load_and_replay` threads that same `as_of` into `replay_into` — so `as_of <
  snapshot_id` is unreachable through the public API. Even if it were reached,
  `deltas_after` is **safe**: its predicate `id > snapshot_id AND id <= as_of` is
  unsatisfiable when `as_of < snapshot_id`, so it returns the empty set and
  `replay_into` is a no-op that leaves the snapshot state untouched. No guard is
  added in `replay_into` for an unreachable case; the journal-level emptiness is
  pinned by a `deltas_after` test (see J7).

### `build_trees` / `build_track_tree` are the shared construction helpers

```rust
fn build_trees(conn: &Connection, adj: &AdjLists) -> Result<PerTrackTrees, ReplayError>;
fn build_track_tree(conn: &Connection, track_id: u32, seq: Vec<Hash>)
    -> Result<TrackTree, ReplayError>;
```

`build_trees` iterates `adj`, calling `build_track_tree(conn, track_id,
list.iter().collect())` per track. `build_track_tree`, for each hash in `seq`:
`let bytes = store::get(conn, &h)?;` then `load_turn` / `load_label` keyed on
`track_id`, collecting `Vec<(Hash, Arc<T>)>`, finishing with
`ImplicitTimelineTree::from_sorted_elements(...)` wrapped in the matching
`TrackTree` variant. The Turn / Label dispatch and the blob-fetch loop live in
exactly one place, used by both public entry points.

### `snapshot_from_trees` is the flatten direction (engine save path)

```rust
pub(crate) fn snapshot_from_trees(trees: &PerTrackTrees) -> Snapshot;
```

Maps each `(track_id, TrackTree)` to `(track_id, track_tree.hashes())`,
collecting into `Snapshot { tracks }` in `track_id` order (`BTreeMap` iteration
is already sorted). Step 11's `save_snapshot_now` calls this on the frozen
cloned trees, then `store_snapshot` + `store::put` + a `type = 1` journal append
(append is Step 9). No DB I/O here — pure flatten + struct build.

### Typed `ReplayError`, carrying the offending journal row id

The engine (Step 11) must surface a recoverable error naming **the failed
journal row id and the snapshot id** the project rolled back to. So replay
errors are typed (not `anyhow`) and attach the row id on the forward-replay
paths:

```rust
#[derive(Debug)]
pub enum ReplayError {
    /// No `type = 1` snapshot row exists at or before the requested `as_of`.
    /// With `as_of = None` this means a malformed project (`new_project` always
    /// writes an initial snapshot, so it implies corruption/tampering); with a
    /// bounded `as_of` it means the point is earlier than the first snapshot.
    NoSnapshot,
    /// A `type = 1` row's payload was not a 16-byte hash pointer.
    MalformedSnapshotPayload { row_id: i64, len: usize },
    /// A `type = 0` row's payload failed to decode into a delta batch.
    DeltaDecode { row_id: i64, source: DecodeBatchError },
    /// A delta failed to apply during forward replay.
    DeltaApply { row_id: i64, source: DeltaError },
    /// A blob fetch failed (not found, or on-disk corruption from `store::get`).
    Store(StoreError),
    /// An element or snapshot blob failed to deserialize.
    Decode(DecodeError),
    /// A journal read query failed.
    Journal(JournalError),
}
```

- `Store`, `Decode`, `Journal` get `From` impls (or `.map_err`) so the
  `?` operator threads them. `DeltaDecode` / `DeltaApply` /
  `MalformedSnapshotPayload` are built explicitly because they must capture the
  `row_id`.
- Implement `Display` + `std::error::Error` (with `source()` chaining the inner
  error for the three wrapper variants), matching the house pattern in
  `store.rs` / `tree.rs`. This satisfies the error-handling convention and keeps
  the engine's M6 user-facing mapping straightforward.
- The split between `DeltaDecode`/`DeltaApply` (forward replay — recoverable via
  snapshot fallback) and `Store`/`Decode` during `load_latest_snapshot`
  (snapshot load — fatal) is what lets the engine decide recover-vs-refuse.
  This module does not make that decision; it just reports precisely.

### `db/journal.rs` is created here with the **read** side only

Replay needs two journal queries, both `as_of`-bounded (see the point-in-history
decision). Rather than scatter inline SQL in `snapshot.rs`, this step creates
`db/journal.rs` and puts the read-side helpers there — the canonical home for
journal SQL. **Step 9 extends the same module** with the append/write side
(`type = -1/0/1` row insertion) and the metadata plumbing. (This is a small
re-slice of the phase1-m1.md Step 8/9 boundary; the
[Documentation touches](#documentation-touches) section updates both bullets.)

```rust
/// A `type = 1` snapshot journal row.
pub(crate) struct SnapshotRow {
    pub id:         i64,
    pub hash:       Hash,   // the snapshot blob this row points to
    pub command_id: i64,    // command-type enum code that produced the row
    pub applied_at: i64,    // POSIX seconds, UTC
}

/// A `type = 0` delta journal row.
pub(crate) struct DeltaRow {
    pub id:         i64,
    pub payload:    Vec<u8>, // version-prefixed postcard `Vec<Delta>`
    pub command_id: i64,
    pub applied_at: i64,
}

#[derive(Debug)]
pub(crate) enum JournalError {
    Sqlite(rusqlite::Error),
    /// A `type = 1` row's payload was not exactly 16 bytes.
    MalformedHashPayload { id: i64, len: usize },
}

/// Most recent `type = 1` row with `id <= as_of` (highest such id), or the
/// absolute latest when `as_of` is `None`. `None` result ⇒ no snapshot in range.
pub(crate) fn latest_snapshot(conn: &Connection, as_of: Option<i64>)
    -> Result<Option<SnapshotRow>, JournalError>;

/// All `type = 0` rows with `snapshot_id < id <= until` (or just `id > snapshot_id`
/// when `until` is `None`), in ascending `id` order.
pub(crate) fn deltas_after(conn: &Connection, snapshot_id: i64, until: Option<i64>)
    -> Result<Vec<DeltaRow>, JournalError>;
```

- **`command_id` + `applied_at` are carried now, deliberately.** Replay ignores
  them, but the M5+ project-history view will want them, and there is **no
  efficiency reason to omit them**: both are small integers already living in
  the same row that the query reads, dwarfed by `DeltaRow.payload`'s heap
  allocation — selecting two extra `INTEGER` columns adds no I/O of consequence
  and no second accessor. So a single, complete read struct beats lean-now /
  widen-later. (`command_id` stays a raw `i64` code here; mapping it to the
  `CommandId` enum from `command_id.rs` is a higher-layer concern landed with the
  journal **write** side, Step 9.) The history view, when built, will likely add
  a *unified* all-types `JournalEntry` query rather than reuse these type-specific
  structs — these fields are forward-compatible convenience, not that feature's
  foundation.
- `latest_snapshot`: `SELECT id, payload, command_id, applied_at FROM journal
  WHERE type = 1 AND (?1 IS NULL OR id <= ?1) ORDER BY id DESC LIMIT 1` (served by
  `journal_type_idx`). Decode `payload` to a `Hash` via `Hash(payload.try_into()
  …)`, erroring `MalformedHashPayload` if the BLOB is not 16 bytes.
  `snapshot_adjacency` maps `None` → `ReplayError::NoSnapshot`.
- `deltas_after`: `SELECT id, payload, command_id, applied_at FROM journal WHERE
  type = 0 AND id > ?1 AND (?2 IS NULL OR id <= ?2) ORDER BY id ASC` (range scan
  on `journal_type_idx`). `?2` is `until`.
- The `(?N IS NULL OR …)` guard binds `Option<i64>` directly (rusqlite maps
  `None` → SQL `NULL`), keeping one prepared statement per helper for both the
  bounded and unbounded cases.
- These are `pub(crate)` + `#[allow(dead_code)]` (their first non-test caller is
  Step 11's engine, transitively through the still-dead replay fns). Add `mod
  journal;` to `db/mod.rs`.

### Visibility + dead-code: mirror the established module patterns

- `Snapshot`, `store_snapshot`, `load_snapshot`, `LATEST_SNAPSHOT_VERSION`, `mod
  v1` (+ `SnapshotV1`): `pub`, mirroring `Turn` / `store_turn` exactly (lib-crate
  public surface; no `#[allow(dead_code)]` needed, but they require
  doc-comments under the `missing_docs` gate).
- `TrackTree`, `PerTrackTrees`, `ReplayError`: `pub` (engine state / error types
  the engine and its tests name).
- `load_latest_snapshot`, `load_and_replay`, `snapshot_from_trees`, and all of
  `db/journal.rs`'s helpers: **`pub(crate)` + `#[allow(dead_code)]`** — internal
  engine plumbing whose first non-test caller is Step 11, identical to how
  `delta.rs`'s `apply` / `AdjacencyList` and `store.rs`'s `put` / `get` carry the
  attribute today.
- `snapshot_adjacency`, `replay_into`, `build_trees`, `build_track_tree`:
  private (module-internal `fn`) — never reach the crate surface. `replay_into`'s
  only caller is `load_and_replay`. Private items reached only from test-reachable
  code still need `#[allow(dead_code)]` until Step 11.
  Do **not** remove the existing `#[allow(dead_code)]` on `db::store::get` or
  `Db::conn` in this step: Step 8 reaches them only through still-dead replay
  fns, so they remain dead to clippy until Step 11 wires the engine. The
  phase1-m1.md Step 9 cleanup note is corrected accordingly (see
  [Documentation touches](#documentation-touches)).

## Module surface

### New: `core/src/project/snapshot.rs`

```rust
//! Timeline snapshots and project replay.
//!
//! A [`Snapshot`] is the vec-flattened transcript of every track — the ordered
//! element-hash sequence per track — stored as a content-addressed
//! `Kind::Snapshot` blob and recorded by a `type = 1` journal row. Replay
//! reconstructs the live per-track trees on open: seed an [`AdjacencyList`] per
//! track from the latest snapshot, apply every `type = 0` delta batch journaled
//! after it (in id order), walk each list back to an ordered `Vec<Hash>`, fetch
//! and decode each element blob, and bulk-build the [`ImplicitTimelineTree`].
//! Delta application runs entirely on the adjacency list — the tree is built
//! once, at the end, via [`ImplicitTimelineTree::from_sorted_elements`].
//!
//! See [data-model.md § Snapshot blob](../../../design/data-model.md#snapshot-blob)
//! and [§ Load / replay](../../../design/data-model.md#load--replay).

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db::journal::{self, JournalError};
use crate::db::store::{self, StoreError};
use crate::db::Db;
use super::delta::{self, DecodeBatchError, DeltaError, Location, AdjacencyList};
use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};
use super::label::{load_label, Label};
use super::tree::ImplicitTimelineTree;
use super::turn::{load_turn, Turn};

pub const LATEST_SNAPSHOT_VERSION: u8 = 1;

pub struct Snapshot { /* tracks: Vec<(u32, Vec<Hash>)> */ }
pub fn store_snapshot(snap: &Snapshot) -> Result<(Hash, Vec<u8>), postcard::Error>;
pub fn load_snapshot(bytes: &[u8]) -> Result<Snapshot, DecodeError>;
pub mod v1 { /* SnapshotV1 */ }

pub enum TrackTree { Labels(ImplicitTimelineTree<Label>), Speech(ImplicitTimelineTree<Turn>) }
impl TrackTree { pub fn hashes(&self) -> Vec<Hash>; }
pub type PerTrackTrees = BTreeMap<u32, TrackTree>;

pub enum ReplayError { /* … */ }

pub(crate) fn snapshot_from_trees(trees: &PerTrackTrees) -> Snapshot;

// Two public load entry points, both `as_of`-bounded (None ⇒ end of journal):
pub(crate) fn load_latest_snapshot(db: &Db, as_of: Option<i64>)
    -> Result<(i64, PerTrackTrees), ReplayError>;
pub(crate) fn load_and_replay(db: &Db, as_of: Option<i64>)
    -> Result<PerTrackTrees, ReplayError>;

// Module-internal, adjacency-passing helpers (private):
type AdjLists = BTreeMap<u32, AdjacencyList>;
fn snapshot_adjacency(db: &Db, as_of: Option<i64>) -> Result<(i64, AdjLists), ReplayError>;
fn replay_into(db: &Db, snapshot_id: i64, as_of: Option<i64>, adj: &mut AdjLists)
    -> Result<(), ReplayError>;
fn build_trees(conn: &Connection, adj: &AdjLists) -> Result<PerTrackTrees, ReplayError>;
fn build_track_tree(conn: &Connection, track_id: u32, seq: Vec<Hash>)
    -> Result<TrackTree, ReplayError>;
```

> **Seeding the adjacency lists:** `snapshot_adjacency` builds each track's
> `AdjacencyList` straight from the snapshot's hash sequence via
> `AdjacencyList::from_sequence` — it never builds a tree first, so there is no
> `seed_for(tree)` flatten step. Replay passes whole decoded `Delta`s through
> `delta::apply`; it constructs no `Location` itself, so `Location` need not be
> imported unless a routing helper names it.

### New: `core/src/db/journal.rs`

Read-side helpers as specified under
[the journal-reads decision](#dbjournalrs-is-created-here-with-the-read-side-only).
Add `mod journal;` to `core/src/db/mod.rs` (sibling of `pub mod store;`).
`journal` may be `pub(crate)` at the module level so `snapshot.rs` can name
`journal::latest_snapshot`.

### Revised: `core/src/project/mod.rs`

Add `pub mod snapshot;` (sibling to the existing `pub mod delta;`, `pub mod
tree;`, etc.).

### Reuse from existing code (no new dependencies, no schema change)

- `hash.rs`: `Hash`, `Kind::Snapshot`, `encode_tagged`, `decode_tagged_as`,
  `parse_tag`, `DecodeError`.
- `db/store.rs`: `store::get` for every blob fetch (snapshot + elements);
  `StoreError`.
- `turn.rs` / `label.rs`: `load_turn` / `load_label`; `Turn` / `Label`.
- `tree.rs`: `ImplicitTimelineTree::from_sorted_elements`, `iter`, `PartialEq`.
- `delta.rs`: `AdjacencyList::{from_sequence, iter}`, `decode_delta_batch`,
  `apply`, `DeltaError`, `DecodeBatchError`.
- `db/mod.rs`: `Db::conn()` (pub(crate)) to obtain the `&Connection` the read
  helpers and `store::get` take.
- `serde` + `postcard` — already present; used exactly as `turn.rs` uses them.

## Test plan

Tests are split: snapshot-blob unit tests inline in `snapshot.rs`; journal
read-helper unit tests inline in `db/journal.rs`; the replay
integration-flavoured tests inline in `snapshot.rs` (they need a real `Db`, so
they build one with `tempfile` and seed it via `store::put` + raw `INSERT INTO
journal`). A few may move to `core/tests/` if they grow; inline is fine for M1.

### Shared test scaffolding (in `snapshot.rs` `#[cfg(test)]`)

```rust
fn open_tmp_db() -> (tempfile::TempDir, Db);   // mirror store.rs's helper

fn put_turn(db: &Db, id: u64, dur: i64, silence: i64) -> Hash;   // store_turn + store::put → hash
fn put_label(db: &Db, id: u64, silence: i64) -> Hash;            // store_label + store::put → hash

/// Store a snapshot blob and append its `type = 1` journal row; return row id.
fn write_snapshot_row(db: &Db, snap: &Snapshot) -> i64;          // store_snapshot + put + raw INSERT

/// Append a `type = 0` row with an encoded batch; return row id.
fn write_delta_row(db: &Db, batch: &[Delta]) -> i64;             // encode_delta_batch + raw INSERT
```

Raw `INSERT INTO journal (type, payload, command_id, applied_at) VALUES (...)`
is used because the journal **append** helper is Step 9. Replay ignores
`command_id` / `applied_at`, so the snapshot/replay helpers can pass any
constant; the **journal** read-helper tests (J-series) pass distinguishable
values so they can assert the new struct fields round-trip.

### Snapshot blob (`snapshot.rs`)

S1. **`snapshot_round_trips`** — a `Snapshot` with two tracks (track 0 with two
    label hashes, track 1 with three turn hashes, hashes with non-zero bytes)
    `store_snapshot` → `load_snapshot` is `PartialEq`-equal to the original.

S2. **`snapshot_empty_round_trips`** — `Snapshot { tracks: vec![] }` round-trips.
    Also a snapshot with a present-but-empty track (`(1, vec![])`).

S3. **`load_snapshot_rejects_wrong_kind`** — feed `load_snapshot` a
    `store_turn` blob ⇒ `Err(DecodeError::KindMismatch { expected: Snapshot, .. })`.

S4. **`load_snapshot_rejects_empty`** — `load_snapshot(&[])` ⇒
    `Err(DecodeError::Empty)`.

S5. **`load_snapshot_rejects_unknown_version`** — a blob with tag
    `(Kind::Snapshot, 2)` ⇒ `Err(DecodeError::UnknownVersion { .. })`.

S6. **`v1_wire_format_pinned`** — `store_snapshot(&sample_snapshot()).1` equals a
    hardcoded `&[u8]`. (`sample_snapshot()` is a fixed two-track snapshot with
    deterministic sentinel hashes.) Catches postcard rule changes / field
    reorders that round-trip can't see. **Mirrors the Turn / Label / Delta
    pinned tests.**

S7. **`v1_wire_hash_pinned`** — `store_snapshot(&sample_snapshot()).0` equals a
    hardcoded `Hash`. The content-addressing hash of the canonical encoding.

S8. **`capture_pinned_values`** (`#[ignore]`) — prints freshly captured
    `PINNED_WIRE_BYTES` / `PINNED_HASH` for `sample_snapshot()`, per the
    regeneration recipe in
    [phase1-m1-04 § Pinned-bytes regeneration workflow](phase1-m1-04.md#pinned-bytes-regeneration-workflow).

S9. **`v1_conversions_total_round_trip`** —
    `Snapshot::from(v1::SnapshotV1::from(&s)) == s`.

### Flatten (`snapshot.rs`)

F1. **`snapshot_from_trees_orders_by_track_id`** — build a `PerTrackTrees` with
    track 0 (labels) and tracks 1, 2 (turns); `snapshot_from_trees` produces
    `tracks` sorted ascending by `track_id`, each carrying that tree's hash
    sequence in timeline order.

F2. **`snapshot_from_trees_round_trips_through_build`** — flatten trees →
    `snapshot_from_trees` → for each `(track_id, seq)` call `build_track_tree`
    → assert the rebuilt `TrackTree` equals the original (`TrackTree`
    `PartialEq`). Pins flatten ∘ build = identity. (Needs the elements in
    `store`, so build the trees from `put_turn` / `put_label` hashes.)

F3. **`track_tree_hashes_matches_iter`** — `TrackTree::hashes()` equals the
    inner tree's `iter().map(|e| e.hash).collect()` for both variants.

### Replay — happy path (`snapshot.rs`)

R1. **`load_latest_snapshot_speech_track`** — store three turns, write a
    snapshot row listing them on track 1, no delta rows ⇒
    `load_latest_snapshot(db, None)` yields `(id, trees)` where `trees[&1]` is a
    `Speech` tree equal to `from_sorted_elements` of those three turns, and the
    returned id matches the row.

R2. **`load_latest_snapshot_labels_track`** — same for a snapshot whose **only**
    track is track 0 with labels ⇒ `trees` has a single `Labels` entry and no
    speech tracks. Pins the label-track-only project shape.

R3. **`load_and_replay_equals_snapshot_when_no_deltas`** —
    `load_and_replay(db, None) == load_latest_snapshot(db, None).trees` when no
    `type = 0` rows follow the snapshot.

R4. **`replay_inserts_after_snapshot_speech`** — snapshot `[t1]` on track 1;
    one `type = 0` row inserting `t2` after `t1`
    (`InsertAfter(After(h1), h2)`) ⇒ `load_and_replay` track 1 equals
    `from_sorted_elements([t1, t2])`. Construct the expected tree directly and
    assert with tree `PartialEq`.

R5. **`replay_full_op_mix_speech`** — snapshot `[t1, t2, t3]`; deltas across two
    rows: row A deletes after `t1` (removes `t2`) and updates after `t1`
    (replaces `t3`'s predecessor chain appropriately), row B inserts a new tail.
    Assert the final track 1 sequence matches a hand-computed expected. Mirrors
    delta.rs's `mixed_kinds_batch` but exercised through the journal.

R6. **`replay_labels_track`** — the R4 scenario on track 0 with labels, proving
    `load_label` dispatch for `track_id == 0`.

R7. **`replay_multiple_tracks_independent`** — snapshot with track 0 (labels)
    and track 1 (turns); a single delta row carrying deltas for **both** tracks
    ⇒ both rebuild correctly and independently. Pins per-track routing in
    `replay_into`.

R8. **`replay_preserves_untouched_tracks`** — snapshot with tracks 1 and 2;
    deltas touch only track 1 ⇒ track 2's tree in the result is sequence-equal
    to its snapshot tree (not rebuilt-but-different).

R9. **`replay_intra_batch_forward_reference`** — a single `type = 0` row whose
    second delta references a hash produced by its first
    (`InsertAfter(After(h1), h2)` then `InsertAfter(After(h2), h3)`) replays to
    `[h1, h2, h3]`. Pins that intra-batch order flows through the journal path.

R10. **`replay_track_born_from_deltas`** — snapshot lists only track 1; a
     `type = 0` row inserts elements on track 3 (not in the snapshot) ⇒ track 3
     appears in the result, built from `AdjacencyList::new()`. (Forward-looking;
     M1 has no command that does this, but the path must be correct.)

R11. **`replay_snapshot_with_empty_track`** — snapshot listing track 1 with three
     turns **and** track 2 with an empty hash vec (`(2, vec![])`), no deltas ⇒
     `load_latest_snapshot(db, None)` yields a `trees` map containing **both**
     tracks, with `trees[&2]` a present-but-empty `Speech` tree
     (`is_empty()`, `len() == 0`). Pins that a listed-but-empty track survives the
     `from_sequence([])` → `build_track_tree` path as an empty tree rather than
     being dropped. Add a track-0 variant (`(0, vec![])` ⇒ empty `Labels` tree).

R12. **`replay_empty_snapshot_yields_no_tracks`** — `Snapshot { tracks: vec![] }`
     (the `new_project` initial state), no deltas ⇒ `load_and_replay(db, None)`
     and `load_latest_snapshot(db, None).trees` are both an empty `PerTrackTrees`.
     Pins the freshly-created-project open path.

### Replay — point-in-history `as_of` (`snapshot.rs`)

AO1. **`replay_as_of_midway_between_deltas`** — snapshot `[t1]` on track 1, then
     three `type = 0` rows (ids d1 < d2 < d3) appending `t2`, `t3`, `t4`.
     `load_and_replay(db, Some(d2))` reconstructs `[t1, t2, t3]` (through d2
     inclusive), **not** `[t1, t2, t3, t4]`. Pins the inclusive replay bound and
     mid-run reconstruction.

AO2. **`load_latest_snapshot_as_of_picks_earlier_snapshot`** — two snapshot rows
     (id s1 lists `[t1]`, id s2 > s1 lists `[t1, t2]`) with `s1 < as_of < s2`.
     `load_latest_snapshot(db, Some(as_of))` returns the **s1** snapshot/`[t1]`,
     and the returned id is `s1` — not the absolute-latest `s2`.

AO3. **`load_and_replay_as_of_at_snapshot_row`** — layout: snapshot s1 `[t1]`,
     delta rows appending `t2` / `t3`, then a **second** snapshot row s2 `[t1, t2,
     t3]` (id s2 > those deltas). `load_and_replay(db, Some(s2))` selects s2
     (the latest snapshot at/before `as_of`), so `snapshot_id == as_of` and
     `replay_into` sees an **empty** delta range (none with `s2 < id <= s2`) —
     the result equals `load_latest_snapshot(db, Some(s2)).trees`, *not* a replay
     from s1. Pins "if `as_of` names a snapshot, load it directly, replay nothing,"
     even when earlier deltas exist.

AO4. **`load_and_replay_as_of_none_is_full_history`** — `load_and_replay(db,
     None)` equals `load_and_replay(db, Some(max_journal_id))`. Pins `None` ⇒
     end-of-journal.

AO5. **`load_latest_snapshot_as_of_before_first_snapshot_errors`** — `as_of`
     smaller than the first snapshot row's id ⇒ `Err(ReplayError::NoSnapshot)`.

AO6. **`as_of_zero_and_negative_yield_no_snapshot`** — `load_latest_snapshot(db,
     Some(0))`, `Some(-1)`, and `load_and_replay` with the same ⇒
     `Err(ReplayError::NoSnapshot)` (journal ids are `AUTOINCREMENT`-positive, so
     nothing satisfies `id <= 0`). Guards the `id <= as_of` bind against an
     off-by-one or sign mishandling distinct from AO5's "before first snapshot."

### Replay — recovery primitive + errors (`snapshot.rs`)

E1. **`load_latest_snapshot_ignores_later_deltas`** — snapshot `[t1]` on track 1
     followed by a delta row inserting `t2`. `load_latest_snapshot(db, None)`
     returns the **pre-delta** tree (`[t1]`), proving it never reads `type = 0`
     rows. **This is the recovery-primitive pin** called for in phase1-m1.md Step 8.

E2. **`load_latest_snapshot_no_snapshot_row_errors`** — a fresh `Db` with no
     `type = 1` row ⇒ `load_latest_snapshot(db, None)` is
     `Err(ReplayError::NoSnapshot)`.

E3. **`load_latest_snapshot_picks_most_recent`** — two snapshot rows (older
     lists `[t1]`, newer lists `[t1, t2]`) ⇒ `load_latest_snapshot(db, None)`
     uses the higher-id row.

E4. **`replay_malformed_delta_payload_carries_row_id`** — a `type = 0` row whose
     payload is `[0xFF, ...]` (unknown delta version) ⇒
     `Err(ReplayError::DeltaDecode { row_id, .. })` with `row_id` equal to that
     row's id. Pins the engine's "name the failed row" requirement.

E5. **`replay_unapplicable_delta_carries_row_id`** — a `type = 0` row with a
     delta whose `Location::After(h_unknown)` is not in the seeded list ⇒
     `Err(ReplayError::DeltaApply { row_id, source: DeltaError::LocationNotFound(h_unknown) })`.

E6. **`replay_missing_element_blob_errors`** — snapshot references a hash that
     was never `put` into `store` ⇒ both `load_latest_snapshot(db, None)` and
     `load_and_replay(db, None)` return
     `Err(ReplayError::Store(StoreError::NotFound(_)))`.

E7. **`replay_corrupt_snapshot_payload_errors`** — a `type = 1` row whose payload
     BLOB is not 16 bytes ⇒ `Err(ReplayError::Journal(JournalError::MalformedHashPayload { .. }))`
     (surfaced through `latest_snapshot`).

E8. **`replay_error_display_and_source`** — each `ReplayError` variant's
     `Display` is non-empty; the three wrapper variants (`Store`, `Decode`,
     `Journal`) chain their inner error via `source()`. Mirrors
     `store.rs`'s `store_error_*` tests.

### Journal read helpers (`db/journal.rs`)

J1. **`latest_snapshot_none_on_empty_journal`** — `latest_snapshot(conn, None)`
    on a fresh DB ⇒ `Ok(None)`.

J2. **`latest_snapshot_returns_highest_id`** — insert two `type = 1` rows and a
    `type = 0` row between them; `latest_snapshot(conn, None)` returns the
    highest-id `type = 1` row's id + decoded hash, ignoring the `type = 0` row.

J3. **`latest_snapshot_rejects_non_16_byte_payload`** — a `type = 1` row with a
    15-byte payload ⇒ `Err(JournalError::MalformedHashPayload { id, len: 15 })`.

J4. **`latest_snapshot_as_of_bounds`** — two `type = 1` rows at ids s1 < s2;
    `latest_snapshot(conn, Some(s2 - 1))` returns s1, `Some(s2)` returns s2,
    `Some(s1 - 1)` returns `None`, and `None` returns s2. Pins the inclusive
    `id <= as_of` selection.

J5. **`latest_snapshot_surfaces_command_id_and_applied_at`** — write a snapshot
    row with distinguishable `command_id` / `applied_at` ⇒ the returned
    `SnapshotRow` carries both verbatim.

J6. **`deltas_after_returns_ascending_subset`** — rows: snapshot (id S), three
    `type = 0` rows (ids > S), one `type = 0` row with id < S (impossible in
    practice but pins the filter), and a `type = -1` row ⇒
    `deltas_after(conn, S, None)` returns exactly the three `id > S` `type = 0`
    rows in ascending id order, excluding the `type = -1` and the `type = 0` row
    at id < S. Also assert each `DeltaRow`'s `command_id` / `applied_at` match
    what was written.

J7. **`deltas_after_until_bounds`** — `type = 0` rows at ids d1 < d2 < d3 after
    snapshot S; `deltas_after(conn, S, Some(d2))` returns `[d1, d2]` (inclusive
    upper bound), `Some(d3)` returns all three, `None` returns all three. Also
    assert `deltas_after(conn, S, Some(S - 1))` (i.e. `until < snapshot_id`)
    returns `Ok(vec![])` — the predicate is unsatisfiable, which is what makes a
    hypothetical `replay_into` with `as_of < snapshot_id` a safe no-op (see the
    `replay_into` invariant).

J8. **`deltas_after_empty_when_none_follow`** — only a snapshot row, no later
    `type = 0` rows ⇒ `deltas_after(conn, S, None)` is `Ok(vec![])`.

### Out-of-scope tests (covered elsewhere or later)

- `AdjacencyList` mechanics (insert/update/delete/walk): [Step 7](phase1-m1-07.md).
- Journal **append** (`type = -1/0/1` row writes via the canonical helper) and
  metadata read/write: Step 9.
- Undo/redo journaling of inverse rows: Step 10.
- Engine open/save lifecycle, the corrupt-journal *fallback decision*, and the
  background snapshot writer: Step 11. (Step 8 ships the primitives that decision
  is built on and proves they report precisely; it does not decide
  recover-vs-refuse.)
- Tauri command wiring: Step 12.
- The committed G1 `.vocalboard` fixture round-trip: Step 13 (which exercises a
  real on-disk snapshot blob through `load_snapshot` by construction).

## Documentation touches

- **`data-model.md`** — no changes required. The
  [§ Snapshot blob](../design/data-model.md#snapshot-blob) and
  [§ Load / replay](../design/data-model.md#load--replay) sections already describe the
  `Snapshot` struct and the four-step replay this plan implements
  field-for-field and step-for-step.
- **`phase1-m1.md` Step 8 bullet** — add the cross-reference line (matching the
  Step 3/4/5/7 pattern):
  > See [phase1-m1-08.md](phase1-m1-08.md) for the detailed action plan.
  and note that Step 8 also introduces `db/journal.rs` with the **read-side**
  helpers (`latest_snapshot` + `deltas_after`) that replay consumes.
- **`phase1-m1.md` Step 9 bullet** — adjust the journal sub-bullet to "append
  `type −1 / 0 / 1` rows … the latest-snapshot lookup and deltas-after-id scan
  landed in Step 8's `db/journal.rs`; Step 9 adds the append/write side and the
  metadata plumbing to the same module." Also correct the Step 9 cleanup
  sub-bullet: the first **non-test** caller of `store::get` / `Db::conn` (and
  `store::put`) is the **Step 11 engine**, not Step 9 — replay and metadata are
  both `pub(crate)` and test-only-reachable until the engine wires them, so the
  `#[allow(dead_code)]` cleanup moves to Step 11.
- **`phase1-m1.md` module-layout comment** — annotate the `journal.rs` line that
  reads (`latest_snapshot` / `deltas_after`) land in Step 8 and append in Step 9.
- **`conventions.md`** — no changes. The G1 invariant is satisfied in-step by the
  pinned snapshot wire tests (S6/S7) plus the Step 13 fixture.

## Out of scope for Step 8

- **The journal append/write path** — `db/journal.rs` ships read helpers only;
  row insertion is Step 9. Step 8 tests insert rows with raw SQL.
- **Metadata (`type = -1`) loading** — separate from replay
  ([data-model.md § Non-timeline data](../design/data-model.md#non-timeline-data));
  Step 9.
- **The engine's recover-vs-refuse decision and the recoverable-error surface to
  the UI** — Step 11. Step 8 provides the two independent load primitives and
  the row-id-carrying errors that decision consumes.
- **The background snapshot writer / threading** — Step 11. `snapshot_from_trees`
  + `store_snapshot` are pure; the writer that calls them on a frozen clone is
  the engine's job.
- **The project-history / time-travel UI** — M5+. Step 8 ships the `as_of`
  parameter and the bounded journal queries the feature stands on, plus the
  `command_id` / `applied_at` fields it will read; it does **not** build the
  history view, a unified all-types `JournalEntry` query, or any UI.
- **Any V2 of `SnapshotV1`** — one dispatch arm; V2 follows the Turn/Label recipe
  if the shape ever changes.

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` — must stay green with `unwrap_used`,
  `expect_used`, `panic`, and `missing_docs` all CI-gated. New `pub(crate)`
  plumbing carries `#[allow(dead_code)]` per the visibility decision; all `pub`
  items carry doc-comments.
- `cargo test -p core snapshot::` and `cargo test -p core journal::` — the
  tests above.
- `cargo test -p core` — confirms no regression from the new `pub mod snapshot;`
  / `mod journal;` lines.
- Manual diff review of `snapshot.rs` against
  [data-model.md § Load / replay](../design/data-model.md#load--replay) for step-for-step
  correspondence, and of the `Snapshot` struct against
  [§ Snapshot blob](../design/data-model.md#snapshot-blob).
- One commit on `claude/1M1`, **unsigned** per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md). Suggested subject:
  `1M1-08: snapshot blob + replay + journal read side`. Bundles
  `core/src/project/snapshot.rs`, `core/src/db/journal.rs`, the `pub mod
  snapshot;` line in `project/mod.rs`, the `mod journal;` line in `db/mod.rs`,
  and (per the pre-commit checklist) the `phase1-m1.md` Step 8 / Step 9 /
  module-layout edits above.

## Downstream implications (flag for later steps)

- **Step 9 (`db/journal.rs` + `metadata.rs`):** extends the journal module
  created here with the append side (`append(tx, type, command_id, payload)`)
  for all three row types, reusing `SnapshotRow` / `DeltaRow` / `JournalError`
  as the read side. The metadata `type = -1` read (most-recent-wins) is a third
  query alongside `latest_snapshot` / `deltas_after`.
- **Step 11 (`engine.rs`):** `open_project` calls `load_and_replay(db, None)`
  (M1 always loads to the end of the journal); on `Err`, it falls back to
  `load_latest_snapshot(db, None)` (fatal if *that* errors) and surfaces a
  recoverable error built from the `ReplayError`'s `row_id` (forward path) and
  the snapshot id (from `load_latest_snapshot`'s return). The future history
  feature reuses the same primitives with a non-`None` `as_of`. `new_project`
  writes an initial empty `Snapshot { tracks: vec![] }` via `snapshot_from_trees`
  on empty trees + `store_snapshot` + a `type = 1` append, guaranteeing
  `load_latest_snapshot` never hits `NoSnapshot` on a well-formed file.
  `save_snapshot_now` clones the trees (O(1) per track), then off-thread calls
  `snapshot_from_trees` → `store_snapshot` → `store::put` → `type = 1` append.
  This is where the `#[allow(dead_code)]` on the Step 8 plumbing, `store::get`,
  and `Db::conn` is finally removed.
- **Step 13 (G1 fixture):** the committed `.vocalboard` file contains a real
  `Kind::Snapshot` blob; opening it exercises `load_snapshot` + the full replay
  path, completing the G1 round-trip for the snapshot format on top of the in-step
  pinned-bytes tests.
