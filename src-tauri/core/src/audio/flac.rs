//! 24-bit FLAC encode and decode.

use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use flacenc::{
    bitsink::BitSink,
    component::{BitRepr, StreamInfo},
    config, encode_fixed_size_frame,
    error::{SourceError, Verify},
    source::{Context, Fill, FrameBuf, Source},
};

// Whole-buffer encode path (`encode_flac_24`) is test-only now that export streams; its
// flacenc imports ride behind `cfg(test)` so they don't show up unused in release builds.
#[cfg(test)]
use flacenc::{bitsink::ByteSink, component::Stream, source::MemSource};

use super::bit_sink::WriteBitSink;
use super::{AudioError, PcmSource};

/// Make an encoded [`Stream`] readable by our Symphonia [`FrameReader`](super::frame_reader).
///
/// flacenc lowers `STREAMINFO.min_block_size` to the (short) final frame, so an otherwise
/// fixed-block stream reports `min != max`. Symphonia then treats it as a variable-blocksize
/// stream and rejects every fixed-coded frame during resync (manifesting as `UnexpectedEof`).
/// libFLAC/ffmpeg keep `min == max` for fixed streams — the final short frame is a permitted
/// exception — so we mirror that. Must run before `stream.write`. STREAMINFO carries no CRC,
/// so rewriting these fields is safe.
#[cfg(test)]
pub(crate) fn normalize_fixed_block_size(stream: &mut Stream) -> Result<(), AudioError> {
    let max_bs = stream.stream_info().max_block_size();
    if max_bs > 0 && stream.stream_info().min_block_size() != max_bs {
        stream
            .stream_info_mut()
            .set_block_sizes(max_bs, max_bs)
            .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;
    }
    Ok(())
}

/// Encode interleaved f32 (clamped to [-1, 1]) as 24-bit integer FLAC at `out`.
///
/// **Test support only.** Production code streams via [`encode_flac_streaming`]; this whole-buffer
/// path remains as a convenient "write a FLAC from a `Vec<f32>`" helper for tests across the crate.
#[cfg(test)]
pub(crate) fn encode_flac_24(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    out: &Path,
) -> Result<(), AudioError> {
    // Scale f32 → signed 24-bit: multiply by 2^23 − 1 so ±1.0 maps to ±8 388 607.
    // Clamp first: sinc resampling can push samples past ±1.0; the FLAC format cannot represent
    // values outside the signed 24-bit range, so clamping here is the correct boundary.
    let scale = (1i32 << 23) - 1;
    let i32_samples: Vec<i32> = samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * scale as f32).round() as i32)
        .collect();

    let config = config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| AudioError::EncodeFailed(e.to_string()))?;

    let source = MemSource::from_samples(&i32_samples, channels as usize, 24, sample_rate as usize);

    let mut stream = flacenc::encode_with_fixed_block_size(&config, source, 4096)
        .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;
    normalize_fixed_block_size(&mut stream)?;

    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;

    std::fs::write(out, sink.into_inner()).map_err(AudioError::Io)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming encode (pull-based; O(one block) peak memory)
// ---------------------------------------------------------------------------

/// FLAC magic + STREAMINFO block header occupy 8 bytes before the STREAMINFO body.
const STREAMINFO_BODY_OFFSET: u64 = 8;

/// Encode a [`PcmSource`] to a 24-bit FLAC at `out`, streaming so peak memory is O(one encoded
/// block) rather than O(stream length).
///
/// Pulls f32 frames from `src` (clamped to `[-1, 1]`, scaled to signed 24-bit on the fly via
/// [`FlacPullSource`]); each encoded [`Frame`](flacenc::component::Frame) is written and dropped
/// immediately. STREAMINFO is written as a placeholder and back-patched (total samples + MD5)
/// after the stream is fully encoded. Returns the frame count written. Shared by the import
/// transcode ([`cache::ensure_resampled`](super::cache)) and audio export
/// ([`export`](super::export)).
///
/// On error, any partially-written output remains; cleanup is the caller's responsibility.
pub(crate) fn encode_flac_streaming(src: impl PcmSource, out: &Path) -> Result<i64, AudioError> {
    let channels = src.channels() as usize;
    let sample_rate = src.sample_rate();
    if channels == 0 {
        return Ok(0);
    }
    let mut flac_src = FlacPullSource::new(src);

    let cfg = config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| AudioError::EncodeFailed(e.to_string()))?;

    // STREAMINFO: min == max == 4096 (the final short frame is a permitted FLAC exception;
    // decoders use min == max to identify fixed-blocksize streams).
    let mut stream_info = StreamInfo::new(sample_rate as usize, channels, 24)
        .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;
    stream_info
        .set_block_sizes(4096, 4096)
        .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;

    // Open output for write + seek (need to backpatch STREAMINFO after encoding).
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(out)
        .map_err(AudioError::Io)?;
    let mut sink = WriteBitSink::new(BufWriter::new(file));

    // FLAC magic (4 bytes) + STREAMINFO metadata block header (4 bytes: type=0, last=1, len=34).
    sink.write_bytes_aligned(&[0x66, 0x4c, 0x61, 0x43])
        .map_err(AudioError::Io)?; // fLaC
    sink.write_bytes_aligned(&[0x80, 0x00, 0x00, 0x22])
        .map_err(AudioError::Io)?; // last block, type 0, length 34
    stream_info
        .write(&mut sink)
        .map_err(|e| AudioError::EncodeFailed(e.to_string()))?; // placeholder; backpatched below

    // Reusable frame buffer and MD5/frame-count context (both updated via Fill on each read).
    let mut fbc = (
        FrameBuf::with_size(channels, 4096).map_err(|e| AudioError::EncodeFailed(e.to_string()))?,
        Context::new(24, channels),
    );

    // Encode and stream: read → encode → write → drop, one block at a time.
    loop {
        let n = flac_src.read_samples(4096, &mut fbc).map_err(|_| {
            flac_src
                .error
                .take()
                .unwrap_or_else(|| AudioError::EncodeFailed("source read failed".into()))
        })?;
        if n == 0 {
            break;
        }
        let frame_number = fbc.1.current_frame_number().ok_or_else(|| {
            AudioError::EncodeFailed("encoder context missing frame number after fill".into())
        })?;
        let frame = encode_fixed_size_frame(&cfg, &fbc.0, frame_number, &stream_info)
            .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;
        frame
            .write(&mut sink)
            .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;
        // `frame` is dropped here, freeing its residual Vecs immediately.
    }

    // Align (no-op: FLAC frames are byte-aligned) and flush the BufWriter to disk.
    sink.align_to_byte().map_err(AudioError::Io)?;
    let mut buf_writer = sink.into_inner();
    buf_writer.flush().map_err(AudioError::Io)?;

    // Backpatch STREAMINFO with total_samples and MD5 (determined only after full encoding).
    stream_info.set_total_samples(fbc.1.total_samples());
    stream_info.set_md5_digest(&fbc.1.md5_digest());

    buf_writer
        .seek(SeekFrom::Start(STREAMINFO_BODY_OFFSET))
        .map_err(AudioError::Io)?;
    {
        let mut patch = WriteBitSink::new(&mut buf_writer);
        stream_info
            .write(&mut patch)
            .map_err(|e| AudioError::EncodeFailed(e.to_string()))?;
        // STREAMINFO is 272 bits = 34 bytes (byte-aligned); into_inner debug-asserts bits_filled==0.
        let _ = patch.into_inner();
    }
    buf_writer.flush().map_err(AudioError::Io)?;

    Ok(fbc.1.total_samples() as i64)
}

/// flacenc [`Source`] adapter: pulls f32 frames from a [`PcmSource`], clamps to `[-1, 1]`,
/// and scales to signed 24-bit on the fly. Stashes the first read error (flacenc's `SourceError`
/// can't carry our typed error through the encoder).
struct FlacPullSource<P: PcmSource> {
    inner: P,
    channels: usize,
    sample_rate: u32,
    f32_buf: Vec<f32>,
    i32_buf: Vec<i32>,
    error: Option<AudioError>,
}

/// Scale factor for f32 → signed 24-bit: ±1.0 maps to ±(2^23 − 1). Matches [`encode_flac_24`].
const I24_SCALE: f32 = ((1i32 << 23) - 1) as f32;

impl<P: PcmSource> FlacPullSource<P> {
    fn new(inner: P) -> Self {
        let channels = inner.channels() as usize;
        let sample_rate = inner.sample_rate();
        Self {
            inner,
            channels,
            sample_rate,
            f32_buf: Vec::new(),
            i32_buf: Vec::new(),
            error: None,
        }
    }
}

impl<P: PcmSource> Source for FlacPullSource<P> {
    fn channels(&self) -> usize {
        self.channels.max(1)
    }

    fn bits_per_sample(&self) -> usize {
        24
    }

    fn sample_rate(&self) -> usize {
        self.sample_rate.max(1) as usize
    }

    fn read_samples<F: Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> Result<usize, SourceError> {
        let ch = self.channels;
        if ch == 0 {
            return Ok(0);
        }
        let cap = block_size * ch;
        if self.f32_buf.len() < cap {
            self.f32_buf.resize(cap, 0.0);
        }

        let frames = match self.inner.read(&mut self.f32_buf[..cap]) {
            Ok(n) => n,
            Err(e) => {
                self.error = Some(e);
                return Err(SourceError::from_unknown());
            }
        };
        if frames == 0 {
            return Ok(0);
        }

        let n = frames * ch;
        self.i32_buf.clear();
        self.i32_buf.extend(
            self.f32_buf[..n]
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * I24_SCALE).round() as i32),
        );
        dest.fill_interleaved(&self.i32_buf)
            .map_err(|_| SourceError::from_unknown())?;
        Ok(frames)
    }
}

/// Decode a FLAC file to interleaved f32 (delegates to the Symphonia path).
///
/// **Test support only.** Production code uses [`SymphoniaFrameReader`](super::frame_reader) or
/// [`probe`](super::decode::probe) + [`count_frames`](super::frame_reader::count_frames).
#[cfg(test)]
pub(crate) fn decode_flac(path: &Path) -> Result<super::DecodedAudio, AudioError> {
    super::decode::decode(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use tempfile::TempDir;

    use crate::audio::decode::probe;
    use crate::audio::frame_reader::{FrameReader, SymphoniaFrameReader};
    use crate::audio::BufferedSource;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sine_f32(freq: f32, sample_rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate as f32).sin())
            .collect()
    }

    fn round_trip(samples: &[f32], sample_rate: u32, channels: u16) -> (Vec<f32>, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.flac");
        encode_flac_24(samples, sample_rate, channels, &path).unwrap();
        let decoded = decode_flac(&path).unwrap();
        (decoded.samples, dir)
    }

    // -----------------------------------------------------------------------
    // C12 — 24-bit round-trip bound (max abs error ≤ 2^-23 ≈ 1.2e-7)
    // -----------------------------------------------------------------------

    #[test]
    fn c12_round_trip_bound() {
        let samples = sine_f32(440.0, 48000, 4800);
        let (decoded, _dir) = round_trip(&samples, 48000, 1);
        assert_eq!(decoded.len(), samples.len(), "C12: length mismatch");
        let max_err = samples
            .iter()
            .zip(decoded.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        let bound = 1.0_f32 / (1 << 23) as f32; // ≈ 1.19e-7
        assert!(
            max_err <= bound * 2.0, // tiny slack for f32 rounding
            "C12: max abs error {max_err} exceeds 24-bit quantisation bound {bound}"
        );
    }

    // -----------------------------------------------------------------------
    // C13 — Full-scale endpoints round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn c13_full_scale_endpoints() {
        let samples = vec![1.0_f32, -1.0, 0.5, -0.5, 0.0];
        let (decoded, _dir) = round_trip(&samples, 48000, 1);
        let bound = 1.0_f32 / (1 << 23) as f32;
        for (i, (&a, &b)) in samples.iter().zip(decoded.iter()).enumerate() {
            assert!(
                (a - b).abs() <= bound * 2.0,
                "C13: sample[{i}]: expected ≈{a}, got {b}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // C14 — Overshoot is clamped at encode; no panic
    // -----------------------------------------------------------------------

    #[test]
    fn c14_overshoot_clamped() {
        // Samples deliberately outside [-1, 1]
        let samples = vec![1.5_f32, -1.5, 2.0, -2.0];
        let (decoded, _dir) = round_trip(&samples, 48000, 1);
        for (i, &s) in decoded.iter().enumerate() {
            assert!(
                (-1.0_f32..=1.0).contains(&s),
                "C14: decoded sample[{i}] = {s} out of [-1, 1] after clamping"
            );
            assert!(
                (s.abs() - 1.0).abs() < 1e-4,
                "C14: decoded sample[{i}] = {s} should be ≈ ±1.0 (clamped)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // C15 — Stereo 48 kHz preserves channels, rate, and interleave order
    // -----------------------------------------------------------------------

    #[test]
    fn c15_stereo_metadata_and_interleave() {
        let frames = 480;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let l = (2.0 * PI * 440.0 * i as f32 / 48000.0).sin();
            let r = (2.0 * PI * 880.0 * i as f32 / 48000.0).sin();
            samples.push(l);
            samples.push(r);
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stereo.flac");
        encode_flac_24(&samples, 48000, 2, &path).unwrap();

        let decoded = decode_flac(&path).unwrap();
        assert_eq!(decoded.channels, 2, "C15: channels");
        assert_eq!(decoded.sample_rate, 48000, "C15: sample rate");
        assert_eq!(decoded.frames(), frames, "C15: frame count");

        // Verify interleave order: L and R channels remain distinct.
        let bound = 1.0_f32 / (1 << 23) as f32;
        for f in 0..frames {
            let orig_l = samples[f * 2];
            let orig_r = samples[f * 2 + 1];
            let dec_l = decoded.samples[f * 2];
            let dec_r = decoded.samples[f * 2 + 1];
            assert!(
                (orig_l - dec_l).abs() <= bound * 2.0,
                "C15: L[{f}] {orig_l} → {dec_l}"
            );
            assert!(
                (orig_r - dec_r).abs() <= bound * 2.0,
                "C15: R[{f}] {orig_r} → {dec_r}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // C16 — Cache file probes as FLAC with correct metadata
    // -----------------------------------------------------------------------

    #[test]
    fn c16_probes_as_flac() {
        let samples = sine_f32(440.0, 48000, 480);
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe.flac");
        encode_flac_24(&samples, 48000, 1, &path).unwrap();

        let p = probe(&path).unwrap();
        assert_eq!(p.codec, "flac", "C16: codec");
        assert_eq!(p.sample_rate, 48000, "C16: sample_rate");
        assert_eq!(p.channels, 1, "C16: channels");
        assert_eq!(p.length_frames, Some(480), "C16: length_frames");
    }

    // -----------------------------------------------------------------------
    // C17 — Within-build determinism: same input → byte-identical FLAC
    // -----------------------------------------------------------------------

    #[test]
    fn c17_within_build_determinism() {
        let samples = sine_f32(440.0, 48000, 480);
        let dir = TempDir::new().unwrap();

        let path_a = dir.path().join("a.flac");
        let path_b = dir.path().join("b.flac");
        encode_flac_24(&samples, 48000, 1, &path_a).unwrap();
        encode_flac_24(&samples, 48000, 1, &path_b).unwrap();

        let bytes_a = std::fs::read(&path_a).unwrap();
        let bytes_b = std::fs::read(&path_b).unwrap();
        assert_eq!(bytes_a, bytes_b, "C17: FLAC bytes are not identical");
    }

    // -----------------------------------------------------------------------
    // C18 — Decoded frame count == encoded frame count (no gapless trim)
    // -----------------------------------------------------------------------

    #[test]
    fn c18_no_length_surprise() {
        let frames = 12345usize;
        let samples = sine_f32(440.0, 48000, frames);
        let (decoded, _dir) = round_trip(&samples, 48000, 1);
        assert_eq!(
            decoded.len(),
            frames,
            "C18: decoded length != encoded length"
        );
    }

    // -----------------------------------------------------------------------
    // Streaming encode (encode_flac_streaming) — pull-based, O(one block) memory
    // -----------------------------------------------------------------------

    fn buffered(samples: Vec<f32>, channels: u16, rate: u32) -> BufferedSource {
        BufferedSource::new(samples, channels, rate)
    }

    fn interleave_stereo(l_freq: f32, r_freq: f32, rate: u32, frames: usize) -> Vec<f32> {
        let mut s = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            s.push((2.0 * PI * l_freq * i as f32 / rate as f32).sin());
            s.push((2.0 * PI * r_freq * i as f32 / rate as f32).sin());
        }
        s
    }

    // SW1 — round-trip within the 24-bit bound; length not a 4096 multiple → final short frame.
    #[test]
    fn sw1_stream_round_trip_bound() {
        let samples = sine_f32(440.0, 48000, 5000);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        let frames = encode_flac_streaming(buffered(samples.clone(), 1, 48000), &out).unwrap();
        assert_eq!(frames, 5000, "SW1: returned frame count");
        let decoded = decode_flac(&out).unwrap();
        assert_eq!(decoded.frames(), 5000, "SW1: decoded frame count");
        let bound = 2.0 / (1 << 23) as f32;
        let max_err = samples
            .iter()
            .zip(decoded.samples.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err <= bound, "SW1: max error {max_err}");
    }

    // SW2 — byte-determinism (stereo): two encodes of the same input are identical.
    #[test]
    fn sw2_stream_determinism() {
        let samples = interleave_stereo(440.0, 660.0, 48000, 9001);
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.flac");
        let b = dir.path().join("b.flac");
        encode_flac_streaming(buffered(samples.clone(), 2, 48000), &a).unwrap();
        encode_flac_streaming(buffered(samples, 2, 48000), &b).unwrap();
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "SW2: byte-identical"
        );
    }

    // SW3 — STREAMINFO: backpatched length + min == max block size (4096) in the raw bytes.
    #[test]
    fn sw3_stream_streaminfo() {
        let samples = sine_f32(440.0, 48000, 5000);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        encode_flac_streaming(buffered(samples, 1, 48000), &out).unwrap();
        let p = probe(&out).unwrap();
        assert_eq!(
            p.length_frames,
            Some(5000),
            "SW3: backpatched total_samples"
        );
        let bytes = std::fs::read(&out).unwrap();
        let min_bs = u16::from_be_bytes([bytes[8], bytes[9]]);
        let max_bs = u16::from_be_bytes([bytes[10], bytes[11]]);
        assert_eq!(min_bs, max_bs, "SW3: min == max block size");
        assert_eq!(min_bs, 4096, "SW3: block size 4096");
    }

    // SW4 — seek accuracy: SymphoniaFrameReader reads correct samples at block seams + short frame.
    #[test]
    fn sw4_stream_seek_accuracy() {
        let samples = sine_f32(440.0, 48000, 9001);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        encode_flac_streaming(buffered(samples, 1, 48000), &out).unwrap();

        let ref_dec = decode_flac(&out).unwrap();
        let mut reader = SymphoniaFrameReader::open(&out).unwrap();
        let bound = 1e-6f32;

        let seam = reader.read_range(4096, 100).unwrap();
        assert_eq!(seam.len(), 100, "SW4: seam read count");
        let seam_err = seam
            .iter()
            .zip(ref_dec.samples[4096..4196].iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(seam_err <= bound, "SW4: seam mismatch {seam_err}");

        let tail = reader.read_range(8192, 9001 - 8192 + 10).unwrap();
        assert_eq!(tail.len(), 9001 - 8192, "SW4: tail truncates at EOS");
    }

    // SW6 — empty source: Ok(0), output is a valid 0-frame FLAC.
    #[test]
    fn sw6_stream_empty_source() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        let frames = encode_flac_streaming(buffered(vec![], 1, 48000), &out).unwrap();
        assert_eq!(frames, 0, "SW6: 0 frames");
        assert!(out.exists(), "SW6: file created");
        assert_eq!(
            decode_flac(&out).unwrap().frames(),
            0,
            "SW6: decoded 0 frames"
        );
    }

    // SW7 — single sub-block (< 4096 frames): one short frame, faithful round-trip.
    #[test]
    fn sw7_stream_single_short_block() {
        let samples = sine_f32(440.0, 48000, 100);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        let frames = encode_flac_streaming(buffered(samples.clone(), 1, 48000), &out).unwrap();
        assert_eq!(frames, 100, "SW7: frame count");
        let decoded = decode_flac(&out).unwrap();
        let bound = 2.0 / (1 << 23) as f32;
        let max_err = samples
            .iter()
            .zip(decoded.samples.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err <= bound, "SW7: round-trip error {max_err}");
    }

    // SW10 — bad output path → AudioError (Io or EncodeFailed), not a panic.
    #[test]
    fn sw10_stream_bad_output_path() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("no_such_dir").join("out.flac");
        let err = encode_flac_streaming(buffered(sine_f32(440.0, 48000, 480), 1, 48000), &out)
            .unwrap_err();
        assert!(
            matches!(err, AudioError::Io(_) | AudioError::EncodeFailed(_)),
            "SW10: got {err:?}"
        );
    }

    // SW11 — STREAMINFO MD5 over the decoded 24-bit PCM matches the backpatched digest.
    #[test]
    fn sw11_stream_streaminfo_md5() {
        use md5::{Digest, Md5};
        let samples = interleave_stereo(440.0, 660.0, 48000, 9001);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        encode_flac_streaming(buffered(samples, 2, 48000), &out).unwrap();

        let decoded = decode_flac(&out).unwrap();
        let mut hasher = Md5::new();
        for &s in &decoded.samples {
            let v = (s * (1i32 << 23) as f32).round() as i32;
            hasher.update(&v.to_le_bytes()[0..3]);
        }
        let digest = hasher.finalize();
        let file = std::fs::read(&out).unwrap();
        assert_eq!(
            &file[26..42],
            digest.as_slice(),
            "SW11: STREAMINFO MD5 must equal MD5 of decoded 24-bit LE PCM"
        );
    }

    // -----------------------------------------------------------------------
    // FlacPullSource direct trait tests
    //
    // `encode_flac_streaming` calls `FlacPullSource::read_samples` directly but never the
    // `Source` accessor methods (it derives channels/rate from the `PcmSource` and hardcodes
    // 24-bit in its own `StreamInfo`). The accessors are still part of the `Source` contract a
    // flacenc-API consumer would read, so we pin them — and the read-side interleave/scale math —
    // by driving the adapter directly. A minimal `Fill` collector captures the interleaved i32.
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct CollectFill {
        interleaved: Vec<i32>,
    }
    impl Fill for CollectFill {
        fn fill_interleaved(&mut self, interleaved: &[i32]) -> Result<(), SourceError> {
            self.interleaved.extend_from_slice(interleaved);
            Ok(())
        }
        fn fill_le_bytes(&mut self, _bytes: &[u8], _bps: usize) -> Result<(), SourceError> {
            unreachable!("FlacPullSource fills via fill_interleaved")
        }
    }

    // SW13 — Source accessors report the encoder's contract values (channels, 24 bps, rate),
    // and the `.max(1)` guards floor a zero-channel / zero-rate source at 1.
    #[test]
    fn sw13_pullsource_trait_accessors() {
        let src = FlacPullSource::new(buffered(vec![0.0; 8], 2, 44_100));
        assert_eq!(src.channels(), 2, "SW13: channels");
        assert_eq!(src.bits_per_sample(), 24, "SW13: bits_per_sample is 24");
        assert_eq!(src.sample_rate(), 44_100, "SW13: sample_rate");

        // Degenerate source: channels/rate floored at 1 by the `.max(1)` guards.
        let degen = FlacPullSource::new(buffered(vec![], 0, 0));
        assert_eq!(degen.channels(), 1, "SW13: zero channels floored to 1");
        assert_eq!(degen.sample_rate(), 1, "SW13: zero rate floored to 1");
    }

    // SW14 — read_samples interleave + scale: a stereo block yields `frames * channels` i32s in
    // interleaved order, each f32 scaled by 2^23 − 1 and rounded. Pins `cap = block_size * ch`
    // (a `*`→`/` mutation makes `cap = block_size / ch`, halving capacity for stereo so a single
    // read short-reads at 2048 frames instead of the full 4096) and the f32→i24 conversion.
    #[test]
    fn sw14_pullsource_read_samples_interleave() {
        // 4096 stereo frames (= one full block). With cap = block_size * ch = 8192 the whole block
        // is read in one call; the `*`→`/` mutant computes cap = 2048, capping the read at 1024
        // frames — so the returned frame count distinguishes the two. Distinct per-channel values
        // (L positive, R negative) also make an interleave swap observable.
        let n = 4096;
        let mut frames_in = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = i as f32 / n as f32; // 0.0 .. <1.0
            frames_in.push(v); // L
            frames_in.push(-v); // R
        }
        let mut src = FlacPullSource::new(buffered(frames_in.clone(), 2, 48_000));
        let mut dest = CollectFill::default();
        let frames = src.read_samples(4096, &mut dest).unwrap();

        assert_eq!(frames, 4096, "SW14: full 4096-frame block read in one call");
        assert_eq!(
            dest.interleaved.len(),
            n * 2,
            "SW14: frames * channels interleaved samples (cap = block_size * channels)"
        );
        let scale = ((1i32 << 23) - 1) as f32;
        let expect: Vec<i32> = frames_in
            .iter()
            .map(|&s| (s * scale).round() as i32)
            .collect();
        assert_eq!(dest.interleaved, expect, "SW14: interleaved 24-bit values");
    }

    // SW15 — repeated reads at the same block size reuse the f32 buffer without corrupting output
    // (exercises the `f32_buf.len() < cap` resize guard on the already-sized second call).
    #[test]
    fn sw15_pullsource_read_samples_repeated() {
        // 4097 mono frames > 4096 → two reads: a full block then a 1-frame block.
        let samples: Vec<f32> = (0..4097).map(|i| (i as f32 / 4097.0) - 0.5).collect();
        let mut src = FlacPullSource::new(buffered(samples.clone(), 1, 48_000));
        let scale = ((1i32 << 23) - 1) as f32;

        let mut first = CollectFill::default();
        assert_eq!(
            src.read_samples(4096, &mut first).unwrap(),
            4096,
            "SW15: first block"
        );
        assert_eq!(first.interleaved.len(), 4096, "SW15: first block size");

        let mut second = CollectFill::default();
        assert_eq!(
            src.read_samples(4096, &mut second).unwrap(),
            1,
            "SW15: short final block"
        );
        assert_eq!(
            second.interleaved,
            vec![(samples[4096] * scale).round() as i32],
            "SW15: final sample value after buffer reuse"
        );

        let mut eof = CollectFill::default();
        assert_eq!(
            src.read_samples(4096, &mut eof).unwrap(),
            0,
            "SW15: EOF returns 0"
        );
    }

    // SW12 — Stereo streaming round-trip: channels preserved and both interleaved channels decode
    // within the 24-bit bound across a block boundary (replaces the former whole-buffer
    // `a1_flac_sink_round_trip`, which the unified pull-based encoder superseded).
    #[test]
    fn sw12_stream_stereo_round_trip() {
        let frames = 5000; // > 4096 → crosses a block boundary with a short final frame
        let samples = interleave_stereo(440.0, 660.0, 48000, frames);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.flac");
        let written = encode_flac_streaming(buffered(samples.clone(), 2, 48000), &out).unwrap();
        assert_eq!(written, frames as i64, "SW12: returned frame count");

        let decoded = decode_flac(&out).unwrap();
        assert_eq!(decoded.channels, 2, "SW12: stereo preserved");
        assert_eq!(decoded.frames(), frames, "SW12: decoded frame count");
        assert_eq!(
            decoded.samples.len(),
            samples.len(),
            "SW12: interleaved sample count"
        );
        let bound = 2.0 / (1 << 23) as f32;
        let max_err = samples
            .iter()
            .zip(decoded.samples.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err <= bound, "SW12: max round-trip error {max_err}");
    }
}
