//! Native f32le WAV streaming encode.
//!
//! f32 WAV is a lossless, bit-exact container — the export round-trip test asserts the decoded
//! PCM equals the renderer output sample-for-sample. The encoder pulls interleaved f32 frames
//! from a [`PcmSource`] and writes them straight to disk (O(one chunk) peak memory), backpatching
//! the RIFF/data chunk sizes once the stream length is known.

use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use super::{AudioError, PcmSource};

/// WAV RIFF+fmt+data header length in bytes (44); chunk sizes are backpatched at finalize.
const WAV_HEADER_LEN: usize = 44;

/// Frames pulled from `src` per write chunk.
const WAV_CHUNK_FRAMES: usize = 4096;

/// Encode `src` to a native f32le WAV at `out`, streaming frames directly to disk.
///
/// Writes a placeholder RIFF/fmt/data header, streams interleaved f32 frames pulled from `src`,
/// then seeks back and patches the RIFF-chunk (offset 4) and data-chunk (offset 40) sizes. The
/// channel count and sample rate come from `src`. On any failure the partial `out` is removed.
pub(crate) fn encode_wav_streaming(mut src: impl PcmSource, out: &Path) -> Result<(), AudioError> {
    let channels = src.channels();
    let sample_rate = src.sample_rate();
    match encode_inner(&mut src, channels, sample_rate, out) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(out); // remove partial file; ignore NotFound
            Err(e)
        }
    }
}

fn encode_inner(
    src: &mut impl PcmSource,
    channels: u16,
    sample_rate: u32,
    out: &Path,
) -> Result<(), AudioError> {
    let mut file = std::fs::File::create(out).map_err(AudioError::Io)?;
    write_header_placeholder(&mut file, channels, sample_rate).map_err(AudioError::Io)?;
    let mut writer = BufWriter::new(file);

    let ch = channels.max(1) as usize;
    let mut buf = vec![0.0f32; WAV_CHUNK_FRAMES * ch];
    let mut samples_written: u64 = 0;
    loop {
        let frames = src.read(&mut buf)?;
        // mutants: `== 0` -> `!= 0` here is a timeout artifact, not a missed mutant — flipping
        // the loop-exit predicate makes an exhausted source (0-frame read) loop forever, so any
        // empty/finite run spins instead of producing observably-wrong output.
        if frames == 0 {
            break;
        }
        let n = frames * ch;
        let bytes: Vec<u8> = buf[..n].iter().flat_map(|s| s.to_le_bytes()).collect();
        writer.write_all(&bytes).map_err(AudioError::Io)?;
        samples_written += n as u64;
    }

    // Recover the File for seeking and backpatch the chunk sizes.
    let mut file = writer
        .into_inner()
        .map_err(|e| AudioError::Io(e.into_error()))?;
    let data_bytes = samples_written * 4; // f32 = 4 bytes/sample
    let riff_size = (36u64 + data_bytes) as u32; // "WAVE"(4) + fmt chunk(24) + data header(8) + data
    let data_size = data_bytes as u32;

    file.seek(SeekFrom::Start(4)).map_err(AudioError::Io)?;
    file.write_all(&riff_size.to_le_bytes())
        .map_err(AudioError::Io)?;
    file.seek(SeekFrom::Start(40)).map_err(AudioError::Io)?;
    file.write_all(&data_size.to_le_bytes())
        .map_err(AudioError::Io)?;
    file.flush().map_err(AudioError::Io)
}

/// Write the 44-byte WAV RIFF/fmt/data header with zero placeholder chunk sizes.
fn write_header_placeholder(
    file: &mut std::fs::File,
    channels: u16,
    sample_rate: u32,
) -> std::io::Result<()> {
    let byte_rate = sample_rate * channels as u32 * 4;
    let block_align = channels * 4;

    let mut hdr = [0u8; WAV_HEADER_LEN];
    hdr[0..4].copy_from_slice(b"RIFF");
    // [4..8]: RIFF chunk size — zero placeholder, backpatched at finalize.
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk data size
    hdr[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE_FLOAT
    hdr[22..24].copy_from_slice(&channels.to_le_bytes());
    hdr[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    hdr[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    hdr[32..34].copy_from_slice(&block_align.to_le_bytes());
    hdr[34..36].copy_from_slice(&32u16.to_le_bytes()); // bits per sample
    hdr[36..40].copy_from_slice(b"data");
    // [40..44]: data chunk size — zero placeholder, backpatched at finalize.
    file.write_all(&hdr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decode::{decode, probe};
    use crate::audio::BufferedSource;
    use tempfile::TempDir;

    fn buffered(samples: Vec<f32>, channels: u16, rate: u32) -> BufferedSource {
        BufferedSource::new(samples, channels, rate)
    }

    fn decode_audio(path: &Path) -> (Vec<f32>, u32, u16) {
        let d = decode(path).unwrap();
        (d.samples, d.sample_rate, d.channels)
    }

    // Round-trip is bit-exact for stereo (f32 WAV is lossless).
    #[test]
    fn wav_round_trip_stereo_exact() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let samples: Vec<f32> = (0..200).map(|i| (i as f32) / 200.0 - 0.5).collect();
        encode_wav_streaming(buffered(samples.clone(), 2, 48000), &out).unwrap();

        let (decoded, rate, ch) = decode_audio(&out);
        assert_eq!(rate, 48000, "stereo: sample rate");
        assert_eq!(ch, 2, "stereo: channels");
        assert_eq!(decoded, samples, "stereo: bit-exact");
    }

    // Round-trip is bit-exact for mono.
    #[test]
    fn wav_round_trip_mono_exact() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let samples: Vec<f32> = (0..100).map(|i| (i as f32) / 50.0 - 1.0).collect();
        encode_wav_streaming(buffered(samples.clone(), 1, 44100), &out).unwrap();

        let (decoded, rate, ch) = decode_audio(&out);
        assert_eq!(rate, 44100, "mono: sample rate");
        assert_eq!(ch, 1, "mono: channels");
        assert_eq!(decoded, samples, "mono: bit-exact");
    }

    // Round-trip across many internal chunk pulls: a stream longer than WAV_CHUNK_FRAMES forces
    // the encoder's write loop to iterate, exercising chunk stitching + size backpatching (the
    // path the former `wav_sink_multi_chunk_equals_single` guarded under the old sink API).
    #[test]
    fn wav_round_trip_multi_chunk_exact() {
        let frames = WAV_CHUNK_FRAMES * 2 + 1808; // > 2 full chunks + a short tail
        let samples: Vec<f32> = (0..frames * 2) // stereo: interleaved L/R
            .map(|i| (i as f32 * 0.0001).sin() * 0.5)
            .collect();
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        encode_wav_streaming(buffered(samples.clone(), 2, 48000), &out).unwrap();

        let (decoded, rate, ch) = decode_audio(&out);
        assert_eq!(rate, 48000, "multi-chunk: sample rate");
        assert_eq!(ch, 2, "multi-chunk: channels");
        assert_eq!(decoded.len(), samples.len(), "multi-chunk: sample count");
        assert_eq!(
            decoded, samples,
            "multi-chunk: bit-exact across chunk seams"
        );
    }

    // Empty source → valid header-only WAV, no panic.
    #[test]
    fn wav_empty_source() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("empty.wav");
        encode_wav_streaming(buffered(vec![], 2, 48000), &out).unwrap();

        let meta = probe(&out).unwrap();
        assert_eq!(meta.sample_rate, 48000, "empty: sample rate");
        assert_eq!(meta.channels, 2, "empty: channels");
        if let Some(n) = meta.length_frames {
            assert_eq!(n, 0, "empty: zero frames");
        }
    }

    // Same input → byte-identical WAV (determinism).
    #[test]
    fn wav_determinism() {
        let dir = TempDir::new().unwrap();
        let samples: Vec<f32> = (0..200).map(|i| i as f32 * 0.005).collect();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        encode_wav_streaming(buffered(samples.clone(), 2, 48000), &a).unwrap();
        encode_wav_streaming(buffered(samples, 2, 48000), &b).unwrap();
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "WAV bytes identical for identical input"
        );
    }

    // The on-disk header fields are exactly the values the WAV spec mandates for the given
    // channel count / sample rate, and the backpatched chunk sizes match the streamed payload.
    // `decode`/`probe` recover rate + channels but ignore byte_rate / block_align / the RIFF
    // size, so this asserts the raw bytes directly to pin those derived header fields.
    #[test]
    fn wav_header_fields_exact() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("hdr.wav");
        let channels: u16 = 2;
        let rate: u32 = 48000;
        let frames = 37usize; // arbitrary non-chunk-aligned length
        let samples: Vec<f32> = (0..frames * channels as usize)
            .map(|i| i as f32 * 0.001)
            .collect();
        encode_wav_streaming(buffered(samples, channels, rate), &out).unwrap();

        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF", "RIFF tag");
        assert_eq!(&bytes[8..12], b"WAVE", "WAVE tag");
        assert_eq!(&bytes[12..16], b"fmt ", "fmt tag");
        assert_eq!(&bytes[36..40], b"data", "data tag");

        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(bytes[o..o + 2].try_into().unwrap());

        assert_eq!(u16_at(20), 3, "format = IEEE_FLOAT");
        assert_eq!(u16_at(22), channels, "fmt channels");
        assert_eq!(u32_at(24), rate, "fmt sample rate");
        // byte_rate = sample_rate * channels * bytes_per_sample (line 83).
        assert_eq!(u32_at(28), rate * channels as u32 * 4, "byte rate");
        // block_align = channels * bytes_per_sample (line 84).
        assert_eq!(u16_at(32), channels * 4, "block align");
        assert_eq!(u16_at(34), 32, "bits per sample");

        // Backpatched chunk sizes (line 64-66): data size = payload bytes, RIFF size = 36 + data.
        let data_bytes = (frames * channels as usize * 4) as u32;
        assert_eq!(u32_at(40), data_bytes, "data chunk size");
        assert_eq!(u32_at(4), 36 + data_bytes, "RIFF chunk size");
        assert_eq!(
            bytes.len() as u32,
            WAV_HEADER_LEN as u32 + data_bytes,
            "total file length = header + payload"
        );
    }

    // Bad output path → Io error, partial file removed.
    #[test]
    fn wav_bad_output_path() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("no_such_dir").join("out.wav");
        let err = encode_wav_streaming(buffered(vec![0.5f32; 100], 1, 48000), &out).unwrap_err();
        assert!(matches!(err, AudioError::Io(_)), "expected Io error");
        assert!(!out.exists(), "no partial file left behind");
    }

    /// A [`PcmSource`] that serves one full chunk, then fails on the next `read` — exercises the
    /// encoder's cleanup path *after* the header (and a data chunk) are already on disk.
    struct FailMidStream {
        served: bool,
    }

    impl PcmSource for FailMidStream {
        fn channels(&self) -> u16 {
            1
        }
        fn sample_rate(&self) -> u32 {
            48000
        }
        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            if self.served {
                return Err(AudioError::Io(std::io::Error::other(
                    "injected read failure",
                )));
            }
            self.served = true;
            out.fill(0.25);
            Ok(out.len())
        }
        fn is_exhausted(&self) -> bool {
            false
        }
    }

    // Mid-stream source failure (header + first chunk already written) → no partial file.
    #[test]
    fn wav_mid_stream_failure_no_partial() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out.wav");
        let err = encode_wav_streaming(FailMidStream { served: false }, &out).unwrap_err();
        assert!(matches!(err, AudioError::Io(_)), "expected Io error");
        assert!(
            !out.exists(),
            "partial file (header + chunk) must be removed"
        );
    }
}
