# Phase 1 · M1 · Step 9 — Journal write side + metadata (`db/journal.rs`, `project/command_id.rs`, `project/metadata.rs`) (action plan)

Per-step action plan for Step 9 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md § Schema DDL](../design/data-model.md#schema-ddl-phase-1-user_version--1)
(the `journal` table), [§ Non-timeline data](../design/data-model.md#non-timeline-data)
(the `Metadata` family), [§ Deltas](../design/data-model.md#deltas) (the `command_id`
enum-code contract), and [§ Audio file resolution](../design/data-model.md#audio-file-resolution).
This step adds the **journal append/write path** (the three row types), the
**`CommandId` enum** that gives each journal row its command-type code, and the
**global metadata object** (`type = -1` blob) with its most-recent-wins load and
the source-file resolution that produces the missing-track list.

**Definition of done:**

- `core/src/db/journal.rs` gains the **write side** — a private `append_row` plus
  three typed wrappers (`append_delta_batch` / `append_snapshot` / `append_metadata`)
  returning the new row id — and a third **read** helper `latest_metadata(conn,
  as_of)` (most-recent `type = -1` row, point-boundable like `latest_snapshot`),
  with a `MetaRow` struct parallel to `SnapshotRow`.
- `core/src/project/command_id.rs` is created with the `CommandId` enum
  (**bit-mask category codes**, append-only policy) and `code` / `from_code`
  conversions plus the exported `UNDO_FLAG` bit.
- `core/src/project/metadata.rs` is created with the `Metadata` / `ProjectMeta` /
  `TrackMeta` / `SpeakerMeta` / `ModelUse` / `SourceType` types, the
  `Kind::Metadata` four-part blob treatment (`LATEST_METADATA_VERSION`,
  `store_metadata` / `load_metadata`, frozen `mod v1`), `load_current_metadata(db,
  as_of)` (most-recent-wins, empty default if none), and the pure source-file
  resolution (`FileResolution` + `resolve_track_source` + `missing_tracks`).
- Both new modules re-exported from `project/mod.rs`.
- Full unit coverage: command-code stability (every variant pinned) + round-trip,
  journal append round-trip through the Step 8 read helpers + transaction rollback,
  metadata pinned wire bytes + pinned wire hash (G1 — a new persisted format),
  metadata round-trip / version dispatch / kind-mismatch, most-recent-wins +
  empty-default + as-of-bounded load, rename-reuses-binary-blobs, the
  canonical-order predicate, and relative-hit / absolute-fallback / missing
  resolution against `tempfile` dirs.
- `cargo test -p core`, `cargo clippy -p core -- -D warnings`, and
  `cargo fmt --check` all green.

## Context

This step sits on top of Steps 3–8 and is consumed by the engine (Step 11) and
undo/redo (Step 10):

- [Step 3](phase1-m1-03.md) — `hash.rs`: `Hash`, `Kind` (incl. `Kind::Metadata
  = 0x2`, already in the enum), `encode_tagged`, `decode_tagged_as`, `parse_tag`,
  `DecodeError`. The metadata blob reuses this tagged-bytes plumbing exactly like
  Turn / Label / Snapshot.
- [Step 4](phase1-m1-04.md) — `turn.rs` / `label.rs`: the canonical
  **four-part blob pattern** this step copies for `Metadata` — `LATEST_*_VERSION`
  constant, in-memory type deriving `Serialize`/`Deserialize`, `store_*` /
  `load_*` (kind-checked, version-dispatched), and a frozen `mod v1` with total
  `From` conversions in both directions. Also the **pinned-bytes / pinned-hash +
  `#[ignore] capture_pinned_values`** test trio and its regeneration workflow
  ([phase1-m1-04 § Pinned-bytes regeneration workflow](phase1-m1-04.md#pinned-bytes-regeneration-workflow)).
- [Step 5](phase1-m1-05.md) — `db/store.rs`: `store::put(conn, &hash, &bytes)` and
  `store::get(conn, &hash)`. Metadata blobs (and the binaries they reference) go
  through `put` / `get` like any other blob. The `StoreError` Display/source
  pattern is the house error-type template.
- [Step 7](phase1-m1-07.md) — `delta.rs`: `encode_delta_batch(&[Delta]) ->
  Vec<u8>` (the version-prefixed payload) and `LATEST_DELTA_VERSION`.
  `append_delta_batch` stores the bytes `encode_delta_batch` produced.
- [Step 8](phase1-m1-08.md) — `db/journal.rs`: the **read side** already shipped —
  `SnapshotRow`, `DeltaRow`, `JournalError` (`Sqlite` + `MalformedHashPayload`),
  `latest_snapshot(conn, as_of)`, `deltas_after(conn, snapshot_id, until)`. Step 8
  **explicitly handed the write side + the `type = -1` metadata read + the
  `CommandId` mapping to Step 9** (see
  [phase1-m1-08 § Downstream implications](phase1-m1-08.md#downstream-implications-flag-for-later-steps)).
  Step 8's replay tests append journal rows via raw `INSERT INTO journal` SQL
  precisely because the append helper did not exist yet.

What this step **does not** touch: the engine lifecycle (`new_project` /
`open_project` / `save_snapshot_now`), the **`project` SQLite singleton table**
(sample-rate + id counters — Step 11 writes it; Step 9 only renames it from
`project_meta`, see the distinction decision below), undo/redo journaling
(Step 10), and Tauri wiring (Step 12). Step 9 ships the primitives those steps
compose.

## Decisions locked in this step

### `CommandId` enum: bit-mask category codes, append-only (`project/command_id.rs`)

Every journal row carries a `command_id` — the enum code for the **command
category** that produced it ([data-model.md § Deltas](../design/data-model.md#deltas)):
metadata for inspection and the future **view/restore-historical-state** feature,
**not** a counter and **not** an undo-grouping key. The read side
([Step 8](phase1-m1-08.md)) keeps it a raw `i64` deliberately; this step
introduces the typed enum that the **write** side uses to produce those codes.

Each code is a **bit-mask flag**, so OR-ing the `command_id`s of every row in a
journal range yields *the set of command categories touched in that range* — the
label the history view shows between two project versions. The view wants coarse
hints ("a cut happened here," "levels changed here"), not the exact command, so
many distinct commands collapse onto one category bit. The layout:

- **Bit 0 (`0x1`) is the Undo flag.** An undo of a category-`X` command is stamped
  `X | 0x1` (so `UndoCut == Cut | 0x1 == 0x3`); the OR then shows "an undo
  happened here" while the undone category stays visible. **Redo** re-stamps the
  plain category code `X` — a redo is a normal forward re-application.
- **Bits 1–13 (`0x2`–`0x2000`) are the command categories**, one bit each. The
  mask is 64-bit, so there is headroom for ~63 categories; 13 are allocated now
  and the rest reserved.
- **`0x0` is `Unknown`** — a row tied to no edit category (e.g. a standalone
  snapshot). **M1 stamps `Unknown` on every row it writes** (see below).

Most categories do not ship in Phase 1. They are declared **now** so their
on-disk codes are fixed before any of them writes its first row — that fixing is
the entire point of the scheme, not speculative abstraction. A single defined
value is always one category (optionally with the Undo flag); a *combined* mask
like `0x6` (`Cut | Mute`) only arises from OR-ing multiple rows and is **not** a
variant — `from_code` returns `None` for it.

```rust
//! Bit-mask category codes naming the command category that produced a journal row.

/// Command-category code stamped on every journal row's `command_id` column.
///
/// Each value is a **bit-mask flag**: OR-ing the codes across a journal range
/// gives the set of command categories touched in that range (the future
/// history-view feature). The integer value of each variant is an **on-disk
/// code** — written into `journal.command_id` and read back across sessions — so
/// codes are **permanent and append-only**: never renumber an existing variant,
/// never reuse a retired bit.
///
/// Bit 0 (`0x1`) is the **Undo flag**: an undo of a category-`X` command is
/// stamped `X | 0x1` (e.g. [`CommandId::UndoCut`] `== Cut as i64 | 0x1`). A redo
/// re-stamps the plain category code. `0x0` ([`CommandId::Unknown`]) is a row
/// tied to no edit category, e.g. a standalone snapshot.
#[repr(i64)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommandId {
    /// No edit category — e.g. an automated or on-demand snapshot. (M1 stamps
    /// this on the `new_project` initial snapshot and every `save_snapshot_now`.)
    Unknown = 0x0,
    /// Generic undo (the undo flag with no specific category).
    Undo = 0x1,
    /// Cut: cut words / section / crop, cut disfluencies / sounds / breaths,
    /// declick-by-cut. On a `type = -1` metadata row, indicates remove-track.
    Cut = 0x2,
    /// Undo of [`CommandId::Cut`].
    UndoCut = 0x3,
    /// Mute (replace with room tone): mute words / disfluencies / sounds /
    /// breaths, declick-by-mute.
    Mute = 0x4,
    /// Undo of [`CommandId::Mute`].
    UndoMute = 0x5,
    /// Edit transcript text (and transcript formatting, if implemented).
    EditText = 0x8,
    /// Undo of [`CommandId::EditText`].
    UndoEditText = 0x9,
    /// Edit a label: add / delete / modify / move / change kind; also rename-track.
    EditLabel = 0x10,
    /// Undo of [`CommandId::EditLabel`].
    UndoEditLabel = 0x11,
    /// Edit a speaker: rename, reassign a turn, merge / delete speakers, re-detect.
    EditSpeaker = 0x20,
    /// Undo of [`CommandId::EditSpeaker`].
    UndoEditSpeaker = 0x21,
    /// Insert: add a speech / non-speech track, paste section, reorder turn,
    /// add room tone, concatenate projects.
    Insert = 0x40,
    /// Undo of [`CommandId::Insert`].
    UndoInsert = 0x41,
    /// Record audio (incl. punch-and-roll).
    RecordAudio = 0x80,
    /// Undo of [`CommandId::RecordAudio`].
    UndoRecordAudio = 0x81,
    /// Adjust spacing: word / turn spacing (manual or automated), align tracks,
    /// split / merge turns.
    AdjustSpacing = 0x100,
    /// Undo of [`CommandId::AdjustSpacing`].
    UndoAdjustSpacing = 0x101,
    /// Adjust levels: track / section / turn levels, ramps, smart ducking,
    /// enhance wet/dry mix, channel fade, crosstalk attenuation, peak normalize.
    AdjustLevels = 0x200,
    /// Undo of [`CommandId::AdjustLevels`].
    UndoAdjustLevels = 0x201,
    /// Adjust EQ (manual or automatic).
    AdjustEq = 0x400,
    /// Undo of [`CommandId::AdjustEq`].
    UndoAdjustEq = 0x401,
    /// Correct speech: in-painting, pace adjustment via resynthesis.
    CorrectSpeech = 0x800,
    /// Undo of [`CommandId::CorrectSpeech`].
    UndoCorrectSpeech = 0x801,
    /// Audio effects: reverb / echo and similar.
    AudioEffects = 0x1000,
    /// Undo of [`CommandId::AudioEffects`].
    UndoAudioEffects = 0x1001,
    /// Separate overlapping speech (over-talk) within a track.
    SeparateOvertalk = 0x2000,
    /// Undo of [`CommandId::SeparateOvertalk`].
    UndoSeparateOvertalk = 0x2001,
}

/// The Undo-flag bit ORed into a category code to mark an undo row.
pub const UNDO_FLAG: i64 = 0x1;

impl CommandId {
    /// The stable on-disk code for this command category.
    pub fn code(self) -> i64 { self as i64 }

    /// Map an on-disk code back to a [`CommandId`], or `None` if it is not a
    /// single defined category — an unknown bit (a newer app version), or a
    /// combined mask such as `Cut | Mute` produced by OR-ing several rows.
    pub fn from_code(code: i64) -> Option<Self> {
        use CommandId::*;
        Some(match code {
            0x0 => Unknown,
            0x1 => Undo,
            0x2 => Cut,
            0x3 => UndoCut,
            0x4 => Mute,
            0x5 => UndoMute,
            0x8 => EditText,
            0x9 => UndoEditText,
            0x10 => EditLabel,
            0x11 => UndoEditLabel,
            0x20 => EditSpeaker,
            0x21 => UndoEditSpeaker,
            0x40 => Insert,
            0x41 => UndoInsert,
            0x80 => RecordAudio,
            0x81 => UndoRecordAudio,
            0x100 => AdjustSpacing,
            0x101 => UndoAdjustSpacing,
            0x200 => AdjustLevels,
            0x201 => UndoAdjustLevels,
            0x400 => AdjustEq,
            0x401 => UndoAdjustEq,
            0x800 => CorrectSpeech,
            0x801 => UndoCorrectSpeech,
            0x1000 => AudioEffects,
            0x1001 => UndoAudioEffects,
            0x2000 => SeparateOvertalk,
            0x2001 => UndoSeparateOvertalk,
            _ => return None,
        })
    }
}
```

- **Full category enum now; M1 only stamps `Unknown`.** The only commands that
  write journal rows in the M1 lifecycle — `new_project` (initial snapshot) and
  `save_snapshot_now` — are snapshots not tied to an edit, so both stamp
  `CommandId::Unknown` (`0x0`). The other categories are reserved for M2+
  editing/ML/recording; declaring them now is the on-disk numbering contract the
  bit-mask design depends on (a category's bit must be permanent before its first
  row is ever written), **not** a speculative abstraction.
- **Undo via the `0x1` flag.** Step 10 stamps an undo row with the matching
  `Undo*` variant (`category | 0x1`); redo re-stamps the plain category. The
  explicit `Undo*` variants keep `from_code` total over real single-row codes and
  let the history view name an undo. `UNDO_FLAG` is exported for callers that set
  the flag arithmetically.
- **`from_code` returns `Option`, never panics.** An unknown bit (newer app
  version) or a combined OR-mask is `None` — the history view shows
  "unknown/mixed," replay ignores `command_id` entirely. Forward-compat stays
  clean with no `unwrap`/`panic` (clippy-gated).
- **Visibility:** `CommandId` + `code` + `from_code` + `UNDO_FLAG` are `pub`.
  `code` gets a real caller (`append_*`); `from_code` / `UNDO_FLAG` may stay dead
  until Step 10 / the M5+ history view — gate just those with `#[allow(dead_code)]`
  if clippy flags them.
- A pinned test (C1) locks **every** variant's on-disk code, so a future reorder
  or accidental renumber can't silently shift codes (mirrors `tag_layout_pinned`
  in `hash.rs`).

### Journal write side: one private `append_row` + three typed wrappers (`db/journal.rs`)

The journal append path produces all three row types from
[data-model.md § Write path](../design/data-model.md#write-path) and § Non-timeline data.
All three share one INSERT; the typed wrappers differ only in the `type` code and
how the payload is formed:

```rust
/// Append one journal row, returning its assigned `id` (`AUTOINCREMENT`).
/// Private: all callers go through the typed wrappers so the `type` code and
/// payload shape are never mismatched at a call site.
fn append_row(
    conn: &Connection,
    row_type: i64,        // -1 | 0 | 1
    command_id: CommandId,
    payload: &[u8],
    applied_at: i64,      // POSIX seconds UTC, supplied by the caller's clock
) -> Result<i64, JournalError>;

/// Append a `type = 0` delta-batch row. `payload` is the version-prefixed
/// `Vec<Delta>` bytes from `delta::encode_delta_batch`.
pub(crate) fn append_delta_batch(
    conn: &Connection, command_id: CommandId, payload: &[u8], applied_at: i64,
) -> Result<i64, JournalError>;

/// Append a `type = 1` snapshot row pointing at the snapshot blob `hash`.
/// The payload is the bare 16-byte hash (no extra version byte — the store
/// blob's own format tag carries the version, per data-model.md § Deltas).
pub(crate) fn append_snapshot(
    conn: &Connection, command_id: CommandId, hash: &Hash, applied_at: i64,
) -> Result<i64, JournalError>;

/// Append a `type = -1` metadata row pointing at the metadata blob `hash`.
/// Payload is the bare 16-byte hash, same as `append_snapshot`.
pub(crate) fn append_metadata(
    conn: &Connection, command_id: CommandId, hash: &Hash, applied_at: i64,
) -> Result<i64, JournalError>;
```

Implementation: `append_row` runs
`INSERT INTO journal (type, payload, command_id, applied_at) VALUES (?1, ?2, ?3, ?4)`
with `command_id.code()` bound, then returns `conn.last_insert_rowid()`. The
snapshot/metadata wrappers pass `&hash.0[..]` as the payload; the delta wrapper
passes the bytes through.

Locked design points:

- **`applied_at` is a caller-supplied parameter, not computed here.** Keeps
  `journal.rs` clock-free (deterministic, trivially unit-testable — tests pass
  fixed constants exactly as the Step 8 read tests already do) and puts the
  wall-clock read where it belongs: the engine (Step 11) computes "now" once per
  edit. The read-side `SnapshotRow` / `DeltaRow` already round-trip `applied_at`,
  so the write tests assert it back through them.
- **`conn: &Connection` works inside a transaction.** `rusqlite::Transaction`
  derefs to `&Connection`, so the engine's single-transaction write path
  (`store::put` + `journal::append_*`, per
  [data-model.md § Write path](../design/data-model.md#write-path)) passes `tx` to both —
  exactly as `store::put` is already called with `tx` in `store.rs` tests.
- **Typed `command_id: CommandId`** (not raw `i64`) at the wrapper boundary —
  the only legal source of a code is the enum, converted internally via
  `.code()`. This gives `CommandId::code` a real non-test caller in this step.
- **`pub(crate)` + `#[allow(dead_code)]`** on the three wrappers: their first
  non-test caller is the Step 11 engine (transitively) / Step 10 undo. Same
  attribute pattern as the Step 8 read helpers. `append_row` is a private `fn`;
  it still needs `#[allow(dead_code)]` until a non-test caller exists.

### Third read helper: `latest_metadata` (`db/journal.rs`)

Metadata load is most-recent-wins with **no replay**
([data-model.md § Non-timeline data](../design/data-model.md#non-timeline-data)): the
current metadata is the blob pointed to by the highest-id `type = -1` row. That is
a third journal read query alongside `latest_snapshot` / `deltas_after` (flagged
in [phase1-m1-08 § Downstream implications](phase1-m1-08.md#downstream-implications-flag-for-later-steps)):

```rust
/// A `type = -1` metadata journal row. Same shape as [`SnapshotRow`] (both are
/// 16-byte hash-pointer rows) but kept distinct: it points at a metadata blob,
/// and the history view may treat the two row types differently.
pub(crate) struct MetaRow {
    pub id: i64,
    pub hash: Hash,        // the metadata blob this row points to
    pub command_id: i64,   // raw code; map via CommandId::from_code at a higher layer
    pub applied_at: i64,
}

/// Most recent `type = -1` row with `id <= as_of` (highest such id), or the
/// absolute latest when `as_of` is `None`. `None` result ⇒ no metadata row in range.
pub(crate) fn latest_metadata(conn: &Connection, as_of: Option<i64>)
    -> Result<Option<MetaRow>, JournalError>;
```

- Query: `SELECT id, payload, command_id, applied_at FROM journal WHERE type = -1
  AND (?1 IS NULL OR id <= ?1) ORDER BY id DESC LIMIT 1` (served by
  `journal_type_idx`). Decode the 16-byte payload to a `Hash` exactly like
  `latest_snapshot`, erroring `MalformedHashPayload` on a non-16-byte BLOB.
- **Takes `as_of: Option<i64>`, mirroring `latest_snapshot`.** Metadata is
  most-recent-wins, so the highest-id row *in range* is the answer; bounding by
  `as_of` makes it "the metadata in effect as of journal position N." **M1 always
  passes `None`** (the absolute latest); the bounded form costs nothing extra and
  lets the future history view (M5+) read metadata-at-a-point with no new API. The
  query parameter and binding are identical in shape to `latest_snapshot`'s, so
  the two stay symmetric.
- `pub(crate)` + `#[allow(dead_code)]` (first non-test caller is
  `metadata::load_current_metadata`, itself test-only until Step 11).
- **Generalize `JournalError::MalformedHashPayload`'s `Display`.** It currently
  reads `"snapshot row {id} has a {len}-byte payload (expected 16)"`; `type = -1`
  rows hit the same path, so change "snapshot row" → "journal row" (one-line edit;
  the existing J3 test only asserts the message contains the id and `15`, so it
  stays green).

### `Metadata` mirrors the Turn / Label tagged-blob pattern (`project/metadata.rs`)

`Metadata` is a store-resident, content-addressed blob (`Kind::Metadata = 0x2`,
already in the `Kind` enum). It gets the **same four-part treatment** as Turn /
Label / Snapshot — and is a **new persisted format**, so per the data-integrity
invariant ([conventions.md](../design/conventions.md) G1 / [CLAUDE.md](../CLAUDE.md)) it
ships pinned-bytes + pinned-hash tests in this step.

```rust
pub const LATEST_METADATA_VERSION: u8 = 1;

/// The global non-timeline metadata object: one blob, most-recent-wins.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Metadata {
    /// Project-scoped mutable state. (NB: distinct from the `project`
    /// SQLite singleton table — see the persistence note below.)
    pub project: ProjectMeta,
    /// Track metadata, **canonical order: ascending by `id`**. Track 0 (labels)
    /// is implicit, not listed.
    pub tracks: Vec<TrackMeta>,
    /// Speaker metadata, **canonical order: ascending by `id`**.
    pub speakers: Vec<SpeakerMeta>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ProjectMeta {
    /// Optional project name.
    pub name: Option<String>,
    /// Sets of `track_id`s aligned together, e.g. `[[1,2,4],[5,6]]`.
    /// **Canonical order:** each inner group ascending, groups ordered by first id.
    pub aligned_groups: Vec<Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackMeta {
    pub id: u32,
    pub name: String,
    pub source_type: SourceType,
    pub source_path_relative: String,
    pub source_path_absolute: String,
    pub resampled_path: Option<String>,
    pub codec: String,
    pub source_sample_rate: u32,
    pub source_channels: u16,
    pub project_start_sample: i64,
    pub original_length_samples: i64,
    pub cut_length_samples: i64,
    pub drift_ppm: f64,
    pub room_tone_hash: Option<Hash>,
    pub room_tone_length_samples: Option<i64>,
    pub models_used: ModelUse,
    pub enhanced_path: Option<String>,
    pub wet_dry_ratio: f32,
    pub disfluencies_identified: bool,
    pub created_at: String,   // ISO 8601, mirrors the project table's TEXT columns
    pub updated_at: String,   // ISO 8601
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceType {
    File,        // 'file'
    Recording,   // 'recording' (Phase 3)
}

/// The model applied to a track, one identifier per role. The role set is fixed
/// (it mirrors the settings `model_paths` roles) and each role's model is applied
/// to a track at most once, so this is a flat struct of optional model
/// identifiers — not a list, and not timestamped. `None` = that role's model was
/// never run on this track.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelUse {
    pub transcription: Option<String>,        // WhisperX
    pub vad: Option<String>,                  // reserved; unused in Phase 1
    pub forced_alignment: Option<String>,     // WhisperX alignment model
    pub enhancement: Option<String>,          // MP-SENet
    pub sound_classification: Option<String>, // YAMnet
    pub llm: Option<String>,                  // Gemma
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerMeta {
    pub id: u32,
    pub name: String,
    pub color_hint: Option<String>,
    pub embedding_hash: Option<Hash>,   // → store blob (normalized mean embedding)
    pub track_ids: Vec<u32>,            // canonical order: ascending
}

pub fn store_metadata(meta: &Metadata) -> Result<(Hash, Vec<u8>), postcard::Error>;
pub fn load_metadata(bytes: &[u8]) -> Result<Metadata, DecodeError>;

pub mod v1 { /* MetadataV1, ProjectMetaV1, TrackMetaV1, SourceTypeV1,
                ModelUseV1, SpeakerMetaV1 — field-identical, total From both ways */ }
```

- `store_metadata` = `encode_tagged(Kind::Metadata, LATEST_METADATA_VERSION,
  &v1::MetadataV1::from(meta))` (preceded by the canonical-order tripwire below);
  `load_metadata` is a byte-for-byte copy of `load_turn` with `Kind::Metadata`
  substituted (kind-check + version dispatch, `UnknownVersion` for anything but
  `1`). **Copy the `mod v1` recipe exactly** from `turn.rs`: field-identical mirror
  structs/enums and total `From<&Metadata> for MetadataV1` / `From<MetadataV1> for
  Metadata`, recursing into every nested type (`ModelUseV1` mirrors the six role
  fields one-for-one).
- **`models_used` is a single `ModelUse`, not a `Vec`.** The model role set is
  fixed and each role's model is applied to a track at most once, so a flat struct
  of `Option<String>` role fields (mirroring the settings `model_paths` roles) is
  the right shape — no role/name/hash/timestamp records, no list. A fresh track's
  `models_used` is `ModelUse::default()` (all `None`). This drops the old
  `Option<Hash>` arm from `ModelUse`; the pinned tests still exercise `Option<Hash>`
  via `TrackMeta::room_tone_hash` and `SpeakerMeta::embedding_hash`.
- **Canonical order is the producer's invariant; Step 9 adds a debug tripwire.**
  Content-addressing requires that equal logical metadata serialize to *identical*
  bytes — otherwise two equal states hash differently, breaking dedup and
  cross-session round-trip equality. So the ordered fields must be in one canonical
  order: `tracks` / `speakers` ascending by `id`; each `SpeakerMeta::track_ids`
  ascending; each `ProjectMeta::aligned_groups` inner group ascending and the
  groups ordered by first id. **Maintaining that order is the *producing
  command's* job, not the blob layer's** — the engine when it builds `Metadata`,
  and specifically the future `align_tracks` (for `aligned_groups`) and
  speaker-assignment commands (for `track_ids`). Step 9 deliberately does **not**
  silently re-sort (that would mask producer bugs); instead `store_metadata` opens
  with `debug_assert!(metadata_is_canonical(meta), …)`, a private predicate, so a
  producer that emits out-of-order collections fails loudly in debug/test builds
  rather than corrupting content-addressing in release. The field doc-comments
  state the required order. (`metadata_is_canonical` is `#[cfg(debug_assertions)]`
  or `#[allow(dead_code)]` as clippy requires; it has a direct unit test, M10.)
- **Ordered collections only — never `HashMap`.** `Vec`s throughout, so there is no
  unordered container to serialize non-deterministically in the first place; the
  canonical-order tripwire handles the *within-`Vec`* ordering on top of that.
- **Type choices pinned by the wire tests.** Timestamps are `String` (ISO 8601) to
  mirror the `project` table's `created_at` / `updated_at` TEXT columns; numeric
  widths are as written above. These are pre-1.0 revisable (the `mod v1`
  doc-comment's Pre-1.0 clause) but locked by M6/M7 pinned tests once chosen, so a
  later edit can't silently shift the on-disk layout.
- `#[derive(Default)]` on `Metadata` / `ProjectMeta` gives the empty-default that
  `load_current_metadata` returns when no `type = -1` row exists. `TrackMeta` /
  `SpeakerMeta` need no `Default` (they're always fully constructed).

### `load_current_metadata`: most-recent-wins, empty default, as-of-boundable (`project/metadata.rs`)

```rust
/// Load the current global metadata: the blob pointed to by the highest-id
/// `type = -1` journal row with `id <= as_of` (or the absolute latest when
/// `as_of` is `None`), or `Metadata::default()` if no such row exists (a freshly
/// created project, or an `as_of` before the first metadata write). No replay —
/// each `type = -1` row is a complete object (data-model.md § Non-timeline data).
pub(crate) fn load_current_metadata(db: &Db, as_of: Option<i64>)
    -> Result<Metadata, MetadataLoadError>;
```

- Body: `journal::latest_metadata(db.conn(), as_of)?` → `None` ⇒
  `Ok(Metadata::default())`; `Some(row)` ⇒ `store::get(db.conn(), &row.hash)?` →
  `load_metadata(&bytes)?`. **M1 callers pass `None`** (`open_project` wants the
  current metadata); the `as_of` threads straight through to `latest_metadata` so
  the future history view reads metadata-at-a-point with no new function.
- **Why both `load_metadata` and `load_current_metadata` exist (and why no third
  "as-of" variant).** They sit at different layers: `load_metadata(bytes)` is the
  **pure blob decoder** — kind-check + version dispatch on raw bytes, no DB — the
  exact parallel of `load_turn` / `load_label`, reused by the pinned/round-trip
  tests and the Step 13 fixture. `load_current_metadata(db, as_of)` is the
  **DB-level read** that composes `latest_metadata` (row lookup) + `store::get`
  (fetch) + `load_metadata` (decode). The two are not redundant. And we do **not**
  add a separate point-in-history function: with `as_of = None` meaning "latest,"
  the one DB reader covers both current and historical reads. "current" in the name
  denotes the **most-recent-wins resolution** (no replay), which `as_of` merely
  bounds — not "now" specifically.
- A small typed `MetadataLoadError { Journal(JournalError), Store(StoreError),
  Decode(DecodeError) }` with `Display` + `source()` + `From` impls (the house
  error pattern from `store.rs` / the Step 8 `ReplayError`). `pub(crate)`.
- `pub(crate)` + `#[allow(dead_code)]` (first non-test caller is the Step 11
  engine's `open_project`).

### Source-file resolution is **pure** (no DB writes); M1 returns the missing list

Per [data-model.md § Audio file resolution](../design/data-model.md#audio-file-resolution),
each `source_type = 'file'` track resolves: (1) relative-to-project hit → use it;
(2) else absolute-path-on-disk hit → use it *and* update the stored relative path;
(3) else missing. The **Missing-Files dialog is M6**; M1 only produces the list
([phase1-m1.md Step 9](phase1-m1.md#step-9--journal-ops--metadata-dbjournalrs-projectmetadatars)).

```rust
/// Outcome of resolving one track's source file against the project directory.
pub(crate) enum FileResolution {
    /// Relative path resolved on disk — use as-is.
    Found(PathBuf),
    /// Relative path missing but the stored absolute path exists. Use it; the
    /// stored relative path SHOULD be rewritten (a metadata change). M1 does not
    /// persist the rewrite — it surfaces `new_relative` for the engine to act on
    /// later (deferred to Step 11 / M6).
    FoundViaAbsolute { path: PathBuf, new_relative: String },
    /// Neither path resolved — the track has a missing source file.
    Missing,
    /// Not a file-backed track (e.g. `Recording`); nothing to resolve.
    NotApplicable,
}

/// Resolve one track's source. Pure: reads the filesystem, writes nothing.
pub(crate) fn resolve_track_source(project_dir: &Path, track: &TrackMeta)
    -> FileResolution;

/// Ids of all `source_type = File` tracks that resolve to `Missing`.
pub(crate) fn missing_tracks(project_dir: &Path, meta: &Metadata) -> Vec<u32>;
```

- **`resolve_track_source` performs no DB I/O** — it only touches the filesystem
  (existence checks) — so it is exhaustively unit-testable with `tempfile` dirs
  and has no engine/transaction entanglement. The `FoundViaAbsolute` →
  persist-the-rewritten-relative-path step (a `type = -1` write) is **deferred**:
  M1's `open_project` consumes only `Missing` (for the list); persisting the
  relative-path rewrite lands with the engine's file-handling maturation
  (flagged for Step 11 / M6 — see Documentation touches).
- `Recording` tracks → `NotApplicable`; `missing_tracks` skips them.
- Path handling: stored paths use `/` separators ([data-model.md § Derived
  files](../design/data-model.md#derived-files)); build the candidate via
  `project_dir.join(source_path_relative)` (Rust's `Path::join` accepts `/` on all
  platforms for relative components). Use `Path::exists()` for the on-disk check.
- `pub(crate)` + `#[allow(dead_code)]` until Step 11.

### `Metadata` blob's `ProjectMeta` struct vs the `project` SQLite table — keep them distinct

A known confusion point flagged in
[data-model.md § Non-timeline data](../design/data-model.md#non-timeline-data): the
`ProjectMeta` **struct** (inside the `Metadata` blob — `name`, `aligned_groups`)
is **not** the `project` **SQLite singleton table** (`sample_rate`, `next_*_id`
counters, `created_at`/`updated_at`). **The SQLite table is now named `project`
(renamed from `project_meta` this step)** precisely to keep the parallel
`ProjectMeta` / `TrackMeta` / `SpeakerMeta` blob-struct naming unambiguous against
the singleton table. The rename is a one-line edit to the **initial** migration
`0001_initial.sql` (and the `min_app_version` query in `migrations.rs`) — it is
**not** a new migration: M1 is unreleased, `user_version` stays `1`, and no
prior-format fixture carries the old name yet (the first committed `.vocalboard`
fixture is Step 13), so this is baseline editing, not a schema migration. This
step touches **only the metadata blob**; the `project` table itself is written by
`new_project` and its id counters mutate as turns/tracks/speakers are created —
that is **Step 11**. Step 9 adds no `project` table I/O. The `Metadata::project`
doc-comment states the distinction so an implementer doesn't conflate them.

### Visibility + dead-code: mirror the established patterns

- `CommandId` (+ `code` / `from_code` / `UNDO_FLAG`): `pub`. `code` gets a real
  caller (`append_*`); `from_code` / `UNDO_FLAG` may stay dead until Step 10 / M5+
  — gate just those with `#[allow(dead_code)]` if clippy flags them.
- `append_delta_batch` / `append_snapshot` / `append_metadata`, `append_row`,
  `latest_metadata`, `MetaRow`: `pub(crate)` (or private `fn` for `append_row`) +
  `#[allow(dead_code)]` — first non-test caller is Step 10/11.
- `Metadata` + family, `SourceType`, `ModelUse`, `store_metadata`,
  `load_metadata`, `LATEST_METADATA_VERSION`, `mod v1`: `pub` (lib-crate public
  surface, mirroring `Turn` / `store_turn`); they carry doc-comments under the
  `missing_docs` gate but need no `#[allow(dead_code)]`.
- `load_current_metadata`, `MetadataLoadError`, `FileResolution`,
  `resolve_track_source`, `missing_tracks`: `pub(crate)` + `#[allow(dead_code)]`.
- **Do not** remove the existing `#[allow(dead_code)]` on `db::store::put` /
  `db::store::get` / `Db::conn` or the Step 8 plumbing — Step 9's additions are
  themselves `pub(crate)`/test-reachable only, so those items stay dead to clippy
  until the **Step 11** engine wires the first genuine non-test caller (the Step 8
  plan already moved this cleanup from Step 9 to Step 11).

## Module surface

### Revised: `core/src/db/journal.rs`

Add (below the existing read helpers): `MetaRow`, `latest_metadata(conn, as_of)`,
`append_row` (private), `append_delta_batch`, `append_snapshot`, `append_metadata`.
Add `use crate::project::command_id::CommandId;`. Generalize the
`MalformedHashPayload` Display string from "snapshot row" to "journal row".

### New: `core/src/project/command_id.rs`

`CommandId` enum + `code` / `from_code` as specified above. Module doc-comment
explaining the on-disk, append-only contract.

### New: `core/src/project/metadata.rs`

```rust
//! Global non-timeline metadata: project/track/speaker state in a single
//! content-addressed `Kind::Metadata` blob, recorded by `type = -1` journal
//! rows (most-recent-wins, no replay). Also the pure source-file resolution
//! that produces the missing-track list on open.
//!
//! See [data-model.md § Non-timeline data](../../../design/data-model.md#non-timeline-data)
//! and [§ Audio file resolution](../../../design/data-model.md#audio-file-resolution).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::journal;
use crate::db::store::{self, StoreError};
use crate::db::Db;
use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};

pub const LATEST_METADATA_VERSION: u8 = 1;
// Metadata, ProjectMeta, TrackMeta, SourceType, ModelUse (flat role struct), SpeakerMeta …
// store_metadata (with canonical-order debug_assert) / load_metadata
// metadata_is_canonical (private predicate behind the debug_assert)
// pub mod v1 { … }
// MetadataLoadError; load_current_metadata(db, as_of)
// FileResolution; resolve_track_source; missing_tracks
```

### Revised: `core/src/project/mod.rs`

Add `pub mod command_id;` and `pub mod metadata;` (siblings to the existing
`pub mod delta;`, `pub mod snapshot;`, …).

### Revised: `core/migrations/0001_initial.sql` + `core/src/db/migrations.rs`

Rename the singleton table `project_meta` → `project` (CREATE TABLE in the initial
migration; the `min_app_version` SELECT in `migrations.rs`). Baseline edit, not a
new migration — see the distinctness decision above. No other DDL changes.

### Reuse from existing code (no new dependencies; only the `project` rename touches schema)

- `hash.rs`: `Hash`, `Kind::Metadata`, `encode_tagged`, `decode_tagged_as`,
  `parse_tag`, `DecodeError`.
- `db/store.rs`: `store::get` / `store::put` and `StoreError`.
- `db/journal.rs`: the Step 8 read structs/helpers (the write tests assert through
  `latest_snapshot` / `deltas_after`); `JournalError`.
- `delta.rs`: `encode_delta_batch` for the `append_delta_batch` test payloads.
- `db/mod.rs`: `Db`, `Db::conn()`, `Db::with_transaction`.
- `serde` + `postcard`, `tempfile` (dev) — already present.

## Test plan

Inline `#[cfg(test)]` per module. Journal write tests live in `db/journal.rs`
(extending its existing `mod tests`, reusing its `open_tmp_db` /
`insert_journal_row` helpers). `CommandId` tests in `command_id.rs`. Metadata blob
+ load + resolution tests in `metadata.rs`.

### `CommandId` (`command_id.rs`)

- **C1 `codes_are_pinned`** — assert the on-disk value of **every** variant
  (`Unknown == 0x0`, `Undo == 0x1`, `Cut == 0x2`, `UndoCut == 0x3`, … through
  `SeparateOvertalk == 0x2000`, `UndoSeparateOvertalk == 0x2001`) and
  `UNDO_FLAG == 0x1`. Locks the on-disk codes against any reorder/renumber
  (mirrors `hash::tag_layout_pinned`).
- **C2 `code_round_trips`** — for every variant,
  `CommandId::from_code(c.code()) == Some(c)`.
- **C3 `undo_flag_relationship`** — for each base category variant `X` and its
  `UndoX`, assert `UndoX.code() == X.code() | UNDO_FLAG` (e.g. `UndoCut == Cut |
  0x1`). Pins the bit-mask undo convention Step 10 relies on.
- **C4 `from_code_unknown_or_combined_is_none`** — `from_code` is `None` for an
  unallocated bit (`0x4000`), a combined OR-mask that is not a single variant
  (`0x6 == Cut | Mute`), and `-1` (forward-compat; no panic). `from_code(0x0)` is
  `Some(Unknown)` (0x0 *is* a defined variant).

### Journal write side (`db/journal.rs`)

- **W1 `append_snapshot_round_trips_via_latest_snapshot`** —
  `append_snapshot(conn, CommandId::Unknown, &h, 1000)` (the real M1 snapshot code)
  returns a positive id; `latest_snapshot(conn, None)` returns a `SnapshotRow`
  whose `id` matches, `hash == h`, `command_id == 0`, `applied_at == 1000`. Closes
  the write↔read loop.
- **W2 `append_delta_batch_round_trips_via_deltas_after`** — append a snapshot
  (id S), then `append_delta_batch` with `delta::encode_delta_batch(&batch)` bytes
  and a **non-zero** category code (`CommandId::Cut`) + `applied_at`;
  `deltas_after(conn, S, None)` returns one `DeltaRow` whose `payload` decodes
  (`decode_delta_batch`) back to `batch` and whose `command_id == CommandId::Cut
  .code()` / `applied_at` match. (Using a non-zero code here, alongside W1's `0`,
  confirms `code()` round-trips through the column for both.)
- **W3 `append_metadata_round_trips_via_latest_metadata`** — `append_metadata`
  then `latest_metadata(conn, None)` returns the matching `MetaRow` (id/hash/
  command_id/applied_at).
- **W4 `append_returns_increasing_ids`** — three successive appends return
  strictly increasing ids (AUTOINCREMENT).
- **W5 `latest_metadata_none_on_empty_journal`** — fresh DB ⇒
  `latest_metadata(conn, None) == Ok(None)`.
- **W6 `latest_metadata_returns_highest_id`** — two `type = -1` rows (+ an
  intervening `type = 0` row) ⇒ `latest_metadata(conn, None)` returns the
  highest-id `type = -1` row, ignoring the delta row.
- **W6b `latest_metadata_as_of_bounds`** — two `type = -1` rows m1 < m2 ⇒
  `latest_metadata(conn, Some(m2 - 1)) == m1`, `Some(m2) == m2`,
  `Some(m1 - 1) == None`, `None == m2` (absolute latest). Mirrors J4 for
  snapshots; confirms the shared `as_of` predicate.
- **W7 `latest_metadata_rejects_non_16_byte_payload`** — a `type = -1` row with a
  15-byte payload ⇒ `Err(JournalError::MalformedHashPayload { id, len: 15 })`.
- **W8 `append_rolls_back_on_transaction_error`** — inside
  `Db::with_transaction`, `append_snapshot` then `bail!`; after the failed
  transaction, `latest_snapshot(conn, None)` is `None` (the row rolled back).
  Mirrors `store.rs`'s `put_rolls_back_on_transaction_error`.
- **W9 `malformed_hash_payload_display_is_generic`** — the regenerated Display
  string contains the row id and "16" but no longer hardcodes "snapshot" (guards
  the message edit so it reads correctly for `type = -1` rows too).

### Metadata blob (`metadata.rs`)

Provide a `fn sample_metadata() -> Metadata` test builder with deterministic,
non-trivial, **canonical-order** values (a named project, one aligned group, one
`File` `TrackMeta` whose `models_used` is a `ModelUse` with `Some` in two role
fields — e.g. `transcription` + `enhancement` — and `Some(room_tone_hash)`, one
`SpeakerMeta` with `Some(embedding_hash)` + two ascending `track_ids`) so the
pinned tests exercise the `Option<Hash>`, `Option<String>`-role, and nested-vec
arms. It must be canonical so it does not trip `store_metadata`'s debug_assert.

- **M1 `metadata_round_trips`** — `load_metadata(store_metadata(&sample).1) ==
  sample` (`PartialEq`).
- **M2 `metadata_default_round_trips`** — `Metadata::default()` round-trips
  (empty project, no tracks/speakers).
- **M3 `load_metadata_rejects_wrong_kind`** — feed a `store_turn` blob ⇒
  `Err(DecodeError::KindMismatch { expected: Metadata, .. })`.
- **M4 `load_metadata_rejects_empty`** — `load_metadata(&[])` ⇒
  `Err(DecodeError::Empty)`.
- **M5 `load_metadata_rejects_unknown_version`** — a blob tagged
  `(Kind::Metadata, 2)` ⇒ `Err(DecodeError::UnknownVersion { .. })`.
- **M6 `v1_wire_format_pinned`** — `store_metadata(&sample_metadata()).1` equals a
  hardcoded `&[u8]` (catches postcard / field-order changes round-trip can't see).
  **Mirrors the Turn / Label / Snapshot pinned tests.**
- **M7 `v1_wire_hash_pinned`** — `store_metadata(&sample_metadata()).0` equals a
  hardcoded `Hash`.
- **M8 `capture_pinned_values`** (`#[ignore]`) — prints freshly captured
  `PINNED_WIRE_BYTES` / `PINNED_HASH`, per the
  [phase1-m1-04 § Pinned-bytes regeneration workflow](phase1-m1-04.md#pinned-bytes-regeneration-workflow).
- **M9 `v1_conversions_total_round_trip`** —
  `Metadata::from(v1::MetadataV1::from(&s)) == s`.
- **M10 `metadata_is_canonical_predicate`** — the private `metadata_is_canonical`
  returns `true` for `sample_metadata()` and `Metadata::default()`, and `false`
  for each single out-of-order axis: `tracks` out of `id` order, `speakers` out of
  `id` order, a `SpeakerMeta::track_ids` descending, an `aligned_groups` inner
  group descending, and `aligned_groups` outer order swapped. Backs the
  `store_metadata` debug tripwire (the producer-owned determinism invariant).

### Metadata read — most-recent-wins + binary reuse (`metadata.rs`)

These need a real `Db` (build via `tempfile`, like the journal tests).

- **MR1 `load_current_metadata_empty_default`** — fresh `Db`, no `type = -1` row
  ⇒ `load_current_metadata(&db, None) == Metadata::default()`.
- **MR2 `load_current_metadata_most_recent_wins`** — store + `append_metadata`
  metadata A, then store + `append_metadata` a renamed metadata B (B is A with
  `tracks[0].name` changed) ⇒ `load_current_metadata(&db, None)` returns **B**.
- **MR2b `load_current_metadata_as_of_returns_earlier`** — same A-then-B setup;
  `load_current_metadata(&db, Some(id_a))` returns **A** (the metadata in effect at
  A's row), and `Some(id_a - 1)` returns `Metadata::default()`. Confirms `as_of`
  threads through to the row lookup.
- **MR3 `rename_reuses_binary_blobs`** — A's `TrackMeta` has `room_tone_hash =
  Some(h_rt)` where `h_rt` points at a binary blob `put` once. B (renamed) keeps
  the **same** `room_tone_hash`. After storing both metadata blobs: assert `store`
  holds the single `h_rt` row exactly once (`SELECT COUNT(*) … WHERE hash = h_rt`
  == 1) and two distinct metadata blobs (A's hash ≠ B's hash). Pins "a rename
  re-serializes only `Metadata`; referenced binaries are reused by hash"
  ([data-model.md § Non-timeline data](../design/data-model.md#non-timeline-data)).
- **MR4 `load_current_metadata_surfaces_store_error`** — `append_metadata` a hash
  whose blob was never `put` ⇒ `load_current_metadata(&db, None)` returns
  `Err(MetadataLoadError::Store(StoreError::NotFound(_)))`.
- **MR5 `metadata_load_error_display_and_source`** — each `MetadataLoadError`
  variant's `Display` is non-empty and the wrapper variants chain `source()`
  (mirrors `store_error_*`).

### Source-file resolution (`metadata.rs`)

Use `tempfile::tempdir()` for the project dir; create/skip files with
`std::fs::write` to drive each branch.

- **RS1 `resolve_relative_hit`** — write `<dir>/audio/a.wav`; a `File` track with
  `source_path_relative = "audio/a.wav"` ⇒ `FileResolution::Found(p)` with `p` the
  resolved path.
- **RS2 `resolve_absolute_fallback`** — relative path **absent**, but
  `source_path_absolute` points at an existing file (e.g. a second tempdir) ⇒
  `FileResolution::FoundViaAbsolute { path, new_relative }` (assert `path` exists;
  `new_relative` is the path the engine would store — document whether it's the
  recomputed relative or the absolute; for M1, return the **absolute path string**
  as `new_relative`, since recomputing a relative path is the engine's M6 job).
- **RS3 `resolve_missing`** — neither path exists ⇒ `FileResolution::Missing`.
- **RS4 `resolve_recording_not_applicable`** — a `Recording` track ⇒
  `FileResolution::NotApplicable` regardless of paths.
- **RS5 `missing_tracks_collects_only_missing_file_tracks`** — a `Metadata` with
  three tracks (one relative-hit, one missing, one `Recording`) ⇒ `missing_tracks`
  returns exactly the missing one's `id`.

### Out-of-scope tests (covered elsewhere or later)

- Journal **read** helpers (`latest_snapshot` / `deltas_after`): [Step 8](phase1-m1-08.md).
- Engine `open_project` consuming the missing list + the recover-vs-refuse
  decision; persisting the `FoundViaAbsolute` relative-path rewrite: Step 11 / M6.
- Undo/redo appending inverse rows tagged with a `command_id`: Step 10.
- The `project` singleton table (sample-rate + id counters): Step 11.
- Tauri command wiring: Step 12. The committed G1 `.vocalboard` fixture: Step 13
  (which round-trips a real metadata blob through `load_metadata` by construction).

## Documentation touches

- **`data-model.md`** — updated in this plan's prep (already applied alongside
  this doc): (1) § Schema DDL — `CREATE TABLE project_meta` → `CREATE TABLE
  project`; (2) the `Turn` / `Label` ID-counter comments and the § Non-timeline
  data `ProjectMeta` note — `project_meta.*` → `project.*`; (3) § Non-timeline
  data `TrackMeta` — `models_used: Vec<ModelUse>` → `models_used: ModelUse` plus
  the flat `ModelUse` role struct (was "role/name/hash/used_at"); (4)
  canonical-order annotations on `aligned_groups` / `track_ids`. § Deltas (the
  `command_id` enum-code contract) and § Audio file resolution already match
  field-for-field. The bit-mask category meaning of `command_id` is documented in
  this plan and `command_id.rs`; § Deltas's one-line "enum CODE … NOT a counter"
  note still holds and needs no edit.
- **Migration code** — `0001_initial.sql` (`CREATE TABLE project`) and
  `migrations.rs` (`SELECT … FROM project`) renamed to match; baseline edit, not a
  new migration (M1 unreleased, `user_version` stays `1`, no old-name fixture
  exists yet). The existing migration tests (`fresh_db_reaches_max_version`,
  `reopen_is_noop`, `future_version_is_refused`) stay green — none asserts the
  table name. Other docs carrying the old name (`index.md`, `architecture.md`,
  `phase1-m1.md`, `phase1-m1-04.md`) renamed for consistency.
- **`phase1-m1.md` Step 9 bullet** — add the cross-reference line matching the
  Step 3/4/5/7/8 pattern:
  > See [phase1-m1-09.md](phase1-m1-09.md) for the detailed action plan.
  Add that Step 9 also creates `project/command_id.rs` (the `CommandId` enum
  feeding journal `command_id` codes) and that source-file resolution is **pure**
  in M1 (returns the missing list; persisting the absolute-fallback relative-path
  rewrite is deferred to Step 11 / M6).
- **`phase1-m1.md` module-layout comment** — annotate the `journal.rs` line that
  the append/write side + `latest_metadata` land in Step 9, and confirm
  `command_id.rs` / `metadata.rs` are created here.
- **`conventions.md`** — no changes. The G1 invariant is satisfied in-step by the
  pinned metadata wire tests (M6/M7) plus the Step 13 fixture.

## Out of scope for Step 9

- **Engine lifecycle** (`new_project` / `open_project` / `save_snapshot_now`) and
  the **background snapshot writer** — Step 11. Step 9 ships `append_*`,
  `store_metadata`, `load_current_metadata`, and the resolution primitives the
  engine composes.
- **The `project` SQLite singleton table** (sample-rate, `next_*_id` counters) —
  Step 11. Distinct from the `Metadata` blob (see the distinction decision). Step 9
  only *renames* the table in the baseline migration; it writes no rows to it.
- **Persisting the `FoundViaAbsolute` relative-path rewrite** and the
  **Missing-Files dialog** — Step 11 / M6. M1 resolution is pure and read-only.
- **Undo/redo journaling** of inverse rows — Step 10 (it calls the same `append_*`
  helpers).
- **A `command_id`-aware history view / unified `JournalEntry` query** — M5+.
  Step 9 ships `CommandId` + `from_code`; it builds no UI.
- **Any V2 of `MetadataV1`** — one dispatch arm; V2 follows the Turn/Label recipe
  if the shape ever changes.

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` — green with `unwrap_used`, `expect_used`,
  `panic`, and `missing_docs` all CI-gated. New `pub(crate)` plumbing carries
  `#[allow(dead_code)]`; all `pub` items carry doc-comments.
- `cargo test -p core command_id::`, `cargo test -p core journal::`,
  `cargo test -p core metadata::` — the tests above.
- `cargo test -p core` — confirms no regression from the new `pub mod command_id;`
  / `pub mod metadata;` lines and the `journal.rs` additions.
- Manual diff review of `metadata.rs` against
  [data-model.md § Non-timeline data](../design/data-model.md#non-timeline-data) (field-for-field)
  and `resolve_track_source` against
  [§ Audio file resolution](../design/data-model.md#audio-file-resolution) (step-for-step).
- One commit on `claude/1M1`, **unsigned** per the GPG-by-branch policy in
  [CLAUDE.md](../CLAUDE.md). Suggested subject:
  `1M1-09: journal write side + command codes + metadata`. Bundles
  `core/src/db/journal.rs` (write side + `latest_metadata`),
  `core/src/project/command_id.rs`, `core/src/project/metadata.rs`, the two
  `pub mod` lines in `project/mod.rs`, and (per the pre-commit checklist) the
  `phase1-m1.md` Step 9 / module-layout edits above.

## Downstream implications (flag for later steps)

- **Step 10 (`undo.rs`):** undo/redo appends inverse rows by calling
  `append_delta_batch` / `append_metadata` with the relevant `CommandId`. An undo
  stamps the matching `Undo*` variant (`category | UNDO_FLAG`); a redo re-stamps
  the plain category. The same helpers this step ships — no new journal API needed.
- **Step 11 (`engine.rs`):** `new_project` stores the initial `Snapshot` blob and
  calls `append_snapshot(tx, CommandId::Unknown, &h, now)` (a snapshot is not an
  edit), plus writes the `project` **singleton table**; `save_snapshot_now`
  likewise calls `append_snapshot` with `CommandId::Unknown`. (The first
  *category-bearing* codes appear in M2+ editing commands.) `open_project` calls
  `metadata::load_current_metadata(&db, None)` and
  `metadata::missing_tracks(dir, &meta)` to build the missing-files list, and is
  where the `FoundViaAbsolute` relative-path rewrite is finally persisted (a
  `type = -1` write). This is also where the `#[allow(dead_code)]` on
  `store::put`/`get`, `Db::conn`, and the Step 8/9 `pub(crate)` plumbing is removed
  (first genuine non-test callers). The engine owns the wall clock — a
  `now_posix() -> i64` helper lives there and supplies `append_*`'s `applied_at`.
- **Step 13 (G1 fixture):** the committed `.vocalboard` file contains a real
  `Kind::Metadata` blob; opening it exercises `load_metadata` + `load_current_metadata`,
  completing the G1 round-trip for the metadata format on top of the in-step
  pinned-bytes tests.
- **M2+ (editing/ML/recording commands):** most new commands map onto a category
  bit **already reserved** here (e.g. `cut_words` → `Cut`, `align_tracks` →
  `AdjustSpacing`), so they consume no new code — they just start stamping an
  existing variant. Only a genuinely new *category* claims the next unused bit
  (`0x4000`, …) with its `Undo*` sibling; the append-only / never-renumber policy
  is the durable contract. M5+ adds the `command_id`-aware history view that
  OR-folds a journal range into a category set via these bits.
