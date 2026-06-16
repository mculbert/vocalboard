//! Audio engine: decoding, resampling, playback, and signal processing.

pub mod bit_sink;
pub mod cache;
pub mod decode;
pub mod edl;
pub mod export;
pub mod ffmpeg;
pub mod flac;
pub mod frame_reader;
pub mod playback;
pub mod render;
pub mod resample;
pub mod room_tone;
pub mod source_provider;
pub mod splice;
pub mod wav;
pub mod zero_crossing;

/// A pull-based source of interleaved f32 PCM, used to stream the import transcode
/// (decode → resample → encode) without materializing the whole signal.
///
/// `read` is **greedy**: it fills the destination completely (up to its frame capacity)
/// unless the source is exhausted, so a short fill unambiguously means "no more frames
/// after these". [`is_exhausted`](PcmSource::is_exhausted) reports whether the underlying
/// stream has been fully consumed (it may become true on the same call that returns a final
/// full-capacity read, which a short fill alone could not signal).
pub trait PcmSource {
    /// Channel count of the produced interleaved frames.
    fn channels(&self) -> u16;
    /// Sample rate (Hz) of the produced frames.
    fn sample_rate(&self) -> u32;
    /// Fill `out` (length a multiple of `channels`) with up to `out.len() / channels`
    /// frames, returning the number of **frames** written. Fills completely unless exhausted.
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError>;
    /// Whether the underlying stream has been fully consumed (no more frames will be produced).
    fn is_exhausted(&self) -> bool;
}

impl PcmSource for Box<dyn PcmSource> {
    fn channels(&self) -> u16 {
        (**self).channels()
    }
    fn sample_rate(&self) -> u32 {
        (**self).sample_rate()
    }
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        (**self).read(out)
    }
    fn is_exhausted(&self) -> bool {
        (**self).is_exhausted()
    }
}

/// A [`PcmSource`] backed by an already-decoded buffer. Used for the ffmpeg fallback path
/// (which decodes the whole stream up front) and as a test/helper source.
pub struct BufferedSource {
    samples: Vec<f32>,
    pos: usize,
    channels: u16,
    sample_rate: u32,
}

impl BufferedSource {
    /// Wrap interleaved f32 PCM as a pull source.
    pub fn new(samples: Vec<f32>, channels: u16, sample_rate: u32) -> Self {
        Self {
            samples,
            pos: 0,
            channels,
            sample_rate,
        }
    }
}

impl From<DecodedAudio> for BufferedSource {
    fn from(d: DecodedAudio) -> Self {
        Self::new(d.samples, d.channels, d.sample_rate)
    }
}

impl PcmSource for BufferedSource {
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let take = out.len().min(self.samples.len() - self.pos);
        out[..take].copy_from_slice(&self.samples[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take / self.channels.max(1) as usize)
    }
    fn is_exhausted(&self) -> bool {
        self.pos >= self.samples.len()
    }
}

/// Decoded audio: interleaved f32 in `[-1.0, 1.0]` at the source's native rate.
#[derive(Debug)]
pub struct DecodedAudio {
    /// Interleaved samples; `len == frames * channels`.
    pub samples: Vec<f32>,
    /// Source native sample rate in Hz.
    pub sample_rate: u32,
    /// Source native channel count.
    pub channels: u16,
}

impl DecodedAudio {
    /// Number of audio frames; equal to `samples.len() / channels`.
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.samples.len() / self.channels as usize
    }
}

/// Lightweight metadata read without decoding all samples.
///
/// Feeds `TrackMeta` at M4 import. `length_frames` is best-effort; the authoritative
/// count comes from a full decode + resample at M4 import.
pub struct AudioProbe {
    /// Pinned codec identifier, e.g. `"pcm_s16le"`, `"flac"`, `"mp3"`, `"aac"`.
    pub codec: String,
    /// Source sample rate in Hz.
    pub sample_rate: u32,
    /// Source channel count.
    pub channels: u16,
    /// Frame count as reported by the container, or `None` when unavailable.
    pub length_frames: Option<i64>,
}

/// Typed audio-engine error.
///
/// `error_key()` gives the snake_case key the M4 command boundary surfaces through Paraglide
/// (conventions.md C3/D2). Display messages never embed file paths (local-first invariant).
#[derive(Debug)]
pub enum AudioError {
    /// Low-level I/O failure (file not found, permission denied, etc.).
    Io(std::io::Error),
    /// The file format or codec is not supported on the Symphonia path.
    UnsupportedFormat {
        /// Codec identifier when known from the format probe.
        codec: Option<String>,
    },
    /// The format was recognised but a packet could not be decoded (corrupt or truncated file).
    DecodeFailed(String),
    /// A fallback decode was needed but no `ffmpeg`/`ffprobe` binary is on PATH.
    FfmpegUnavailable,
    /// The `ffmpeg` subprocess exited non-zero or produced unparsable output.
    FfmpegFailed {
        /// Path-redacted summary of ffmpeg's failure output.
        detail: String,
    },
    /// The FLAC encoder returned an error (encode boundary, not a decode path).
    EncodeFailed(String),
    /// The audio output device could not be opened, configured, or started.
    DeviceError(String),
    /// The requested export format is unsupported or unavailable (unknown extension, encoder absent).
    ExportUnsupportedFormat,
}

impl AudioError {
    /// Snake_case error key for the M4 Paraglide mapping (conventions.md C3/D2).
    pub fn error_key(&self) -> &'static str {
        match self {
            Self::Io(_) => "audio_io_error",
            Self::UnsupportedFormat { .. } => "decode_unsupported_format",
            Self::DecodeFailed(_) => "decode_failed",
            Self::FfmpegUnavailable => "ffmpeg_unavailable",
            Self::FfmpegFailed { .. } => "ffmpeg_failed",
            Self::EncodeFailed(_) => "encode_failed",
            Self::DeviceError(_) => "audio_device_error",
            Self::ExportUnsupportedFormat => "export_unsupported_format",
        }
    }
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "audio I/O error: {e}"),
            Self::UnsupportedFormat { codec: Some(c) } => {
                write!(f, "unsupported audio codec: {c}")
            }
            Self::UnsupportedFormat { codec: None } => write!(f, "unsupported audio format"),
            Self::DecodeFailed(msg) => write!(f, "decode failed: {msg}"),
            Self::FfmpegUnavailable => write!(f, "ffmpeg not available on PATH"),
            Self::FfmpegFailed { detail } => write!(f, "ffmpeg failed: {detail}"),
            Self::EncodeFailed(msg) => write!(f, "encode failed: {msg}"),
            Self::DeviceError(msg) => write!(f, "audio device error: {msg}"),
            Self::ExportUnsupportedFormat => write!(f, "unsupported or unavailable export format"),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Equal-power crossfade fade-**in** factor at ramp index `i` of `len` frames:
/// `sin(π/2 · i/(len − 1))`.
///
/// The complementary fade-out is `cos(π/2 · i/(len − 1)) == equal_power_gain(len − 1 − i, len)`,
/// so `fade_in² + fade_out² == 1` — **constant power** across the seam, the correct curve for
/// crossfading **uncorrelated** material (cut/mute seams, and the room-tone stitch + loop fold,
/// where a linear ramp would dip ~3 dB at the midpoint). `len <= 1` returns `1.0` (no
/// divide-by-zero). Inline so callers fold it into their seam loop with zero allocation.
#[inline]
pub fn equal_power_gain(i: usize, len: usize) -> f32 {
    if len <= 1 {
        return 1.0;
    }
    let t = i as f32 / (len - 1) as f32;
    (t * std::f32::consts::FRAC_PI_2).sin()
}

#[cfg(test)]
mod tests {
    use super::{equal_power_gain, AudioError, BufferedSource, DecodedAudio, PcmSource};
    use std::error::Error as _;

    // BufferedSource::read fills greedily and reports the frame count (not the
    // sample count); pins line 90 (`take` slicing) and line 93 (`/ channels`).
    #[test]
    fn buffered_source_read_returns_frame_count() {
        // 3 stereo frames = 6 interleaved samples.
        let mut src = BufferedSource::new(vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 2, 48_000);
        assert_eq!(src.channels(), 2);
        assert_eq!(src.sample_rate(), 48_000);

        let mut out = [0.0f32; 6];
        let frames = src.read(&mut out).expect("read");
        assert_eq!(frames, 3, "6 samples / 2 channels == 3 frames");
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        assert!(src.is_exhausted(), "fully drained");

        // A second read past the end yields 0 frames and stays exhausted.
        let frames = src.read(&mut out).expect("read past end");
        assert_eq!(frames, 0);
        assert!(src.is_exhausted());
    }

    // pos advances by the number of samples consumed (pins `+=`, line 92), so a
    // capped read leaves the source not-yet-exhausted with the next slice intact.
    #[test]
    fn buffered_source_read_advances_position() {
        let mut src = BufferedSource::new(vec![1.0, 2.0, 3.0, 4.0], 1, 8_000);
        let mut out = [0.0f32; 2];

        let frames = src.read(&mut out).expect("first read");
        assert_eq!(frames, 2);
        assert_eq!(out, [1.0, 2.0]);
        assert!(!src.is_exhausted(), "two of four samples remain");

        let frames = src.read(&mut out).expect("second read");
        assert_eq!(frames, 2);
        assert_eq!(
            out,
            [3.0, 4.0],
            "position must have advanced past the first slice"
        );
        assert!(src.is_exhausted());
    }

    // is_exhausted is strictly position-driven: false before any read, true only
    // once pos has reached len. Pins lines 95-96 (the `>=` boundary, and that it
    // is neither hardcoded true nor false).
    #[test]
    fn buffered_source_is_exhausted_tracks_position() {
        let mut src = BufferedSource::new(vec![1.0, 2.0], 1, 8_000);
        assert!(!src.is_exhausted(), "fresh source is not exhausted");

        let mut one = [0.0f32; 1];
        src.read(&mut one).expect("partial read");
        assert!(!src.is_exhausted(), "one sample still pending (pos < len)");

        src.read(&mut one).expect("final read");
        assert!(src.is_exhausted(), "pos == len ⇒ exhausted");
    }

    // The blanket `Box<dyn PcmSource>` impl forwards every method to the inner
    // source; pins lines 41-52 (channels/sample_rate/read/is_exhausted forwarding).
    #[test]
    fn boxed_source_forwards_to_inner() {
        let mut boxed: Box<dyn PcmSource> =
            Box::new(BufferedSource::new(vec![0.5, -0.5], 2, 44_100));
        assert_eq!(boxed.channels(), 2);
        assert_eq!(boxed.sample_rate(), 44_100);
        assert!(!boxed.is_exhausted(), "not yet read");

        let mut out = [0.0f32; 2];
        let frames = boxed.read(&mut out).expect("boxed read");
        assert_eq!(frames, 1, "2 samples / 2 channels");
        assert_eq!(out, [0.5, -0.5]);
        assert!(boxed.is_exhausted(), "forwarded exhaustion after drain");
    }

    // DecodedAudio -> BufferedSource carries rate/channels and the samples through.
    #[test]
    fn buffered_source_from_decoded_audio() {
        let decoded = DecodedAudio {
            samples: vec![0.0, 0.25, 0.5, 0.75],
            sample_rate: 16_000,
            channels: 2,
        };
        assert_eq!(decoded.frames(), 2);

        let mut src = BufferedSource::from(decoded);
        assert_eq!(src.channels(), 2);
        assert_eq!(src.sample_rate(), 16_000);
        let mut out = [0.0f32; 4];
        assert_eq!(src.read(&mut out).expect("read"), 2);
        assert_eq!(out, [0.0, 0.25, 0.5, 0.75]);
    }

    // Display must produce the documented, non-empty messages — pins line 184
    // (the whole match body, which a `Ok(Default::default())` mutant blanks out).
    #[test]
    fn audio_error_display_messages() {
        let io = AudioError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "boom"));
        assert_eq!(io.to_string(), "audio I/O error: boom");
        assert_eq!(
            AudioError::UnsupportedFormat {
                codec: Some("mp3".into())
            }
            .to_string(),
            "unsupported audio codec: mp3"
        );
        assert_eq!(
            AudioError::UnsupportedFormat { codec: None }.to_string(),
            "unsupported audio format"
        );
        assert_eq!(
            AudioError::DecodeFailed("bad".into()).to_string(),
            "decode failed: bad"
        );
        assert_eq!(
            AudioError::FfmpegUnavailable.to_string(),
            "ffmpeg not available on PATH"
        );
        assert_eq!(
            AudioError::FfmpegFailed { detail: "x".into() }.to_string(),
            "ffmpeg failed: x"
        );
        assert_eq!(
            AudioError::EncodeFailed("y".into()).to_string(),
            "encode failed: y"
        );
        assert_eq!(
            AudioError::DeviceError("z".into()).to_string(),
            "audio device error: z"
        );
        assert_eq!(
            AudioError::ExportUnsupportedFormat.to_string(),
            "unsupported or unavailable export format"
        );
    }

    // Error::source returns the underlying io::Error for the Io variant and None
    // for everything else; pins lines 202-204 (the source() body and Io arm).
    #[test]
    fn audio_error_source_chains_io_only() {
        let io = AudioError::Io(std::io::Error::other("inner"));
        let inner = io.source().expect("Io must expose its source");
        assert_eq!(inner.to_string(), "inner");

        assert!(
            AudioError::DecodeFailed("d".into()).source().is_none(),
            "non-Io variants have no source"
        );
        assert!(AudioError::FfmpegUnavailable.source().is_none());
    }

    // Endpoints: fade-in is exactly 0 at the head and ~1 at the tail.
    #[test]
    fn equal_power_gain_endpoints() {
        let len = 96usize;
        assert_eq!(equal_power_gain(0, len), 0.0);
        assert!((equal_power_gain(len - 1, len) - 1.0).abs() < 1e-6);
    }

    // Constant power: fade_in² + fade_out² == 1 at every index (fade_out is the
    // symmetric `equal_power_gain(len - 1 - i, len)`).
    #[test]
    fn equal_power_gain_constant_power() {
        let len = 96usize;
        for i in 0..len {
            let g_in = equal_power_gain(i, len);
            let g_out = equal_power_gain(len - 1 - i, len);
            assert!(
                (g_in * g_in + g_out * g_out - 1.0).abs() < 1e-6,
                "non-constant power at i={i}: {}",
                g_in * g_in + g_out * g_out
            );
        }
    }

    // Monotonic increasing, and the midpoint is the equal-power 0.707 crossover.
    #[test]
    fn equal_power_gain_monotonic_midpoint() {
        let len = 96usize;
        for i in 1..len {
            assert!(
                equal_power_gain(i, len) > equal_power_gain(i - 1, len),
                "not monotonic at i={i}"
            );
        }
        // Odd length ⇒ an exact midpoint index at t = 0.5 ⇒ sin(π/4) = 1/√2.
        let mid_len = 97usize;
        assert!((equal_power_gain(48, mid_len) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    // Degenerate len <= 1 ⇒ 1.0 (no divide-by-zero).
    #[test]
    fn equal_power_gain_degenerate_len() {
        assert_eq!(equal_power_gain(0, 1), 1.0);
        assert_eq!(equal_power_gain(0, 0), 1.0);
    }
}
