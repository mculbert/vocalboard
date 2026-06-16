//! Global non-timeline metadata: project/track/speaker state in a single
//! content-addressed `Kind::Metadata` blob, recorded by `type = -1` journal
//! rows (most-recent-wins, no replay). Also the pure source-file resolution
//! that produces the missing-track list on open.
//!
//! See [data-model.md § Non-timeline data](../../../design/data-model.md#non-timeline-data)
//! and [§ Audio file resolution](../../../design/data-model.md#audio-file-resolution).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};
use crate::db::journal;
use crate::db::store::{self, StoreError};
use crate::db::Db;

/// Format version emitted by every new [`encode_metadata`] call.
pub const LATEST_METADATA_VERSION: u8 = 1;

/// The global non-timeline metadata object: one blob, most-recent-wins.
///
/// **Canonical order** (required for deterministic hashing): `tracks` and
/// `speakers` ascending by `id`; `SpeakerMeta::track_ids` ascending;
/// `ProjectMeta::aligned_groups` inner groups ascending, groups ordered by
/// first id. Maintaining order is the *producer's* job; [`encode_metadata`]
/// fires a `debug_assert!` in debug/test builds.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Metadata {
    /// Project-scoped mutable state. (NB: distinct from the `project`
    /// SQLite singleton table — see data-model.md § Non-timeline data.)
    pub project: ProjectMeta,
    /// Track metadata, **canonical order: ascending by `id`**. Track 0 (labels)
    /// is implicit, not listed.
    pub tracks: Vec<TrackMeta>,
    /// Speaker metadata, **canonical order: ascending by `id`**.
    pub speakers: Vec<SpeakerMeta>,
}

/// Project-scoped mutable metadata stored in the [`Metadata`] blob.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ProjectMeta {
    /// Optional project name.
    pub name: Option<String>,
    /// Sets of `track_id`s aligned together, e.g. `[[1,2,4],[5,6]]`.
    /// **Canonical order:** each inner group ascending, groups ordered by first id.
    pub aligned_groups: Vec<Vec<u32>>,
}

/// Per-track metadata stored in the [`Metadata`] blob.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackMeta {
    /// Persistent track ID.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Source kind.
    pub source_type: SourceType,
    /// Relative path from the project directory (uses `/` separators).
    pub source_path_relative: String,
    /// Absolute path on disk at import time.
    pub source_path_absolute: String,
    /// Audio codec identifier.
    pub codec: String,
    /// Source file's native sample rate.
    pub source_sample_rate: u32,
    /// Source file's channel count.
    pub source_channels: u16,
    /// Offset of the track's first sample in the project timeline (project-rate samples).
    pub project_start_sample: i64,
    /// Length of the source audio in project-rate samples.
    pub original_length_samples: i64,
    /// Number of project-rate samples cut from this track.
    pub cut_length_samples: i64,
    /// Clock-drift correction in parts per million.
    pub drift_ppm: f64,
    /// Hash of the room-tone PCM blob, if recorded.
    pub room_tone_hash: Option<Hash>,
    /// Models applied to this track.
    pub models_used: ModelUse,
    /// Wet/dry mix ratio for the enhancer output (0.0 = dry, 1.0 = wet).
    pub wet_dry_ratio: f32,
    /// Whether disfluency identification has been run on this track.
    pub disfluencies_identified: bool,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// ISO 8601 last-updated timestamp.
    pub updated_at: String,
}

/// Whether a track's audio comes from a file or a live recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceType {
    /// Audio loaded from a file.
    File,
    /// Audio captured via live recording (Phase 3).
    Recording,
}

/// The model applied to a track, one identifier per role.
///
/// The role set is fixed (it mirrors the settings `model_paths` roles) and each
/// role's model is applied to a track at most once, so this is a flat struct of
/// optional model identifiers — not a list, and not timestamped. `None` = that
/// role's model was never run on this track.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelUse {
    /// WhisperX transcription model identifier.
    pub transcription: Option<String>,
    /// VAD model identifier (reserved; unused in Phase 1).
    pub vad: Option<String>,
    /// WhisperX forced-alignment model identifier.
    pub forced_alignment: Option<String>,
    /// MP-SENet enhancement model identifier.
    pub enhancement: Option<String>,
    /// YAMnet sound-classification model identifier.
    pub sound_classification: Option<String>,
    /// Gemma LLM identifier.
    pub llm: Option<String>,
}

/// Per-speaker metadata stored in the [`Metadata`] blob.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerMeta {
    /// Persistent speaker ID.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// UI colour hint (CSS colour string).
    pub color_hint: Option<String>,
    /// Hash of the normalized mean embedding blob in the store.
    pub embedding_hash: Option<Hash>,
    /// Track IDs this speaker appears on, **canonical order: ascending**.
    pub track_ids: Vec<u32>,
}

/// Encode `meta` as the latest Metadata-kind wire format.
///
/// Returns the content-addressing hash and the tagged blob, ready for
/// `store::put`.
///
/// # Panics (debug/test builds only)
/// Fires a `debug_assert!` if `meta` violates canonical ordering — tracks or
/// speakers not ascending by `id`, a `SpeakerMeta::track_ids` not ascending, or
/// an `aligned_groups` inner group / outer order not ascending. Maintaining
/// canonical order is the producer's responsibility.
pub fn encode_metadata(meta: &Metadata) -> Result<(Hash, Vec<u8>), postcard::Error> {
    debug_assert!(
        metadata_is_canonical(meta),
        "encode_metadata: metadata is not in canonical order"
    );
    let v1 = v1::MetadataV1::from(meta);
    encode_tagged(Kind::Metadata, LATEST_METADATA_VERSION, &v1)
}

/// Decode a `Kind::Metadata` blob into the latest in-memory [`Metadata`].
///
/// Verifies the tag is `Kind::Metadata`, dispatches on the version nibble, and
/// upgrades through `From<MetadataV1> for Metadata`. Unknown versions return
/// [`DecodeError::UnknownVersion`]; non-Metadata tags return
/// [`DecodeError::KindMismatch`].
pub fn decode_metadata(bytes: &[u8]) -> Result<Metadata, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (_, version) = parse_tag(bytes[0])?;
    match version {
        1 => {
            let (_, v1_meta): (u8, v1::MetadataV1) = decode_tagged_as(Kind::Metadata, bytes)?;
            Ok(Metadata::from(v1_meta))
        }
        _ => Err(DecodeError::UnknownVersion {
            kind: Kind::Metadata,
            version,
        }),
    }
}

/// Returns `true` iff `meta` satisfies the canonical-order invariant required
/// for deterministic hashing.
///
/// Checks:
/// - `tracks` ascending by `id`
/// - `speakers` ascending by `id`
/// - each `SpeakerMeta::track_ids` ascending
/// - each `aligned_groups` inner group ascending, outer groups ordered by first id
#[cfg_attr(not(debug_assertions), allow(dead_code))]
fn metadata_is_canonical(meta: &Metadata) -> bool {
    // tracks ascending by id
    if !meta.tracks.windows(2).all(|w| w[0].id < w[1].id) {
        return false;
    }
    // speakers ascending by id
    if !meta.speakers.windows(2).all(|w| w[0].id < w[1].id) {
        return false;
    }
    // each speaker's track_ids ascending
    for s in &meta.speakers {
        if !s.track_ids.windows(2).all(|w| w[0] < w[1]) {
            return false;
        }
    }
    // aligned_groups: each inner group ascending
    for g in &meta.project.aligned_groups {
        if !g.windows(2).all(|w| w[0] < w[1]) {
            return false;
        }
    }
    // aligned_groups: outer order by first id
    if !meta.project.aligned_groups.windows(2).all(|w| {
        let first_a = w[0].first().copied().unwrap_or(0);
        let first_b = w[1].first().copied().unwrap_or(0);
        first_a < first_b
    }) {
        return false;
    }
    true
}

/// Errors returned by [`load_current_metadata`].
#[derive(Debug)]
pub(crate) enum MetadataLoadError {
    /// Journal query failed.
    Journal(journal::JournalError),
    /// Blob-store fetch failed.
    Store(StoreError),
    /// Blob decoding failed (kind mismatch, unknown version, postcard error, etc.).
    Decode(DecodeError),
}

impl std::fmt::Display for MetadataLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataLoadError::Journal(e) => write!(f, "journal error: {e}"),
            MetadataLoadError::Store(e) => write!(f, "store error: {e}"),
            MetadataLoadError::Decode(e) => write!(f, "decode error: {e}"),
        }
    }
}

impl std::error::Error for MetadataLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MetadataLoadError::Journal(e) => Some(e),
            MetadataLoadError::Store(e) => Some(e),
            MetadataLoadError::Decode(e) => Some(e),
        }
    }
}

impl From<journal::JournalError> for MetadataLoadError {
    fn from(e: journal::JournalError) -> Self {
        MetadataLoadError::Journal(e)
    }
}

impl From<StoreError> for MetadataLoadError {
    fn from(e: StoreError) -> Self {
        MetadataLoadError::Store(e)
    }
}

impl From<DecodeError> for MetadataLoadError {
    fn from(e: DecodeError) -> Self {
        MetadataLoadError::Decode(e)
    }
}

/// Load the current global metadata: the blob pointed to by the highest-id
/// `type = -1` journal row with `id <= as_of` (or the absolute latest when
/// `as_of` is `None`), or [`Metadata::default()`] if no such row exists (a
/// freshly created project, or an `as_of` before the first metadata write).
/// No replay — each `type = -1` row is a complete object.
pub(crate) fn load_current_metadata(
    db: &Db,
    as_of: Option<i64>,
) -> Result<Metadata, MetadataLoadError> {
    match journal::latest_metadata(db.conn(), as_of)? {
        None => Ok(Metadata::default()),
        Some(row) => {
            let bytes = store::get(db.conn(), &row.hash)?;
            Ok(decode_metadata(&bytes)?)
        }
    }
}

/// Outcome of resolving one track's source file against the project directory.
pub(crate) enum FileResolution {
    /// Relative path resolved on disk — use as-is.
    /// The resolved path is surfaced by the Missing-Files dialog (M6).
    Found(
        #[allow(dead_code)] // M6: Missing-Files dialog will surface the resolved path.
        PathBuf,
    ),
    /// Relative path missing but the stored absolute path exists. Use it; the
    /// stored relative path SHOULD be rewritten (a metadata change). M1 surfaces
    /// `new_relative` for the engine to act on later (deferred to M6).
    FoundViaAbsolute {
        /// The resolved absolute path (surfaced by the Missing-Files dialog in M6).
        #[allow(dead_code)] // M6: Missing-Files dialog will read this path.
        path: PathBuf,
        /// The absolute path string to store as the new relative path until the
        /// engine recomputes the true relative path (M6).
        new_relative: String,
    },
    /// Neither path resolved — the track has a missing source file.
    Missing,
    /// Not a file-backed track (e.g. `Recording`); nothing to resolve.
    NotApplicable,
}

/// Resolve one track's source. Pure: reads the filesystem, writes nothing.
pub(crate) fn resolve_track_source(project_dir: &Path, track: &TrackMeta) -> FileResolution {
    if track.source_type != SourceType::File {
        return FileResolution::NotApplicable;
    }
    let relative = project_dir.join(&track.source_path_relative);
    if relative.exists() {
        return FileResolution::Found(relative);
    }
    let absolute = Path::new(&track.source_path_absolute);
    if absolute.exists() {
        return FileResolution::FoundViaAbsolute {
            path: absolute.to_path_buf(),
            new_relative: track.source_path_absolute.clone(),
        };
    }
    FileResolution::Missing
}

/// IDs of all `source_type = File` tracks that resolve to [`FileResolution::Missing`].
pub(crate) fn missing_tracks(project_dir: &Path, meta: &Metadata) -> Vec<u32> {
    meta.tracks
        .iter()
        .filter_map(|t| {
            if matches!(
                resolve_track_source(project_dir, t),
                FileResolution::Missing
            ) {
                Some(t.id)
            } else {
                None
            }
        })
        .collect()
}

/// V1 wire schema. Field-identical to the M1 in-memory types.
///
/// **Pre-1.0:** MAY be revised if implementation surfaces a missing or wrong
/// field; every revision requires regenerating the pinned hex/hash tests and
/// any committed G1 fixtures, and SHOULD bump `min_app_version`.
/// **Post-1.0:** frozen indefinitely — no field reorders, no enum-variant
/// reorders, no field insertions/deletions. Shape changes go through a new
/// `mod v2`, bumping `LATEST_METADATA_VERSION`, and writing
/// `From<MetadataV2> for Metadata`.
pub mod v1 {
    use serde::{Deserialize, Serialize};

    use super::{Hash, Metadata, ModelUse, ProjectMeta, SourceType, SpeakerMeta, TrackMeta};

    /// Frozen V1 wire representation of [`Metadata`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct MetadataV1 {
        /// Project metadata.
        pub project: ProjectMetaV1,
        /// Track metadata.
        pub tracks: Vec<TrackMetaV1>,
        /// Speaker metadata.
        pub speakers: Vec<SpeakerMetaV1>,
    }

    /// Frozen V1 wire representation of [`ProjectMeta`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct ProjectMetaV1 {
        /// Optional project name.
        pub name: Option<String>,
        /// Aligned track groups.
        pub aligned_groups: Vec<Vec<u32>>,
    }

    /// Frozen V1 wire representation of [`TrackMeta`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct TrackMetaV1 {
        /// Persistent track ID.
        pub id: u32,
        /// Display name.
        pub name: String,
        /// Source kind.
        pub source_type: SourceTypeV1,
        /// Relative source path.
        pub source_path_relative: String,
        /// Absolute source path.
        pub source_path_absolute: String,
        /// Audio codec identifier.
        pub codec: String,
        /// Source sample rate.
        pub source_sample_rate: u32,
        /// Source channel count.
        pub source_channels: u16,
        /// Project-timeline start sample.
        pub project_start_sample: i64,
        /// Original length in project-rate samples.
        pub original_length_samples: i64,
        /// Cut length in project-rate samples.
        pub cut_length_samples: i64,
        /// Clock-drift correction in PPM.
        pub drift_ppm: f64,
        /// Room-tone blob hash.
        pub room_tone_hash: Option<Hash>,
        /// Models applied to this track.
        pub models_used: ModelUseV1,
        /// Wet/dry ratio for the enhancer.
        pub wet_dry_ratio: f32,
        /// Whether disfluency identification has been run.
        pub disfluencies_identified: bool,
        /// ISO 8601 creation timestamp.
        pub created_at: String,
        /// ISO 8601 last-updated timestamp.
        pub updated_at: String,
    }

    /// Frozen V1 wire representation of [`SourceType`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum SourceTypeV1 {
        /// Audio loaded from a file.
        File,
        /// Audio captured via live recording.
        Recording,
    }

    /// Frozen V1 wire representation of [`ModelUse`].
    #[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct ModelUseV1 {
        /// WhisperX transcription model.
        pub transcription: Option<String>,
        /// VAD model (reserved).
        pub vad: Option<String>,
        /// WhisperX forced-alignment model.
        pub forced_alignment: Option<String>,
        /// MP-SENet enhancement model.
        pub enhancement: Option<String>,
        /// YAMnet sound-classification model.
        pub sound_classification: Option<String>,
        /// Gemma LLM.
        pub llm: Option<String>,
    }

    /// Frozen V1 wire representation of [`SpeakerMeta`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SpeakerMetaV1 {
        /// Persistent speaker ID.
        pub id: u32,
        /// Display name.
        pub name: String,
        /// UI colour hint.
        pub color_hint: Option<String>,
        /// Embedding blob hash.
        pub embedding_hash: Option<Hash>,
        /// Track IDs this speaker appears on.
        pub track_ids: Vec<u32>,
    }

    // --- Conversions ---

    impl From<&SourceType> for SourceTypeV1 {
        fn from(v: &SourceType) -> Self {
            match v {
                SourceType::File => SourceTypeV1::File,
                SourceType::Recording => SourceTypeV1::Recording,
            }
        }
    }

    impl From<SourceTypeV1> for SourceType {
        fn from(v: SourceTypeV1) -> Self {
            match v {
                SourceTypeV1::File => SourceType::File,
                SourceTypeV1::Recording => SourceType::Recording,
            }
        }
    }

    impl From<&ModelUse> for ModelUseV1 {
        fn from(v: &ModelUse) -> Self {
            ModelUseV1 {
                transcription: v.transcription.clone(),
                vad: v.vad.clone(),
                forced_alignment: v.forced_alignment.clone(),
                enhancement: v.enhancement.clone(),
                sound_classification: v.sound_classification.clone(),
                llm: v.llm.clone(),
            }
        }
    }

    impl From<ModelUseV1> for ModelUse {
        fn from(v: ModelUseV1) -> Self {
            ModelUse {
                transcription: v.transcription,
                vad: v.vad,
                forced_alignment: v.forced_alignment,
                enhancement: v.enhancement,
                sound_classification: v.sound_classification,
                llm: v.llm,
            }
        }
    }

    impl From<&TrackMeta> for TrackMetaV1 {
        fn from(v: &TrackMeta) -> Self {
            TrackMetaV1 {
                id: v.id,
                name: v.name.clone(),
                source_type: SourceTypeV1::from(&v.source_type),
                source_path_relative: v.source_path_relative.clone(),
                source_path_absolute: v.source_path_absolute.clone(),
                codec: v.codec.clone(),
                source_sample_rate: v.source_sample_rate,
                source_channels: v.source_channels,
                project_start_sample: v.project_start_sample,
                original_length_samples: v.original_length_samples,
                cut_length_samples: v.cut_length_samples,
                drift_ppm: v.drift_ppm,
                room_tone_hash: v.room_tone_hash,
                models_used: ModelUseV1::from(&v.models_used),
                wet_dry_ratio: v.wet_dry_ratio,
                disfluencies_identified: v.disfluencies_identified,
                created_at: v.created_at.clone(),
                updated_at: v.updated_at.clone(),
            }
        }
    }

    impl From<TrackMetaV1> for TrackMeta {
        fn from(v: TrackMetaV1) -> Self {
            TrackMeta {
                id: v.id,
                name: v.name,
                source_type: SourceType::from(v.source_type),
                source_path_relative: v.source_path_relative,
                source_path_absolute: v.source_path_absolute,
                codec: v.codec,
                source_sample_rate: v.source_sample_rate,
                source_channels: v.source_channels,
                project_start_sample: v.project_start_sample,
                original_length_samples: v.original_length_samples,
                cut_length_samples: v.cut_length_samples,
                drift_ppm: v.drift_ppm,
                room_tone_hash: v.room_tone_hash,
                models_used: ModelUse::from(v.models_used),
                wet_dry_ratio: v.wet_dry_ratio,
                disfluencies_identified: v.disfluencies_identified,
                created_at: v.created_at,
                updated_at: v.updated_at,
            }
        }
    }

    impl From<&SpeakerMeta> for SpeakerMetaV1 {
        fn from(v: &SpeakerMeta) -> Self {
            SpeakerMetaV1 {
                id: v.id,
                name: v.name.clone(),
                color_hint: v.color_hint.clone(),
                embedding_hash: v.embedding_hash,
                track_ids: v.track_ids.clone(),
            }
        }
    }

    impl From<SpeakerMetaV1> for SpeakerMeta {
        fn from(v: SpeakerMetaV1) -> Self {
            SpeakerMeta {
                id: v.id,
                name: v.name,
                color_hint: v.color_hint,
                embedding_hash: v.embedding_hash,
                track_ids: v.track_ids,
            }
        }
    }

    impl From<&ProjectMeta> for ProjectMetaV1 {
        fn from(v: &ProjectMeta) -> Self {
            ProjectMetaV1 {
                name: v.name.clone(),
                aligned_groups: v.aligned_groups.clone(),
            }
        }
    }

    impl From<ProjectMetaV1> for ProjectMeta {
        fn from(v: ProjectMetaV1) -> Self {
            ProjectMeta {
                name: v.name,
                aligned_groups: v.aligned_groups,
            }
        }
    }

    impl From<&Metadata> for MetadataV1 {
        fn from(v: &Metadata) -> Self {
            MetadataV1 {
                project: ProjectMetaV1::from(&v.project),
                tracks: v.tracks.iter().map(TrackMetaV1::from).collect(),
                speakers: v.speakers.iter().map(SpeakerMetaV1::from).collect(),
            }
        }
    }

    impl From<MetadataV1> for Metadata {
        fn from(v: MetadataV1) -> Self {
            Metadata {
                project: ProjectMeta::from(v.project),
                tracks: v.tracks.into_iter().map(TrackMeta::from).collect(),
                speakers: v.speakers.into_iter().map(SpeakerMeta::from).collect(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::db::journal::append_metadata;
    use crate::db::store;
    use crate::project::command_id::CommandId;
    use crate::project::turn::{encode_turn, Turn};

    /// Non-trivial, deterministic, canonical-order metadata for pinned tests.
    fn sample_metadata() -> Metadata {
        Metadata {
            project: ProjectMeta {
                name: Some("My Project".to_string()),
                aligned_groups: vec![vec![1, 2]],
            },
            tracks: vec![TrackMeta {
                id: 1,
                name: "Host".to_string(),
                source_type: SourceType::File,
                source_path_relative: "audio/host.wav".to_string(),
                source_path_absolute: "/recordings/host.wav".to_string(),
                codec: "wav".to_string(),
                source_sample_rate: 48000,
                source_channels: 1,
                project_start_sample: 0,
                original_length_samples: 480000,
                cut_length_samples: 4800,
                drift_ppm: 0.0,
                room_tone_hash: Some(Hash([0xAA; 16])),
                models_used: ModelUse {
                    transcription: Some("whisperx-large-v3".to_string()),
                    vad: None,
                    forced_alignment: None,
                    enhancement: Some("mpsenet-v1".to_string()),
                    sound_classification: None,
                    llm: None,
                },
                wet_dry_ratio: 0.8,
                disfluencies_identified: true,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-02T00:00:00Z".to_string(),
            }],
            speakers: vec![SpeakerMeta {
                id: 1,
                name: "Alice".to_string(),
                color_hint: Some("#ff0000".to_string()),
                embedding_hash: Some(Hash([0xBB; 16])),
                track_ids: vec![1, 2],
            }],
        }
    }

    fn open_tmp_db() -> (tempfile::TempDir, crate::db::Db) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = crate::db::Db::create(&path).unwrap();
        (dir, db)
    }

    // M1
    #[test]
    fn metadata_round_trips() {
        let sample = sample_metadata();
        let (_, bytes) = encode_metadata(&sample).unwrap();
        let decoded = decode_metadata(&bytes).unwrap();
        assert_eq!(decoded, sample);
    }

    // M2
    #[test]
    fn metadata_default_round_trips() {
        let def = Metadata::default();
        let (_, bytes) = encode_metadata(&def).unwrap();
        let decoded = decode_metadata(&bytes).unwrap();
        assert_eq!(decoded, def);
    }

    // M3
    #[test]
    fn decode_metadata_rejects_wrong_kind() {
        let turn = Turn {
            id: 1,
            speaker_id: None,
            turn_duration: 100,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![],
        };
        let (_, bytes) = encode_turn(&turn).unwrap();
        let err = decode_metadata(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::KindMismatch {
                    expected: Kind::Metadata,
                    ..
                }
            ),
            "expected KindMismatch, got: {err:?}"
        );
    }

    // M4
    #[test]
    fn decode_metadata_rejects_empty() {
        let err = decode_metadata(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    // M5
    #[test]
    fn decode_metadata_rejects_unknown_version() {
        use crate::project::hash::tag_byte;
        let tag = tag_byte(Kind::Metadata, 2);
        let bytes = vec![tag, 0x00];
        let err = decode_metadata(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::UnknownVersion {
                    kind: Kind::Metadata,
                    version: 2,
                }
            ),
            "expected UnknownVersion {{ kind: Metadata, version: 2 }}, got: {err:?}"
        );
    }

    // M6
    #[test]
    fn v1_wire_format_pinned() {
        let (_, bytes) = encode_metadata(&sample_metadata()).unwrap();
        assert_eq!(
            bytes.as_slice(),
            &PINNED_WIRE_BYTES,
            "V1 wire format changed — regenerate pinned bytes"
        );
    }

    // M7
    #[test]
    fn v1_wire_hash_pinned() {
        let (hash, _) = encode_metadata(&sample_metadata()).unwrap();
        assert_eq!(
            hash.0, PINNED_HASH,
            "V1 hash changed — regenerate pinned hash"
        );
    }

    // M8 — capture helper; run with --ignored --nocapture to regenerate
    #[test]
    #[ignore]
    fn capture_pinned_values() {
        let (hash, bytes) = encode_metadata(&sample_metadata()).unwrap();
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

    // M9
    #[test]
    fn v1_conversions_total_round_trip() {
        let s = sample_metadata();
        let restored = Metadata::from(v1::MetadataV1::from(&s));
        assert_eq!(restored, s);
    }

    // M10
    #[test]
    #[allow(clippy::too_many_lines)]
    fn metadata_is_canonical_predicate() {
        assert!(metadata_is_canonical(&sample_metadata()));
        assert!(metadata_is_canonical(&Metadata::default()));

        // tracks out of id order
        let mut m = sample_metadata();
        m.tracks.push(TrackMeta {
            id: 0,
            name: "Early".to_string(),
            source_type: SourceType::File,
            source_path_relative: String::new(),
            source_path_absolute: String::new(),
            codec: String::new(),
            source_sample_rate: 48000,
            source_channels: 1,
            project_start_sample: 0,
            original_length_samples: 0,
            cut_length_samples: 0,
            drift_ppm: 0.0,
            room_tone_hash: None,
            models_used: ModelUse::default(),
            wet_dry_ratio: 0.0,
            disfluencies_identified: false,
            created_at: String::new(),
            updated_at: String::new(),
        });
        assert!(!metadata_is_canonical(&m), "tracks out of id order");

        // speakers out of id order
        let mut m = sample_metadata();
        m.speakers.push(SpeakerMeta {
            id: 0,
            name: "Early".to_string(),
            color_hint: None,
            embedding_hash: None,
            track_ids: vec![],
        });
        assert!(!metadata_is_canonical(&m), "speakers out of id order");

        // speaker track_ids descending
        let mut m = sample_metadata();
        m.speakers[0].track_ids = vec![2, 1];
        assert!(!metadata_is_canonical(&m), "track_ids descending");

        // aligned_groups inner group descending
        let mut m = sample_metadata();
        m.project.aligned_groups[0] = vec![2, 1];
        assert!(
            !metadata_is_canonical(&m),
            "aligned_groups inner group descending"
        );

        // aligned_groups outer order swapped
        let mut m = sample_metadata();
        m.project.aligned_groups = vec![vec![5, 6], vec![1, 2]];
        assert!(
            !metadata_is_canonical(&m),
            "aligned_groups outer order swapped"
        );

        // Two ascending tracks in canonical order — exercises the track-id comparison
        // so that mutating < to == is detected (single-track sample doesn't cover it).
        let m = Metadata {
            tracks: vec![make_file_track_id(1, "", ""), make_file_track_id(2, "", "")],
            ..Metadata::default()
        };
        assert!(metadata_is_canonical(&m), "two ascending tracks");

        // Equal track ids — detects < → <= mutation (equal ids are not canonical).
        let m = Metadata {
            tracks: vec![make_file_track_id(1, "", ""), make_file_track_id(1, "", "")],
            ..Metadata::default()
        };
        assert!(!metadata_is_canonical(&m), "equal track ids not canonical");

        // Two ascending speakers — exercises speaker-id comparison for positive case.
        let m = Metadata {
            speakers: vec![make_speaker_id(1), make_speaker_id(2)],
            ..Metadata::default()
        };
        assert!(metadata_is_canonical(&m), "two ascending speakers");

        // Equal speaker ids — detects < → <= mutation.
        let m = Metadata {
            speakers: vec![make_speaker_id(1), make_speaker_id(1)],
            ..Metadata::default()
        };
        assert!(
            !metadata_is_canonical(&m),
            "equal speaker ids not canonical"
        );

        // Duplicate speaker track_ids — detects track_ids < → <= mutation.
        let m = Metadata {
            speakers: vec![SpeakerMeta {
                id: 1,
                name: "A".to_string(),
                color_hint: None,
                embedding_hash: None,
                track_ids: vec![1, 1],
            }],
            ..Metadata::default()
        };
        assert!(
            !metadata_is_canonical(&m),
            "duplicate speaker track_ids not canonical"
        );

        // Duplicate aligned_groups inner element — detects inner < → <= mutation.
        let mut m = Metadata::default();
        m.project.aligned_groups = vec![vec![1, 1]];
        assert!(
            !metadata_is_canonical(&m),
            "duplicate aligned_groups inner not canonical"
        );

        // Two aligned_groups in canonical outer order — exercises outer comparison for positive case.
        let mut m = Metadata::default();
        m.project.aligned_groups = vec![vec![1, 2], vec![5, 6]];
        assert!(
            metadata_is_canonical(&m),
            "two aligned_groups canonical outer order"
        );

        // Two aligned_groups with same first id — detects outer < → <= mutation.
        let mut m = Metadata::default();
        m.project.aligned_groups = vec![vec![1, 2], vec![1, 3]];
        assert!(
            !metadata_is_canonical(&m),
            "aligned_groups same first id not canonical"
        );
    }

    // MR1
    #[test]
    fn load_current_metadata_empty_default() {
        let (_dir, db) = open_tmp_db();
        let meta = load_current_metadata(&db, None).unwrap();
        assert_eq!(meta, Metadata::default());
    }

    // MR2
    #[test]
    fn load_current_metadata_most_recent_wins() {
        let (_dir, db) = open_tmp_db();
        let mut meta_a = sample_metadata();
        let (h_a, b_a) = encode_metadata(&meta_a).unwrap();
        store::put(db.conn(), &h_a, &b_a).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h_a, 100).unwrap();

        meta_a.tracks[0].name = "Updated Host".to_string();
        let meta_b = meta_a;
        let (h_b, b_b) = encode_metadata(&meta_b).unwrap();
        store::put(db.conn(), &h_b, &b_b).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h_b, 101).unwrap();

        let loaded = load_current_metadata(&db, None).unwrap();
        assert_eq!(loaded, meta_b);
    }

    // MR2b
    #[test]
    fn load_current_metadata_as_of_returns_earlier() {
        let (_dir, db) = open_tmp_db();
        let meta_a = sample_metadata();
        let (h_a, b_a) = encode_metadata(&meta_a).unwrap();
        store::put(db.conn(), &h_a, &b_a).unwrap();
        let id_a = append_metadata(db.conn(), CommandId::Unknown, &h_a, 100).unwrap();

        let mut meta_b = meta_a.clone();
        meta_b.tracks[0].name = "Updated Host".to_string();
        let (h_b, b_b) = encode_metadata(&meta_b).unwrap();
        store::put(db.conn(), &h_b, &b_b).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h_b, 101).unwrap();

        let loaded_a = load_current_metadata(&db, Some(id_a)).unwrap();
        assert_eq!(loaded_a, meta_a, "as_of=id_a should return meta_a");

        let loaded_default = load_current_metadata(&db, Some(id_a - 1)).unwrap();
        assert_eq!(
            loaded_default,
            Metadata::default(),
            "as_of before first row should return default"
        );
    }

    // MR3
    #[test]
    fn rename_reuses_binary_blobs() {
        use crate::project::hash::hash_tagged;
        let (_dir, db) = open_tmp_db();
        // Build room-tone blob with a real (matching) hash
        let fake_rt_bytes = vec![0x41u8; 32];
        let h_rt = hash_tagged(&fake_rt_bytes);
        // Put the referenced room-tone blob once
        store::put(db.conn(), &h_rt, &fake_rt_bytes).unwrap();

        let mut meta_a = sample_metadata();
        meta_a.tracks[0].room_tone_hash = Some(h_rt);

        let (h_a, b_a) = encode_metadata(&meta_a).unwrap();
        store::put(db.conn(), &h_a, &b_a).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h_a, 100).unwrap();

        let mut meta_b = meta_a;
        meta_b.tracks[0].name = "Renamed Host".to_string();
        let (h_b, b_b) = encode_metadata(&meta_b).unwrap();
        store::put(db.conn(), &h_b, &b_b).unwrap();
        append_metadata(db.conn(), CommandId::Unknown, &h_b, 101).unwrap();

        // The room-tone blob should appear exactly once
        let rt_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM store WHERE hash = ?1",
                (&h_rt.0[..],),
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rt_count, 1, "room-tone blob should appear exactly once");

        // The two metadata blobs should be distinct
        assert_ne!(h_a, h_b, "metadata blobs should differ after rename");
    }

    // MR4
    #[test]
    fn load_current_metadata_surfaces_store_error() {
        let (_dir, db) = open_tmp_db();
        let phantom_hash = Hash([0xDEu8; 16]);
        // Append a metadata row pointing at a hash that was never put
        append_metadata(db.conn(), CommandId::Unknown, &phantom_hash, 100).unwrap();

        let err = load_current_metadata(&db, None).unwrap_err();
        assert!(
            matches!(err, MetadataLoadError::Store(StoreError::NotFound(_))),
            "expected Store(NotFound), got: {err:?}"
        );
    }

    // MR5
    #[test]
    fn metadata_load_error_display_and_source() {
        use std::error::Error;

        let journal_err =
            MetadataLoadError::Journal(crate::db::journal::JournalError::MalformedHashPayload {
                id: 1,
                len: 5,
            });
        assert!(!journal_err.to_string().is_empty());
        assert!(journal_err.source().is_some());

        let store_err = MetadataLoadError::Store(StoreError::NotFound(Hash([0u8; 16])));
        assert!(!store_err.to_string().is_empty());
        assert!(store_err.source().is_some());

        let decode_err = MetadataLoadError::Decode(DecodeError::Empty);
        assert!(!decode_err.to_string().is_empty());
        assert!(decode_err.source().is_some());
    }

    // RS1
    #[test]
    fn resolve_relative_hit() {
        let dir = tempdir().unwrap();
        let audio_dir = dir.path().join("audio");
        fs::create_dir_all(&audio_dir).unwrap();
        fs::write(audio_dir.join("a.wav"), b"fake").unwrap();

        let track = make_file_track("audio/a.wav", "/nonexistent/a.wav");
        match resolve_track_source(dir.path(), &track) {
            FileResolution::Found(p) => assert!(p.exists()),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // RS2
    #[test]
    fn resolve_absolute_fallback() {
        let dir = tempdir().unwrap();
        let abs_dir = tempdir().unwrap();
        let abs_path = abs_dir.path().join("a.wav");
        fs::write(&abs_path, b"fake").unwrap();

        let track = make_file_track("audio/a.wav", abs_path.to_str().unwrap());
        match resolve_track_source(dir.path(), &track) {
            FileResolution::FoundViaAbsolute { path, new_relative } => {
                assert!(path.exists());
                assert_eq!(new_relative, abs_path.to_str().unwrap());
            }
            other => panic!("expected FoundViaAbsolute, got {other:?}"),
        }
    }

    // RS3
    #[test]
    fn resolve_missing() {
        let dir = tempdir().unwrap();
        let track = make_file_track("audio/missing.wav", "/nonexistent/missing.wav");
        assert!(matches!(
            resolve_track_source(dir.path(), &track),
            FileResolution::Missing
        ));
    }

    // RS4
    #[test]
    fn resolve_recording_not_applicable() {
        let dir = tempdir().unwrap();
        let mut track = make_file_track("audio/a.wav", "/a.wav");
        track.source_type = SourceType::Recording;
        assert!(matches!(
            resolve_track_source(dir.path(), &track),
            FileResolution::NotApplicable
        ));
    }

    // RS5
    #[test]
    fn missing_tracks_collects_only_missing_file_tracks() {
        let dir = tempdir().unwrap();
        let audio_dir = dir.path().join("audio");
        fs::create_dir_all(&audio_dir).unwrap();
        fs::write(audio_dir.join("present.wav"), b"fake").unwrap();

        let meta = Metadata {
            tracks: vec![
                make_file_track_id(1, "audio/present.wav", "/nonexistent"),
                make_file_track_id(2, "audio/missing.wav", "/nonexistent"),
                {
                    let mut t = make_file_track_id(3, "audio/a.wav", "/nonexistent");
                    t.source_type = SourceType::Recording;
                    t
                },
            ],
            ..Metadata::default()
        };

        let missing = missing_tracks(dir.path(), &meta);
        assert_eq!(
            missing,
            vec![2],
            "only the missing file track should be listed"
        );
    }

    // --- test helpers ---

    fn make_speaker_id(id: u32) -> SpeakerMeta {
        SpeakerMeta {
            id,
            name: format!("Speaker {id}"),
            color_hint: None,
            embedding_hash: None,
            track_ids: vec![],
        }
    }

    fn make_file_track(relative: &str, absolute: &str) -> TrackMeta {
        make_file_track_id(1, relative, absolute)
    }

    fn make_file_track_id(id: u32, relative: &str, absolute: &str) -> TrackMeta {
        TrackMeta {
            id,
            name: format!("Track {id}"),
            source_type: SourceType::File,
            source_path_relative: relative.to_string(),
            source_path_absolute: absolute.to_string(),
            codec: "wav".to_string(),
            source_sample_rate: 48000,
            source_channels: 1,
            project_start_sample: 0,
            original_length_samples: 0,
            cut_length_samples: 0,
            drift_ppm: 0.0,
            room_tone_hash: None,
            models_used: ModelUse::default(),
            wet_dry_ratio: 0.0,
            disfluencies_identified: false,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    // Pinned wire bytes for sample_metadata(). Regenerate via capture_pinned_values.
    const PINNED_WIRE_BYTES: [u8; 219] = [
        0x21, 0x01, 0x0a, 0x4d, 0x79, 0x20, 0x50, 0x72, 0x6f, 0x6a, 0x65, 0x63, 0x74, 0x01, 0x02,
        0x01, 0x02, 0x01, 0x01, 0x04, 0x48, 0x6f, 0x73, 0x74, 0x00, 0x0e, 0x61, 0x75, 0x64, 0x69,
        0x6f, 0x2f, 0x68, 0x6f, 0x73, 0x74, 0x2e, 0x77, 0x61, 0x76, 0x14, 0x2f, 0x72, 0x65, 0x63,
        0x6f, 0x72, 0x64, 0x69, 0x6e, 0x67, 0x73, 0x2f, 0x68, 0x6f, 0x73, 0x74, 0x2e, 0x77, 0x61,
        0x76, 0x03, 0x77, 0x61, 0x76, 0x80, 0xf7, 0x02, 0x01, 0x00, 0x80, 0xcc, 0x3a, 0x80, 0x4b,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa,
        0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x01, 0x11, 0x77, 0x68, 0x69,
        0x73, 0x70, 0x65, 0x72, 0x78, 0x2d, 0x6c, 0x61, 0x72, 0x67, 0x65, 0x2d, 0x76, 0x33, 0x00,
        0x00, 0x01, 0x0a, 0x6d, 0x70, 0x73, 0x65, 0x6e, 0x65, 0x74, 0x2d, 0x76, 0x31, 0x00, 0x00,
        0xcd, 0xcc, 0x4c, 0x3f, 0x01, 0x14, 0x32, 0x30, 0x32, 0x34, 0x2d, 0x30, 0x31, 0x2d, 0x30,
        0x31, 0x54, 0x30, 0x30, 0x3a, 0x30, 0x30, 0x3a, 0x30, 0x30, 0x5a, 0x14, 0x32, 0x30, 0x32,
        0x34, 0x2d, 0x30, 0x31, 0x2d, 0x30, 0x32, 0x54, 0x30, 0x30, 0x3a, 0x30, 0x30, 0x3a, 0x30,
        0x30, 0x5a, 0x01, 0x01, 0x05, 0x41, 0x6c, 0x69, 0x63, 0x65, 0x01, 0x07, 0x23, 0x66, 0x66,
        0x30, 0x30, 0x30, 0x30, 0x01, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb,
        0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0x02, 0x01, 0x02,
    ];
    const PINNED_HASH: [u8; 16] = [
        0xd8, 0xd1, 0x30, 0x73, 0x4e, 0x26, 0x4a, 0xa9, 0x1e, 0x09, 0x5c, 0x94, 0x9a, 0x11, 0x21,
        0x08,
    ];

    impl std::fmt::Debug for FileResolution {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                FileResolution::Found(p) => write!(f, "Found({p:?})"),
                FileResolution::FoundViaAbsolute { path, new_relative } => {
                    write!(
                        f,
                        "FoundViaAbsolute {{ path: {path:?}, new_relative: {new_relative:?} }}"
                    )
                }
                FileResolution::Missing => write!(f, "Missing"),
                FileResolution::NotApplicable => write!(f, "NotApplicable"),
            }
        }
    }
}
