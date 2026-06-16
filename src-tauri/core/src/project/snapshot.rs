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

use super::delta::{self, AdjacencyList, DecodeBatchError, DeltaError};
use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};
use super::label::{decode_label, Label};
use super::tree::ImplicitTimelineTree;
use super::turn::{decode_turn, Turn};
use crate::db::journal::{self, JournalError};
use crate::db::store::{self, StoreError};
use crate::db::Db;

/// Format version emitted by every new [`encode_snapshot`] call.
///
/// [`decode_snapshot`] reads any version present in the dispatch table; only `1` is
/// known in M1.
pub const LATEST_SNAPSHOT_VERSION: u8 = 1;

/// A full timeline snapshot: per-track ordered element-hash sequences.
///
/// Stored as a content-addressed `Kind::Snapshot` blob and referenced by a
/// `type = 1` journal row. The snapshot is kind-agnostic: `track_id == 0`
/// entries point to [`Label`] blobs; `track_id > 0` to [`Turn`] blobs. The
/// loader picks `decode_label` / `decode_turn` from `track_id` at fetch time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Per-track ordered element hashes, in timeline order.
    ///
    /// Sorted by `track_id` when produced by [`snapshot_from_trees`] for
    /// byte-identical deduplication in the content-addressed store.
    pub tracks: Vec<(u32, Vec<Hash>)>,
}

/// Encode `snap` as the latest Snapshot-kind wire format.
///
/// Always emits `(Kind::Snapshot, LATEST_SNAPSHOT_VERSION)`. Returns the
/// content-addressing hash of the tagged bytes and the tagged-bytes blob
/// itself, ready for `store::put`.
pub fn encode_snapshot(snap: &Snapshot) -> Result<(Hash, Vec<u8>), postcard::Error> {
    let v1 = v1::SnapshotV1::from(snap);
    encode_tagged(Kind::Snapshot, LATEST_SNAPSHOT_VERSION, &v1)
}

/// Decode a `Kind::Snapshot` blob into the latest in-memory [`Snapshot`].
///
/// Verifies the tag is `Kind::Snapshot`, dispatches on the version nibble, and
/// upgrades through `From<SnapshotV{N}> for Snapshot`. Unknown versions return
/// [`DecodeError::UnknownVersion`]; non-Snapshot tags return
/// [`DecodeError::KindMismatch`].
pub fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (_, version) = parse_tag(bytes[0])?;
    match version {
        1 => {
            let (_, v1_snap): (u8, v1::SnapshotV1) = decode_tagged_as(Kind::Snapshot, bytes)?;
            Ok(Snapshot::from(v1_snap))
        }
        _ => Err(DecodeError::UnknownVersion {
            kind: Kind::Snapshot,
            version,
        }),
    }
}

/// Frozen V1 wire schema for snapshots.
///
/// **Pre-1.0:** MAY be revised if implementation surfaces a missing or wrong
/// field; every revision requires regenerating the pinned hex/hash tests and
/// any committed G1 fixtures, and SHOULD bump `min_app_version`.
/// **Post-1.0:** frozen indefinitely. Shape changes go through a new `mod v2`,
/// bumping `LATEST_SNAPSHOT_VERSION`, and writing `From<SnapshotV2> for Snapshot`.
pub mod v1 {
    use serde::{Deserialize, Serialize};

    use super::super::hash::Hash;

    /// Frozen V1 wire representation of a [`super::Snapshot`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SnapshotV1 {
        /// Per-track ordered element hashes, in timeline order.
        pub tracks: Vec<(u32, Vec<Hash>)>,
    }
}

impl From<v1::SnapshotV1> for Snapshot {
    fn from(v: v1::SnapshotV1) -> Self {
        Snapshot { tracks: v.tracks }
    }
}

impl From<&Snapshot> for v1::SnapshotV1 {
    fn from(v: &Snapshot) -> Self {
        v1::SnapshotV1 {
            tracks: v.tracks.clone(),
        }
    }
}

/// Per-track in-memory element tree, discriminated by track kind.
///
/// Track 0 holds `Labels`; every other track holds `Speech`. [`PartialEq`]
/// delegates to the inner tree's sequence-equality: two trees built via
/// different paths are equal when their `(hash, total_duration)` sequences
/// match.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackTree {
    /// Track 0 label entries.
    Labels(ImplicitTimelineTree<Label>),
    /// Speech turn entries for tracks 1+.
    Speech(ImplicitTimelineTree<Turn>),
}

impl TrackTree {
    /// Return the ordered element hashes for this track, in timeline order.
    pub fn hashes(&self) -> Vec<Hash> {
        match self {
            TrackTree::Labels(tree) => tree.iter().map(|e| e.hash).collect(),
            TrackTree::Speech(tree) => tree.iter().map(|e| e.hash).collect(),
        }
    }
}

/// Map from `track_id` to its in-memory [`TrackTree`].
///
/// `BTreeMap` iteration is sorted by key, ensuring `snapshot_from_trees`
/// produces `tracks` in `track_id` order for deterministic serialization.
pub type PerTrackTrees = BTreeMap<u32, TrackTree>;

/// Errors returned by the replay load entry points.
#[allow(private_interfaces)]
#[derive(Debug)]
pub enum ReplayError {
    /// No `type = 1` snapshot row exists at or before the requested `as_of`.
    ///
    /// With `as_of = None` this means a malformed project (`new_project` always
    /// writes an initial snapshot, so it implies corruption/tampering); with a
    /// bounded `as_of` it means the point precedes the first snapshot.
    NoSnapshot,
    /// A `type = 1` row's payload was not a 16-byte hash pointer.
    MalformedSnapshotPayload {
        /// The offending row id.
        row_id: i64,
        /// Actual payload length.
        len: usize,
    },
    /// A `type = 0` row's payload failed to decode into a delta batch.
    DeltaDecode {
        /// The offending row id.
        row_id: i64,
        /// Decode error.
        source: DecodeBatchError,
    },
    /// A delta failed to apply during forward replay.
    DeltaApply {
        /// The offending row id.
        row_id: i64,
        /// Apply error.
        source: DeltaError,
    },
    /// A blob fetch failed (not found, or on-disk corruption from `store::get`).
    Store(StoreError),
    /// An element or snapshot blob failed to deserialize.
    Decode(DecodeError),
    /// A journal read query failed.
    Journal(JournalError),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::NoSnapshot => write!(f, "no snapshot row found"),
            ReplayError::MalformedSnapshotPayload { row_id, len } => write!(
                f,
                "snapshot row {row_id} has a {len}-byte payload (expected 16)"
            ),
            ReplayError::DeltaDecode { row_id, source } => {
                write!(f, "delta decode failed at journal row {row_id}: {source:?}")
            }
            ReplayError::DeltaApply { row_id, source } => {
                write!(f, "delta apply failed at journal row {row_id}: {source:?}")
            }
            ReplayError::Store(e) => write!(f, "blob store error: {e}"),
            ReplayError::Decode(e) => write!(f, "blob decode error: {e}"),
            ReplayError::Journal(e) => write!(f, "journal error: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReplayError::Store(e) => Some(e),
            ReplayError::Decode(e) => Some(e),
            ReplayError::Journal(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for ReplayError {
    fn from(e: StoreError) -> Self {
        ReplayError::Store(e)
    }
}

impl From<DecodeError> for ReplayError {
    fn from(e: DecodeError) -> Self {
        ReplayError::Decode(e)
    }
}

impl From<JournalError> for ReplayError {
    fn from(e: JournalError) -> Self {
        ReplayError::Journal(e)
    }
}

/// Flatten `trees` into a [`Snapshot`] in `track_id` order (ready for `encode_snapshot`).
///
/// Pure: no DB I/O. The engine calls this on a frozen clone before writing via
/// `encode_snapshot` + `store::put` + a `type = 1` journal append.
pub(crate) fn snapshot_from_trees(trees: &PerTrackTrees) -> Snapshot {
    let tracks = trees
        .iter()
        .map(|(&track_id, tree)| (track_id, tree.hashes()))
        .collect();
    Snapshot { tracks }
}

/// Reconstruct per-track trees from the latest snapshot at or before `as_of`,
/// **without** applying any deltas. The snapshot-only recovery / inspection path.
///
/// Returns `(snapshot_row_id, trees)` so the engine can identify the snapshot
/// it rolled back to in a recoverable-error message. Errors [`ReplayError::NoSnapshot`]
/// if no `type = 1` row exists at or before `as_of`.
///
/// "Latest" (not "current") is deliberate: a snapshot is a raw checkpoint, not
/// the effective timeline state — the deltas after it still have to be applied.
/// The effective-state loaders are [`load_and_replay`] (trees = snapshot + deltas)
/// and [`load_current_metadata`](crate::project::metadata::load_current_metadata)
/// (metadata, which needs no replay, so its latest row *is* the current value).
pub(crate) fn load_latest_snapshot(
    db: &Db,
    as_of: Option<i64>,
) -> Result<(i64, PerTrackTrees), ReplayError> {
    let (snapshot_id, adj) = snapshot_adjacency(db, as_of)?;
    Ok((snapshot_id, build_trees(db.conn(), &adj)?))
}

/// Reconstruct per-track trees as of `as_of`: latest snapshot at/before `as_of`,
/// then every `type = 0` batch with `snapshot_id < id <= as_of`. The happy path.
///
/// `as_of = None` applies all rows after the snapshot (open-to-latest).
pub(crate) fn load_and_replay(db: &Db, as_of: Option<i64>) -> Result<PerTrackTrees, ReplayError> {
    let (snapshot_id, mut adj) = snapshot_adjacency(db, as_of)?;
    replay_into(db, snapshot_id, as_of, &mut adj)?;
    build_trees(db.conn(), &adj)
}

// --- module-internal adjacency-passing helpers ---

type AdjLists = BTreeMap<u32, AdjacencyList>;

/// Latest snapshot at/before `as_of` → per-track adjacency lists seeded from
/// the snapshot's hash sequences via `AdjacencyList::from_sequence`. No element
/// blobs fetched, no trees built. Returns the snapshot row id too.
fn snapshot_adjacency(db: &Db, as_of: Option<i64>) -> Result<(i64, AdjLists), ReplayError> {
    let snap_row = journal::latest_snapshot(db.conn(), as_of)?.ok_or(ReplayError::NoSnapshot)?;

    let snap_bytes = store::get(db.conn(), &snap_row.hash)?;
    let snap = decode_snapshot(&snap_bytes)?;

    let mut adj: AdjLists = BTreeMap::new();
    for (track_id, hashes) in snap.tracks {
        adj.insert(track_id, AdjacencyList::from_sequence(hashes));
    }

    Ok((snap_row.id, adj))
}

/// Apply the `type = 0` rows with `snapshot_id < id <= as_of`, in id order,
/// to the adjacency lists in place. Routes each delta to its track's list.
fn replay_into(
    db: &Db,
    snapshot_id: i64,
    as_of: Option<i64>,
    adj: &mut AdjLists,
) -> Result<(), ReplayError> {
    debug_assert!(
        as_of.is_none_or(|x| x >= snapshot_id),
        "as_of ({as_of:?}) must be >= snapshot_id ({snapshot_id})"
    );
    let delta_rows = journal::deltas_after(db.conn(), snapshot_id, as_of)?;
    for row in delta_rows {
        let batch =
            delta::decode_delta_batch(&row.payload).map_err(|e| ReplayError::DeltaDecode {
                row_id: row.id,
                source: e,
            })?;
        for d in &batch {
            let list = adj.entry(d.track_id).or_insert_with(AdjacencyList::new);
            delta::apply(list, std::slice::from_ref(d)).map_err(|e| ReplayError::DeltaApply {
                row_id: row.id,
                source: e,
            })?;
        }
    }
    Ok(())
}

/// Walk each adjacency list to its ordered `Vec<Hash>`, fetch + decode every
/// element blob, and bulk-build the per-track trees. The single construction sweep.
fn build_trees(conn: &Connection, adj: &AdjLists) -> Result<PerTrackTrees, ReplayError> {
    let mut trees = PerTrackTrees::new();
    for (&track_id, list) in adj {
        let seq: Vec<Hash> = list.iter().collect();
        let tree = build_track_tree(conn, track_id, seq)?;
        trees.insert(track_id, tree);
    }
    Ok(trees)
}

/// Fetch + decode every element hash in `seq`, then bulk-build a [`TrackTree`].
/// Dispatches `decode_label` for `track_id == 0`, `decode_turn` for `track_id > 0`.
fn build_track_tree(
    conn: &Connection,
    track_id: u32,
    seq: Vec<Hash>,
) -> Result<TrackTree, ReplayError> {
    if track_id == 0 {
        let mut elements = Vec::with_capacity(seq.len());
        for h in seq {
            let bytes = store::get(conn, &h)?;
            let label = decode_label(&bytes)?;
            elements.push((h, Arc::new(label)));
        }
        Ok(TrackTree::Labels(
            ImplicitTimelineTree::from_sorted_elements(elements),
        ))
    } else {
        let mut elements = Vec::with_capacity(seq.len());
        for h in seq {
            let bytes = store::get(conn, &h)?;
            let turn = decode_turn(&bytes)?;
            elements.push((h, Arc::new(turn)));
        }
        Ok(TrackTree::Speech(
            ImplicitTimelineTree::from_sorted_elements(elements),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{store, Db};
    use crate::project::delta::{encode_delta_batch, Delta, Location};
    use crate::project::label::{encode_label, Label, LabelKind};
    use crate::project::turn::{encode_turn, Turn};
    use tempfile::tempdir;

    fn open_tmp_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = Db::create(&path).unwrap();
        (dir, db)
    }

    fn put_turn(db: &Db, id: u64, dur: i64, silence: i64) -> Hash {
        let turn = Turn {
            id,
            speaker_id: None,
            turn_duration: dur,
            post_turn_silence: silence,
            words: vec![],
            splices: vec![],
        };
        let (h, bytes) = encode_turn(&turn).unwrap();
        store::put(db.conn(), &h, &bytes).unwrap();
        h
    }

    fn put_label(db: &Db, id: u64, silence: i64) -> Hash {
        let label = Label {
            id,
            text: String::new(),
            kind: LabelKind::Plain,
            post_label_silence: silence,
        };
        let (h, bytes) = encode_label(&label).unwrap();
        store::put(db.conn(), &h, &bytes).unwrap();
        h
    }

    fn write_snapshot_row(db: &Db, snap: &Snapshot) -> i64 {
        let (h, bytes) = encode_snapshot(snap).unwrap();
        store::put(db.conn(), &h, &bytes).unwrap();
        db.conn()
            .execute(
                "INSERT INTO journal (type, payload, command_id, applied_at) \
                 VALUES (1, ?1, 0, 0)",
                (&h.0[..],),
            )
            .unwrap();
        db.conn().last_insert_rowid()
    }

    fn write_delta_row(db: &Db, batch: &[Delta]) -> i64 {
        let payload = encode_delta_batch(batch).unwrap();
        db.conn()
            .execute(
                "INSERT INTO journal (type, payload, command_id, applied_at) \
                 VALUES (0, ?1, 0, 0)",
                (&payload[..],),
            )
            .unwrap();
        db.conn().last_insert_rowid()
    }

    // Fixed sentinel hashes for the pinned-bytes tests.
    fn sample_snapshot() -> Snapshot {
        fn h(b: u8) -> Hash {
            let mut arr = [0u8; 16];
            arr[0] = b;
            Hash(arr)
        }
        Snapshot {
            tracks: vec![
                (0, vec![h(0x10), h(0x11)]),
                (1, vec![h(0x20), h(0x21), h(0x22)]),
            ],
        }
    }

    // ── Snapshot blob ────────────────────────────────────────────────────────

    // S1
    #[test]
    fn snapshot_round_trips() {
        fn h(b: u8) -> Hash {
            let mut arr = [0u8; 16];
            arr[0] = b;
            Hash(arr)
        }
        let snap = Snapshot {
            tracks: vec![
                (0, vec![h(0x10), h(0x11)]),
                (1, vec![h(0x20), h(0x21), h(0x22)]),
            ],
        };
        let (_, bytes) = encode_snapshot(&snap).unwrap();
        let decoded = decode_snapshot(&bytes).unwrap();
        assert_eq!(decoded, snap);
    }

    // S2
    #[test]
    fn snapshot_empty_round_trips() {
        let empty = Snapshot { tracks: vec![] };
        let (_, bytes) = encode_snapshot(&empty).unwrap();
        assert_eq!(decode_snapshot(&bytes).unwrap(), empty);

        let with_empty_track = Snapshot {
            tracks: vec![(1, vec![])],
        };
        let (_, bytes2) = encode_snapshot(&with_empty_track).unwrap();
        assert_eq!(decode_snapshot(&bytes2).unwrap(), with_empty_track);
    }

    // S3
    #[test]
    fn decode_snapshot_rejects_wrong_kind() {
        let (_, bytes) = encode_turn(&Turn {
            id: 1,
            speaker_id: None,
            turn_duration: 100,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![],
        })
        .unwrap();
        let err = decode_snapshot(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::KindMismatch {
                    expected: Kind::Snapshot,
                    found: Kind::Turn,
                }
            ),
            "expected KindMismatch, got: {err:?}"
        );
    }

    // S4
    #[test]
    fn decode_snapshot_rejects_empty() {
        let err = decode_snapshot(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    // S5
    #[test]
    fn decode_snapshot_rejects_unknown_version() {
        use crate::project::hash::tag_byte;
        let tag = tag_byte(Kind::Snapshot, 2);
        let bytes = [tag, 0x00];
        let err = decode_snapshot(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::UnknownVersion {
                    kind: Kind::Snapshot,
                    version: 2,
                }
            ),
            "expected UnknownVersion, got: {err:?}"
        );
    }

    // S6
    #[test]
    fn v1_wire_format_pinned() {
        let snap = sample_snapshot();
        let (_, bytes) = encode_snapshot(&snap).unwrap();
        assert_eq!(
            bytes.as_slice(),
            &PINNED_WIRE_BYTES,
            "V1 wire format changed — regenerate via capture_pinned_values"
        );
    }

    // S7
    #[test]
    fn v1_wire_hash_pinned() {
        let snap = sample_snapshot();
        let (hash, _) = encode_snapshot(&snap).unwrap();
        assert_eq!(
            hash.0, PINNED_HASH,
            "V1 wire hash changed — regenerate via capture_pinned_values"
        );
    }

    // S8: run with: cargo test -p core snapshot::tests::capture_pinned_values -- --ignored --nocapture
    #[test]
    #[ignore]
    fn capture_pinned_values() {
        let snap = sample_snapshot();
        let (hash, bytes) = encode_snapshot(&snap).unwrap();
        println!("PINNED_WIRE_BYTES len={}", bytes.len());
        print!("const PINNED_WIRE_BYTES: [u8; {}] = [", bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("0x{b:02x}");
        }
        println!("];");
        print!("const PINNED_HASH: [u8; 16] = [");
        for (i, b) in hash.0.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("0x{b:02x}");
        }
        println!("];");
    }

    // S9
    #[test]
    fn v1_conversions_total_round_trip() {
        let snap = sample_snapshot();
        let restored = Snapshot::from(v1::SnapshotV1::from(&snap));
        assert_eq!(restored, snap);
    }

    // ── Flatten ──────────────────────────────────────────────────────────────

    // F1
    #[test]
    fn snapshot_from_trees_orders_by_track_id() {
        let (_dir, db) = open_tmp_db();
        let h0 = put_label(&db, 1, 100);
        let h1a = put_turn(&db, 10, 200, 0);
        let h2a = put_turn(&db, 20, 300, 0);

        let mut trees = PerTrackTrees::new();
        trees.insert(2, build_track_tree(db.conn(), 2, vec![h2a]).unwrap());
        trees.insert(0, build_track_tree(db.conn(), 0, vec![h0]).unwrap());
        trees.insert(1, build_track_tree(db.conn(), 1, vec![h1a]).unwrap());

        let snap = snapshot_from_trees(&trees);
        let track_ids: Vec<u32> = snap.tracks.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            track_ids,
            vec![0, 1, 2],
            "tracks must be in ascending track_id order"
        );
        assert_eq!(snap.tracks[0].1, vec![h0]);
        assert_eq!(snap.tracks[1].1, vec![h1a]);
        assert_eq!(snap.tracks[2].1, vec![h2a]);
    }

    // F2
    #[test]
    fn snapshot_from_trees_round_trips_through_build() {
        let (_dir, db) = open_tmp_db();
        let h0a = put_label(&db, 1, 100);
        let h0b = put_label(&db, 2, 200);
        let h1a = put_turn(&db, 10, 300, 0);
        let h1b = put_turn(&db, 11, 400, 50);

        let orig_tree0 = build_track_tree(db.conn(), 0, vec![h0a, h0b]).unwrap();
        let orig_tree1 = build_track_tree(db.conn(), 1, vec![h1a, h1b]).unwrap();
        let mut trees = PerTrackTrees::new();
        trees.insert(0, orig_tree0);
        trees.insert(1, orig_tree1);

        let snap = snapshot_from_trees(&trees);

        for (track_id, seq) in &snap.tracks {
            let rebuilt = build_track_tree(db.conn(), *track_id, seq.clone()).unwrap();
            assert_eq!(
                &rebuilt,
                trees.get(track_id).unwrap(),
                "rebuilt track {track_id} must equal original"
            );
        }
    }

    // F3
    #[test]
    fn track_tree_hashes_matches_iter() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);
        let speech = build_track_tree(db.conn(), 1, vec![h1, h2]).unwrap();
        let expected: Vec<Hash> = if let TrackTree::Speech(ref t) = speech {
            t.iter().map(|e| e.hash).collect()
        } else {
            panic!("expected Speech variant")
        };
        assert_eq!(speech.hashes(), expected);

        let hl1 = put_label(&db, 10, 50);
        let labels = build_track_tree(db.conn(), 0, vec![hl1]).unwrap();
        let expected_l: Vec<Hash> = if let TrackTree::Labels(ref t) = labels {
            t.iter().map(|e| e.hash).collect()
        } else {
            panic!("expected Labels variant")
        };
        assert_eq!(labels.hashes(), expected_l);
    }

    // ── Replay — happy path ───────────────────────────────────────────────────

    // R1
    #[test]
    fn load_latest_snapshot_speech_track() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);
        let h3 = put_turn(&db, 3, 300, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1, h2, h3])],
        };
        let snap_id = write_snapshot_row(&db, &snap);

        let (returned_id, trees) = load_latest_snapshot(&db, None).unwrap();
        assert_eq!(returned_id, snap_id);
        assert!(trees.contains_key(&1));
        let expected = build_track_tree(db.conn(), 1, vec![h1, h2, h3]).unwrap();
        assert_eq!(trees[&1], expected);
    }

    // R2
    #[test]
    fn load_latest_snapshot_labels_track() {
        let (_dir, db) = open_tmp_db();
        let hl1 = put_label(&db, 1, 100);
        let hl2 = put_label(&db, 2, 200);

        let snap = Snapshot {
            tracks: vec![(0, vec![hl1, hl2])],
        };
        write_snapshot_row(&db, &snap);

        let (_, trees) = load_latest_snapshot(&db, None).unwrap();
        assert_eq!(trees.len(), 1, "only track 0 (labels)");
        assert!(trees.contains_key(&0));
        assert!(matches!(trees[&0], TrackTree::Labels(_)));
        let expected = build_track_tree(db.conn(), 0, vec![hl1, hl2]).unwrap();
        assert_eq!(trees[&0], expected);
    }

    // R3
    #[test]
    fn load_and_replay_equals_snapshot_when_no_deltas() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);
        let snap = Snapshot {
            tracks: vec![(1, vec![h1, h2])],
        };
        write_snapshot_row(&db, &snap);

        let (_, snap_trees) = load_latest_snapshot(&db, None).unwrap();
        let replay_trees = load_and_replay(&db, None).unwrap();
        assert_eq!(snap_trees, replay_trees);
    }

    // R4
    #[test]
    fn replay_inserts_after_snapshot_speech() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h1), h2)]);

        let trees = load_and_replay(&db, None).unwrap();
        let expected = build_track_tree(db.conn(), 1, vec![h1, h2]).unwrap();
        assert_eq!(trees[&1], expected);
    }

    // R5
    #[test]
    fn replay_full_op_mix_speech() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);
        let h3 = put_turn(&db, 3, 300, 0);
        let h4 = put_turn(&db, 4, 300, 0);
        let h5 = put_turn(&db, 5, 150, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1, h2, h3])],
        };
        write_snapshot_row(&db, &snap);

        // Row A: delete h2 (after h1), then update h3→h4 (after h1)
        write_delta_row(
            &db,
            &[
                Delta::delete_after(1, Location::After(h1)),
                Delta::update_after(1, Location::After(h1), h4),
            ],
        );
        // Row B: append h5 after h4
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h4), h5)]);

        let trees = load_and_replay(&db, None).unwrap();
        let expected = build_track_tree(db.conn(), 1, vec![h1, h4, h5]).unwrap();
        assert_eq!(trees[&1], expected);
    }

    // R6
    #[test]
    fn replay_labels_track() {
        let (_dir, db) = open_tmp_db();
        let hl1 = put_label(&db, 1, 100);
        let hl2 = put_label(&db, 2, 200);

        let snap = Snapshot {
            tracks: vec![(0, vec![hl1])],
        };
        write_snapshot_row(&db, &snap);
        write_delta_row(&db, &[Delta::insert_after(0, Location::After(hl1), hl2)]);

        let trees = load_and_replay(&db, None).unwrap();
        let expected = build_track_tree(db.conn(), 0, vec![hl1, hl2]).unwrap();
        assert_eq!(trees[&0], expected);
        assert!(matches!(trees[&0], TrackTree::Labels(_)));
    }

    // R7
    #[test]
    fn replay_multiple_tracks_independent() {
        let (_dir, db) = open_tmp_db();
        let hl1 = put_label(&db, 1, 100);
        let hl2 = put_label(&db, 2, 200);
        let h1 = put_turn(&db, 10, 100, 0);
        let h2 = put_turn(&db, 11, 200, 0);

        let snap = Snapshot {
            tracks: vec![(0, vec![hl1]), (1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        // Single delta row with ops for both tracks
        write_delta_row(
            &db,
            &[
                Delta::insert_after(0, Location::After(hl1), hl2),
                Delta::insert_after(1, Location::After(h1), h2),
            ],
        );

        let trees = load_and_replay(&db, None).unwrap();
        let expected0 = build_track_tree(db.conn(), 0, vec![hl1, hl2]).unwrap();
        let expected1 = build_track_tree(db.conn(), 1, vec![h1, h2]).unwrap();
        assert_eq!(trees[&0], expected0);
        assert_eq!(trees[&1], expected1);
    }

    // R8
    #[test]
    fn replay_preserves_untouched_tracks() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);
        let h3 = put_turn(&db, 3, 300, 0);
        let h4 = put_turn(&db, 4, 400, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1, h2]), (2, vec![h3, h4])],
        };
        write_snapshot_row(&db, &snap);
        // Only touch track 1
        write_delta_row(&db, &[Delta::delete_after(1, Location::Start)]);

        let trees = load_and_replay(&db, None).unwrap();
        let expected1 = build_track_tree(db.conn(), 1, vec![h2]).unwrap();
        let expected2 = build_track_tree(db.conn(), 2, vec![h3, h4]).unwrap();
        assert_eq!(trees[&1], expected1);
        assert_eq!(trees[&2], expected2, "track 2 must be unchanged");
    }

    // R9
    #[test]
    fn replay_intra_batch_forward_reference() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);
        let h3 = put_turn(&db, 3, 300, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        write_delta_row(
            &db,
            &[
                Delta::insert_after(1, Location::After(h1), h2),
                Delta::insert_after(1, Location::After(h2), h3),
            ],
        );

        let trees = load_and_replay(&db, None).unwrap();
        let expected = build_track_tree(db.conn(), 1, vec![h1, h2, h3]).unwrap();
        assert_eq!(trees[&1], expected);
    }

    // R10
    #[test]
    fn replay_track_born_from_deltas() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h3a = put_turn(&db, 30, 150, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        // Delta inserts onto track 3 (not in snapshot)
        write_delta_row(&db, &[Delta::insert_after(3, Location::Start, h3a)]);

        let trees = load_and_replay(&db, None).unwrap();
        assert!(
            trees.contains_key(&3),
            "track 3 should appear even though it was not in snapshot"
        );
        let expected3 = build_track_tree(db.conn(), 3, vec![h3a]).unwrap();
        assert_eq!(trees[&3], expected3);
    }

    // R11
    #[test]
    fn replay_snapshot_with_empty_track() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1]), (2, vec![])],
        };
        write_snapshot_row(&db, &snap);

        let (_, trees) = load_latest_snapshot(&db, None).unwrap();
        assert!(trees.contains_key(&1));
        assert!(trees.contains_key(&2), "empty track 2 must be present");
        if let TrackTree::Speech(ref t) = trees[&2] {
            assert!(t.is_empty());
            assert_eq!(t.len(), 0);
        } else {
            panic!("track 2 should be Speech variant");
        }

        // Also verify track-0 empty variant
        let snap0 = Snapshot {
            tracks: vec![(0, vec![])],
        };
        write_snapshot_row(&db, &snap0);
        let (_, trees0) = load_latest_snapshot(&db, None).unwrap();
        assert!(matches!(trees0[&0], TrackTree::Labels(ref t) if t.is_empty()));
    }

    // R12
    #[test]
    fn replay_empty_snapshot_yields_no_tracks() {
        let (_dir, db) = open_tmp_db();
        let snap = Snapshot { tracks: vec![] };
        write_snapshot_row(&db, &snap);

        let (_, snap_trees) = load_latest_snapshot(&db, None).unwrap();
        assert!(snap_trees.is_empty(), "load_latest_snapshot: no tracks");

        let replay_trees = load_and_replay(&db, None).unwrap();
        assert!(replay_trees.is_empty(), "load_and_replay: no tracks");
    }

    // ── Replay — point-in-history `as_of` ────────────────────────────────────

    // AO1
    #[test]
    fn replay_as_of_midway_between_deltas() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 100, 0);
        let h3 = put_turn(&db, 3, 100, 0);
        let h4 = put_turn(&db, 4, 100, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h1), h2)]);
        let d2 = write_delta_row(&db, &[Delta::insert_after(1, Location::After(h2), h3)]);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h3), h4)]);

        let trees = load_and_replay(&db, Some(d2)).unwrap();
        let expected = build_track_tree(db.conn(), 1, vec![h1, h2, h3]).unwrap();
        assert_eq!(
            trees[&1], expected,
            "as_of=d2 should include h1,h2,h3 but not h4"
        );
    }

    // AO2
    #[test]
    fn load_latest_snapshot_as_of_picks_earlier_snapshot() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 100, 0);

        let snap1 = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        let s1 = write_snapshot_row(&db, &snap1);
        // Insert a delta row to create a strict gap: s1 < d_between < s2
        let d_between = write_delta_row(&db, &[]);
        let snap2 = Snapshot {
            tracks: vec![(1, vec![h1, h2])],
        };
        let s2 = write_snapshot_row(&db, &snap2);
        assert!(
            s1 < d_between && d_between < s2,
            "gap: {s1} < {d_between} < {s2}"
        );

        // as_of = d_between is strictly between s1 and s2
        let (returned_id, trees) = load_latest_snapshot(&db, Some(d_between)).unwrap();
        assert_eq!(returned_id, s1, "should select s1 (not s2) when as_of < s2");
        let expected = build_track_tree(db.conn(), 1, vec![h1]).unwrap();
        assert_eq!(trees[&1], expected);
    }

    // AO3
    #[test]
    fn load_and_replay_as_of_at_snapshot_row() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 100, 0);
        let h3 = put_turn(&db, 3, 100, 0);

        let snap1 = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        let s1 = write_snapshot_row(&db, &snap1);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h1), h2)]);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h2), h3)]);
        let snap2 = Snapshot {
            tracks: vec![(1, vec![h1, h2, h3])],
        };
        let s2 = write_snapshot_row(&db, &snap2);
        let _ = s1;

        // as_of = s2 → selects s2 as snapshot, empty delta range → equals load_latest_snapshot
        let (_, snap_trees) = load_latest_snapshot(&db, Some(s2)).unwrap();
        let replay_trees = load_and_replay(&db, Some(s2)).unwrap();
        assert_eq!(
            snap_trees, replay_trees,
            "when as_of names a snapshot, load it directly with no deltas"
        );
    }

    // AO4
    #[test]
    fn load_and_replay_as_of_none_is_full_history() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 100, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h1), h2)]);

        let max_id: i64 = db
            .conn()
            .query_row("SELECT MAX(id) FROM journal", [], |r| r.get(0))
            .unwrap();

        let trees_none = load_and_replay(&db, None).unwrap();
        let trees_max = load_and_replay(&db, Some(max_id)).unwrap();
        assert_eq!(trees_none, trees_max, "None ⇒ end-of-journal");
    }

    // AO5
    #[test]
    fn load_latest_snapshot_as_of_before_first_snapshot_errors() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        let s1 = write_snapshot_row(&db, &snap);

        let err = load_latest_snapshot(&db, Some(s1 - 1)).unwrap_err();
        assert!(
            matches!(err, ReplayError::NoSnapshot),
            "as_of before first snapshot should error NoSnapshot"
        );
    }

    // AO6
    #[test]
    fn as_of_zero_and_negative_yield_no_snapshot() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);

        for &as_of in &[0i64, -1] {
            let err = load_latest_snapshot(&db, Some(as_of)).unwrap_err();
            assert!(
                matches!(err, ReplayError::NoSnapshot),
                "as_of={as_of} should error NoSnapshot (journal ids are positive AUTOINCREMENT)"
            );
            let err = load_and_replay(&db, Some(as_of)).unwrap_err();
            assert!(matches!(err, ReplayError::NoSnapshot));
        }
    }

    // ── Replay — recovery primitive + errors ─────────────────────────────────

    // E1
    #[test]
    fn load_latest_snapshot_ignores_later_deltas() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        write_delta_row(&db, &[Delta::insert_after(1, Location::After(h1), h2)]);

        let (_, trees) = load_latest_snapshot(&db, None).unwrap();
        let expected = build_track_tree(db.conn(), 1, vec![h1]).unwrap();
        assert_eq!(
            trees[&1], expected,
            "load_latest_snapshot must ignore type=0 rows"
        );
    }

    // E2
    #[test]
    fn load_latest_snapshot_no_snapshot_row_errors() {
        let (_dir, db) = open_tmp_db();
        let err = load_latest_snapshot(&db, None).unwrap_err();
        assert!(matches!(err, ReplayError::NoSnapshot));
    }

    // E3
    #[test]
    fn load_latest_snapshot_picks_most_recent() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h2 = put_turn(&db, 2, 200, 0);

        let snap1 = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap1);
        let snap2 = Snapshot {
            tracks: vec![(1, vec![h1, h2])],
        };
        write_snapshot_row(&db, &snap2);

        let (_, trees) = load_latest_snapshot(&db, None).unwrap();
        let expected = build_track_tree(db.conn(), 1, vec![h1, h2]).unwrap();
        assert_eq!(trees[&1], expected, "must use the higher-id snapshot");
    }

    // E4
    #[test]
    fn replay_malformed_delta_payload_carries_row_id() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);

        // Insert a type=0 row with an unknown-version payload (0xFF prefix)
        let bad_payload = [0xFF, 0x00, 0x01];
        let bad_row_id = db
            .conn()
            .execute(
                "INSERT INTO journal (type, payload, command_id, applied_at) \
                 VALUES (0, ?1, 0, 0)",
                (&bad_payload[..],),
            )
            .map(|_| db.conn().last_insert_rowid())
            .unwrap();

        let err = load_and_replay(&db, None).unwrap_err();
        assert!(
            matches!(err, ReplayError::DeltaDecode { row_id, .. } if row_id == bad_row_id),
            "expected DeltaDecode with row_id={bad_row_id}, got: {err:?}"
        );
    }

    // E5
    #[test]
    fn replay_unapplicable_delta_carries_row_id() {
        let (_dir, db) = open_tmp_db();
        let h1 = put_turn(&db, 1, 100, 0);
        let h_unknown = Hash([0xDE, 0xAD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        let snap = Snapshot {
            tracks: vec![(1, vec![h1])],
        };
        write_snapshot_row(&db, &snap);
        // Delta references a hash not in the adjacency list
        let bad_row_id = write_delta_row(
            &db,
            &[Delta::insert_after(1, Location::After(h_unknown), h1)],
        );

        let err = load_and_replay(&db, None).unwrap_err();
        assert!(
            matches!(
                err,
                ReplayError::DeltaApply {
                    row_id,
                    source: DeltaError::LocationNotFound(h),
                } if row_id == bad_row_id && h == h_unknown
            ),
            "expected DeltaApply with row_id={bad_row_id}, got: {err:?}"
        );
    }

    // E6
    #[test]
    fn replay_missing_element_blob_errors() {
        let (_dir, db) = open_tmp_db();
        // Hash not in store
        let h_missing = Hash([0xAB, 0xCD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        let snap = Snapshot {
            tracks: vec![(1, vec![h_missing])],
        };
        write_snapshot_row(&db, &snap);

        let err_snap = load_latest_snapshot(&db, None).unwrap_err();
        assert!(
            matches!(err_snap, ReplayError::Store(StoreError::NotFound(_))),
            "load_latest_snapshot should error Store(NotFound): {err_snap:?}"
        );

        let err_replay = load_and_replay(&db, None).unwrap_err();
        assert!(
            matches!(err_replay, ReplayError::Store(StoreError::NotFound(_))),
            "load_and_replay should error Store(NotFound): {err_replay:?}"
        );
    }

    // E7
    #[test]
    fn replay_corrupt_snapshot_payload_errors() {
        let (_dir, db) = open_tmp_db();
        // A type=1 row whose payload is not exactly 16 bytes
        let bad_payload = [0u8; 15];
        let row_id = db
            .conn()
            .execute(
                "INSERT INTO journal (type, payload, command_id, applied_at) \
                 VALUES (1, ?1, 0, 0)",
                (&bad_payload[..],),
            )
            .map(|_| db.conn().last_insert_rowid())
            .unwrap();

        let err = load_latest_snapshot(&db, None).unwrap_err();
        assert!(
            matches!(
                err,
                ReplayError::Journal(JournalError::MalformedHashPayload { id, len: 15 })
                if id == row_id
            ),
            "expected Journal(MalformedHashPayload {{ id={row_id}, len=15 }}), got: {err:?}"
        );
    }

    // E8
    #[test]
    fn replay_error_display_and_source() {
        use std::error::Error;

        let h = Hash([0u8; 16]);

        // All variants produce non-empty Display
        let variants: &[ReplayError] = &[
            ReplayError::NoSnapshot,
            ReplayError::MalformedSnapshotPayload { row_id: 1, len: 5 },
            ReplayError::Store(StoreError::NotFound(h)),
            ReplayError::Decode(DecodeError::Empty),
            ReplayError::Journal(JournalError::MalformedHashPayload { id: 1, len: 5 }),
        ];
        for v in variants {
            let msg = v.to_string();
            assert!(!msg.is_empty(), "Display should be non-empty for {v:?}");
        }

        // Three wrapper variants chain via source()
        assert!(
            ReplayError::Store(StoreError::NotFound(h))
                .source()
                .is_some(),
            "Store must chain via source()"
        );
        assert!(
            ReplayError::Decode(DecodeError::Empty).source().is_some(),
            "Decode must chain via source()"
        );
        assert!(
            ReplayError::Journal(JournalError::MalformedHashPayload { id: 1, len: 0 })
                .source()
                .is_some(),
            "Journal must chain via source()"
        );

        // Other variants do not chain
        assert!(ReplayError::NoSnapshot.source().is_none());
        assert!(ReplayError::MalformedSnapshotPayload { row_id: 1, len: 5 }
            .source()
            .is_none());
    }

    // ── Pinned bytes / hash for sample_snapshot() ────────────────────────────
    // Regenerate via: cargo test -p core snapshot::tests::capture_pinned_values -- --ignored --nocapture

    // Regenerate via: cargo test -p core snapshot::tests::capture_pinned_values -- --ignored --nocapture
    const PINNED_WIRE_BYTES: [u8; 86] = [
        0x31, 0x02, 0x00, 0x02, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x03, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const PINNED_HASH: [u8; 16] = [
        0x6d, 0x74, 0x86, 0xcf, 0x9e, 0xfe, 0x1c, 0xa5, 0xd2, 0xb8, 0x2e, 0xc6, 0x5d, 0x0e, 0x08,
        0xb5,
    ];
}
