//! Concrete [`SourceProvider`] over a project's `.vbdata` FLAC cache.
//!
//! [`CacheSourceProvider`] is the real production implementation used by the playback engine
//! and the export path. Room tone is pre-decoded by the caller (from `engine.rs`) and passed
//! in as `Arc<RoomTone>` — no SQLite on the provider (RT-path invariant). Dry and enhanced
//! FLAC readers are opened lazily on first use (pre-roll thread, not the RT callback).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    cache::resampled_cache_path,
    frame_reader::{FrameReader, SymphoniaFrameReader},
    render::SourceProvider,
    room_tone::RoomTone,
    AudioError,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One track within a [`CacheSourceProvider`]: the render contract (channels, wet/dry,
/// length, pre-decoded room tone) plus the lazily-opened FLAC readers.
///
/// The scalar fields are the explicit render inputs — a deliberately narrow projection of
/// `TrackMeta` (`source_channels`, `wet_dry_ratio`, `original_length_samples`,
/// `room_tone_hash`), so the audio path stays decoupled from the persisted-metadata format.
/// Construct via [`TrackSource::new`]; the reader fields open on the first read.
pub struct TrackSource {
    /// Track identifier (matches the timeline's track IDs).
    pub id: u32,
    /// Source channel count (1 = mono, 2 = stereo).
    pub channels: u16,
    /// Wet/dry blend ratio in [0, 1] (0 = full dry, 1 = full enhanced).
    pub wet_ratio: f32,
    /// Length of the resampled cache in frames (`original_length_samples` from `TrackMeta`).
    pub source_len: i64,
    /// Pre-decoded room tone blob; `None` when `room_tone_hash` was null on the track.
    pub room_tone: Option<Arc<RoomTone>>,
    /// Lazily opened reader over `resampled/<id>.flac`. `None` until first `dry()` call.
    dry_reader: Option<SymphoniaFrameReader>,
    /// `None` = not yet checked; `Some(None)` = file absent; `Some(Some(r))` = open reader.
    enhanced_reader: Option<Option<SymphoniaFrameReader>>,
}

impl TrackSource {
    /// Build a track-source config from its render contract. The FLAC readers are not opened
    /// here — they open lazily on the first `dry()` / `enhanced()` call (pre-roll thread).
    pub fn new(
        id: u32,
        channels: u16,
        wet_ratio: f32,
        source_len: i64,
        room_tone: Option<Arc<RoomTone>>,
    ) -> Self {
        Self {
            id,
            channels,
            wet_ratio,
            source_len,
            room_tone,
            dry_reader: None,
            enhanced_reader: None,
        }
    }
}

/// The real [`SourceProvider`] over a project's `.vbdata` resampled FLAC cache.
///
/// Built off the real-time path; room tone is pre-loaded by the caller. Dry/enhanced FLAC
/// readers are opened lazily on first access (on the pre-roll thread, never the callback).
/// No SQLite/`Db` reference is held or accessed.
pub struct CacheSourceProvider {
    vbdata_dir: PathBuf,
    tracks: BTreeMap<u32, TrackSource>,
}

impl CacheSourceProvider {
    /// Build the provider from per-track metadata and pre-decoded room tones.
    ///
    /// `vbdata_dir` is the project's `.vbdata` directory (dry cache lives under
    /// `<vbdata_dir>/resampled/<id>.flac`; enhanced under `<vbdata_dir>/enhanced/<id>.flac`).
    /// No file I/O is performed here — readers open lazily on the pre-roll thread.
    pub fn new(vbdata_dir: PathBuf, tracks: Vec<TrackSource>) -> Self {
        let tracks = tracks.into_iter().map(|s| (s.id, s)).collect();
        Self { vbdata_dir, tracks }
    }
}

// ---------------------------------------------------------------------------
// SourceProvider impl
// ---------------------------------------------------------------------------

impl SourceProvider for CacheSourceProvider {
    fn dry(&mut self, track_id: u32, from: i64, n: i64) -> Result<Vec<f32>, AudioError> {
        // Lazy open: compute path before the mutable borrow of the entry.
        if self
            .tracks
            .get(&track_id)
            .is_none_or(|e| e.dry_reader.is_none())
        {
            if !self.tracks.contains_key(&track_id) {
                return Err(track_not_found(track_id));
            }
            let path = resampled_cache_path(&self.vbdata_dir, track_id);
            let reader = SymphoniaFrameReader::open(&path)?;
            if let Some(e) = self.tracks.get_mut(&track_id) {
                e.dry_reader = Some(reader);
            }
        }
        let entry = self
            .tracks
            .get_mut(&track_id)
            .ok_or_else(|| track_not_found(track_id))?;
        let reader = entry.dry_reader.as_mut().ok_or_else(|| {
            AudioError::Io(std::io::Error::other(format!(
                "dry reader unexpectedly absent for track {track_id}"
            )))
        })?;
        reader.read_range(from, n as usize)
    }

    fn enhanced(
        &mut self,
        track_id: u32,
        from: i64,
        n: i64,
    ) -> Result<Option<Vec<f32>>, AudioError> {
        // Lazy check: compute path before the mutable borrow of the entry.
        if self
            .tracks
            .get(&track_id)
            .is_none_or(|e| e.enhanced_reader.is_none())
        {
            if !self.tracks.contains_key(&track_id) {
                return Err(track_not_found(track_id));
            }
            let path = self
                .vbdata_dir
                .join("enhanced")
                .join(format!("{track_id}.flac"));
            let state = if path.exists() {
                Some(SymphoniaFrameReader::open(&path)?)
            } else {
                None
            };
            if let Some(e) = self.tracks.get_mut(&track_id) {
                e.enhanced_reader = Some(state);
            }
        }
        let entry = self
            .tracks
            .get_mut(&track_id)
            .ok_or_else(|| track_not_found(track_id))?;
        let state = entry.enhanced_reader.as_mut().ok_or_else(|| {
            AudioError::Io(std::io::Error::other(format!(
                "enhanced state unexpectedly absent for track {track_id}"
            )))
        })?;
        match state {
            None => Ok(None),
            Some(reader) => Ok(Some(reader.read_range(from, n as usize)?)),
        }
    }

    fn room_tone(&mut self, track_id: u32) -> Result<Option<&[f32]>, AudioError> {
        self.tracks
            .get(&track_id)
            .ok_or_else(|| track_not_found(track_id))
            .map(|e| e.room_tone.as_deref().map(|rt| rt.samples.as_slice()))
    }

    fn channels(&self, track_id: u32) -> u16 {
        self.tracks.get(&track_id).map_or(1, |e| e.channels)
    }

    fn wet_ratio(&self, track_id: u32) -> f32 {
        self.tracks.get(&track_id).map_or(0.0, |e| e.wet_ratio)
    }

    fn source_len(&self, track_id: u32) -> i64 {
        self.tracks.get(&track_id).map_or(0, |e| e.source_len)
    }
}

fn track_not_found(id: u32) -> AudioError {
    AudioError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("track {id} not registered in CacheSourceProvider"),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::audio::edl::{EdlCursor, TrackCursor};
    use crate::audio::flac::{decode_flac, encode_flac_24};
    use crate::audio::render::Renderer;
    use crate::project::hash::Hash;
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{encode_turn, Splice, SpliceKind, Turn};

    // --- Helpers ---

    fn make_vbdata(dir: &TempDir) -> PathBuf {
        let p = dir.path().to_path_buf();
        std::fs::create_dir_all(p.join("resampled")).unwrap();
        p
    }

    fn write_dry(vbdata: &Path, id: u32, data: &[f32], rate: u32, ch: u16) {
        let path = resampled_cache_path(vbdata, id);
        encode_flac_24(data, rate, ch, &path).unwrap();
    }

    fn write_enhanced(vbdata: &Path, id: u32, data: &[f32], rate: u32, ch: u16) {
        let dir = vbdata.join("enhanced");
        std::fs::create_dir_all(&dir).unwrap();
        encode_flac_24(data, rate, ch, &dir.join(format!("{id}.flac"))).unwrap();
    }

    fn make_track(id: u32, ch: u16, len: i64, wet: f32) -> TrackSource {
        TrackSource::new(id, ch, wet, len, None)
    }

    fn make_turn(id: u64, splice_len: i64, source_start: i64) -> (Hash, Arc<Turn>) {
        let turn = Turn {
            id,
            speaker_id: None,
            turn_duration: splice_len,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![Splice {
                length_samples: splice_len,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: source_start,
                },
            }],
        };
        let (h, _) = encode_turn(&turn).unwrap();
        (h, Arc::new(turn))
    }

    fn build_tree(turns: Vec<(Hash, Arc<Turn>)>) -> ImplicitTimelineTree<Turn> {
        let mut tree = ImplicitTimelineTree::new();
        for (h, t) in turns {
            tree = tree.insert_at(tree.total_duration(), h, t).unwrap();
        }
        tree
    }

    // V1: dry(track, from, n) == decode_flac(resampled/<id>.flac).samples slice — sample-accurate.
    #[test]
    fn v1_dry_range_sample_accurate() {
        let dir = TempDir::new().unwrap();
        let vbdata = make_vbdata(&dir);
        let frames = 1000usize;
        let data: Vec<f32> = (0..frames).map(|i| (i % 1000) as f32 / 2000.0).collect();
        write_dry(&vbdata, 1, &data, 48_000, 1);

        let whole = decode_flac(&resampled_cache_path(&vbdata, 1)).unwrap();
        let mut provider =
            CacheSourceProvider::new(vbdata, vec![make_track(1, 1, frames as i64, 0.0)]);

        let start = 250i64;
        let n = 100i64;
        let got = provider.dry(1, start, n).unwrap();
        let expected = &whole.samples[start as usize..(start + n) as usize];
        assert_eq!(got.len(), expected.len(), "V1: frame count mismatch");
        for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
            assert!((g - e).abs() < 1e-7, "V1 sample {i}: expected {e}, got {g}");
        }
    }

    // V2: enhanced() returns None when no enhanced file exists.
    #[test]
    fn v2_enhanced_absent_returns_none() {
        let dir = TempDir::new().unwrap();
        let vbdata = make_vbdata(&dir);
        let data = vec![0.1f32; 100];
        write_dry(&vbdata, 1, &data, 48_000, 1);

        let mut provider = CacheSourceProvider::new(vbdata, vec![make_track(1, 1, 100, 0.0)]);
        let result = provider.enhanced(1, 0, 10).unwrap();
        assert!(result.is_none(), "V2: enhanced absent must return None");
    }

    // V3: room_tone() returns the pre-loaded Arc<RoomTone> PCM slice; None when none.
    #[test]
    fn v3_room_tone_served_from_memory() {
        let dir = TempDir::new().unwrap();
        let vbdata = make_vbdata(&dir);
        let data = vec![0.1f32; 100];
        write_dry(&vbdata, 1, &data, 48_000, 1);

        let tone_samples = vec![0.01f32, 0.02, 0.03, 0.04];
        let rt = Arc::new(RoomTone {
            samples: tone_samples.clone(),
            channels: 1,
            sample_rate: 48_000,
            rms: 0.025,
        });
        let mut ts = make_track(1, 1, 100, 0.0);
        ts.room_tone = Some(rt);
        let mut provider = CacheSourceProvider::new(vbdata.clone(), vec![ts]);

        let got = provider
            .room_tone(1)
            .unwrap()
            .expect("V3: expected Some room tone");
        assert_eq!(
            got,
            tone_samples.as_slice(),
            "V3: room tone samples must match"
        );

        // Track with no room tone returns None.
        let data2 = vec![0.0f32; 50];
        write_dry(&vbdata, 2, &data2, 48_000, 1);
        let vbdata2 = dir.path().to_path_buf();
        let mut p2 = CacheSourceProvider::new(vbdata2, vec![make_track(2, 1, 50, 0.0)]);
        assert!(
            p2.room_tone(2).unwrap().is_none(),
            "V3: no room tone must be None"
        );
    }

    // V4: channels / wet_ratio / source_len getters match the TrackSource.
    #[test]
    fn v4_metadata_getters() {
        let dir = TempDir::new().unwrap();
        let vbdata = make_vbdata(&dir);
        let data: Vec<f32> = (0..100)
            .map(|i| if i % 2 == 0 { 0.1 } else { 0.2 })
            .collect(); // stereo: 50 frames × 2 ch
        write_dry(&vbdata, 7, &data, 48_000, 2);

        let provider =
            CacheSourceProvider::new(vbdata, vec![TrackSource::new(7, 2, 0.4, 50, None)]);

        assert_eq!(provider.channels(7), 2, "V4: channels");
        assert!((provider.wet_ratio(7) - 0.4).abs() < 1e-6, "V4: wet_ratio");
        assert_eq!(provider.source_len(7), 50, "V4: source_len");

        // Mono track uses channels=1.
        let dir2 = TempDir::new().unwrap();
        let vbdata2 = make_vbdata(&dir2);
        let data2 = vec![0.5f32; 20];
        write_dry(&vbdata2, 3, &data2, 48_000, 1);
        let p2 = CacheSourceProvider::new(vbdata2, vec![make_track(3, 1, 20, 0.0)]);
        assert_eq!(p2.channels(3), 1, "V4: mono channels");
    }

    // V5: A Renderer over CacheSourceProvider produces the expected PCM for a synthetic project.
    #[test]
    fn v5_end_to_end_through_renderer() {
        let dir = TempDir::new().unwrap();
        let vbdata = make_vbdata(&dir);
        let frames = 200usize;
        // Distinct values per frame so any seek/range error is detectable.
        let data: Vec<f32> = (0..frames).map(|i| (i % 200) as f32 / 400.0).collect();
        write_dry(&vbdata, 1, &data, 48_000, 1);
        let whole = decode_flac(&resampled_cache_path(&vbdata, 1)).unwrap();

        let source_start = 50i64;
        let splice_len = 100i64;
        let tree = build_tree(vec![make_turn(1, splice_len, source_start)]);
        let cursor = TrackCursor::at(&tree, 1, 0, 0);
        let edl = EdlCursor::new(vec![cursor], 0, None);

        let provider = CacheSourceProvider::new(vbdata, vec![make_track(1, 1, frames as i64, 0.0)]);
        let mut renderer = Renderer::new(edl, provider, 0, 48_000);
        let out = renderer.render(splice_len as usize).unwrap();

        assert_eq!(out.len() / 2, splice_len as usize, "V5: frame count");
        for i in 0..splice_len as usize {
            let expected = whole.samples[source_start as usize + i];
            let l = out[i * 2];
            let r = out[i * 2 + 1];
            assert!(
                (l - expected).abs() < 1e-7,
                "V5 frame {i} L: expected {expected}, got {l}"
            );
            assert_eq!(l, r, "V5 frame {i}: mono upmix L == R");
        }
    }

    // V6: enhanced() returns Some(samples) — sample-accurate — when an enhanced FLAC exists.
    // Guards against the lazy-open path collapsing to an unconditional Ok(None).
    #[test]
    fn v6_enhanced_present_returns_samples() {
        let dir = TempDir::new().unwrap();
        let vbdata = make_vbdata(&dir);
        let frames = 500usize;
        let dry: Vec<f32> = vec![0.0f32; frames];
        write_dry(&vbdata, 1, &dry, 48_000, 1);
        // Distinct per-frame values so any seek/range error is detectable.
        let enh: Vec<f32> = (0..frames).map(|i| (i % 500) as f32 / 1000.0).collect();
        write_enhanced(&vbdata, 1, &enh, 48_000, 1);
        let whole = decode_flac(&vbdata.join("enhanced").join("1.flac")).unwrap();

        let mut provider =
            CacheSourceProvider::new(vbdata, vec![make_track(1, 1, frames as i64, 0.0)]);

        let start = 100i64;
        let n = 80i64;
        let got = provider
            .enhanced(1, start, n)
            .unwrap()
            .expect("V6: enhanced present must return Some");
        let expected = &whole.samples[start as usize..(start + n) as usize];
        assert_eq!(got.len(), expected.len(), "V6: frame count mismatch");
        for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
            assert!((g - e).abs() < 1e-7, "V6 sample {i}: expected {e}, got {g}");
        }
    }
}
