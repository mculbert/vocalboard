//! Deltas: the journal-resident, kind-agnostic edit primitive.
//!
//! A delta names an edit site by the element that *precedes* it
//! (`Location::Start` or `Location::After(Hash)`) and one of three
//! ops (insert / update / delete). Replay builds an
//! `AdjacencyList` — a `HashMap<Location, Option<Hash>>` modelling
//! the "next element after location" edge set (with the terminal
//! end represented as a `Location → None` entry so every legal
//! location is a key) — from the latest snapshot and applies each
//! subsequent `type = 0` row's batch to it before walking the result
//! back to an ordered hash sequence. Forward edits (the engine's `apply_batch`) skip the
//! adjacency list entirely: the engine mutates the in-memory tree
//! directly through its O(log n) primitives and emits the
//! forward+inverse `Delta` pair at the edit site, where it already
//! holds `h_old` / `h_removed`.
//!
//! Delta payloads sit in `journal.payload` for `type = 0` rows, with
//! a leading `delta_version: u8` byte (M1 writes `0x01`) followed by
//! the postcard-serialized `Vec<Delta>`. See
//! [data-model.md § Deltas](../../../design/data-model.md#deltas).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::hash::Hash;

/// Wire version written by [`encode_delta_batch`] and recognised by
/// [`decode_delta_batch`].
pub(crate) const LATEST_DELTA_VERSION: u8 = 1;

/// One recorded edit to a track's element sequence.
///
/// `hash` is `None` iff `op == DeltaOp::DeleteAfter`. Use the typed
/// constructors ([`Delta::insert_after`], [`Delta::update_after`],
/// [`Delta::delete_after`]) to maintain that invariant; `apply` guards it
/// with `debug_assert!` in debug builds and returns
/// [`DeltaError::HashFieldMismatch`] in release builds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// Track this edit applies to. `0` = labels track.
    pub track_id: u32,
    /// Edit kind.
    pub op: DeltaOp,
    /// Element that *precedes* the edit site.
    pub location: Location,
    /// New / replacing element hash. `None` for `DeleteAfter`.
    pub hash: Option<Hash>,
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
/// Always names the element that *precedes* the site (never the site itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Location {
    /// The head of the track. Always a legal location.
    Start,
    /// Immediately after the element with this hash.
    After(Hash),
}

impl Delta {
    /// Construct an `InsertAfter` delta.
    pub fn insert_after(track_id: u32, location: Location, hash: Hash) -> Self {
        Self {
            track_id,
            op: DeltaOp::InsertAfter,
            location,
            hash: Some(hash),
        }
    }

    /// Construct an `UpdateAfter` delta.
    pub fn update_after(track_id: u32, location: Location, hash: Hash) -> Self {
        Self {
            track_id,
            op: DeltaOp::UpdateAfter,
            location,
            hash: Some(hash),
        }
    }

    /// Construct a `DeleteAfter` delta.
    pub fn delete_after(track_id: u32, location: Location) -> Self {
        Self {
            track_id,
            op: DeltaOp::DeleteAfter,
            location,
            hash: None,
        }
    }
}

/// Errors returned by delta application.
#[derive(Debug, PartialEq, Eq)]
pub enum DeltaError {
    /// `Location::After(h)` named an element not present in the adjacency list.
    LocationNotFound(Hash),
    /// `Update` or `Delete` at a location whose successor slot is empty.
    NoSuccessor(Location),
    /// Adjacency list invariant violated: a key that must be present was missing.
    ///
    /// `Location::Start` means `Start` was not seeded; `Location::After(h)` means
    /// `After(h)` had no entry despite `h` being a known element.
    MissingEdge(Location),
    /// `hash` field's None-iff-Delete invariant was violated.
    HashFieldMismatch {
        /// The operation that triggered the mismatch.
        op: DeltaOp,
        /// Whether a hash was present (should be the opposite of what `op` requires).
        hash_present: bool,
    },
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
/// `After(h)` location whose value is `None`. Operations maintain the
/// invariant atomically.
pub(crate) struct AdjacencyList {
    edges: HashMap<Location, Option<Hash>>,
}

impl AdjacencyList {
    /// Build an empty list. Seeds `{ Start: None }` so `Start` is always a key.
    pub(crate) fn new() -> Self {
        let mut edges = HashMap::new();
        edges.insert(Location::Start, None);
        Self { edges }
    }

    /// Build from an ordered hash sequence (e.g. a snapshot's `Vec<Hash>`).
    ///
    /// The first hash becomes `Start`'s successor; each subsequent hash
    /// becomes the previous one's successor; the last element's `After(h)`
    /// entry is seeded with `None` (terminal).
    pub(crate) fn from_sequence<I: IntoIterator<Item = Hash>>(seq: I) -> Self {
        let mut edges = HashMap::new();
        let mut prev = Location::Start;
        for h in seq {
            edges.insert(prev, Some(h));
            prev = Location::After(h);
        }
        edges.insert(prev, None);
        Self { edges }
    }

    /// Walk `Start → … → terminal`, yielding each element's hash in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Hash> + '_ {
        let mut current = Location::Start;
        std::iter::from_fn(move || match self.edges.get(&current).copied().flatten() {
            Some(h) => {
                current = Location::After(h);
                Some(h)
            }
            None => None,
        })
    }
}

/// Read-side query API. Exercised by unit tests and reserved for `apply_batch`
/// and diagnostics; no non-test lib caller exists yet.
#[allow(dead_code)]
impl AdjacencyList {
    /// Number of elements in the track.
    ///
    /// Computed as `edges.len() - 1`: every track has exactly one trailing
    /// terminal entry beyond its element count.
    pub(crate) fn len(&self) -> usize {
        self.edges.len() - 1
    }

    /// True if the track has no elements.
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// First element's hash, or `None` for an empty track.
    pub(crate) fn head(&self) -> Option<Hash> {
        self.successor(&Location::Start).flatten()
    }

    /// Two-layer lookup for a location:
    ///
    /// - `None` ⇒ `loc` is not a legal location in this list.
    /// - `Some(None)` ⇒ `loc` is legal and is the terminal end.
    /// - `Some(Some(h))` ⇒ `loc`'s successor is `h`.
    pub(crate) fn successor(&self, loc: &Location) -> Option<Option<Hash>> {
        self.edges.get(loc).copied()
    }
}

fn apply_one(adj: &mut AdjacencyList, d: &Delta) -> Result<(), DeltaError> {
    // Validate location is a key. Start is always seeded; After(h) iff h is an element.
    if !adj.edges.contains_key(&d.location) {
        return Err(match d.location {
            Location::After(h) => DeltaError::LocationNotFound(h),
            Location::Start => DeltaError::MissingEdge(Location::Start),
        });
    }

    match d.op {
        DeltaOp::InsertAfter => {
            let Some(new_h) = d.hash else {
                return Err(DeltaError::HashFieldMismatch {
                    op: DeltaOp::InsertAfter,
                    hash_present: false,
                });
            };
            // adj.edges[&loc] is safe: contains_key checked above.
            // The old value flows into the new element's outgoing edge,
            // preserving the chain whether loc was terminal or not.
            let prev_next = adj.edges[&d.location];
            adj.edges.insert(d.location, Some(new_h));
            adj.edges.insert(Location::After(new_h), prev_next);
        }
        DeltaOp::UpdateAfter => {
            let Some(new_h) = d.hash else {
                return Err(DeltaError::HashFieldMismatch {
                    op: DeltaOp::UpdateAfter,
                    hash_present: false,
                });
            };
            // adj.edges[&loc] is safe: contains_key checked above.
            let old_h = adj.edges[&d.location].ok_or(DeltaError::NoSuccessor(d.location))?;
            // After(old_h) is a key whenever old_h is an element (adjacency list invariant).
            let next_after_old = adj
                .edges
                .remove(&Location::After(old_h))
                .ok_or(DeltaError::MissingEdge(Location::After(old_h)))?;
            adj.edges.insert(d.location, Some(new_h));
            adj.edges.insert(Location::After(new_h), next_after_old);
        }
        DeltaOp::DeleteAfter => {
            if d.hash.is_some() {
                return Err(DeltaError::HashFieldMismatch {
                    op: DeltaOp::DeleteAfter,
                    hash_present: true,
                });
            }
            // adj.edges[&loc] is safe: contains_key checked above.
            let old_h = adj.edges[&d.location].ok_or(DeltaError::NoSuccessor(d.location))?;
            // After(old_h) is a key whenever old_h is an element (adjacency list invariant).
            let next_after_old = adj
                .edges
                .remove(&Location::After(old_h))
                .ok_or(DeltaError::MissingEdge(Location::After(old_h)))?;
            adj.edges.insert(d.location, next_after_old);
        }
    }
    Ok(())
}

/// Apply each delta in `batch` to `adj`, in order.
///
/// Stops on the first error, leaving `adj` in a partial state — the only
/// caller is replay, where any error is fatal and the abandoned list
/// is dropped.
pub(crate) fn apply(adj: &mut AdjacencyList, batch: &[Delta]) -> Result<(), DeltaError> {
    for d in batch {
        apply_one(adj, d)?;
    }
    Ok(())
}

/// Encode a delta batch for the `journal.payload` column of a `type = 0` row.
///
/// Format: [`LATEST_DELTA_VERSION`] byte followed by `postcard::to_stdvec(batch)`.
pub(crate) fn encode_delta_batch(batch: &[Delta]) -> Result<Vec<u8>, postcard::Error> {
    let v1_batch: Vec<v1::DeltaV1> = batch.iter().map(v1::DeltaV1::from).collect();
    let payload = postcard::to_stdvec(&v1_batch)?;
    let mut bytes = Vec::with_capacity(1 + payload.len());
    bytes.push(LATEST_DELTA_VERSION);
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

/// Decode a delta batch from a `type = 0` row's `journal.payload`.
///
/// Peeks the leading version byte and dispatches to the per-version decoder.
pub(crate) fn decode_delta_batch(bytes: &[u8]) -> Result<Vec<Delta>, DecodeBatchError> {
    if bytes.is_empty() {
        return Err(DecodeBatchError::Empty);
    }
    match bytes[0] {
        1 => {
            let v1_batch: Vec<v1::DeltaV1> =
                postcard::from_bytes(&bytes[1..]).map_err(DecodeBatchError::Postcard)?;
            Ok(v1_batch.into_iter().map(Delta::from).collect())
        }
        v => Err(DecodeBatchError::UnknownVersion(v)),
    }
}

pub mod v1 {
    //! Frozen V1 wire schema.
    //!
    //! **Pre-1.0:** MAY be revised if implementation surfaces a missing or wrong
    //! field; every revision requires regenerating the pinned hex/hash tests and
    //! any committed G1 fixtures, and SHOULD bump `min_app_version`.
    //! **Post-1.0:** frozen indefinitely — no field reorders, no enum-variant
    //! reorders, no field insertions/deletions. Shape changes go through a new
    //! `mod v2`, bumping `LATEST_DELTA_VERSION`, and retaining v1 deserialization.

    use serde::{Deserialize, Serialize};

    use super::super::hash::Hash;

    /// Frozen V1 wire representation of a [`super::Delta`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DeltaV1 {
        /// Track this edit applies to.
        pub track_id: u32,
        /// Edit kind.
        pub op: DeltaOpV1,
        /// Edit location.
        pub location: LocationV1,
        /// New / replacing hash. `None` for `DeleteAfter`.
        pub hash: Option<Hash>,
    }

    /// Frozen V1 edit-kind enum.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum DeltaOpV1 {
        /// Insert immediately after location.
        InsertAfter,
        /// Replace element after location.
        UpdateAfter,
        /// Remove element after location.
        DeleteAfter,
    }

    /// Frozen V1 location enum.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum LocationV1 {
        /// Head of the track.
        Start,
        /// Immediately after the named hash.
        After(Hash),
    }
}

impl From<v1::DeltaOpV1> for DeltaOp {
    fn from(v: v1::DeltaOpV1) -> Self {
        match v {
            v1::DeltaOpV1::InsertAfter => DeltaOp::InsertAfter,
            v1::DeltaOpV1::UpdateAfter => DeltaOp::UpdateAfter,
            v1::DeltaOpV1::DeleteAfter => DeltaOp::DeleteAfter,
        }
    }
}

impl From<DeltaOp> for v1::DeltaOpV1 {
    fn from(v: DeltaOp) -> Self {
        match v {
            DeltaOp::InsertAfter => v1::DeltaOpV1::InsertAfter,
            DeltaOp::UpdateAfter => v1::DeltaOpV1::UpdateAfter,
            DeltaOp::DeleteAfter => v1::DeltaOpV1::DeleteAfter,
        }
    }
}

impl From<v1::LocationV1> for Location {
    fn from(v: v1::LocationV1) -> Self {
        match v {
            v1::LocationV1::Start => Location::Start,
            v1::LocationV1::After(h) => Location::After(h),
        }
    }
}

impl From<Location> for v1::LocationV1 {
    fn from(v: Location) -> Self {
        match v {
            Location::Start => v1::LocationV1::Start,
            Location::After(h) => v1::LocationV1::After(h),
        }
    }
}

impl From<v1::DeltaV1> for Delta {
    fn from(v: v1::DeltaV1) -> Self {
        Delta {
            track_id: v.track_id,
            op: v.op.into(),
            location: v.location.into(),
            hash: v.hash,
        }
    }
}

impl From<&Delta> for v1::DeltaV1 {
    fn from(v: &Delta) -> Self {
        v1::DeltaV1 {
            track_id: v.track_id,
            op: v.op.into(),
            location: v.location.into(),
            hash: v.hash,
        }
    }
}

#[cfg(test)]
impl AdjacencyList {
    /// Remove a key from the edge map to simulate an invariant violation in tests.
    fn remove_edge(&mut self, loc: Location) {
        self.edges.remove(&loc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::hash::hash_tagged;

    fn h(byte: u8) -> Hash {
        let mut bytes = [0u8; 16];
        bytes[0] = byte;
        Hash(bytes)
    }

    fn seq(adj: &AdjacencyList) -> Vec<Hash> {
        adj.iter().collect()
    }

    // Covers all three DeltaOp variants and both Location variants.
    fn sample_v1_batch() -> Vec<v1::DeltaV1> {
        vec![
            v1::DeltaV1 {
                track_id: 0,
                op: v1::DeltaOpV1::InsertAfter,
                location: v1::LocationV1::Start,
                hash: Some(h(1)),
            },
            v1::DeltaV1 {
                track_id: 1,
                op: v1::DeltaOpV1::UpdateAfter,
                location: v1::LocationV1::After(h(1)),
                hash: Some(h(2)),
            },
            v1::DeltaV1 {
                track_id: 0,
                op: v1::DeltaOpV1::DeleteAfter,
                location: v1::LocationV1::After(h(2)),
                hash: None,
            },
        ]
    }

    // --- AdjacencyList construction & queries ---

    #[test]
    fn empty_list_walks_to_empty_vec() {
        let adj = AdjacencyList::new();
        assert_eq!(seq(&adj), vec![]);
        assert!(adj.is_empty());
        assert_eq!(adj.len(), 0);
        assert_eq!(adj.head(), None);
    }

    #[test]
    fn from_sequence_round_trips() {
        let adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        assert_eq!(seq(&adj), vec![h(1), h(2), h(3)]);
        assert_eq!(adj.len(), 3);
        assert!(!adj.is_empty());
        assert_eq!(adj.head(), Some(h(1)));
    }

    #[test]
    fn from_sequence_empty() {
        let adj = AdjacencyList::from_sequence([]);
        assert_eq!(seq(&adj), vec![]);
        assert!(adj.is_empty());
        assert_eq!(adj.len(), 0);
    }

    #[test]
    fn successor_at_start_returns_head() {
        let adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        assert_eq!(adj.successor(&Location::Start), Some(Some(h(1))));
        assert_eq!(adj.successor(&Location::Start).flatten(), adj.head());

        let empty = AdjacencyList::new();
        assert_eq!(empty.successor(&Location::Start), Some(None));
    }

    #[test]
    fn successor_at_terminal_returns_some_none() {
        let adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        assert_eq!(adj.successor(&Location::After(h(3))), Some(None));
    }

    #[test]
    fn successor_at_invalid_returns_none() {
        let adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        assert_eq!(adj.successor(&Location::After(h(99))), None);
    }

    // --- apply per variant ---

    #[test]
    fn insert_after_start_on_empty_list() {
        let mut adj = AdjacencyList::new();
        apply(&mut adj, &[Delta::insert_after(0, Location::Start, h(1))]).unwrap();
        assert_eq!(seq(&adj), vec![h(1)]);
    }

    #[test]
    fn insert_after_start_on_nonempty_list() {
        let mut adj = AdjacencyList::from_sequence([h(2), h(3)]);
        apply(&mut adj, &[Delta::insert_after(0, Location::Start, h(1))]).unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(2), h(3)]);
    }

    #[test]
    fn insert_after_middle() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(3)]);
        apply(
            &mut adj,
            &[Delta::insert_after(0, Location::After(h(1)), h(2))],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(2), h(3)]);
    }

    #[test]
    fn insert_after_terminal_appends() {
        let mut adj = AdjacencyList::from_sequence([h(1)]);
        apply(
            &mut adj,
            &[Delta::insert_after(0, Location::After(h(1)), h(2))],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(2)]);
    }

    #[test]
    fn update_after_start() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        apply(&mut adj, &[Delta::update_after(0, Location::Start, h(9))]).unwrap();
        assert_eq!(seq(&adj), vec![h(9), h(2)]);
    }

    #[test]
    fn update_after_middle_preserves_tail() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        apply(
            &mut adj,
            &[Delta::update_after(0, Location::After(h(1)), h(9))],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(9), h(3)]);
    }

    #[test]
    fn update_after_terminal() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        apply(
            &mut adj,
            &[Delta::update_after(0, Location::After(h(1)), h(9))],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(9)]);
    }

    #[test]
    fn delete_after_start_singleton() {
        let mut adj = AdjacencyList::from_sequence([h(1)]);
        apply(&mut adj, &[Delta::delete_after(0, Location::Start)]).unwrap();
        assert_eq!(seq(&adj), vec![]);
    }

    #[test]
    fn delete_after_start_two_elements() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        apply(&mut adj, &[Delta::delete_after(0, Location::Start)]).unwrap();
        assert_eq!(seq(&adj), vec![h(2)]);
    }

    #[test]
    fn delete_after_middle() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        apply(&mut adj, &[Delta::delete_after(0, Location::After(h(1)))]).unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(3)]);
    }

    #[test]
    fn delete_after_predecessor_of_terminal() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        apply(&mut adj, &[Delta::delete_after(0, Location::After(h(1)))]).unwrap();
        assert_eq!(seq(&adj), vec![h(1)]);
    }

    // --- batch behaviour ---

    #[test]
    fn empty_batch_is_noop() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        apply(&mut adj, &[]).unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(2)]);
    }

    #[test]
    fn intra_batch_forward_reference() {
        // Later delta references a hash produced by an earlier one in the same batch.
        let mut adj = AdjacencyList::from_sequence([h(1)]);
        apply(
            &mut adj,
            &[
                Delta::insert_after(0, Location::After(h(1)), h(2)),
                Delta::insert_after(0, Location::After(h(2)), h(3)),
            ],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(2), h(3)]);
    }

    #[test]
    fn intra_batch_update_then_reference_new_hash() {
        // After UpdateAfter replaces h(2) with h(9), the second delta can reference h(9).
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        apply(
            &mut adj,
            &[
                Delta::update_after(0, Location::After(h(1)), h(9)),
                Delta::insert_after(0, Location::After(h(9)), h(10)),
            ],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(9), h(10)]);
    }

    #[test]
    fn mixed_kinds_batch() {
        // Delete h(2), insert h(9) where h(2) was, then replace h(3) with h(10).
        let mut adj = AdjacencyList::from_sequence([h(1), h(2), h(3)]);
        apply(
            &mut adj,
            &[
                Delta::delete_after(0, Location::After(h(1))),
                Delta::insert_after(0, Location::After(h(1)), h(9)),
                Delta::update_after(0, Location::After(h(9)), h(10)),
            ],
        )
        .unwrap();
        assert_eq!(seq(&adj), vec![h(1), h(9), h(10)]);
    }

    // --- error cases ---

    #[test]
    fn insert_after_unknown_location() {
        let mut adj = AdjacencyList::new();
        let err = apply(
            &mut adj,
            &[Delta::insert_after(0, Location::After(h(1)), h(2))],
        )
        .unwrap_err();
        assert!(matches!(err, DeltaError::LocationNotFound(x) if x == h(1)));
    }

    #[test]
    fn update_after_start_on_empty_list() {
        let mut adj = AdjacencyList::new();
        let err = apply(&mut adj, &[Delta::update_after(0, Location::Start, h(1))]).unwrap_err();
        assert!(matches!(err, DeltaError::NoSuccessor(Location::Start)));
    }

    #[test]
    fn update_after_terminal_no_successor() {
        let mut adj = AdjacencyList::from_sequence([h(1)]);
        let err = apply(
            &mut adj,
            &[Delta::update_after(0, Location::After(h(1)), h(9))],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DeltaError::NoSuccessor(Location::After(x)) if x == h(1)
        ));
    }

    #[test]
    fn delete_after_start_on_empty_list() {
        let mut adj = AdjacencyList::new();
        let err = apply(&mut adj, &[Delta::delete_after(0, Location::Start)]).unwrap_err();
        assert!(matches!(err, DeltaError::NoSuccessor(Location::Start)));
    }

    #[test]
    fn delete_after_terminal_no_successor() {
        let mut adj = AdjacencyList::from_sequence([h(1)]);
        let err = apply(&mut adj, &[Delta::delete_after(0, Location::After(h(1)))]).unwrap_err();
        assert!(matches!(
            err,
            DeltaError::NoSuccessor(Location::After(x)) if x == h(1)
        ));
    }

    #[test]
    fn hash_field_mismatch_insert_missing_hash() {
        let mut adj = AdjacencyList::new();
        let d = Delta {
            op: DeltaOp::InsertAfter,
            hash: None,
            location: Location::Start,
            track_id: 0,
        };
        let err = apply(&mut adj, &[d]).unwrap_err();
        assert!(matches!(
            err,
            DeltaError::HashFieldMismatch {
                op: DeltaOp::InsertAfter,
                hash_present: false
            }
        ));
    }

    #[test]
    fn hash_field_mismatch_delete_extra_hash() {
        let mut adj = AdjacencyList::from_sequence([h(1)]);
        let d = Delta {
            op: DeltaOp::DeleteAfter,
            hash: Some(h(1)),
            location: Location::Start,
            track_id: 0,
        };
        let err = apply(&mut adj, &[d]).unwrap_err();
        assert!(matches!(
            err,
            DeltaError::HashFieldMismatch {
                op: DeltaOp::DeleteAfter,
                hash_present: true
            }
        ));
    }

    // --- MissingEdge invariant violations ---

    #[test]
    fn missing_edge_start() {
        let mut adj = AdjacencyList::new();
        adj.remove_edge(Location::Start);
        let err = apply(&mut adj, &[Delta::insert_after(0, Location::Start, h(1))]).unwrap_err();
        assert!(matches!(err, DeltaError::MissingEdge(Location::Start)));
    }

    #[test]
    fn missing_edge_update_after() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        adj.remove_edge(Location::After(h(2)));
        let err = apply(
            &mut adj,
            &[Delta::update_after(0, Location::After(h(1)), h(9))],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DeltaError::MissingEdge(Location::After(x)) if x == h(2)
        ));
    }

    #[test]
    fn missing_edge_delete_after() {
        let mut adj = AdjacencyList::from_sequence([h(1), h(2)]);
        adj.remove_edge(Location::After(h(2)));
        let err = apply(&mut adj, &[Delta::delete_after(0, Location::After(h(1)))]).unwrap_err();
        assert!(matches!(
            err,
            DeltaError::MissingEdge(Location::After(x)) if x == h(2)
        ));
    }

    // --- encode / decode ---

    #[test]
    fn encode_round_trip() {
        let batch = vec![
            Delta::insert_after(0, Location::Start, h(0xAB)),
            Delta::update_after(1, Location::After(h(0xAB)), h(0xCD)),
            Delta::delete_after(0, Location::After(h(0xCD))),
        ];
        let encoded = encode_delta_batch(&batch).unwrap();
        let decoded = decode_delta_batch(&encoded).unwrap();
        assert_eq!(decoded, batch);
    }

    #[test]
    fn encode_empty_batch() {
        let encoded = encode_delta_batch(&[]).unwrap();
        assert_eq!(encoded[0], LATEST_DELTA_VERSION);
        let decoded = decode_delta_batch(&encoded).unwrap();
        assert_eq!(decoded, vec![]);
    }

    #[test]
    fn leading_byte_is_latest_version() {
        let batch = vec![Delta::insert_after(0, Location::Start, h(1))];
        let encoded = encode_delta_batch(&batch).unwrap();
        assert_eq!(encoded[0], LATEST_DELTA_VERSION);
        assert_eq!(LATEST_DELTA_VERSION, 1);
    }

    #[test]
    fn decode_empty_input() {
        let err = decode_delta_batch(&[]).unwrap_err();
        assert!(matches!(err, DecodeBatchError::Empty));
    }

    #[test]
    fn decode_unknown_version() {
        let err = decode_delta_batch(&[0xFF, 0x00]).unwrap_err();
        assert!(matches!(err, DecodeBatchError::UnknownVersion(0xFF)));
    }

    #[test]
    fn decode_truncated() {
        // Version byte only — no postcard body; postcard needs at least 1 byte for length.
        let err = decode_delta_batch(&[0x01]).unwrap_err();
        assert!(matches!(err, DecodeBatchError::Postcard(_)));
    }

    #[test]
    fn v1_conversions_total_round_trip() {
        for d in &[
            Delta::insert_after(0, Location::Start, h(1)),
            Delta::update_after(1, Location::After(h(1)), h(2)),
            Delta::delete_after(0, Location::After(h(2))),
        ] {
            assert_eq!(*d, Delta::from(v1::DeltaV1::from(d)));
        }
    }

    #[test]
    fn v1_wire_format_pinned() {
        let batch: Vec<Delta> = sample_v1_batch().into_iter().map(Delta::from).collect();
        let encoded = encode_delta_batch(&batch).unwrap();
        let expected: &[u8] = &PINNED_WIRE_BYTES;
        assert_eq!(
            encoded.as_slice(),
            expected,
            "V1 wire format changed — regenerate via capture_pinned_values"
        );
    }

    #[test]
    fn v1_wire_hash_pinned() {
        let batch: Vec<Delta> = sample_v1_batch().into_iter().map(Delta::from).collect();
        let encoded = encode_delta_batch(&batch).unwrap();
        // hash_tagged used as a stable byte-level fingerprint; deltas are not
        // stored by hash in the content-addressed store.
        let hash = hash_tagged(&encoded);
        assert_eq!(
            hash.0, PINNED_HASH,
            "V1 wire hash changed — regenerate via capture_pinned_values"
        );
    }

    // --- cross-cutting ---

    #[test]
    fn mixed_track_ids_coexist_in_batch() {
        // Pins the kind-agnostic contract: delta.rs never inspects track_id for routing.
        // The test wrapper splits by track_id and drives each list independently,
        // mirroring what the engine does.
        let batch = vec![
            Delta::insert_after(0, Location::Start, h(1)),
            Delta::insert_after(1, Location::Start, h(10)),
            Delta::insert_after(0, Location::After(h(1)), h(2)),
            Delta::insert_after(1, Location::After(h(10)), h(11)),
        ];

        let mut track0 = AdjacencyList::new();
        let mut track1 = AdjacencyList::new();

        for d in &batch {
            if d.track_id == 0 {
                apply_one(&mut track0, d).unwrap();
            } else {
                apply_one(&mut track1, d).unwrap();
            }
        }

        assert_eq!(seq(&track0), vec![h(1), h(2)]);
        assert_eq!(seq(&track1), vec![h(10), h(11)]);
    }

    // Pinned bytes and hash for sample_v1_batch(). Regenerate via capture_pinned_values.
    // Captured after the implementation was stable; re-run if DeltaV1 shape changes.
    const PINNED_WIRE_BYTES: [u8; 78] = [
        0x01, 0x03, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
        0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00,
    ];
    const PINNED_HASH: [u8; 16] = [
        0x95, 0x5b, 0x35, 0x42, 0x04, 0xa9, 0xe9, 0xe3, 0x9d, 0x2d, 0xdb, 0x29, 0xa2, 0x2d, 0x9e,
        0x1c,
    ];

    // Run with: cargo test -p core delta::tests::capture_pinned_values -- --ignored --nocapture
    #[test]
    #[ignore]
    fn capture_pinned_values() {
        let batch: Vec<Delta> = sample_v1_batch().into_iter().map(Delta::from).collect();
        let encoded = encode_delta_batch(&batch).unwrap();
        let hash = hash_tagged(&encoded);
        println!("PINNED_WIRE_BYTES len={}", encoded.len());
        print!("const PINNED_WIRE_BYTES: [u8; {}] = [", encoded.len());
        for (i, b) in encoded.iter().enumerate() {
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
}
