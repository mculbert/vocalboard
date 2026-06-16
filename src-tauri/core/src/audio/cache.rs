//! Resampled FLAC cache: path convention and `ensure_resampled`.

use std::path::{Path, PathBuf};

use crate::settings::ResamplingQuality;

use super::{
    decode::{open_source, probe},
    flac::encode_flac_streaming,
    frame_reader::count_frames,
    resample::StreamingResampler,
    AudioError,
};

/// `<vbdata_dir>/resampled/<track_id>.flac` (id is always filesystem-safe; no sanitization).
pub fn resampled_cache_path(vbdata_dir: &Path, track_id: u32) -> PathBuf {
    vbdata_dir
        .join("resampled")
        .join(format!("{track_id}.flac"))
}

/// Outcome from [`ensure_resampled`].
#[derive(Debug)]
pub struct CacheOutcome {
    /// '/'-separated path, relative to the project dir.
    pub relative_path: String,
    /// `true` if this call (re)wrote the file; `false` if it was already present.
    pub regenerated: bool,
    /// Project-rate frame count (feeds `original_length_samples` at M4).
    pub length_samples: i64,
}

/// Ensure the resampled cache exists for a track: if the file is missing, decode the source,
/// resample to `project_sample_rate`, and write the 24-bit FLAC. Returns the relative path
/// (`resampled/<track_id>.flac`, derivable from the track ID), whether it (re)wrote the file,
/// and the project-rate frame count. Requires the source to exist.
pub fn ensure_resampled(
    source: &Path,
    vbdata_dir: &Path,
    track_id: u32,
    project_sample_rate: u32,
    quality: ResamplingQuality,
) -> Result<CacheOutcome, AudioError> {
    let path = resampled_cache_path(vbdata_dir, track_id);
    let relative_path = format!("resampled/{track_id}.flac");

    if path.exists() {
        return Ok(CacheOutcome {
            relative_path,
            regenerated: false,
            length_samples: probe_length(&path)?,
        });
    }

    // Create the resampled/ subdirectory (mkdir -p).
    std::fs::create_dir_all(vbdata_dir.join("resampled")).map_err(AudioError::Io)?;

    // Stream source → resample → 24-bit FLAC. Propagates AudioError::Io(NotFound) when the
    // source is absent. On any failure, remove the partial/zero-byte artifact so a later call
    // retries cleanly.
    let out_frames = match transcode_to_cache(source, &path, project_sample_rate, quality) {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };

    Ok(CacheOutcome {
        relative_path,
        regenerated: true,
        length_samples: out_frames,
    })
}

/// Probe the frame count from an existing cache file.
///
/// Uses the container frame count from the probe (fast path). Falls back to a full decode if the
/// container header omits the length — FLAC files we write always include it, but be defensive.
fn probe_length(path: &Path) -> Result<i64, AudioError> {
    match probe(path)?.length_frames {
        Some(n) => Ok(n),
        None => count_frames(path),
    }
}

/// Transcode `source` to a 24-bit FLAC at `out`, resampled to `project_rate`.
///
/// Streams decode → resample → encode (via [`encode_flac_streaming`](super::flac)) so peak
/// memory is O(one encoded block). Returns the project-rate frame count. On error, any
/// partially-written output remains; cleanup is [`ensure_resampled`]'s responsibility.
fn transcode_to_cache(
    source: &Path,
    out: &Path,
    project_rate: u32,
    quality: ResamplingQuality,
) -> Result<i64, AudioError> {
    let decoder = open_source(source)?;
    let resampler = StreamingResampler::new(decoder, project_rate, quality)?;
    encode_flac_streaming(resampler, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decode::decode;
    use crate::audio::flac::decode_flac;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Write a minimal valid 16-bit mono WAV at the given rate with a 440 Hz sine.
    fn write_wav_sine(dir: &Path, name: &str, sample_rate: u32, frames: usize) -> PathBuf {
        let path = dir.join(name);
        let samples_i16: Vec<i16> = (0..frames)
            .map(|i| {
                let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin();
                (v * i16::MAX as f32) as i16
            })
            .collect();
        write_wav_s16(&path, sample_rate, 1, &samples_i16);
        path
    }

    fn write_wav_s16(path: &Path, sample_rate: u32, channels: u16, samples: &[i16]) {
        let data_size = (samples.len() * 2) as u32;
        let byte_rate = sample_rate * channels as u32 * 2;
        let block_align = channels * 2;
        let mut buf = Vec::with_capacity(44 + samples.len() * 2);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_size).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, &buf).expect("write WAV s16");
    }

    // -----------------------------------------------------------------------
    // E19 — Path resolution
    // -----------------------------------------------------------------------

    #[test]
    fn e19_path_resolution() {
        let dir = TempDir::new().unwrap();
        let p = resampled_cache_path(dir.path(), 1);
        assert!(
            p.ends_with("resampled/1.flac"),
            "E19: path should end with resampled/1.flac, got {p:?}"
        );
    }

    // -----------------------------------------------------------------------
    // E20 — End-to-end at a different rate
    // -----------------------------------------------------------------------

    #[test]
    fn e20_end_to_end_different_rate() {
        let dir = TempDir::new().unwrap();
        let source = write_wav_sine(dir.path(), "src.wav", 44100, 4410);
        let outcome =
            ensure_resampled(&source, dir.path(), 42, 48000, ResamplingQuality::Balanced).unwrap();

        assert!(outcome.regenerated, "E20: regenerated");
        assert_eq!(
            outcome.relative_path, "resampled/42.flac",
            "E20: relative_path"
        );

        let cache_path = resampled_cache_path(dir.path(), 42);
        assert!(cache_path.exists(), "E20: cache file should exist");

        let decoded = decode_flac(&cache_path).unwrap();
        assert_eq!(decoded.sample_rate, 48000, "E20: cache is at project rate");
        assert!(decoded.frames() > 0, "E20: non-empty cache");
        assert_eq!(
            outcome.length_samples,
            decoded.frames() as i64,
            "E20: length_samples matches"
        );
    }

    // -----------------------------------------------------------------------
    // E21 — Identity-rate source still cached
    // -----------------------------------------------------------------------

    #[test]
    fn e21_identity_rate_source_cached() {
        let dir = TempDir::new().unwrap();
        let source = write_wav_sine(dir.path(), "src.wav", 48000, 4800);

        let outcome =
            ensure_resampled(&source, dir.path(), 7, 48000, ResamplingQuality::Balanced).unwrap();

        assert!(outcome.regenerated, "E21: file should be generated");
        let cache_path = resampled_cache_path(dir.path(), 7);
        assert!(
            cache_path.exists(),
            "E21: cache file must exist even for identity rate"
        );

        // The decoded PCM should be a faithful reproduction of the source: identity-rate
        // resampling is pass-through, so source → 24-bit FLAC cache → decode must agree within
        // the 24-bit quantisation bound (regression guard for the lost `tc2_identity_rate`).
        let src_decoded = decode(&source).unwrap();
        let cache_decoded = decode_flac(&cache_path).unwrap();
        assert_eq!(cache_decoded.sample_rate, 48000, "E21: cache rate");
        assert_eq!(
            cache_decoded.frames(),
            src_decoded.frames(),
            "E21: frame count"
        );
        let bound = 2.0 / (1 << 23) as f32;
        let max_err = src_decoded
            .samples
            .iter()
            .zip(cache_decoded.samples.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err <= bound, "E21: identity round-trip error {max_err}");
    }

    // -----------------------------------------------------------------------
    // E22 — Idempotence: second call returns regenerated=false
    // -----------------------------------------------------------------------

    #[test]
    fn e22_idempotence() {
        let dir = TempDir::new().unwrap();
        let source = write_wav_sine(dir.path(), "src.wav", 44100, 4410);

        let first =
            ensure_resampled(&source, dir.path(), 1, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(first.regenerated, "E22: first call regenerated");

        let second =
            ensure_resampled(&source, dir.path(), 1, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(!second.regenerated, "E22: second call must not regenerate");
        assert_eq!(first.relative_path, second.relative_path, "E22: same path");
        assert_eq!(
            first.length_samples, second.length_samples,
            "E22: same length"
        );
    }

    // -----------------------------------------------------------------------
    // E23 — Regenerate after deletion (deterministic)
    // -----------------------------------------------------------------------

    #[test]
    fn e23_regenerate_after_deletion() {
        let dir = TempDir::new().unwrap();
        let source = write_wav_sine(dir.path(), "src.wav", 44100, 4410);

        let first =
            ensure_resampled(&source, dir.path(), 5, 48000, ResamplingQuality::Balanced).unwrap();

        let cache_path = resampled_cache_path(dir.path(), 5);
        let bytes_first = std::fs::read(&cache_path).unwrap();

        std::fs::remove_file(&cache_path).unwrap();
        assert!(!cache_path.exists(), "E23: deleted");

        let second =
            ensure_resampled(&source, dir.path(), 5, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(second.regenerated, "E23: regenerated after deletion");

        let bytes_second = std::fs::read(&cache_path).unwrap();
        assert_eq!(bytes_first, bytes_second, "E23: deterministic regeneration");

        assert_eq!(
            first.length_samples, second.length_samples,
            "E23: length matches"
        );
    }

    // -----------------------------------------------------------------------
    // E24 — Missing source → AudioError::Io(NotFound); no partial cache file
    // -----------------------------------------------------------------------

    #[test]
    fn e24_missing_source() {
        let dir = TempDir::new().unwrap();
        let absent = dir.path().join("no_such_file.wav");
        let result = ensure_resampled(&absent, dir.path(), 99, 48000, ResamplingQuality::Balanced);
        assert!(
            matches!(result, Err(AudioError::Io(_))),
            "E24: expected Io error, got {result:?}"
        );
        // No partial file left behind.
        let cache_path = resampled_cache_path(dir.path(), 99);
        assert!(
            !cache_path.exists(),
            "E24: no partial cache file on failure"
        );
    }

    // -----------------------------------------------------------------------
    // E25 — Creates the resampled/ subdir when absent
    // -----------------------------------------------------------------------

    #[test]
    fn e25_creates_subdir() {
        let dir = TempDir::new().unwrap();
        let vbdata = dir.path().join("project");
        std::fs::create_dir(&vbdata).unwrap();
        // resampled/ does NOT exist yet.
        assert!(!vbdata.join("resampled").exists(), "E25: precondition");

        let source = write_wav_sine(&vbdata, "src.wav", 44100, 4410);
        ensure_resampled(&source, &vbdata, 3, 48000, ResamplingQuality::Balanced).unwrap();

        assert!(
            vbdata.join("resampled").is_dir(),
            "E25: resampled/ subdir created"
        );
    }

    // -----------------------------------------------------------------------
    // E26 — Length without regen (probed from existing file, not recomputed)
    // -----------------------------------------------------------------------

    #[test]
    fn e26_length_without_regen() {
        let dir = TempDir::new().unwrap();
        let source = write_wav_sine(dir.path(), "src.wav", 44100, 4410);

        // First call: generate and note length.
        let first =
            ensure_resampled(&source, dir.path(), 8, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(first.regenerated, "E26: first call regenerated");

        // Second call: file exists, length is probed from the file.
        let second =
            ensure_resampled(&source, dir.path(), 8, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(!second.regenerated, "E26: second call did not regenerate");
        assert_eq!(
            first.length_samples, second.length_samples,
            "E26: probed length matches generated length"
        );
    }

    // -----------------------------------------------------------------------
    // X27 — No Db / no metadata mutation (signature / structural test)
    // -----------------------------------------------------------------------

    #[test]
    fn x27_no_db_connection() {
        // ensure_resampled has no Db parameter and returns no TrackMeta.
        // The test verifies this by calling it and confirming only file + CacheOutcome are produced.
        let dir = TempDir::new().unwrap();
        let source = write_wav_sine(dir.path(), "src.wav", 48000, 480);
        let outcome =
            ensure_resampled(&source, dir.path(), 1, 48000, ResamplingQuality::Balanced).unwrap();
        // If this compiles and runs, the function signature has no Db connection.
        let _ = outcome.relative_path;
        let _ = outcome.regenerated;
        let _ = outcome.length_samples;
    }

    // -----------------------------------------------------------------------
    // X28 — error_key() for EncodeFailed variant
    // -----------------------------------------------------------------------

    #[test]
    fn x28_encode_failed_error_key() {
        use crate::audio::AudioError;
        let err = AudioError::EncodeFailed("test error".to_owned());
        assert_eq!(err.error_key(), "encode_failed", "X28: error_key");
    }

    // -----------------------------------------------------------------------
    // E29 — Stereo source: channels preserved through the cache + deterministic bytes
    //       (regression guard for the lost `tc3_stereo_and_determinism`)
    // -----------------------------------------------------------------------

    #[test]
    fn e29_stereo_preserved_and_deterministic() {
        let dir = TempDir::new().unwrap();
        // 44100 Hz stereo source: distinct L (440 Hz) / R (660 Hz) tones, interleaved s16.
        let frames = 4410usize;
        let mut interleaved = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let l = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin();
            let r = (2.0 * std::f32::consts::PI * 660.0 * i as f32 / 44100.0).sin();
            interleaved.push((l * i16::MAX as f32) as i16);
            interleaved.push((r * i16::MAX as f32) as i16);
        }
        let source = dir.path().join("stereo.wav");
        write_wav_s16(&source, 44100, 2, &interleaved);

        // Two project dirs so the two caches don't collide on the same path.
        let a_dir = dir.path().join("a");
        let b_dir = dir.path().join("b");
        std::fs::create_dir(&a_dir).unwrap();
        std::fs::create_dir(&b_dir).unwrap();
        ensure_resampled(&source, &a_dir, 1, 48000, ResamplingQuality::Balanced).unwrap();
        ensure_resampled(&source, &b_dir, 1, 48000, ResamplingQuality::Balanced).unwrap();

        let cache_a = resampled_cache_path(&a_dir, 1);
        let cache_b = resampled_cache_path(&b_dir, 1);

        let decoded = decode_flac(&cache_a).unwrap();
        assert_eq!(decoded.channels, 2, "E29: stereo preserved through cache");
        assert_eq!(decoded.sample_rate, 48000, "E29: cache at project rate");

        assert_eq!(
            std::fs::read(&cache_a).unwrap(),
            std::fs::read(&cache_b).unwrap(),
            "E29: deterministic FLAC cache bytes"
        );
    }

    // -----------------------------------------------------------------------
    // Fixture-based integration tests (moved from tests/audio_cache.rs)
    // -----------------------------------------------------------------------

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/audio")
            .join(name)
    }

    /// End-to-end cache round-trip using the committed 44100 Hz stereo WAV fixture.
    #[test]
    fn cache_round_trip_wav_44100_to_48000() {
        let dir = TempDir::new().unwrap();
        let source = fixture("fixture_440hz.wav");
        let track_id = 1u32;

        let outcome = ensure_resampled(
            &source,
            dir.path(),
            track_id,
            48000,
            ResamplingQuality::Balanced,
        )
        .expect("ensure_resampled should succeed");

        assert!(outcome.regenerated, "first call should generate the cache");
        assert_eq!(outcome.relative_path, format!("resampled/{track_id}.flac"));

        let cache_path = resampled_cache_path(dir.path(), track_id);
        assert!(cache_path.exists(), "cache file must exist");

        let decoded = decode_flac(&cache_path).expect("decode_flac should succeed");
        assert_eq!(
            decoded.sample_rate, 48000,
            "cache must be at the project rate"
        );
        assert!(decoded.frames() > 0, "cache must be non-empty");
        assert_eq!(
            outcome.length_samples,
            decoded.frames() as i64,
            "length_samples must match the decoded frame count"
        );
    }

    /// Idempotence: calling twice on the same source gives regenerated=false the second time.
    #[test]
    fn cache_idempotent_fixture() {
        let dir = TempDir::new().unwrap();
        let source = fixture("fixture_440hz.wav");

        let first = ensure_resampled(&source, dir.path(), 2, 48000, ResamplingQuality::Balanced)
            .expect("first call");
        let second = ensure_resampled(&source, dir.path(), 2, 48000, ResamplingQuality::Balanced)
            .expect("second call");

        assert!(first.regenerated);
        assert!(!second.regenerated);
        assert_eq!(first.length_samples, second.length_samples);
    }

    /// FLAC fixture as source: 44100 Hz FLAC → 48000 Hz cache.
    #[test]
    fn cache_from_flac_fixture() {
        let dir = TempDir::new().unwrap();
        let source = fixture("fixture_440hz.flac");

        let outcome = ensure_resampled(&source, dir.path(), 3, 48000, ResamplingQuality::Balanced)
            .expect("FLAC fixture should be decodable and cacheable");

        assert!(outcome.regenerated);
        let cache = resampled_cache_path(dir.path(), 3);
        let decoded = decode_flac(&cache).unwrap();
        assert_eq!(decoded.sample_rate, 48000);
    }
}
