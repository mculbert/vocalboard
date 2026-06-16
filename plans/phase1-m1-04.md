# Phase 1 · M1 · Step 4 — Tree element payloads: `Turn` / `Word` / `Splice` / `Label` + `Tilable` (action plan)

Per-step action plan for Step 4 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md § Turn payload (speech tracks)](../design/data-model.md#turn-payload-speech-tracks)
and [§ Label payload (track 0)](../design/data-model.md#label-payload-track-0).
This step defines the units stored in the content-addressed blob store: the
in-memory `Turn` / `Word` / `Splice` (latest format) for speech tracks and
`Label` for track 0, each with a frozen V1 wire schema and a kind-typed
dispatching loader/writer that calls into the generic primitives from
[Step 3](phase1-m1-03.md). It also introduces the [`Tilable`](../design/data-model.md#tilable-trait)
trait that lets Step 6's implicit timeline tree be generic over its element type.

**Definition of done:** the project module exposes `Tilable`, `Turn`, `Word`,
`Splice`, `WordType`, `SpliceKind`, `Label`, `LabelKind`, with V1 dispatch arms
(`v1::TurnV1`, `v1::LabelV1`, and their conversions), `LATEST_TURN_VERSION`,
`LATEST_LABEL_VERSION`, typed `store_turn` / `load_turn` and `store_label` /
`load_label` helpers, and full unit coverage. `hash.rs` recognises
`Kind::Label = 0x6`. The project's module index re-exports the new modules;
`cargo test`, `cargo clippy -D warnings`, and `cargo fmt --check` are all green.

## Relationship to the prior Step 4 commit

The prior `1M1-04: Turn / Word / Splice with V1 wire schema` commit landed the
*original* V1 shape from data-model.md (labels-as-turns, source-only Splice
fields as `Option<i64>`, `WordType` carrying `Label` / `Section`). Subsequent
design review concluded the unified shape leaked turn-specific machinery into
the labels track, and the `Option<i64>` Splice fields were a half-expressed
invariant.

This step **supersedes** that work: it revises the V1 wire schema and adds the
Label kind. Per the pre-1.0 escape hatch on `mod v1`
([phase1-m1-04 — prior decisions; § Decisions locked](#decisions-locked-in-this-step)),
the V1 pinned bytes / pinned hash get regenerated and `min_app_version` is bumped.
The change lands as a follow-up commit on `claude/1M1` (unsigned per the
GPG-by-branch policy in [CLAUDE.md](../CLAUDE.md)); the eventual squash to `main`
collapses the original Step 4 and this revision into one coherent M1 increment.

## Context

Step 3 laid down content-addressing primitives (16-byte BLAKE3 hash, kind+version
tag byte, generic `encode_tagged` / `decode_tagged_as`) and committed to **lazy
migration**: old-format blobs stay readable forever via per-version
deserializers, and re-serialization only happens on genuine content edits. Step
4 is the first kind to materialize that dispatch pattern, now for two kinds
(`Turn` and `Label`). Without it, neither the blob store (Step 5) nor the
timeline tree (Step 6) has units to store — the tree's `Node<T>` carries
`Arc<T>` plus the element's existing on-disk hash, sourced from whichever V_N
deserializer fired at load time.

The shape of the M1 V1 wire formats is the project's first persisted-blob
contract. Once shipped, V1 deserialization MUST work on any future build (G1 in
[conventions.md](../design/conventions.md#g-data--persistence-integrity)), which is why
each V1 is defined as its own frozen struct from day 1 rather than as an alias
for the in-memory shape.

## Decisions locked in this step

- **`Tilable` is a one-method trait.** `fn total_duration(&self) -> i64`,
  implemented by both `Turn` (`turn_duration + post_turn_silence`) and `Label`
  (`post_label_silence`). Anything that varies between kinds (in-element offset
  semantics, edit operations, V_N schema shape) lives outside the trait, in
  per-kind code.

- **`mod v1` holds a separate frozen struct per kind.** `v1::TurnV1` for Turn
  (with `WordV1` / `SpliceV1` / `WordTypeV1` / `SpliceKindV1`); `v1::LabelV1`
  for Label (with `LabelKindV1`). Each has explicit `From<v1::*V1> for T` and
  `From<&T> for v1::*V1` conversions. In-memory types evolve; the V1 structs
  do not.

- **`store_turn` always emits `LATEST_TURN_VERSION`; `store_label` always emits
  `LATEST_LABEL_VERSION`** (both currently `1`). Each converts `&T → v1::*V1`
  then calls `encode_tagged(kind, version, &v1)`.

- **`load_turn` and `load_label` each peek the tag, assert their `Kind`,
  dispatch on version.** The M1 dispatch table has one arm per kind: `1 ⇒
  decode_tagged_as::<v1::*V1> ⇒ T::from`. Other versions return
  `DecodeError::UnknownVersion`. Other kinds return `DecodeError::KindMismatch`.

- **`WordType` drops `Label` and `Section`.** With `Label` as its own kind, no
  `WordType` variant needs to disambiguate a label "turn." `WordType` is now
  exactly `Normal | Disfluency | Sound` — the three speech-aligned token kinds
  that genuinely share the Word's shape (positions, cut/mute state,
  source-seconds).

- **`Splice` source-only fields move into `SpliceKind::Source`.**
  `SpliceKind` becomes a data enum: `Source { source_start_sample,
  source_decode_offset }`, `RoomTone`, `Silence`. The parent `Splice` keeps
  the fields common to every variant (`length_samples`, `fade_in_samples`,
  `fade_out_samples`). This removes the `Option<i64>` invariant that nothing
  in the type system enforced.

- **`Label` is its own struct, its own blob kind (`Kind::Label = 0x6`), its
  own ID counter (`project.next_label_id`).** Labels and turns are
  different entities; sharing the `next_turn_id` counter would conflate them.

- **No `Eq` on `Turn` / `Word` / `Splice` — but `Eq` IS derived on `Label`.**
  `Word.start_sec` / `end_sec` are `f64`, so the Turn graph is `PartialEq`
  only. Label has no float fields, so it supports full `Eq`. Tests rely on
  `PartialEq` (NaN does not appear in any constructed value; postcard
  serialises NaN bitwise so it round-trips byte-identically regardless).

- **No `Hash` (the trait) on any element struct.** Content addressing uses the
  BLAKE3 hash of the serialized bytes, not a `std::hash::Hash` impl.

- **V1 wire formats are pinned by byte-equality tests** for each kind: one
  hand-constructed `TurnV1` (populated word + two splices including one
  `Source` variant) and one hand-constructed `LabelV1` (one of each
  `LabelKind`). Each encodes to a hardcoded hex byte sequence. This is the
  belt-and-suspenders to the round-trip test: it catches self-consistent
  breakage that round-trip alone cannot (a postcard-rule change, a field-order
  swap that flips encode and decode symmetrically, an enum-variant reorder).

- **Sample fields are nominally ≥0; the spec uses `i64` for timeline-math
  ergonomics** (the temporal query subtracts `left_subtree_sum` from a search
  position and uses the sign of the result as the "recurse left" signal). The
  ≥0 invariant is enforced at command-schema boundaries (`"minimum": 0` on
  sample params, landing with the M4/M5 mutation commands) and constructor
  `debug_assert!`s (landing with the M4/M5 constructors). M1's element structs
  are plain `pub`-fields structs with no constructor, so no assertion lives in
  Step 4 itself; the cross-reference to
  [data-model.md § Time representation](../design/data-model.md#time-representation)
  carries the rule.

- **V1 frozen-shape discipline binds at first public release, not at this
  commit.** Backward compatibility is owed to `.vocalboard` files in the wild;
  before 1.0 there are no such files. Pre-release, both `v1::TurnV1` and
  `v1::LabelV1` MAY be revised if M1–M7 implementation surfaces a genuinely
  missing or wrong field — every revision requires regenerating the affected
  pinned hex bytes, pinned hash, and any committed G1 fixtures (Step 13), and
  SHOULD bump `min_app_version` so in-flight dev projects refuse cleanly on the
  modified app. **This step is exactly such a pre-1.0 revision** of the prior
  Turn V1. Post-release, V1 is frozen indefinitely and shape changes go
  through `mod v2` plus an `UpdateAfter` re-serialization on edit.

## Module surface

### New: `core/src/project/tilable.rs`

```rust
//! Tilable: the contract every tree-element type implements so the implicit
//! timeline tree (Step 6) can be generic over Turn / Label.

/// Total contribution of this element to its track's timeline, in project-rate samples.
///
/// Used by the tree's `left_subtree_sum` augmentation and the temporal-query
/// advance step. See [data-model.md § Tilable trait](../design/data-model.md#tilable-trait).
pub trait Tilable {
    /// Returns the element's total contribution to the timeline, in samples.
    fn total_duration(&self) -> i64;
}
```

The `Tilable` impls for `Turn` and `Label` live with their types in `turn.rs`
and `label.rs` respectively (Rust idiom: impls go next to the type, not the
trait). The trait itself has no behaviour to test in isolation — coverage is
the `tilable_total_duration_turn` and `tilable_total_duration_label` tests in
the respective module test suites. **No `#[cfg(test)] mod tests` in
`tilable.rs`.**

### Revised: `core/src/project/turn.rs`

The high-level surface stays close to the prior step (`Turn`, `Word`, `Splice`,
their V1 wire-format twins, `store_turn` / `load_turn`, `LATEST_TURN_VERSION`),
with three changes:

```rust
pub enum WordType {
    Normal,
    Disfluency,
    Sound,
    // No Label / Section: those are not Word kinds.
}

pub struct Splice {
    pub length_samples:   i64,
    pub fade_in_samples:  i64,
    pub fade_out_samples: i64,
    pub kind:             SpliceKind,
}

pub enum SpliceKind {
    Source { source_start_sample: i64, source_decode_offset: i64 },
    RoomTone,
    Silence,
}

impl Tilable for Turn {
    fn total_duration(&self) -> i64 { self.turn_duration + self.post_turn_silence }
}
```

`mod v1` mirrors the new in-memory shape with `TurnV1`, `WordV1`, `WordTypeV1`,
`SpliceV1`, `SpliceKindV1` (with the same `Source { … } | RoomTone | Silence`
variant data). The `From` impls are total identity-shaped conversions, both
ways. Tests 18 / 19 are regenerated against the new shape.

### New: `core/src/project/label.rs`

```rust
//! Label / LabelKind — the unit stored as a Kind::Label blob (track 0).
//!
//! The in-memory types are the LATEST format. `mod v1` holds the frozen V1
//! wire schema (currently field-identical to the in-memory types) with
//! explicit conversions; future V2 introduces `mod v2` and evolves the
//! in-memory types, while `mod v1` stays untouched.

use serde::{Deserialize, Serialize};

use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};
use super::tilable::Tilable;

pub const LATEST_LABEL_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id:                  u64,
    pub text:                String,
    pub kind:                LabelKind,
    pub post_label_silence:  i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelKind {
    Plain,
    Section,
}

impl Tilable for Label {
    fn total_duration(&self) -> i64 { self.post_label_silence }
}

pub fn store_label(label: &Label) -> Result<(Hash, Vec<u8>), postcard::Error> { /* ... */ }
pub fn load_label(bytes: &[u8]) -> Result<Label, DecodeError> { /* ... */ }

pub mod v1 {
    /// Frozen V1 wire schema; field-identical to in-memory.
    /// Pre-1.0 escape hatch documented in the same terms as TurnV1.
    pub struct LabelV1 { /* same fields as Label */ }
    pub enum LabelKindV1 { Plain, Section }
}

// Total identity-shaped From impls between Label / LabelKind and their V1 twins.
```

### Revised: `core/src/project/hash.rs`

Add `Kind::Label = 0x6` to the `Kind` enum and the `parse_tag` match. Extend
the `tag_layout_pinned` and `tag_round_trip` tests to cover the new variant.

### Revised: `core/src/project/mod.rs`

Add `pub mod tilable;`, `pub mod label;`. (Existing `pub mod turn;` stays.)

### Revised: `core/migrations/0001_initial.sql`

Two amendments, both under the pre-1.0 escape hatch (no `.vocalboard` files
in the wild):

1. Add `next_label_id INTEGER NOT NULL DEFAULT 1` to the `project` table,
   between `next_turn_id` and `created_at`, matching
   [data-model.md § Schema DDL](../design/data-model.md#schema-ddl-phase-1-user_version--1).

2. Bump the `min_app_version` column default from `'0.1.0'` to `'0.1.1'`.
   This is the in-flight-dev-project safety valve: dev projects created
   against the prior Turn V1 shape will refuse to open on a build carrying
   the revised V1, surfacing the refusal cleanly instead of silently
   deserializing nonsense. (Older `0.1.0` builds reading a fresh `0.1.1` file
   refuse symmetrically.)

`schema_version` stays at `1` — this is not a real migration, it is a
pre-1.0 reshape of the inaugural migration. Step 2's existing migration
tests stay green (no new test surface). Post-1.0 this kind of change goes
through a numbered migration plus a fixture-round-trip test instead.

No engine code reads `next_label_id` in this step — the counter is wired up
when label-creating commands land in M4/M5.

### Reuse from existing code

- `hash::{Hash, Kind, DecodeError, encode_tagged, decode_tagged_as, parse_tag}`
  ([`core/src/project/hash.rs`](../src-tauri/core/src/project/hash.rs)) —
  `store_*` calls `encode_tagged`; `load_*` calls `parse_tag` to peek the
  version then `decode_tagged_as` to decode the dispatched-on V_N type.
- No new dependencies. `serde`, `postcard`, `blake3` are already in
  [`core/Cargo.toml`](../src-tauri/core/Cargo.toml).

## Test plan

All tests inline `#[cfg(test)] mod tests` in their respective files.

### Reconciliation against the prior `turn.rs` test suite

The prior commit `6877f0c 1M1-04: Turn / Word / Splice with V1 wire schema`
landed 19 tests + a `capture_pinned_values` helper in `turn.rs`. Map them
to the revised suite as follows:

| Prior test                              | Action     | Notes |
|-----------------------------------------|------------|-------|
| `store_load_round_trip`                 | keep        | revise `sample_turn()` to new `Splice` / `SpliceKind::Source { … }` / 3-variant `WordType` |
| `empty_collections_round_trip`          | keep        | no shape changes touch the assertions |
| `label_turn_round_trip`                 | **delete**  | labels are no longer Turns — coverage moves to `label.rs` L1/L2 |
| `sound_event_round_trip`                | keep        | revise the `Splice` literal to the new shape |
| `each_word_type_round_trips`            | keep        | shrink from 5 variants to 3 (`Normal`, `Disfluency`, `Sound`); drop the `Label` / `Section` cases |
| `each_splice_kind_round_trips`          | keep        | revise to construct `SpliceKind::Source { source_start_sample, source_decode_offset }` directly (no `Option<i64>` on parent) |
| `extreme_value_samples_round_trip`      | keep        | put `i64::MAX` inside `SpliceKind::Source { source_start_sample: i64::MAX, … }` |
| `hash_determinism`                      | keep        | unchanged |
| `hash_sensitive_to_id`                  | keep        | unchanged |
| `hash_sensitive_to_speaker`             | keep        | unchanged |
| `hash_sensitive_to_word_text`           | keep        | unchanged |
| `tag_byte_is_turn_v1`                   | keep        | unchanged |
| `load_turn_kind_mismatch`               | keep        | unchanged (Snapshot-tagged ⇒ `KindMismatch`) |
| `load_turn_unknown_version`             | keep        | unchanged |
| `load_turn_empty_input`                 | keep        | unchanged |
| `load_turn_truncated_input`             | keep        | unchanged |
| `v1_conversions_total_round_trip`       | keep        | unchanged in intent; `sample_turn()` body updates |
| `v1_wire_format_pinned`                 | keep        | **regenerate** `PINNED_WIRE_BYTES` (see workflow below) |
| `hash_pinned_for_v1_sample`             | keep        | **regenerate** `PINNED_HASH` (same workflow) |
| `capture_pinned_values` (`#[ignore]`)   | keep        | revise `sample_v1_turn()` body, rerun to print new constants |
| —                                       | **add**     | `hash_sensitive_to_splice_source_offset` — `SpliceKind::Source.source_start_sample` change ⇒ different hash (Turn test #11 below) |
| —                                       | **add**     | `load_turn_kind_mismatch_label` — Label-tagged blob fed to `load_turn` ⇒ `KindMismatch { expected: Turn, found: Label }` (Turn test #14) |
| —                                       | **add**     | `tilable_total_duration_turn` — pins the `Tilable` impl (Turn test #21) |

Net change: 1 delete, 3 adds, 19 → 21 tests, plus all 17 new Label tests
and 2 new hash-module assertions.

### Pinned-bytes regeneration workflow

For both `v1_wire_format_pinned` and `hash_pinned_for_v1_sample` (Turn) and
their Label-side equivalents `v1_wire_format_pinned` / `v1_hash_pinned`:

1. Update `sample_v1_turn()` (and the new `sample_v1_label()`) bodies to
   reflect the revised V1 wire structs (`SpliceKind::Source { … }`,
   3-variant `WordType`, etc.).
2. Run the ignored capture helper:
   ```
   cargo test -p core turn::tests::capture_pinned_values -- --ignored --nocapture
   cargo test -p core label::tests::capture_pinned_values -- --ignored --nocapture
   ```
   Each prints freshly captured `PINNED_WIRE_BYTES` and `PINNED_HASH`
   constants in copy-pasteable form.
3. Paste the new constants into the pinned-test module, replacing the prior
   values byte-for-byte.
4. Re-run the normal test invocation (`cargo test -p core turn::` and
   `cargo test -p core label::`) — the pinned tests now pass against the
   freshly captured constants.

The Label module ships a sibling `capture_pinned_values` `#[ignore]` test
matching the Turn helper's structure.

### `turn.rs` — revised (replaces prior tests)

1. **`store_load_round_trip`** — a non-trivial `Turn` (id, speaker, three words
   including a `Disfluency` and a cut/muted word, two splices including a
   `Source` and a `Silence`) encodes and decodes back to `PartialEq`-equal.
2. **`empty_collections_round_trip`** — empty `words` and `splices` round-trip.
3. **`sound_event_round_trip`** — `speaker_id = None`, single `WordType::Sound`
   word, `Splice { kind: Source { … }, … }`. (The label-turn variant is gone.)
4. **`each_word_type_round_trips`** — one turn whose `words` contains all three
   `WordType` variants in order; decoded vector preserves order and identity.
5. **`each_splice_kind_round_trips`** — one turn whose `splices` contains all
   three `SpliceKind` variants, with `source_start_sample` /
   `source_decode_offset` only constructible inside the `Source` variant.
6. **`extreme_value_samples_round_trip`** — `turn_duration`, `length_samples`,
   and `Source { source_start_sample }` set to `i64::MAX` (and a small positive
   value); encodes and decodes losslessly. Does **not** test negatives — sample
   fields are nominally ≥0; see
   [data-model.md § Time representation](../design/data-model.md#time-representation).
7. **`hash_determinism`** — two encodings of the same `Turn` yield byte-identical
   bytes and the same `Hash`.
8. **`hash_sensitive_to_id`** — `id` change ⇒ different hash.
9. **`hash_sensitive_to_speaker`** — `speaker_id` change ⇒ different hash.
10. **`hash_sensitive_to_word_text`** — `Word.text` change ⇒ different hash.
11. **`hash_sensitive_to_splice_source_offset`** — `Source { source_start_sample }`
    change ⇒ different hash. (Verifies the variant-data fields participate.)
12. **`tag_byte_is_turn_v1`** — first byte of `store_turn`'s output is `0x11`.
13. **`load_turn_kind_mismatch`** — Snapshot-tagged blob ⇒ `KindMismatch`.
14. **`load_turn_kind_mismatch_label`** — Label-tagged blob fed to `load_turn`
    ⇒ `KindMismatch { expected: Turn, found: Label }`. (New: confirms the two
    kinds are distinguished at the dispatch boundary.)
15. **`load_turn_unknown_version`** — version `0xF` ⇒ `UnknownVersion`.
16. **`load_turn_empty_input`** — `&[]` ⇒ `Empty`.
17. **`load_turn_truncated_input`** — just the tag byte ⇒ `Postcard(_)`.
18. **`v1_conversions_total_round_trip`** — `Turn::from(v1::TurnV1::from(&turn)) == turn`.
19. **`v1_wire_format_pinned`** — hand-constructed `v1::TurnV1` encodes to a
    regenerated hardcoded `&[u8]`. (Regeneration uses the existing
    `capture_pinned_values` helper.)
20. **`hash_pinned_for_v1_sample`** — the same hand-constructed `v1::TurnV1`
    hashes to a regenerated hardcoded 16-byte `Hash`.
21. **`tilable_total_duration_turn`** — `Turn::total_duration()` equals
    `turn_duration + post_turn_silence`. (Pins the Tilable impl.)

### `label.rs` — new tests

L1. **`store_load_round_trip`** — a non-trivial `Label` (id, multi-word text,
    `LabelKind::Section`, non-zero `post_label_silence`) round-trips.
L2. **`empty_text_round_trip`** — a `Label` with empty text round-trips.
L3. **`each_label_kind_round_trips`** — two labels, one of each `LabelKind`
    variant, both round-trip preserving variant identity.
L4. **`extreme_value_samples_round_trip`** — `post_label_silence = i64::MAX`
    round-trips losslessly.
L5. **`hash_determinism`** — two encodings of the same `Label` yield identical
    bytes and the same `Hash`.
L6. **`hash_sensitive_to_id`** — `id` change ⇒ different hash. (The Turn-style
    persistent-ID invariant.)
L7. **`hash_sensitive_to_text`** — `text` change ⇒ different hash.
L8. **`hash_sensitive_to_kind`** — `LabelKind::Plain` vs `Section` ⇒ different
    hash.
L9. **`tag_byte_is_label_v1`** — first byte of `store_label`'s output is `0x61`.
L10. **`load_label_kind_mismatch`** — Turn-tagged blob fed to `load_label`
     ⇒ `KindMismatch { expected: Label, found: Turn }`.
L11. **`load_label_unknown_version`** — version `0xF` ⇒ `UnknownVersion`.
L12. **`load_label_empty_input`** — `&[]` ⇒ `Empty`.
L13. **`load_label_truncated_input`** — just the tag byte ⇒ `Postcard(_)`.
L14. **`v1_conversions_total_round_trip`** — `Label::from(v1::LabelV1::from(&l)) == l`.
L15. **`v1_wire_format_pinned`** — hand-constructed `v1::LabelV1` encodes to a
     hardcoded `&[u8]`. Captured via a parallel `capture_pinned_values` helper.
L16. **`v1_hash_pinned`** — the same hand-constructed `v1::LabelV1` hashes to a
     hardcoded `Hash`.
L17. **`tilable_total_duration_label`** — `Label::total_duration()` equals
     `post_label_silence`.

### `hash.rs` — additions

H1. Extend `ALL_KINDS` to include `Kind::Label`.
H2. Extend `tag_layout_pinned` with `assert_eq!(tag_byte(Kind::Label, 1), 0x61);`.
H3. (`tag_round_trip` already covers `Kind::Label` once it's in `ALL_KINDS`.)

## Documentation touches

- **data-model.md is already updated** in the same revision as this action
  plan: § Schema DDL gains `next_label_id`; § Implicit timeline tree introduces
  the `Tilable` trait and `Node<T>`; § Turn payload (speech tracks) carries the
  new `Splice` / `SpliceKind` / `WordType` shape; § Label payload (track 0) is
  new; § Temporal query is generalized with per-kind in-element offset
  interpretation; § Labels (track 0) is rewritten; § Hashing adds
  `Label = 0x6`; § Snapshot / Deltas / Load notes mention per-track-id load
  dispatch; the delta `Location` variant is renamed `After(Hash)`.
- **phase1-m1.md is already updated**: module layout adds `tilable.rs` and
  `label.rs`; Step 4 description is rewritten; Step 6 (tree) is generalized
  over `T: Tilable`; Step 7 (delta) reflects the `Location::After(Hash)`
  rename and the kind-agnostic-delta note; Step 8 (snapshot) reflects per-
  track-id load dispatch.
- The doc-comment on each `mod v1` carries the frozen-shape discipline; no
  separate convention edit needed.

## Out of scope for Step 4

- The blob store (`db/store.rs`) — Step 5. `store_*` returns `(Hash, Vec<u8>)`
  but does not write to SQLite.
- The implicit timeline tree (`tree.rs`) — Step 6. This step ships only the
  `Tilable` trait + the per-kind `total_duration` impls; the generic tree
  itself lands in Step 6.
- Delta / Snapshot / replay shape — Steps 7 / 8. The `Location::After(Hash)`
  rename is documented here only to keep the design coherent; the code lands
  with delta.rs.
- Any V2 schema for either kind — there is no V2 yet; each dispatch table has
  one arm.
- A `compact` command to normalize mixed-version stores — deferred past M1.

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` (must remain green with
  `unwrap_used`, `expect_used`, `panic`, and `missing_docs` all CI-gated).
- `cargo test -p core hash::` — confirms the new `Kind::Label` is wired into
  the existing tag tests.
- `cargo test -p core turn::` — runs the revised Turn tests (21 above).
- `cargo test -p core label::` — runs the new Label tests (17 above).
- `cargo test -p core db::` — confirms the `next_label_id` column addition
  hasn't broken Step 2's migration tests.
- `cargo test -p core` — confirms no regression against other modules.
- Manual diff review of `turn.rs`, `label.rs`, and `tilable.rs` against
  [data-model.md § Turn payload (speech tracks)](../design/data-model.md#turn-payload-speech-tracks),
  [§ Label payload (track 0)](../design/data-model.md#label-payload-track-0), and
  [§ Tilable trait](../design/data-model.md#tilable-trait) for field-for-field
  correspondence.
- One commit on `claude/1M1`, **unsigned** per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md). Bundles the three module files (`turn.rs`,
  `label.rs`, `tilable.rs`), the `hash.rs` Kind addition, the
  `0001_initial.sql` `next_label_id` amendment, the `project/mod.rs` re-exports,
  and all updated tests.

## Downstream implications (flag for later steps)

- **Step 5 (`db/store.rs`):** `store::put(tagged_bytes)` does `INSERT OR
  IGNORE` keyed by `hash_tagged(tagged_bytes)` — unchanged by this step; the
  store is kind-agnostic at the SQL level.
- **Step 6 (`tree.rs`):** `Node<T: Tilable>` carries `(hash, Arc<T>)` — the
  hash is the V_N hash seen on disk (load path) or `store_{turn,label}(&t).0`
  (edit path), never recomputed from the upgraded in-memory element. The tree
  is instantiated as `Tree<Turn>` for speech tracks and `Tree<Label>` for
  track 0.
- **Step 7 (`delta.rs`):** `Location` variants are `Start` and
  `After(Hash)`. The delta itself is kind-agnostic; per-kind dispatch happens
  at load time using `track_id`.
- **Step 8 (`snapshot.rs`):** Snapshot writes collect the existing
  `Node.hash` values; replay calls `load_label` for `track_id == 0` and
  `load_turn` otherwise. The snapshot blob shape is unchanged.
- **Step 9 (`metadata.rs`):** `project.next_label_id` is read/written
  for new-label allocation (the `project` singleton write itself lands with the
  engine in Step 11); ensure the SQL singleton column is in place before that.
