//! `ProjectState` engine: owns an open project's database, live immutable state,
//! undo/redo history, and snapshot writer.
//!
//! # Producer / consumer split
//! The engine is the **producer**: `apply_batch` applies edits, captures
//! forward+inverse deltas, and calls [`crate::project::undo::History::record`]. The
//! [`crate::project::undo::History`] consumer drives undo/redo by appending the
//! inverse/forward journal effects.
//!
//! # "Undo is a forward-recorded edit"
//! Undo appends the *inverse* delta batch to the journal; replay on reopen reproduces
//! the post-undo state automatically. Redo appends the forward batch again.
//!
//! # Single-writer + lock-free handoff
//! [`SnapshotWriter`] owns a dedicated [`crate::db::Db`] handle to the same project file.
//! WAL allows concurrent readers; writers serialize via SQLite's write lock. The
//! expensive work (flatten + BLAKE3 + postcard) runs without holding any connection;
//! only the final two-INSERT transaction holds the write lock (single-digit ms). From
//! M5 on, a synchronous main-thread edit can race the snapshot writer; `busy_timeout =
//! 5000` makes the loser wait and retry rather than fail. The cpal audio callback never
//! touches SQLite; playback latency is unaffected.
//!
//! M1 implementation note: the writer is synchronous (no idle-autosave timer). The
//! connection-ownership and lock-free-handoff shape is in place for M5 to add the
//! idle timer without structural changes.
//!
//! # Open-recovery contract
//! If the journal tail is corrupt (`load_and_replay` fails), `open_project` falls back
//! to `load_latest_snapshot`. If the fallback also fails the open is fatal and no
//! `ProjectState` is constructed. The caller **must** surface a warning when
//! [`OpenOutcome::recovery`] is `Some` — silently ignoring it is silent data loss
//! (post-snapshot edits were dropped).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::audio::room_tone::{decode_room_tone, RoomTone};
use crate::db::journal::{self, JournalError};
use crate::db::project as db_project;
use crate::db::store::{self, StoreError};
use crate::db::{Db, DbOpenError};
use crate::project::command_id::CommandId;
use crate::project::delta::{encode_delta_batch, Delta, Location};
use crate::project::hash::{DecodeError, Hash};
use crate::project::label::Label;
use crate::project::metadata::{
    encode_metadata, load_current_metadata, missing_tracks, resolve_track_source, FileResolution,
    Metadata, MetadataLoadError,
};
use crate::project::snapshot::{
    encode_snapshot, load_and_replay, load_latest_snapshot, snapshot_from_trees, PerTrackTrees,
    ReplayError, Snapshot, TrackTree,
};
use crate::project::tilable::Tilable;
use crate::project::tree::{ImplicitTimelineTree, TreeError};
use crate::project::turn::Turn;
use crate::project::undo::{History, HistoryError, TimelineState, UndoEntry};
use crate::settings::Settings;

/// Errors returned by [`ProjectState`] operations.
#[allow(private_interfaces)]
#[derive(Debug)]
pub enum EngineError {
    /// A raw SQLite error.
    Sqlite(rusqlite::Error),
    /// A blob-store error.
    Store(StoreError),
    /// A journal I/O error.
    Journal(JournalError),
    /// An undo/redo history error.
    History(HistoryError),
    /// A timeline replay error (open path).
    Replay(ReplayError),
    /// A serialization error.
    Encode(postcard::Error),
    /// A blob decode error.
    Decode(DecodeError),
    /// Database open or migration failed (version mismatch, filesystem error, etc.).
    OpenDb(Box<dyn std::error::Error + Send + Sync>),
    /// The journal tail was corrupt AND the fallback recovery snapshot also failed.
    ///
    /// Both failures together are unrecoverable; no [`ProjectState`] was constructed.
    /// This is the dedicated error variant for the open-recovery path.
    RecoveryFailed(ReplayError),
    /// A tree mutation error (sample out of range, not on element boundary, etc.).
    Tree(TreeError),
    /// An operation's element type does not match the target track's tree kind,
    /// or an update or delete targets an absent track.
    TrackTypeMismatch {
        /// The affected track id.
        track_id: u32,
    },
    /// `new_project` was called but the project file already exists at this path.
    ProjectFileExists {
        /// The path that already exists.
        path: PathBuf,
    },
    /// `open_project` was called but no project file exists at this path.
    ProjectFileNotFound {
        /// The path that was not found.
        path: PathBuf,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            EngineError::Store(e) => write!(f, "store error: {e}"),
            EngineError::Journal(e) => write!(f, "journal error: {e}"),
            EngineError::History(e) => write!(f, "undo/redo error: {e}"),
            EngineError::Replay(e) => write!(f, "timeline replay error: {e}"),
            EngineError::Encode(e) => write!(f, "encoding error: {e}"),
            EngineError::Decode(e) => write!(f, "decoding error: {e}"),
            EngineError::OpenDb(e) => write!(f, "database open error: {e}"),
            EngineError::RecoveryFailed(e) => {
                write!(f, "journal corrupt and recovery snapshot also failed: {e}")
            }
            EngineError::Tree(e) => write!(f, "tree mutation error: {e}"),
            EngineError::TrackTypeMismatch { track_id } => {
                write!(f, "element type mismatch or absent track {track_id}")
            }
            EngineError::ProjectFileExists { path } => {
                write!(f, "project file already exists: {}", path.display())
            }
            EngineError::ProjectFileNotFound { path } => {
                write!(f, "project file not found: {}", path.display())
            }
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Sqlite(e) => Some(e),
            EngineError::Store(e) => Some(e),
            EngineError::Journal(e) => Some(e),
            EngineError::History(e) => Some(e),
            EngineError::Replay(e) => Some(e),
            EngineError::Encode(e) => Some(e),
            EngineError::Decode(e) => Some(e),
            EngineError::OpenDb(e) => Some(&**e),
            EngineError::RecoveryFailed(e) => Some(e),
            EngineError::Tree(e) => Some(e),
            EngineError::TrackTypeMismatch { .. } => None,
            EngineError::ProjectFileExists { .. } => None,
            EngineError::ProjectFileNotFound { .. } => None,
        }
    }
}

impl From<MetadataLoadError> for EngineError {
    fn from(e: MetadataLoadError) -> Self {
        match e {
            MetadataLoadError::Journal(je) => EngineError::Journal(je),
            MetadataLoadError::Store(se) => EngineError::Store(se),
            MetadataLoadError::Decode(de) => EngineError::Decode(de),
        }
    }
}

/// Outcome of a successful [`ProjectState::open_project`] call.
///
/// The caller **must** surface a warning when [`recovery`](Self::recovery) is `Some`
/// — silently ignoring it is silent data loss (post-snapshot edits were dropped and
/// the user must be told).
#[derive(Debug)]
pub struct OpenOutcome {
    /// IDs of `source_type = File` tracks whose source file could not be resolved.
    ///
    /// The Missing-Files dialog is deferred to M6; M1 returns the list here for
    /// the Tauri command handler to surface.
    pub missing_tracks: Vec<u32>,
    /// `Some` iff the journal tail was corrupt and the project was rolled back to
    /// the latest snapshot. Post-snapshot edits are lost.
    pub recovery: Option<RecoveryInfo>,
}

/// Details about a corrupt-journal recovery during `open_project`.
#[derive(Debug)]
pub struct RecoveryInfo {
    /// Row id of the journal row that failed to decode (0 for errors without a row id).
    pub failed_row: i64,
    /// Row id of the snapshot the project was rolled back to.
    pub snapshot_id: i64,
}

/// A new element blob for an [`Insert`](BatchOpKind::Insert) or
/// [`Update`](BatchOpKind::Update) operation within [`ProjectState::apply_batch`].
///
/// Callers serialize the element with [`encode_turn`] / [`encode_label`] and pass the
/// resulting hash and bytes alongside the decoded `Arc<T>` for in-memory tree mutation.
///
/// [`encode_turn`]: crate::project::turn::encode_turn
/// [`encode_label`]: crate::project::label::encode_label
#[allow(dead_code)] // apply_batch is the only caller; its non-test callers arrive in M4/M5.
pub(crate) enum NewElement {
    /// A label element for the labels track (track id 0).
    Label {
        /// Content hash of the serialized blob.
        hash: Hash,
        /// Serialized blob bytes (ready for `store::put`).
        bytes: Vec<u8>,
        /// Decoded element (for in-memory tree insertion).
        element: Arc<Label>,
    },
    /// A turn element for a speech track (track id 1+).
    Turn {
        /// Content hash of the serialized blob.
        hash: Hash,
        /// Serialized blob bytes (ready for `store::put`).
        bytes: Vec<u8>,
        /// Decoded element (for in-memory tree insertion).
        element: Arc<Turn>,
    },
}

impl NewElement {
    #[allow(dead_code)] // See NewElement.
    fn hash(&self) -> Hash {
        match self {
            Self::Label { hash, .. } | Self::Turn { hash, .. } => *hash,
        }
    }

    #[allow(dead_code)] // See NewElement.
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Label { bytes, .. } | Self::Turn { bytes, .. } => bytes,
        }
    }
}

/// The kind of a single edit within a [`BatchOp`].
#[allow(dead_code)] // See NewElement.
pub(crate) enum BatchOpKind {
    /// Insert a new element at `sample` (an element-boundary sample).
    Insert(NewElement),
    /// Replace the element covering `sample` with the given new element.
    Update(NewElement),
    /// Delete the element covering `sample`.
    Delete,
}

/// One edit operation in a [`ProjectState::apply_batch`] call.
#[allow(dead_code)] // See NewElement.
pub(crate) struct BatchOp {
    /// Track to edit.
    pub(crate) track_id: u32,
    /// Position in **original-tree** coordinate samples.
    ///
    /// For `Insert`: must be `0`, the cumulative start of an existing element,
    /// or `total_duration()` (append). For `Update` / `Delete`: any in-interval
    /// sample of the target element.
    pub(crate) sample: i64,
    /// Operation kind and payload.
    pub(crate) kind: BatchOpKind,
}

/// Synchronous snapshot writer.
///
/// Owns a dedicated [`Db`] handle to the project database so that snapshot writes do
/// not contend with the main edit connection. Only one `ProjectState` (and thus one
/// `SnapshotWriter`) may be open per project file at a time; WAL mode +
/// `busy_timeout = 5000` handles any racing between the two connections from M5 on,
/// when idle-autosave will run concurrently with edit commands.
///
/// M1 implementation: synchronous. No idle-autosave timer. The connection-ownership
/// and lock-free-handoff shape is in place for M5 to attach the timer without
/// structural changes to this type.
struct SnapshotWriter {
    db: Db,
}

impl SnapshotWriter {
    /// Flatten `state` to a snapshot blob and commit it as a `type = 1` journal row.
    ///
    /// Lock-free portion (no connection held): the O(1) `Arc` reference was cloned
    /// at the call site; flatten + postcard + BLAKE3 happen here before acquiring any
    /// lock. Only the final two-INSERT transaction (blob + row) holds the write lock.
    fn write(&mut self, state: &Arc<TimelineState>, now: i64) -> Result<(), EngineError> {
        let snap = snapshot_from_trees(&state.trees);
        let (h, bytes) = encode_snapshot(&snap).map_err(EngineError::Encode)?;
        let tx = self
            .db
            .conn_mut()
            .transaction()
            .map_err(EngineError::Sqlite)?;
        store::put(&tx, &h, &bytes).map_err(EngineError::Store)?;
        journal::append_snapshot(&tx, CommandId::Unknown, &h, now).map_err(EngineError::Journal)?;
        tx.commit().map_err(EngineError::Sqlite)
    }
}

/// The single owner of one open project: its database handle, the live immutable
/// [`TimelineState`](crate::project::undo::TimelineState), the in-memory undo/redo
/// history, and the snapshot writer.
///
/// The only type that mutates the project database. All database-mutating methods run
/// in a single atomic transaction.
pub struct ProjectState {
    db: Db,
    current: Arc<TimelineState>,
    history: History,
    sample_rate: u32,
    writer: SnapshotWriter,
    /// Room tones keyed by track ID; derived from the store on open.
    room_tones: BTreeMap<u32, Arc<RoomTone>>,
    /// Absolute path of the project's `.vocalboard` SQLite file, retained so the
    /// audio handlers can resolve the co-located `<project>.vbdata/` directory
    /// (the resampled FLAC cache) without re-deriving it from a command param.
    project_path: PathBuf,
}

impl std::fmt::Debug for ProjectState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectState")
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl ProjectState {
    /// Create a new, empty project at `path` with the given `sample_rate`.
    ///
    /// Creates the SQLite file (errors with [`EngineError::ProjectFileExists`] if it
    /// already exists), runs schema migrations, then writes the `project` singleton row
    /// and an initial empty snapshot in one transaction.
    pub fn new_project(
        path: &Path,
        sample_rate: u32,
        settings: &Settings,
    ) -> Result<Self, EngineError> {
        let mut db = Db::create(path).map_err(map_db_open_error)?;
        let now = now_posix();
        {
            let tx = db.conn_mut().transaction().map_err(EngineError::Sqlite)?;
            db_project::insert_project_row(&tx, sample_rate).map_err(EngineError::Sqlite)?;
            let snap = Snapshot::default();
            let (h, bytes) = encode_snapshot(&snap).map_err(EngineError::Encode)?;
            store::put(&tx, &h, &bytes).map_err(EngineError::Store)?;
            journal::append_snapshot(&tx, CommandId::Unknown, &h, now)
                .map_err(EngineError::Journal)?;
            tx.commit().map_err(EngineError::Sqlite)?;
        }
        // Call open_shared before moving `db` into ProjectState (immutable borrow — no conflict).
        let writer_db = db.open_shared().map_err(map_db_open_error)?;
        Ok(Self {
            db,
            current: Arc::new(TimelineState::default()),
            history: History::new(settings.undo_history_limit),
            sample_rate,
            writer: SnapshotWriter { db: writer_db },
            room_tones: BTreeMap::new(),
            project_path: path.to_path_buf(),
        })
    }

    /// Open an existing project at `path`.
    ///
    /// Errors with [`EngineError::ProjectFileNotFound`] if the path does not exist.
    /// Runs schema migrations (refuses files created by a newer build), replays the
    /// journal, resolves source files, and returns an [`OpenOutcome`]. The caller
    /// **must** surface a warning when `outcome.recovery.is_some()`.
    pub fn open_project(
        path: &Path,
        settings: &Settings,
    ) -> Result<(Self, OpenOutcome), EngineError> {
        let mut db = Db::open(path).map_err(map_db_open_error)?;

        let sample_rate = db_project::read_sample_rate(db.conn()).map_err(EngineError::Sqlite)?;

        // Happy path: full replay. On failure, fall back to the latest snapshot.
        let (trees, recovery) = match load_and_replay(&db, None) {
            Ok(trees) => (trees, None),
            Err(replay_err) => {
                let failed_row = replay_error_row_id(&replay_err);
                match load_latest_snapshot(&db, None) {
                    Ok((snapshot_id, trees)) => (
                        trees,
                        Some(RecoveryInfo {
                            failed_row,
                            snapshot_id,
                        }),
                    ),
                    Err(snap_err) => return Err(EngineError::RecoveryFailed(snap_err)),
                }
            }
        };

        // When recovery rolled the timeline back to a snapshot, pin metadata to the
        // same point: a post-snapshot `type = -1` write (e.g. a track added after the
        // snapshot) must not surface against a rolled-back timeline that has no tree
        // for it. On the happy path (`recovery == None`) load the latest metadata.
        let meta_as_of = recovery.as_ref().map(|r| r.snapshot_id);
        let meta = load_current_metadata(&db, meta_as_of).map_err(EngineError::from)?;

        // Source-file resolution: compute missing-track list and persist FoundViaAbsolute rewrites.
        let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let missing = missing_tracks(project_dir, &meta);

        let mut updated_meta = meta.clone();
        let mut has_rewrites = false;
        for track in &mut updated_meta.tracks {
            if let FileResolution::FoundViaAbsolute { new_relative, .. } =
                resolve_track_source(project_dir, track)
            {
                track.source_path_relative = new_relative;
                has_rewrites = true;
            }
        }
        let meta = if has_rewrites {
            let now = now_posix();
            let tx = db.conn_mut().transaction().map_err(EngineError::Sqlite)?;
            let (h, bytes) = encode_metadata(&updated_meta).map_err(EngineError::Encode)?;
            store::put(&tx, &h, &bytes).map_err(EngineError::Store)?;
            journal::append_metadata(&tx, CommandId::Unknown, &h, now)
                .map_err(EngineError::Journal)?;
            tx.commit().map_err(EngineError::Sqlite)?;
            updated_meta
        } else {
            meta
        };

        // Load room tones: for each track with a room_tone_hash, fetch and decode from the store.
        let mut room_tones: BTreeMap<u32, Arc<RoomTone>> = BTreeMap::new();
        for track in &meta.tracks {
            if let Some(hash) = track.room_tone_hash {
                if let Ok(bytes) = store::get(db.conn(), &hash) {
                    if let Ok(rt) = decode_room_tone(&bytes) {
                        room_tones.insert(track.id, Arc::new(rt));
                    }
                }
            }
        }

        // Call open_shared before moving `db` into ProjectState (immutable borrow — no conflict).
        let writer_db = db.open_shared().map_err(map_db_open_error)?;

        Ok((
            Self {
                db,
                current: Arc::new(TimelineState {
                    trees,
                    metadata: meta,
                }),
                history: History::new(settings.undo_history_limit),
                sample_rate,
                writer: SnapshotWriter { db: writer_db },
                room_tones,
                project_path: path.to_path_buf(),
            },
            OpenOutcome {
                missing_tracks: missing,
                recovery,
            },
        ))
    }

    /// Write the current state as a new snapshot to the journal.
    ///
    /// Clones `current` (O(1) Arc bump), then flattens and persists it on the
    /// snapshot writer's connection. Blocks until the snapshot row is committed so
    /// that callers and tests observe the row immediately.
    pub fn save_snapshot_now(&mut self) -> Result<(), EngineError> {
        let state = Arc::clone(&self.current);
        self.writer.write(&state, now_posix())
    }

    /// Undo the most recent edit. Returns `Ok(false)` when the undo stack is empty.
    pub fn undo(&mut self) -> Result<bool, EngineError> {
        self.history
            .undo(&mut self.current, self.db.conn_mut(), now_posix())
            .map_err(EngineError::History)
    }

    /// Redo the most recently undone edit. Returns `Ok(false)` when redo is empty.
    pub fn redo(&mut self) -> Result<bool, EngineError> {
        self.history
            .redo(&mut self.current, self.db.conn_mut(), now_posix())
            .map_err(EngineError::History)
    }

    /// The project sample rate in Hz (locked at creation; immutable).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Pre-decoded room tone for a track, if one was recorded and decoded on open.
    pub fn room_tone(&self, track_id: u32) -> Option<&Arc<RoomTone>> {
        self.room_tones.get(&track_id)
    }

    /// Insert or replace the decoded room tone for `track_id`.
    ///
    /// Called by the M4 import path after a new room tone is detected and persisted.
    pub fn insert_room_tone(&mut self, track_id: u32, rt: Arc<RoomTone>) {
        self.room_tones.insert(track_id, rt);
    }

    /// The per-track timeline trees of the live state, keyed by `track_id`.
    ///
    /// Read-only borrow of the immutable [`TimelineState`](crate::project::undo::TimelineState);
    /// holds no `Db` and performs no I/O. The audio handlers walk these to build the EDL
    /// cursors (speech tracks) and the transcript (per-turn).
    pub fn trees(&self) -> &crate::project::snapshot::PerTrackTrees {
        &self.current.trees
    }

    /// The track metadata of the live state, in canonical ascending-`id` order.
    ///
    /// Read-only; track 0 (labels) is implicit and not listed. The export/playback
    /// handlers project each [`TrackMeta`](crate::project::metadata::TrackMeta) into a
    /// `TrackSource` (channels, wet/dry, length) for the renderer.
    pub fn tracks(&self) -> &[crate::project::metadata::TrackMeta] {
        &self.current.metadata.tracks
    }

    /// The speaker metadata of the live state, in canonical ascending-`id` order.
    ///
    /// Read-only; the transcript handler builds a `speaker_id → name` map from this.
    pub fn speakers(&self) -> &[crate::project::metadata::SpeakerMeta] {
        &self.current.metadata.speakers
    }

    /// The co-located `<project>.vbdata/` directory holding the resampled FLAC cache.
    ///
    /// Derived from the project file path (its `.vocalboard` extension replaced with
    /// `.vbdata`); the dry cache lives under `<vbdata_dir>/resampled/<id>.flac`. This is
    /// the path the audio handlers pass to `CacheSourceProvider`. Read-only; touches no disk.
    pub fn vbdata_dir(&self) -> PathBuf {
        self.project_path.with_extension("vbdata")
    }

    /// The directory containing the project's `.vocalboard` file.
    ///
    /// Falls back to `.` when the stored path has no parent. Read-only; touches no disk.
    pub fn project_dir(&self) -> &Path {
        self.project_path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Inject a single speech `track` + its `tree` into the live timeline state (test support).
    ///
    /// M2 has no public track-creation command (import lands at M4), so cross-crate handler tests
    /// (the `app` export/playback handlers) synthesise a populated project this way. **Does not
    /// journal** — it mutates only the in-memory `current` state, so it does not perturb the
    /// non-journaled-command assertions that count journal rows. Feature-gated; no production
    /// callers.
    #[cfg(feature = "test-support")]
    pub fn test_inject_speech_track(
        &mut self,
        track: crate::project::metadata::TrackMeta,
        tree: crate::project::snapshot::TrackTree,
    ) {
        let mut state = (*self.current).clone();
        state.trees.insert(track.id, tree);
        state.metadata.tracks.push(track);
        state.metadata.tracks.sort_by_key(|t| t.id);
        self.current = Arc::new(state);
    }

    /// Count rows in the `journal` table (test support).
    ///
    /// Used by cross-crate tests to assert that non-journaled commands (`play_from`/`pause`/`stop`)
    /// append no journal rows. Feature-gated; no production callers.
    #[cfg(feature = "test-support")]
    pub fn test_journal_row_count(&self) -> u64 {
        self.db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Apply a batch of edits atomically.
    ///
    /// Ops are applied in descending sample order over original-tree coordinates,
    /// keeping each position stable for the whole batch with no re-resolution.
    /// Forward deltas are persisted in application order; each op's inverse is
    /// stored in reversed (ascending) order in the [`UndoEntry`] so
    /// [`History::undo`] can replay it correctly.
    ///
    /// If `metadata` is `Some(new)`, the metadata change is journalled in the
    /// **same transaction** as the delta batch (combined tree+metadata edit). An
    /// empty `ops` with `Some` metadata produces a metadata-only edit (no delta
    /// row). Empty `ops` and `None` metadata is a no-op.
    ///
    /// `current` is only swapped after the transaction commits — a commit failure
    /// leaves the database and `current` unchanged.
    ///
    /// [`UndoEntry`]: crate::project::undo::UndoEntry
    /// [`History::undo`]: crate::project::undo::History::undo
    #[allow(dead_code)] // Non-test callers arrive in M4/M5 (the first turn-mutating command).
    #[allow(clippy::too_many_lines)] // Three op kinds × two track types yields inherent branching.
    pub(crate) fn apply_batch(
        &mut self,
        ops: &[BatchOp],
        metadata: Option<Metadata>,
        category: CommandId,
    ) -> Result<(), EngineError> {
        if ops.is_empty() && metadata.is_none() {
            return Ok(());
        }
        let now = now_posix();
        let (metadata_changed, after_metadata) = match metadata {
            Some(new_meta) => (true, new_meta),
            None => (false, self.current.metadata.clone()),
        };

        // Sort descending by sample; tie-break on track_id (descending) for determinism.
        let mut order: Vec<usize> = (0..ops.len()).collect();
        order.sort_by(|&a, &b| {
            ops[b]
                .sample
                .cmp(&ops[a].sample)
                .then_with(|| ops[b].track_id.cmp(&ops[a].track_id))
        });

        // Original trees (never mutated) for coordinate resolution throughout the batch.
        let original_trees: PerTrackTrees = self.current.trees.clone();
        // Working trees: start identical to original, mutated per op in application order.
        let mut working_trees: PerTrackTrees = original_trees.clone();

        let mut forward_deltas: Vec<Delta> = Vec::with_capacity(ops.len());
        // Per-op inverse in application order (descending); reversed before storing.
        let mut inverse_fwd: Vec<Delta> = Vec::with_capacity(ops.len());
        let mut new_blobs: Vec<(Hash, Vec<u8>)> = Vec::new();

        for &idx in &order {
            let op = &ops[idx];
            let tid = op.track_id;

            match &op.kind {
                BatchOpKind::Insert(new_elem) => {
                    let loc = insert_location(&original_trees, tid, op.sample);
                    let new_hash = new_elem.hash();
                    new_blobs.push((new_hash, new_elem.bytes().to_vec()));
                    forward_deltas.push(Delta::insert_after(tid, loc, new_hash));
                    inverse_fwd.push(Delta::delete_after(tid, loc));

                    let updated = match (working_trees.remove(&tid), new_elem) {
                        (Some(TrackTree::Labels(t)), NewElement::Label { hash, element, .. }) => {
                            TrackTree::Labels(
                                t.insert_at(op.sample, *hash, Arc::clone(element))
                                    .map_err(EngineError::Tree)?,
                            )
                        }
                        (None, NewElement::Label { hash, element, .. }) => {
                            let empty: ImplicitTimelineTree<Label> = ImplicitTimelineTree::new();
                            TrackTree::Labels(
                                empty
                                    .insert_at(op.sample, *hash, Arc::clone(element))
                                    .map_err(EngineError::Tree)?,
                            )
                        }
                        (Some(TrackTree::Speech(t)), NewElement::Turn { hash, element, .. }) => {
                            TrackTree::Speech(
                                t.insert_at(op.sample, *hash, Arc::clone(element))
                                    .map_err(EngineError::Tree)?,
                            )
                        }
                        (None, NewElement::Turn { hash, element, .. }) => {
                            let empty: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
                            TrackTree::Speech(
                                empty
                                    .insert_at(op.sample, *hash, Arc::clone(element))
                                    .map_err(EngineError::Tree)?,
                            )
                        }
                        _ => return Err(EngineError::TrackTypeMismatch { track_id: tid }),
                    };
                    working_trees.insert(tid, updated);
                }

                BatchOpKind::Update(new_elem) => {
                    let (loc, h_old) = resolve_edit_location(&original_trees, tid, op.sample)?;
                    let new_hash = new_elem.hash();
                    new_blobs.push((new_hash, new_elem.bytes().to_vec()));
                    forward_deltas.push(Delta::update_after(tid, loc, new_hash));
                    inverse_fwd.push(Delta::update_after(tid, loc, h_old));

                    let updated = match (working_trees.remove(&tid), new_elem) {
                        (Some(TrackTree::Labels(t)), NewElement::Label { hash, element, .. }) => {
                            TrackTree::Labels(
                                t.update_at(op.sample, *hash, Arc::clone(element))
                                    .map_err(EngineError::Tree)?,
                            )
                        }
                        (Some(TrackTree::Speech(t)), NewElement::Turn { hash, element, .. }) => {
                            TrackTree::Speech(
                                t.update_at(op.sample, *hash, Arc::clone(element))
                                    .map_err(EngineError::Tree)?,
                            )
                        }
                        _ => return Err(EngineError::TrackTypeMismatch { track_id: tid }),
                    };
                    working_trees.insert(tid, updated);
                }

                BatchOpKind::Delete => {
                    let (loc, h_deleted) = resolve_edit_location(&original_trees, tid, op.sample)?;
                    forward_deltas.push(Delta::delete_after(tid, loc));
                    inverse_fwd.push(Delta::insert_after(tid, loc, h_deleted));

                    let updated = match working_trees.remove(&tid) {
                        Some(TrackTree::Labels(t)) => {
                            TrackTree::Labels(t.delete_at(op.sample).map_err(EngineError::Tree)?)
                        }
                        Some(TrackTree::Speech(t)) => {
                            TrackTree::Speech(t.delete_at(op.sample).map_err(EngineError::Tree)?)
                        }
                        None => return Err(EngineError::TrackTypeMismatch { track_id: tid }),
                    };
                    working_trees.insert(tid, updated);
                }
            }
        }

        // One atomic transaction: persist new element blobs, the forward delta row
        // (omitted for metadata-only edits), and the metadata row (if changed).
        {
            let tx = self
                .db
                .conn_mut()
                .transaction()
                .map_err(EngineError::Sqlite)?;
            for (hash, bytes) in &new_blobs {
                store::put(&tx, hash, bytes).map_err(EngineError::Store)?;
            }
            if !forward_deltas.is_empty() {
                let payload = encode_delta_batch(&forward_deltas).map_err(EngineError::Encode)?;
                journal::append_delta_batch(&tx, category, &payload, now)
                    .map_err(EngineError::Journal)?;
            }
            if metadata_changed {
                let (h, bytes) = encode_metadata(&after_metadata).map_err(EngineError::Encode)?;
                store::put(&tx, &h, &bytes).map_err(EngineError::Store)?;
                journal::append_metadata(&tx, category, &h, now).map_err(EngineError::Journal)?;
            }
            tx.commit().map_err(EngineError::Sqlite)?;
        }

        // Commit succeeded: swap current and record the undo entry.
        let before = Arc::clone(&self.current);
        let new_state = Arc::new(TimelineState {
            trees: working_trees,
            metadata: after_metadata,
        });
        // Inverse is each op's inverse in reverse-application order (ascending sample).
        inverse_fwd.reverse();
        let (forward_delta, inverse_delta) = if ops.is_empty() {
            (None, None)
        } else {
            (Some(forward_deltas), Some(inverse_fwd))
        };
        self.history.record(UndoEntry {
            before,
            after: Arc::clone(&new_state),
            forward_delta,
            inverse_delta,
            metadata_changed,
            category,
        });
        self.current = new_state;
        Ok(())
    }
}

/// POSIX seconds (UTC). Called once per mutating command and threaded to every
/// `journal::append_*` call in that command's transaction, keeping `journal.rs`
/// clock-free and deterministic in tests.
fn now_posix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Saturating cast: in practice well within i64 range until year ~292 billion.
    secs.min(i64::MAX as u64) as i64
}

/// Resolve the insert [`Location`] for `sample` in `track_id`'s original tree.
///
/// Returns `Location::Start` when the track is absent (inserting into a new track).
/// For existing tracks, a boundary interior to the tree resolves via
/// `element_at_sample`; an append (`sample >= total_duration()`) iterates to the
/// last element.
#[allow(dead_code)] // Called only by apply_batch; see apply_batch's dead_code note.
fn insert_location(original_trees: &PerTrackTrees, track_id: u32, sample: i64) -> Location {
    match original_trees.get(&track_id) {
        None => Location::Start,
        Some(TrackTree::Labels(t)) => insert_location_in_tree(t, sample),
        Some(TrackTree::Speech(t)) => insert_location_in_tree(t, sample),
    }
}

#[allow(dead_code)] // See insert_location.
fn insert_location_in_tree<T: Tilable>(tree: &ImplicitTimelineTree<T>, sample: i64) -> Location {
    if sample >= tree.total_duration() {
        // Append: O(log n) right-spine walk via last_hash (vs. O(n) iter().last()).
        tree.last_hash().map_or(Location::Start, Location::After)
    } else {
        // Interior boundary: the element at this sample's predecessor gives the location.
        tree.element_at_sample(sample)
            .map(|hit| hit.predecessor.map_or(Location::Start, Location::After))
            .unwrap_or(Location::Start)
    }
}

/// Resolve the edit [`Location`] and the existing element hash for an update or delete.
///
/// Returns `Err(TrackTypeMismatch)` if the track is absent, or
/// `Err(Tree(SampleOutOfRange))` if `sample` does not hit any element.
#[allow(dead_code)] // See insert_location.
fn resolve_edit_location(
    original_trees: &PerTrackTrees,
    track_id: u32,
    sample: i64,
) -> Result<(Location, Hash), EngineError> {
    let tree = original_trees
        .get(&track_id)
        .ok_or(EngineError::TrackTypeMismatch { track_id })?;
    let (hash, predecessor) = match tree {
        TrackTree::Labels(t) => t
            .element_at_sample(sample)
            .map(|h| (h.hash, h.predecessor))
            .ok_or_else(|| EngineError::Tree(TreeError::SampleOutOfRange(sample)))?,
        TrackTree::Speech(t) => t
            .element_at_sample(sample)
            .map(|h| (h.hash, h.predecessor))
            .ok_or_else(|| EngineError::Tree(TreeError::SampleOutOfRange(sample)))?,
    };
    Ok((predecessor.map_or(Location::Start, Location::After), hash))
}

/// Map a [`DbOpenError`] to the appropriate [`EngineError`] variant.
fn map_db_open_error(e: DbOpenError) -> EngineError {
    match e {
        DbOpenError::AlreadyExists(path) => EngineError::ProjectFileExists { path },
        DbOpenError::NotFound(path) => EngineError::ProjectFileNotFound { path },
        DbOpenError::Sqlite(e) => EngineError::OpenDb(Box::new(e)),
        DbOpenError::Migration(e) => EngineError::OpenDb(e),
    }
}

/// Extract the journal row id from a [`ReplayError`], if one is available.
/// Returns `0` for errors not tied to a specific journal row.
///
/// Note: [`ReplayError::MalformedSnapshotPayload`] is intentionally absent. If
/// `load_and_replay` fails because the latest snapshot row is malformed, the fallback
/// `load_latest_snapshot` picks the same row and also fails, so `open_project` returns
/// `RecoveryFailed` and `failed_row` is never surfaced to the caller.
fn replay_error_row_id(e: &ReplayError) -> i64 {
    match e {
        ReplayError::DeltaDecode { row_id, .. } => *row_id,
        ReplayError::DeltaApply { row_id, .. } => *row_id,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;
    use crate::db::journal::append_delta_batch;
    use crate::project::command_id::CommandId;
    use crate::project::delta::{encode_delta_batch, Delta, Location};
    use crate::project::hash::Hash;
    use crate::project::label::{encode_label, Label, LabelKind};
    use crate::project::metadata::{load_current_metadata, Metadata, ProjectMeta};
    use crate::project::snapshot::{PerTrackTrees, TrackTree};
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{encode_turn, Turn};
    use crate::project::undo::{TimelineState, UndoEntry};
    use crate::settings::Settings;

    /// Build a non-empty two-track [`TimelineState`] (track 0 label, track 1
    /// turn), storing each element blob on `conn`. The `seed` distinguishes the
    /// element ids/durations so two calls produce different hashes (e.g. for an
    /// update edit). Returns the state plus the label and turn hashes.
    fn two_track_state(conn: &Connection, seed: u64) -> (TimelineState, Hash, Hash) {
        let turn = Turn {
            id: seed,
            speaker_id: None,
            turn_duration: 100 + seed as i64,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![],
        };
        let (h_turn, turn_bytes) = encode_turn(&turn).unwrap();
        store::put(conn, &h_turn, &turn_bytes).unwrap();

        let label = Label {
            id: seed,
            text: format!("label-{seed}"),
            kind: LabelKind::Plain,
            post_label_silence: 50,
        };
        let (h_label, label_bytes) = encode_label(&label).unwrap();
        store::put(conn, &h_label, &label_bytes).unwrap();

        let mut trees = PerTrackTrees::new();
        trees.insert(
            0,
            TrackTree::Labels(ImplicitTimelineTree::from_sorted_elements(vec![(
                h_label,
                Arc::new(label),
            )])),
        );
        trees.insert(
            1,
            TrackTree::Speech(ImplicitTimelineTree::from_sorted_elements(vec![(
                h_turn,
                Arc::new(turn),
            )])),
        );
        let state = TimelineState {
            trees,
            metadata: Metadata::default(),
        };
        (state, h_label, h_turn)
    }

    // E1 — EngineError Display is non-empty; every variant chains source().
    #[test]
    fn engine_error_display_and_source() {
        use std::error::Error;

        use crate::db::journal::JournalError;
        use crate::db::store::StoreError;
        use crate::project::hash::{DecodeError, Hash};
        use crate::project::snapshot::ReplayError;

        let pe: postcard::Error = postcard::from_bytes::<u32>(&[]).unwrap_err();
        let se: rusqlite::Error = rusqlite::Error::QueryReturnedNoRows;

        let opendb_err: Box<dyn std::error::Error + Send + Sync> =
            Box::new(rusqlite::Error::QueryReturnedNoRows);

        let variants: Vec<EngineError> = vec![
            EngineError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
            EngineError::Store(StoreError::NotFound(Hash([0u8; 16]))),
            EngineError::Journal(JournalError::MalformedHashPayload { id: 1, len: 5 }),
            EngineError::History(crate::project::undo::HistoryError::Sqlite(se)),
            EngineError::Replay(ReplayError::NoSnapshot),
            EngineError::Encode(pe),
            EngineError::Decode(DecodeError::Empty),
            EngineError::OpenDb(opendb_err),
            EngineError::RecoveryFailed(ReplayError::NoSnapshot),
        ];
        for v in &variants {
            assert!(
                !v.to_string().is_empty(),
                "Display must be non-empty for {v:?}"
            );
            assert!(v.source().is_some(), "source() must chain for {v:?}");
        }

        // Edit-application variants.
        let v_tree = EngineError::Tree(crate::project::tree::TreeError::SampleOutOfRange(0));
        assert!(!v_tree.to_string().is_empty(), "Tree: Display non-empty");
        assert!(v_tree.source().is_some(), "Tree: chains source()");

        let v_mm = EngineError::TrackTypeMismatch { track_id: 42 };
        assert!(
            !v_mm.to_string().is_empty(),
            "TrackTypeMismatch: Display non-empty"
        );
        assert!(v_mm.source().is_none(), "TrackTypeMismatch: no source");

        // Project-file lifecycle variants.
        let v_exists = EngineError::ProjectFileExists {
            path: std::path::PathBuf::from("/x"),
        };
        assert!(
            !v_exists.to_string().is_empty(),
            "ProjectFileExists: Display non-empty"
        );
        assert!(v_exists.source().is_none(), "ProjectFileExists: no source");

        let v_missing = EngineError::ProjectFileNotFound {
            path: std::path::PathBuf::from("/y"),
        };
        assert!(
            !v_missing.to_string().is_empty(),
            "ProjectFileNotFound: Display non-empty"
        );
        assert!(
            v_missing.source().is_none(),
            "ProjectFileNotFound: no source"
        );
    }

    // E1b — new_project on an existing path returns ProjectFileExists.
    #[test]
    fn new_project_fails_if_file_exists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vocalboard");
        let settings = Settings::default();
        let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        drop(ps);
        let err = ProjectState::new_project(&path, 48000, &settings).unwrap_err();
        assert!(
            matches!(err, EngineError::ProjectFileExists { .. }),
            "expected ProjectFileExists, got {err:?}"
        );
    }

    // E1c — open_project on a missing path returns ProjectFileNotFound.
    #[test]
    fn open_project_fails_if_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.vocalboard");
        let settings = Settings::default();
        let err = ProjectState::open_project(&path, &settings).unwrap_err();
        assert!(
            matches!(err, EngineError::ProjectFileNotFound { .. }),
            "expected ProjectFileNotFound, got {err:?}"
        );
    }

    // E1d — open_shared writer round-trip: save_snapshot_now still works
    // (verifies SnapshotWriter uses Db::open_shared internally).
    #[test]
    fn snapshot_writer_round_trip_via_open_shared() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        ps.save_snapshot_now().unwrap();
        drop(ps);
        let (_ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
        assert!(outcome.recovery.is_none(), "clean journal — no recovery");
    }

    // E2 — Full lifecycle round-trip with non-empty content: build a two-track
    // state and a non-default metadata by hand (standing in for the apply_batch producer),
    // snapshot, drop, reopen, and assert the trees AND metadata survive verbatim.
    #[test]
    fn lifecycle_round_trip_with_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vocalboard");
        let settings = Settings::default();

        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        let before = Arc::clone(&ps.current);

        // Producer stand-in: non-empty two-track edit over the empty initial tree.
        let (mut after, h_label, h_turn) = two_track_state(ps.db.conn(), 1);
        after.metadata.project.name = Some("roundtrip".to_string());

        // Persist the non-default metadata as a type=-1 row so it survives reopen
        // (the snapshot carries only trees; metadata is journalled separately).
        let (mh, mbytes) = encode_metadata(&after.metadata).unwrap();
        store::put(ps.db.conn(), &mh, &mbytes).unwrap();
        journal::append_metadata(ps.db.conn(), CommandId::Unknown, &mh, 0).unwrap();

        // Write the forward delta row directly, then record the undo entry.
        let forward = vec![
            Delta::insert_after(0, Location::Start, h_label),
            Delta::insert_after(1, Location::Start, h_turn),
        ];
        let payload = encode_delta_batch(&forward).unwrap();
        append_delta_batch(ps.db.conn(), CommandId::Unknown, &payload, 0).unwrap();

        let after = Arc::new(after);
        ps.history.record(UndoEntry {
            before,
            after: Arc::clone(&after),
            forward_delta: Some(forward),
            inverse_delta: Some(vec![
                Delta::delete_after(0, Location::Start),
                Delta::delete_after(1, Location::Start),
            ]),
            metadata_changed: true,
            category: CommandId::Unknown,
        });
        ps.current = Arc::clone(&after);

        // Snapshot a copy of the live state for post-reopen comparison.
        let saved = Arc::clone(&ps.current);
        ps.save_snapshot_now().unwrap();
        drop(ps);

        let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
        assert!(outcome.recovery.is_none(), "clean journal — no recovery");
        assert!(outcome.missing_tracks.is_empty());
        assert_eq!(
            ps2.current.trees, saved.trees,
            "non-empty trees must round-trip through snapshot + reopen"
        );
        assert_eq!(
            ps2.current.metadata, saved.metadata,
            "metadata must round-trip through reopen"
        );
    }

    // E3 — undo/redo are journal-recorded: after each, the in-memory `current`
    // AND a fresh `load_and_replay` of the journal agree on the trees. Uses an
    // update edit over an established baseline (both pre/post states have the same
    // track keys), so replay and the in-memory state compare cleanly.
    #[test]
    fn undo_redo_reproduced_by_replay() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vocalboard");
        let settings = Settings::default();

        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Baseline: a two-track state, snapshotted (no history entry — it is the
        // pre-edit state, not an undoable edit).
        let (baseline, _bl, _bt) = two_track_state(ps.db.conn(), 1);
        let baseline = Arc::new(baseline);
        ps.current = Arc::clone(&baseline);
        ps.save_snapshot_now().unwrap();

        // The undoable edit: update both tracks' first element (seed 2 → new hashes).
        let (updated, h_label2, h_turn2) = two_track_state(ps.db.conn(), 2);
        let updated = Arc::new(updated);
        let bl_hash = match &baseline.trees[&0] {
            TrackTree::Labels(t) => t.iter().next().unwrap().hash,
            _ => unreachable!(),
        };
        let bt_hash = match &baseline.trees[&1] {
            TrackTree::Speech(t) => t.iter().next().unwrap().hash,
            _ => unreachable!(),
        };
        let forward = vec![
            Delta::update_after(0, Location::Start, h_label2),
            Delta::update_after(1, Location::Start, h_turn2),
        ];
        let inverse = vec![
            Delta::update_after(0, Location::Start, bl_hash),
            Delta::update_after(1, Location::Start, bt_hash),
        ];
        let payload = encode_delta_batch(&forward).unwrap();
        append_delta_batch(ps.db.conn(), CommandId::Unknown, &payload, 0).unwrap();

        ps.history.record(UndoEntry {
            before: Arc::clone(&baseline),
            after: Arc::clone(&updated),
            forward_delta: Some(forward),
            inverse_delta: Some(inverse),
            metadata_changed: false,
            category: CommandId::Unknown,
        });
        ps.current = Arc::clone(&updated);

        // Undo → pre-edit baseline, and journal replay reproduces it.
        assert!(ps.undo().unwrap(), "undo returns true");
        assert_eq!(ps.current.trees, baseline.trees, "undo restores baseline");
        assert_eq!(
            load_and_replay(&ps.db, None).unwrap(),
            baseline.trees,
            "replay after undo reproduces baseline trees"
        );

        // Redo → post-edit state, and replay reproduces it.
        assert!(ps.redo().unwrap(), "redo returns true");
        assert_eq!(ps.current.trees, updated.trees, "redo restores edit");
        assert_eq!(
            load_and_replay(&ps.db, None).unwrap(),
            updated.trees,
            "replay after redo reproduces edited trees"
        );

        // Stacks: a second undo works; a third returns false.
        assert!(ps.undo().unwrap());
        assert!(!ps.undo().unwrap(), "undo on empty stack returns false");
    }

    // E4 — DeltaApply recovery path: inject a validly-encoded delta that fails
    // during tree application (references a hash not in the adjacency list).
    // Verifies that replay_error_row_id returns the injected row's id, so the
    // mutant "always return 1" is caught alongside the DeltaDecode path in R1.
    #[test]
    fn delta_apply_error_recovers_to_snapshot_inline() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();

        // new_project writes the initial snapshot at row 1.
        let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        drop(ps);

        // Inject a delta that decodes fine but fails to apply: delete at
        // After(fake_hash) where fake_hash is absent from the (empty) tree.
        let fake_hash = Hash([0xABu8; 16]);
        let bad_delta = vec![Delta::delete_after(1, Location::After(fake_hash))];
        let payload = encode_delta_batch(&bad_delta).unwrap();
        let injected_row_id: i64 = {
            let mut db = Db::open(&path).unwrap();
            db.conn_mut()
                .execute(
                    "INSERT INTO journal (type, payload, command_id, applied_at) \
                     VALUES (0, ?1, 0, 0)",
                    rusqlite::params![payload],
                )
                .unwrap();
            db.conn_mut().last_insert_rowid()
        };

        let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
        let ri = outcome
            .recovery
            .expect("DeltaApply replay error must trigger recovery");
        assert_eq!(
            ri.failed_row, injected_row_id,
            "failed_row must match the injected delta row id"
        );
        assert!(ri.snapshot_id > 0, "snapshot_id must be valid");
        assert_eq!(ps2.sample_rate(), 48000);
    }

    // E5 — recovery restores NON-EMPTY post-snapshot content. The other recovery
    // tests (E4 and core/tests/engine_recovery.rs) snapshot an empty project, so
    // they pin that recovery *fires* with the right failed_row/snapshot_id but not
    // that the recovered *content* is the snapshot's. Here: build real two-track
    // content, snapshot it, apply a post-snapshot edit, corrupt that edit's row,
    // and assert open_project rolls back to the exact snapshotted trees with the
    // post-snapshot edit dropped: the recovered ProjectState equals the
    // post-snapshot, pre-corruption state (see `design/data-model.md` § Load /
    // replay). Cannot live in core/tests/ — apply_batch is pub(crate).
    #[test]
    fn recovery_restores_post_snapshot_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Content: label L on track 0, turn A on track 1, then turn B appended.
        ps.apply_batch(
            &[
                BatchOp {
                    track_id: 0,
                    sample: 0,
                    kind: BatchOpKind::Insert(label_elem(1, 24000)),
                },
                BatchOp {
                    track_id: 1,
                    sample: 0,
                    kind: BatchOpKind::Insert(turn_elem(1, 48000)),
                },
            ],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 48000,
                kind: BatchOpKind::Insert(turn_elem(2, 24000)),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        // Snapshot WITH content; capture the snapshotted trees for comparison.
        ps.save_snapshot_now().unwrap();
        let expected = ps.current.trees.clone();

        // Post-snapshot edit: append turn C on track 1. Its delta row is the one
        // we corrupt — recovery must discard exactly this edit.
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 72000,
                kind: BatchOpKind::Insert(turn_elem(3, 12000)),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        assert_eq!(
            speech_hashes(&ps, 1).len(),
            3,
            "pre-corruption state has all three turns"
        );
        assert_ne!(
            ps.current.trees, expected,
            "post-snapshot edit must differ from the snapshot"
        );
        drop(ps);

        // Corrupt the trailing type=0 row (the post-snapshot edit) so it won't decode.
        let corrupt_row_id: i64 = {
            let mut db = Db::open(&path).unwrap();
            let id: i64 = db
                .conn()
                .query_row("SELECT MAX(id) FROM journal WHERE type = 0", [], |r| {
                    r.get(0)
                })
                .unwrap();
            db.conn_mut()
                .execute(
                    "UPDATE journal SET payload = X'DEADBEEF' WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap();
            id
        };

        // Open: replay fails on the corrupt row → fallback to the content snapshot.
        let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
        let ri = outcome
            .recovery
            .expect("corrupt post-snapshot row must trigger recovery");
        assert_eq!(
            ri.failed_row, corrupt_row_id,
            "failed_row must name the corrupt post-snapshot row"
        );
        assert!(ri.snapshot_id > 0, "snapshot_id must be valid");
        assert!(outcome.missing_tracks.is_empty());

        // The crux: recovered trees equal the SNAPSHOTTED content — not the empty
        // default, and not the post-snapshot edit. The dropped turn C is gone.
        assert_eq!(
            ps2.current.trees, expected,
            "recovery must restore the exact snapshotted trees"
        );
        assert_eq!(
            label_hashes(&ps2, 0).len(),
            1,
            "track 0 label survives recovery"
        );
        assert_eq!(
            speech_hashes(&ps2, 1).len(),
            2,
            "post-snapshot turn C must be dropped, leaving A and B"
        );
    }

    // E6 — recovery pins METADATA to the snapshot point, not the latest row.
    // Regression for the open_project asymmetry: timeline-tail recovery rolled the
    // trees back to the snapshot but `load_current_metadata(.., None)` still loaded
    // the absolute-latest `type = -1` row, so a metadata write made *after* the
    // snapshot surfaced against a rolled-back timeline. Here a "before" metadata is
    // written pre-snapshot and an "after" metadata post-snapshot; after corrupting
    // the post-snapshot delta, recovery must restore the "before" metadata.
    #[test]
    fn recovery_pins_metadata_to_snapshot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Helper: journal a name-only metadata blob as a `type = -1` row.
        let write_meta = |ps: &ProjectState, name: &str| {
            let meta = Metadata {
                project: ProjectMeta {
                    name: Some(name.to_string()),
                    aligned_groups: vec![],
                },
                tracks: vec![],
                speakers: vec![],
            };
            let (mh, mbytes) = encode_metadata(&meta).unwrap();
            store::put(ps.db.conn(), &mh, &mbytes).unwrap();
            journal::append_metadata(ps.db.conn(), CommandId::Unknown, &mh, 0).unwrap();
        };

        // Pre-snapshot: "before" metadata, then real timeline content.
        write_meta(&ps, "before");
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 0,
                kind: BatchOpKind::Insert(turn_elem(1, 48000)),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        // Snapshot: the point recovery will roll back to.
        ps.save_snapshot_now().unwrap();

        // Post-snapshot: "after" metadata, then a delta edit (the row we corrupt).
        write_meta(&ps, "after");
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 48000,
                kind: BatchOpKind::Insert(turn_elem(2, 24000)),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        drop(ps);

        // Corrupt the trailing type=0 row so replay falls back to the snapshot.
        {
            let mut db = Db::open(&path).unwrap();
            let id: i64 = db
                .conn()
                .query_row("SELECT MAX(id) FROM journal WHERE type = 0", [], |r| {
                    r.get(0)
                })
                .unwrap();
            db.conn_mut()
                .execute(
                    "UPDATE journal SET payload = X'DEADBEEF' WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap();
        }

        let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
        assert!(
            outcome.recovery.is_some(),
            "corrupt post-snapshot row must trigger recovery"
        );
        // The crux: metadata is pinned to the snapshot point, so the post-snapshot
        // "after" write is dropped alongside the rolled-back timeline tail.
        assert_eq!(
            ps2.current.metadata.project.name.as_deref(),
            Some("before"),
            "recovery must load metadata as of the snapshot, not the latest row"
        );
    }

    // read-accessors expose the live state without holding a Db or mutating.
    // A synthetic project receives a turn on track 1 plus a metadata row carrying one
    // track and one speaker; trees() / tracks() / speakers() must round-trip those
    // inputs, and vbdata_dir() / project_dir() must derive from the project file path.
    #[test]
    fn accessors_expose_read_state() {
        use crate::project::metadata::{ModelUse, SourceType, SpeakerMeta, TrackMeta};

        let dir = tempdir().unwrap();
        let path = dir.path().join("acc.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // vbdata_dir / project_dir derive from the project file path, not the live state.
        assert_eq!(ps.vbdata_dir(), dir.path().join("acc.vbdata"));
        assert_eq!(ps.project_dir(), dir.path());

        // Empty project: no trees, no tracks, no speakers.
        assert!(ps.trees().is_empty(), "fresh project has no trees");
        assert!(ps.tracks().is_empty(), "fresh project has no tracks");
        assert!(ps.speakers().is_empty(), "fresh project has no speakers");

        let track = TrackMeta {
            id: 1,
            name: "Host".to_string(),
            source_type: SourceType::File,
            source_path_relative: "host.wav".to_string(),
            source_path_absolute: "/tmp/host.wav".to_string(),
            codec: "pcm".to_string(),
            source_sample_rate: 48000,
            source_channels: 1,
            project_start_sample: 0,
            original_length_samples: 100,
            cut_length_samples: 0,
            drift_ppm: 0.0,
            room_tone_hash: None,
            models_used: ModelUse::default(),
            wet_dry_ratio: 0.0,
            disfluencies_identified: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let speaker = SpeakerMeta {
            id: 7,
            name: "Alice".to_string(),
            color_hint: None,
            embedding_hash: None,
            track_ids: vec![1],
        };
        let meta = Metadata {
            project: ProjectMeta::default(),
            tracks: vec![track.clone()],
            speakers: vec![speaker.clone()],
        };

        // One combined tree+metadata edit: insert a turn on track 1 and set metadata.
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 0,
                kind: BatchOpKind::Insert(turn_elem(1, 100)),
            }],
            Some(meta),
            CommandId::Unknown,
        )
        .unwrap();

        // trees(): the live track-1 tree is present and is a Speech tree.
        let trees = ps.trees();
        assert_eq!(trees.len(), 1, "exactly the one inserted track");
        assert!(
            matches!(trees.get(&1), Some(TrackTree::Speech(_))),
            "track 1 is a speech tree"
        );

        // tracks() / speakers(): the metadata round-trips verbatim.
        assert_eq!(
            ps.tracks(),
            &[track],
            "tracks() returns the stored TrackMeta"
        );
        assert_eq!(
            ps.speakers(),
            &[speaker],
            "speakers() returns the stored SpeakerMeta"
        );
    }

    // ── apply_batch ──────────────────────────────────────────────────────────

    fn turn_elem(id: u64, duration: i64) -> NewElement {
        let turn = Turn {
            id,
            speaker_id: None,
            turn_duration: duration,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![],
        };
        let (hash, bytes) = encode_turn(&turn).unwrap();
        NewElement::Turn {
            hash,
            bytes,
            element: Arc::new(turn),
        }
    }

    fn label_elem(id: u64, silence: i64) -> NewElement {
        let label = Label {
            id,
            text: format!("L{id}"),
            kind: LabelKind::Plain,
            post_label_silence: silence,
        };
        let (hash, bytes) = encode_label(&label).unwrap();
        NewElement::Label {
            hash,
            bytes,
            element: Arc::new(label),
        }
    }

    fn speech_hashes(ps: &ProjectState, track_id: u32) -> Vec<Hash> {
        match ps.current.trees.get(&track_id) {
            Some(TrackTree::Speech(t)) => t.iter().map(|e| e.hash).collect(),
            _ => vec![],
        }
    }

    fn label_hashes(ps: &ProjectState, track_id: u32) -> Vec<Hash> {
        match ps.current.trees.get(&track_id) {
            Some(TrackTree::Labels(t)) => t.iter().map(|e| e.hash).collect(),
            _ => vec![],
        }
    }

    // AB1 — descending-order sort: ops provided in ascending sample order on the
    // same track; result must be identical to descending order (verifies sort).
    // Correct inverse capture is validated by undo restoring the exact pre-batch state.
    #[test]
    fn apply_batch_descending_order_on_same_track() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Build [A(0..100), B(100..200), C(200..300)] via three single-op batches.
        let ea = turn_elem(1, 100);
        let ha = ea.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 0,
                kind: BatchOpKind::Insert(ea),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        let eb = turn_elem(2, 100);
        let hb = eb.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 100,
                kind: BatchOpKind::Insert(eb),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        let ec = turn_elem(3, 100);
        let hc = ec.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 200,
                kind: BatchOpKind::Insert(ec),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        assert_eq!(speech_hashes(&ps, 1), vec![ha, hb, hc]);

        // Batch provided in ASCENDING sample order: [Update B at 150, Delete C at 250].
        // apply_batch must sort to descending: delete C (250) before update B (150).
        let eb2 = turn_elem(22, 100);
        let hb2 = eb2.hash();
        ps.apply_batch(
            &[
                BatchOp {
                    track_id: 1,
                    sample: 150,
                    kind: BatchOpKind::Update(eb2),
                },
                BatchOp {
                    track_id: 1,
                    sample: 250,
                    kind: BatchOpKind::Delete,
                },
            ],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        // State: [A, B']
        assert_eq!(speech_hashes(&ps, 1), vec![ha, hb2]);

        // Undo must restore [A, B, C]; wrong inverse order would produce a corrupt state.
        assert!(ps.undo().unwrap());
        assert_eq!(
            speech_hashes(&ps, 1),
            vec![ha, hb, hc],
            "undo must restore [A, B, C]"
        );
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);

        // Redo must restore [A, B'].
        assert!(ps.redo().unwrap());
        assert_eq!(speech_hashes(&ps, 1), vec![ha, hb2]);
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);
    }

    // AB2 — two-track lifecycle: inserts, a delete, an update across two tracks;
    // undo/redo agree with load_and_replay; snapshot + reopen preserve the final state.
    //
    // History is in-memory only (not persisted), so undo/redo are exercised on the
    // original ProjectState before drop; the reopen step verifies snapshot persistence.
    #[test]
    fn apply_batch_two_track_lifecycle() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Batch 1: insert L1 (dur=100) on track 0, T1 (dur=200) on track 1.
        let l1 = label_elem(1, 100);
        let hl1 = l1.hash();
        let t1 = turn_elem(1, 200);
        let ht1 = t1.hash();
        ps.apply_batch(
            &[
                BatchOp {
                    track_id: 0,
                    sample: 0,
                    kind: BatchOpKind::Insert(l1),
                },
                BatchOp {
                    track_id: 1,
                    sample: 0,
                    kind: BatchOpKind::Insert(t1),
                },
            ],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        assert_eq!(label_hashes(&ps, 0), vec![hl1]);
        assert_eq!(speech_hashes(&ps, 1), vec![ht1]);

        ps.save_snapshot_now().unwrap();

        // Batch 2: delete L1 (track 0, sample 50), update T1→T2 (track 1, sample 100).
        // Provided in ascending sample order (50, 100); sort → (100, 50).
        let t2 = turn_elem(2, 200);
        let ht2 = t2.hash();
        ps.apply_batch(
            &[
                BatchOp {
                    track_id: 0,
                    sample: 50,
                    kind: BatchOpKind::Delete,
                },
                BatchOp {
                    track_id: 1,
                    sample: 100,
                    kind: BatchOpKind::Update(t2),
                },
            ],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        // State: track 0 empty, track 1 = [T2].
        assert_eq!(label_hashes(&ps, 0), vec![]);
        assert_eq!(speech_hashes(&ps, 1), vec![ht2]);
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);

        // Undo batch 2 (on original ps — history is in-memory, not persisted across reopen).
        assert!(ps.undo().unwrap());
        assert_eq!(label_hashes(&ps, 0), vec![hl1]);
        assert_eq!(speech_hashes(&ps, 1), vec![ht1]);
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);

        // Redo: track 0 empty, track 1 = [T2].
        assert!(ps.redo().unwrap());
        assert_eq!(label_hashes(&ps, 0), vec![]);
        assert_eq!(speech_hashes(&ps, 1), vec![ht2]);
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);

        // Snapshot, drop, reopen — trees must survive the snapshot/reopen cycle.
        ps.save_snapshot_now().unwrap();
        let saved = ps.current.trees.clone();
        drop(ps);

        let (ps2, outcome) = ProjectState::open_project(&path, &settings).unwrap();
        assert!(outcome.recovery.is_none());
        assert_eq!(
            ps2.current.trees, saved,
            "trees must survive snapshot + reopen"
        );
    }

    // AB3 — undo_history_limit = 0: edit is persisted in the journal but undo is disabled.
    #[test]
    fn apply_batch_zero_undo_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings {
            undo_history_limit: 0,
            ..Settings::default()
        };
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        let t1 = turn_elem(1, 100);
        let ht1 = t1.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 0,
                kind: BatchOpKind::Insert(t1),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        // Edit is in the journal and in current.
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);
        assert_eq!(speech_hashes(&ps, 1), vec![ht1]);

        // Undo disabled (limit = 0).
        assert!(!ps.undo().unwrap(), "undo returns false when limit = 0");
    }

    // AB5 — label insert into an existing Labels tree, and label update, exercise
    // the `Some(TrackTree::Labels(t))` arms in apply_batch. Inserts the first
    // label (creating the track) then a second into the existing tree, then
    // updates the first; verifies undo and journal replay after each step.
    #[test]
    fn apply_batch_label_existing_tree_insert_and_update() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Insert L1 (dur=100) at sample 0 → creates track 0 (None arm).
        let l1 = label_elem(1, 100);
        let hl1 = l1.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 0,
                sample: 0,
                kind: BatchOpKind::Insert(l1),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        assert_eq!(label_hashes(&ps, 0), vec![hl1]);

        // Insert L2 (dur=50) at sample 100 → appends to existing Labels tree
        // (Some(TrackTree::Labels(t)) arm for Insert).
        let l2 = label_elem(2, 50);
        let hl2 = l2.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 0,
                sample: 100,
                kind: BatchOpKind::Insert(l2),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        assert_eq!(label_hashes(&ps, 0), vec![hl1, hl2]);
        // Replay must reproduce the same order: catches >= vs < in insert_location_in_tree
        // (wrong comparison produces Location::Start instead of Location::After(hl1)).
        assert_eq!(
            load_and_replay(&ps.db, None).unwrap(),
            ps.current.trees,
            "append insert must use Location::After(predecessor) in delta"
        );

        // Update L1 at sample 50 → existing Labels tree
        // (Some(TrackTree::Labels(t)) arm for Update).
        let l1b = label_elem(10, 100);
        let hl1b = l1b.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 0,
                sample: 50,
                kind: BatchOpKind::Update(l1b),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        assert_eq!(label_hashes(&ps, 0), vec![hl1b, hl2]);

        // Undo update → [L1, L2].
        assert!(ps.undo().unwrap());
        assert_eq!(label_hashes(&ps, 0), vec![hl1, hl2]);
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);

        // Undo second insert → [L1].
        assert!(ps.undo().unwrap());
        assert_eq!(label_hashes(&ps, 0), vec![hl1]);
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);
    }

    // AB4 — undo_history_limit = 2: the oldest entry is evicted after 3 edits.
    #[test]
    fn apply_batch_undo_limit_eviction() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings {
            undo_history_limit: 2,
            ..Settings::default()
        };
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        let t1 = turn_elem(1, 100);
        let ht1 = t1.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 0,
                kind: BatchOpKind::Insert(t1),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        let t2 = turn_elem(2, 100);
        let ht2 = t2.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 100,
                kind: BatchOpKind::Insert(t2),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        let t3 = turn_elem(3, 100);
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 200,
                kind: BatchOpKind::Insert(t3),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();

        // Undo batch 3: removes T3.
        assert!(ps.undo().unwrap(), "first undo");
        assert_eq!(speech_hashes(&ps, 1), vec![ht1, ht2]);

        // Undo batch 2: removes T2.
        assert!(ps.undo().unwrap(), "second undo");
        assert_eq!(speech_hashes(&ps, 1), vec![ht1]);

        // Batch 1 was evicted (limit = 2 with 3 recorded): returns false; T1 persists.
        assert!(!ps.undo().unwrap(), "third undo: evicted");
        assert_eq!(
            speech_hashes(&ps, 1),
            vec![ht1],
            "T1 from evicted batch persists"
        );
    }

    // ── apply_batch metadata producer ────────────────────────────────────────

    /// Count journal rows of a given `type` value.
    fn journal_row_count(db: &crate::db::Db, row_type: i64) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM journal WHERE type = ?1",
                [row_type],
                |r| r.get(0),
            )
            .unwrap()
    }

    // AB6 — combined tree+metadata edit: both a type=0 and a type=-1 row are
    // appended in one apply_batch call; undo reverts trees AND metadata; redo
    // reapplies both; load_and_replay and load_current_metadata agree after each.
    //
    // Baseline has track 1 = [T1] (snapshot captured) so that undo does not
    // land on a fully-empty track state — keeping load_and_replay comparable.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn apply_batch_combined_tree_and_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        // Baseline: insert T1 on track 1 then snapshot.
        let t1 = turn_elem(1, 200);
        let ht1 = t1.hash();
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 0,
                kind: BatchOpKind::Insert(t1),
            }],
            None,
            CommandId::Unknown,
        )
        .unwrap();
        ps.save_snapshot_now().unwrap();

        // Record row counts before the combined edit.
        let delta_before = journal_row_count(&ps.db, 0);
        let meta_before = journal_row_count(&ps.db, -1);

        // Combined edit: append T2 on track 1 + change project name.
        let t2 = turn_elem(2, 150);
        let ht2 = t2.hash();
        let after_meta = Metadata {
            project: ProjectMeta {
                name: Some("my project".to_string()),
                aligned_groups: vec![],
            },
            tracks: vec![],
            speakers: vec![],
        };
        ps.apply_batch(
            &[BatchOp {
                track_id: 1,
                sample: 200,
                kind: BatchOpKind::Insert(t2),
            }],
            Some(after_meta.clone()),
            CommandId::Unknown,
        )
        .unwrap();

        // Both a type=0 and a type=-1 row were appended in the one call.
        assert_eq!(
            journal_row_count(&ps.db, 0) - delta_before,
            1,
            "one type=0 row added"
        );
        assert_eq!(
            journal_row_count(&ps.db, -1) - meta_before,
            1,
            "one type=-1 row added"
        );

        // current reflects both trees and metadata.
        assert_eq!(speech_hashes(&ps, 1), vec![ht1, ht2]);
        assert_eq!(
            ps.current.metadata.project.name,
            Some("my project".to_string())
        );
        assert_eq!(load_and_replay(&ps.db, None).unwrap(), ps.current.trees);
        assert_eq!(
            load_current_metadata(&ps.db, None).unwrap(),
            after_meta,
            "load_current_metadata agrees after combined edit"
        );

        // Undo reverts both trees and metadata.
        assert!(ps.undo().unwrap());
        assert_eq!(
            speech_hashes(&ps, 1),
            vec![ht1],
            "trees reverted to baseline"
        );
        assert_eq!(
            ps.current.metadata.project.name, None,
            "metadata project name reverted"
        );
        assert_eq!(
            load_and_replay(&ps.db, None).unwrap(),
            ps.current.trees,
            "load_and_replay agrees after undo"
        );
        assert_eq!(
            load_current_metadata(&ps.db, None).unwrap(),
            Metadata::default(),
            "load_current_metadata agrees after undo"
        );

        // Redo reapplies both.
        assert!(ps.redo().unwrap());
        assert_eq!(
            speech_hashes(&ps, 1),
            vec![ht1, ht2],
            "trees restored after redo"
        );
        assert_eq!(
            ps.current.metadata.project.name,
            Some("my project".to_string()),
            "metadata restored after redo"
        );
        assert_eq!(
            load_and_replay(&ps.db, None).unwrap(),
            ps.current.trees,
            "load_and_replay agrees after redo"
        );
        assert_eq!(
            load_current_metadata(&ps.db, None).unwrap(),
            after_meta,
            "load_current_metadata agrees after redo"
        );
    }

    // AB7 — metadata-only edit: empty ops + Some(metadata) appends exactly one
    // type=-1 row and no type=0 row; undo reverts; redo reapplies;
    // load_current_metadata agrees after each.
    #[test]
    fn apply_batch_metadata_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        let delta_before = journal_row_count(&ps.db, 0);
        let meta_before = journal_row_count(&ps.db, -1);

        let after_meta = Metadata {
            project: ProjectMeta {
                name: Some("renamed".to_string()),
                aligned_groups: vec![],
            },
            tracks: vec![],
            speakers: vec![],
        };
        ps.apply_batch(&[], Some(after_meta.clone()), CommandId::Unknown)
            .unwrap();

        // No type=0 row; exactly one type=-1 row.
        assert_eq!(
            journal_row_count(&ps.db, 0),
            delta_before,
            "no type=0 row for metadata-only edit"
        );
        assert_eq!(
            journal_row_count(&ps.db, -1) - meta_before,
            1,
            "exactly one type=-1 row"
        );

        // current.metadata updated; trees unchanged (empty).
        assert_eq!(
            ps.current.metadata.project.name,
            Some("renamed".to_string())
        );
        assert!(ps.current.trees.is_empty(), "trees untouched");
        assert_eq!(
            load_current_metadata(&ps.db, None).unwrap(),
            after_meta,
            "load_current_metadata agrees after metadata-only edit"
        );

        // Undo reverts metadata; no trees change.
        assert!(ps.undo().unwrap());
        assert_eq!(
            ps.current.metadata.project.name, None,
            "metadata reverted after undo"
        );
        assert!(ps.current.trees.is_empty());
        assert_eq!(
            load_current_metadata(&ps.db, None).unwrap(),
            Metadata::default(),
            "load_current_metadata agrees after undo"
        );

        // Redo reapplies metadata.
        assert!(ps.redo().unwrap());
        assert_eq!(
            ps.current.metadata.project.name,
            Some("renamed".to_string()),
            "metadata restored after redo"
        );
        assert_eq!(
            load_current_metadata(&ps.db, None).unwrap(),
            after_meta,
            "load_current_metadata agrees after redo"
        );
    }

    // ── G1 round-trip fixture ────────────────────────────────────────────────

    use crate::project::metadata::{ModelUse, SourceType, SpeakerMeta, TrackMeta};
    use crate::project::snapshot::{encode_snapshot, snapshot_from_trees};

    struct FixtureSpec {
        sample_rate: u32,
        label: Label,
        turn_a: Turn,
        turn_b: Turn,
        metadata: Metadata,
    }

    fn fixture_spec() -> FixtureSpec {
        FixtureSpec {
            sample_rate: 48000,
            label: Label {
                id: 1,
                text: "Intro".to_string(),
                kind: LabelKind::Plain,
                post_label_silence: 24000,
            },
            turn_a: Turn {
                id: 1,
                speaker_id: Some(1),
                turn_duration: 48000,
                post_turn_silence: 4800,
                words: vec![],
                splices: vec![],
            },
            turn_b: Turn {
                id: 2,
                speaker_id: Some(1),
                turn_duration: 24000,
                post_turn_silence: 0,
                words: vec![],
                splices: vec![],
            },
            metadata: Metadata {
                project: ProjectMeta {
                    name: Some("Fixture Project".to_string()),
                    aligned_groups: vec![],
                },
                tracks: vec![TrackMeta {
                    id: 1,
                    name: "Host".to_string(),
                    source_type: SourceType::File,
                    source_path_relative: "audio/host.wav".to_string(),
                    source_path_absolute: "/nonexistent/vocalboard-fixture/host.wav".to_string(),
                    codec: "wav".to_string(),
                    source_sample_rate: 48000,
                    source_channels: 1,
                    project_start_sample: 0,
                    original_length_samples: 48000,
                    cut_length_samples: 0,
                    drift_ppm: 0.0,
                    room_tone_hash: None,
                    models_used: ModelUse::default(),
                    wet_dry_ratio: 0.0,
                    disfluencies_identified: false,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                }],
                speakers: vec![SpeakerMeta {
                    id: 1,
                    name: "Alice".to_string(),
                    color_hint: Some("#3366cc".to_string()),
                    embedding_hash: None,
                    track_ids: vec![1],
                }],
            },
        }
    }

    // F1 — generate the committed v1 fixture. Not run in CI; run manually with:
    //   cargo test -p core --lib -- --ignored gen_fixture --nocapture
    #[test]
    #[ignore]
    fn gen_fixture() {
        use std::path::PathBuf;
        let fixture_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/project_v1.vocalboard"
        ));
        if fixture_path.exists() {
            std::fs::remove_file(&fixture_path).unwrap();
        }
        std::fs::create_dir_all(fixture_path.parent().unwrap()).unwrap();

        let spec = fixture_spec();

        let mut db = Db::create(&fixture_path).unwrap();
        {
            let tx = db.conn_mut().transaction().unwrap();
            db_project::insert_project_row(&tx, spec.sample_rate).unwrap();

            let (h_l, l_bytes) = encode_label(&spec.label).unwrap();
            store::put(&tx, &h_l, &l_bytes).unwrap();

            let (h_a, a_bytes) = encode_turn(&spec.turn_a).unwrap();
            store::put(&tx, &h_a, &a_bytes).unwrap();

            let (h_b, b_bytes) = encode_turn(&spec.turn_b).unwrap();
            store::put(&tx, &h_b, &b_bytes).unwrap();

            let mut trees = PerTrackTrees::new();
            trees.insert(
                0,
                TrackTree::Labels(ImplicitTimelineTree::from_sorted_elements(vec![(
                    h_l,
                    Arc::new(spec.label.clone()),
                )])),
            );
            trees.insert(
                1,
                TrackTree::Speech(ImplicitTimelineTree::from_sorted_elements(vec![(
                    h_a,
                    Arc::new(spec.turn_a.clone()),
                )])),
            );
            let snap = snapshot_from_trees(&trees);
            let (h_s, s_bytes) = encode_snapshot(&snap).unwrap();
            store::put(&tx, &h_s, &s_bytes).unwrap();
            journal::append_snapshot(&tx, CommandId::Unknown, &h_s, 1_700_000_000).unwrap();

            let delta_payload =
                encode_delta_batch(&[Delta::insert_after(1, Location::After(h_a), h_b)]).unwrap();
            journal::append_delta_batch(&tx, CommandId::Unknown, &delta_payload, 1_700_000_001)
                .unwrap();

            let (h_m, m_bytes) = encode_metadata(&spec.metadata).unwrap();
            store::put(&tx, &h_m, &m_bytes).unwrap();
            journal::append_metadata(&tx, CommandId::Unknown, &h_m, 1_700_000_002).unwrap();

            tx.commit().unwrap();
        }

        // Collapse WAL into main file so the committed fixture is a single file.
        db.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")
            .unwrap();
        drop(db);

        println!("Generated fixture: {}", fixture_path.display());
    }

    // F2 — G1 authoritative round-trip: opens the committed fixture via a temp copy
    // and asserts deep tree + metadata equality against fixture_spec().
    #[test]
    fn g1_fixture_round_trip() {
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/project_v1.vocalboard"
        ));
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        std::fs::write(&path, bytes).unwrap();

        let settings = Settings::default();
        let (ps, outcome) = ProjectState::open_project(&path, &settings).unwrap();

        assert_eq!(ps.sample_rate(), 48000);
        assert!(
            outcome.recovery.is_none(),
            "fixture must have clean journal"
        );
        assert_eq!(
            outcome.missing_tracks,
            vec![1],
            "fixture track 1 has nonexistent paths → missing"
        );

        let spec = fixture_spec();

        // Recompute hashes from spec to use as the tripwire.
        let (h_l, _) = encode_label(&spec.label).unwrap();
        let (h_a, _) = encode_turn(&spec.turn_a).unwrap();
        let (h_b, _) = encode_turn(&spec.turn_b).unwrap();

        // Track 0 (labels): one element.
        match ps.current.trees.get(&0).unwrap() {
            TrackTree::Labels(t) => {
                let elems: Vec<_> = t.iter().collect();
                assert_eq!(elems.len(), 1);
                assert_eq!(elems[0].hash, h_l);
                assert_eq!(elems[0].element.text, "Intro");
                assert_eq!(elems[0].element.post_label_silence, 24000);
            }
            _ => panic!("track 0 must be Labels"),
        }

        // Track 1 (speech): [hA, hB] in order.
        match ps.current.trees.get(&1).unwrap() {
            TrackTree::Speech(t) => {
                let elems: Vec<_> = t.iter().collect();
                assert_eq!(elems.len(), 2);
                assert_eq!(elems[0].hash, h_a);
                assert_eq!(elems[0].element.id, 1);
                assert_eq!(elems[0].element.turn_duration, 48000);
                assert_eq!(elems[1].hash, h_b);
                assert_eq!(elems[1].element.id, 2);
                assert_eq!(elems[1].element.turn_duration, 24000);
            }
            _ => panic!("track 1 must be Speech"),
        }

        assert_eq!(
            ps.current.metadata, spec.metadata,
            "metadata must round-trip through the committed fixture"
        );
    }

    // RT1 — open_project loads a persisted room-tone blob into ProjectState.room_tones.
    #[test]
    fn open_project_loads_room_tone() {
        use crate::audio::room_tone::{encode_room_tone, RoomTone};
        use crate::db::{journal, store};
        use crate::project::metadata::{encode_metadata, ModelUse, SourceType, TrackMeta};

        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.vocalboard");
        let settings = Settings::default();

        // Create a project and write a room-tone blob + track metadata referencing it.
        let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        let rt = RoomTone {
            samples: vec![0.0f32; 480],
            channels: 1,
            sample_rate: 48000,
            rms: 0.01,
        };
        let (rt_hash, rt_bytes) = encode_room_tone(&rt).unwrap();
        store::put(ps.db.conn(), &rt_hash, &rt_bytes).unwrap();

        let meta = Metadata {
            project: ProjectMeta {
                name: None,
                aligned_groups: vec![],
            },
            tracks: vec![TrackMeta {
                id: 1,
                name: "Host".to_string(),
                source_type: SourceType::File,
                source_path_relative: "audio/host.wav".to_string(),
                source_path_absolute: "/nonexistent/host.wav".to_string(),
                codec: "wav".to_string(),
                source_sample_rate: 48000,
                source_channels: 1,
                project_start_sample: 0,
                original_length_samples: 480,
                cut_length_samples: 0,
                drift_ppm: 0.0,
                room_tone_hash: Some(rt_hash),
                models_used: ModelUse::default(),
                wet_dry_ratio: 0.0,
                disfluencies_identified: false,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            }],
            speakers: vec![],
        };
        let (mh, mbytes) = encode_metadata(&meta).unwrap();
        store::put(ps.db.conn(), &mh, &mbytes).unwrap();
        journal::append_metadata(ps.db.conn(), CommandId::Unknown, &mh, 0).unwrap();
        drop(ps);

        // Re-open and verify room_tone is resident.
        let (ps2, _) = ProjectState::open_project(&path, &settings).unwrap();
        let loaded = ps2
            .room_tone(1)
            .expect("RT1: room_tone must be resident after open");
        assert_eq!(loaded.samples.len(), 480, "RT1: sample count matches");
        assert_eq!(
            loaded.samples.len(),
            rt.samples.len(),
            "RT1: derived length matches blob"
        );
    }

    // AB8 — no-op: empty ops + None metadata returns Ok and adds no journal rows.
    #[test]
    fn apply_batch_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        let total_before: i64 = ps
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();

        ps.apply_batch(&[], None, CommandId::Unknown).unwrap();

        let total_after: i64 = ps
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_before, total_after, "no-op must add no journal rows");
        assert!(
            !ps.history.can_undo(),
            "no-op must not be recorded in history"
        );
    }

    // insert_room_tone actually stores the tone, retrievable per-track by room_tone().
    #[test]
    fn insert_room_tone_round_trips() {
        use crate::audio::room_tone::RoomTone;

        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.vocalboard");
        let settings = Settings::default();
        let mut ps = ProjectState::new_project(&path, 48000, &settings).unwrap();

        assert!(ps.room_tone(1).is_none(), "absent before insert");
        let rt = Arc::new(RoomTone {
            samples: vec![0.25f32; 96],
            channels: 2,
            sample_rate: 48000,
            rms: 0.25,
        });
        ps.insert_room_tone(1, Arc::clone(&rt));

        let got = ps.room_tone(1).expect("present after insert");
        assert_eq!(
            got.samples.len(),
            96,
            "stored tone retrievable for the track"
        );
        assert_eq!(got.channels, 2);
        assert!(ps.room_tone(2).is_none(), "other tracks unaffected");
    }

    // now_posix returns a plausible *current* UTC timestamp, not a fixed sentinel.
    #[test]
    fn now_posix_is_a_recent_timestamp() {
        // 1_700_000_000 = 2023-11-14; the wall clock is well past it (and far from 0/1/-1).
        assert!(
            now_posix() >= 1_700_000_000,
            "now_posix must report the real clock, got {}",
            now_posix()
        );
    }

    // ProjectState's Debug surfaces the sample rate (not an empty/blank impl).
    #[test]
    fn project_state_debug_shows_sample_rate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dbg.vocalboard");
        let settings = Settings::default();
        let ps = ProjectState::new_project(&path, 48000, &settings).unwrap();
        let s = format!("{ps:?}");
        assert!(s.contains("ProjectState"), "debug names the struct: {s}");
        assert!(s.contains("48000"), "debug shows the sample rate: {s}");
    }
}
