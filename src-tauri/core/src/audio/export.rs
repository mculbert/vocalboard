//! Export pipeline: audio (FLAC/WAV/ffmpeg) and transcript (VTT/Markdown).
//!
//! Audio export reuses the [`Renderer`] offline: a caller-built [`EdlCursor`](super::edl)
//! (carrying the desired `end` / silence padding) wrapped in a [`Renderer`], pulled by a
//! streaming encoder (`flac` / `wav` / `ffmpeg`). Transcript export reads the timeline tree
//! directly (not an audio op).

use std::path::Path;

use super::ffmpeg::{encode_via_ffmpeg, ffmpeg_available};
use super::flac::encode_flac_streaming;
use super::render::{MonoSource, Renderer, SourceProvider};
use super::wav::encode_wav_streaming;
use super::{AudioError, PcmSource};

// ---------------------------------------------------------------------------
// AudioFormat
// ---------------------------------------------------------------------------

/// Audio export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// 24-bit FLAC (lossless).
    Flac,
    /// f32le WAV (lossless, bit-exact round-trip).
    Wav,
    /// MP3 via ffmpeg (lossy; requires `ffmpeg` on PATH).
    Mp3,
    /// Ogg Vorbis via ffmpeg (lossy; requires `ffmpeg` on PATH).
    Ogg,
    /// AAC via ffmpeg (lossy; requires `ffmpeg` on PATH).
    Aac,
}

/// Map an output-file extension to an [`AudioFormat`].
///
/// Returns `Err(AudioError::ExportUnsupportedFormat)` for unrecognised extensions. The extension is
/// matched case-insensitively.
/// Extension wins over any caller-supplied format (see design/audio-pipeline.md § Format selection).
pub fn audio_format_for(path: &Path) -> Result<AudioFormat, AudioError> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("flac") => Ok(AudioFormat::Flac),
        Some("wav") => Ok(AudioFormat::Wav),
        Some("mp3") => Ok(AudioFormat::Mp3),
        Some("ogg") => Ok(AudioFormat::Ogg),
        Some("aac") => Ok(AudioFormat::Aac),
        _ => Err(AudioError::ExportUnsupportedFormat),
    }
}

// ---------------------------------------------------------------------------
// export_audio
// ---------------------------------------------------------------------------

/// Export the rendered audio from `renderer` to `out` in `format`.
///
/// `mono` collapses the stereo render to one channel (mean of L + R) before encoding. The render
/// length — including any trailing silence padding — is governed by the renderer's
/// [`EdlCursor`](super::edl::EdlCursor): build it with the desired `end` (e.g. the project length)
/// via [`EdlCursor::build`](super::edl::EdlCursor::build) before calling, so the export is
/// sample-for-sample identical to playback over the same range (test X23).
///
/// `format` is the codec; resolve it from the output extension with [`audio_format_for`] at the
/// caller (extension wins). `mp3`/`ogg`/`aac` require `ffmpeg` on PATH — otherwise, or for an
/// unsupported format, returns `export_unsupported_format`. Each encoder removes a partial `out`
/// on failure.
pub fn export_audio<P: SourceProvider + 'static>(
    renderer: Renderer<P>,
    format: AudioFormat,
    mono: bool,
    out: &Path,
) -> Result<(), AudioError> {
    // ffmpeg formats: verify availability before consuming the renderer.
    let needs_ffmpeg = matches!(
        format,
        AudioFormat::Mp3 | AudioFormat::Ogg | AudioFormat::Aac
    );
    if needs_ffmpeg && !ffmpeg_available() {
        return Err(AudioError::ExportUnsupportedFormat);
    }

    // `mono` wraps the renderer; both arms erase to a single boxed source for the encoders.
    let src: Box<dyn PcmSource> = if mono {
        Box::new(MonoSource::new(renderer))
    } else {
        Box::new(renderer)
    };

    match format {
        AudioFormat::Flac => encode_flac_streaming(src, out).map(|_| ()),
        AudioFormat::Wav => encode_wav_streaming(src, out),
        AudioFormat::Mp3 => encode_via_ffmpeg(src, out, "libmp3lame"),
        AudioFormat::Ogg => encode_via_ffmpeg(src, out, "libvorbis"),
        AudioFormat::Aac => encode_via_ffmpeg(src, out, "aac"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use tempfile::TempDir;

    use crate::audio::edl::EdlCursor;
    use crate::audio::ffmpeg::ffmpeg_available;
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{encode_turn, Splice, SpliceKind, Turn};

    fn decode_audio(path: &Path) -> (Vec<f32>, u32, u16) {
        let d = crate::audio::decode::decode(path).unwrap();
        (d.samples, d.sample_rate, d.channels)
    }

    // ---------------------------------------------------------------------------
    // Shared test infrastructure
    // ---------------------------------------------------------------------------

    /// Minimal in-memory [`SourceProvider`]. Serves `n` interleaved f32 frames from
    /// a per-track buffer at the requested offset; out-of-bounds → zero-pad.
    struct MockProvider {
        dry: BTreeMap<u32, Vec<f32>>,
        channels: BTreeMap<u32, u16>,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                dry: BTreeMap::new(),
                channels: BTreeMap::new(),
            }
        }

        fn track(mut self, id: u32, ch: u16, dry: Vec<f32>) -> Self {
            self.channels.insert(id, ch);
            self.dry.insert(id, dry);
            self
        }
    }

    impl SourceProvider for MockProvider {
        fn dry(&mut self, track_id: u32, from: i64, n: i64) -> Result<Vec<f32>, AudioError> {
            let ch = *self.channels.get(&track_id).unwrap_or(&1) as usize;
            let buf = self.dry.get(&track_id).map_or(&[][..], |v| v.as_slice());
            let start = (from as usize * ch).min(buf.len());
            let end = ((from as usize + n as usize) * ch).min(buf.len());
            let mut out = vec![0.0f32; n as usize * ch];
            out[..end - start].copy_from_slice(&buf[start..end]);
            Ok(out)
        }

        fn enhanced(&mut self, _: u32, _: i64, _: i64) -> Result<Option<Vec<f32>>, AudioError> {
            Ok(None)
        }

        fn room_tone(&mut self, _: u32) -> Result<Option<&[f32]>, AudioError> {
            Ok(None)
        }

        fn channels(&self, track_id: u32) -> u16 {
            *self.channels.get(&track_id).unwrap_or(&1)
        }

        fn wet_ratio(&self, _: u32) -> f32 {
            0.0
        }

        fn source_len(&self, track_id: u32) -> i64 {
            let ch = *self.channels.get(&track_id).unwrap_or(&1) as i64;
            self.dry.get(&track_id).map_or(0, |v| v.len() as i64 / ch)
        }
    }

    /// One-turn tree covering `[0, frames)` from source frame 0.
    fn make_tree(turn_id: u64, frames: i64) -> ImplicitTimelineTree<Turn> {
        let turn = Turn {
            id: turn_id,
            speaker_id: None,
            turn_duration: frames,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![Splice {
                length_samples: frames,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            }],
        };
        let (h, _) = encode_turn(&turn).unwrap();
        ImplicitTimelineTree::new()
            .insert_at(0, h, Arc::new(turn))
            .unwrap()
    }

    /// Build a renderer over one track for `[0, end)` (the cursor pads to `end`) and export it.
    #[allow(clippy::too_many_arguments)]
    fn export_one(
        tree: &ImplicitTimelineTree<Turn>,
        provider: MockProvider,
        track_id: u32,
        project_start_sample: i64,
        end: i64,
        max_fade_samples: usize,
        project_rate: u32,
        format: AudioFormat,
        mono: bool,
        out: &Path,
    ) -> Result<(), AudioError> {
        let cursor = EdlCursor::build(&[(track_id, project_start_sample, tree)], 0, Some(end));
        let renderer = Renderer::new(cursor, provider, max_fade_samples, project_rate);
        export_audio(renderer, format, mono, out)
    }

    /// Build a renderer over multiple tracks for `[0, end)` and export it.
    #[allow(clippy::too_many_arguments)]
    fn export_many(
        trees: &[(u32, i64, &ImplicitTimelineTree<Turn>)],
        provider: MockProvider,
        end: i64,
        max_fade_samples: usize,
        project_rate: u32,
        format: AudioFormat,
        mono: bool,
        out: &Path,
    ) -> Result<(), AudioError> {
        let cursor = EdlCursor::build(trees, 0, Some(end));
        let renderer = Renderer::new(cursor, provider, max_fade_samples, project_rate);
        export_audio(renderer, format, mono, out)
    }

    // A6: Extension routing for known audio formats.
    #[test]
    fn a6_audio_format_for_known() {
        assert_eq!(
            audio_format_for(Path::new("out.flac")).unwrap(),
            AudioFormat::Flac,
            "A6: flac"
        );
        assert_eq!(
            audio_format_for(Path::new("out.wav")).unwrap(),
            AudioFormat::Wav,
            "A6: wav"
        );
        assert_eq!(
            audio_format_for(Path::new("out.mp3")).unwrap(),
            AudioFormat::Mp3,
            "A6: mp3"
        );
        assert_eq!(
            audio_format_for(Path::new("out.ogg")).unwrap(),
            AudioFormat::Ogg,
            "A6: ogg"
        );
        assert_eq!(
            audio_format_for(Path::new("out.aac")).unwrap(),
            AudioFormat::Aac,
            "A6: aac"
        );
        // Extension matching is case-insensitive.
        assert_eq!(
            audio_format_for(Path::new("out.FLAC")).unwrap(),
            AudioFormat::Flac,
            "A6: FLAC"
        );
        assert_eq!(
            audio_format_for(Path::new("out.Mp3")).unwrap(),
            AudioFormat::Mp3,
            "A6: Mp3"
        );
    }

    // A7: Unknown extension → ExportUnsupportedFormat.
    #[test]
    fn a7_audio_format_for_unknown() {
        for name in &["out.xyz", "no_extension", "out.txt", "out."] {
            let err = audio_format_for(Path::new(name)).unwrap_err();
            assert_eq!(
                err.error_key(),
                "export_unsupported_format",
                "A7: error key for {name}"
            );
            assert!(
                matches!(err, AudioError::ExportUnsupportedFormat),
                "A7: variant for {name}"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // export_track / export_mixed / render loop tests
    // ---------------------------------------------------------------------------

    // A3: Silence padding — a track shorter than project_length has zero-filled tail.
    #[test]
    fn a3_silence_padding_to_project_length() {
        let track_frames = 100i64;
        let project_length = 200i64;
        // Mono track: all 0.5
        let dry = vec![0.5f32; track_frames as usize];
        let tree = make_tree(1, track_frames);
        let provider = MockProvider::new().track(1, 1, dry);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        export_one(
            &tree,
            provider,
            1,
            0,
            project_length,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap();

        let (decoded, _, ch) = decode_audio(&out);
        assert_eq!(ch, 2, "A3: stereo output");
        // Stereo: project_length frames × 2 channels.
        assert_eq!(
            decoded.len(),
            project_length as usize * 2,
            "A3: total length == project_length"
        );
        // First track_frames stereo frames: L == R == 0.5 (mono up-mix).
        for i in 0..track_frames as usize {
            assert!(
                (decoded[i * 2] - 0.5).abs() < 1e-6,
                "A3: L[{i}] should be 0.5"
            );
            assert!(
                (decoded[i * 2 + 1] - 0.5).abs() < 1e-6,
                "A3: R[{i}] should be 0.5"
            );
        }
        // Silence pad: remaining frames must be exactly 0.
        for i in track_frames as usize..project_length as usize {
            assert_eq!(decoded[i * 2], 0.0, "A3: pad L[{i}] should be silence");
            assert_eq!(decoded[i * 2 + 1], 0.0, "A3: pad R[{i}] should be silence");
        }
    }

    // A4: Mono collapse — output has 1 channel, each sample == (L + R) / 2.
    #[test]
    fn a4_mono_collapse() {
        let frames = 100i64;
        // Stereo track: L = 0.3, R = 0.7 for every frame.
        let dry: Vec<f32> = std::iter::repeat_n([0.3f32, 0.7], frames as usize)
            .flatten()
            .collect();
        let tree = make_tree(1, frames);
        let provider = MockProvider::new().track(1, 2, dry);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        export_one(
            &tree,
            provider,
            1,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            true, // mono
            &out,
        )
        .unwrap();

        let (decoded, _, ch) = decode_audio(&out);
        assert_eq!(ch, 1, "A4: mono output");
        assert_eq!(decoded.len(), frames as usize, "A4: frame count");
        let expected = (0.3f32 + 0.7) / 2.0; // 0.5
        for (i, &s) in decoded.iter().enumerate() {
            assert!(
                (s - expected).abs() < 1e-6,
                "A4: sample[{i}]: expected {expected}, got {s}"
            );
        }
    }

    // A4 mono via FLAC (verifies mono header in FLAC path too).
    #[test]
    fn a4_mono_collapse_flac() {
        let frames = 96i64;
        let dry: Vec<f32> = std::iter::repeat_n([0.6f32, 0.4], frames as usize)
            .flatten()
            .collect();
        let tree = make_tree(2, frames);
        let provider = MockProvider::new().track(2, 2, dry);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        export_one(
            &tree,
            provider,
            2,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Flac,
            true,
            &out,
        )
        .unwrap();

        let (decoded, _, ch) = decode_audio(&out);
        assert_eq!(ch, 1, "A4 FLAC: mono output");
        let expected = (0.6f32 + 0.4) / 2.0;
        let bound = 2.0_f32 / (1 << 23) as f32;
        for (i, &s) in decoded.iter().enumerate() {
            assert!((s - expected).abs() <= bound, "A4 FLAC: sample[{i}]: {s}");
        }
    }

    // A5: Mixed export sums tracks — decoded PCM == clamped sum of both tracks.
    #[test]
    fn a5_mixed_export_sums_tracks() {
        let frames = 100i64;
        // Track 1: all +0.3 (mono)
        let dry1 = vec![0.3f32; frames as usize];
        // Track 2: all +0.4 (mono)
        let dry2 = vec![0.4f32; frames as usize];
        let tree1 = make_tree(1, frames);
        let tree2 = make_tree(2, frames);
        let provider = MockProvider::new().track(1, 1, dry1).track(2, 1, dry2);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let trees = [(1u32, 0i64, &tree1), (2u32, 0i64, &tree2)];
        export_many(
            &trees,
            provider,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap();

        let (decoded, _, ch) = decode_audio(&out);
        assert_eq!(ch, 2, "A5: stereo output");
        assert_eq!(decoded.len(), frames as usize * 2, "A5: frame count");
        // 0.3 + 0.4 = 0.7, up-mixed to L=R=0.7, no clamping needed.
        let expected = 0.7f32;
        for i in 0..frames as usize {
            assert!(
                (decoded[i * 2] - expected).abs() < 1e-5,
                "A5: L[{i}] expected {expected}"
            );
            assert!(
                (decoded[i * 2 + 1] - expected).abs() < 1e-5,
                "A5: R[{i}] expected {expected}"
            );
        }
    }

    // A5: Mixed export with clamping — two +0.8 tracks → sum +1.6 clamped to +1.0.
    #[test]
    fn a5_mixed_export_clamps() {
        let frames = 50i64;
        let dry1 = vec![0.8f32; frames as usize];
        let dry2 = vec![0.8f32; frames as usize];
        let tree1 = make_tree(1, frames);
        let tree2 = make_tree(2, frames);
        let provider = MockProvider::new().track(1, 1, dry1).track(2, 1, dry2);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let trees = [(1u32, 0i64, &tree1), (2u32, 0i64, &tree2)];
        export_many(
            &trees,
            provider,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap();

        let (decoded, _, _) = decode_audio(&out);
        for i in 0..frames as usize {
            assert!(
                (decoded[i * 2] - 1.0).abs() < 1e-6,
                "A5 clamp: L[{i}] should be 1.0 (clamped)"
            );
        }
    }

    // A8: Export writes only to out — no side effects in the temp dir.
    #[test]
    fn a8_not_cached() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let tree = make_tree(1, 50);
        let provider = MockProvider::new().track(1, 1, vec![0.3f32; 50]);
        export_one(
            &tree,
            provider,
            1,
            0,
            50,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap();

        // Only the output file itself should exist in the temp directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "A8: only out.wav should exist");
        assert_eq!(entries[0], out, "A8: the only file is out");
    }

    // A9: Determinism — same project → byte-identical WAV export twice.
    #[test]
    fn a9_export_determinism() {
        let frames = 150i64;
        let dry: Vec<f32> = (0..frames as usize).map(|i| i as f32 * 0.006).collect();
        let tree = make_tree(1, frames);
        let dir = TempDir::new().unwrap();

        let out_a = dir.path().join("a.wav");
        let p_a = MockProvider::new().track(1, 1, dry.clone());
        export_one(
            &tree,
            p_a,
            1,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out_a,
        )
        .unwrap();

        let out_b = dir.path().join("b.wav");
        let p_b = MockProvider::new().track(1, 1, dry.clone());
        export_one(
            &tree,
            p_b,
            1,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out_b,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(&out_a).unwrap(),
            std::fs::read(&out_b).unwrap(),
            "A9: byte-identical WAV for identical input"
        );
    }

    // X21: Unwritable path → Io error; no partial file left behind.
    #[test]
    fn x21_write_failure_no_partial_file() {
        let dir = TempDir::new().unwrap();
        // Parent directory does not exist → File::create fails immediately.
        let out = dir.path().join("nonexistent_subdir").join("out.wav");
        let tree = make_tree(1, 50);
        let provider = MockProvider::new().track(1, 1, vec![0.5f32; 50]);
        let err = export_one(
            &tree,
            provider,
            1,
            0,
            50,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap_err();
        assert!(matches!(err, AudioError::Io(_)), "X21: should be Io error");
        assert_eq!(err.error_key(), "audio_io_error", "X21: error_key");
        assert!(!out.exists(), "X21: no partial file");
    }

    // X22: No SQLite connection — export uses only tree + MockProvider, no Db.
    #[test]
    fn x22_no_sqlite_connection() {
        let frames = 80i64;
        let dry: Vec<f32> = (0..frames as usize).map(|i| i as f32 * 0.01).collect();
        let tree = make_tree(1, frames);
        let provider = MockProvider::new().track(1, 1, dry);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        // Should complete without needing a Db.
        export_one(
            &tree,
            provider,
            1,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap();
        let (decoded, _, _) = decode_audio(&out);
        assert_eq!(
            decoded.len(),
            frames as usize * 2,
            "X22: exported correct frame count"
        );
    }

    // ---------------------------------------------------------------------------
    // ffmpeg encode path tests
    // ---------------------------------------------------------------------------

    // F19: MP3 export via ffmpeg — file exists and decodes to a non-empty, finite signal.
    // Skipped when ffmpeg is not available on the system.
    #[test]
    fn f19_mp3_export() {
        if !ffmpeg_available() {
            return;
        }
        // 0.1 s at 48 kHz — enough for a valid MP3 file.
        let frames = 4800i64;
        let dry: Vec<f32> = (0..frames as usize)
            .map(|i| (i as f32 * 0.01_f32).sin() * 0.5)
            .collect();
        let tree = make_tree(1, frames);
        let provider = MockProvider::new().track(1, 1, dry);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.mp3");
        export_one(
            &tree,
            provider,
            1,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Mp3,
            false,
            &out,
        )
        .unwrap();
        assert!(out.exists(), "F19: mp3 file must exist");
        // Decode and check basic properties (lossy — no exact-match, just sanity).
        let (decoded, _, _) = decode_audio(&out);
        assert!(!decoded.is_empty(), "F19: decoded mp3 must be non-empty");
        for &s in &decoded {
            assert!(s.is_finite(), "F19: all samples must be finite");
        }
    }

    // F20: No ffmpeg → ExportUnsupportedFormat for mp3/ogg/aac.
    // Only exercises the no-ffmpeg path; skipped when ffmpeg is available (can't force absent).
    #[test]
    fn f20_no_ffmpeg_returns_unsupported() {
        if ffmpeg_available() {
            return; // Cannot test absent-ffmpeg branch when ffmpeg is present.
        }
        let tree = make_tree(1, 100);
        let dir = TempDir::new().unwrap();
        for ext in &["mp3", "ogg", "aac"] {
            let provider = MockProvider::new().track(1, 1, vec![0.3f32; 100]);
            let out = dir.path().join(format!("out.{ext}"));
            let err = export_one(
                &tree,
                provider,
                1,
                0,
                100,
                0,
                48_000,
                AudioFormat::Mp3,
                false,
                &out,
            )
            .unwrap_err();
            assert!(
                matches!(err, AudioError::ExportUnsupportedFormat),
                "F20: {ext} must be ExportUnsupportedFormat when ffmpeg absent"
            );
            assert_eq!(
                err.error_key(),
                "export_unsupported_format",
                "F20: {ext} error_key"
            );
            assert!(!out.exists(), "F20: {ext} no partial file");
        }
    }

    // X23: Renderer parity — exported WAV matches direct render, sample-for-sample.
    #[test]
    fn x23_renderer_parity() {
        let frames = 200i64;
        let dry: Vec<f32> = (0..frames as usize)
            .map(|i| (i as f32 * 0.013).sin() * 0.5)
            .collect();
        let tree = make_tree(1, frames);

        // Direct render via the same cursor assembly the export uses.
        let p_direct = MockProvider::new().track(1, 1, dry.clone());
        let cursor = EdlCursor::build(&[(1, 0, &tree)], 0, Some(frames));
        let mut renderer = Renderer::new(cursor, p_direct, 0, 48_000);
        let mut direct: Vec<f32> = Vec::new();
        loop {
            let chunk = renderer.render(128).unwrap();
            if chunk.is_empty() {
                break;
            }
            direct.extend_from_slice(&chunk);
        }

        // Export to WAV via export_track.
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let p_export = MockProvider::new().track(1, 1, dry.clone());
        export_one(
            &tree,
            p_export,
            1,
            0,
            frames,
            0,
            48_000,
            AudioFormat::Wav,
            false,
            &out,
        )
        .unwrap();
        let (decoded, _, _) = decode_audio(&out);

        assert_eq!(decoded.len(), direct.len(), "X23: length matches");
        assert_eq!(
            decoded, direct,
            "X23: exported WAV == direct render (bit-exact)"
        );
    }
}
