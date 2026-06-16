//! Symphonia-based audio decoding and format probe.

use std::path::Path;

use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions},
    errors::Error as SymphoniaError,
    formats::{probe::Hint, FormatOptions, FormatReader, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

use super::{AudioError, AudioProbe, PcmSource};

#[cfg(test)]
use super::DecodedAudio;

// ---------------------------------------------------------------------------
// Shared Symphonia open result
// ---------------------------------------------------------------------------

/// Result of opening a Symphonia stream: format reader, decoder, and stream metadata.
///
/// Returned by [`open_symphonia`] and consumed by the streaming impls
/// ([`SymphoniaSource`], [`super::frame_reader::SymphoniaFrameReader`]) to avoid
/// repeating the open/probe/track-resolve/decoder-build sequence.
pub(crate) struct SymphoniaOpen {
    pub format: Box<dyn FormatReader>,
    pub decoder: Box<dyn AudioDecoder>,
    pub track_id: u32,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Open `path` via Symphonia: probe format, resolve the default audio track, build the decoder.
///
/// Returns [`AudioError::UnsupportedFormat`] when the format or codec is unrecognised — the
/// caller can use this to route to the ffmpeg fallback. Real I/O errors are preserved as
/// [`AudioError::Io`].
pub(crate) fn open_symphonia(path: &Path) -> Result<SymphoniaOpen, AudioError> {
    let src = std::fs::File::open(path).map_err(AudioError::Io)?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(map_probe_error)?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(AudioError::UnsupportedFormat { codec: None })?;

    let track_id = track.id;
    // Clone so that the immutable borrow of `format` ends before it is moved into SymphoniaOpen.
    let audio_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(AudioError::UnsupportedFormat { codec: None })?
        .clone();

    let sample_rate = audio_params.sample_rate.unwrap_or(0);
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(0);

    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| AudioError::UnsupportedFormat {
            codec: Some(e.to_string()),
        })?;

    Ok(SymphoniaOpen {
        format,
        decoder,
        track_id,
        channels,
        sample_rate,
    })
}

/// Decode the next non-empty audio packet from a Symphonia stream, returning interleaved f32.
///
/// Returns `Ok(None)` at clean EOF. Skips packets for other tracks and zero-sample packets.
/// On format-level `ResetRequired` (chained streams such as Ogg), rebuilds the decoder from
/// the updated track params — more correct than a plain `reset()` for chained sources. On
/// decoder-level `ResetRequired`, resets without rebuilding (codec params changed mid-frame —
/// the packet is discarded). `UnexpectedEof` mid-stream indicates file truncation.
pub(crate) fn decode_next_packet(
    format: &mut Box<dyn FormatReader>,
    decoder: &mut Box<dyn AudioDecoder>,
    track_id: u32,
) -> Result<Option<Vec<f32>>, AudioError> {
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(None),
            Err(SymphoniaError::ResetRequired) => {
                // Chained stream: rebuild decoder from updated track params.
                if let Some(track) = format.default_track(TrackType::Audio) {
                    if let Some(ap) = track.codec_params.as_ref().and_then(|p| p.audio()) {
                        if let Ok(d) = symphonia::default::get_codecs()
                            .make_audio_decoder(ap, &AudioDecoderOptions::default())
                        {
                            *decoder = d;
                        }
                    }
                }
                continue;
            }
            Err(e) => return Err(map_next_packet_error(e)),
        };

        if packet.track_id != track_id {
            continue;
        }

        let audio_buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(SymphoniaError::DecodeError(msg)) => {
                return Err(AudioError::DecodeFailed(msg.to_string()));
            }
            Err(SymphoniaError::IoError(e)) => return Err(AudioError::Io(e)),
            Err(e) => return Err(AudioError::DecodeFailed(e.to_string())),
        };

        let n = audio_buf.samples_interleaved();
        if n == 0 {
            continue;
        }
        let mut samples = vec![0.0f32; n];
        audio_buf.copy_to_slice_interleaved(&mut samples);
        return Ok(Some(samples));
    }
}

// ---------------------------------------------------------------------------
// Format probe (production)
// ---------------------------------------------------------------------------

/// Read codec/rate/channel/length metadata without decoding all packets.
///
/// Falls back to the ffmpeg subprocess on an unsupported-format rejection, mirroring [`open_source`].
/// `length_frames` is best-effort; the authoritative count comes from a full decode at M4 import.
pub fn probe(path: &Path) -> Result<AudioProbe, AudioError> {
    match probe_symphonia(path) {
        Ok(p) => Ok(p),
        Err(AudioError::UnsupportedFormat { .. }) => {
            if super::ffmpeg::ffmpeg_available() {
                super::ffmpeg::probe_via_ffmpeg(path)
            } else {
                Err(AudioError::FfmpegUnavailable)
            }
        }
        Err(e) => Err(e),
    }
}

fn probe_symphonia(path: &Path) -> Result<AudioProbe, AudioError> {
    let src = std::fs::File::open(path).map_err(AudioError::Io)?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(map_probe_error)?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(AudioError::UnsupportedFormat { codec: None })?;

    let audio_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(AudioError::UnsupportedFormat { codec: None })?;

    let codec = codec_name(audio_params.codec);
    let sample_rate = audio_params.sample_rate.unwrap_or(0);
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(0);
    let length_frames = track.num_frames.map(|n| n as i64);

    Ok(AudioProbe {
        codec,
        sample_rate,
        channels,
        length_frames,
    })
}

// ---------------------------------------------------------------------------
// Streaming source decode (import transcode input)
// ---------------------------------------------------------------------------

/// A streaming [`PcmSource`] over a Symphonia-decodable file: pulls and decodes packets on
/// demand at the source's native rate/channels, so the import transcode never materializes
/// the whole signal. Leftover samples from a decoded packet that don't fit the caller's
/// buffer are held until the next `read`.
pub struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    channels: u16,
    sample_rate: u32,
    leftover: Vec<f32>,
    leftover_pos: usize,
    eof: bool,
    exhausted: bool,
}

impl SymphoniaSource {
    /// Open `path` for streaming decode. Errors mirror [`decode`] (e.g. `UnsupportedFormat`
    /// routes the caller to the ffmpeg fallback).
    fn open(path: &Path) -> Result<Self, AudioError> {
        let o = open_symphonia(path)?;
        Ok(Self {
            format: o.format,
            decoder: o.decoder,
            track_id: o.track_id,
            channels: o.channels,
            sample_rate: o.sample_rate,
            leftover: Vec::new(),
            leftover_pos: 0,
            eof: false,
            exhausted: false,
        })
    }

    /// Decode the next non-empty audio packet into `leftover`. Returns `false` at clean EOF.
    fn fill_one_packet(&mut self) -> Result<bool, AudioError> {
        match decode_next_packet(&mut self.format, &mut self.decoder, self.track_id)? {
            None => Ok(false),
            Some(samples) => {
                self.leftover = samples;
                self.leftover_pos = 0;
                Ok(true)
            }
        }
    }
}

impl PcmSource for SymphoniaSource {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let ch = self.channels as usize;
        if ch == 0 {
            self.exhausted = true;
            return Ok(0);
        }
        let mut filled = 0;
        while filled < out.len() {
            let avail = self.leftover.len() - self.leftover_pos;
            if avail > 0 {
                let take = (out.len() - filled).min(avail);
                out[filled..filled + take]
                    .copy_from_slice(&self.leftover[self.leftover_pos..self.leftover_pos + take]);
                self.leftover_pos += take;
                filled += take;
                continue;
            }
            if self.eof {
                break;
            }
            if !self.fill_one_packet()? {
                self.eof = true;
            }
        }
        if self.eof && self.leftover_pos >= self.leftover.len() {
            self.exhausted = true;
        }
        Ok(filled / ch)
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }
}

/// Open a source file as a streaming [`PcmSource`] for the import transcode.
///
/// Tries the streaming Symphonia path first; on an `UnsupportedFormat` rejection falls back to
/// [`FfmpegSource`](super::ffmpeg::FfmpegSource), which streams raw f32le from an ffmpeg
/// subprocess pipe (truly streaming — no whole-buffer decode).
pub fn open_source(path: &Path) -> Result<Box<dyn PcmSource>, AudioError> {
    match SymphoniaSource::open(path) {
        Ok(s) => Ok(Box::new(s)),
        Err(AudioError::UnsupportedFormat { .. }) => {
            if super::ffmpeg::ffmpeg_available() {
                Ok(Box::new(super::ffmpeg::FfmpegSource::open(path)?))
            } else {
                Err(AudioError::FfmpegUnavailable)
            }
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Map a terminal `next_packet` error from [`decode_next_packet`] to an [`AudioError`].
///
/// `UnexpectedEof` mid-stream means the file was truncated mid-packet — surfaced as
/// `DecodeFailed` (the format *was* recognised, unlike the probe stage). Any other I/O error is
/// preserved as `Io` so a permission/disk failure is not misreported as corruption. The
/// caller handles `ResetRequired` separately (it rebuilds the decoder rather than failing).
fn map_next_packet_error(e: SymphoniaError) -> AudioError {
    match e {
        SymphoniaError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            AudioError::DecodeFailed(format!("truncated file: {io}"))
        }
        SymphoniaError::IoError(io) => AudioError::Io(io),
        e => AudioError::DecodeFailed(e.to_string()),
    }
}

/// Map a Symphonia error from the format-probe stage to an [`AudioError`].
///
/// `UnexpectedEof` during probing means the file is too short to identify — mapped to
/// `UnsupportedFormat` so the ffmpeg fallback is tried.  Actual I/O errors are preserved.
fn map_probe_error(e: SymphoniaError) -> AudioError {
    match e {
        SymphoniaError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            AudioError::UnsupportedFormat { codec: None }
        }
        SymphoniaError::IoError(io) => AudioError::Io(io),
        SymphoniaError::Unsupported(_) | SymphoniaError::DecodeError(_) => {
            AudioError::UnsupportedFormat { codec: None }
        }
        e => AudioError::UnsupportedFormat {
            codec: Some(e.to_string()),
        },
    }
}

/// Return the pinned short-name string for an `AudioCodecId` via the codec registry.
///
/// Pinned so that a Symphonia upgrade can't silently rename the value flowing into
/// `TrackMeta.codec` (test T9 guards this).
fn codec_name(id: symphonia::core::codecs::audio::AudioCodecId) -> String {
    symphonia::default::get_codecs()
        .get_audio_decoder(id)
        .map(|r| r.codec.info.short_name.to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Whole-buffer decode — test support only
//
// No production code calls these. They serve as the oracle for streaming-reader
// tests (FR3/FR4, TC2, cache tests) and for cross-format fixture comparisons.
// ---------------------------------------------------------------------------

/// Decode a source file to interleaved f32 PCM at its native rate.
///
/// Tries Symphonia first. Falls back to the ffmpeg subprocess only on a format-unsupported
/// rejection — not on I/O errors or mid-stream decode errors from a recognised format.
///
/// **Test support only.** Production code uses the streaming [`open_source`] path.
#[cfg(test)]
pub(crate) fn decode(path: &Path) -> Result<DecodedAudio, AudioError> {
    match decode_symphonia(path) {
        Ok(audio) => Ok(audio),
        Err(AudioError::UnsupportedFormat { .. }) => {
            if super::ffmpeg::ffmpeg_available() {
                super::ffmpeg::decode_via_ffmpeg(path)
            } else {
                Err(AudioError::FfmpegUnavailable)
            }
        }
        Err(e) => Err(e),
    }
}

/// Whole-buffer Symphonia decode. Built on [`open_symphonia`] + [`decode_next_packet`].
#[cfg(test)]
fn decode_symphonia(path: &Path) -> Result<DecodedAudio, AudioError> {
    let mut o = open_symphonia(path)?;
    let mut samples: Vec<f32> = Vec::new();
    loop {
        match decode_next_packet(&mut o.format, &mut o.decoder, o.track_id)? {
            None => break,
            Some(pkt) => samples.extend_from_slice(&pkt),
        }
    }
    Ok(DecodedAudio {
        samples,
        sample_rate: o.sample_rate,
        channels: o.channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // WAV writing helpers (generated-in-test fixtures, never committed)
    // -----------------------------------------------------------------------

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
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
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

    /// `samples` are 24-bit values packed in the low 3 bytes of each `i32` (little-endian).
    fn write_wav_s24(path: &Path, sample_rate: u32, channels: u16, samples: &[i32]) {
        let data_size = (samples.len() * 3) as u32;
        let byte_rate = sample_rate * channels as u32 * 3;
        let block_align = channels * 3;
        let mut buf = Vec::with_capacity(44 + samples.len() * 3);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_size).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&24u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        for &s in samples {
            let b = s.to_le_bytes();
            buf.extend_from_slice(&b[0..3]); // low 3 bytes, little-endian
        }
        std::fs::write(path, &buf).expect("write WAV s24");
    }

    fn write_wav_f32(path: &Path, sample_rate: u32, channels: u16, samples: &[f32]) {
        let data_size = (samples.len() * 4) as u32;
        let byte_rate = sample_rate * channels as u32 * 4;
        let block_align = channels * 4;
        let mut buf = Vec::with_capacity(44 + samples.len() * 4);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_size).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&32u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, &buf).expect("write WAV f32");
    }

    /// Raw Symphonia decode: returns Symphonia's result without any ffmpeg routing.
    /// Used for E13/E14 to verify Symphonia itself returns a typed error.
    fn decode_symphonia_only(path: &Path) -> Result<DecodedAudio, AudioError> {
        decode_symphonia(path)
    }

    /// Decode with the full routing but ffmpeg branch disabled.
    /// Maps UnsupportedFormat → FfmpegUnavailable, mirroring decode() when ffmpeg is absent.
    /// Used for E/F21 to verify the routing returns FfmpegUnavailable.
    fn decode_routing_no_ffmpeg(path: &Path) -> Result<DecodedAudio, AudioError> {
        match decode_symphonia(path) {
            Ok(audio) => Ok(audio),
            Err(AudioError::UnsupportedFormat { .. }) => Err(AudioError::FfmpegUnavailable),
            Err(e) => Err(e),
        }
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/audio")
            .join(name)
    }

    // -----------------------------------------------------------------------
    // T1 — WAV s16 mono, exact PCM values
    // -----------------------------------------------------------------------

    #[test]
    fn t1_wav_s16_mono_exact_pcm() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mono.wav");
        // Values chosen so s/32768.0 is exactly representable as f32.
        let src: Vec<i16> = vec![0, 16384, -16384, i16::MIN, i16::MAX];
        write_wav_s16(&path, 48000, 1, &src);

        let decoded = decode(&path).expect("T1: decode should succeed");
        assert_eq!(decoded.channels, 1, "T1: channels");
        assert_eq!(decoded.sample_rate, 48000, "T1: sample rate");
        assert_eq!(decoded.frames(), src.len(), "T1: frame count");
        assert_eq!(decoded.samples.len(), src.len(), "T1: sample count");

        let tolerance = 2.0 / 32768.0_f32; // 2 LSBs
        let expected: Vec<f32> = src.iter().map(|&s| s as f32 / 32768.0).collect();
        for (i, (&got, &exp)) in decoded.samples.iter().zip(expected.iter()).enumerate() {
            assert!(
                (got - exp).abs() <= tolerance,
                "T1: sample[{i}]: got {got}, expected {exp}"
            );
            assert!(
                (-1.0..=1.0).contains(&got),
                "T1: sample[{i}] out of [-1,1]: {got}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // T2 — WAV s16 stereo, interleave order and no channel collapse
    // -----------------------------------------------------------------------

    #[test]
    fn t2_wav_s16_stereo_interleave() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stereo.wav");
        // L=+0.5 (16384), R=−0.5 (−16384), repeated 10 times.
        let src: Vec<i16> = std::iter::repeat_n([16384i16, -16384i16], 10)
            .flatten()
            .collect();
        write_wav_s16(&path, 48000, 2, &src);

        let decoded = decode(&path).expect("T2: decode should succeed");
        assert_eq!(decoded.channels, 2, "T2: channels");
        assert_eq!(decoded.sample_rate, 48000, "T2: sample rate");
        assert_eq!(decoded.frames(), 10, "T2: frame count");
        assert_eq!(decoded.samples.len(), 20, "T2: interleaved sample count");

        let tolerance = 2.0 / 32768.0_f32;
        // Odd indices are L (≈+0.5), even indices are R (≈−0.5).
        for i in (0..decoded.samples.len()).step_by(2) {
            let l = decoded.samples[i];
            let r = decoded.samples[i + 1];
            assert!((l - 0.5).abs() <= tolerance, "T2: L[{i}]={l} not ≈ 0.5");
            assert!((r - (-0.5)).abs() <= tolerance, "T2: R[{i}]={r} not ≈ −0.5");
        }
    }

    // -----------------------------------------------------------------------
    // T3 — Sample-format coverage: s16, s24, f32 all land in [−1, 1]
    // -----------------------------------------------------------------------

    #[test]
    fn t3_sample_format_coverage() {
        let dir = TempDir::new().unwrap();

        // s16: mid-scale value → 0.5
        let path_s16 = dir.path().join("s16.wav");
        write_wav_s16(&path_s16, 44100, 1, &[16384i16, i16::MIN, i16::MAX]);
        let dec_s16 = decode(&path_s16).expect("T3: s16 decode");
        assert_eq!(dec_s16.channels, 1);
        assert!((dec_s16.samples[0] - 0.5).abs() < 1e-5, "T3: s16 mid-scale");
        assert!(
            (dec_s16.samples[1] - (-1.0)).abs() < 2.0 / 32768.0,
            "T3: s16 MIN≈−1.0"
        );
        assert!(
            (dec_s16.samples[2] - 1.0).abs() < 2.0 / 32768.0,
            "T3: s16 MAX≈+1.0"
        );
        for &s in &dec_s16.samples {
            assert!((-1.0..=1.0).contains(&s), "T3: s16 sample {s} out of range");
        }

        // s24: 0.5 * 2^23 = 4194304 → 0.5
        let path_s24 = dir.path().join("s24.wav");
        write_wav_s24(&path_s24, 44100, 1, &[4194304i32]);
        let dec_s24 = decode(&path_s24).expect("T3: s24 decode");
        assert!((dec_s24.samples[0] - 0.5).abs() < 1e-5, "T3: s24 mid-scale");
        for &s in &dec_s24.samples {
            assert!((-1.0..=1.0).contains(&s), "T3: s24 sample {s} out of range");
        }

        // f32: 0.5 → exactly 0.5 (bit-exact passthrough)
        let path_f32 = dir.path().join("f32.wav");
        write_wav_f32(&path_f32, 44100, 1, &[0.5f32, -1.0, 1.0]);
        let dec_f32 = decode(&path_f32).expect("T3: f32 decode");
        assert_eq!(dec_f32.samples[0], 0.5f32, "T3: f32 bit-exact");
        assert_eq!(dec_f32.samples[1], -1.0f32, "T3: f32 −1.0 bit-exact");
        assert_eq!(dec_f32.samples[2], 1.0f32, "T3: f32 +1.0 bit-exact");
        for &s in &dec_f32.samples {
            assert!((-1.0..=1.0).contains(&s), "T3: f32 sample {s} out of range");
        }
    }

    // -----------------------------------------------------------------------
    // T4 — FLAC == WAV (lossless cross-format round-trip, fixture files)
    // -----------------------------------------------------------------------

    #[test]
    fn t4_flac_equals_wav_lossless() {
        let wav = decode(&fixture("fixture_440hz.wav")).expect("T4: WAV decode");
        let flac = decode(&fixture("fixture_440hz.flac")).expect("T4: FLAC decode");

        assert_eq!(wav.sample_rate, flac.sample_rate, "T4: sample rate");
        assert_eq!(wav.channels, flac.channels, "T4: channels");
        assert_eq!(wav.samples.len(), flac.samples.len(), "T4: sample count");

        for (i, (&w, &f)) in wav.samples.iter().zip(flac.samples.iter()).enumerate() {
            assert_eq!(w, f, "T4: sample[{i}] WAV={w} FLAC={f}");
        }
    }

    // -----------------------------------------------------------------------
    // T5 — MP3 decode: correct metadata; all samples finite and in [−1, 1]
    // -----------------------------------------------------------------------

    #[test]
    fn t5_mp3_decode() {
        let decoded = decode(&fixture("fixture_440hz.mp3")).expect("T5: MP3 decode");
        assert_eq!(decoded.sample_rate, 44100, "T5: sample_rate");
        assert_eq!(decoded.channels, 1, "T5: channels");
        assert!(decoded.frames() > 0, "T5: non-empty");
        for &s in &decoded.samples {
            assert!(
                s.is_finite() && (-1.0..=1.0).contains(&s),
                "T5: sample {s} out of range"
            );
        }
    }

    // -----------------------------------------------------------------------
    // T6 — AAC-LC .m4a decodes via Symphonia (not ffmpeg)
    // -----------------------------------------------------------------------

    #[test]
    fn t6_aac_lc_m4a_decode_via_symphonia() {
        let decoded = decode(&fixture("fixture_440hz_aac.m4a")).expect("T6: AAC-LC decode");
        assert_eq!(decoded.sample_rate, 44100, "T6: sample_rate");
        assert_eq!(decoded.channels, 1, "T6: channels");
        assert!(decoded.frames() > 0, "T6: non-empty");
        for &s in &decoded.samples {
            assert!(
                s.is_finite() && (-1.0..=1.0).contains(&s),
                "T6: sample {s} out of range"
            );
        }
        let p = probe(&fixture("fixture_440hz_aac.m4a")).expect("T6: probe AAC");
        assert_eq!(p.codec, "aac", "T6: codec identifier");
    }

    // -----------------------------------------------------------------------
    // T7 — Empty-but-valid WAV: no error, 0 frames
    // -----------------------------------------------------------------------

    #[test]
    fn t7_empty_wav() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.wav");
        write_wav_s16(&path, 48000, 1, &[]);

        let decoded = decode(&path).expect("T7: empty WAV should succeed");
        assert_eq!(decoded.frames(), 0, "T7: frame count");
        assert!(decoded.samples.is_empty(), "T7: samples vec");

        let p = probe(&path).expect("T7: probe should succeed");
        assert_eq!(p.sample_rate, 48000);
        assert_eq!(p.channels, 1);
        assert_eq!(p.length_frames, Some(0), "T7: probe length_frames");
    }

    // -----------------------------------------------------------------------
    // T8 — probe() metadata matches decode() for all fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn t8_probe_matches_decode_all_fixtures() {
        for name in &[
            "fixture_440hz.wav",
            "fixture_440hz.flac",
            "fixture_440hz.mp3",
            "fixture_440hz_aac.m4a",
        ] {
            let path = fixture(name);
            let decoded = decode(&path).unwrap_or_else(|e| panic!("T8: decode {name}: {e}"));
            let p = probe(&path).unwrap_or_else(|e| panic!("T8: probe {name}: {e}"));

            assert_eq!(p.sample_rate, decoded.sample_rate, "T8: {name} sample_rate");
            assert_eq!(p.channels, decoded.channels, "T8: {name} channels");

            if let Some(plen) = p.length_frames {
                let decoded_frames = decoded.frames() as i64;
                let tolerance: i64 = if name.ends_with(".mp3") {
                    1152
                } else if name.ends_with(".m4a") {
                    2048
                } else {
                    0
                };
                assert!(
                    (plen - decoded_frames).abs() <= tolerance,
                    "T8: {name} probe length {plen} vs decode {decoded_frames} (tol {tolerance})"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // T9 (WAV only) — Pinned codec identifier for generated WAV
    // -----------------------------------------------------------------------

    #[test]
    fn t9_wav_pinned_codec_identifier() {
        let dir = TempDir::new().unwrap();

        let path_s16 = dir.path().join("s16.wav");
        write_wav_s16(&path_s16, 44100, 1, &[0i16]);
        let p = probe(&path_s16).expect("T9: probe s16 WAV");
        assert_eq!(p.codec, "pcm_s16le", "T9: s16 WAV codec");

        let path_f32 = dir.path().join("f32.wav");
        write_wav_f32(&path_f32, 44100, 1, &[0.0f32]);
        let pf = probe(&path_f32).expect("T9: probe f32 WAV");
        assert_eq!(pf.codec, "pcm_f32le", "T9: f32 WAV codec");
    }

    // -----------------------------------------------------------------------
    // T9 (fixture) — Pinned codec identifiers for committed fixture files
    // -----------------------------------------------------------------------

    #[test]
    fn t9_pinned_codec_identifiers_fixtures() {
        let cases: &[(&str, &str)] = &[
            ("fixture_440hz.wav", "pcm_s16le"),
            ("fixture_440hz.flac", "flac"),
            ("fixture_440hz.mp3", "mp3"),
            ("fixture_440hz_aac.m4a", "aac"),
        ];
        for &(name, expected_codec) in cases {
            let p =
                probe(&fixture(name)).unwrap_or_else(|e| panic!("T9-fixture: probe {name}: {e}"));
            assert_eq!(
                p.codec, expected_codec,
                "T9-fixture: {name} codec — update TrackMeta mapping if intentional"
            );
        }
    }

    // -----------------------------------------------------------------------
    // T10 — probe() vs decode() for a headerless MP3 (no Xing/Info tag)
    // -----------------------------------------------------------------------

    #[test]
    fn t10_probe_best_effort_vs_authoritative_decode_for_headerless_mp3() {
        let path = fixture("fixture_440hz_headerless.mp3");
        let p = probe(&path).expect("T10: probe headerless MP3");
        let decoded = decode(&path).expect("T10: decode headerless MP3");

        assert_eq!(p.codec, "mp3", "T10: codec");
        assert_eq!(p.sample_rate, 44100, "T10: sample_rate");
        assert_eq!(p.channels, 1, "T10: channels");
        assert!(decoded.frames() > 0, "T10: decoded frames > 0");

        if let Some(probe_len) = p.length_frames {
            let diff = (probe_len - decoded.frames() as i64).abs();
            assert!(
                diff < 5 * 1152,
                "T10: probe estimate {probe_len} is far from decode {}: diff {diff}",
                decoded.frames()
            );
        }
    }

    // -----------------------------------------------------------------------
    // T11 — Determinism: decode twice → byte-identical samples
    // -----------------------------------------------------------------------

    #[test]
    fn t11_determinism() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("det.wav");
        let src: Vec<i16> = (0..100).map(|i| (i * 327) as i16).collect();
        write_wav_s16(&path, 44100, 1, &src);

        let a = decode(&path).expect("T11: first decode");
        let b = decode(&path).expect("T11: second decode");
        assert_eq!(a.samples, b.samples, "T11: determinism");
        assert_eq!(a.sample_rate, b.sample_rate);
        assert_eq!(a.channels, b.channels);
    }

    // -----------------------------------------------------------------------
    // E12 — Nonexistent path → AudioError::Io (NotFound)
    // -----------------------------------------------------------------------

    #[test]
    fn e12_nonexistent_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no_such_file.wav");
        let err = decode(&path).expect_err("E12: should fail for nonexistent file");
        assert!(
            matches!(err, AudioError::Io(_)),
            "E12: expected Io, got {err:?}"
        );
        assert_eq!(err.error_key(), "audio_io_error", "E12: error_key");
    }

    // -----------------------------------------------------------------------
    // E13 — Zero-byte file → typed error, no panic
    // -----------------------------------------------------------------------

    #[test]
    fn e13_zero_byte_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.wav");
        std::fs::write(&path, b"").expect("write empty file");
        let err = decode_symphonia_only(&path).expect_err("E13: should fail for zero-byte file");
        assert!(
            matches!(
                err,
                AudioError::UnsupportedFormat { .. } | AudioError::DecodeFailed(_)
            ),
            "E13: expected UnsupportedFormat or DecodeFailed, got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // E14 — Garbage bytes with audio extension → UnsupportedFormat, no panic
    // -----------------------------------------------------------------------

    #[test]
    fn e14_garbage_with_audio_extension() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.wav");
        std::fs::write(
            &path,
            b"This is not audio data at all. Random garbage bytes here.",
        )
        .unwrap();
        let err = decode_symphonia_only(&path).expect_err("E14: garbage wav should fail");
        assert!(
            matches!(
                err,
                AudioError::UnsupportedFormat { .. } | AudioError::DecodeFailed(_)
            ),
            "E14: expected UnsupportedFormat or DecodeFailed, got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // E15 — Truncated supported file → AudioError::DecodeFailed, no partial success
    // -----------------------------------------------------------------------

    #[test]
    fn e15_truncated_wav() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("truncated.wav");
        let src: Vec<i16> = (0..2000).map(|i| (i * 16) as i16).collect();
        write_wav_s16(&path, 44100, 1, &src);

        // Truncate: keep the 44-byte header + 64 bytes of data (incomplete packet).
        let mut data = std::fs::read(&path).expect("read wav");
        data.truncate(44 + 64);
        std::fs::write(&path, &data).expect("write truncated");

        let err = decode(&path).expect_err("E15: truncated WAV should fail");
        assert!(
            matches!(err, AudioError::DecodeFailed(_)),
            "E15: expected DecodeFailed, got {err:?}"
        );
        assert_eq!(err.error_key(), "decode_failed", "E15: error_key");
    }

    // -----------------------------------------------------------------------
    // E16 — error_key() mapping is total across all variants
    // -----------------------------------------------------------------------

    #[test]
    fn e16_error_key_is_total() {
        use std::io;
        let cases: &[(&AudioError, &str)] = &[
            (
                &AudioError::Io(io::Error::new(io::ErrorKind::NotFound, "x")),
                "audio_io_error",
            ),
            (
                &AudioError::UnsupportedFormat { codec: None },
                "decode_unsupported_format",
            ),
            (
                &AudioError::UnsupportedFormat {
                    codec: Some("opus".into()),
                },
                "decode_unsupported_format",
            ),
            (&AudioError::DecodeFailed("corrupt".into()), "decode_failed"),
            (&AudioError::FfmpegUnavailable, "ffmpeg_unavailable"),
            (
                &AudioError::FfmpegFailed {
                    detail: "oops".into(),
                },
                "ffmpeg_failed",
            ),
        ];
        for (err, expected_key) in cases {
            assert_eq!(
                err.error_key(),
                *expected_key,
                "E16: {err:?} → expected key {expected_key}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // E/F21 — No ffmpeg + unsupported format → FfmpegUnavailable, no subprocess
    // -----------------------------------------------------------------------

    #[test]
    fn ef21_no_ffmpeg_unsupported_returns_unavailable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mystery.audio");
        std::fs::write(&path, b"FAKE_AUDIO_FORMAT\x00\x00\x00\x00FAKE_DATA").unwrap();
        let err = decode_routing_no_ffmpeg(&path)
            .expect_err("E/F21: unsupported format with no ffmpeg should fail");
        assert!(
            matches!(err, AudioError::FfmpegUnavailable),
            "E/F21: expected FfmpegUnavailable, got {err:?}"
        );
        assert_eq!(err.error_key(), "ffmpeg_unavailable");
    }

    // -----------------------------------------------------------------------
    // X22 — Supported formats never invoke the ffmpeg fallback
    // -----------------------------------------------------------------------

    #[test]
    fn x22_supported_formats_decode_successfully() {
        for name in &[
            "fixture_440hz.wav",
            "fixture_440hz.flac",
            "fixture_440hz.mp3",
            "fixture_440hz_aac.m4a",
        ] {
            let result = decode(&fixture(name));
            assert!(result.is_ok(), "X22: {name} should decode, got {result:?}");
        }
    }

    // -----------------------------------------------------------------------
    // F18 — Opus decodes via ffmpeg fallback
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "requires system ffmpeg on PATH"]
    fn f18_opus_decodes_via_ffmpeg() {
        assert!(
            super::super::ffmpeg::ffmpeg_available(),
            "F18: ffmpeg must be available"
        );
        let decoded = decode(&fixture("fixture_440hz.opus")).expect("F18: Opus decode");
        assert_eq!(decoded.sample_rate, 48000, "F18: sample_rate");
        assert_eq!(decoded.channels, 1, "F18: channels");
        assert!(decoded.frames() > 0, "F18: non-empty");
        for &s in &decoded.samples {
            assert!(
                s.is_finite() && (-1.0..=1.0).contains(&s),
                "F18: sample {s} out of range"
            );
        }
    }

    // -----------------------------------------------------------------------
    // F19 — ffmpeg fallback preserves native rate/channels
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "requires system ffmpeg on PATH"]
    fn f19_ffmpeg_fallback_preserves_native_rate_channels() {
        assert!(
            super::super::ffmpeg::ffmpeg_available(),
            "F19: ffmpeg must be available"
        );
        let path = fixture("fixture_440hz.opus");
        let p = probe(&path).expect("F19: probe opus");
        let decoded = decode(&path).expect("F19: decode opus");

        assert_eq!(
            decoded.sample_rate, p.sample_rate,
            "F19: decoded rate must match probe"
        );
        assert_eq!(
            decoded.channels, p.channels,
            "F19: decoded channels must match probe"
        );
        assert_eq!(decoded.sample_rate, 48000, "F19: native 48 kHz preserved");
        assert_eq!(decoded.channels, 1, "F19: native mono preserved");
    }

    // -----------------------------------------------------------------------
    // S1 — SymphoniaSource::read returns FRAMES, not interleaved samples
    //
    // The streaming source is the production import path (open_source); decode()
    // above is test-only. read() must report frames (samples / channels), so a
    // full stereo read of N interleaved samples returns N/2. This pins the
    // `filled / ch` division (a `* ch` mutant would return N*ch instead) and the
    // frame/sample distinction the resampler downstream depends on.
    // -----------------------------------------------------------------------
    #[test]
    fn s1_symphonia_source_read_returns_frames() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stereo_stream.wav");
        // 100 stereo frames = 200 interleaved samples, L = +0.25, R = −0.25.
        let src: Vec<i16> = std::iter::repeat_n([8192i16, -8192i16], 100)
            .flatten()
            .collect();
        write_wav_s16(&path, 44100, 2, &src);

        let mut source = SymphoniaSource::open(&path).expect("S1: open stream");
        assert_eq!(source.channels(), 2, "S1: channels");
        assert_eq!(source.sample_rate(), 44100, "S1: sample_rate");
        assert!(!source.is_exhausted(), "S1: fresh source is not exhausted");

        // Read the whole stream into a buffer larger than it.
        let mut out = vec![0.0f32; 1024];
        let frames = source.read(&mut out).expect("S1: read");
        assert_eq!(
            frames, 100,
            "S1: 200 interleaved samples / 2 channels = 100 frames"
        );
        assert!(
            source.is_exhausted(),
            "S1: exhausted after draining the stream"
        );

        // Interleave order preserved: even = L (+0.25), odd = R (−0.25).
        let tol = 2.0 / 32768.0_f32;
        for i in (0..frames * 2).step_by(2) {
            assert!(
                (out[i] - 0.25).abs() <= tol,
                "S1: L[{i}]={} not ≈ 0.25",
                out[i]
            );
            assert!(
                (out[i + 1] - (-0.25)).abs() <= tol,
                "S1: R[{i}]={} not ≈ −0.25",
                out[i + 1]
            );
        }
    }

    // -----------------------------------------------------------------------
    // S2 — is_exhausted stays false mid-stream, flips true only when drained
    //
    // Reads one frame at a time across a multi-packet stream. is_exhausted() must
    // be false on every intermediate read (even one that lands exactly on a
    // decoded-packet boundary, where leftover_pos == leftover.len() but EOF has
    // not been reached). Pins the `eof && pos >= len` conjunction (an `||` mutant
    // sets exhausted true at the first packet-boundary read) and rejects a
    // hardcoded `is_exhausted -> true`.
    // -----------------------------------------------------------------------
    #[test]
    fn s2_symphonia_source_is_exhausted_only_when_drained() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ramp_stream.wav");
        // 500 mono frames so the decode spans several Symphonia packets.
        let src: Vec<i16> = (0..500).map(|i| (i * 50) as i16).collect();
        write_wav_s16(&path, 44100, 1, &src);

        let mut source = SymphoniaSource::open(&path).expect("S2: open");
        assert!(!source.is_exhausted(), "S2: fresh source not exhausted");

        let mut total = 0usize;
        let mut one = [0.0f32; 1];
        loop {
            let n = source.read(&mut one).expect("S2: read one frame");
            if n == 0 {
                break;
            }
            total += n;
            if total < src.len() {
                assert!(
                    !source.is_exhausted(),
                    "S2: not exhausted at frame {total} of {} (mid-stream packet boundary)",
                    src.len()
                );
            }
        }
        assert_eq!(total, src.len(), "S2: read every frame exactly once");
        assert!(source.is_exhausted(), "S2: exhausted after the final frame");
    }

    // -----------------------------------------------------------------------
    // S4 — A read that ends exactly on a packet boundary is NOT exhausted
    //
    // When a read fills the caller's buffer exactly at a decoded-packet boundary
    // (leftover_pos == leftover.len()) the source still has further packets, so
    // EOF has not been seen: exhausted must stay false. This is the one state that
    // distinguishes `eof && pos >= len` from an `||` mutant — the conjunction is
    // false (eof is false), the disjunction is true (pos >= len). We size the
    // buffer to exactly one decoded packet so the read returns on that boundary
    // with eof still unset.
    // -----------------------------------------------------------------------
    #[test]
    fn s4_read_ending_on_packet_boundary_not_exhausted() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("boundary.wav");
        // Enough mono frames to span multiple decoded packets.
        let src: Vec<i16> = (0..4096).map(|i| (i % 200) as i16).collect();
        write_wav_s16(&path, 44100, 1, &src);

        // Learn the first decoded packet's sample length.
        let mut probe_src = SymphoniaSource::open(&path).expect("S4: open for probe");
        assert!(
            probe_src.fill_one_packet().expect("S4: fill"),
            "S4: a packet exists"
        );
        let first_packet_len = probe_src.leftover.len();
        assert!(first_packet_len > 0, "S4: non-empty packet");
        assert!(
            first_packet_len < src.len(),
            "S4: first packet must not be the whole stream (need a later packet to remain)"
        );

        // Fresh source: read exactly one packet's worth, landing on the boundary.
        let mut source = SymphoniaSource::open(&path).expect("S4: open");
        let mut out = vec![0.0f32; first_packet_len];
        let frames = source.read(&mut out).expect("S4: boundary read");
        assert_eq!(
            frames, first_packet_len,
            "S4: filled exactly one packet (mono)"
        );
        assert!(
            !source.is_exhausted(),
            "S4: more packets remain — must not be exhausted on a packet boundary"
        );

        // The remaining frames are still readable.
        let mut rest = vec![0.0f32; src.len()];
        let more = source.read(&mut rest).expect("S4: read remainder");
        assert_eq!(more, src.len() - first_packet_len, "S4: remainder readable");
        assert!(source.is_exhausted(), "S4: exhausted after the full stream");
    }

    // -----------------------------------------------------------------------
    // S3 — Zero-channel source reports exhausted and reads nothing
    //
    // Guards the `ch == 0` early return in read(): a degenerate source must not
    // divide by zero and must report exhaustion immediately.
    // -----------------------------------------------------------------------
    #[test]
    fn s3_symphonia_source_zero_channels() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mono.wav");
        write_wav_s16(&path, 44100, 1, &[1i16, 2, 3]);
        let mut source = SymphoniaSource::open(&path).expect("S3: open");
        // Force the zero-channel degenerate path.
        source.channels = 0;

        let mut out = [0.0f32; 8];
        let frames = source.read(&mut out).expect("S3: read");
        assert_eq!(frames, 0, "S3: zero-channel read yields no frames");
        assert!(
            source.is_exhausted(),
            "S3: zero-channel source is exhausted"
        );
    }

    // -----------------------------------------------------------------------
    // M1 — map_probe_error discriminates EOF from real I/O errors
    //
    // A too-short probe (UnexpectedEof) is "not our format" so the ffmpeg fallback
    // is tried (→ UnsupportedFormat); any other I/O error (e.g. PermissionDenied)
    // MUST surface as a real Io error, never be misclassified as unsupported.
    // Pins the `io.kind() == UnexpectedEof` match guard (a `true` mutant would
    // route a permission failure to the ffmpeg fallback). Mirrors frame_reader's
    // ME1 (conventions.md A5 — shared error-mapping test class).
    // -----------------------------------------------------------------------
    #[test]
    fn m1_map_probe_error_discriminates_io_kinds() {
        use std::io::{Error as IoError, ErrorKind};

        // UnexpectedEof → UnsupportedFormat (guard true).
        let eof = SymphoniaError::IoError(IoError::new(ErrorKind::UnexpectedEof, "eof"));
        assert!(
            matches!(
                map_probe_error(eof),
                AudioError::UnsupportedFormat { codec: None }
            ),
            "M1: UnexpectedEof → UnsupportedFormat"
        );

        // Non-EOF I/O error → Io (guard false), kind preserved.
        let denied = SymphoniaError::IoError(IoError::new(ErrorKind::PermissionDenied, "denied"));
        match map_probe_error(denied) {
            AudioError::Io(io) => {
                assert_eq!(io.kind(), ErrorKind::PermissionDenied, "M1: preserves kind");
            }
            other => panic!("M1: non-EOF I/O must map to Io, got {other:?}"),
        }

        // Unsupported / DecodeError → UnsupportedFormat.
        assert!(
            matches!(
                map_probe_error(SymphoniaError::Unsupported("codec")),
                AudioError::UnsupportedFormat { codec: None }
            ),
            "M1: Unsupported → UnsupportedFormat"
        );
        assert!(
            matches!(
                map_probe_error(SymphoniaError::DecodeError("bad")),
                AudioError::UnsupportedFormat { codec: None }
            ),
            "M1: DecodeError → UnsupportedFormat"
        );
    }

    // -----------------------------------------------------------------------
    // M2 — map_next_packet_error discriminates truncation from real I/O errors
    //
    // Mid-decode, an UnexpectedEof means the recognised file was truncated
    // mid-packet (→ DecodeFailed, not the probe stage's UnsupportedFormat), while
    // any other I/O error (PermissionDenied stands in) MUST surface as a real Io
    // error so a transient/permission failure is not misreported as corruption.
    // Pins the `io.kind() == UnexpectedEof` match guard inside decode_next_packet
    // (a `true` mutant would mislabel every I/O failure as a truncated file).
    // -----------------------------------------------------------------------
    #[test]
    fn m2_map_next_packet_error_discriminates_io_kinds() {
        use std::io::{Error as IoError, ErrorKind};

        // UnexpectedEof → DecodeFailed (truncation; guard true).
        let eof = SymphoniaError::IoError(IoError::new(ErrorKind::UnexpectedEof, "eof"));
        match map_next_packet_error(eof) {
            AudioError::DecodeFailed(msg) => {
                assert!(
                    msg.contains("truncated"),
                    "M2: EOF message mentions truncation"
                );
            }
            other => panic!("M2: UnexpectedEof must map to DecodeFailed, got {other:?}"),
        }

        // Non-EOF I/O error → Io (guard false), kind preserved.
        let denied = SymphoniaError::IoError(IoError::new(ErrorKind::PermissionDenied, "denied"));
        match map_next_packet_error(denied) {
            AudioError::Io(io) => {
                assert_eq!(io.kind(), ErrorKind::PermissionDenied, "M2: preserves kind");
            }
            other => panic!("M2: non-EOF I/O must map to Io, got {other:?}"),
        }

        // Non-I/O error → DecodeFailed.
        assert!(
            matches!(
                map_next_packet_error(SymphoniaError::DecodeError("bad")),
                AudioError::DecodeFailed(_)
            ),
            "M2: DecodeError → DecodeFailed"
        );
    }
}
