# Phase 1 · M1 · Step 3 — Hashing + Serialization (action plan)

Per-step action plan for Step 3 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md](../design/data-model.md). This step lays down the content-addressing
primitives — 16-byte BLAKE3-128 hash and format-tagged postcard serialization —
under a **lazy migration** scheme: old-format blobs stay readable forever via
per-version deserializers, and re-serialization only happens when a blob's
content is genuinely edited.

**Definition of done:** `core/src/project/hash.rs` exposes `Hash`, `Kind`, the
tag-byte helpers, and generic `encode_tagged` / `decode_tagged` with full unit
coverage; [data-model.md](../design/data-model.md) and [phase1-m1.md](phase1-m1.md) are
updated to describe lazy migration and the kind+version tag-byte layout;
`cargo test`, `cargo clippy -D warnings`, and `cargo fmt --check` are all green.

## Decisions locked in this step

- **Lazy migration over eager rewrite.** Eager rewrite cascades through the
  content-addressed store — rewriting a `Turn` changes its hash, which forces
  rewriting every `Snapshot` referencing it, which forces rewriting `type = 1`
  journal rows, which (combined with delta-shape changes) forces rewriting
  `type = 0` rows too. Nearly a full DB rewrite on open. The lazy scheme keeps
  old hashes valid forever; the loader dispatches on the version nibble and
  upgrades in memory; blobs are re-serialized in the new format only when their
  content actually changes. A future opt-in **compact** operation (post-M1) is
  the escape valve for normalizing a mixed-version store on user request.
- **Tag byte = kind + version, packed into one byte.** High nibble = kind, low
  nibble = version. 16 kinds × 16 versions per kind. Two-byte extension is the
  documented escape path if either ceiling is hit.
- **Delta versioning is inline in the journal.** `type = 0` payloads get a
  leading `delta_version: u8` byte before the postcard `Vec<Delta>`; M1 writes
  `0x01`. `type = -1` and `type = 1` payloads are just hashes pointing into the
  tagged store, so they need no extra version byte — the store blob's own tag
  carries the version.

## Tag-byte layout

```rust
#[repr(u8)]
pub enum Kind {
    Turn        = 0x1,
    Metadata    = 0x2,
    Snapshot    = 0x3,
    RoomTonePcm = 0x4,
    Embedding   = 0x5,
}
// Tag byte = (kind << 4) | version.
// e.g. Turn v1 = 0x11, Turn v2 = 0x12, Snapshot v1 = 0x31.
```

The hash covers the **full tagged bytes** (tag ++ postcard payload), so two
blobs with identical postcard content but different tags hash differently.
Encoding always writes the latest version for the kind; decoding reads any
version present in the per-kind dispatch table.

## Dispatch architecture (only V1 implementations exist in M1)

For each kind, the per-kind module (Step 4 for `Turn`, Step 8 for `Snapshot`,
Step 9 for `Metadata`) will expose:

- A latest-version in-memory type (e.g. `Turn`).
- A `mod vN` submodule per historical version, holding the on-disk struct
  (e.g. `TurnV1`) and a deterministic `From<TurnVN> for Turn` upgrade.
- A typed loader `load_turn(bytes) -> Result<Turn, DecodeError>` that parses
  the tag, asserts the kind, matches the version, deserializes with the right
  struct, and upgrades.
- A typed writer `store_turn(&Turn) -> (Hash, Vec<u8>)` that always emits
  `LATEST_TURN_VERSION`.

M1 has one arm per dispatch table (V1 only). Adding V2 later means: add
`mod v2`, add the `2 => ...` arm, implement `From<TurnV2> for Turn`, bump
`LATEST_TURN_VERSION`. No restructuring of `hash.rs`.

Step 3 itself provides only the **generic primitives**; the per-kind loaders
and writers belong with their structs in later steps.

## Sub-steps

### 3a — Update [`design/data-model.md`](../design/data-model.md)

Three surgical edits:

1. **§ Schema version:** replace "a migration deserializes old blobs with the
   old struct definition (kept in the migration module) and re-serializes them
   with the new one" with a paragraph describing lazy migration — the tag
   byte's low nibble is a version, per-version deserializers stay in the
   migration module indefinitely, blobs are re-serialized only when their
   content is edited (a read-only open does zero rewrite work), mixed-version
   snapshots are normal until edits replace old turns, and a future opt-in
   compact operation is the escape valve for normalization.
2. **§ Hashing and serialization (`{#serialization}` anchor):** extend the
   `FormatTag` description from a flat enum to the kind+version nibble split
   (table + 16-version ceiling + extension path).
3. **§ Deltas (and the schema-DDL comment on the `journal.payload` column):**
   document the leading `delta_version: u8` byte on `type = 0` payloads, and
   note that `type = -1` / `type = 1` carry no extra version byte (the tagged
   store blob they point to carries it).

- **Verify:** doc reads coherently end-to-end; no remaining references to
  eager re-serialization or to a flat `FormatTag`.

### 3b — Update [`phase1-m1.md`](phase1-m1.md) Step 3

- Replace "Bincode payload" with "postcard payload" (stale from the postcard
  substitution decided in Step 1).
- Replace the eager-migration sentence with a one-line pointer to the
  lazy-migration policy in `data-model.md`.
- Describe the kind+version nibble split (one sentence).
- Cross-reference this document (`phase1-m1-03.md`).
- **Verify:** `phase1-m1.md` and `data-model.md` describe the same scheme;
  no internal contradictions.

### 3c — Implement `core/src/project/hash.rs`

Module surface:

```rust
pub const HASH_BYTES: usize = 16;

pub struct Hash(pub [u8; HASH_BYTES]);
// derives Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize.
// Debug formats as hex; no Display impl yet.

#[repr(u8)]
pub enum Kind {
    Turn        = 0x1,
    Metadata    = 0x2,
    Snapshot    = 0x3,
    RoomTonePcm = 0x4,
    Embedding   = 0x5,
}

pub fn tag_byte(kind: Kind, version: u8) -> u8;       // debug_assert! version <= 0x0F
pub fn parse_tag(b: u8) -> Result<(Kind, u8), DecodeError>;
pub fn hash_tagged(bytes: &[u8]) -> Hash;             // BLAKE3 truncated to HASH_BYTES

pub fn encode_tagged<T: Serialize>(kind: Kind, version: u8, value: &T)
    -> (Hash, Vec<u8>);
pub fn decode_tagged<T: DeserializeOwned>(bytes: &[u8])
    -> Result<(Kind, u8, T), DecodeError>;

pub enum DecodeError {
    Empty,
    UnknownKind(u8),
    UnknownVersion { kind: Kind, version: u8 },
    KindMismatch { expected: Kind, found: Kind },
    Postcard(postcard::Error),
}
```

Notes:

- All `pub` items doc-commented; `#![warn(missing_docs)]` is a CI gate.
- No `unwrap` / `expect` / `panic` in non-test code without a justifying
  comment; `clippy::unwrap_used` is gated.
- Hashed structs throughout the codebase use ordered collections only (`Vec`,
  `BTreeMap`) per the determinism invariant in `data-model.md` — `hash.rs`
  itself uses no maps, but this is the rule its callers must follow.
- Per-kind loaders/writers are **not** in this module; they will live with
  their structs (`turn.rs`, `snapshot.rs`, `metadata.rs`) in later steps and
  call `encode_tagged` / `decode_tagged` for the generic plumbing.

**Verify (unit tests in `#[cfg(test)] mod tests` within `hash.rs`):**

- **Tag round-trip:** for every `Kind` and `v in 0..=15`,
  `parse_tag(tag_byte(k, v)) == Ok((k, v))`.
- **Tag layout pinned:** `tag_byte(Kind::Turn, 1) == 0x11`,
  `tag_byte(Kind::Snapshot, 1) == 0x31`, etc. Pins the on-disk format so a
  later edit cannot silently reshuffle codes.
- **Hash determinism:** the same struct value encoded twice produces
  byte-identical output and the same `Hash`.
- **Hash covers tag:** changing the tag byte while keeping postcard bytes
  constant changes the `Hash`.
- **Hash width:** `HASH_BYTES == 16`; computed hash length matches.
- **Encode/decode round-trip:** for a representative struct,
  `decode_tagged(encode_tagged(k, v, &x).1)` returns `(k, v, x)`.
- **Kind mismatch:** decoding bytes tagged `Snapshot` as a `Turn` returns
  `KindMismatch`.
- **Unknown version:** decoding bytes tagged `(Turn, 0xF)` returns
  `UnknownVersion`.
- **Empty payload:** `decode_tagged(&[])` returns `Empty`.
- **Truncated payload:** decoding the first few bytes of a valid encoding
  (just the tag, or tag + partial postcard prefix) returns
  `DecodeError::Postcard(_)`. Pins the error mapping so a future refactor
  cannot silently `unwrap` the postcard result. (Hash-mismatch detection for
  on-disk corruption belongs in Step 5's `store::get`, not here — `hash.rs`
  doesn't know the expected hash.)
- **Postcard determinism guard:** a struct containing a `BTreeMap` encodes
  identically across runs (sanity check that callers using ordered collections
  get deterministic bytes from postcard).

### 3d — Final pass + commit

- `cargo fmt --check` from `src-tauri/`
- `cargo clippy -p core -- -D warnings`
- `cargo test -p core hash::`
- Manual diff review of `data-model.md` and `phase1-m1.md` for cross-doc
  consistency.
- One commit `1M1-03: hashing + serialization (lazy-migration tag scheme)` on
  `claude/1M1`, unsigned per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md). The commit bundles the two design-doc edits with
  `hash.rs` and its tests, per the pre-commit checklist — design docs stay in
  sync with the implementation that motivates them.

## Downstream implication (flag in Step 6)

Lazy migration relies on **hash-at-edit-time, not hash-at-snapshot-time**.
Each tree `Node` (Step 6) will need to carry the turn's hash alongside its
`Arc<Turn>` — sourced from the on-disk tag at load time (which may be V1) or
computed at edit time (always V_latest). Snapshotting collects those existing
hashes; it does **not** re-serialize and re-hash turns. This is what makes
read-only opens zero-work and what produces mixed-version snapshots after
partial edits of an old project. The Step 6 plan should call this out so the
`Node` shape is right from the start.

## Out of scope for Step 3

- The blob store (`db/store.rs`) — Step 5.
- `Turn` / `Word` / `Splice` and their `mod v1` submodule — Step 4 (consumes
  `hash.rs` via `encode_tagged` / `decode_tagged`).
- Any V2 of any format — there is no V2 yet; the per-kind dispatch tables
  built in Steps 4, 8, 9 will each have one arm.
- A **compact** command — deferred past M1; named in the `data-model.md`
  update as the future escape valve for normalizing mixed-version stores.
