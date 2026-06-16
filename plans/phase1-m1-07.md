# Phase 1 · M1 · Step 7 — Deltas (`project/delta.rs`) (action plan)

Per-step action plan for Step 7 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md § Deltas](../design/data-model.md#deltas) and
[§ Load / replay](../design/data-model.md#load--replay). This step lays down the
**delta language** — the three-operation, kind-agnostic edit primitive that
carries the project's evolution between snapshots — and the **working
adjacency list** that replay and forward-edit code drive deltas against.
It is the first step to introduce a wire format that lives in the
**journal** (vs. the content-addressed store), so it also lays down the
parallel-to-Turn/Label V_N dispatch shim for journal-payload versioning.

**Definition of done:** `core/src/project/delta.rs` exposes `Delta`,
`DeltaOp`, `Location`, `DeltaError`, `AdjacencyList`, an `apply`
operation, and the `LATEST_DELTA_VERSION` + `encode_delta_batch` /
`decode_delta_batch` journal-payload helpers (with a `mod v1` frozen
wire schema mirroring the Turn / Label pattern). Module is
re-exported from `project/mod.rs`. Full unit coverage (apply per
variant, intra-batch hash forwarding, error variants, pinned wire
format, pinned wire hash). `cargo test -p core delta::`, `cargo
clippy -p core -- -D warnings`, and `cargo fmt --check` are all
green.

`delta.rs` ships **no inverse-computation API.** The engine (Step 11)
mutates the in-memory tree directly via its O(log n) primitives from
[Step 6](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers),
already holds the `h_old` / `h_removed` it needs to capture the
inverse at the edit site, and emits the forward+inverse pair as a
side effect of the edit by calling the right `Delta::{insert,update,
delete}_after` constructor with the right hash. The "inversion rules"
in [data-model.md § Undo / redo](../design/data-model.md#undo--redo) are thus
documented behaviour at the engine layer, not code in `delta.rs`. The
`AdjacencyList` exists **purely as the replay-side intermediate**
(Step 8): snapshot → seed adjacency → apply forward delta batches in
journal order → walk to ordered `Vec<Hash>` → bulk-build the tree.

## Context

[Step 4](phase1-m1-04.md) shipped the kinded element types (`Turn`,
`Label`) that sit in the store and whose hashes flow through deltas.
[Step 5](phase1-m1-05.md) shipped the blob-store plumbing those hashes
ultimately address. [Step 6](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers)
shipped the immutable `ImplicitTimelineTree<T: Tilable>` whose nodes
carry `(hash, Arc<T>)` pairs.

Step 7 sits **between** the on-disk journal and the in-memory tree:

- **Toward the journal (writes):** the engine (Step 11) produces a
  `Vec<Delta>` per command, serializes it via this module's
  `encode_delta_batch`, and writes it as a `type = 0` journal row
  (Step 9). The engine builds the `Vec<Delta>` by mutating the tree
  through its own primitives; nothing in `delta.rs` participates in
  that path beyond providing the type constructors.
- **Toward the tree (reads):** replay (Step 8) deserializes a
  `Vec<Delta>` via this module's `decode_delta_batch`, applies it to
  an `AdjacencyList` seeded from the snapshot, walks the result to an
  ordered `Vec<Hash>`, and bulk-builds the tree via
  `ImplicitTimelineTree::from_sorted_elements`. **This is the only
  caller of `apply` and the only consumer of `AdjacencyList`.**
- **Toward undo (no involvement):** the engine captures inverses at
  edit time directly from the tree (which holds `h_old` /
  `h_removed`) and pushes an `UndoEntry` (the before/after
  `Arc<TimelineState>` snapshots plus the forward and inverse delta
  batches) onto the in-memory undo stack (Step 10). `delta.rs` provides
  only the type constructors the engine uses to build the inverse
  `Delta` values; it owns no inversion code path.

This step ships nothing kind-aware: a delta carries a `Hash` and a
`track_id`; the per-kind dispatch (Turn vs. Label) is purely a
**load-time** concern that Step 8 handles when walking the resulting
hash sequence. Keeping the delta language kind-agnostic is what makes
the same `apply` work for both speech tracks and track 0.

## Decisions locked in this step

- **Flat `Delta` struct with `op: DeltaOp` + `hash: Option<Hash>`** —
  not a sum-variant `enum Delta`. This matches the spec shape in
  [data-model.md § Deltas](../design/data-model.md#deltas) field-for-field:

  ```rust
  pub struct Delta {
      pub track_id: u32,
      pub op:       DeltaOp,
      pub location: Location,
      pub hash:     Option<Hash>,   // None for DeleteAfter
  }
  ```

  A sum variant would be a small ergonomics win at the cost of
  re-shaping the wire format; the spec is the authority. The flat
  struct's "hash is None iff op is DeleteAfter" invariant is enforced
  by a constructor (`Delta::insert_after` / `update_after` /
  `delete_after`) and `debug_assert!`s in `apply`; tests pin both
  directions.

- **`Location` is `Start | After(Hash)`, no `End`.** Three reasons:
  (1) `InsertAfter` against the last element is `After(<last hash>)`,
  not `End`; (2) `End` would require the engine to look up the
  current last hash on every append, which the tree already provides
  in O(log n) via predecessor/successor; (3) the data-model.md grammar
  is explicit — only `Start` and `After(Hash)`.

- **Adjacency list keys are `Location`, values are `Option<Hash>`** —
  `Some(next)` for a non-terminal successor, `None` for the terminal
  edge. The map is `HashMap<Location, Option<Hash>>`. **Every legal
  location has an entry**, including the terminal (whose value is
  `None`); the empty list is `{ Start: None }`. Two clean
  consequences:

  - **Single, uniform validation rule:** "location `L` is legal iff
    `L` is a key in `edges`." Applies identically to `Start` and
    `After(h)`. O(1) lookup. No "Start is implicitly always valid"
    convention to remember; no `elements: HashSet<Hash>` side index;
    no O(n) `values().any(…)` scan.
  - **Terminal vs. invalid are distinguishable in `successor`:** the
    method returns `Option<Option<Hash>>` — outer `None` means "you
    passed an invalid location," inner `None` means "this location
    is the terminal end of the track." Useful for diagnostics.

  `Location` derives `Eq + Hash` from `Hash`'s existing derives — no
  `Ord` needed, so no new derive on `Hash`. `HashMap` is the natural
  choice here because the adjacency list is **ephemeral** (not
  persisted, not content-addressed) and the determinism invariant in
  [CLAUDE.md](../CLAUDE.md) (and [conventions.md](../design/conventions.md))
  applies only to hashed structs. Walks always go from `Start` and
  use lookup, never iteration over the map. Per-track storage cost
  is one extra entry (the trailing `→ None`) and 17-byte values
  instead of 16 — both negligible.

- **`AdjacencyList` is replay-only; forward edits never touch it.**
  The engine (Step 11) drives forward edits through the tree's O(log
  n) `insert_at` / `update_at` / `delete_at` primitives from
  [Step 6](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers).
  At the edit site the engine already holds the node it's mutating,
  so it knows `h_old` (for `UpdateAfter`) and `h_removed` (for
  `DeleteAfter`) directly — and emits the forward+inverse pair as a
  side effect of the edit by calling
  `Delta::{insert,update,delete}_after` with the right hashes.
  Round-tripping the tree through an adjacency list to obtain the
  same information would throw away structural sharing, allocate an
  O(n) HashMap per edit, and rebuild the tree from scratch via
  `from_sorted_elements`. The only path that uses `AdjacencyList` is
  replay (Step 8), where the snapshot is already a flat `Vec<Hash>`
  and the tree doesn't exist yet — so the adjacency list is the
  natural fit.

- **`apply` mutates in place; no `apply_with_inverse` ships.** Replay
  doesn't need inverses (it's pure reconstruction; the undo stack
  starts empty after open per
  [data-model.md § Undo / redo](../design/data-model.md#undo--redo)), and
  forward edits don't go through this code path at all (see prior
  decision). An inverse-batch helper in `delta.rs` would have no
  caller in M1.

- **Errors abort replay; the adjacency list may be left in a partial
  state.** A malformed delta encountered during replay aborts the
  forward-replay step — the engine (Step 11) catches the error and
  falls back to the snapshot-only load path (Step 8's
  `load_latest_snapshot`), so the partially-mutated `AdjacencyList`
  is dropped without consequence. No rollback or partial-application
  contract is documented for `apply` itself because the recovery
  lives at the engine layer.

- **All three operations error if `location` is not a key in the
  adjacency list.** Per the representation decision above, this is
  the *uniform* rule — `Start` is always a key (it's seeded on
  construction), and `After(h)` is a key iff `h` is currently an
  element of the track. `Update`/`Delete` additionally require the
  *value* at `loc` to be `Some(_)` — the inner `None` is exactly
  what cleanly means "no successor." `Insert` accepts either
  `Some(_)` or `None` as the current value (inserting at a terminal
  location is legal and produces a new tail; the prior terminal
  `None` flows into the new element's outgoing edge). Errors are
  typed (`DeltaError`) rather than `anyhow` so tests can `matches!`
  them and the engine's user-facing error mapping (M6) can render
  them.

- **`LATEST_DELTA_VERSION: u8 = 1`** lives in `delta.rs`, paralleling
  `LATEST_TURN_VERSION` in `turn.rs` and `LATEST_LABEL_VERSION` in
  `label.rs`. `encode_delta_batch` always writes `0x01` then the
  postcard payload; `decode_delta_batch` peeks the first byte and
  dispatches. M1 has one arm. Other versions return
  `DecodeBatchError::UnknownVersion`. An empty input returns
  `DecodeBatchError::Empty`. This mirrors the Step 4 Turn / Label
  dispatch architecture.

- **`mod v1::DeltaV1` is a frozen wire struct identical-in-shape to
  in-memory `Delta`** (with `DeltaOpV1` and `LocationV1` twins),
  with explicit `From<v1::DeltaV1> for Delta` and `From<&Delta> for
  v1::DeltaV1` total identity-shaped conversions. Same pre-1.0
  escape hatch as Turn V1 / Label V1: the V1 wire format MAY be
  revised pre-release with a regenerated pinned-bytes constant and a
  `min_app_version` bump; post-release V1 is frozen indefinitely. The
  V1 shim is **not** a no-op: it's what lets future Delta-shape
  evolution (a fourth `DeltaOp`, a richer `Location` variant) ship
  V2 without breaking V1 read paths.

- **Pinned wire format + pinned hash tests, paralleling Turn / Label.**
  A `sample_v1_batch()` helper builds a small `Vec<v1::DeltaV1>`
  covering all three `DeltaOp` variants and both `Location` variants;
  `v1_wire_format_pinned` asserts the `encode_delta_batch` output
  matches a hardcoded `&[u8]`; `v1_wire_hash_pinned` asserts
  `hash_tagged(&encoded)` matches a hardcoded `Hash`. The hash test
  pins a wire-level fingerprint that round-trip alone cannot catch
  (postcard rule changes, enum-variant reorderings, leading-byte
  reshuffles).

  Note: `hash_tagged` is *not* semantically meaningful for journal
  payloads (deltas are not stored in the content-addressed store
  and are not referenced by hash anywhere); the pinned hash is
  purely a stable byte-level fingerprint of the canonical encoding,
  reusing the existing BLAKE3-128 primitive instead of introducing
  a new digest API. The comment on the test calls this out.

- **A `capture_pinned_values` `#[ignore]` test** prints the freshly
  captured bytes / hash for the current `sample_v1_batch()`, matching
  the Turn / Label workflow. Future shape changes (pre-1.0) follow
  the same regeneration recipe documented in
  [phase1-m1-04.md § Pinned-bytes regeneration workflow](phase1-m1-04.md#pinned-bytes-regeneration-workflow).

- **`AdjacencyList` exposes `from_sequence`, `iter`, `head`,
  `successor`, and `len`** (the small constructive / inspection
  surface needed by Step 8's replay). Building from a snapshot's
  `Vec<Hash>` is `AdjacencyList::from_sequence(hashes)`; walking
  Start → end is `adj.iter()`. Materialising back to `Vec<Hash>` is
  `adj.iter().collect()`. Step 8 owns the snapshot ↔ tree glue;
  Step 7 owns the underlying `AdjacencyList` type and its API.

- **`pub(crate)` for `apply`, `encode_delta_batch`,
  `decode_delta_batch`, `AdjacencyList`, and the journal payload
  helpers.** These are internal engine plumbing — no external caller
  (frontend, Phase 6 scripting) should reach past `ProjectState` to
  drive deltas directly. `Delta`, `DeltaOp`, `Location`,
  `DeltaError`, and `DecodeBatchError` are `pub` because they show up
  in `pub(crate)` signatures (and because `Delta` construction is
  what the engine's edit code consumes from this module) — Rust
  requires the components of a function signature to be at least as
  visible as the function itself, but the engine wrappers in Step 11
  will re-export the construction surface as needed.

## Tag-byte layout — N/A here

Deltas are **journal-payload-resident**, not store-resident. They
carry no tag byte (the kind+version nibble layout from
[Step 3](phase1-m1-03.md) applies to `store` blobs). Their wire
format starts with `LATEST_DELTA_VERSION: u8 = 0x01` followed by the
postcard-serialized batch. The `type = 0` row's `command_id` column
(set by Step 9) names the command that produced the batch; the
version byte names the wire shape of the batch itself.

## Module surface

### New: `core/src/project/delta.rs`

```rust
//! Deltas: the journal-resident, kind-agnostic edit primitive.
//!
//! A delta names an edit site by the element that *precedes* it
//! (`Location::Start` or `Location::After(Hash)`) and one of three
//! ops (insert / update / delete). Replay (Step 8) builds an
//! `AdjacencyList` — a `HashMap<Location, Option<Hash>>` modelling
//! the "next element after location" edge set (with the terminal
//! end represented as a `Location → None` entry so every legal
//! location is a key) — from the latest snapshot and applies each
//! subsequent `type = 0` row's batch to it before walking the result
//! back to an ordered hash sequence. Forward
//! edits (Step 11) skip the adjacency list entirely: the engine
//! mutates the in-memory tree directly through its O(log n)
//! primitives and emits the forward+inverse `Delta` pair at the
//! edit site, where it already holds `h_old` / `h_removed`.
//!
//! Delta payloads sit in `journal.payload` for `type = 0` rows, with
//! a leading `delta_version: u8` byte (M1 writes `0x01`) followed by
//! the postcard-serialized `Vec<Delta>`. See
//! [data-model.md § Deltas](../../../design/data-model.md#deltas).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::hash::Hash;

/// Wire version of the journal `type = 0` payload (`delta_version`
/// byte). Written by [`encode_delta_batch`]; recognised by
/// [`decode_delta_batch`].
pub(crate) const LATEST_DELTA_VERSION: u8 = 1;

/// One recorded edit to a track's element sequence.
///
/// `hash` is `None` iff `op == DeltaOp::DeleteAfter`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// Track this edit applies to. `0` = labels track.
    pub track_id: u32,
    /// Edit kind.
    pub op:       DeltaOp,
    /// Element that *precedes* the edit site.
    pub location: Location,
    /// New / replacing element hash. `None` for `DeleteAfter`.
    pub hash:     Option<Hash>,
}

/// Edit-kind tag for [`Delta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaOp {
    /// Insert the new element immediately after `location`.
    InsertAfter,
    /// Replace the element immediately after `location` with the new one.
    UpdateAfter,
    /// Remove the element immediately after `location`.
    DeleteAfter,
}

/// Position identifier for an edit site.
///
/// Always names the element that *precedes* the site (never the site
/// itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Location {
    /// The head of the track. Always a legal location.
    Start,
    /// Immediately after the element with this hash.
    After(Hash),
}

impl Delta {
    /// Construct an `InsertAfter` delta.
    pub fn insert_after(track_id: u32, location: Location, hash: Hash) -> Self;
    /// Construct an `UpdateAfter` delta.
    pub fn update_after(track_id: u32, location: Location, hash: Hash) -> Self;
    /// Construct a `DeleteAfter` delta.
    pub fn delete_after(track_id: u32, location: Location) -> Self;
}

/// Errors returned by delta application.
#[derive(Debug, PartialEq, Eq)]
pub enum DeltaError {
    /// `Location::After(h)` named an element not present in the
    /// adjacency list.
    LocationNotFound(Hash),
    /// `Update` / `Delete` at a location with no successor.
    NoSuccessor(Location),
    /// `hash` field's None-iff-Delete invariant was violated.
    /// (Debug builds catch this via `debug_assert!`; release-mode
    /// callers see this variant.)
    HashFieldMismatch { op: DeltaOp, hash_present: bool },
}

/// Errors returned by [`decode_delta_batch`].
#[derive(Debug)]
pub enum DecodeBatchError {
    /// Empty payload.
    Empty,
    /// Leading version byte is not recognised by this build.
    UnknownVersion(u8),
    /// The postcard body failed to deserialize.
    Postcard(postcard::Error),
}

/// Working "next element after location" edge set used by replay.
///
/// Invariant: every legal location is a key. The empty list is
/// `{ Start: None }`. The terminal end of a non-empty track is the
/// location whose value is `None`. Inserting/deleting maintains the
/// invariant — each operation simultaneously adds the new edges
/// (Start → first, After(h) → next) and removes the now-stale ones.
pub(crate) struct AdjacencyList {
    edges: HashMap<Location, Option<Hash>>,
}

impl AdjacencyList {
    /// Build an empty list. Seeds `{ Start: None }` so `Start` is
    /// a legal location even on the empty track.
    pub(crate) fn new() -> Self;

    /// Build from an ordered hash sequence (e.g. a snapshot's
    /// `Vec<Hash>`). The first hash becomes `Start`'s successor;
    /// each subsequent hash becomes the previous one's successor;
    /// the last element's `After(h)` entry is seeded with `None`
    /// (terminal).
    pub(crate) fn from_sequence<I: IntoIterator<Item = Hash>>(seq: I) -> Self;

    /// Number of elements in the track. Computed as `edges.len() - 1`
    /// (every track has exactly one trailing terminal entry beyond
    /// its element count).
    pub(crate) fn len(&self) -> usize;

    /// True if the track has no elements (only the seeded
    /// `Start → None` entry is present).
    pub(crate) fn is_empty(&self) -> bool;

    /// First element's hash, or `None` for an empty track.
    /// (Equivalent to `successor(&Start).flatten()`.)
    pub(crate) fn head(&self) -> Option<Hash>;

    /// Two-layer Option:
    /// - `None` ⇒ `loc` is not a legal location in this list.
    /// - `Some(None)` ⇒ `loc` is legal and is the terminal end.
    /// - `Some(Some(h))` ⇒ `loc`'s successor is `h`.
    pub(crate) fn successor(&self, loc: &Location) -> Option<Option<Hash>>;

    /// Walk `Start → … → terminal`, yielding each element's hash
    /// in order. Stops when the current location's value is `None`.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Hash> + '_;
}

/// Apply each delta in `batch` to `adj`, in order. Stops on the
/// first error, leaving `adj` in a partial state — the only caller
/// is replay (Step 8), where any error is fatal to the open and the
/// abandoned adjacency list is dropped.
pub(crate) fn apply(
    adj: &mut AdjacencyList,
    batch: &[Delta],
) -> Result<(), DeltaError>;

/// Encode a delta batch for the `journal.payload` column of a
/// `type = 0` row: `LATEST_DELTA_VERSION` byte followed by
/// `postcard::to_stdvec(batch)`.
pub(crate) fn encode_delta_batch(
    batch: &[Delta],
) -> Result<Vec<u8>, postcard::Error>;

/// Decode a delta batch from a `type = 0` row's `journal.payload`.
/// Peeks the leading version byte and dispatches to the matching
/// per-version decoder. M1 has one arm (V1).
pub(crate) fn decode_delta_batch(
    bytes: &[u8],
) -> Result<Vec<Delta>, DecodeBatchError>;

pub mod v1 {
    //! Frozen V1 wire schema. Pre-1.0 escape hatch documented in
    //! [phase1-m1-04 § Decisions locked](phase1-m1-04.md#decisions-locked-in-this-step).

    use serde::{Deserialize, Serialize};

    use super::super::hash::Hash;

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DeltaV1 {
        pub track_id: u32,
        pub op:       DeltaOpV1,
        pub location: LocationV1,
        pub hash:     Option<Hash>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum DeltaOpV1 { InsertAfter, UpdateAfter, DeleteAfter }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum LocationV1 { Start, After(Hash) }
}

// Total identity-shaped From impls between Delta / DeltaOp / Location
// and their V1 twins, both directions.
```

### Implementation sketch — apply

```rust
fn apply_one(
    adj: &mut AdjacencyList,
    d: &Delta,
) -> Result<(), DeltaError> {
    // Validate `hash` field against `op`.
    match (d.op, d.hash.is_some()) {
        (DeltaOp::DeleteAfter, false) | (DeltaOp::InsertAfter, true)
        | (DeltaOp::UpdateAfter, true) => {}
        (op, hash_present) => {
            return Err(DeltaError::HashFieldMismatch { op, hash_present });
        }
    }
    // Validate the location is a key. Uniform rule, applies to Start
    // and After(_); After(h) being a key iff h is an element of the
    // track. Start is always a key by construction.
    if !adj.edges.contains_key(&d.location) {
        let missing = match d.location {
            Location::After(h) => h,
            // Unreachable: AdjacencyList::new and from_sequence always
            // seed Location::Start, and no operation removes its key.
            Location::Start    => unreachable!("Start is always a key"),
        };
        return Err(DeltaError::LocationNotFound(missing));
    }

    match d.op {
        DeltaOp::InsertAfter => {
            let new_h = d.hash.expect("validated above");
            // The old value at `loc` becomes the new element's
            // outgoing edge — Some(next) gracefully if `loc` had a
            // successor, None gracefully if `loc` was terminal.
            let prev_next = adj.edges.insert(d.location, Some(new_h))
                .expect("validated as a key above");
            adj.edges.insert(Location::After(new_h), prev_next);
            Ok(())
        }
        DeltaOp::UpdateAfter => {
            let new_h = d.hash.expect("validated above");
            // Inner None at `loc` means "no successor to update."
            let old_h = adj.edges.get(&d.location)
                .copied()
                .expect("validated as a key above")
                .ok_or(DeltaError::NoSuccessor(d.location))?;
            let next_after_old = adj.edges.remove(&Location::After(old_h))
                .expect("After(old_h) is a key whenever old_h is an element");
            adj.edges.insert(d.location, Some(new_h));
            adj.edges.insert(Location::After(new_h), next_after_old);
            Ok(())
        }
        DeltaOp::DeleteAfter => {
            let old_h = adj.edges.get(&d.location)
                .copied()
                .expect("validated as a key above")
                .ok_or(DeltaError::NoSuccessor(d.location))?;
            let next_after_old = adj.edges.remove(&Location::After(old_h))
                .expect("After(old_h) is a key whenever old_h is an element");
            adj.edges.insert(d.location, next_after_old);
            Ok(())
        }
    }
}
```

### Revised: `core/src/project/mod.rs`

Add `pub mod delta;` (sibling to the existing `pub mod tree;`,
`pub mod turn;`, `pub mod label;`, `pub mod hash;`,
`pub mod tilable;`).

### Reuse from existing code

- `hash::Hash` ([`core/src/project/hash.rs`](../src-tauri/core/src/project/hash.rs))
  — the 16-byte content hash carried in every `Location::After(_)`
  and every `Delta.hash`.
- `serde` + `postcard` — already in
  [`core/Cargo.toml`](../src-tauri/core/Cargo.toml); used the same
  way as `turn.rs` (derive `Serialize` / `Deserialize`; serialize via
  `postcard::to_stdvec`; deserialize via `postcard::from_bytes`).
- `std::collections::HashMap` — for the adjacency list's edge table;
  see the determinism-invariant decision above.
- No new dependencies. No schema changes. No additions to `hash.rs`.

## Test plan

All tests inline `#[cfg(test)] mod tests` in `delta.rs`. Most operate
on hand-constructed `AdjacencyList`s built via `from_sequence` so the
suite doesn't need to call `store_turn` to obtain hashes.

### Test helpers

```rust
fn h(byte: u8) -> Hash {
    let mut bytes = [0u8; 16];
    bytes[0] = byte;
    Hash(bytes)
}

fn seq(adj: &AdjacencyList) -> Vec<Hash> {
    adj.iter().collect()
}
```

(The byte-indexed sentinel hashes — `h(1)`, `h(2)`, … — let tests
read like ordered-list assertions without the noise of full
hex literals. They are *not* real BLAKE3 outputs; the delta module
never re-hashes them, so the test's correctness doesn't depend on
the bytes being canonical.)

### `AdjacencyList` construction & queries

A1. **`empty_list_walks_to_empty_vec`** — `AdjacencyList::new()`
    produces `seq(_) == []`, `is_empty()` is true, `len() == 0`,
    `head()` is `None`.

A2. **`from_sequence_round_trips`** — `from_sequence([h(1),
    h(2), h(3)])` produces `seq(_) == [h(1), h(2), h(3)]`,
    `len() == 3`, `head() == Some(h(1))`.

A3. **`from_sequence_empty`** — `from_sequence([])` ≡
    `AdjacencyList::new()`.

A4. **`successor_at_start_returns_head`** — for a 3-element list,
    `adj.successor(&Location::Start) == Some(Some(h(1)))` and the
    inner value matches `adj.head()`. For an empty list,
    `adj.successor(&Location::Start) == Some(None)` (Start is
    legal, terminal).

A5. **`successor_at_terminal_returns_some_none`** — for a 3-element
    list, `adj.successor(&Location::After(h(3))) == Some(None)`
    (terminal: location is legal, has no successor).

A6. **`successor_at_invalid_returns_none`** —
    `adj.successor(&Location::After(h(99)))` on a 3-element list
    (no `h(99)` element) returns `None` (outer `None`: location is
    not legal). Pins the diagnostic distinction between "terminal"
    and "invalid" that the two-layer Option encodes.

### `apply` — per variant

D1. **`insert_after_start_on_empty_list`** — empty list, apply
    `InsertAfter(Start, h(1))` ⇒ `seq == [h(1)]`.

D2. **`insert_after_start_on_nonempty_list`** — `[h(2), h(3)]`,
    apply `InsertAfter(Start, h(1))` ⇒ `seq == [h(1), h(2),
    h(3)]`.

D3. **`insert_after_middle`** — `[h(1), h(3)]`, apply
    `InsertAfter(After(h(1)), h(2))` ⇒ `seq == [h(1), h(2),
    h(3)]`.

D4. **`insert_after_terminal_appends`** — `[h(1)]`, apply
    `InsertAfter(After(h(1)), h(2))` ⇒ `seq == [h(1), h(2)]`.

D5. **`update_after_start`** — `[h(1), h(2)]`, apply
    `UpdateAfter(Start, h(9))` ⇒ `seq == [h(9), h(2)]`.

D6. **`update_after_middle_preserves_tail`** — `[h(1), h(2),
    h(3)]`, apply `UpdateAfter(After(h(1)), h(9))` ⇒ `seq == [h(1),
    h(9), h(3)]`. (The `After(h(2)) → h(3)` edge is rewritten as
    `After(h(9)) → h(3)`.)

D7. **`update_after_terminal`** — `[h(1), h(2)]`, apply
    `UpdateAfter(After(h(1)), h(9))` ⇒ `seq == [h(1), h(9)]`.

D8. **`delete_after_start_singleton`** — `[h(1)]`, apply
    `DeleteAfter(Start)` ⇒ `seq == []`. (`edges[Start]` is removed.)

D9. **`delete_after_start_two_elements`** — `[h(1), h(2)]`, apply
    `DeleteAfter(Start)` ⇒ `seq == [h(2)]`. (`Start` is welded to
    the former second element.)

D10. **`delete_after_middle`** — `[h(1), h(2), h(3)]`, apply
     `DeleteAfter(After(h(1)))` ⇒ `seq == [h(1), h(3)]`.

D11. **`delete_after_predecessor_of_terminal`** — `[h(1), h(2)]`,
     apply `DeleteAfter(After(h(1)))` ⇒ `seq == [h(1)]`.

### `apply` — batch behaviour

B1. **`empty_batch_is_noop`** — `apply(adj, &[])` returns `Ok(())`
    and leaves `seq(adj)` unchanged.

B2. **`intra_batch_forward_reference`** — `[h(1)]`, batch:
    `[InsertAfter(After(h(1)), h(2)), InsertAfter(After(h(2)), h(3))]`
    ⇒ `seq == [h(1), h(2), h(3)]`. (Pins the "later delta references
    a hash produced by an earlier one" guarantee from phase1-m1.md
    Step 7.)

B3. **`intra_batch_update_then_reference_new_hash`** — `[h(1), h(2)]`,
    batch:
    `[UpdateAfter(After(h(1)), h(9)),
      InsertAfter(After(h(9)), h(10))]`
    ⇒ `seq == [h(1), h(9), h(10), h(2)]`. (The second delta sees
    the post-update adjacency.)

B4. **`mixed_kinds_batch`** — `[h(1), h(2), h(3)]`, batch:
    `[DeleteAfter(After(h(1))),
      InsertAfter(After(h(1)), h(9)),
      UpdateAfter(After(h(9)), h(10))]`
    ⇒ `seq == [h(1), h(9), h(10)]`. (h(2) is deleted, h(9) replaces
    it, then h(3) is replaced by h(10).)

### `apply` — error cases

E1. **`insert_after_unknown_location`** — empty list,
    `InsertAfter(After(h(1)), h(2))` ⇒
    `Err(DeltaError::LocationNotFound(h(1)))`.

E2. **`update_after_start_on_empty_list`** — empty list,
    `UpdateAfter(Start, h(1))` ⇒
    `Err(DeltaError::NoSuccessor(Location::Start))`.

E3. **`update_after_terminal_no_successor`** — `[h(1)]`,
    `UpdateAfter(After(h(1)), h(9))` ⇒
    `Err(DeltaError::NoSuccessor(Location::After(h(1))))`.

E4. **`delete_after_start_on_empty_list`** — empty list,
    `DeleteAfter(Start)` ⇒
    `Err(DeltaError::NoSuccessor(Location::Start))`.

E5. **`delete_after_terminal_no_successor`** — `[h(1)]`,
    `DeleteAfter(After(h(1)))` ⇒
    `Err(DeltaError::NoSuccessor(Location::After(h(1))))`.

E6. **`hash_field_mismatch_insert_missing_hash`** (release-mode
    behaviour; `cfg(not(debug_assertions))` guard, OR use a direct
    `apply_one` call that bypasses the constructor) — manually
    constructed `Delta { op: InsertAfter, hash: None, … }` ⇒
    `Err(DeltaError::HashFieldMismatch { … })`.

E7. **`hash_field_mismatch_delete_extra_hash`** — manually
    constructed `Delta { op: DeleteAfter, hash: Some(h(1)), … }` ⇒
    `Err(DeltaError::HashFieldMismatch { … })`.

### `encode_delta_batch` / `decode_delta_batch`

C1. **`encode_round_trip`** — a non-trivial batch (all three ops,
    both locations, including `Hash` with non-zero bytes) encodes
    then decodes back to a `PartialEq`-equal `Vec<Delta>`.

C2. **`encode_empty_batch`** — `encode_delta_batch(&[])` produces a
    single byte (`LATEST_DELTA_VERSION`) followed by postcard's
    encoding of an empty Vec (one zero byte for length). Decoded
    back to `vec![]`.

C3. **`leading_byte_is_latest_version`** — first byte of any
    `encode_delta_batch` output is `LATEST_DELTA_VERSION == 1`.

C4. **`decode_empty_input`** — `decode_delta_batch(&[])` returns
    `Err(DecodeBatchError::Empty)`.

C5. **`decode_unknown_version`** — input `[0xFF, …]` returns
    `Err(DecodeBatchError::UnknownVersion(0xFF))`.

C6. **`decode_truncated`** — `[0x01]` alone (version byte but no
    body — postcard expects at least one length byte) returns
    `Err(DecodeBatchError::Postcard(_))`.

C7. **`v1_conversions_total_round_trip`** —
    `Delta::from(v1::DeltaV1::from(&d)) == d` for one delta of
    each op variant (insert, update, delete) and both locations
    (start, after). Pins the V1 ↔ in-memory bijection.

C8. **`v1_wire_format_pinned`** — `encode_delta_batch(&sample_v1_batch())`
    equals a hardcoded `&[u8]` constant. Regenerated via the
    ignored `capture_pinned_values` helper. Catches a postcard rule
    change, an enum-variant reorder, or a field-order flip that
    round-trip alone can't see.

C9. **`v1_wire_hash_pinned`** — `hash_tagged(&encoded_bytes)` equals
    a hardcoded `Hash` constant. (Belt to C8's suspenders: a
    second checksum of the same bytes, using the existing
    BLAKE3-128 primitive. Not semantically meaningful — deltas are
    not stored by hash anywhere — purely a stable fingerprint.)

C10. **`capture_pinned_values`** (`#[ignore]`) — builds
     `sample_v1_batch()`, prints freshly captured
     `PINNED_WIRE_BYTES` and `PINNED_HASH` constants in
     copy-pasteable form. Matches the Turn / Label workflow
     documented in
     [phase1-m1-04 § Pinned-bytes regeneration workflow](phase1-m1-04.md#pinned-bytes-regeneration-workflow).

### Cross-cutting

X1. **`mixed_track_ids_coexist_in_batch`** — apply a batch
    containing deltas with `track_id = 0` and `track_id = 1`
    against two separate adjacency lists (the test wrapper splits
    the batch by `track_id` and drives each list independently —
    `apply` itself is single-track; the engine in Step 11 owns the
    multi-track dispatch). This pins the kind-agnostic contract:
    `delta.rs` never inspects `track_id` for routing purposes.

    (The wrapper exists to document the contract; the actual
    multi-track dispatch lives in Step 11. If implementing this
    test feels awkward — e.g. because the test wrapper duplicates
    Step 11 logic — replace it with a doc-comment assertion on
    `apply` instead. Either way, the property to pin is that
    `delta.rs` is single-track-aware-only-via-the-track_id-field.)

### Out-of-scope tests (covered elsewhere or in later steps)

- **End-to-end replay** (snapshot → apply → ordered Vec<Hash> →
  tree): Step 8.
- **Journal row I/O** (`type = 0` row write + read): Step 9.
- **Undo-stack semantics** (push, pop, journal-append-of-inverse):
  Step 10.
- **Engine clone-on-mutate wrapper** (if all-or-nothing semantics
  are required at a higher layer): Step 11.

## Documentation touches

- **`data-model.md`** — no changes required. The
  [§ Deltas](../design/data-model.md#deltas) and
  [§ Load / replay](../design/data-model.md#load--replay) sections were
  already updated in Step 4 to the post-`Location::After(Hash)`
  rename and to mention per-track-id load dispatch. The Delta
  struct shape in this step matches them field-for-field.
- **`phase1-m1.md` Step 7 bullet** — the existing wording is
  consistent with this plan and no edit is required. Add a
  cross-reference line at the bottom of the Step 7 bullet pointing
  to this document, matching the pattern Step 3 / Step 4 / Step 5
  introduced:
  > See [phase1-m1-07.md](phase1-m1-07.md) for the detailed action
  > plan.
- **`conventions.md`** — no changes. The G1 invariant (a persisted-
  format change ships a migration + a round-trip test) applies to
  the delta wire format, but M1's G1 fixture (Step 13) round-trips
  a full `.vocalboard` file that includes `type = 0` rows by
  construction, so the journal-payload coverage falls out of the
  Step 13 fixture without an extra test.

## Out of scope for Step 7

- **The journal table I/O.** `delta.rs` produces and consumes
  `Vec<u8>` payloads; the SQL `INSERT INTO journal (…) VALUES (…)`
  is Step 9's job.
- **Multi-track delta dispatch.** `Delta` carries `track_id` so the
  engine can route it; `apply` is single-track (one `AdjacencyList`).
  The Step 11 engine owns the `BTreeMap<u32, TrackTree>` and routes
  each replayed batch's deltas to the right track's `AdjacencyList`
  before applying.
- **Kind dispatch for replay** (`load_turn` vs `load_label`): Step
  8, after the adjacency walk produces `Vec<Hash>`.
- **Undo / redo stack mechanics**: Step 10. The engine captures
  inverses at the tree-edit site (where it already holds `h_old` /
  `h_removed`) by calling the `Delta::{insert,update,delete}_after`
  constructors this step ships; `delta.rs` itself owns no inversion
  code path.
- **Forward-edit code path**: Step 11. Forward edits mutate the tree
  through its Step 6 primitives; the engine emits forward+inverse
  delta pairs at each edit site. No use of `AdjacencyList`.
- **`Delta::op == InsertAfter` with a hash already present in the
  adjacency list** — left undefined; the engine never produces
  such a delta because every newly-created element carries a
  freshly-allocated `next_turn_id` / `next_label_id` (per
  [data-model.md § Turn payload](../design/data-model.md#turn-payload-speech-tracks)).
  No invariant guard in `delta.rs` because no current caller can
  trip it; the worked case is "the hash silently displaces the
  existing element's outgoing edge," which is a self-consistent
  ill-defined state, not a crash.
- **Any V2 of `DeltaV1`** — there is no V2 yet. The dispatch table
  has one arm.

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` (must remain green with
  `unwrap_used`, `expect_used`, `panic`, and `missing_docs` all
  CI-gated). The `apply_one` body uses `.expect("validated above")`
  on the `hash.unwrap()` calls — this is the documented justifying
  comment pattern from [conventions.md](../design/conventions.md); the
  preceding match arm exhaustively rules out the `None` case.
- `cargo test -p core delta::` — runs the tests above (~39 tests:
  6 AdjacencyList + 11 apply-per-variant + 4 batch behaviour + 7
  errors + 10 encode/decode + V1 wire format + 1 cross-cutting).
- `cargo test -p core` — confirms no regression elsewhere (e.g.
  the new `pub mod delta;` line in `project/mod.rs` doesn't
  conflict with anything).
- Manual diff review of `delta.rs` against
  [data-model.md § Deltas](../design/data-model.md#deltas) for
  field-for-field correspondence.
- One commit on `claude/1M1`, **unsigned** per the GPG-by-branch
  policy in [CLAUDE.md](../CLAUDE.md). Subject:
  `1M1-07: delta language + replay-side adjacency list`.
  Bundles `core/src/project/delta.rs`, the `pub mod delta;` line in
  `project/mod.rs`, and (per the pre-commit checklist) the
  corresponding revision to Step 7 in `phase1-m1.md`.

## Downstream implications (flag for later steps)

- **Step 8 (`snapshot.rs`):** replay calls
  `AdjacencyList::from_sequence(snapshot_hashes)` to seed,
  `delta::decode_delta_batch(row.payload)` + `delta::apply(adj,
  &batch)` per `type = 0` row newer than the snapshot, then
  `adj.iter().collect::<Vec<Hash>>()` for the ordered fetch list.
  Per-kind dispatch (`load_turn` vs `load_label`) happens *after*
  this step's work — Step 8 walks the resulting `Vec<Hash>`,
  resolves each blob via `db::store::get`, and parses with the
  loader keyed on `track_id`.
- **Step 9 (`journal.rs`):** the `type = 0` row's `payload` column
  is the `Vec<u8>` from `delta::encode_delta_batch`. The version
  byte is owned by this module, not journal.rs — journal.rs is
  payload-agnostic. The `command_id` column is journal.rs's
  responsibility (separate from `LATEST_DELTA_VERSION`).
- **Step 10 (`undo.rs`):** the `UndoEntry` stores the forward and
  inverse batches as `Vec<Delta>` *captured by the engine at edit time*
  (not by anything in `delta.rs`), alongside the before/after
  `Arc<TimelineState>` snapshots. On undo, the engine writes the inverse
  batch as a fresh `type = 0` journal row tagged with the relevant
  `command_id` code — effectively, the inverse becomes a forward
  edit from replay's perspective, which is what makes replay
  reproduce the post-undo state on next open.
- **Step 11 (`engine.rs`):** holds one `AdjacencyList` per track
  during replay, then discards them once the resulting `Vec<Hash>`
  has been resolved to elements and bulk-built into each
  `TrackTree`. Forward edits never construct an `AdjacencyList` — the
  engine walks the tree via its Step 6 temporal queries to find the
  edit's predecessor (yielding `Location::Start` or
  `Location::After(<predecessor hash>)`), reads `h_old` /
  `h_removed` directly from the tree node it's about to mutate,
  constructs the forward `Delta` plus its inverse via the
  `Delta::{insert,update,delete}_after` constructors, then calls the
  tree's `insert_at` / `update_at` / `delete_at` to apply the edit
  in place (one path-copy per delta, with structural sharing
  preserved). The forward batch is encoded via
  `delta::encode_delta_batch` for the `type = 0` row; the inverse
  batch is held on the undo stack.
- **Phase 6 scripting / plugin host (post-M1):** the `pub(crate)`
  visibility on `apply` / `encode_delta_batch` / `decode_delta_batch`
  / `AdjacencyList` is deliberate — only the engine layer should
  drive the delta primitives. If a scripting API ever needs to
  author a multi-delta batch, it should go through a high-level
  command, not the delta module directly.
