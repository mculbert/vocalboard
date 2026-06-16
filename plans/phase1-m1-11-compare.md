# M1 Step 11 — cross-model implementation comparison

A cross-model comparison of independent implementations of **Step 11** — the `ProjectState`
engine. It is split into two committed sub-steps (see
[phase1-m1-11.md](phase1-m1-11.md)):

- **Step 11a** — engine skeleton, lifecycle, recovery, and the Step 8/9/10 dead-code sweep
  (low risk; pure assembly of already-tested functions).
- **Step 11b** — the `apply_batch` **producer** (high risk; the one piece of novel logic in
  the step — descending-order application, forward/inverse delta capture).

Four comparison models are measured against the **Sonnet 4.6 reference**: Gemma 4 31B,
DeepSeek V4 Flash, Qwen3 Coder Next, and (11a only, from the first round) the same Gemma and
DeepSeek branches.

## Subjects

| Model | 11a branch / commit | 11b branch / commit |
|---|---|---|
| **Sonnet 4.6** (reference) | `claude/1M1` — `a5ec6fe` + Opus remediation `f86f8e8` | `claude/1M1` — `f7e969a` + mutation pass `e97560d` |
| **Gemma 4 31B** | `claude/1M1-11a-gemma` — `32f0828` | `claude/1M1-11b-gemma` — `0100943` + `a341631` |
| **DeepSeek V4 Flash** | `claude/1M1-11a-deepseek` — `7307e1a` | `claude/1M1-11b-deepseek` — `a77ea0e` + `862c0d6` |
| **Qwen3 Coder Next** | `claude/1M1-11a-qwen` — `dfd3830` | `claude/1M1-11b-qwen` — `743c63b` + `48b4902` |

A **second pass (v2)** re-ran Gemma and Qwen with the toolchain supports this report recommended
(see [Part C](#part-c--second-pass-with-toolchain-supports-v2)):

| Model | 11a-v2 branch / commit | 11b-v2 branch / commit |
|---|---|---|
| **Gemma 4 31B (v2)** | `claude/1M1-11a-gemma-v2` — `74de1d4` | `claude/1M1-11b-gemma-v2` — `056904b` → `ba84d9f` |
| **Qwen3 Coder Next (v2)** | `claude/1M1-11a-qwen-v2` — `2291a02` | `claude/1M1-11b-qwen-v2` — `46728e0` → `4c137c9` |

Both v2 passes fork from the supports commit `46f3ba9` (a root `Makefile` task-runner + the expanded
CLAUDE.md build/test section — Supports 1 & 2 below) on the plan base `babc212`, plus a
clippy-enforcing `pre-commit` hook applied in the run environment (Support 3). As in the first round,
the **11a-v2** branches are independent engines, while the **11b-v2** branches fork from Sonnet's
reference 11a (`e5fdace`, via merge `68ef336`) plus a shared test-helper clippy fix (`dbce7f7`), so
the 11b-v2 comparison again isolates the `apply_batch` producer on the same clean reference 11a.

**Base relationships (important for reading the 11b results).** The three 11b comparison
branches all fork from `e5fdace`, which is Sonnet's **final reference 11a** (`f86f8e8`) plus
three doc-only commits. So every 11b submission builds on the *same* clean reference 11a — the
11b comparison therefore isolates the `apply_batch` producer and its tests, with none of the
11a-level differences bleeding in. The reference 11b (`f7e969a` + `e97560d`) forks from the same
`e5fdace`. Qwen's 11a, by contrast, is an independent fork from the plan base `babc212`, so the
11a comparison is genuinely three independent engines.

## Method

Each branch was built and exercised in an isolated worktree:

- `cargo test -p core` (full unit + integration suite)
- `cargo clippy -p core -- -D warnings` (the CI gate, including `missing_docs`,
  `unwrap_used`, `dead_code`)
- a manual diff read of `engine.rs` (and every swept module, for 11a) against the reference and
  the plan
- for 11b, two **purpose-built probe tests** (append-to-a-non-empty-track, with and without a
  trailing snapshot) compiled against each branch to confirm the append-edge behaviour
  empirically rather than by inspection.

---

# Part A — Step 11a (engine skeleton, lifecycle, recovery, dead-code sweep)

## 11a bottom line

| | Reference (Sonnet+Opus) | Gemma `32f0828` | DeepSeek `7307e1a` | Qwen `dfd3830` |
|---|---|---|---|---|
| `cargo test -p core` | green | **1 integration test fails** | green | green (272 pass) |
| `cargo clippy -D warnings` | green | **20 errors** | green | green |
| `new_project` works | ✓ | ✗ (NOT NULL violation) | ✓ | ✓ |
| Corrupt-journal recovery | ✓ typed, row-id tracked | partial / loose | ✓ both branches tested | ✓ tested, but **narrowed** to two error kinds |
| Source resolution (`missing_tracks`) | ✓ wired + persists rewrites | **not wired** (`vec![]`) | ✓ wired + persists rewrites | **partial** — computes list, **omits `FoundViaAbsolute` rewrite-persist** |
| Dead-code sweep | ✓ | ✗ (over- & under-swept) | ✓ (but engine self-allowed) | ✓ green, but **redundant allow on a live `History::new`**; no design-doc note |
| Public API surface | `pub` (Step-12-ready) | `pub` **fields** (leaks internals) | `pub(crate)` + blanket allow | `pub` methods, **private fields** (closest to reference) |
| Tests | strong, non-empty content; `core/tests/` | present but **fail to run** | broad inline; empty trees | recovery + fatal covered, but **inline only** (no `core/tests/engine_*`) |
| Design-doc gap recording | ✓ (Opus) | none | brief note (one inaccuracy) | none |
| Stray/unintended files | none | **`AGENTS.md`/`GEMINI.md`/`.agents/…`** | none | none |

**Verdict (11a):** Both **DeepSeek** and **Qwen** produced green, shippable 11a engines;
**Gemma** did not (it neither compiles under the gate nor runs). DeepSeek and Qwen each land
close to the reference's first pass with different rough edges — DeepSeek is broader on
recovery/source-resolution depth; Qwen has the cleaner public surface (private fields) but a
narrower recovery branch, an unfinished source-resolution step, and a stray `eprintln!`.

## Gemma 4 31B — `32f0828` (11a)

Unchanged from the first-round review. Summary: covers the lifecycle surface but **stubs source
resolution** (`missing_tracks: vec![]`), ships **no design-doc update**, and commits **stray
agent-harness files** (`AGENTS.md`, `GEMINI.md`, `.agents/…`). It **does not work**: `new_project`
omits the `NOT NULL created_at`/`updated_at` columns and errors at runtime
(`lifecycle_round_trip` panics on the first `new_project`), and `cargo clippy -D warnings` fails
with **20 errors** (an over- and under-done dead-code sweep, `missing_docs` on public fields, a
private-type leak via `pub writer`, and assorted lint hits). Public `ProjectState` fields break
encapsulation. Below the bar of a committable first pass. (Full detail retained in git history of
this doc.)

## DeepSeek V4 Flash — `7307e1a` (11a)

Unchanged from the first-round review. Summary: **green and the most complete on substance** —
implements source-file resolution end to end (persists `FoundViaAbsolute` rewrites as a
`type = -1` row), anticipates a slice of 11c (`Db::path()`), and ships a broad inline suite
(15 tests) with the **strongest recovery coverage** (failed-row id, snapshot-only fallback,
fatal second-failure path). Deviations: makes the engine `pub(crate)` + a **blanket
`#[allow(dead_code)]`** rather than the plan's `pub` surface (Step 12 must re-widen); two
never-constructed `EngineError` variants hidden by that allow; an **untyped
`Unrecoverable(String)`** instead of a typed recovery-failed variant; ~35 lines of **reinvented
date math** (`days_to_date`) where the reference uses `datetime('now')`; lifecycle round-trip
uses **empty trees**; and a design-doc note that contradicts its own diff. (Full detail retained
in git history of this doc.)

## Qwen3 Coder Next — `dfd3830` (11a)

### Completeness
Qwen covers the full lifecycle surface (`new_project`, `open_project`, `save_snapshot_now`,
`undo`, `redo`, plus `can_undo`/`can_redo` accessors), the `SnapshotWriter`, and the
`EngineError` / `OpenOutcome` / `RecoveryInfo` types. The engine is green under the full CI gate
(`cargo test -p core`: 272 pass; `clippy -D warnings`: clean). Gaps against the plan:

- **Source resolution is only half-wired.** `open_project` computes the `missing_tracks` list
  (better than Gemma, which stubbed it) but **never calls `resolve_track_source` to persist a
  `FoundViaAbsolute` relative-path rewrite** — the plan's `open_project` step 3 requires storing
  a `type = -1` metadata row when a track's on-disk relative path changed. The reference and
  DeepSeek both do this; Qwen drops it silently.
- **Tests are inline only.** The plan names `core/tests/engine_lifecycle.rs` and
  `core/tests/engine_recovery.rs`; Qwen places all five tests inline in `engine.rs`
  (`new_project_round_trip`, `save_snapshot_now_survives_reopen`, `undo_redo_work`,
  `journal_corruption_triggers_recovery`, `corrupt_snapshot_is_unrecoverable`). Functionally
  fine, and it *does* cover both the recovery and the fatal-snapshot path — a location deviation,
  not a coverage gap.
- **No design-doc update.** Like Gemma, Qwen records none of the kept `#[allow(dead_code)]`
  gaps in `phase1-m1-11.md` / `phase1.md`, which the plan + CLAUDE.md require.

### Correctness — green, but the recovery branch is narrowed
- The dead-code sweep is largely correct (allows removed from `db::store`, `db::journal`,
  `db::mod`, etc.; clippy green) and engine fields are **private** — no blanket allow. But the
  sweep is imperfect: Qwen **re-adds `#[allow(dead_code)]` on `History::new`**, which the engine
  actually calls in `new_project`/`open_project` — a redundant allow on a now-live item (clippy
  stays green because such an allow is silently redundant, so the lint doesn't catch the
  sloppiness). `History::record` keeping its allow is correct (its first caller is 11b).
- **Recovery is narrower than the spec.** The plan says any `load_and_replay` failure — bad
  payload, missing blob, **or hash mismatch** — should fall back to the snapshot. Qwen only
  routes `ReplayError::DeltaDecode` and `ReplayError::Store` into recovery; **every other replay
  failure (e.g. a `DeltaApply` hash-mismatch mid-replay) is treated as fatal** and returned as
  `Err`. The recovery test corrupts the payload (→ `DeltaDecode`), so it passes, but a
  hash-mismatch corruption — explicitly in scope — would fail to recover. The `Store` branch also
  hardcodes `failed_row = -1` (loses the diagnostic) and **re-opens a second `Db` connection**
  unnecessarily.
- **No typed recovery-failed variant.** A fatal recovery (snapshot itself unreadable) returns the
  same `EngineError::Replay` as an ordinary replay error, so the two are indistinguishable to the
  caller — the reference has a dedicated `RecoveryFailed(ReplayError)`.

### Style
- **Couples metadata-load failure into `ReplayError`.** To thread metadata through the happy
  path, Qwen adds a new `ReplayError::Metadata(MetadataLoadError)` variant to `snapshot.rs` and
  does `load_and_replay(..).and_then(|trees| { let meta = load_current_metadata(..)?; .. })`.
  This widens a Step-9 module's error taxonomy so a *metadata* failure now looks like a *replay*
  failure — the same over-coupling the reference avoids by loading metadata separately with its
  own error mapping.
- **Stray `eprintln!` debug line** left in `open_project` (`eprintln!("load_and_replay failed:
  {:?}", e)`) — unstructured stderr output that violates the local-first/clean-logging norm and
  is plainly leftover debugging.
- **`apply_batch` is a panicking `unimplemented!()` stub** in 11a (reasonable as a forward
  placeholder for 11b, and documented with a `# Panics` note, but it is a panic path in non-test
  code).
- Doc-comments are present on `pub` items; the `EngineError` `Display`/`source()` are hand-rolled
  per the plan (no `thiserror`). The private-fields choice is the **closest of the three
  comparison models to the reference's encapsulation intent**.

---

# Part B — Step 11b (`apply_batch` producer)

All three comparison 11b branches build on Sonnet's reference 11a, so the differences below are
purely in the producer logic and its tests.

## 11b bottom line

| | Reference (`e97560d`) | Gemma `a341631` | DeepSeek `862c0d6` | Qwen `48b4902` |
|---|---|---|---|---|
| `cargo test -p core` | green | green | green | green |
| `cargo clippy -D warnings` | green | **4 errors — gate fails** | green | green |
| `apply_batch` visibility | `pub(crate)` (per plan) | **`pub`** | `pub(crate)` (per plan) | **`pub`** |
| Coordinate resolution | frozen `original` clone | frozen `original` ref | frozen `original` clone | **live working tree** |
| Inverse batch reversed | ✓ `.reverse()` | **✗ never reversed** | ✓ `.reverse()` | ✓ `.reverse()` |
| Append to non-empty track | ✓ correct | **✗ hard-errors** (probe) | ✓ correct (probe) | **✗ wrong location → reorders on replay** (probe) |
| Typed errors (tree / type mismatch) | `Tree` / `TrackTypeMismatch` | reuses `ReplayError::NoSnapshot` | `Tree` | reuses `ReplayError::NoSnapshot` |
| `apply_batch` tests | 5 inline + lifecycle | 1 inline + 1 integration | **8 inline apply_batch tests** | 5 integration (L6–L10) |
| Same-track order-dependent undo tested | ✓ (dedicated test) | ✗ (batch uses two tracks → masks the bug) | ✓ | partial |

**Verdict (11b):** **DeepSeek** is the standout — `pub(crate)` per plan, frozen-original
resolution, inverse reversed, correct append handling, typed errors, and by far the deepest
`apply_batch` test suite; it is the only comparison model that is both green **and** free of a
demonstrated correctness bug. **Qwen** is green and structurally close (it reverses the inverse
and tests it), but ships a **silent append-reordering bug** and resolves coordinates against the
mutated working tree. **Gemma** is not shippable: it **fails the clippy gate** and carries **two
independent latent bugs** (a never-reversed inverse and a hard error on append).

## Reference (Sonnet `f7e969a` → `e97560d`) — the target

`apply_batch(&mut self, ops: &[BatchOp], category)` (`pub(crate)`): sorts an index vector
descending by sample (tie-break on `track_id`), keeps a **frozen `original_trees` clone** for all
coordinate resolution and a separate **`working_trees`** it mutates per op, captures forward and
per-op inverse deltas, persists new blobs + the forward batch in one transaction, then — only
after commit — **reverses** the per-op inverse vector, swaps `current`, and records the
`UndoEntry`. Append is handled by a dedicated `insert_location` that walks to the last element
when `sample >= total_duration()`. Errors are typed (`EngineError::Tree`,
`TrackTypeMismatch`). Tests are **inline** (a necessary consequence of `pub(crate)` — integration
tests in a separate crate cannot call it): a same-track descending-order test, two-track
lifecycle, zero-undo-limit, label-tree insert/update, and undo-limit eviction, plus the
content-bearing lifecycle round-trip. The second commit (`e97560d`) is a focused mutation pass.

## Gemma 4 31B — `a341631` (11b)

### Correctness — not green, plus two latent bugs
- **Fails `clippy -D warnings` (4 errors)** — the commit could not have passed the CI gate:
  - `missing_docs` ×2 on `undo::TimelineState::{trees, metadata}`, which Gemma made **`pub`** (an
    encapsulation regression — the same public-field instinct it showed in 11a — that also
    happens to break the build);
  - `empty line after doc comment` from a **duplicated `now_posix` doc-comment** block;
  - `this function has too many lines (153/100)` on `apply_batch` with no `#[allow]`;
  - `TrackTree has a public len method, but no is_empty` — Gemma added a bare `pub fn len` to
    `snapshot.rs` for its tests.
- **The inverse batch is never reversed.** Gemma pushes per-op inverses in application
  (descending) order and stores them as-is. The reference does `inverse_fwd.reverse()`. For a
  batch with two order-dependent ops on the **same track** (e.g. *update B* + *delete C*, where
  the delete's inverse `insert_after(After(hB))` must run *after* B is restored), replaying the
  inverse in the wrong order references a hash that is no longer present → a broken or wrong undo.
  Gemma's own `apply_batch_lifecycle_round_trip` test **masks this**: its multi-op batch puts the
  update on track 1 and the delete on track 0, so the two ops are independent and order doesn't
  matter. The reference's dedicated same-track test is exactly what catches it.
- **Append to a non-empty track hard-errors** (empirically confirmed). Gemma resolves an
  insert's location with `element_at_sample(sample)`, which returns `None` at
  `sample == total_duration()`; Gemma maps that to `EngineError::Replay(ReplayError::NoSnapshot)`.
  The append probe returns `Err(timeline replay error: no snapshot row found)` — appending a turn
  to an existing track is impossible, and the error message is actively misleading.

### Style
- **`apply_batch` is `pub`**, not the plan's `pub(crate)` — widened specifically so the
  integration-test file can call it (the reference keeps it `pub(crate)` and tests inline).
- **Misuses `ReplayError::NoSnapshot` as a catch-all** for tree-mutation failures, type
  mismatches, and append-resolution failures — a garbage error type for every non-replay
  condition, where the reference returns typed `Tree` / `TrackTypeMismatch`.
- Thin test coverage of the novel logic: one inline `apply_batch` test plus the masked
  integration round-trip — no same-track ordering test, no inverse-capture test.

## DeepSeek V4 Flash — `862c0d6` (11b)

### Correctness — green and bug-free on the probes
- `cargo test -p core` green; `clippy -D warnings` clean.
- **Frozen-original resolution.** Resolves `(loc, h_old)` via a `resolve_location_and_hash`
  helper against an `Arc::clone`d `original` state, mutating a separate `working_trees` — matching
  the reference and the plan's two-representation guidance exactly.
- **Inverse reversed** (`inverse.reverse()` after the commit) and **append handled correctly**:
  `resolve_location_and_hash` has an explicit branch for `sample == total_duration() &&
  !is_empty()` that returns `After(last)`, using a justified `.expect("non-empty tree has a last
  element")`. The append probe round-trips through both snapshot and pure delta-replay reopen.
- **Typed errors** throughout (`EngineError::Tree(TreeError::…)`).
- **By far the deepest test suite of the four** — 8 dedicated `apply_batch` tests covering
  insert round-trip, insert/update/delete, descending-order inserts, inverse capture, zero-undo
  limit, snapshot round-trip, and four append/update/delete location-resolution tests — kept
  **inline** (the correct consequence of `pub(crate)`). The second commit is a mutation pass.

### Style
- Minor: `apply_batch` takes `ops: Vec<EditOp>` **by value** where the reference takes
  `&[BatchOp]` (an unnecessary move). Otherwise the closest match to the reference's structure and
  the strongest comparison submission for 11b.

## Qwen3 Coder Next — `48b4902` (11b)

### Correctness — green, reverses the inverse, but a silent append bug
- `cargo test -p core` green; `clippy -D warnings` clean.
- **Inverse is correctly reversed** (`inverse_deltas.reverse()`), and Qwen even has a dedicated
  integration test (`apply_batch_inverse_is_reversed_forward`, L10) that asserts it — a genuine
  strength over Gemma.
- **Coordinate resolution uses the live working tree, not a frozen original.** The variable is
  named `original_tree` but is sourced from `working_trees` (which is mutated as ops are applied).
  This is the exact footgun the plan warns against ("Do not read the `Location`/predecessor off
  the working (already-mutated) tree"). It happens to stay *correct* for strictly-descending,
  distinct-sample batches — every already-applied op lies to the right, so left-side predecessors
  are unchanged — so Qwen's tests pass. It is nonetheless fragile-by-construction and against the
  plan's explicit instruction.
- **Append to a non-empty track silently records the wrong location** (empirically confirmed).
  `resolve_location` returns `(None, dummy_hash)` when `element_at_sample(sample)` is `None`
  (i.e. at `sample == total_duration()`), which becomes `Location::Start`. The in-memory working
  tree appends correctly, but the **forward delta says `insert_after(Start)`**. The probe shows
  the divergence concretely: after appending B to a track holding A and reopening **via delta
  replay**, the track comes back as `[B, A]` instead of `[A, B]` — silent element reordering /
  data corruption. (A snapshot taken *after* the append masks it, since the snapshot stores the
  correct in-memory order — which is why Qwen's snapshot-based tests don't catch it.) The
  reference and DeepSeek both special-case the append to `After(last)`.

### Style
- **`apply_batch` is `pub`** (renamed types `EditOp` / `EditKind` / `ElementType`), again widened
  so the integration-test file can drive it — same deviation as Gemma from the plan's
  `pub(crate)`.
- **Misuses `ReplayError::NoSnapshot`** as a catch-all for type mismatches, and threads a **dummy
  `Hash([0u8;16])`** as `h_old` for the empty/append case — both smells the reference avoids with
  typed errors and an `Option<Hash>`.
- Test coverage of the producer is reasonable (L6–L10 in the integration file: two-track
  descending order, descending-preserves-samples, undo/redo, zero-undo limit, inverse-reversed),
  but **all inserts are at sample 0** — so the append edge is never exercised, which is precisely
  why the append bug shipped uncaught.

---

# Part C — Second pass with toolchain supports (v2)

Gemma and Qwen were re-run with the supports recommended at the end of this report: a `Makefile`
task-runner, an expanded CLAUDE.md build/test section, and a clippy-enforcing `pre-commit` hook.
The question this part answers: **did the supports improve the code (completeness, correctness,
style), and did they close the gap with DeepSeek?**

Method is identical to Parts A/B (build + `cargo test -p core` + `cargo clippy -p core --all-targets
-- -D warnings` in isolated worktrees, a diff read against the reference and the v1 branches, and
**purpose-built probe tests** compiled into each branch for the load-bearing correctness claims).
Per the user's session notes, both models still needed mid-run redirection during the mutation pass
(Gemma got stuck in a debugging loop and was steered to fix clippy errors first; Qwen largely ignored
the Makefile and had to be reminded of the `VOCALBOARD_PYTHON` env var) — so the supports reduced but
did not eliminate toolchain friction.

## The headline: the gate gap closed

**All four v2 branches are green** under both `cargo test -p core` and `cargo clippy -p core
--all-targets -- -D warnings`. In the first round, Gemma's 11a (**20 clippy errors**, broken
`new_project`) and 11b (**4 clippy errors**) both failed the CI gate outright. With the supports,
**every v2 submission compiles and passes the gate.** This is the single biggest improvement and it
is entirely attributable to the supports — Gemma in particular went from "does not compile, does not
run" to "green and shippable" on 11a. (Caveat, developed below: a `-D warnings` pre-commit hook can
be satisfied by sprinkling `#[allow(...)]`, which Qwen's 11b did — so "green" no longer implies "no
silenced lints.")

## Part C.1 — Step 11a-v2

| | Reference | Gemma v1 `32f0828` | **Gemma v2 `74de1d4`** | Qwen v1 `dfd3830` | **Qwen v2 `2291a02`** |
|---|---|---|---|---|---|
| `cargo test -p core` | green | **integration test fails** | **green** | green | green |
| `cargo clippy -D warnings` | green | **20 errors** | **green** | green | green |
| `new_project` works | ✓ | ✗ (NOT NULL) | **✓** | ✓ | ✓ |
| Reads stored `sample_rate` on open | ✓ | n/a | ✓ | ✓ (`load_sample_rate`) | **✗ hardcodes `48000`** |
| Source resolution (`FoundViaAbsolute` persist) | ✓ | ✗ (`vec![]`) | **✓ wired + persists** | partial (no persist) | partial (no persist; now inlined) |
| Corrupt-journal recovery | ✓ typed | partial | **✓ broad (all replay errs)** | narrowed (2 kinds) | **narrowed (DeltaDecode only)** |
| Public struct fields | private | **pub (leak)** | **private** | private | private |
| metadata→ReplayError coupling | none | none | none | **present** | **gone** |
| Stray harness files | none | **`AGENTS.md` etc.** | **none** | none | none |
| Tests | `core/tests/` | fail to run | **`core/tests/` (plan-named)** | inline only | inline only |

### Gemma 4 31B — `74de1d4` (11a-v2)

**A dramatic improvement — from "below committable" to green and substantially complete.** Every
first-round blocker is fixed:

- **Compiles and runs.** `new_project` now supplies `created_at`/`updated_at`, so the runtime
  `NOT NULL` failure is gone; `clippy -D warnings` is clean (was 20 errors).
- **Source resolution fully wired.** `open_project` runs `resolve_track_source` over every track and
  **persists a `FoundViaAbsolute` relative-path rewrite as a `type = -1` metadata row** — the exact
  end-to-end behaviour the plan requires and that v1 Gemma stubbed (`missing_tracks: vec![]`). This
  now matches the reference and DeepSeek on substance.
- **Private struct fields** (the v1 `pub`-field encapsulation leak is gone), **no stray harness
  files** (`AGENTS.md`/`GEMINI.md`/`.agents/…` are absent), and the tests live in the plan-named
  `core/tests/engine_lifecycle.rs` + `engine_recovery.rs`.
- **Recovery is broad and correct:** any `load_and_replay` error (including a `DeltaApply`
  hash-mismatch) falls back to `load_latest_snapshot`, and a failing snapshot is fatal — wider than
  Qwen's narrowed branch.

Remaining style dings (none gate-breaking):

- **Error-type laundering.** `with_transaction`'s closure returns `anyhow::Result`, so `?` would
  preserve every error — but Gemma instead maps `store`/`postcard`/`journal` failures to
  `rusqlite::Error::InvalidQuery`, and maps a metadata-load failure to a fabricated
  `StoreError::Sqlite(QueryReturnedNoRows)`. This is an **unforced** loss of diagnostic information.
- **A new `chrono` dependency** added solely to format an RFC-3339 timestamp where the reference uses
  SQL `datetime('now')` — a supply-chain cost for nothing.
- **Encapsulation widened in `undo.rs`:** to drive its `core/tests/` integration tests, Gemma made
  `undo::TimelineState` and `UndoEntry` **fields `pub`** (documented this time, so clippy passes) and
  added `pub` `record_synthetic_edit` / `current_state` helpers on `ProjectState`. It compiles, but it
  leaks Step-10 internals the reference keeps `pub(crate)` — the same public-surface instinct from v1,
  now merely legal. (Qwen avoids this by keeping its tests inline.)
- A redundant `#[allow(dead_code)]` on the `sample_rate` field.

### Qwen3 Coder Next — `2291a02` (11a-v2)

Still green with the cleanest encapsulation surface (private fields, inline tests), and it **fixed two
v1 style issues**: the metadata→`ReplayError` coupling is gone (`snapshot.rs` is untouched; there is a
dedicated `EngineError::MetadataLoad` variant), and the stray `eprintln!` is gone. But the supports did
not move its substance, and it **regressed in two places**:

- **`sample_rate` is hardcoded to `48000` on open.** v1 Qwen correctly read the stored rate
  (`load_sample_rate(&db)?`); v2 constructs `ProjectState { …, sample_rate: 48000, … }` in
  `open_project` and never queries the `project` row. Reopening a non-48 kHz project now reports the
  wrong rate — a genuine correctness regression, and untested (every test uses 48000).
- **Recovery is still narrowed** to `ReplayError::DeltaDecode`; a hash-mismatch (`DeltaApply`) or
  missing-blob (`Store`) corruption is still returned as a fatal `Err` instead of recovering to the
  snapshot — unchanged from v1, despite being explicitly in the plan's recovery scope.
- **Source resolution still omits the `FoundViaAbsolute` persist**, and v2 actually *reimplements* the
  missing-track check inline (`missing_tracks_from_metadata`) instead of calling the metadata module's
  `resolve_track_source`/`missing_tracks` — a step away from the designed API.
- Minor: `created_at`/`updated_at` are written as a hardcoded literal `"2024-01-01T00:00:00Z"`; an
  `#[allow(private_interfaces)]` sits on the `pub` `EngineError`.

**Net for 11a:** the supports inverted the v1 ordering. In round one, Qwen 11a > Gemma 11a (Gemma was
broken). In round two, **Gemma 11a-v2 ≳ Qwen 11a-v2 on completeness and correctness** (Gemma persists
source rewrites, reads the real sample rate, and recovers broadly; Qwen does none of those), while Qwen
retains the cleaner encapsulation surface.

## Part C.2 — Step 11b-v2 (`apply_batch` producer)

| | Reference (`e97560d`) | Gemma v1 `a341631` | **Gemma v2 `ba84d9f`** | Qwen v1 `48b4902` | **Qwen v2 `4c137c9`** |
|---|---|---|---|---|---|
| `cargo clippy -D warnings` | green | **4 errors** | **green** | green | green (via blanket `#[allow]`) |
| `apply_batch` visibility | `pub(crate)` | `pub` | `pub` | `pub` | **`pub(crate)`** |
| Coordinate resolution | frozen original | frozen | **frozen** | live working tree | frozen |
| Inverse batch reversed | ✓ | **✗** | **✗ (still)** | ✓ (+test) | **✗ (regression)** |
| Insert first element into new track | ✓ | ✓ | **✓** | ✓ | **✗ silently skipped (probe)** |
| Append to non-empty track | ✓ | ✗ hard-error | **✓ correct, round-trips (probe)** | ✗ silent reorder | **✗ panics (probe)** |
| Producer exercised by a test | ✓ | thin | **✓ inline + integration** | ✓ (L6–L10) | **✗ only empty-ops no-op** |
| Typed tree errors | ✓ | `NoSnapshot` catch-all | partial (`Tree` + `NoSnapshot`) | `NoSnapshot` catch-all | `panic!` + `Tree` |

### Gemma 4 31B — `ba84d9f` (11b-v2)

**A large step up, but one v1 bug survives and a new sloppiness appears.** What improved:

- **Green** (the v1 clippy gate failure is gone), **frozen-original resolution** (resolves `Location`
  + `h_old` against `self.current.trees` while mutating a separate `working_trees`), and — the
  v1-decisive edge — **append is handled correctly**: `sample == total_duration` resolves to
  `After(rightmost_hash())`. A probe confirms an append to a non-empty track **round-trips through pure
  delta replay** (`load_and_replay(&db, None) == current.trees`), the exact assertion v1 Gemma failed
  by hard-erroring. Gemma also **drives `apply_batch` with real ops** (inline `test_apply_batch_boundaries`
  exercising sample 0, `total_duration`, and gaps, plus +243 lines in `engine_lifecycle.rs`).

What did not improve / regressed:

- **The inverse batch is still never reversed.** `apply_batch` pushes per-op inverses in application
  (descending) order and stores them as-is — no `.reverse()` anywhere. A probe builds `[A,B,C]` on one
  track, applies a same-track `{update B, delete C}` batch, undoes it, and replays: it **fails with
  `delta apply failed at journal row 7: LocationNotFound`** — the delete's inverse `insert_after(After(hB))`
  is replayed before B is restored. This is the identical v1 latent bug, and it is again **masked** by
  Gemma's own tests (the lifecycle batch spreads ops across two tracks; the boundary test never undoes).
- **Two leftover `println!("DEBUG: …")` statements ship in the production `apply_batch`** — clippy does
  not lint `println!`, so the gate let them through. Plain debugging sludge in a persistence-critical path.
- **It modified the locked Step 6 tree primitive** (`element_at_sample` gained a 0-duration-element
  branch) and exposed `rightmost_hash`/`total_duration` as `pub` on `TrackTree` — touching
  already-tested foundational code rather than confining the change to the engine.
- `apply_batch` is `pub` (plan says `pub(crate)`), it still misuses `ReplayError::NoSnapshot` as the
  catch-all for out-of-range inserts, and a type-mismatched update/delete silently no-ops.

### Qwen3 Coder Next — `4c137c9` (11b-v2)

**A clear regression from v1 — the producer is essentially unverified, non-functional code.** The one
real improvement is visibility: `apply_batch` and `BatchOp` are now `pub(crate)` (matching the plan,
where v1 had widened them to `pub`). But that change, made without porting the tests, is exactly what
sank it:

- **`apply_batch` is never exercised with real ops.** The *only* call to it anywhere in the crate is
  the inline `apply_batch_empty_ops` test, which passes `&[]` and hits the `if ops.is_empty()` early
  return. The producer body is dead code — it carries `#[allow(dead_code)]`, and its comment "Used by
  tests in `engine_lifecycle.rs`" is false (the 11b-v2 diff never touches that file). v1 Qwen at least
  had integration tests L6–L10 driving the producer; switching to `pub(crate)` without moving those
  tests inline **deleted all producer coverage.**
- **It cannot create a track's first element.** Ops whose `track_id` is not already in `current.trees`
  are silently skipped (`None => continue`); `new_project` starts empty, so a from-scratch insert is a
  no-op. A probe inserting a turn into a fresh project leaves `current.trees` **empty** (`tracks after
  insert = []`).
- **It panics on append.** For a track that does exist, the insert path is
  `original_tree.element_at_sample(sample).expect("…")`; at `sample == total_duration` that is `None`,
  so it **panics** (probe: "append PANICKED" at the `expect`). v1's silent reorder became a hard panic,
  behind a production `expect`.
- **The inverse is never reversed** — a regression from v1, which reversed it *and* asserted it
  (`apply_batch_inverse_is_reversed_forward`). v2 stores the inverse in descending application order;
  the `track_ops.reverse()` call only sets the descending *processing* order.
- **Style:** payloads are carried as `Arc<dyn Any + Send + Sync>` and recovered with
  `downcast_ref::<T>().expect(...)` instead of a typed enum; a track-type mismatch is a `panic!`; and
  the gate is satisfied by a stack of `#[allow(clippy::{expect_used, panic, unwrap_used,
  too_many_lines, dead_code, type_complexity})]` — i.e. the producer is "green" precisely because its
  lints are silenced. Near-duplicate `process_track_ops_labels`/`_speech` helpers round it out.

The supports' clippy hook is what makes this case instructive: it forced *compileability* without
forcing *correctness or coverage*, and a model can satisfy `-D warnings` by annotating the warnings
away. Qwen shipped a green commit whose central deliverable does not run.

## Probes (empirical confirmations, this pass)

| Probe | Result |
|---|---|
| Gemma 11b-v2 — append to non-empty track, replay with no intervening snapshot | **round-trips (correct)** |
| Gemma 11b-v2 — same-track `{update,delete}` batch → undo → replay | **`LocationNotFound` (inverse-order bug)** |
| Qwen 11b-v2 — insert into fresh empty project | **op skipped, track never created** |
| Qwen 11b-v2 — append to existing non-empty track | **panic at `element_at_sample(...).expect(...)`** |

---

# Conclusion

**Step 11a.** On **completeness**, reference ≈ DeepSeek > Qwen > Gemma (DeepSeek and the
reference fully wire + persist source resolution; Qwen computes the missing list but skips the
`FoundViaAbsolute` rewrite; Gemma stubs it). On **correctness**, reference ≈ DeepSeek ≈ Qwen
(all green) ≫ Gemma (broken `new_project`, 20 clippy errors) — with the caveat that Qwen's
recovery branch is narrowed to two error kinds and would not recover a hash-mismatch corruption.
On **style**, the reference leads; **Qwen has the cleanest surface of the comparison models**
(private fields, closest to the reference) but is undercut by a stray `eprintln!`, a metadata→
replay error coupling, and a panicking stub; DeepSeek under-exposes the API and reinvents date
math; Gemma leaks internals and commits stray harness files.

**Step 11b.** Because all three comparison branches build on Sonnet's reference 11a, the producer
is judged in isolation, and the spread is sharp:

- **DeepSeek** is the clear best comparison result — green, plan-faithful (`pub(crate)`,
  frozen-original resolution, inverse reversed, typed errors), correct on the append edge, and
  the most thoroughly tested. It is the only comparison model with **no demonstrated bug**.
- **Qwen** is green and gets the inverse-reversal right (and tests it), but ships a **silent
  append-reordering corruption** and resolves against the mutated tree — both flowing from a test
  suite that never appends to a non-empty track.
- **Gemma** is not a viable 11b: it **fails the clippy gate (4 errors)** and carries **two latent
  bugs at once** — a never-reversed inverse (masked by a two-track test) and a hard error on
  append. The 11b verdict mirrors its 11a verdict.

The single most telling 11b datapoint is the append edge: a one-branch detail
(`sample == total_duration()` → `After(last)`) that the reference and DeepSeek handle, that Gemma
turns into a misleading hard error, and that Qwen turns into silent on-disk reordering — caught
here only by a probe test neither model wrote. As in 11a, the gap between the models is less about
the happy path (all four produce a working descending-order applier) than about the edge cases and
the discipline of the surrounding gate: the dead-code sweep, the API-visibility contract, typed
errors, and a test suite that actually exercises the boundary the bug lives on.

## Second-pass (v2) verdict — did the supports close the gap?

**On the gate: yes, decisively.** All four v2 branches compile and pass `clippy -D warnings` + tests;
the first round's two red Gemma commits (20 and 4 clippy errors) are gone. The `Makefile` + CLAUDE.md
working-directory documentation removed most of the toolchain archaeology, and Gemma in particular went
from non-compiling to green-and-shippable on 11a. That is exactly the "redirect effort from toolchain
to diff" outcome the supports were meant to produce.

**On completeness and correctness: mixed, and not closing the DeepSeek gap on 11b.**

- **Gemma 11a-v2 is the big winner** — green, private fields, no stray files, and source resolution
  fully wired + persisted, putting it on par with DeepSeek on 11a substance and *ahead* of Qwen.
- **Qwen 11a-v2 is roughly static**: it cleaned up two style issues (no more metadata→`ReplayError`
  coupling, no stray `eprintln!`) but **regressed** (hardcoded `sample_rate` on open) and left the two
  v1 substance gaps (narrowed recovery, no `FoundViaAbsolute` persist) untouched.
- **11b is where the supports did *not* help.** Gemma 11b-v2 fixed the append edge (its v1-decisive
  miss) and now tests the producer, but **still ships the never-reversed-inverse bug** (proven again by
  probe) plus two `DEBUG` `println!`s. Qwen 11b-v2 **regressed badly**: switching `apply_batch` to
  `pub(crate)` without porting its tests left the producer as unverified dead code that cannot create a
  track's first element and panics on append — all green only because the lints are `#[allow]`-ed away.
  **DeepSeek 11b remains the sole comparison submission that is green, plan-faithful, correct on the
  append edge, inverse-reversed, and deeply tested.** The gap is not closed.

**The cross-cutting lesson.** The supports fixed what they targeted — the *gate* and *toolchain
friction* — but those were never where the quality gap lived. The 11b gap is about **spec
comprehension** (Qwen's narrowed recovery; both models' inverse handling) and **test discipline**
(exercising the novel logic on its boundaries), and the supports moved neither. Worse, a `-D warnings`
pre-commit hook introduced a new failure mode: it can be satisfied by sprinkling `#[allow(...)]`,
converting a hard signal into a silenced one — which is precisely how Qwen's non-functional producer
committed green. This *reinforces* the first round's core recommendation: the lever that would have
caught Gemma's inverse bug and Qwen's entire dead producer is not a better gate but the **named,
mandatory edge-case tests** below (append-via-replay + same-track order-dependent undo), which no amount
of clippy tuning substitutes for.

## Recommendation — make the append-via-replay edge a named, mandatory test

The single probe that separated correct from buggy 11b producers is worth pinning as an explicit
contract test rather than leaving to each implementer's judgement.

**The test case (append to a non-empty track, verified through delta replay only):**

1. `new_project`; `apply_batch` an insert of element A at `sample = 0` (creates a non-empty
   track — A spans `0..len_A`).
2. `apply_batch` a second insert of element B at `sample == len_A` (i.e. exactly
   `total_duration()` of the now-non-empty track — the **append** boundary). Assert the in-memory
   order is `[A, B]`.
3. **Without any intervening `save_snapshot_now`**, assert `load_and_replay(&db, None)` equals the
   live `current.trees`. This is the load-bearing assertion: it forces the forward delta to be
   replayed from the initial empty snapshot, so a wrong `Location::Start` (instead of
   `After(hash_A)`) reorders the track to `[B, A]` and the equality fails. A correct
   `After(predecessor)` round-trips.

The "no intervening snapshot" condition is what makes the test sharp — a snapshot taken after the
append stores the correct in-memory order and **masks** a wrong delta (exactly why Qwen's
snapshot-based tests passed while the bug shipped). A second variant that *does* snapshot after the
append is a useful contrast but does not, on its own, catch the bug.

**Status / recommendation.** The reference **already encodes this** as
`apply_batch_label_existing_tree_insert_and_update` (it appends `L2` at `sample == total_duration`
and asserts `load_and_replay(&db, None) == current.trees`, with an inline comment that it "catches
`>=` vs `<` in `insert_location_in_tree`"). So the reference does not need the test added — it has
it. The gap is that the **plan** ([phase1-m1-11.md § Test plan](phase1-m1-11.md#test-plan)) lists
the 11b lifecycle/ordering tests but does not call out this append-via-replay case by name, so an
implementer can satisfy the listed tests (two-track lifecycle, descending order, undo/redo) and
still omit the one boundary where the producer's `Location` derivation is most fragile.
**Recommended:** promote this case to an explicitly named, required item in the Step 11b test list
(e.g. "*append at `total_duration` to a non-empty track, asserted through `load_and_replay` with no
intervening snapshot*"), so any implementation — local models included — is forced to write it.
Pair it with the existing same-track order-dependent undo test (which Gemma's two-track batch
masked) as the two named edge-case guards for the producer.

## Recommendation — toolchain supports for a local-model re-run

A second observation from watching the Gemma and Qwen transcripts: both spent significant effort
just getting the build/test tools to run (the `cd src-tauri` working-directory requirement, the
exact `clippy`/`test` flags, and especially the `cargo-mutants` invocation), and both appear to
have **abandoned the mutation pass** rather than complete it. That matters because the two latent
11b bugs found above — a never-reversed inverse and a `Location::Start`-vs-`After` swap — are
exactly the kind a working mutation run over `apply_batch` surfaces as live mutants. The models did
not lack the ability to fix them; they never got the signal. Two cheap, model-agnostic supports
would address this for the re-run.

### Why the current gate doesn't catch a red commit

The repo's `pre-commit` hook is **policy-only** — it enforces the branch rules and GPG-signing
policy and runs **no `fmt`/`clippy`/`test`**. The actual quality gate lives in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml), which only runs *after* a PR is opened.
And the hooks live in `.git/hooks/` (untracked), so a fresh clone or a sandboxed model environment
has no hook at all. Net effect: nothing local stops a model from committing red — which is exactly
what Gemma did (its 11a and 11b both fail `clippy -D warnings`).

### Support 1 — a `Makefile` task runner (stable commands; hides the `cd` and the flags)

A root `Makefile` so every command is runnable from the repo root and the working-directory gotcha
disappears. Mirrors the CI Rust job exactly, so "green locally" means "green in CI." **Draft:**

```makefile
# Vocalboard task runner. All targets run from the repo root; each cd's into the
# correct workspace so the tool's working directory is never something to remember.
.DEFAULT_GOAL := help
RUST_DIR := src-tauri

# ── Rust (workspace lives in src-tauri/) ──────────────────────────────────────
.PHONY: fmt
fmt:        ## Rust: check formatting (CI gate)
	cd $(RUST_DIR) && cargo fmt --all -- --check
.PHONY: fmt-fix
fmt-fix:    ## Rust: apply formatting
	cd $(RUST_DIR) && cargo fmt --all
.PHONY: clippy
clippy:     ## Rust: lints as errors (incl. missing_docs, unwrap_used, dead_code)
	cd $(RUST_DIR) && cargo clippy --workspace --all-targets -- -D warnings
.PHONY: test
test:       ## Rust: full workspace test suite
	cd $(RUST_DIR) && cargo test --workspace
.PHONY: test-core
test-core:  ## Rust: core crate only (fast inner loop)
	cd $(RUST_DIR) && cargo test -p core
.PHONY: doc
doc:        ## Rust: broken intra-doc-link lint (CI gate)
	cd $(RUST_DIR) && RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace
.PHONY: verify
verify: fmt clippy test doc  ## Rust: everything the CI Rust job enforces — run before EVERY commit

# ── Mutation testing (milestone gate, slow — not per-commit) ──────────────────
# Defaults to the conventions.md file-focused form (known-good). Override FILE= to scope.
FILE ?= core/src/project/engine.rs
.PHONY: mutants
mutants:    ## Rust: focused mutation pass over FILE (default: engine.rs)
	cd $(RUST_DIR) && cargo mutants -f $(FILE)
.PHONY: mutants-all
mutants-all: ## Rust: full workspace mutation sweep (very slow)
	cd $(RUST_DIR) && cargo mutants --workspace

# ── Python / Frontend ─────────────────────────────────────────────────────────
.PHONY: py-test
py-test:    ## Python: sidecar test suite
	cd python && uv run pytest
.PHONY: fe-check
fe-check:   ## Frontend: svelte-check + lint + vitest + build
	pnpm check && pnpm lint && pnpm test && pnpm build

.PHONY: help
help:
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
```

> Caveat to validate before the re-run: the mutation target uses the file-focused
> `cargo mutants -f <file>` form documented in [conventions.md](../design/conventions.md) (known-good). To
> narrow to a single function, `cargo-mutants` supports a name regex (`--re apply_batch`), but the
> exact flag spelling should be confirmed against the installed `cargo-mutants` version — it was
> not run while drafting this. The plan's "*`cargo-mutants` scoped to `apply_batch`*"
> ([phase1-m1-11.md](phase1-m1-11.md#step-11b--apply_batch-producer-high-risk--the-only-novel-logic))
> is the intent.

### Support 2 — expanded CLAUDE.md toolchain section (the working-dir rule, stated loudly)

Replace the terse "Build / test / run" block in [CLAUDE.md](../CLAUDE.md) with an explicit section.
**Draft:**

> ## Build / test / run
>
> **All Rust commands run from `src-tauri/`** (the workspace root) — running them from the repo
> root fails to find the workspace. The `Makefile` targets below cd for you; prefer them.
>
> | Task | Make target | Raw command (cwd) |
> |---|---|---|
> | Format check | `make fmt` | `cargo fmt --all -- --check` (`src-tauri/`) |
> | Lints as errors | `make clippy` | `cargo clippy --workspace --all-targets -- -D warnings` (`src-tauri/`) |
> | Tests (all) | `make test` | `cargo test --workspace` (`src-tauri/`) |
> | Tests (core only) | `make test-core` | `cargo test -p core` (`src-tauri/`) |
> | Doc-link lint | `make doc` | `RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace` (`src-tauri/`) |
> | **Full pre-commit gate** | **`make verify`** | the four above, in order |
> | Mutation (milestone) | `make mutants` | `cargo mutants -f core/src/project/engine.rs` (`src-tauri/`) |
> | Python | `make py-test` | `uv run pytest` (`python/`) |
> | Frontend | `make fe-check` | `pnpm check && pnpm lint && pnpm test && pnpm build` (repo root) |
> | App (dev) | — | `pnpm tauri dev` (repo root) |
>
> **Before every commit, run `make verify` and confirm it exits 0.** "Green" = `cargo fmt` reports
> no diff, `clippy` prints `Finished` with zero warnings, and the test summary shows `0 failed`. A
> warning from `clippy` is a hard failure — `-D warnings` makes every lint (including `missing_docs`
> and `dead_code`) an error.
>
> **Mutation testing** (`make mutants`) is a milestone gate, not per-commit (runs are slow); a
> surviving mutant in `apply_batch` means a test is missing — add it, do not annotate it away
> unless the mutant is genuinely equivalent ([conventions.md](../design/conventions.md)).

### Support 3 — the actual gate: ship a quality pre-commit hook in-repo

The piece that makes the above *enforced* rather than advisory. Two parts:

1. **Version-control the hooks** so every clone/sandbox has them: move the hook scripts into a
   tracked directory (e.g. `scripts/hooks/`) and wire them with
   `git config core.hooksPath scripts/hooks` (a one-line setup step, addable to a bootstrap
   target). Today's hooks in `.git/hooks/` exist only on this machine.
2. **Add a quality stage to `pre-commit`** — keep the existing branch/signing policy checks, then
   run the fast gate and refuse a red commit:
   ```bash
   # … existing branch + GPG-signing policy checks stay above …
   # Quality gate: fmt + clippy + core tests must pass before a commit lands.
   if ! make fmt clippy test-core; then
       echo "❌ pre-commit: 'make fmt clippy test-core' failed — fix before committing."
       exit 1
   fi
   ```
   (Use `test-core` for commit-time speed; `make verify` — full workspace + doc lint — belongs in
   `pre-push`, mirroring CI.) With this in place, Gemma's red 11a/11b could not have been committed,
   and the model would have been forced to confront the `clippy` errors and the failing gate while
   it still had context — converting a silent miss into an actionable signal.

**Net for the re-run:** give the local models (a) `make verify` as the one command to remember,
(b) a CLAUDE.md that states the working-directory rule and what green looks like, and (c) an
enforced hook so red never commits. The hypothesis to test is that redirecting the effort they
currently spend on toolchain archaeology toward the diff — plus a mutation pass that actually
runs — closes much of the gap, Qwen especially (its producer already reverses the inverse and
keeps a clean private-field surface; its misses are edge-case tests, which the named-test
recommendation above directly targets).
