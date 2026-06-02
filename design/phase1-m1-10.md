# Phase 1 · M1 — Step 10: Undo / redo (`project/undo.rs`) — detailed action plan

Detailed breakdown of [Step 10 in phase1-m1.md](phase1-m1.md#step-10--undo--redo-projectundors).
Authoritative behaviour spec: [data-model.md § Undo / redo](data-model.md#undo--redo).

This step delivers the **in-memory undo/redo machinery**. The undoable project state — the
per-track timeline trees **and** the non-timeline metadata — is held as one immutable value
behind an `Arc` (`TimelineState`). An edit swaps that `Arc`; a `History` value records
`UndoEntry` packages holding the before/after snapshots plus the journal effects needed to
persist the reversal. It is implementable and fully testable without the Step 11 engine —
`History`'s operations take the state `Arc` and a DB connection as parameters rather than
reaching into a `ProjectState` that does not exist yet.

## Scope

**In scope (this step):**

- New module `core/src/project/undo.rs`: `TimelineState`, `UndoEntry`, `History`, `HistoryError`.
- `CommandId::undo_of` helper added to `core/src/project/command_id.rs`.
- Derive `Clone` on the existing `TrackTree` enum in `core/src/project/snapshot.rs`.
- `Db::conn_mut(&mut self) -> &mut Connection` added to `core/src/db/mod.rs` (the undo transaction
  needs a mutable connection).
- `undo_history_limit` field + `DEFAULT_UNDO_HISTORY_LIMIT` const in `core/src/settings.rs`, with
  a round-trip test (the data-integrity invariant — see [§ Settings](#settings-coresrcsettingsrs)).
- `pub mod undo;` in `core/src/project/mod.rs`.
- Unit tests (stack transitions + eviction) + integration tests (replay-after-undo for a
  tree-only edit and a metadata-changing edit).

**Out of scope — Step 11 (`engine.rs`):** the `apply_batch` *producer* that builds an `UndoEntry`
from a real edit; `ProjectState` (which owns the `current: Arc<TimelineState>` and a `History`);
the snapshot writer; Tauri wiring; reading `settings.undo_history_limit` to size the `History`;
removing the `#[allow(dead_code)]` attributes this step adds (Step 11 provides the first non-test
callers — see the cleanup list in
[phase1-m1.md Step 11](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectengeners)).

**Out of scope — M5:** the real editing commands. Because undo snapshots the whole `TimelineState`
(trees + metadata) and journals both effects, **metadata-undo already works** after this step;
M5 only adds the commands that produce metadata-changing edits and stamps their `CommandId`.

## Where Step 10 sits in the edit pipeline (context)

A turn-mutating command flows through three layers (see
[data-model.md § Batched edits](data-model.md#batched-multi-element-edits) and
[§ Undo / redo](data-model.md#undo--redo)):

1. **Command (M4/M5)** — computes *what* the new element(s)/metadata are (pure function of the
   originals + parameters; content is position-independent) and submits semantic ops.
2. **`apply_batch` applier (Step 11)** — the **producer**. Mutates working clones of the touched
   `TrackTree`s via the Step 6 primitives; per op, reads `Location` + `h_old` from
   `tree.element_at_sample(..)` and emits the forward+inverse `Delta` pair at the edit site
   (Step 7); `store::put`s new blobs and appends the forward row(s) in one transaction; then,
   only on commit, builds the new `Arc<TimelineState>`, swaps the live `current`, and calls
   `History::record`.
3. **`History` (this step)** — the **consumer**. Holds the undo/redo stacks; `undo`/`redo` swap
   the state `Arc` and append the inverse/forward effect to the journal. **They never compute an
   inverse** — the inverse was captured at edit time and is stored in the entry.

Step 10 builds layer 3 only (plus the shared `TimelineState` type it snapshots). Tests synthesize
`UndoEntry` values directly (standing in for layer 2).

## Existing APIs this step builds on (all already implemented)

- `project::snapshot::{TrackTree, PerTrackTrees}` — `PerTrackTrees = BTreeMap<u32, TrackTree>`;
  `TrackTree::{Labels(ImplicitTimelineTree<Label>), Speech(ImplicitTimelineTree<Turn>)}`.
  Currently `#[derive(Debug, PartialEq)]` — this step adds `Clone`.
- `project::metadata::{Metadata, store_metadata}` — `Metadata` already derives
  `Clone, Debug, PartialEq, Default`; `store_metadata(&Metadata) -> Result<(Hash, Vec<u8>),
  postcard::Error>`; `load_current_metadata(&Db, as_of) -> …` (used by the integration test).
- `project::delta::{Delta, encode_delta_batch}` — `encode_delta_batch(&[Delta]) ->
  Result<Vec<u8>, postcard::Error>` produces the version-prefixed `type = 0` payload.
- `db::store::put(conn: &Connection, hash: &Hash, bytes: &[u8]) -> Result<bool, StoreError>`.
- `db::journal::{append_delta_batch, append_metadata, JournalError}` —
  `append_delta_batch(conn, command_id, payload: &[u8], applied_at) -> Result<i64, JournalError>`;
  `append_metadata(conn, command_id, hash: &Hash, applied_at) -> Result<i64, JournalError>`.
  `JournalError` is `pub(crate)`, already `impl std::error::Error`.
- `project::command_id::{CommandId, UNDO_FLAG}` — `code(self) -> i64`, `from_code(i64) ->
  Option<CommandId>`; `UNDO_FLAG = 0x1`; every category `X` has `UndoX` with `UndoX.code() ==
  X.code() | UNDO_FLAG`.
- `project::snapshot::load_and_replay(&Db, as_of: Option<i64>) -> Result<PerTrackTrees,
  ReplayError>` — used by the integration test to prove replay reproduces post-undo state.
- `db::Db` — `open`, `conn(&self) -> &Connection`, `with_transaction`. `&rusqlite::Transaction`
  derefs to `&Connection`, so the `append_*` / `store::put` signatures work unchanged inside a
  transaction (the [Step 5](phase1-m1-05.md) pattern).

## Types (in `core/src/project/undo.rs`)

All types are `pub(crate)` (engine-internal; not part of the command surface, so not subject to
the `missing_docs` gate, but still doc-commented per house style). Mark items `#[allow(dead_code)]`
— they are only test-reachable until Step 11 wires the engine (see [§ Dead-code](#dead-code)).

### `TimelineState`

The complete undoable project state, snapshotted as one immutable value.

```rust
use crate::project::metadata::Metadata;
use crate::project::snapshot::PerTrackTrees;

/// The complete undoable project state: the per-track timeline trees and the
/// non-timeline metadata. Held behind an `Arc` by the engine (Step 11) and by
/// each `UndoEntry`; an edit builds a new value and swaps the `Arc`. Cloning is
/// cheap — the `PerTrackTrees` spine + `Metadata` struct are small, and tree
/// subtrees stay `Arc`-shared (large metadata binaries are referenced by hash).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TimelineState {
    pub(crate) trees: PerTrackTrees,
    pub(crate) metadata: Metadata,
}
```

`Default` gives the empty initial state (`PerTrackTrees::default()` + `Metadata::default()`),
useful for `new_project` and tests. `PartialEq` (via `TrackTree`/tree sequence-equality and
`Metadata`'s derive) drives test assertions.

### `UndoEntry`

A symmetric package describing one undoable edit. Carries both state snapshots so undo and redo
are pure `Arc` swaps; carries the journal effects (what to append to persist each direction).

```rust
use std::sync::Arc;
use crate::project::command_id::CommandId;
use crate::project::delta::Delta;

/// One undoable edit.
#[derive(Debug)]
pub(crate) struct UndoEntry {
    /// State before the edit (restored on undo).
    pub(crate) before: Arc<TimelineState>,
    /// State after the edit (restored on redo).
    pub(crate) after: Arc<TimelineState>,
    /// Forward timeline delta batch; `None` for a metadata-only edit.
    pub(crate) forward_delta: Option<Vec<Delta>>,
    /// Inverse timeline delta batch; `None` for a metadata-only edit.
    pub(crate) inverse_delta: Option<Vec<Delta>>,
    /// Whether this edit changed metadata (⇒ a `type = -1` row each direction).
    pub(crate) metadata_changed: bool,
    /// Forward command category. Redo stamps this; undo stamps `category.undo_of()`.
    pub(crate) category: CommandId,
}
```

Notes:
- The metadata blobs to append are **derived from the snapshots** at undo/redo time
  (`store_metadata(&before.metadata)` for undo, `&after.metadata` for redo) — not stored twice.
- A pure-timeline edit has `metadata_changed == false`; a pure-metadata edit (e.g. a future
  `rename_track`) has `forward_delta == None` / `inverse_delta == None` and `metadata_changed ==
  true`. At least one of (`*_delta`, `metadata_changed`) is meaningful per entry.
- No `Clone` needed on `UndoEntry` (it is moved between stacks). The `Arc<TimelineState>` fields
  are `Arc`-cloned during the swap.

### `History`

```rust
use std::collections::VecDeque;

/// In-memory undo/redo stacks (not persisted — see data-model.md § Undo / redo).
/// `undo` is bounded: it behaves as a stack at the back (`push_back`/`pop_back`)
/// and a queue at the front (`pop_front` evicts the oldest past `limit`). `redo`
/// is a plain `Vec` — cleared on every `record`, bounded by undo depth, never
/// front-evicted.
#[derive(Debug)]
pub(crate) struct History {
    undo: VecDeque<UndoEntry>,
    redo: Vec<UndoEntry>,
    limit: usize,
}
```

### `HistoryError`

Follow the repo's hand-rolled error convention (no derive-macro crate such as `thiserror`; see
`JournalError` / `StoreError`): manual `Display` + `std::error::Error` with `source()`.

```rust
#[derive(Debug)]
pub(crate) enum HistoryError {
    /// Encoding a delta batch failed.
    Encode(postcard::Error),
    /// Storing a metadata blob failed.
    Store(crate::db::store::StoreError),
    /// Appending a journal row failed.
    Journal(crate::db::journal::JournalError),
    /// A transaction begin/commit failed.
    Sqlite(rusqlite::Error),
}
```

- `Display`: one arm each, e.g. `Encode(e) => write!(f, "failed to encode delta batch for
  undo/redo: {e}")`; etc.
- `std::error::Error::source`: return `Some(e)` for each wrapped error.
- The `From`/mapping is explicit at call sites (see the algorithm) to keep the arms unambiguous.

## Behaviour (methods on `History`)

```rust
use rusqlite::Connection;

impl History {
    /// New history with the given undo-depth limit (clamp 0 → 1 so at least one
    /// edit is always undoable; or document that 0 disables undo — pick one and
    /// test it. Recommended: treat 0 as "disabled", skip recording).
    pub(crate) fn new(limit: usize) -> Self {
        Self { undo: VecDeque::new(), redo: Vec::new(), limit }
    }

    /// Record a freshly-applied edit. Clears redo (a new edit invalidates it) and
    /// evicts the oldest undo entry while over `limit`. No journal action — the
    /// forward row(s) were already written by the producer (apply_batch).
    pub(crate) fn record(&mut self, entry: UndoEntry) {
        if self.limit == 0 { self.redo.clear(); return; }
        self.undo.push_back(entry);
        while self.undo.len() > self.limit { self.undo.pop_front(); }
        self.redo.clear();
    }

    /// Undo the most recent edit. `Ok(false)` if the undo stack is empty.
    pub(crate) fn undo(
        &mut self,
        current: &mut Arc<TimelineState>,
        conn: &mut Connection,
        applied_at: i64,
    ) -> Result<bool, HistoryError> { /* see algorithm */ }

    /// Redo the most recently undone edit. `Ok(false)` if redo is empty.
    pub(crate) fn redo(
        &mut self,
        current: &mut Arc<TimelineState>,
        conn: &mut Connection,
        applied_at: i64,
    ) -> Result<bool, HistoryError> { /* symmetric */ }

    pub(crate) fn can_undo(&self) -> bool { !self.undo.is_empty() }
    pub(crate) fn can_redo(&self) -> bool { !self.redo.is_empty() }
}
```

### `undo` algorithm

Persist first, then swap memory (mirrors the write path,
[data-model.md § Write path](data-model.md#write-path)). On any error, restore the popped entry so
the stacks and journal stay consistent:

```
1. let entry = match self.undo.pop_back() { Some(e) => e, None => return Ok(false) };
2. match append_effect(conn, /*inverse*/ entry.inverse_delta.as_deref(),
                        entry.metadata_changed, &entry.before.metadata,
                        entry.category.undo_of(), applied_at) {
       Ok(())  => { *current = entry.before.clone(); self.redo.push(entry); Ok(true) }
       Err(e)  => { self.undo.push_back(entry); Err(e) }
   }
```

`redo` is symmetric: `self.redo.pop()`; append the **forward** effect
(`entry.forward_delta.as_deref()`, `&entry.after.metadata`, stamp = plain `entry.category`); on
`Ok`, `*current = entry.after.clone()` and `self.undo.push_back(entry)` (then evict over `limit`,
though redo can never push past it); on `Err`, `self.redo.push(entry)`.

### `append_effect` (private helper) — the single transaction

Appends 0–2 rows atomically. Up to one `type = 0` (delta) and one `type = -1` (metadata), both
stamped with the same `command_id`:

```
fn append_effect(conn, delta: Option<&[Delta]>, metadata_changed: bool,
                 metadata: &Metadata, stamp: CommandId, applied_at: i64)
    -> Result<(), HistoryError>
{
    let tx = conn.transaction().map_err(HistoryError::Sqlite)?;
    if let Some(batch) = delta {
        let payload = encode_delta_batch(batch).map_err(HistoryError::Encode)?;
        append_delta_batch(&tx, stamp, &payload, applied_at).map_err(HistoryError::Journal)?;
    }
    if metadata_changed {
        let (h, bytes) = store_metadata(metadata).map_err(HistoryError::Encode)?;
        store::put(&tx, &h, &bytes).map_err(HistoryError::Store)?;     // INSERT OR IGNORE — idempotent
        append_metadata(&tx, stamp, &h, applied_at).map_err(HistoryError::Journal)?;
    }
    tx.commit().map_err(HistoryError::Sqlite)?;
    Ok(())
}
```

`&tx` derefs to `&Connection`, so `append_delta_batch` / `store::put` / `append_metadata` take it
directly. The metadata blob is re-derived and `INSERT OR IGNORE`d (it was already stored when that
state was current, so `put` is a no-op then; doing it again is harmless and keeps the helper
self-contained).

### Why this is correct (for the implementer)

- **Undo is a forward-recorded edit.** Appending the inverse effect means the journal reads
  `[… forward …, inverse]`; replay on reopen applies all rows and lands on the post-undo state.
  Redo appends `forward` again → `[…, inverse, forward]` → post-redo state.
- **Clearing redo touches no journal.** The inverse row(s) from a prior undo are already
  persisted; a later new edit appends its own forward row(s). The discarded redo entry's effect is
  simply never re-applied. Replay stays consistent.
- **Whole-state swap.** `*current = entry.before.clone()` reverts trees and metadata together in
  one `Arc` assignment; no field-by-field reconciliation.

## `CommandId::undo_of` (in `command_id.rs`)

```rust
impl CommandId {
    /// The undo-stamp for a forward command category: the category with
    /// [`UNDO_FLAG`] set (e.g. `Cut.undo_of() == UndoCut`; `Unknown.undo_of() ==
    /// Undo`). Defined for every single category; falls back to `Undo` for a
    /// non-category code (which commands never pass).
    pub fn undo_of(self) -> CommandId {
        CommandId::from_code(self.code() | UNDO_FLAG).unwrap_or(CommandId::Undo)
    }
}
```

- Use `unwrap_or(CommandId::Undo)`, **not** `.unwrap()` (`unwrap_used` is clippy-gated).
- Add `#[allow(dead_code)]` until Step 11.
- Extend the existing `command_id.rs` tests: `Cut.undo_of() == UndoCut`, `Mute.undo_of() ==
  UndoMute`, `Unknown.undo_of() == Undo`, and idempotence on an undo code (`UndoCut.undo_of() ==
  UndoCut`).

## `TrackTree: Clone` (in `snapshot.rs`)

Change the derive on `TrackTree` from `#[derive(Debug, PartialEq)]` to `#[derive(Debug, Clone,
PartialEq)]`. `ImplicitTimelineTree<T>` already implements `Clone`, so the derive composes. No
other change. (`PerTrackTrees = BTreeMap<u32, TrackTree>` then clones automatically, which
`TimelineState: Clone` relies on.)

## `Db::conn_mut` (in `db/mod.rs`)

Add next to `conn`:

```rust
/// Mutable borrow of the connection — needed to open a transaction.
#[allow(dead_code)]
pub(crate) fn conn_mut(&mut self) -> &mut Connection { &mut self.conn }
```

(`Connection::transaction()` requires `&mut Connection`.) The engine calls
`self.history.undo(&mut self.current, self.db.conn_mut(), now)` — `history`, `current`, and `db`
are disjoint fields, so the three mutable borrows are allowed.

## Settings (`core/src/settings.rs`)

Add an undo-depth setting, mirroring the existing `snapshot_idle_seconds` pattern exactly:

- A const: `pub const DEFAULT_UNDO_HISTORY_LIMIT: usize = 50;` (near the other `DEFAULT_*` consts).
- A field on `Settings`:
  ```rust
  /// Maximum number of undoable edits retained in memory (oldest evicted past this).
  #[serde(default = "default_undo_history_limit")]
  pub undo_history_limit: usize,
  ```
- `fn default_undo_history_limit() -> usize { DEFAULT_UNDO_HISTORY_LIMIT }`.
- Initialise it in the `Default for Settings` impl: `undo_history_limit: DEFAULT_UNDO_HISTORY_LIMIT`.
- **No version bump.** This is an additive, defaulted field: an old `settings.json` lacking the key
  deserializes to 50 via `#[serde(default)]`, exactly like `snapshot_idle_seconds` /
  `model_idle_unload_seconds`. `apply_migration` is untouched.
- **Round-trip test** (the data-integrity invariant for a persisted-format change): in
  `core/tests/settings_roundtrip.rs`, add a case that loads a settings JSON value **without** the
  `undo_history_limit` key and asserts the parsed `Settings.undo_history_limit == 50`, plus a
  serialize→deserialize round-trip that preserves a non-default value (e.g. 10).

The engine (Step 11) passes `settings.undo_history_limit` to `History::new`. Step 10 itself only
needs the const + the `usize` parameter; the test for the *field* lives in
`settings_roundtrip.rs`.

## Module wiring

- Add `pub mod undo;` to `core/src/project/mod.rs` (keep the file's alphabetical ordering).
- `undo.rs` imports: `std::collections::VecDeque`; `std::sync::Arc`; `rusqlite::Connection`;
  `crate::db::store`; `crate::db::journal::{append_delta_batch, append_metadata, JournalError}`;
  `crate::db::store::StoreError`; `crate::project::command_id::CommandId`;
  `crate::project::delta::{encode_delta_batch, Delta}`;
  `crate::project::metadata::{store_metadata, Metadata}`;
  `crate::project::snapshot::PerTrackTrees`.

## Dead-code

Every new item (`TimelineState`, `UndoEntry`, `History` + methods, `CommandId::undo_of`,
`Db::conn_mut`) is only reachable from tests until Step 11 wires the engine. Mark them
`#[allow(dead_code)]` (per the Step 8/9 pattern). **Do not remove** any existing
`#[allow(dead_code)]` — Step 11 removes them together with this step's (the list in
[phase1-m1.md Step 11](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectengeners)
already names the Step 10 items). Deriving `Clone` on `TrackTree` does not trigger dead-code
warnings (trait impls are not dead-code-linted).

## Test plan (`#[cfg(test)] mod tests` in `undo.rs`)

### Test helpers

Define local helpers mirroring `snapshot.rs`'s test module (they are private there):

- `open_tmp_db() -> (tempfile::TempDir, Db)` — `Db::open(dir/"p.vocalboard")`. The `Db` must be
  `mut` so tests can call `conn_mut()`.
- `put_turn(db, id, dur, silence) -> Hash` — build a `Turn`, `store_turn`, `store::put`.
- `speech_tree(conn, seq: Vec<Hash>) -> TrackTree` — `snapshot::build_track_tree` is private, so
  build directly: for each `h` in `seq`, `let bytes = store::get(conn, &h)?; let turn =
  load_turn(&bytes)?;` collect `(h, Arc::new(turn))`, then
  `TrackTree::Speech(ImplicitTimelineTree::from_sorted_elements(elements))`.
- `state(trees: PerTrackTrees, metadata: Metadata) -> Arc<TimelineState>`.
- `write_snapshot_row(db, &Snapshot) -> i64` — `store_snapshot` + `store::put` + insert a
  `type = 1` journal row (see the snapshot.rs helper of the same name).
- A trivial `UndoEntry` builder for the unit tests (empty/short fields are fine).

### Unit tests (pure stack transitions + eviction)

These need only a throwaway `Db` (for the journal append in undo/redo) and trivial entries
(empty delta batches, `metadata_changed = false`, `before == after` allowed).

- `record_pushes_and_clears_redo` — after `record`, `can_undo()` true, `can_redo()` false; forcing
  a redo entry (via `undo`) then `record` clears it.
- `undo_then_redo_round_trips_stacks` — `record` → `undo` (moves to redo) → `redo` (back to undo);
  `can_undo`/`can_redo` track at each step.
- `undo_empty_is_noop` / `redo_empty_is_noop` — return `Ok(false)`, stacks unchanged, no journal
  row appended.
- `new_edit_after_undo_discards_redo` — `record(A)` → `undo` → `record(B)`: redo empty, undo has B.
- `oldest_evicted_past_limit` — `History::new(2)`; record 3 entries; `can_undo` true and exactly 2
  retained (undo three times succeeds twice then `Ok(false)`; or assert `undo.len()` via a
  test-only accessor). Confirms `pop_front` eviction.
- `zero_limit_disables_recording` — `History::new(0)`; `record` then `can_undo()` is false (or
  whichever 0-semantics you implement in `new`/`record` — keep it consistent and asserted).

### Integration tests (replay-after-undo)

`undo_replays_to_post_undo_state` (tree-only edit):

1. `open_tmp_db()`.
2. Build a single-track timeline: `hA, hB, hC = put_turn(...)` (distinct durations).
   `let trees0 = { let mut m = PerTrackTrees::new(); m.insert(1, speech_tree(conn, vec![hA, hB,
   hC])); m };` `let before = state(trees0, Metadata::default());`
3. `write_snapshot_row(&db, &snapshot_from_trees(&before.trees))` — the initial `type = 1` row.
4. Synthesize the edit (update B → B′): `hB2 = put_turn(...)`; build `trees1` = `[hA, hB2, hC]`;
   `let after = state(trees1, before.metadata.clone());`
   - `forward = vec![Delta::update_after(1, Location::After(hA), hB2)]`;
     `inverse = vec![Delta::update_after(1, Location::After(hA), hB)]`.
   - Append the forward row (what apply_batch will do): `append_delta_batch(db.conn(),
     CommandId::Unknown, &encode_delta_batch(&forward)?, 0)?`.
   - `let mut current = after.clone();`
   - `let entry = UndoEntry { before: before.clone(), after: after.clone(),
     forward_delta: Some(forward), inverse_delta: Some(inverse), metadata_changed: false,
     category: CommandId::Unknown }; history.record(entry);`
5. Assert `current == after` (post-edit).
6. `assert!(history.undo(&mut current, db.conn_mut(), 0)?);`
   - Assert `current == before` (state reverted).
   - Assert the last journal row is `type = 0` with `command_id == CommandId::Unknown.undo_of()
     .code()` (= `CommandId::Undo.code() == 0x1`) — query the journal directly.
7. `let replayed = load_and_replay(&db, None)?;` assert `replayed == before.trees` — replay of
   `[snapshot, forward, inverse]` reproduces the post-undo trees.
8. `assert!(history.redo(&mut current, db.conn_mut(), 0)?);` → `current == after`;
   `load_and_replay(&db, None)? == after.trees`.

`undo_replays_to_post_undo_metadata` (metadata-changing edit):

1. As above through the initial snapshot, but also seed an initial metadata row: build
   `meta0` (e.g. one `TrackMeta` named `"a"`), `let (h0, b0) = store_metadata(&meta0)?;
   store::put(db.conn(), &h0, &b0)?;` append a `type = -1` row pointing at `h0`. `before =
   state(trees0, meta0)`.
2. Synthesize a metadata-only edit (rename track `"a"` → `"b"`): `meta1` = `meta0` with the new
   name. Append the forward `type = -1` row: `store_metadata(&meta1)` → put → `append_metadata(
   db.conn(), CommandId::Unknown, &h1, 0)`. `after = state(trees0_clone, meta1)`.
   `entry = UndoEntry { before, after, forward_delta: None, inverse_delta: None,
   metadata_changed: true, category: CommandId::Unknown }; history.record(entry);`
3. `history.undo(&mut current, db.conn_mut(), 0)?` → `current.metadata == meta0`; the appended
   inverse is a `type = -1` row (no `type = 0`) stamped `Undo`.
4. `load_current_metadata(&db, None)? == meta0` — replay/most-recent-wins reproduces the
   post-undo metadata. `redo` → `meta1`, and `load_current_metadata` agrees.

(Use a speech track, `track_id = 1`; the labels track and `TrackTree`'s two variants are already
covered by Step 8 — `History` is kind-agnostic.)

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` — stays green with `unwrap_used`, `expect_used`, `panic`,
  `missing_docs` gated (hence `undo_of` uses `unwrap_or`, and `HistoryError` is documented).
- `cargo test -p core undo::` — the new module tests.
- `cargo test -p core command_id::` — the extended `undo_of` tests.
- `cargo test -p core --test settings_roundtrip` — the new `undo_history_limit` round-trip case.
- `cargo test -p core` — no regression (in particular `snapshot::` after the `TrackTree` derive
  change, and `db::` after `conn_mut`).
- One commit on `claude/1M1`, **unsigned** per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md). Suggested subject: `1M1-10: undo/redo History (whole-state snapshot,
  journal-recorded)`.

## Documentation touches

- Behaviour is already specified in [data-model.md § Undo / redo](data-model.md#undo--redo) and the
  layering in [phase1-m1.md Step 10](phase1-m1.md#step-10--undo--redo-projectundors); no further
  design-doc change is required. Doc-comment the new module header with the producer/consumer split
  and the "undo is a forward-recorded edit" invariant.

## Downstream implications (flag for Step 11 / M5)

- **Step 11 (`engine.rs`):** `ProjectState` owns `current: Arc<TimelineState>` and a `History`
  built via `History::new(settings.undo_history_limit)`; its `undo`/`redo` handlers call
  `history.undo(&mut self.current, self.db.conn_mut(), now_posix())`. `apply_batch` is the
  `UndoEntry` producer — it enforces descending-sample application over original-tree-coordinate
  positions ([data-model.md § Batched edits](data-model.md#batched-multi-element-edits)), builds
  the new `Arc<TimelineState>`, and calls `history.record`. Step 11 removes this step's
  `#[allow(dead_code)]` attributes.
- **M5:** the editing commands (`rename_track` etc.) supply real `CommandId` categories (so
  `undo_of` produces `UndoCut`, `UndoEditLabel`, …); metadata-only commands produce entries with
  `forward_delta == None` / `metadata_changed == true`. No undo-machinery changes are needed.
