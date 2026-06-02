//! Undo/redo machinery for the project editor.
//!
//! # Producer / consumer split
//! The engine (`apply_batch`, Step 11) is the **producer**: it builds an
//! [`UndoEntry`] from a real edit and calls [`History::record`]. This module
//! is the **consumer**: [`History::undo`] / [`History::redo`] swap the live
//! `Arc<TimelineState>` and append the inverse/forward journal effect so that
//! replay on reopen reproduces the post-undo state.
//!
//! # "Undo is a forward-recorded edit"
//! An undo appends the *inverse* effect rows to the journal (the same
//! `type = 0` / `type = -1` rows a forward edit writes, but with the
//! inverse deltas and the pre-edit metadata blob). Replay therefore lands
//! on the post-undo state automatically. Redo appends the forward effect
//! again. Clearing redo touches no journal.

use std::collections::VecDeque;
use std::sync::Arc;

use rusqlite::Connection;

use crate::db::journal::{append_delta_batch, append_metadata, JournalError};
use crate::db::store;
use crate::db::store::StoreError;
use crate::project::command_id::CommandId;
use crate::project::delta::{encode_delta_batch, Delta};
use crate::project::metadata::{encode_metadata, Metadata};
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

/// Errors from undo/redo operations.
#[derive(Debug)]
pub(crate) enum HistoryError {
    /// Encoding a delta batch failed.
    Encode(postcard::Error),
    /// Storing a metadata blob failed.
    Store(StoreError),
    /// Appending a journal row failed.
    Journal(JournalError),
    /// A transaction begin/commit failed.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Encode(e) => {
                write!(f, "failed to encode delta batch for undo/redo: {e}")
            }
            HistoryError::Store(e) => {
                write!(f, "failed to store metadata blob for undo/redo: {e}")
            }
            HistoryError::Journal(e) => {
                write!(f, "failed to append journal row for undo/redo: {e}")
            }
            HistoryError::Sqlite(e) => write!(f, "transaction error during undo/redo: {e}"),
        }
    }
}

impl std::error::Error for HistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HistoryError::Encode(e) => Some(e),
            HistoryError::Store(e) => Some(e),
            HistoryError::Journal(e) => Some(e),
            HistoryError::Sqlite(e) => Some(e),
        }
    }
}

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

impl History {
    /// New history with the given undo-depth limit. `0` disables recording (no
    /// edit is undoable).
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit,
        }
    }

    /// Record a freshly-applied edit. Clears redo (a new edit invalidates it) and
    /// evicts the oldest undo entry while over `limit`. No journal action — the
    /// forward row(s) were already written by the producer (`apply_batch`).
    #[allow(dead_code)] // apply_batch is the direct caller; its non-test callers arrive in M4/M5.
    pub(crate) fn record(&mut self, entry: UndoEntry) {
        if self.limit == 0 {
            self.redo.clear();
            return;
        }
        self.undo.push_back(entry);
        while self.undo.len() > self.limit {
            self.undo.pop_front();
        }
        self.redo.clear();
    }

    /// Undo the most recent edit. `Ok(false)` if the undo stack is empty.
    pub(crate) fn undo(
        &mut self,
        current: &mut Arc<TimelineState>,
        conn: &mut Connection,
        applied_at: i64,
    ) -> Result<bool, HistoryError> {
        let entry = match self.undo.pop_back() {
            Some(e) => e,
            None => return Ok(false),
        };
        match append_effect(
            conn,
            entry.inverse_delta.as_deref(),
            entry.metadata_changed,
            &entry.before.metadata,
            entry.category.undo_of(),
            applied_at,
        ) {
            Ok(()) => {
                *current = entry.before.clone();
                self.redo.push(entry);
                Ok(true)
            }
            Err(e) => {
                self.undo.push_back(entry);
                Err(e)
            }
        }
    }

    /// Redo the most recently undone edit. `Ok(false)` if redo is empty.
    pub(crate) fn redo(
        &mut self,
        current: &mut Arc<TimelineState>,
        conn: &mut Connection,
        applied_at: i64,
    ) -> Result<bool, HistoryError> {
        let entry = match self.redo.pop() {
            Some(e) => e,
            None => return Ok(false),
        };
        match append_effect(
            conn,
            entry.forward_delta.as_deref(),
            entry.metadata_changed,
            &entry.after.metadata,
            entry.category,
            applied_at,
        ) {
            Ok(()) => {
                *current = entry.after.clone();
                self.undo.push_back(entry);
                // Redo pushes back onto undo; guard against exceeding limit in case
                // the caller somehow drove undo below capacity and then redoes past it.
                while self.undo.len() > self.limit {
                    self.undo.pop_front();
                }
                Ok(true)
            }
            Err(e) => {
                self.redo.push(entry);
                Err(e)
            }
        }
    }

    /// Returns `true` if there is at least one undoable edit.
    #[allow(dead_code)] // M4/M5: Tauri undo/redo command surfaces UI can-undo state.
    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Returns `true` if there is at least one redoable edit.
    #[allow(dead_code)] // M4/M5: Tauri undo/redo command surfaces UI can-redo state.
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

/// Append the journal effect for one undo or redo operation, atomically.
///
/// Writes 0–2 rows: an optional `type = 0` delta row and/or a `type = -1`
/// metadata row, both stamped with `stamp`. The metadata blob is re-derived from
/// `metadata` and stored idempotently (`INSERT OR IGNORE`).
fn append_effect(
    conn: &mut Connection,
    delta: Option<&[Delta]>,
    metadata_changed: bool,
    metadata: &Metadata,
    stamp: CommandId,
    applied_at: i64,
) -> Result<(), HistoryError> {
    let tx = conn.transaction().map_err(HistoryError::Sqlite)?;
    if let Some(batch) = delta {
        let payload = encode_delta_batch(batch).map_err(HistoryError::Encode)?;
        append_delta_batch(&tx, stamp, &payload, applied_at).map_err(HistoryError::Journal)?;
    }
    if metadata_changed {
        let (h, bytes) = encode_metadata(metadata).map_err(HistoryError::Encode)?;
        store::put(&tx, &h, &bytes).map_err(HistoryError::Store)?;
        append_metadata(&tx, stamp, &h, applied_at).map_err(HistoryError::Journal)?;
    }
    tx.commit().map_err(HistoryError::Sqlite)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;
    use crate::db::journal::append_metadata;
    use crate::db::{store, Db};
    use crate::project::command_id::CommandId;
    use crate::project::delta::{encode_delta_batch, Delta, Location};
    use crate::project::hash::Hash;
    use crate::project::metadata::{encode_metadata, load_current_metadata, Metadata};
    use crate::project::snapshot::{
        encode_snapshot, load_and_replay, snapshot_from_trees, PerTrackTrees, TrackTree,
    };
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{decode_turn, encode_turn, Turn};

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

    fn speech_tree(conn: &Connection, seq: Vec<Hash>) -> TrackTree {
        let mut elements = Vec::with_capacity(seq.len());
        for h in seq {
            let bytes = store::get(conn, &h).unwrap();
            let turn = decode_turn(&bytes).unwrap();
            elements.push((h, Arc::new(turn)));
        }
        TrackTree::Speech(ImplicitTimelineTree::from_sorted_elements(elements))
    }

    fn state(trees: PerTrackTrees, metadata: Metadata) -> Arc<TimelineState> {
        Arc::new(TimelineState { trees, metadata })
    }

    fn write_snapshot_row(db: &Db, snap: &crate::project::snapshot::Snapshot) -> i64 {
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

    fn trivial_entry(before: Arc<TimelineState>, after: Arc<TimelineState>) -> UndoEntry {
        UndoEntry {
            before,
            after,
            forward_delta: None,
            inverse_delta: None,
            metadata_changed: false,
            category: CommandId::Unknown,
        }
    }

    fn make_track_meta(id: u32, name: &str) -> crate::project::metadata::TrackMeta {
        use crate::project::metadata::{ModelUse, SourceType, TrackMeta};
        TrackMeta {
            id,
            name: name.to_string(),
            source_type: SourceType::File,
            source_path_relative: String::new(),
            source_path_absolute: String::new(),
            resampled_path: None,
            codec: "wav".to_string(),
            source_sample_rate: 48000,
            source_channels: 1,
            project_start_sample: 0,
            original_length_samples: 0,
            cut_length_samples: 0,
            drift_ppm: 0.0,
            room_tone_hash: None,
            room_tone_length_samples: None,
            models_used: ModelUse::default(),
            enhanced_path: None,
            wet_dry_ratio: 0.0,
            disfluencies_identified: false,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    // ── Unit tests (stack transitions) ───────────────────────────────────────

    // U1
    #[test]
    fn record_pushes_and_clears_redo() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);
        let s = Arc::new(TimelineState::default());

        history.record(trivial_entry(s.clone(), s.clone()));
        assert!(history.can_undo());
        assert!(!history.can_redo());

        // Force a redo entry by undoing
        let mut tmp = Arc::clone(&s);
        history.undo(&mut tmp, db.conn_mut(), 0).unwrap();
        assert!(!history.can_undo());
        assert!(history.can_redo());

        // record clears redo
        history.record(trivial_entry(s.clone(), s.clone()));
        assert!(history.can_undo());
        assert!(!history.can_redo());
    }

    // U2
    #[test]
    fn undo_then_redo_round_trips_stacks() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);
        let s = Arc::new(TimelineState::default());
        let mut current = Arc::clone(&s);

        history.record(trivial_entry(s.clone(), s.clone()));
        assert!(history.can_undo());
        assert!(!history.can_redo());

        let did_undo = history.undo(&mut current, db.conn_mut(), 0).unwrap();
        assert!(did_undo);
        assert!(!history.can_undo());
        assert!(history.can_redo());

        let did_redo = history.redo(&mut current, db.conn_mut(), 0).unwrap();
        assert!(did_redo);
        assert!(history.can_undo());
        assert!(!history.can_redo());
    }

    // U3
    #[test]
    fn undo_empty_is_noop() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);
        let mut current = Arc::new(TimelineState::default());

        let count_before: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();

        let result = history.undo(&mut current, db.conn_mut(), 0).unwrap();
        assert!(!result);
        assert!(!history.can_undo());
        assert!(!history.can_redo());

        let count_after: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_before, count_after, "no journal row appended");
    }

    // U4
    #[test]
    fn redo_empty_is_noop() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);
        let mut current = Arc::new(TimelineState::default());

        let count_before: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();

        let result = history.redo(&mut current, db.conn_mut(), 0).unwrap();
        assert!(!result);

        let count_after: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_before, count_after, "no journal row appended");
    }

    // U5
    #[test]
    fn new_edit_after_undo_discards_redo() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);
        let s = Arc::new(TimelineState::default());
        let mut current = Arc::clone(&s);

        history.record(trivial_entry(s.clone(), s.clone())); // A
        history.undo(&mut current, db.conn_mut(), 0).unwrap();
        assert!(history.can_redo());

        history.record(trivial_entry(s.clone(), s.clone())); // B — clears redo
        assert!(!history.can_redo(), "redo must be empty after new record");
        assert!(history.can_undo(), "undo must have B");
    }

    // U6
    #[test]
    fn oldest_evicted_past_limit() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(2);
        let s = Arc::new(TimelineState::default());
        let mut current = Arc::clone(&s);

        history.record(trivial_entry(s.clone(), s.clone()));
        history.record(trivial_entry(s.clone(), s.clone()));
        history.record(trivial_entry(s.clone(), s.clone()));

        // Only 2 retained; undo succeeds twice then returns Ok(false)
        assert!(
            history.undo(&mut current, db.conn_mut(), 0).unwrap(),
            "first undo"
        );
        assert!(
            history.undo(&mut current, db.conn_mut(), 0).unwrap(),
            "second undo"
        );
        assert!(
            !history.undo(&mut current, db.conn_mut(), 0).unwrap(),
            "third undo — evicted"
        );
    }

    // U7
    #[test]
    fn zero_limit_disables_recording() {
        let mut history = History::new(0);
        let s = Arc::new(TimelineState::default());

        history.record(trivial_entry(s.clone(), s.clone()));
        assert!(!history.can_undo(), "limit=0 must disable recording");
        assert!(!history.can_redo());
    }

    // U8 — HistoryError Display is non-empty; every variant chains source().
    #[test]
    fn history_error_display_and_source() {
        use std::error::Error;

        use crate::db::journal::JournalError;
        use crate::db::store::StoreError;

        let pe: postcard::Error = postcard::from_bytes::<u32>(&[]).unwrap_err();
        let se: rusqlite::Error = rusqlite::Error::QueryReturnedNoRows;

        let variants = vec![
            HistoryError::Encode(pe),
            HistoryError::Store(StoreError::NotFound(Hash([0u8; 16]))),
            HistoryError::Journal(JournalError::MalformedHashPayload { id: 1, len: 5 }),
            HistoryError::Sqlite(se),
        ];
        for v in &variants {
            assert!(
                !v.to_string().is_empty(),
                "Display must be non-empty for {v:?}"
            );
            assert!(v.source().is_some(), "source() must chain for {v:?}");
        }
    }

    // U9 — redo at full capacity must not evict (catches > vs == vs >= mutations).
    #[test]
    fn redo_at_capacity_does_not_evict() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(2);
        let s = Arc::new(TimelineState::default());
        let mut current = Arc::clone(&s);

        // Fill undo to capacity
        history.record(trivial_entry(s.clone(), s.clone())); // A
        history.record(trivial_entry(s.clone(), s.clone())); // B

        // Undo both — undo is empty, redo=[B, A]
        assert!(history.undo(&mut current, db.conn_mut(), 0).unwrap());
        assert!(history.undo(&mut current, db.conn_mut(), 0).unwrap());

        // Redo both — must restore undo to [A, B] with no eviction
        assert!(
            history.redo(&mut current, db.conn_mut(), 0).unwrap(),
            "redo A"
        );
        assert!(
            history.redo(&mut current, db.conn_mut(), 0).unwrap(),
            "redo B"
        );

        // Both entries are still in undo — a spurious eviction would cause one to fail
        assert!(
            history.undo(&mut current, db.conn_mut(), 0).unwrap(),
            "undo B"
        );
        assert!(
            history.undo(&mut current, db.conn_mut(), 0).unwrap(),
            "undo A"
        );
        assert!(
            !history.undo(&mut current, db.conn_mut(), 0).unwrap(),
            "stack empty"
        );
    }

    // ── Integration tests (replay-after-undo) ────────────────────────────────

    // I1 — tree-only edit: undo restores trees; replay reproduces post-undo state.
    #[test]
    fn undo_replays_to_post_undo_state() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);

        let h_a = put_turn(&db, 1, 100, 0);
        let h_b = put_turn(&db, 2, 200, 0);
        let h_c = put_turn(&db, 3, 300, 0);

        let mut trees0 = PerTrackTrees::new();
        trees0.insert(1, speech_tree(db.conn(), vec![h_a, h_b, h_c]));
        let before = state(trees0, Metadata::default());

        let snap0 = snapshot_from_trees(&before.trees);
        write_snapshot_row(&db, &snap0);

        // Synthesize edit: update B → B'
        let h_b2 = put_turn(&db, 22, 222, 0);
        let mut trees1 = PerTrackTrees::new();
        trees1.insert(1, speech_tree(db.conn(), vec![h_a, h_b2, h_c]));
        let after = state(trees1, Metadata::default());

        let forward = vec![Delta::update_after(1, Location::After(h_a), h_b2)];
        let inverse = vec![Delta::update_after(1, Location::After(h_a), h_b)];

        // Append the forward row (what apply_batch would do)
        let payload = encode_delta_batch(&forward).unwrap();
        crate::db::journal::append_delta_batch(db.conn(), CommandId::Unknown, &payload, 0).unwrap();

        let mut current = after.clone();
        history.record(UndoEntry {
            before: before.clone(),
            after: after.clone(),
            forward_delta: Some(forward),
            inverse_delta: Some(inverse),
            metadata_changed: false,
            category: CommandId::Unknown,
        });

        // Undo
        assert!(history.undo(&mut current, db.conn_mut(), 0).unwrap());
        assert_eq!(*current, *before, "state must revert to before");

        // Last journal row is the inverse delta stamped with undo_of(Unknown) = Undo
        let (row_type, row_cmd): (i64, i64) = db
            .conn()
            .query_row(
                "SELECT type, command_id FROM journal ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_type, 0, "inverse row must be type=0");
        assert_eq!(
            row_cmd,
            CommandId::Unknown.undo_of().code(),
            "stamped with undo_of(Unknown)"
        );

        // Replay reproduces the before trees
        let replayed = load_and_replay(&db, None).unwrap();
        assert_eq!(
            replayed, before.trees,
            "replay must match before.trees after undo"
        );

        // Redo restores after state and replay agrees
        assert!(history.redo(&mut current, db.conn_mut(), 0).unwrap());
        assert_eq!(*current, *after, "state must restore to after");
        let replayed2 = load_and_replay(&db, None).unwrap();
        assert_eq!(
            replayed2, after.trees,
            "replay must match after.trees after redo"
        );
    }

    // I2 — metadata-only edit: undo restores metadata; load_current_metadata agrees.
    #[test]
    fn undo_replays_to_post_undo_metadata() {
        let (_dir, mut db) = open_tmp_db();
        let mut history = History::new(10);

        // Empty trees; initial snapshot
        let trees0 = PerTrackTrees::new();
        let snap0 = snapshot_from_trees(&trees0);
        write_snapshot_row(&db, &snap0);

        // meta0: track named "a"
        let meta0 = Metadata {
            project: Default::default(),
            tracks: vec![make_track_meta(1, "a")],
            speakers: vec![],
        };
        let (h0, b0) = encode_metadata(&meta0).unwrap();
        store::put(db.conn(), &h0, &b0).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h0, 0).unwrap();

        let before = state(trees0.clone(), meta0.clone());

        // meta1: rename "a" → "b"
        let meta1 = Metadata {
            project: Default::default(),
            tracks: vec![make_track_meta(1, "b")],
            speakers: vec![],
        };
        let (h1, b1) = encode_metadata(&meta1).unwrap();
        store::put(db.conn(), &h1, &b1).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h1, 0).unwrap();

        let after = state(trees0, meta1.clone());

        history.record(UndoEntry {
            before: before.clone(),
            after: after.clone(),
            forward_delta: None,
            inverse_delta: None,
            metadata_changed: true,
            category: CommandId::Unknown,
        });

        let mut current = after.clone();

        // Undo
        assert!(history.undo(&mut current, db.conn_mut(), 0).unwrap());
        assert_eq!(current.metadata, meta0, "metadata must revert to meta0");

        // Last journal row is type=-1 (metadata), no type=0 row appended by undo
        let (row_type, row_cmd): (i64, i64) = db
            .conn()
            .query_row(
                "SELECT type, command_id FROM journal ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_type, -1, "inverse row must be type=-1 (metadata)");
        assert_eq!(row_cmd, CommandId::Unknown.undo_of().code());

        let loaded = load_current_metadata(&db, None).unwrap();
        assert_eq!(
            loaded, meta0,
            "load_current_metadata must return meta0 after undo"
        );

        // Redo restores meta1
        assert!(history.redo(&mut current, db.conn_mut(), 0).unwrap());
        assert_eq!(current.metadata, meta1, "metadata must restore to meta1");
        let loaded2 = load_current_metadata(&db, None).unwrap();
        assert_eq!(
            loaded2, meta1,
            "load_current_metadata must return meta1 after redo"
        );
    }
}
