# Phase 1 · M1 — Step 11: `ProjectState` engine + snapshot writer (`project/engine.rs`) — detailed action plan

Detailed breakdown of [Step 11 in phase1-m1.md](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectengeners).
Authoritative behaviour specs: [data-model.md](../design/data-model.md) (§ Write path, § Load / replay,
§ Batched (multi-element) edits, § Undo / redo, § Audio file resolution).

This step assembles everything built in Steps 2–10 into the **`ProjectState` engine** — the single
owner of an open project's live state and the only thing that mutates the database. It is the first
genuine non-test caller of the Step 8/9/10 plumbing (so it removes their `#[allow(dead_code)]`
attributes), and it adds the **producer** half of the edit pipeline (`apply_batch`) plus the
**snapshot writer**. It stays in the `core` crate with **no Tauri dependency**; the Tauri command
handlers + TS contract are [Step 12](phase1-m1.md#step-12--tauri-wiring--contract).

Build and prove it with **synthetic** edits — no real editing command and no audio/ML exist until
M4/M5.

## Scope

**Implement in two committed sub-steps.** Step 11 lands three loosely-coupled subsystems with
very different risk profiles, so it is split at the natural producer/consumer fault line (the same
seam Step 10 used when it deferred "the producer `apply_batch` … lands in Step 11"). **Commit 11a
green before starting 11b** — never debug the novel applier logic on top of unverified scaffolding.

### Step 11a — engine skeleton, lifecycle, recovery, dead-code sweep (low risk)

Pure assembly of already-tested functions; no novel algorithm. Build, test, **commit as
`1M1-11a`**, *then* start 11b.

- New module `core/src/project/engine.rs` with `pub mod engine;` added to `core/src/project/mod.rs`
  (keep the file's alphabetical ordering — `engine` sorts before `command_id`).
- `ProjectState` — owns the `Db`, the live `current: Arc<TimelineState>`, a `History`, and the
  snapshot-writer handle. `EngineError` + `OpenOutcome` types.
- `now_posix() -> i64` wall-clock helper (engine-local).
- `new_project(...)`, `open_project(...)` (with the corrupt-journal recovery fallback),
  `save_snapshot_now()`.
- `undo()` / `redo()` passthroughs that drive the Step 10 `History` against `current` + the DB.
- The **snapshot-writer mechanism** wired to `save_snapshot_now` and app-exit (its own `rusqlite`
  connection; the lock-free clone-and-flatten handoff). **No 30 s idle-autosave timer** (deferred
  to M5 — see [§ Out of scope](#out-of-scope-deferred)).
- Wire `History::new(settings.undo_history_limit)` so the configured undo depth takes effect.
- **Remove** the `#[allow(dead_code)]` attributes on all Step 8/9/10 plumbing the engine now calls
  (full list in [§ Dead-code cleanup](#dead-code-cleanup)). ⚠️ **If the sweep leaves an item still
  flagged unused, stop and report it** — that means the plan missed a wiring point; it is a decision
  for the user, not something to paper over by re-adding the allow or inventing a fake caller.
- 11a tests (`core/tests/engine_lifecycle.rs` + inline): the journal-corruption recovery test, and a
  lifecycle round-trip that exercises `new_project → save_snapshot_now → drop → open_project` plus
  `undo`/`redo` driven by **synthetically-recorded** `UndoEntry`s (Step 10 already proved `History`
  works against a hand-built `Db`, so 11a can record entries directly without `apply_batch`).

**11a is a coherent, shippable unit even with no `apply_batch`** — it compiles, passes, and commits
on its own. That is the point of the split.

### Step 11b — `apply_batch` producer (high risk — the only novel logic)

Builds on a green 11a. This is the one piece with no precedent in the codebase, so it gets its own
commit and focused mutation testing.

- `apply_batch` (`pub(crate)` — only tests/M5 call it in M1) — the **producer** that applies a
  synthetic edit batch in descending-sample order over original-tree coordinates, captures the
  forward+inverse deltas at each edit site, persists them, swaps `current`, and calls
  `History::record`. See the **`apply_batch` … the producer** section below for the algorithm,
  the sample-vs-`Location` footgun, and a worked example.
- Extend the lifecycle test to drive **real `apply_batch` edits** across two tracks (replacing 11a's
  synthetic `record`s), plus the descending-order and inverse-capture tests.
- `cargo-mutants` scoped to `apply_batch` (ordering + delta/inverse capture).
- **Commit as `1M1-11b`.**

11a/11b shipped; a design review of the result raised four issues, addressed as **two further
commits along the same producer/consumer risk fault line 11a/11b used** — 11c (mechanical) green
before 11d (novel logic) begins.

### Step 11c — db-module hardening + tree `last_hash` (post-11b review; low risk)

The mechanical half: route *all* connection-opening through the `db` module, add a `db::project`
module, add existence-policed `Db::create`/`Db::open`, and add `ImplicitTimelineTree::last_hash`
(review issues 1, 2, 4). Detail:
[§ Step 11c detailed](#step-11c-detailed-db-module-and-tree-last_hash). **Commit as `1M1-11c`.**

### Step 11d — `apply_batch` metadata producer (review issue 3; novel logic)

Builds on a green 11c. Extend `apply_batch` to journal a metadata change in the **same transaction**
as the delta batch — the producer half of combined tree+metadata edits (e.g. `add_track`) — plus the
data-model/phase-doc narrowing of M5. Gets its own focused mutation pass. Detail:
[§ Step 11d detailed](#step-11d-detailed-apply_batch-metadata-producer). **Commit as `1M1-11d`.**

> Keep all sub-steps in **this one plan doc** (they share `ProjectState`, `EngineError`,
> `OpenOutcome`, and the test file). The `### Step 11a`–`### Step 11d` headers above are the
> contract; the detailed sections below apply to whichever sub-step owns the item.

**Out of scope — [Step 12](phase1-m1.md#step-12--tauri-wiring--contract):** the
`#[tauri::command]` handlers, the managed `Mutex<Option<ProjectState>>` in `app/main.rs`, the
`proto` param/result types, and the generated TS wrappers. Step 11 exposes the engine API; Step 12
wires it to the webview.

**Out of scope — [Step 13](phase1-m1.md#step-13--g1-round-trip-fixture--final-pass):** the committed
`.vocalboard` G1 fixture + its round-trip test, and the final fmt/clippy/test gate.

## Where Step 11 sits in the edit pipeline (context)

A turn-mutating command flows through three layers (see
[data-model.md § Batched edits](../design/data-model.md#batched-multi-element-edits) and
[§ Undo / redo](../design/data-model.md#undo--redo)):

1. **Command (M4/M5)** — computes *what* the new element(s)/metadata are. Not in M1.
2. **`apply_batch` applier (this step)** — the **producer**. Mutates working clones of the touched
   `TrackTree`s via the Step 6 tree primitives; per op, reads `Location` + `h_old` from
   `tree.element_at_sample(..)` and emits the forward+inverse `Delta` pair at the edit site (Step 7);
   `store::put`s new blobs and appends the forward row(s) in one transaction; then, only on commit,
   builds the new `Arc<TimelineState>`, swaps the live `current`, and calls `History::record`.
3. **`History` (Step 10, already built)** — the **consumer**. `undo`/`redo` swap the state `Arc`
   and append the inverse/forward effect to the journal. It never computes an inverse — the inverse
   was captured by `apply_batch` and stored in the `UndoEntry`.

Step 11 builds layer 2 plus the engine that owns layer 3. Tests synthesize layer-1 edits directly.

## Existing APIs this step builds on (all already implemented in Steps 2–10)

From the Step 10 plan's inventory ([phase1-m1-10.md § Existing APIs](phase1-m1-10.md)) plus Steps 8/9.
**Confirm the exact public names/signatures against the source before calling** — the list below is
the contract as designed, not a guarantee of identical spelling.

- **`db::Db`** — `open(path)`, `conn(&self) -> &Connection`,
  `conn_mut(&mut self) -> &mut Connection` (Step 10), `with_transaction`. `&rusqlite::Transaction`
  derefs to `&Connection`, so the `append_*` / `store::*` signatures work unchanged inside a `tx`.
- **`db::migrations`** — run by `Db::open` (Step 2): WAL + `foreign_keys` + `busy_timeout = 5000`
  pragmas, `user_version` runner, future-version refusal.
- **`db::store`** — `put(conn, &Hash, &[u8]) -> Result<bool, StoreError>` (INSERT OR IGNORE,
  idempotent); `get(conn, &Hash) -> Result<Vec<u8>, StoreError>` (re-hash check).
- **`db::journal`** — read: `latest_snapshot`, `deltas_after`, `latest_metadata`/`MetaRow`. Write:
  `append_delta_batch(conn, CommandId, payload: &[u8], applied_at) -> Result<i64, JournalError>`,
  `append_snapshot(conn, CommandId, &Hash, applied_at) -> Result<i64, _>`,
  `append_metadata(conn, CommandId, &Hash, applied_at) -> Result<i64, _>`.
- **`project::snapshot`** — `TrackTree { Labels(ImplicitTimelineTree<Label>),
  Speech(ImplicitTimelineTree<Turn>) }`; `PerTrackTrees = BTreeMap<u32, TrackTree>`;
  `store_snapshot(&Snapshot) -> (Hash, Vec<u8>)`; `Snapshot`;
  `load_and_replay(&Db, as_of: Option<i64>) -> Result<PerTrackTrees, ReplayError>` (latest snapshot
  + forward `type = 0` rows); `load_latest_snapshot(&Db, as_of) -> Result<(i64, PerTrackTrees), _>`
  (snapshot-only, no forward replay — the recovery path).
- **`project::delta`** — `Delta`, `DeltaOp { InsertAfter, UpdateAfter, DeleteAfter }`,
  `Location { Start, After(Hash) }`, `encode_delta_batch(&[Delta]) -> Result<Vec<u8>, postcard::Error>`.
- **`project::metadata`** — `Metadata`, `store_metadata(&Metadata) -> Result<(Hash, Vec<u8>), _>`,
  `load_current_metadata(&Db, as_of) -> …`, and the **pure** source-file resolver returning the
  missing-track list + any `FoundViaAbsolute` rewrites (`resolve_track_source` / `missing_tracks` /
  `FileResolution` — confirm exact names). M1 returns the list; the Missing-Files dialog is M6.
- **`project::command_id`** — `CommandId` (`Unknown = 0x0`, `Undo`, category variants),
  `code()`, `from_code()`, `UNDO_FLAG = 0x1`, `undo_of()`.
- **`project::undo`** (Step 10) — `TimelineState { trees: PerTrackTrees, metadata: Metadata }`
  (`Clone + Default`); `UndoEntry { before, after, forward_delta, inverse_delta, metadata_changed,
  category }`; `History::{ new(limit), record(entry), undo(&mut current, &mut Connection,
  applied_at) -> Result<bool, HistoryError>, redo(..), can_undo(), can_redo() }`; `HistoryError`.
  These are currently `pub(crate)` + `#[allow(dead_code)]`; the engine is their first real caller.
- **`project::tree`** (Step 6) — `ImplicitTimelineTree<T>`: `from_sorted_elements`,
  `element_at_sample(t: i64) -> Option<ElementHit<T>>`, and the immutable mutation primitives
  `insert_at(at_sample, hash, Arc<T>)` / `update_at(sample, new_hash, Arc<T>)` /
  `delete_at(sample)` (each `&self -> Result<Self, TreeError>`, path-copy + structural sharing).
  **There is no `start_sample_of` / `predecessor` / `successor` method** — the predecessor a delta
  needs is returned *inside* the hit: `ElementHit { hash, element: Arc<T>, in_offset, predecessor:
  Option<Hash> }`. So `apply_batch` resolves a delta `Location` from the hit directly:
  `hit.predecessor.map_or(Location::Start, Location::After)`. (Verified against `tree.rs` lines
  61–73 and 601–680.)
- **`settings`** (`core/src/settings.rs`) — `Settings` with `undo_history_limit`
  (`#[serde(default = "default_undo_history_limit")]`, `DEFAULT_UNDO_HISTORY_LIMIT = 50`, line ~42)
  and `snapshot_idle_seconds`; the settings `version` + `apply_migration` machinery. The engine
  **reads** `undo_history_limit`; it does not change the settings format. (The field is
  `undo_history_limit`, *not* `undo_limit`.)

## `ProjectState` (in `core/src/project/engine.rs`)

```rust
use std::sync::Arc;
use crate::db::Db;
use crate::project::undo::{History, TimelineState};

/// The single owner of one open project: its database handle, the live immutable
/// state (`current`), the in-memory undo/redo history, and the snapshot writer.
/// The only type that mutates the project database.
pub struct ProjectState {
    db: Db,
    current: Arc<TimelineState>,
    history: History,
    sample_rate: u32,
    writer: SnapshotWriter, // see § Snapshot writer
}
```

- `pub` API surface (Step 12 wires these to Tauri): `new_project`, `open_project`,
  `save_snapshot_now`, `undo`, `redo`, plus `apply_batch` (`pub(crate)` — only tests/M5 call it in
  M1) and small read accessors as needed by tests. Doc-comment every `pub` item
  (`#![warn(missing_docs)]` is a hard CI gate).
- **No `unwrap`/`expect`/`panic`** in non-test code without a justifying comment (clippy-gated).
  Return a typed `EngineError` (new enum, hand-rolled `Display` + `std::error::Error` with
  `source()`, mirroring `HistoryError`/`JournalError`/`StoreError` — **no** `thiserror`). Wrap
  `rusqlite::Error`, `StoreError`, `JournalError`, `HistoryError`, `ReplayError`, and
  `postcard::Error`, and add a dedicated recoverable variant for the open-recovery path
  (see `open_project`).

### Wall clock

```rust
/// POSIX seconds (UTC). Called once per mutating command; the value is threaded to
/// every `journal::append_*` in that command's transaction, keeping `journal.rs`
/// clock-free and deterministic in tests (Step 9 decision).
fn now_posix() -> i64 { /* SystemTime::now() since UNIX_EPOCH, saturating to i64 */ }
```

### `new_project(path, sample_rate, settings) -> Result<Self, EngineError>`

1. `Db::open(path)` — creates the file, runs migrations (`user_version = 1`), applies pragmas.
2. In one transaction: write the `project` singleton row (`sample_rate`, `CHECK (id = 1)`); store
   an **initial empty snapshot** blob (`store_snapshot(&Snapshot::default())` → `store::put`) and
   append its `type = 1` row via `journal::append_snapshot(&tx, CommandId::Unknown, &h, now_posix())`.
   A snapshot is not an edit, so `CommandId::Unknown` is the correct stamp.
3. `current = Arc::new(TimelineState::default())` (empty trees + default metadata).
4. `history = History::new(settings.undo_history_limit)`.
5. Start the snapshot writer.

### `open_project(path, settings) -> Result<(Self, OpenOutcome), EngineError>`

Return the open result so the caller (Step 12) can distinguish *opened cleanly* from *opened but
recovered* from *fatal*. `OpenOutcome` carries the missing-track list and an optional recovery
record:

```rust
/// Non-fatal facts about an open. The caller MUST surface a warning when
/// `recovery.is_some()` (Step 12 obligation) — recovery means post-snapshot edits
/// were dropped, and silently ignoring it is silent data loss.
pub struct OpenOutcome {
    /// Track ids whose source file could not be resolved (Missing-Files dialog is M6).
    pub missing_tracks: Vec<u32>,
    /// `Some` iff the journal tail was corrupt and the project was rolled back to a snapshot.
    pub recovery: Option<RecoveryInfo>,
}
pub struct RecoveryInfo { pub failed_row: i64, pub snapshot_id: i64 }
```

**Why this shape (decision — recorded here per the no-ADR model):** the tuple `Result<(Self,
OpenOutcome), EngineError>` is chosen over two alternatives. (a) Putting the recovered `ProjectState`
inside an `EngineError` variant is rejected — it hides a *usable* value in an `Err`, so `?` and
ordinary error handling would discard a good open and every caller would have to special-case the
error path to recover success. (b) A three-variant `enum OpenResult { Opened, Recovered, Failed }`
is cleaner in that "recovered" is impossible to ignore, but it loses `?` ergonomics for the engine's
own internal callers and tests, and still needs `missing_tracks` on the clean-open arm. The tuple
keeps **`Err` meaning unambiguously "no project opened,"** never hides state in an error, and carries
`missing_tracks` uniformly. The cost — that a careless caller could ignore `.recovery` — is bought
down by the **Step 12 obligation** above (the only consumer in M1 is one Tauri handler we control).
**Do not** smuggle a usable state inside an `Err`.

1. `Db::open(path)` (migrations run; a future-`user_version` file is refused by Step 2's runner).
2. **Happy path:** `let trees = load_and_replay(&db, None)?;`
   `let meta = load_current_metadata(&db, None)?;`
3. Source resolution (pure, no DB I/O): run the metadata resolver against the project directory to
   compute the missing-track list and any `FoundViaAbsolute` relative-path rewrites. **Persist a
   rewrite** as a `type = -1` metadata row (`store_metadata` → `store::put` → `append_metadata(&tx,
   CommandId::Unknown, &h, now)`), since the on-disk relative path changed. Put the missing-track
   list in `OpenOutcome` (Step 12 surfaces it; the Missing-Files dialog is M6).
4. `current = Arc::new(TimelineState { trees, metadata: meta })`;
   `history = History::new(settings.undo_history_limit)` (empty — history is not persisted).
5. **Corrupt-journal recovery (the important branch).** If step 2's `load_and_replay` fails (a
   `type = 0` row whose payload won't decode, a missing blob, a hash mismatch), **fall back** to
   `load_latest_snapshot(&db, None)?` (snapshot-only, no forward replay), build a usable
   `ProjectState` rolled back to that snapshot, and return it with `OpenOutcome.recovery =
   Some { failed_row, snapshot_id }`. Edits after the snapshot are lost; the user is informed
   (Step 12) and can keep working — a subsequent `save_snapshot_now` writes a fresh snapshot, after
   which the abandoned `type = 0` rows are journal-tail garbage the M5+ compaction step prunes. **If
   `load_latest_snapshot` itself fails, it is unrecoverable** — return a fatal `Err` and construct
   no `ProjectState`.

### `save_snapshot_now() -> Result<(), EngineError>`

Clone `current` (O(1) `Arc` bump) and hand it to the snapshot writer (see below). The writer
flattens each `TrackTree` to its ordered `Vec<Hash>`, builds a `Snapshot`, `store_snapshot` →
`store::put`, and `append_snapshot(&tx, CommandId::Unknown, &h, now_posix())` — all on the writer's
own connection. `save_snapshot_now` blocks until that write commits (so callers/tests observe the
row); the app-exit trigger uses the same path.

### `apply_batch(&mut self, ops, category) -> Result<(), EngineError>` — the producer (Step 11b)

`ops` is a synthetic edit description (a small test-facing type; real semantics arrive in M4/M5).
Per [data-model.md § Batched edits](../design/data-model.md#batched-multi-element-edits):

1. Work on clones of the touched `TrackTree`s. **Apply ops in descending sample order** over
   *original-tree-coordinate* positions, so earlier edits don't shift the positions of later ones.
2. For each op: resolve `Location` (`Start` or `After(predecessor_hash)`) and `h_old` from
   `original_tree.element_at_sample(sample)` on the **original** tree (see the dual-representation
   note); mutate the **working** tree via the Step 6 primitive; emit the **forward** `Delta` and its
   **inverse** at the edit site (`InsertAfter`↔`DeleteAfter`; `UpdateAfter h_new`↔`UpdateAfter
   h_old`).
3. In one transaction on `self.db`: `store::put` each new element blob; `append_delta_batch(&tx,
   category, &encode_delta_batch(&forward)?, now_posix())`. Commit.
4. Only after commit: build the new `Arc<TimelineState>` (swap in the rebuilt `TrackTree`s + any
   changed metadata), set `self.current`, and
   `self.history.record(UndoEntry { before, after, forward_delta: Some(forward),
   inverse_delta: Some(inverse), metadata_changed, category })`.

This is the mirror of the Step 10 `History` consumer: the inverse it stores is exactly what `undo`
later replays. `History::record` already no-ops/evicts per `undo_history_limit` (including the
`limit == 0` "undo disabled" case — see [§ Undo limit = 0](#undo-limit--0)).

#### ⚠️ Two-representation footgun: tree primitives speak *samples*, deltas speak *`Location`*

The tree mutation primitives are keyed by an `i64` **sample** (`insert_at(at_sample, …)`,
`update_at(sample, …)`, `delete_at(sample)`), but the journal `Delta` is keyed by a **`Location`**
(`Start | After(Hash)`). `apply_batch` straddles both for the *same* op:

- **Mutate** the working tree with the sample.
- **Record** the delta with the `Location` derived from the *original* tree's
  `element_at_sample(sample)` hit: `let hit = original.element_at_sample(s)?;`
  `let loc = hit.predecessor.map_or(Location::Start, Location::After);` and `h_old = hit.hash`.

Do **not** read the `Location`/predecessor off the working (already-mutated) tree, and do **not**
re-resolve samples against it mid-batch. Resolve *everything* against the original tree first
(descending order makes this sound — see below), then apply.

#### Why descending order (and what "original coordinate" means)

[data-model.md § Batched edits](../design/data-model.md#batched-multi-element-edits) (the rationale is pinned
at data-model.md line ~311): applying highest-sample-first means every already-applied op lies
strictly to the right of the current one, so it cannot shift the current op's coordinate — original
positions stay valid for the whole batch with no re-resolution. **Forward deltas are recorded in
application order** (descending), because journal replay re-applies them in that order. The inverse
batch is each op's inverse with the **batch order reversed**. The `delta.rs` test
`mixed_track_ids_coexist_in_batch` (delta.rs ~line 881) demonstrates the per-`track_id` split the
engine performs ("mirroring what the Step 11 engine will do").

#### Worked example (single speech track, two ops in one batch)

Original track (boundary samples in brackets): `[0] A [100] B [250] C [400]` — A spans 0–100, B
100–250, C 250–400; hashes `hA, hB, hC`.

Batch = { **update B → B′** (at any sample in B's interval, e.g. 150), **delete C** (e.g. 300) }.

1. Sort descending by original sample: process **delete C (300)** before **update B (150)**.
2. **delete C @ 300:** `hit = original.element_at_sample(300)` → `{ hash: hC, predecessor: Some(hB) }`.
   - forward: `Delta::delete_after(track, Location::After(hB))`
   - inverse: `Delta::insert_after(track, Location::After(hB), hC)`
   - working tree: `working = working.delete_at(300)?` → `[0] A [100] B [250]`.
3. **update B @ 150:** resolve against the **original** tree: `hit =
   original.element_at_sample(150)` → `{ hash: hB, predecessor: Some(hA) }`. Build B′, `hB2 =
   store_turn(&b2)`.
   - forward: `Delta::update_after(track, Location::After(hA), hB2)`
   - inverse: `Delta::update_after(track, Location::After(hA), hB)`
   - working tree: `working = working.update_at(150, hB2, b2)?` → `[0] A [100] B′ [250]`.
4. **forward batch (application order = descending):** `[ delete_after(After(hB)),
   update_after(After(hA), hB2) ]` — `store::put(hB2)`, then `append_delta_batch`.
5. **inverse batch (reverse of application order):** `[ update_after(After(hA), hB),
   insert_after(After(hB), hC) ]` — stored in the `UndoEntry`, applied verbatim by `History::undo`.
6. Build the new `TimelineState` from the final `working` trees; swap `current`; `history.record`.

Sanity: replay of `[snapshot, …forward…]` reproduces the post-edit tree `[A, B′]`; `undo` appends
`…inverse…` and replay of `[snapshot, forward, inverse]` reproduces the pre-edit `[A, B, C]`.

#### Boundary-alignment note for the synthetic-edit generator / test helpers

`insert_at(at_sample, …)` errors `SampleNotOnBoundary` if `at_sample` is strictly interior to an
element, and `SampleOutOfRange` if `at_sample > total_duration()` (insert) or `>= total_duration()`
(update/delete). So the test edit-generator must pick **element-boundary samples** for inserts
(`0`, or the cumulative start of an existing element) and any in-interval sample for update/delete
(the primitive maps it to the covering element; `element_at_sample` uses the same covering rule, so
forward/inverse resolution and the mutation agree). Keep the synthetic `ops` type expressing
positions the same way the tree does to avoid off-by-one mismatches.

#### Write-cost of heavy edits (one transaction is the right default)

Unlike the snapshot writer — which writes a hash-list only, because the element blobs are already in
`store` from edit time ([data-model.md line ~305](../design/data-model.md)) — `apply_batch` must persist a
**brand-new blob per touched element** inside the command transaction (content changed ⇒ new hash, so
`INSERT OR IGNORE` gives no dedup benefit). A "touches every turn" edit (e.g. a future
`remove_disfluencies` over the whole project) therefore approaches a full-store rewrite at write time.
Sizes from `data-model.md`: a `Turn` ≈ ~0.5 KB postcard (~12 words/turn; two `f64`s/word dominate),
one `Delta` ≈ ~35 B. At ~750 turns/hour of speech:

| Project | Turns | New blob bytes | Delta row | Est. txn time (SSD) |
|---|---|---|---|---|
| 1 hr | ~750 | ~0.4 MB | ~26 KB | ~20–50 ms |
| 3 hr | ~2,250 | ~1.1 MB | ~80 KB | ~50–120 ms |
| 10 hr audiobook | ~7,500 | ~3.8 MB | ~260 KB | ~100–250 ms |
| 20 hr (pathological) | ~15,000 | ~7.5 MB | ~525 KB | ~200–450 ms |

Dominant terms at 15k turns: postcard serialize ~50–120 ms; N `INSERT OR IGNORE` in **one** WAL
transaction (prepared stmt, page-cache-resident index) ~60–150 ms; tree path-copies ~10–30 ms; BLAKE3
<10 ms; **one** fsync at commit ~1–10 ms SSD (the single sync is the whole point — far cheaper than a
sync-per-row loop). So ~50–150 ms for realistic podcasts, sub-second even for a pathological 20-hr
project.

**Keep the whole edit in one atomic transaction** (the algorithm above) — it gives all-or-nothing
atomicity *and* a single fsync, and is actually faster than splitting. What this does **not** do is
freeze the UI: the Chromium webview paints on its own process (never frozen by Rust work), and the
cpal callback never touches SQLite (playback unaffected). The only thing a long synchronous command
can stall is **Tauri's event loop**.

> **⚠️ Step 12 obligation (flag for the Tauri-wiring author):** dispatch heavy edit commands **off
> the event loop** — an `async` `#[tauri::command]` with `tokio::task::spawn_blocking` (or a dedicated
> worker), not a synchronous handler. Then even the pathological case is just a slightly delayed
> "edit applied," not a window stutter. M1 itself never hits this (no real edit command exists until
> M4/M5; `apply_batch` is exercised synchronously in tests), so this is purely a forward note.

**Deferred mitigations (recorded, not built — only if profiling later shows a problem on huge
projects):** (1) Decouple blob writes from the delta append — the content-addressed store is
monotonic, so a blob with no referencing delta row is harmless garbage the M5+ compaction step
collects; one could `store::put` all new blobs first (even in autocommit) then append the single delta
row in a tiny transaction, shrinking lock-hold time at the cost of a second fsync (usually a wash).
(2) Promote known-huge edits to cancellable background tasks with progress (complicates undo, since
the `UndoEntry` is one unit). Neither is needed for M1.

#### Why not a journal-as-snapshot interface for heavy edits

A tempting alternative for whole-project edits is a *second* producer interface: the command builds a
fully-edited in-memory tree and the journal entry **is** a snapshot (no delta batch), so reopen starts
from a fresh snapshot with no trailing deltas to replay. **Rejected — use the existing expensive-op
snapshot trigger instead.** Rationale (recorded here per the no-ADR model):

- **It breaks the Step 10 undo invariant.** The undo model is built on *delta inverses* —
  "[full snapshots are no longer used as inverses](../design/data-model.md)" (data-model.md line ~438); every
  `UndoEntry` carries a forward+inverse `Delta` pair. A snapshot-only edit produces neither, so it
  would be either non-undoable (a sharp exception to "everything is undoable") or would have to
  reconstruct the inverse delta batch anyway — at which point all the delta-construction work is done
  and nothing is saved.
- **No write savings** — every new blob is written either way (you noted this); the only difference is
  one large delta-row blob (~5–7% of the edit's bytes), which is noise in the budget above.
- **The reopen benefit already exists without a second interface.** `data-model.md` line ~445 already
  specifies "*snapshot immediately after an expensive op*" (`identify_disfluencies`,
  `remove_disfluencies`, `remove_sounds`, `align_tracks`). The command handler simply calls
  `apply_batch(...)` then `save_snapshot_now()` — both already in this plan — leaving a recent snapshot
  with ~0 trailing deltas. That is a two-call sequencing decision in the command, not a new persistence
  path.
- **A second mutate-the-project path doubles the bug surface** in the most safety-critical layer (two
  replay interactions, two things mutation-testing must cover, "which path did this command use?" on
  every edit). The delta path is the one with pinned wire formats and heavy tests.

**M1 scope:** `apply_batch` is the single edit interface, and it must merely *compose* with
`save_snapshot_now` (both built here). No real heavy edit command exists until M4/M5, so **wiring
specific commands to auto-snapshot after a heavy `apply_batch` is an M4/M5 responsibility** (the
trigger is already specified at data-model.md line ~445) — flagged here as a forward dependency, not
built in M1.

### `undo()` / `redo()`

```rust
pub fn undo(&mut self) -> Result<bool, EngineError> {
    self.history
        .undo(&mut self.current, self.db.conn_mut(), now_posix())
        .map_err(EngineError::History)
}
```

`history`, `current`, and `db` are disjoint fields, so the three mutable borrows are allowed
(Step 10 `conn_mut` note). `redo` is symmetric. Both return `false` when the respective stack is
empty.

## Snapshot writer

Per [phase1-m1.md Step 11 threading note](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectengeners)
and the "Decisions to lock first" entry (explicit + app-exit triggers only; **idle timer deferred to
M5**):

- A `SnapshotWriter` owning its **own `rusqlite` connection** to the same file. WAL allows one
  concurrent reader while writers serialize via SQLite locks; no edits run concurrently in M1, so
  this is safe. Document the single-writer constraint where the writer is created.
- The expensive work is **lock-free**: the O(1) `current.clone()` handoff and the O(n) flatten +
  postcard + BLAKE3 happen with no connection held. Only the final two-INSERT write (snapshot blob
  + `type = 1` row; a `Vec` of 16-byte hashes, ≲ ~1 MB even for huge projects) takes the write lock,
  for single-digit ms. **No write queue is needed.**
- From M5 on, a synchronous main-thread edit write can race the snapshot commit; the
  `busy_timeout = 5000` pragma (Step 2) makes the loser wait-and-retry rather than hit `SQLITE_BUSY`.
  The cpal callback never touches SQLite, so playback latency is unaffected — note this where the
  writer is documented.
- M1 may implement the writer **synchronously** (a method on a dedicated connection invoked by
  `save_snapshot_now` and at app-exit) as long as the connection-ownership + lock-free-handoff shape
  is in place; a background thread is acceptable but not required since there is no idle trigger yet.
  Whichever is chosen, structure it so M5 can attach the idle-autosave timer and so the cpal
  real-time path is never blocked. Record the choice in the module doc-comment.

## Undo limit = 0

`undo_history_limit == 0` is a **valid, intentional** setting: the Step 10 `History` treats it as
"undo disabled" and skips recording, so no undo is captured and undo/redo become no-ops. Reversions
must then be made manually (or, later, via the Time Machine feature). The engine simply passes the
configured value to `History::new` — **no special handling here, and the backend must not reject 0.**

⚠️ The **user-facing warning** belongs to the settings dialog, which lands in **M3** (not this
step): entering `0` there must trigger a confirmation dialog so the user explicitly acknowledges
that undo will be disabled. That requirement is recorded in
[`design/frontend.md`](../design/frontend.md) (settings dialog bullet) and in
[data-model.md § Undo / redo](../design/data-model.md#undo--redo). Nothing for Step 11 to build —
this note exists so the engine author does not "helpfully" clamp or reject `0`.

## Dead-code cleanup

Step 11 is the first genuine non-test caller of the Step 8/9/10 plumbing, so **remove** the
`#[allow(dead_code)]` attributes on (per
[phase1-m1.md Step 11](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectengeners)):
`db::store::put`, `db::store::get`, `Db::conn`, `db::journal::append_*`, `latest_metadata`,
`MetaRow`, `load_current_metadata`, `missing_tracks`/`resolve_track_source`/`FileResolution`, and the
Step 10 `undo::{History, UndoEntry, TimelineState}`, `CommandId::undo_of`, `Db::conn_mut`. After
removal, `cargo clippy -p core -- -D warnings` must stay green — anything *still* flagged unused
means a call site is missing (fix the call site; don't re-add the allow). Keep `#[allow(dead_code)]`
**only** where a deliberate M1/M5 gap genuinely remains, and record that gap in this file +
`phase1.md`.

### Remaining `#[allow(dead_code)]` after the 11a sweep (deliberate gaps)

The sweep must reach the plumbing the engine calls **transitively**, not just the names it spells
out directly: wiring `open_project`/`save_snapshot_now`/`undo`/`redo` makes the entire
`load_and_replay` / `load_latest_snapshot` / `snapshot_from_trees` / `History::{new,undo,redo}`
chain live, so the allows on `snapshot::{snapshot_adjacency, replay_into, build_trees,
build_track_tree, load_and_replay, load_latest_snapshot, snapshot_from_trees}`,
`delta::{apply, apply_one, encode_delta_batch, decode_delta_batch, LATEST_DELTA_VERSION,
AdjacencyList}`, `journal::{latest_snapshot, deltas_after}`, `undo::append_effect`, and
`command_id::{from_code, UNDO_FLAG}` all come off too. (Clippy stays green either way — an allow on a
now-used item is silently redundant — so these must be removed by inspection, not by leaning on the
lint.)

The following allows are intentionally **kept** after the sweep, each guarding a genuine
not-yet-wired gap (verified to fail `clippy -p core -- -D warnings` if removed):

- `undo::History::record` — first non-test caller is `apply_batch` (**Step 11b**).
- `undo::History::can_undo` / `can_redo` — first caller is the **M4/M5** Tauri undo/redo command that
  exposes UI enable/disable state (Step 12 wired only the three lifecycle commands; undo/redo has no
  command surface until the first turn-mutating edit lands).
- `journal::{SnapshotRow, DeltaRow, MetaRow}::{command_id, applied_at}` (field-level) — read by the
  **Step 12+** history-view feature; only `id`/`hash`/`payload` are read in M1.
- `metadata::FileResolution::{Found(path), FoundViaAbsolute{path}}` (field-level) — the resolved /
  absolute paths are surfaced by the **M6** Missing-Files dialog; M1 reads only `new_relative`.
- `delta::AdjacencyList::{len, is_empty, head, successor}` — the read-side query API, exercised by
  unit tests and reserved for `apply_batch` (**Step 11b**) / diagnostics. Moved into a dedicated
  `impl AdjacencyList` block under a single allow so the live constructors/`iter` stay unguarded.
- `db::Db::with_transaction` — a Step 2 convenience helper used only by tests; the engine uses raw
  `conn.transaction()`. Pre-existing; left as-is (no engine call site is *missing* — it is simply a
  helper the engine chose not to use).
- `metadata::metadata_is_canonical` — a `debug_assertions`-only invariant check (`cfg_attr`).

## Module wiring

- `pub mod engine;` in `core/src/project/mod.rs` (alphabetical — it goes before `command_id`).
- Re-export `ProjectState` (and `EngineError`, `OpenOutcome`) from the crate as the Step 12 surface
  only if that matches the existing `core/src/lib.rs` re-export style; otherwise leave them under
  `crate::project::engine` and let Step 12 import the path. Match the existing convention — check
  `core/src/lib.rs` before adding re-exports.

## Step 11c detailed (db module and tree last_hash)

Review-driven follow-ups to the committed 11a/11b — the mechanical half (review issues 1, 2, 4),
landing as commit `1M1-11c`, green before
[Step 11d](#step-11d-detailed-apply_batch-metadata-producer) begins. The "Existing APIs" and
two-representation-footgun sections above still apply unchanged.

### 11c · Connection-opening goes through the `db` module (review issue 1)

**Problem.** `SnapshotWriter::open` (`engine.rs`) calls `Connection::open` + the
WAL/foreign-keys/busy-timeout pragma string directly, duplicating `Db::open` — the pragma string now
lives in three places (`db/mod.rs`, `engine.rs`, and the recovery test). The engine also reaches past
the `db` modules to run raw `project`-table SQL.

**Fix.**

1. Add a private helper in `db/mod.rs` — the **single** place `Connection::open` + the pragma
   `execute_batch` ever appear in production code:
   ```rust
   fn open_conn(path: &Path) -> rusqlite::Result<Connection> {
       let conn = Connection::open(path)?;
       conn.execute_batch(
           "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
       )?;
       Ok(conn)
   }
   ```
2. `Db` gains a `path: PathBuf` field (remembered at open/create; cheap and useful for diagnostics).
3. Constructors (existence policy in issue 2 below):
   - `pub fn create(path) -> Result<Self>` — `open_conn` + `migrations::run`; stores `path`.
   - `pub fn open(path) -> Result<Self>` — `open_conn` + `migrations::run`; stores `path`.
   - `pub(crate) fn open_shared(&self) -> Result<Self>` — `open_conn(&self.path)` only, **no
     migrations**: the snapshot writer's second connection to the already-open project.
4. `SnapshotWriter` holds a `Db`, not a bare `Connection`: `struct SnapshotWriter { db: Db }`, built
   as `SnapshotWriter { db: db.open_shared()? }` in `new_project`/`open_project`. Call `open_shared`
   on the local `db` **before** moving `db` into `ProjectState` (immutable borrow — no conflict).
   `write` uses `self.db.conn_mut().transaction()`.
5. Migrate the recovery test's raw injection connection (`engine.rs` ~line 980) to `Db::open` so no
   code path outside `Db` opens a raw connection. (Lower-level unit tests in `db/mod.rs` /
   `migrations.rs` that exercise `migrations::run` directly may keep their own `Connection`.)

**Why `open_shared` is a method on `&Db` (decision — no-ADR model).** It is structurally
uncallable without a `Db` in hand, and the only way to obtain a `Db` is `open`/`create`. So "the
writer only attaches *after* the primary has opened+migrated the file" is enforced by the type, not
by convention — and the primary opener is the only thing that migrates. The one residual case (the
file deleted/swapped externally between the two opens) cannot be prevented in-process and self-detects
loudly on the writer's first `write` (missing tables), so **no runtime `user_version` check is added**
to `open_shared`. Named constructors are used rather than a `migrate: bool` flag because the milestone
now has two axes (existence policy × migrate-or-not) and a boolean reads poorly at the call site.

### 11c · `db::project` module for the singleton row (review issue 1, cont.)

Move the raw `project`-table SQL out of `engine.rs` into a new `src-tauri/core/src/db/project.rs`
(`pub(crate) mod project;` in `db/mod.rs`, placed after `migrations`, before `store`):

- `read_sample_rate(conn: &Connection) -> rusqlite::Result<u32>` — replaces the inline
  `SELECT sample_rate FROM project WHERE id = 1` in `open_project`.
- `insert_project_row(conn: &Connection, sample_rate: u32) -> rusqlite::Result<()>` — replaces the
  inline `INSERT INTO project …` in `new_project` (keep the existing column set and `datetime('now')`
  stamps).

This keeps every table behind a schema-aware module (like `store`/`journal`) and leaves `engine.rs`
free of embedded SQL. Refer to it qualified (`db::project::read_sample_rate`) to avoid `use`-level
confusion with the top-level `crate::project` module. (`migrations.rs`'s best-effort `min_app_version`
read stays where it is — it runs during migration, before the engine is involved.)

### 11c · `Db::create` vs `Db::open` existence policy (review issue 2)

**Problem.** Today both `new_project` and `open_project` call `Db::open`, whose `Connection::open`
creates the file if absent and opens it if present. So `new_project` on an existing project surfaces
only a cryptic `project.id` PK-constraint `SqliteError`; `new_project` on an unrelated SQLite file
silently migrates schema *into* it; `open_project` on a missing path silently creates an empty one.

**Fix.** Split the existence policy across the two constructors (the check lives only in `Db` — no
duplication):

- `Db::create(path)` — error if `path` already exists (`path.try_exists()`); else `open_conn` +
  migrate. `new_project` calls this.
- `Db::open(path)` — error if `path` is absent; else `open_conn` + migrate. `open_project` calls this.

Surface the two conditions as distinct, typed errors so the Step 12 Tauri layer can show proper
messages: add `EngineError::ProjectFileExists { path }` and `EngineError::ProjectFileNotFound { path }`
(`Display` + `source()` like the other variants — `source()` is `None`). `Db::{create,open}` return
the two conditions distinguishably and the engine maps them; keep `Db` below `EngineError` in the
layering (return a small `db`-level error the engine maps, or `anyhow` with a stable downcastable
sentinel — Sonnet's choice, consistent with `Db`'s current `anyhow::Result` style — as long as the
two cases are testable and distinct from the catch-all `OpenDb`). TOCTOU between the existence check
and the open is acceptable for a desktop file-open.

### 11c · `ImplicitTimelineTree::last_hash` (review issue 4)

**Problem.** `insert_location_in_tree` (`engine.rs`) resolves the append predecessor with
`tree.iter().last()`, which the default `Iterator::last` walks in **O(n)** (every element), making a
build-by-repeated-append O(n²). The O(log n) right-spine walk already exists privately as
`rightmost_hash` (`tree.rs` ~line 463).

**Fix.** Add a public accessor on `ImplicitTimelineTree<T>`:
```rust
/// Content hash of the last element in timeline order, or `None` if empty. O(log n).
pub fn last_hash(&self) -> Option<Hash> { rightmost_hash(&self.root) }
```
and change `insert_location_in_tree`'s append branch to
`tree.last_hash().map_or(Location::Start, Location::After)`.

### 11c · Test plan

- `Db::create` on a non-existent path succeeds; on an existing file returns the typed "already
  exists" error and does **not** mangle the file. `Db::open` on an existing project succeeds; on a
  missing path returns the typed "not found" error.
- `open_shared` returns a working second connection to an open project (e.g. it reads a row the
  primary wrote); the writer round-trip still passes (`save_snapshot_now` → reopen).
- `db::project::{insert_project_row, read_sample_rate}` round-trip on a fresh `Db`.
- `last_hash`: `None` on empty, the sole element on a singleton, the rightmost on a multi-element
  tree, and **equal to `iter().last().map(|e| e.hash)`** across a randomized tree (pins the cheap
  path to the semantic one). The existing append-order replay assertion (AB5) keeps passing after
  switching `insert_location_in_tree` to `last_hash`.

### 11c · Verification & commit

- `cargo fmt --check`, `cargo clippy -p core -- -D warnings` (incl. `missing_docs` — doc-comment
  `last_hash`, the new `Db` methods, the `db::project` fns, and the new `EngineError` variants),
  `cargo test -p core` green.
- One unsigned commit on `claude/1M1` (GPG-by-branch policy):
  `1M1-11c: route connection-opening through db module + create/open policy + tree last_hash`.

## Step 11d detailed (apply_batch metadata producer)

Builds on a green 11c — the novel-logic half (review issue 3), with its own focused mutation pass.
Lands as commit `1M1-11d`.

### 11d · `apply_batch` journals metadata in the same transaction (review issue 3)

**Why now, not M5 (decision — no-ADR model).** Adding a speech track is a *combined* edit: the
track's `TrackMeta` and its **entire** transcript (a full turn `Delta` batch) enter project state
atomically — there is no state in which a speech track exists without its transcript (only a
pre-commit ML-processing delay). So the combined tree+metadata transaction is foundational, not
hypothetical. The seam is fully pinned by the persistence format (metadata is a whole `type = -1`
blob, not a delta) and the **consumer half already exists and is tested**: `undo::append_effect`
writes the `type = 0` and `type = -1` rows in one transaction, and `UndoEntry` already carries
`metadata_changed` + before/after `TimelineState`. Leaving the producer unable to emit metadata is
the same half-built-seam footgun this review removed elsewhere — it invites a future author to write
metadata *outside* the batch transaction and break atomicity. And `apply_batch` is *already* built
ahead of its M4/M5 callers and validated synthetically, so extending it is the existing posture, not
new speculation.

**Signature.** Add a metadata parameter:
```rust
pub(crate) fn apply_batch(
    &mut self,
    ops: &[BatchOp],
    metadata: Option<Metadata>,   // Some(new) ⇒ metadata changed to `new`; None ⇒ unchanged
    category: CommandId,
) -> Result<(), EngineError>
```
All existing call sites (tests) pass `None`.

**Behaviour changes** (mirror `append_effect`'s 0–2-row structure):

- Early-return only when there is genuinely nothing to do:
  `if ops.is_empty() && metadata.is_none() { return Ok(()); }`.
- Inside the existing single transaction:
  - append the `type = 0` delta row **only if `!forward_deltas.is_empty()`** (metadata-only edits
    have no delta row);
  - if `metadata` is `Some(new)`: `store_metadata(&new)` → `store::put(&tx, …)` →
    `journal::append_metadata(&tx, category, &h, now)` — the **same `category` stamp** as the delta
    row, in the **same `tx`**.
- Build `new_state.metadata` from `new` when `Some`, else `self.current.metadata.clone()`.
- `UndoEntry`: `metadata_changed: metadata.is_some()`; `forward_delta`/`inverse_delta` = `None` when
  `ops` is empty (so `append_effect` writes no delta row on undo/redo), else `Some(...)` as today.
  `before` (old metadata) and `after` (new metadata) carry the revert/reapply states —
  `append_effect` re-derives the metadata blob from them.

This supersedes 11b's hardcoded `metadata_changed: false`; the `apply_batch` detail above (step 4,
"+ any changed metadata") already anticipated this shape.

**Scope boundary — what stays M5.** 11d adds only the *producer capability* + synthetic tests. The
actual `add_track`/`remove_track` **commands** (the callers) and the **load-time tree↔metadata
reconciliation guard** ([data-model.md § Load](../design/data-model.md), line ~380) with its round-trip
fixtures remain M5. Because reconciliation does not exist yet, **11d's synthetic metadata tests must
use changes that do not orphan a tree** (e.g. a `ProjectMeta` field like project name, or a
speaker-name change) — never a `remove_track`-shaped edit that drops a `TrackMeta` while leaving its
tree, which cannot round-trip cleanly until the guard lands.

### 11d · Design-doc updates (same commit)

Per CLAUDE.md (specs stay authoritative; update downstream milestones when shortcuts move):

- **data-model.md** — state explicitly that **adding a speech track is an atomic combined edit** (its
  `TrackMeta` + the full transcript turn batch commit together; no track-without-transcript state),
  and that `remove_track` is metadata-only (orphaned tree reconciled at load). Update the § Load note
  (line ~380): the producer (`apply_batch`) **can** journal a metadata change alongside a delta batch
  as of 11c/11d; what remains M5 is the track *commands* + the reconciliation guard + their round-trip
  fixtures. Keep the note that M1 synthetic metadata tests avoid orphaning trees.
- **phase1-m1.md** (Step 11) and **phase1.md** (M5) — record that the combined-edit *producer support*
  landed in 11d; M5 owns the track commands, the reconciliation guard, and the fixtures.

### 11d · Test plan

- **Combined edit** (add_track shape): one `apply_batch` inserting turn(s) on a new speech track
  **and** a non-orphaning metadata change. Assert both a `type = 0` and a `type = -1` row were
  appended in that one call; `current` reflects both; `load_and_replay` trees and
  `load_current_metadata` agree; `undo` reverts **both** trees and metadata; `redo` reapplies both;
  replay agrees after each.
- **Metadata-only edit** (empty `ops`, `Some` metadata): assert **no** `type = 0` row and exactly one
  `type = -1` row; `current.metadata` updated; undo/redo revert/reapply metadata; `load_current_metadata`
  agrees after each.
- **No-op**: `ops` empty + `metadata` `None` ⇒ `Ok`, no new rows.

### 11d · Verification & commit

- `cargo fmt --check`, `cargo clippy -p core -- -D warnings` (incl. `missing_docs`), `cargo test -p
  core` green.
- `cargo-mutants` scoped to the new `apply_batch` branches (conditional delta-row append, the metadata
  append, the `metadata_changed` flag, the after-state metadata selection) — aim for no survivors.
- One unsigned commit on `claude/1M1` (GPG-by-branch policy), after `1M1-11c` is green:
  `1M1-11d: apply_batch metadata producer (combined + metadata-only edits)`.

## Test plan

Integration tests live in `core/tests/` (cross-cutting lifecycle); pure helpers may stay inline.
Implement fully, then validate with mutation testing on the new logic (skip red-green stubs — see
the project's TDD norm); any bug found gets a failing-first regression test.

### Lifecycle round-trip (`core/tests/engine_lifecycle.rs`)

**11a version** (no `apply_batch` yet — drive state through synthetically-recorded history):

1. `new_project(tmp, sample_rate, settings)`.
2. Mutate `current` and `history` the way Step 10's tests do: build new `TrackTree`s by hand, write
   the forward row(s) directly, and `history.record(UndoEntry { … })` — i.e. stand in for the 11b
   producer. (Step 10 already proved `History` round-trips against a hand-built `Db`.)
3. Snapshot a copy of `current` (clone the `Arc`) for later comparison.
4. `save_snapshot_now()`.
5. Drop the `ProjectState`; `open_project(tmp, settings)`.
6. Assert the reopened `current.trees == saved.trees` and `current.metadata == saved.metadata`
   (uses the `TimelineState`/tree sequence-equality `PartialEq`), and `OpenOutcome.recovery` is
   `None`.
7. `undo()` then `redo()` and assert `current` matches the expected pre/post states at each step,
   and that `load_and_replay(&db, None)` reproduces the same trees (undo/redo are journal-recorded).

**11b version** (extend the same test): replace the hand-recorded step 2 with **real `apply_batch`
calls** across **two tracks** (a speech track and track 0 / labels) — inserts, an update, a delete —
exercising descending-sample ordering with a multi-op batch. Everything downstream (3–7) is unchanged
and now also proves the producer's forward/inverse capture survives a reopen + undo/redo.

### Journal-corruption recovery (`core/tests/engine_recovery.rs`, 11a)

1. `new_project` → `apply_batch` → `save_snapshot_now` (a known-good snapshot) → `apply_batch` again
   (a post-snapshot edit, so there is a `type = 0` row after the snapshot).
2. Hand-doctor that trailing `type = 0` row so its payload won't decode (write garbage via a raw
   `rusqlite` connection in the test).
3. Drop + `open_project`: assert it returns `Ok` with `OpenOutcome.recovery = Some { failed_row,
   snapshot_id }`, and the resulting `ProjectState` equals the post-snapshot, pre-corruption state.
4. Assert a **fatal** path too: corrupt the snapshot blob itself and assert `open_project` returns
   `Err` and constructs no `ProjectState`.

### Settings / undo-limit (11b — needs the real producer)

- `apply_batch` with `settings.undo_history_limit = 0`: assert `history.can_undo()` is false and
  `undo()` returns `Ok(false)` (undo disabled), while the edit itself still persisted (the journal
  has the forward row and `load_and_replay` reflects the edit). Pins the "0 disables undo, but edits
  are not blocked" semantics.
- `apply_batch` past a small limit (e.g. 2): assert the oldest undo is evicted (delegates to the
  Step 10 `History` behaviour, verified through the engine).

### Snapshot writer (11a)

- `save_snapshot_now` writes exactly one new `type = 1` journal row; calling it twice with no
  intervening edit still yields a valid latest snapshot (the unchanged blob is reused via
  `store::put` INSERT OR IGNORE, but the journal row is still appended).

## Verification

- `cargo fmt --check` and `cargo clippy -p core -- -D warnings` (incl. `unwrap_used`,
  `expect_used`, `panic`, `missing_docs`) stay green — in particular after removing the
  `#[allow(dead_code)]` attributes.
- `cargo test -p core engine::` and the new `core/tests/engine_*` integration tests pass.
- `cargo test -p core` — no regression in `snapshot::`, `undo::`, `journal::`, `db::`.
- `cargo-mutants` (11b) on `engine.rs` `apply_batch` (ordering + delta/inverse capture); also worth
  running over the 11a recovery branch and `new_project`/`save_snapshot_now` stamping — aim for no
  surviving mutants in the persistence-critical paths.
- **Commits on `claude/1M1`, all unsigned** per the GPG-by-branch policy
  ([CLAUDE.md](../CLAUDE.md)), each green before the next begins:
  - `1M1-11a: ProjectState engine + lifecycle + recovery + dead-code sweep`
  - `1M1-11b: apply_batch producer (descending-order, inverse capture)`
  - `1M1-11c` — review follow-ups (db module + tree `last_hash`); detail + verification in
    [§ Step 11c detailed](#step-11c-detailed-db-module-and-tree-last_hash).
  - `1M1-11d` — review follow-up (`apply_batch` metadata producer); detail + verification in
    [§ Step 11d detailed](#step-11d-detailed-apply_batch-metadata-producer).
- Run `cargo fmt --check` / `clippy -D warnings` / `cargo test -p core` at **each** commit
  boundary, not just at the end — 11a must be independently green.

## Documentation touches

- Behaviour is already specified in [data-model.md](../design/data-model.md); no spec change is expected. If
  implementation forces a field/behaviour adjustment, update `data-model.md` **in the same commit**
  (it stays authoritative).
- Doc-comment the `engine.rs` module header with: the producer/consumer split, the
  "undo is a forward-recorded edit" invariant, the single-writer + lock-free-handoff constraint, and
  the open-recovery contract.
- The M3 undo-limit-zero warning note has already been added to [`frontend.md`](../design/frontend.md) as a
  cross-doc consistency edit accompanying this plan (see [§ Undo limit = 0](#undo-limit--0)).

## Out of scope (deferred)

- **30 s idle-autosave timer → M5** (no command mutates the timeline outside tests in M1, so it
  can't be exercised end-to-end; recorded under M5 in [phase1.md](phase1.md)).
- **Tauri handlers + TS contract → [Step 12](phase1-m1.md#step-12--tauri-wiring--contract).**
- **G1 fixture round-trip → [Step 13](phase1-m1.md#step-13--g1-round-trip-fixture--final-pass).**
- **Settings/preferences UI (incl. the undo-limit-zero confirmation) → M3** ([frontend.md](../design/frontend.md)).
- **Missing-Files dialog UI + migration-consent/read-only open → M6** (M1 returns the missing-track
  list and runs migrations unconditionally; the dialogs land later — see the deferral note at the
  end of [phase1-m1.md](phase1-m1.md)).
- **Journal compaction** (pruning post-recovery journal-tail garbage) → M5+.
- **`add_track`/`remove_track` commands + load-time tree↔metadata reconciliation guard + their
  round-trip fixtures → M5.** 11d ([§ Step 11d detailed](#step-11d-detailed-apply_batch-metadata-producer))
  adds the *producer* support for combined tree+metadata edits and metadata-only edits; the commands
  themselves and the reconciliation guard ([data-model.md § Load](../design/data-model.md)) remain M5.
