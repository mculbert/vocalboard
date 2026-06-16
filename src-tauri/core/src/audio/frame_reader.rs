//! Sequential / seekable frame readers over project-rate PCM.
//!
//! [`FrameReader`] is the read-side counterpart to [`PcmSource`](super::PcmSource): it pulls
//! interleaved f32 frames at the project rate/channels and can seek to an arbitrary frame for
//! targeted reads. Room-tone detection uses it (pass 1 sequential, pass 2 seeked), and the
//! renderer/export use it for `SpliceKind::Source` seek+discard reads.

use std::path::Path;

use symphonia::core::{
    codecs::audio::AudioDecoder,
    errors::Error as SymphoniaError,
    formats::{FormatReader, SeekMode, SeekTo},
    units::Timestamp,
};

use super::AudioError;

/// A seekable, pull-based source of interleaved f32 PCM at the project rate/channels.
///
/// `read_frames` is **greedy**: it fills `out` completely (up to its frame capacity) unless the
/// stream is exhausted, so a short fill unambiguously means "no more frames after these"
/// (mirroring [`PcmSource`](super::PcmSource)). After [`seek_to_frame`](FrameReader::seek_to_frame)
/// the next read yields frames starting exactly at the requested frame.
pub trait FrameReader {
    /// Channel count of the produced interleaved frames.
    fn channels(&self) -> u16;
    /// Sample rate (Hz) of the produced frames.
    fn sample_rate(&self) -> u32;
    /// Fill `out` (length a multiple of `channels`) with up to `out.len() / channels` frames,
    /// returning the number of **frames** written. Fills completely unless exhausted.
    fn read_frames(&mut self, out: &mut [f32]) -> Result<usize, AudioError>;
    /// Position the reader so the next [`read_frames`](FrameReader::read_frames) yields the frame
    /// at `frame` (clamped to `[0, len]`). Negative values clamp to 0.
    fn seek_to_frame(&mut self, frame: i64) -> Result<(), AudioError>;

    /// Seek to `start` and read exactly `n_frames` frames into a fresh interleaved buffer.
    ///
    /// The returned buffer is truncated to whatever the stream actually yields (short only at
    /// end-of-stream). Default impl built on [`seek_to_frame`](FrameReader::seek_to_frame) +
    /// [`read_frames`](FrameReader::read_frames).
    fn read_range(&mut self, start: i64, n_frames: usize) -> Result<Vec<f32>, AudioError> {
        self.seek_to_frame(start)?;
        let ch = self.channels().max(1) as usize;
        let mut out = vec![0.0f32; n_frames * ch];
        let mut filled = 0;
        while filled < n_frames {
            let got = self.read_frames(&mut out[filled * ch..])?;
            if got == 0 {
                break;
            }
            filled += got;
        }
        out.truncate(filled * ch);
        Ok(out)
    }
}

/// A [`FrameReader`] over an in-memory interleaved buffer. Trivially seekable; used by tests and
/// any caller that already holds the decoded PCM.
pub struct SliceFrameReader<'a> {
    samples: &'a [f32],
    channels: u16,
    sample_rate: u32,
    /// Read cursor, in frames.
    pos_frame: usize,
}

impl<'a> SliceFrameReader<'a> {
    /// Wrap interleaved f32 PCM (`len == frames × channels`) as a seekable reader.
    pub fn new(samples: &'a [f32], channels: u16, sample_rate: u32) -> Self {
        Self {
            samples,
            channels,
            sample_rate,
            pos_frame: 0,
        }
    }

    fn n_frames(&self) -> usize {
        let ch = self.channels.max(1) as usize;
        self.samples.len() / ch
    }
}

impl FrameReader for SliceFrameReader<'_> {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn read_frames(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let ch = self.channels.max(1) as usize;
        let cap_frames = out.len() / ch;
        let avail_frames = self.n_frames() - self.pos_frame;
        let take = cap_frames.min(avail_frames);
        let begin = self.pos_frame * ch;
        out[..take * ch].copy_from_slice(&self.samples[begin..begin + take * ch]);
        self.pos_frame += take;
        Ok(take)
    }

    fn seek_to_frame(&mut self, frame: i64) -> Result<(), AudioError> {
        let clamped = frame.max(0) as usize;
        self.pos_frame = clamped.min(self.n_frames());
        Ok(())
    }
}

/// A [`FrameReader`] backed by Symphonia over a `MediaSourceStream` (production use: the
/// resampled FLAC cache). Reads any Symphonia-supported codec — nothing in the implementation
/// is FLAC-specific.
///
/// Sequential reads decode packets on demand and hold the leftover of a decoded packet between
/// calls. [`seek_to_frame`](FrameReader::seek_to_frame) uses Symphonia's accurate seek (which
/// lands on a frame boundary ≤ the target) and then **decodes forward and discards** the
/// `required_ts − actual_ts` frames so the next read is sample-accurate.
///
/// **Seek caveat.** Sample-accurate seek relies on the Symphonia demuxer coarse-seeking to a
/// boundary at-or-before the target. This is rock-solid for the fixed-block-size FLAC cache we
/// write; it has not been validated as sample-accurate across arbitrary codecs.
pub struct SymphoniaFrameReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    channels: u16,
    sample_rate: u32,
    /// Decoded interleaved samples not yet handed to the caller.
    leftover: Vec<f32>,
    /// Consumed offset (in samples, not frames) within `leftover`.
    leftover_pos: usize,
    /// Frames still to drop (from a pending seek) before yielding any output.
    discard_frames: usize,
    eof: bool,
}

impl SymphoniaFrameReader {
    /// Open `path` for seekable frame reads.
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        let o = super::decode::open_symphonia(path)?;
        Ok(Self {
            format: o.format,
            decoder: o.decoder,
            track_id: o.track_id,
            channels: o.channels,
            sample_rate: o.sample_rate,
            leftover: Vec::new(),
            leftover_pos: 0,
            discard_frames: 0,
            eof: false,
        })
    }

    /// Decode the next non-empty audio packet into `leftover`. Returns `false` at clean EOF.
    ///
    /// Delegates to the shared [`decode_next_packet`](super::decode::decode_next_packet) pump,
    /// which uses the rebuild path on format-level `ResetRequired` (chained streams).
    fn fill_one_packet(&mut self) -> Result<bool, AudioError> {
        match super::decode::decode_next_packet(&mut self.format, &mut self.decoder, self.track_id)?
        {
            None => Ok(false),
            Some(samples) => {
                self.leftover = samples;
                self.leftover_pos = 0;
                Ok(true)
            }
        }
    }
}

impl FrameReader for SymphoniaFrameReader {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn read_frames(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let ch = self.channels as usize;
        if ch == 0 {
            return Ok(0);
        }
        let cap_frames = out.len() / ch;
        let mut filled = 0;
        while filled < cap_frames {
            if self.leftover_pos >= self.leftover.len() {
                if self.eof {
                    break;
                }
                if !self.fill_one_packet()? {
                    self.eof = true;
                }
                continue;
            }
            // Drop seek-discard frames before yielding anything to the caller.
            if self.discard_frames > 0 {
                let avail_frames = (self.leftover.len() - self.leftover_pos) / ch;
                let drop = self.discard_frames.min(avail_frames);
                self.leftover_pos += drop * ch;
                self.discard_frames -= drop;
                continue;
            }
            let avail = self.leftover.len() - self.leftover_pos;
            let want = (cap_frames - filled) * ch;
            let take = want.min(avail);
            out[filled * ch..filled * ch + take]
                .copy_from_slice(&self.leftover[self.leftover_pos..self.leftover_pos + take]);
            self.leftover_pos += take;
            filled += take / ch;
        }
        Ok(filled)
    }

    fn seek_to_frame(&mut self, frame: i64) -> Result<(), AudioError> {
        let target = frame.max(0);
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: Timestamp::new(target),
                    track_id: self.track_id,
                },
            )
            .map_err(map_symphonia_error)?;
        self.decoder.reset();
        self.leftover.clear();
        self.leftover_pos = 0;
        self.eof = false;
        // Accurate seek lands at or before the target; decode-forward-and-discard the remainder.
        self.discard_frames = (seeked.required_ts.get() - seeked.actual_ts.get()).max(0) as usize;
        Ok(())
    }
}

/// Count the project-rate frames in a FLAC cache file by streaming through it.
///
/// Used as the fallback in [`probe_length`](super::cache) when the container header omits the
/// frame count (rare for our fixed-block-size FLAC cache, but defensive). Replaces the prior
/// whole-buffer `decode_flac(path)?.frames()` call.
pub fn count_frames(path: &Path) -> Result<i64, AudioError> {
    let mut reader = SymphoniaFrameReader::open(path)?;
    let ch = reader.channels().max(1) as usize;
    let mut total = 0i64;
    let mut buf = vec![0.0f32; 4096 * ch];
    loop {
        let n = reader.read_frames(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as i64;
    }
    Ok(total)
}

/// Map a Symphonia error to an [`AudioError`], preserving real I/O errors.
fn map_symphonia_error(e: SymphoniaError) -> AudioError {
    match e {
        SymphoniaError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            AudioError::UnsupportedFormat { codec: None }
        }
        SymphoniaError::IoError(io) => AudioError::Io(io),
        SymphoniaError::Unsupported(_) => AudioError::UnsupportedFormat { codec: None },
        e => AudioError::DecodeFailed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decode::probe;
    use crate::audio::flac::{decode_flac, encode_flac_24};
    use std::f32::consts::PI;
    use tempfile::TempDir;

    fn sine(freq: f32, rate: u32, frames: usize, ch: u16) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * ch as usize);
        for i in 0..frames {
            let v = (2.0 * PI * freq * i as f32 / rate as f32).sin();
            for _ in 0..ch {
                out.push(v);
            }
        }
        out
    }

    // FR1 — In-memory sequential read returns the whole buffer in order.
    #[test]
    fn fr1_slice_sequential() {
        let data = sine(440.0, 48000, 1000, 1);
        let mut r = SliceFrameReader::new(&data, 1, 48000);
        let mut buf = vec![0.0f32; 256];
        let mut out = Vec::new();
        loop {
            let n = r.read_frames(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data, "FR1: sequential read must reproduce the buffer");
    }

    // FR2 — In-memory seek + range read lands sample-accurate.
    #[test]
    fn fr2_slice_seek_range() {
        let data: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut r = SliceFrameReader::new(&data, 1, 48000);
        let got = r.read_range(250, 100).unwrap();
        let expected: Vec<f32> = (250..350).map(|i| i as f32).collect();
        assert_eq!(got, expected, "FR2: range read must be sample-accurate");

        // Seek past EOF clamps; read yields nothing.
        let tail = r.read_range(5000, 10).unwrap();
        assert!(tail.is_empty(), "FR2: seek past end yields no frames");
    }

    // FR3 — FLAC sequential read matches a whole-file decode.
    #[test]
    fn fr3_flac_sequential_matches_decode() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("seq.flac");
        let data = sine(440.0, 48000, 9000, 2);
        encode_flac_24(&data, 48000, 2, &path).unwrap();

        let whole = decode_flac(&path).unwrap();
        let mut r = SymphoniaFrameReader::open(&path).unwrap();
        assert_eq!(r.channels(), 2, "FR3: channels");
        assert_eq!(r.sample_rate(), 48000, "FR3: rate");

        let mut buf = vec![0.0f32; 333 * 2];
        let mut out = Vec::new();
        loop {
            let n = r.read_frames(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n * 2]);
        }
        assert_eq!(
            out.len(),
            whole.samples.len(),
            "FR3: streamed length matches decode"
        );
        assert_eq!(out, whole.samples, "FR3: streamed samples match decode");
    }

    // FR4 — FLAC seek + range read is sample-accurate against the whole-file decode.
    #[test]
    fn fr4_flac_seek_accurate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("seek.flac");
        // 24-bit FLAC round-trips exactly for these mid-scale values; pick distinct per-frame
        // values so any off-by-one in the seek/discard shows up immediately.
        let frames = 20_000usize;
        let data: Vec<f32> = (0..frames).map(|i| (i % 1000) as f32 / 2000.0).collect();
        encode_flac_24(&data, 48000, 1, &path).unwrap();
        let whole = decode_flac(&path).unwrap();

        let mut r = SymphoniaFrameReader::open(&path).unwrap();
        // Seek to several offsets, including non-packet-boundary frames.
        for &start in &[0i64, 1, 4097, 4096, 12345, 19990] {
            let got = r.read_range(start, 50).unwrap();
            let s = start as usize;
            let expected = &whole.samples[s..(s + 50).min(whole.samples.len())];
            assert_eq!(
                got, expected,
                "FR4: seek to {start} must be sample-accurate"
            );
        }
    }

    // FR5 — Stereo FLAC seek + range read is sample-accurate. The mono FR4 cannot distinguish
    //        frame-vs-sample arithmetic in the seek-discard path (`drop * ch`, `(len - pos) / ch`)
    //        because `ch == 1` makes `* ch` / `/ ch` no-ops; stereo with per-sample-distinct,
    //        L≠R values forces those factors to bind so any frame/sample confusion misaligns.
    #[test]
    fn fr5_flac_stereo_seek_accurate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("seek_stereo.flac");
        let frames = 20_000usize;
        // Interleaved stereo, every sample distinct and L≠R within a frame; mid-scale so 24-bit
        // FLAC round-trips exactly. L = +(i%997)/4000, R = -(i%991)/4000.
        let mut data = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            data.push((i % 997) as f32 / 4000.0);
            data.push(-((i % 991) as f32) / 4000.0);
        }
        encode_flac_24(&data, 48000, 2, &path).unwrap();
        let whole = decode_flac(&path).unwrap();
        assert_eq!(whole.channels, 2, "FR5: stereo fixture");

        let mut r = SymphoniaFrameReader::open(&path).unwrap();
        assert_eq!(r.channels(), 2, "FR5: channels");
        // Include non-block-boundary frames so the seek-discard path runs.
        for &start in &[0i64, 1, 4097, 4096, 12345, 19990] {
            let got = r.read_range(start, 50).unwrap();
            let s = start as usize;
            let avail = whole.frames().saturating_sub(s).min(50);
            let expected = &whole.samples[s * 2..(s + avail) * 2];
            assert_eq!(
                got, expected,
                "FR5: stereo seek to frame {start} must be sample-accurate"
            );
        }
    }

    // CF1 — count_frames == probe().length_frames == whole-buffer frame count (A4 seam).
    #[test]
    fn cf1_count_frames_matches_probe_and_decode() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cf1.flac");
        let frames = 9000usize;
        let data = sine(440.0, 48000, frames, 1);
        encode_flac_24(&data, 48000, 1, &path).unwrap();

        let cf = count_frames(&path).unwrap();
        let p = probe(&path).unwrap();
        let whole = decode_flac(&path).unwrap();

        assert_eq!(cf, frames as i64, "CF1: count_frames == encoded frames");
        assert_eq!(
            cf,
            p.length_frames.expect("CF1: probe must report length"),
            "CF1: count_frames == probe.length_frames"
        );
        assert_eq!(
            cf,
            whole.frames() as i64,
            "CF1: count_frames == decode().frames()"
        );
    }

    // CF2 — count_frames on a file whose length is not a 4096-frame multiple returns the exact
    //        total (exercises the final short FLAC frame — A4 block-boundary seam).
    #[test]
    fn cf2_count_frames_non_multiple_block_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cf2.flac");
        let frames = 10_003usize; // not a multiple of 4096
        let data = sine(440.0, 48000, frames, 1);
        encode_flac_24(&data, 48000, 1, &path).unwrap();

        let cf = count_frames(&path).unwrap();
        assert_eq!(
            cf, frames as i64,
            "CF2: exact total including final short frame"
        );
    }

    // ME1 — map_symphonia_error routes each Symphonia variant to the right AudioError. The
    //        `UnexpectedEof` I/O case must map to UnsupportedFormat (a truncated/garbage probe
    //        is "not our format"), while every other I/O error must surface as a real Io error
    //        so callers don't mistake a permission failure for an unsupported codec.
    #[test]
    fn me1_map_symphonia_error_routes_each_variant() {
        use std::io::{Error as IoError, ErrorKind};

        // EOF I/O error -> UnsupportedFormat (guard true).
        let eof = SymphoniaError::IoError(IoError::new(ErrorKind::UnexpectedEof, "eof"));
        assert!(
            matches!(
                map_symphonia_error(eof),
                AudioError::UnsupportedFormat { codec: None }
            ),
            "ME1: UnexpectedEof I/O -> UnsupportedFormat"
        );

        // Non-EOF I/O error -> Io (guard false). PermissionDenied stands in for a real failure.
        let denied = SymphoniaError::IoError(IoError::new(ErrorKind::PermissionDenied, "denied"));
        match map_symphonia_error(denied) {
            AudioError::Io(io) => {
                assert_eq!(
                    io.kind(),
                    ErrorKind::PermissionDenied,
                    "ME1: preserves kind"
                );
            }
            other => panic!("ME1: non-EOF I/O must map to Io, got {other:?}"),
        }

        // Unsupported -> UnsupportedFormat.
        assert!(
            matches!(
                map_symphonia_error(SymphoniaError::Unsupported("codec")),
                AudioError::UnsupportedFormat { codec: None }
            ),
            "ME1: Unsupported -> UnsupportedFormat"
        );

        // Anything else -> DecodeFailed.
        assert!(
            matches!(
                map_symphonia_error(SymphoniaError::DecodeError("corrupt")),
                AudioError::DecodeFailed(_)
            ),
            "ME1: other -> DecodeFailed"
        );
    }
}
