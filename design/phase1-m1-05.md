# Phase 1 · M1 · Step 5 — Blob store (`db/store.rs`) (action plan)

Per-step action plan for Step 5 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md § Turn and Label blobs](data-model.md#turn-and-label-blobs)
and [§ Schema DDL](data-model.md#schema-ddl-phase-1-user_version--1). This
step lays down the **content-addressed blob-store plumbing**: a thin pair of
functions over the `store` SQLite table that turn the `(Hash, Vec<u8>)`
already produced by [Step 3](phase1-m1-03.md)'s `encode_tagged` and
[Step 4](phase1-m1-04.md)'s `store_turn` / `store_label` into durable rows,
and read them back with on-disk corruption detection.

**Definition of done:** `core/src/db/store.rs` exposes a typed `StoreError`,
an idempotent `put` (`INSERT OR IGNORE` keyed by hash), and a `get` that
re-hashes fetched bytes before returning them — surfacing on-disk corruption
as a typed `HashMismatch` error. Module is re-exported from `db/mod.rs`.
Full unit coverage. `cargo test -p core db::store::`, `cargo clippy -p core
-- -D warnings`, and `cargo fmt --check` are all green.

## Context

[Step 2](phase1-m1.md#step-2--db-schema--migrations) created the empty `store`
table:

```sql
CREATE TABLE store (
    hash     BLOB PRIMARY KEY,   -- 16 bytes: BLAKE3 digest truncated to 128 bits
    payload  BLOB NOT NULL       -- format-tagged, postcard-serialized object
);
```

[Step 3](phase1-m1-03.md) shipped `encode_tagged` (returns `(Hash, Vec<u8>)`
where the bytes are tag ++ postcard payload). [Step 4](phase1-m1-04.md)
shipped `store_turn` / `store_label`, both thin wrappers that call
`encode_tagged` with their respective `(Kind, LATEST_*_VERSION)` and return
the same `(Hash, Vec<u8>)` shape. **Tagging and hashing already happen
upstream of this step.** What's missing is the durable read/write pair that
actually touches the `store` table, plus the integrity check on read.

The phase1-m1.md Step 5 wording says `put` "prepends the format tag, hashes
the tagged bytes, …" — that prose predates the Step 3 encode/decode split,
which moved tag-prepending and hashing into `hash.rs`. By the time Step 5
runs, every caller already has the `(Hash, Vec<u8>)` pair. `store::put` is
therefore the **insert half** of the round-trip, not a re-tagger; symmetry
is preserved by `store::get` re-hashing on read to detect bit-rot.

## Decisions locked in this step

- **Free functions over a `&Connection`, not methods on `Db`.** Two reasons:
  (1) the engine module (Step 11) will call `put` from inside a
  `Db::with_transaction(|tx| …)` closure that also writes a `journal` row,
  and `&Transaction` derefs to `&Connection` so a single `&Connection`
  parameter works for both standalone and in-transaction calls; (2) keeping
  `store` as a stateless module mirrors the shape of `migrations.rs`
  (a free `run(&mut Connection)`) and avoids growing the `Db` surface
  unnecessarily.

- **Caller supplies the hash; `put` does not re-hash.** `encode_tagged` /
  `store_turn` / `store_label` already produced `(Hash, Vec<u8>)`;
  re-hashing in `put` would be wasted work on the write path. A
  `debug_assert!(hash_tagged(bytes) == *hash)` in debug builds catches
  buggy callers; release builds skip the check (and any latent bug surfaces
  on the next `get`, which always re-hashes).

- **`put` returns `bool` indicating "newly inserted" vs "already present".**
  The dedup invariant from
  [data-model.md § Blob-and-tree persistence](data-model.md#blob-and-tree-persistence)
  ("a byte-identical payload is stored exactly once, no matter how many
  snapshots or journal entries reference it") is testable as
  `put(h, b) == true` then `put(h, b) == false`. The engine doesn't act on
  the boolean in M1 — it journals regardless of whether the blob was new —
  but the signal is free (`rusqlite::Statement::execute` already returns
  the affected-row count) and pins the no-op-on-duplicate behaviour in a
  test that is otherwise hard to observe.

- **`get` re-hashes on read.** [`hash.rs`](phase1-m1-03.md#3c--implement-coresrcprojecthashrs)
  catches postcard-level errors; bit-flips that still parse as valid postcard
  do not surface there. `store::get` is the only layer that knows the
  expected hash for a given fetched payload, so it is the only place
  bit-rot detection belongs. Phase1-m1.md Step 5 calls this out explicitly:
  > "**re-hash the fetched bytes and return a typed hash-mismatch error if
  > the recomputed hash differs from the lookup key** (catches on-disk
  > corruption — postcard-level errors are caught by hash.rs, but bit-flips
  > that still parse as valid postcard only get caught here)".

- **`get` returns the full tagged bytes (tag ++ postcard payload), not the
  stripped payload.** `load_turn` / `load_label` (already shipped in
  Step 4) parse the tag themselves and dispatch on the version nibble. If
  `get` stripped the tag, it would have to surface `(Kind, version,
  payload)` separately and the per-kind loaders would need a second entry
  point. Returning the tagged blob untouched keeps the boundary at "raw
  bytes in, raw bytes out" and lets the existing typed loaders work
  unchanged. (Phase1-m1.md Step 5 says "parse and strip the tag and return
  the payload" — read literally this would force a duplicate decode path;
  the natural reading is that `get` returns bytes that downstream callers
  then parse via `load_turn` / `load_label`.)

- **Typed `StoreError`, not bare `anyhow::Result`.** Callers need to
  distinguish "the row is missing" (a corrupt or out-of-sync journal — fatal
  to the open) from "the row exists but the bytes are wrong" (on-disk
  corruption — same fatality, different diagnostic) from "SQLite I/O failed"
  (transient, retryable in principle). A `StoreError` enum with `From<rusqlite::Error>`
  composes cleanly into the engine's `anyhow::Result` via `?` while keeping
  variants matchable in tests and in the eventual user-facing error mapping
  (M6).

- **No `ToSql` / `FromSql` impls for `Hash`.** The struct's inner
  `[u8; HASH_BYTES]` already passes through rusqlite as `&[u8]` for
  parameter binding, and reads back as `Vec<u8>` for results. A custom impl
  would save four characters at the call site and add 30 lines of trait
  machinery. Skip until a third caller wants it.

## Module surface

### New: `core/src/db/store.rs`

```rust
//! Content-addressed blob store: thin INSERT/SELECT over the `store` table.
//!
//! Callers serialize their object via `hash.rs` / per-kind store helpers,
//! receive `(Hash, Vec<u8>)`, and hand both to `put`. `get` is the inverse
//! — it returns the tagged bytes after verifying the on-disk payload still
//! hashes to the requested key (bit-rot detection).

use rusqlite::{Connection, OptionalExtension};

use crate::project::hash::{hash_tagged, Hash};

/// Errors returned by blob-store operations.
#[derive(Debug)]
pub enum StoreError {
    /// No row exists in `store` with the requested hash.
    NotFound(Hash),
    /// A row exists, but its payload no longer hashes to the lookup key
    /// (on-disk corruption — bit-flip, partial write, schema tampering).
    HashMismatch {
        /// Hash the caller asked for (and used as the row's PRIMARY KEY).
        expected: Hash,
        /// Hash actually computed from the fetched payload bytes.
        computed: Hash,
    },
    /// Underlying SQLite I/O or query error.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for StoreError { /* matches the three variants */ }
impl std::error::Error for StoreError { /* Sqlite is the source */ }
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self { StoreError::Sqlite(e) }
}

/// Insert a content-addressed blob, idempotent on duplicate.
///
/// `bytes` must be the full tag ++ postcard blob produced by
/// `encode_tagged` (or a per-kind helper that wraps it); `hash` must be the
/// 128-bit BLAKE3 of those bytes. A `debug_assert!` checks the pairing in
/// debug builds; release builds trust the caller and rely on `get`'s
/// re-hash to surface any mismatch on read.
///
/// Returns `Ok(true)` when a new row was written, `Ok(false)` when a row
/// with this hash was already present (the canonical dedup signal).
pub(crate) fn put(conn: &Connection, hash: &Hash, bytes: &[u8])
    -> Result<bool, StoreError>;

/// Fetch a content-addressed blob, verifying its on-disk integrity.
///
/// Re-hashes the fetched payload and returns
/// [`StoreError::HashMismatch`] if the recomputed hash differs from
/// `hash` (bit-rot detection — postcard-valid corruption that
/// `hash.rs` cannot see). Returns [`StoreError::NotFound`] if no row
/// exists for `hash`.
///
/// The returned `Vec<u8>` is the full tagged blob, suitable for the
/// kind-specific loader (`load_turn`, `load_label`, …) without further
/// processing.
pub(crate) fn get(conn: &Connection, hash: &Hash)
    -> Result<Vec<u8>, StoreError>;
```

Implementation sketch:

```rust
pub(crate) fn put(conn: &Connection, hash: &Hash, bytes: &[u8])
    -> Result<bool, StoreError>
{
    debug_assert_eq!(
        hash_tagged(bytes), *hash,
        "store::put called with hash that doesn't match bytes",
    );
    let affected = conn.execute(
        "INSERT OR IGNORE INTO store (hash, payload) VALUES (?1, ?2)",
        (&hash.0[..], bytes),
    )?;
    Ok(affected == 1)
}

pub(crate) fn get(conn: &Connection, hash: &Hash)
    -> Result<Vec<u8>, StoreError>
{
    let payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM store WHERE hash = ?1",
            (&hash.0[..],),
            |r| r.get(0),
        )
        .optional()?
        .ok_or(StoreError::NotFound(*hash))?;
    let computed = hash_tagged(&payload);
    if computed != *hash {
        return Err(StoreError::HashMismatch { expected: *hash, computed });
    }
    Ok(payload)
}
```

### Revised: `core/src/db/mod.rs`

Add `pub mod store;` (sibling to the existing `mod migrations;`). The
`store` module is `pub` so `pub(crate) fn put` / `get` are reachable from
`crate::project::engine` in Step 11. The existing `Db::conn() -> &Connection`
and `Db::with_transaction` accessors remain unchanged; `store::put` /
`get` are called as `store::put(db.conn(), …)` outside transactions and
`store::put(tx, …)` inside `with_transaction` closures.

The `#[allow(dead_code)]` on `Db::conn` should be removed in this step
(`store` is the first internal consumer; no further attribute is needed
once it's used).

### Reuse from existing code

- `hash::{Hash, hash_tagged}` ([`core/src/project/hash.rs`](../src-tauri/core/src/project/hash.rs))
  — for `debug_assert!` in `put` and for the re-hash check in `get`.
- `rusqlite::{Connection, OptionalExtension}` — already in
  [`core/Cargo.toml`](../src-tauri/core/Cargo.toml). `OptionalExtension`
  gives the `Option<T>` adapter on `query_row` that distinguishes
  "no rows" from a SQL error.
- No new dependencies. No schema changes.

## Test plan

All tests inline `#[cfg(test)] mod tests` in `store.rs`. Use the existing
`Db::open` (which runs migrations) over a `tempfile::tempdir` directory,
matching the pattern in [`core/src/db/mod.rs`](../src-tauri/core/src/db/mod.rs).

Shared helper:

```rust
fn open_tmp_db() -> (tempfile::TempDir, super::super::Db) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.vocalboard");
    let db = super::super::Db::open(&path).unwrap();
    (dir, db)
}

fn sample_blob() -> (Hash, Vec<u8>) {
    // Use store_turn or encode_tagged so the test exercises the real
    // tagging path, not a hand-built byte buffer.
    crate::project::turn::store_turn(&sample_turn()).unwrap()
}
```

(The `sample_turn()` constructor is the test-only helper already in
[`turn.rs`](../src-tauri/core/src/project/turn.rs); promote it to
`pub(crate)` inside `#[cfg(test)]` or duplicate a minimal `Turn` literal
here. The duplication is fine — `store.rs` only needs *some* tagged
blob, not specifically a `Turn`.)

### Tests

S1. **`put_get_round_trip`** — `put(conn, &h, &b)?` then
    `get(conn, &h)? == b`. The bytes returned by `get` are byte-identical
    to the bytes written (tag included).

S2. **`put_returns_true_on_first_insert`** — `put` returns `Ok(true)` for a
    hash not previously in `store`.

S3. **`put_returns_false_on_duplicate`** — calling `put` twice with the
    same `(hash, bytes)` returns `Ok(true)` then `Ok(false)`. Pins the
    `INSERT OR IGNORE` dedup contract.

S4. **`put_dedup_keeps_one_row`** — after putting the same blob 5 times,
    `SELECT COUNT(*) FROM store WHERE hash = ?` is `1`. (Belt-and-suspenders
    to S3: the bool could lie about the underlying state.)

S5. **`distinct_blobs_coexist`** — two different turns (different `id`
    fields ⇒ different hashes per the existing `hash_sensitive_to_id`
    coverage in `turn.rs`) both put, both gettable, `COUNT(*) == 2`.

S6. **`get_missing_returns_not_found`** — `get(conn, &Hash([0u8; 16]))`
    returns `Err(StoreError::NotFound(h))` where `h.0 == [0u8; 16]`. The
    error carries the requested hash for diagnostic logging.

S7. **`get_detects_payload_corruption`** — after a successful `put`, flip
    one byte in the stored payload via a raw SQL `UPDATE`:
    ```rust
    conn.execute(
        "UPDATE store SET payload = ?1 WHERE hash = ?2",
        (&corrupted_bytes[..], &h.0[..]),
    )?;
    let err = get(conn, &h).unwrap_err();
    assert!(matches!(err, StoreError::HashMismatch { .. }));
    ```
    Pins the bit-rot detection. The expected/computed hashes in the error
    are not asserted equal to specific values (they depend on the flipped
    byte) — `matches!` is enough.

S8. **`get_detects_truncation`** — same approach as S7 but the corrupted
    payload is just `&b[..b.len()-1]` (the last byte dropped). `hash_tagged`
    of the truncated bytes ≠ `h`, so `HashMismatch` fires. (Truncation
    catches a different failure mode from a flipped byte — e.g. a partial
    `write()` mid-crash — and the same `re-hash != expected` check covers
    both.)

S9. **`put_then_get_works_inside_transaction`** — open a transaction via
    `db.with_transaction(|tx| { store::put(tx, &h, &b)?; let read =
    store::get(tx, &h)?; … Ok(()) })`. Verifies `&Transaction → &Connection`
    deref works for both functions and that an uncommitted blob is visible
    within its own transaction.

S10. **`put_rolls_back_on_transaction_error`** — `db.with_transaction` that
     calls `store::put` then returns `Err`; after the closure,
     `store::get(db.conn(), &h)` returns `NotFound`. Pins that store ops
     respect the surrounding transaction.

S11. **`put_hash_mismatch_debug_assert`** (`#[cfg(debug_assertions)]`,
     `#[should_panic(expected = "doesn't match")]`) — `put(conn, &Hash([0u8;
     16]), &b)` where `b` is a real tagged blob whose hash is non-zero
     panics in debug builds. Skipped in release builds. (Optional but
     cheap — pins the `debug_assert!` contract.)

S12. **`empty_payload_round_trips`** — `put(conn, &hash_tagged(&[]), &[])`
     succeeds, `get` returns `Vec::new()`. (Edge case: zero-length BLOBs
     are legal in SQLite; the schema's `NOT NULL` does not forbid empty.)
     This is more of a sanity check that nothing in the SQL or re-hash
     path special-cases empty input.

### Out-of-scope tests (covered elsewhere or in later steps)

- Tag-byte / kind-mismatch / postcard-decode errors — covered by `turn.rs`
  and `label.rs` Step 4 tests (`load_turn_kind_mismatch`,
  `load_turn_unknown_version`, `load_turn_truncated_input`, …). `store.rs`
  is kind-agnostic; it neither parses tags nor calls postcard.
- Concurrent reader/writer behaviour — single-connection M1, no
  concurrency. The background snapshot writer's separate connection
  (Step 11) carries its own threading test.
- Schema corruption (e.g. a doctored `user_version`) — covered by Step 2's
  migration tests.
- Cross-process file locking — out of scope for M1.

## Out of scope for Step 5

- **The `journal` table.** `put` writes to `store` only; the journal write
  that *references* the new hash is Step 9. A "blob exists in store but is
  unreferenced by any journal row" is a perfectly valid intermediate state
  during a multi-row transaction.
- **Transaction management.** Callers wrap `store::put` / `get` in
  `db.with_transaction(…)` when they want atomicity with other writes
  (Step 9 onward). `store::put` against a `&Connection` without a
  surrounding transaction runs in SQLite's implicit per-statement
  transaction — fine for tests and for one-off opens.
- **High-level engine wrappers.** `ProjectState` may grow `store_turn` /
  `load_turn` convenience methods in Step 11 (turn-typed put/get that
  call `crate::project::turn::store_turn` and `store::put` /
  `store::get` and `load_turn` together). Step 5 ships only the
  byte-level primitives.
- **A `compact` operation** to normalize mixed-version stores — deferred
  past M1 per [data-model.md § Schema version](data-model.md#schema-version).
- **Garbage collection** of unreferenced blobs — the persistent ID in
  every `Turn` / `Label` payload guarantees uniqueness, and dedup means
  re-derived blobs cost nothing, so unreferenced rows are rare and
  cheap. A post-M1 `vacuum`-style sweep is not needed for correctness.
- **`ToSql` / `FromSql` for `Hash`** — see decision above.
- **The `journal_type_idx` index** — already created by Step 2's
  migration; used by Step 9, not Step 5.

## Documentation touches

- **No design doc changes are required.** Step 5 is a thin
  implementation of behaviour already specified in
  [data-model.md § Turn and Label blobs](data-model.md#turn-and-label-blobs)
  and [§ Blob-and-tree persistence](data-model.md#blob-and-tree-persistence).
  The phase1-m1.md Step 5 prose ("prepend the format tag, hash the tagged
  bytes") is mildly stale relative to the post-Step-3 reality (tagging and
  hashing now live in `encode_tagged`), but it's a description of the
  end-to-end pipeline rather than of `store::put` specifically — leaving
  it lets the high-level Step 5 bullet still read end-to-end. A reviewer
  who flags it can take a one-line clarifying edit; the action plan in
  this file is the authoritative breakdown either way.
- A short doc-comment header on the new module summarises what was
  decided above (boundary at "raw tagged bytes in, raw tagged bytes
  out"; integrity check on read; dedup signal on write).

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` (must remain green with
  `unwrap_used`, `expect_used`, `panic`, and `missing_docs` all CI-gated).
- `cargo test -p core db::store::` — runs the 12 tests above.
- `cargo test -p core db::` — confirms no regression against the existing
  `db::mod` and `db::migrations` tests (the `pub mod store;` addition and
  the `#[allow(dead_code)]` removal on `Db::conn` must not break either).
- `cargo test -p core` — confirms no regression elsewhere; in particular
  that the Step 4 `turn.rs` / `label.rs` tests still pass (they construct
  `(Hash, Vec<u8>)` via `encode_tagged` directly, so they don't touch
  `store.rs`, but a stray `pub(crate)` visibility mistake could surface
  as a downstream compile error).
- One commit on `claude/1M1`, **unsigned** per the GPG-by-branch policy
  in [CLAUDE.md](../CLAUDE.md). Subject:
  `1M1-05: blob store (put/get with on-read hash verification)`.

## Downstream implications (flag for later steps)

- **Step 6 (`tree.rs`):** the tree carries `Arc<T>` plus the on-disk
  `Hash`; it never calls `store::put` itself. Snapshot replay (Step 8)
  calls `store::get(conn, &h)?` then `load_turn(&bytes)?` /
  `load_label(&bytes)?` to populate the tree.
- **Step 9 (`journal.rs` + `metadata.rs`):** every `type = -1` /
  `type = 1` journal write is paired with a `store::put` for the
  payload blob it references, inside the same `with_transaction`
  closure. The `bool` return from `put` is **not** consulted — the
  journal row is appended unconditionally (dedup means the blob is
  already there if `put` returned `false`).
- **Step 11 (`engine.rs`):** `ProjectState` holds a single primary
  `Db` connection plus a second connection for the background
  snapshot writer (per [phase1-m1.md § Step 11](phase1-m1.md#step-11--projectstate-engine--snapshot-writer-projectengeners)
  threading note). Both connections call `store::put` / `get` via
  `&Connection` deref; the WAL pragma applied by `Db::open` permits
  the concurrent-reader + serialized-writer pattern this requires.
- **Phase 6 scripting / plugin host (post-M1):** the `pub(crate)`
  visibility on `put` / `get` is deliberate — only the engine layer
  should reach into the store. If a scripting API ever needs raw
  blob access, it should go through a high-level command, not the
  store module directly.
