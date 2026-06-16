//! ffmpeg subprocess fallback decoder and availability probe.
//!
//! Invoked only when Symphonia rejects a file with an unsupported-format error. Outputs
//! native rate and native channel layout (`-f f32le`, no `-ar`/`-ac` rewrite) so that
//! rubato remains the sole resampler in the system.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use super::{AudioError, AudioProbe, PcmSource};

#[cfg(test)]
use super::DecodedAudio;

static FFMPEG_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Returns `true` when both `ffmpeg` and `ffprobe` binaries are on PATH.
///
/// The result is cached on first call; subsequent calls return the cached value without
/// spawning any process.
pub fn ffmpeg_available() -> bool {
    *FFMPEG_AVAILABLE.get_or_init(|| {
        let ffmpeg_ok = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let ffprobe_ok = Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        ffmpeg_ok && ffprobe_ok
    })
}

/// Decode `path` to interleaved f32 PCM at its native rate via the ffmpeg subprocess.
///
/// Probes first to obtain sample rate and channel count, then runs ffmpeg with
/// `-f f32le -acodec pcm_f32le` to get raw PCM on stdout. No `-ar`/`-ac` rewrite —
/// rubato is the only resampler in the system.
///
/// **Test support only.** Production code uses [`FfmpegSource`] (streaming).
#[cfg(test)]
pub(crate) fn decode_via_ffmpeg(path: &Path) -> Result<DecodedAudio, AudioError> {
    // Probe first to get the rate and channel count.
    let meta = probe_via_ffmpeg(path)?;

    let output = Command::new("ffmpeg")
        .args(["-v", "error"])
        .arg("-i")
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-"])
        .output()
        .map_err(AudioError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(AudioError::FfmpegFailed {
            detail: redact_path(&stderr, path),
        });
    }

    let bytes = output.stdout;
    if bytes.len() % 4 != 0 {
        return Err(AudioError::FfmpegFailed {
            detail: format!("stdout length {} is not a multiple of 4 bytes", bytes.len()),
        });
    }

    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    Ok(DecodedAudio {
        samples,
        sample_rate: meta.sample_rate,
        channels: meta.channels,
    })
}

/// Read codec/rate/channel/length metadata via `ffprobe`.
pub(crate) fn probe_via_ffmpeg(path: &Path) -> Result<AudioProbe, AudioError> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "a:0",
        ])
        .arg(path)
        .output()
        .map_err(AudioError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(AudioError::FfmpegFailed {
            detail: redact_path(&stderr, path),
        });
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| AudioError::FfmpegFailed {
            detail: format!("ffprobe JSON parse error: {e}"),
        })?;

    let stream = json["streams"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| AudioError::FfmpegFailed {
            detail: "ffprobe: no audio stream in output".into(),
        })?;

    let codec = stream["codec_name"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();

    let sample_rate: u32 = stream["sample_rate"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let channels: u16 = stream["channels"].as_u64().map(|c| c as u16).unwrap_or(0);

    // nb_frames is a string in ffprobe JSON; fall back to duration * sample_rate.
    let length_frames = stream["nb_frames"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|&n| n > 0)
        .or_else(|| {
            let dur: f64 = stream["duration"].as_str()?.parse().ok()?;
            if sample_rate > 0 && dur > 0.0 {
                Some((dur * sample_rate as f64).round() as i64)
            } else {
                None
            }
        });

    Ok(AudioProbe {
        codec,
        sample_rate,
        channels,
        length_frames,
    })
}

/// Replace the source path in `s` with `<path>` to avoid leaking file paths in error messages.
fn redact_path(s: &str, path: &Path) -> String {
    match path.to_str() {
        Some(p) if !p.is_empty() => s.replace(p, "<path>"),
        _ => s.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Streaming ffmpeg source
// ---------------------------------------------------------------------------

/// A streaming [`PcmSource`] that decodes via an ffmpeg subprocess piping raw f32le PCM.
///
/// Probes with ffprobe first for native `sample_rate`/`channels` (no `-ar`/`-ac` — rubato is
/// the sole resampler). Stderr is redirected to a temp file so the stdout/stderr pipe-fill
/// deadlock is avoided without a reader thread (`-v error` keeps stderr tiny). The pipe is
/// read incrementally; any sub-4-byte cross-call remainder is held in a 3-byte buffer to
/// handle pipe chunk boundaries that don't align to sample boundaries.
pub struct FfmpegSource {
    child: std::process::Child,
    stdout: std::process::ChildStdout,
    stderr_path: PathBuf,
    source_path: PathBuf,
    channels: u16,
    sample_rate: u32,
    /// Bytes from the previous `read` that did not complete a 4-byte f32 group (0..=3 bytes).
    remainder: [u8; 3],
    remainder_len: usize,
    eof: bool,
    exhausted: bool,
}

impl FfmpegSource {
    /// Spawn ffmpeg on `path` and return a streaming source at the file's native rate.
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        let meta = probe_via_ffmpeg(path)?;

        let stderr_name = format!("vb_ffmpeg_{}.tmp", uuid::Uuid::new_v4().simple());
        let stderr_path = std::env::temp_dir().join(stderr_name);
        let stderr_file = std::fs::File::create(&stderr_path).map_err(AudioError::Io)?;

        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .map_err(AudioError::Io)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AudioError::FfmpegFailed {
                detail: "failed to capture ffmpeg stdout pipe".into(),
            })?;

        Ok(Self {
            child,
            stdout,
            stderr_path,
            source_path: path.to_path_buf(),
            channels: meta.channels,
            sample_rate: meta.sample_rate,
            remainder: [0; 3],
            remainder_len: 0,
            eof: false,
            exhausted: false,
        })
    }

    /// Called at stdout EOF: validate no partial sample remains, wait for the child, check
    /// the exit status. Sets `exhausted = true` on success.
    fn finish(&mut self) -> Result<(), AudioError> {
        if self.remainder_len > 0 {
            return Err(AudioError::FfmpegFailed {
                detail: format!(
                    "ffmpeg stdout length is not a multiple of 4 bytes ({} trailing bytes)",
                    self.remainder_len
                ),
            });
        }
        let status = self.child.wait().map_err(AudioError::Io)?;
        if !status.success() {
            let detail = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
            return Err(AudioError::FfmpegFailed {
                detail: redact_path(&detail, &self.source_path),
            });
        }
        self.exhausted = true;
        Ok(())
    }
}

impl Drop for FfmpegSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

impl PcmSource for FfmpegSource {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Fill `out` with decoded f32 frames. Greedily fills unless the stream is exhausted.
    ///
    /// Reads from the ffmpeg stdout pipe in chunks; any sub-4-byte boundary left over from a
    /// pipe read is held across calls so that frame assembly is always correct regardless of
    /// how the OS delivers pipe bytes.
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        if self.exhausted || self.eof {
            return Ok(0);
        }
        if self.channels == 0 {
            self.exhausted = true;
            return Ok(0);
        }
        let ch = self.channels as usize;
        let cap_samples = out.len();
        if cap_samples == 0 {
            return Ok(0);
        }

        let mut out_pos = 0usize;
        // Room for 1024 f32s (4096 bytes) + 3 remainder bytes at the front.
        let mut byte_buf = [0u8; 4096 + 3];

        while out_pos < cap_samples && !self.eof {
            let need_samples = cap_samples - out_pos;
            let start = self.remainder_len;
            // Request exactly enough bytes to fill `need_samples`, capped at buffer capacity.
            // Invariant: need_samples * 4 > start (since start <= 3 < 4 <= need_samples * 4).
            let request = (need_samples * 4 - start).min(byte_buf.len() - start);

            byte_buf[..start].copy_from_slice(&self.remainder[..start]);

            let n = match self.stdout.read(&mut byte_buf[start..start + request]) {
                Ok(0) => {
                    self.eof = true;
                    0
                }
                Ok(n) => n,
                Err(e) => return Err(AudioError::Io(e)),
            };

            // total <= start + request <= need_samples * 4, so complete <= need_samples.
            let total = start + n;
            let complete = total / 4;
            for i in 0..complete {
                let b = i * 4;
                out[out_pos + i] = f32::from_le_bytes([
                    byte_buf[b],
                    byte_buf[b + 1],
                    byte_buf[b + 2],
                    byte_buf[b + 3],
                ]);
            }
            out_pos += complete;

            // Remainder is total % 4, always in 0..=3.
            let consumed = complete * 4;
            self.remainder_len = total - consumed;
            self.remainder[..self.remainder_len].copy_from_slice(&byte_buf[consumed..total]);
        }

        if self.eof {
            self.finish()?;
        }

        Ok(out_pos / ch)
    }
}

// ---------------------------------------------------------------------------
// Encode: pipe f32le PCM to ffmpeg (mp3 / ogg / aac export)
// ---------------------------------------------------------------------------

/// Encode `src` to `out` via an `ffmpeg` subprocess using `codec` (e.g. `libmp3lame`,
/// `libvorbis`, `aac`).
///
/// Pulls interleaved f32 frames from `src` and pipes them as f32le PCM to ffmpeg's stdin;
/// ffmpeg writes `out` directly (we never read the encoded bytes back). The channel count and
/// sample rate come from `src`. Caller must have verified [`ffmpeg_available`]. On failure
/// (spawn, pipe, or non-zero exit) the partial `out` is removed and ffmpeg's stderr is surfaced.
pub(crate) fn encode_via_ffmpeg(
    mut src: impl PcmSource,
    out: &Path,
    codec: &str,
) -> Result<(), AudioError> {
    let channels = src.channels();
    let sample_rate = src.sample_rate();
    match encode_inner(&mut src, channels, sample_rate, out, codec) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(out); // ffmpeg owns the write; drop any partial output
            Err(e)
        }
    }
}

fn encode_inner(
    src: &mut impl PcmSource,
    channels: u16,
    sample_rate: u32,
    out: &Path,
    codec: &str,
) -> Result<(), AudioError> {
    let stderr_name = format!("vb_ffmpeg_enc_{}.tmp", uuid::Uuid::new_v4().simple());
    let stderr_path = std::env::temp_dir().join(stderr_name);
    let stderr_file = std::fs::File::create(&stderr_path).map_err(AudioError::Io)?;

    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(["-f", "f32le"])
        .args(["-ar", &sample_rate.to_string()])
        .args(["-ac", &channels.to_string()])
        .args(["-i", "pipe:0"])
        .args(["-c:a", codec])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(AudioError::Io)?;

    let mut stdin = child.stdin.take().ok_or_else(|| AudioError::FfmpegFailed {
        detail: "failed to capture ffmpeg stdin pipe".into(),
    })?;

    // Pull frames → write f32le to stdin. A read error from `src` (or a broken pipe if ffmpeg
    // died) ends the loop; we still close stdin and reap the child below before surfacing it.
    let ch = channels.max(1) as usize;
    let mut buf = vec![0.0f32; 4096 * ch];
    let write_result = (|| -> Result<(), AudioError> {
        loop {
            let frames = src.read(&mut buf)?;
            if frames == 0 {
                break;
            }
            let n = frames * ch;
            let bytes: Vec<u8> = buf[..n].iter().flat_map(|s| s.to_le_bytes()).collect();
            stdin.write_all(&bytes).map_err(AudioError::Io)?;
        }
        Ok(())
    })();

    drop(stdin); // close → EOF → ffmpeg finalizes the encoded file
    let status = child.wait().map_err(AudioError::Io)?;
    let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    let _ = std::fs::remove_file(&stderr_path);

    // A non-zero exit means ffmpeg rejected the input (or the codec) — surface its stderr, which
    // is more informative than the broken-pipe write error that race produces.
    if !status.success() {
        return Err(AudioError::FfmpegFailed {
            detail: stderr_text,
        });
    }
    // ffmpeg succeeded: the only way `write_result` is an error here is a genuine `src` read
    // failure (we fed ffmpeg a truncated stream), so surface that.
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use tempfile::TempDir;

    // Minimal f32le WAV writer for tests; produces the same format ffmpeg decodes to.
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
        buf.extend_from_slice(&3u16.to_le_bytes()); // IEEE float PCM
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

    /// Drain a `PcmSource` into a flat sample vec.
    fn drain(src: &mut FfmpegSource, buf_frames: usize) -> Result<Vec<f32>, AudioError> {
        let ch = src.channels().max(1) as usize;
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; buf_frames * ch];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n * ch]);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // F17 — ffmpeg_available() is side-effect-free and returns consistently
    // -----------------------------------------------------------------------

    #[test]
    fn f17_ffmpeg_available_consistent() {
        // Should not panic and should return the same value on repeated calls.
        let first = ffmpeg_available();
        let second = ffmpeg_available();
        assert_eq!(first, second, "F17: availability must be consistent");
        // OnceLock must be populated after the first call.
        assert!(
            FFMPEG_AVAILABLE.get().is_some(),
            "F17: OnceLock should be set"
        );
    }

    // -----------------------------------------------------------------------
    // F20 — Bad input → FfmpegFailed with path redacted from detail
    // -----------------------------------------------------------------------

    #[test]
    fn f20_bad_input_ffmpeg_failed_path_redacted() {
        if !ffmpeg_available() {
            return; // ffmpeg absent on this system; nothing to test
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("not_audio.wav");
        std::fs::write(&path, b"garbage bytes not parseable by ffmpeg").unwrap();

        let err = decode_via_ffmpeg(&path).expect_err("F20: bad input should fail");
        assert!(
            matches!(err, AudioError::FfmpegFailed { .. }),
            "F20: expected FfmpegFailed, got {err:?}"
        );
        if let AudioError::FfmpegFailed { ref detail } = err {
            assert!(
                !detail.contains(path.to_str().unwrap_or("")),
                "F20: path must be redacted from detail: {detail}"
            );
        }
        assert_eq!(err.error_key(), "ffmpeg_failed");
    }

    // -----------------------------------------------------------------------
    // FS1 — FfmpegSource streaming output is sample-for-sample equal to the
    //        whole-buffer decode_via_ffmpeg oracle (A4 translate-and-replay).
    // -----------------------------------------------------------------------

    #[test]
    fn fs1_streaming_matches_decode_via_ffmpeg() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sine.wav");

        let frames = 44100usize; // 1 second @ 44100 Hz
        let samples: Vec<f32> = (0..frames)
            .map(|i| (2.0 * PI * 440.0 * i as f32 / 44100.0).sin() * 0.5)
            .collect();
        write_wav_f32(&path, 44100, 1, &samples);

        let oracle = decode_via_ffmpeg(&path).expect("FS1: oracle");
        let mut src = FfmpegSource::open(&path).expect("FS1: open");
        assert_eq!(src.channels(), oracle.channels, "FS1: channels");
        assert_eq!(src.sample_rate(), oracle.sample_rate, "FS1: sample_rate");
        assert!(!src.is_exhausted(), "FS1: not yet exhausted");

        let got = drain(&mut src, 1024).expect("FS1: drain");
        assert!(src.is_exhausted(), "FS1: exhausted after drain");
        assert_eq!(got.len(), oracle.samples.len(), "FS1: sample count");
        assert_eq!(got, oracle.samples, "FS1: samples match oracle");
    }

    // -----------------------------------------------------------------------
    // FS2 — Chunk-boundary robustness: reading with various frame counts
    //        (including sizes that exercise the partial-byte remainder seam).
    // -----------------------------------------------------------------------

    #[test]
    fn fs2_chunk_boundary_robustness() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stereo.wav");

        // Stereo, 2 channels. Use 997 frames so the total is not a power-of-two multiple.
        let frames = 997usize;
        let ch = 2u16;
        let samples: Vec<f32> = (0..frames * ch as usize)
            .map(|i| (i % 1000) as f32 / 500.0 - 1.0)
            .collect();
        write_wav_f32(&path, 48000, ch, &samples);

        let oracle = decode_via_ffmpeg(&path).expect("FS2: oracle");

        // Read with various odd buffer sizes to hit partial-byte cross-call remainders.
        for buf_frames in [1usize, 3, 7, 100, 333, 1024] {
            let mut src = FfmpegSource::open(&path).expect("FS2: open");
            let got = drain(&mut src, buf_frames).expect("FS2: drain buf={buf_frames}");
            assert!(src.is_exhausted(), "FS2: exhausted (buf={buf_frames})");
            assert_eq!(
                got.len(),
                oracle.samples.len(),
                "FS2: sample count (buf={buf_frames})"
            );
            assert_eq!(
                got, oracle.samples,
                "FS2: samples match oracle (buf={buf_frames})"
            );
        }
    }

    // -----------------------------------------------------------------------
    // FS3 — Bad input → FfmpegFailed, path redacted, non-zero exit surfaced.
    // -----------------------------------------------------------------------

    #[test]
    fn fs3_bad_input_ffmpeg_failed_path_redacted() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("not_audio.wav");
        std::fs::write(&path, b"garbage bytes not parseable by ffmpeg").unwrap();

        // Error may come from probe (open) or from decode (read); either is FfmpegFailed.
        let err = match FfmpegSource::open(&path) {
            Err(e) => e,
            Ok(mut src) => src
                .read(&mut [0.0f32; 8])
                .expect_err("FS3: read should fail"),
        };
        assert!(
            matches!(err, AudioError::FfmpegFailed { .. }),
            "FS3: expected FfmpegFailed, got {err:?}"
        );
        if let AudioError::FfmpegFailed { ref detail } = err {
            assert!(
                !detail.contains(path.to_str().unwrap_or("")),
                "FS3: path must be redacted from detail: {detail}"
            );
        }
        assert_eq!(err.error_key(), "ffmpeg_failed", "FS3: error_key");
    }

    // -----------------------------------------------------------------------
    // FS4 — Empty/zero-frame source → 0 frames, no error, is_exhausted().
    // -----------------------------------------------------------------------

    #[test]
    fn fs4_empty_source_is_exhausted() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.wav");
        write_wav_f32(&path, 48000, 1, &[]);

        let mut src = FfmpegSource::open(&path).expect("FS4: open");
        assert_eq!(src.channels(), 1, "FS4: channels");
        assert_eq!(src.sample_rate(), 48000, "FS4: sample_rate");

        let mut buf = [0.0f32; 64];
        let n = src.read(&mut buf).expect("FS4: read should not error");
        assert_eq!(n, 0, "FS4: 0 frames for empty source");
        assert!(src.is_exhausted(), "FS4: must be exhausted");
    }

    // -----------------------------------------------------------------------
    // F21 — redact_path replaces the path, preserves surrounding text, and is
    //        a no-op for an empty path (the `!p.is_empty()` guard).
    // -----------------------------------------------------------------------

    #[test]
    fn f21_redact_path_behaviour() {
        let p = Path::new("/home/secret/user/song.wav");
        // The path is replaced by the placeholder...
        let out = redact_path("error opening /home/secret/user/song.wav: bad", p);
        assert_eq!(out, "error opening <path>: bad", "F21: path replaced");
        assert!(!out.contains("secret"), "F21: no path component leaks");

        // ...and unrelated text with no path occurrence is returned verbatim
        // (kills the `String::new()` / "xyzzy" whole-body replacements).
        let untouched = redact_path("unrelated message", p);
        assert_eq!(
            untouched, "unrelated message",
            "F21: verbatim when no match"
        );

        // An empty path must NOT trigger a replace (the match guard): replacing
        // the empty string would otherwise splice the placeholder everywhere.
        let empty = Path::new("");
        let out_empty = redact_path("abc", empty);
        assert_eq!(out_empty, "abc", "F21: empty path is a no-op (guard)");
    }

    // -----------------------------------------------------------------------
    // F22 — probe_via_ffmpeg length_frames: the `duration * sample_rate`
    //        fallback (WAV reports duration, no nb_frames) yields the exact
    //        frame count; codec/rate/channels are surfaced.
    // -----------------------------------------------------------------------

    #[test]
    fn f22_probe_length_frames_duration_fallback() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe.wav");

        let frames = 44100usize; // exactly 1.0 s @ 44100 Hz
        let samples: Vec<f32> = (0..frames).map(|i| (i % 100) as f32 / 100.0).collect();
        write_wav_f32(&path, 44100, 1, &samples);

        let probe = probe_via_ffmpeg(&path).expect("F22: probe");
        assert_eq!(probe.sample_rate, 44100, "F22: sample_rate");
        assert_eq!(probe.channels, 1, "F22: channels");
        // 1.0 s * 44100 Hz = 44100 frames. Kills `*`→`+`/`/` (would give 44101 /
        // ~0) and the `>0`/`&&` guard mutants (false → None).
        assert_eq!(
            probe.length_frames,
            Some(44100),
            "F22: duration*rate frame count"
        );
    }

    // -----------------------------------------------------------------------
    // F23 — probe_via_ffmpeg length_frames: when the container reports
    //        nb_frames (> 0), that value is used in preference to duration.
    //        AAC/m4a reports nb_frames (packet count); we assert it is taken.
    // -----------------------------------------------------------------------

    #[test]
    fn f23_probe_length_frames_nb_frames_preferred() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let wav = dir.path().join("src.wav");
        let m4a = dir.path().join("out.m4a");

        let frames = 44100usize;
        let samples: Vec<f32> = (0..frames)
            .map(|i| (2.0 * PI * 440.0 * i as f32 / 44100.0).sin() * 0.3)
            .collect();
        write_wav_f32(&wav, 44100, 1, &samples);

        // Transcode to AAC/m4a, which makes ffprobe emit a positive nb_frames.
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&wav)
            .args(["-c:a", "aac"])
            .arg(&m4a)
            .status()
            .expect("F23: ffmpeg transcode");
        if !status.success() {
            return; // AAC encoder unavailable on this build; nothing to assert
        }

        let probe = probe_via_ffmpeg(&m4a).expect("F23: probe");
        let len = probe.length_frames.expect("F23: length present");
        // nb_frames is the AAC packet count (far smaller than the 44100 sample
        // frames the duration fallback would compute). Asserting it is small and
        // positive pins the nb_frames branch and its `> 0` filter.
        assert!(len > 0, "F23: nb_frames is positive");
        assert!(
            len < 44100,
            "F23: nb_frames (packets) used, not duration*rate: got {len}"
        );
    }

    // -----------------------------------------------------------------------
    // F24 — Encode round-trip: encode_via_ffmpeg pulls every frame from a
    //        multi-channel source (so `frames * ch` buffer sizing matters) and
    //        produces a decodable file with the right rate/channels/length.
    // -----------------------------------------------------------------------

    #[test]
    fn f24_encode_round_trip_stereo() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("encoded.flac");

        let frames = 5000usize;
        let ch = 2u16;
        let samples: Vec<f32> = (0..frames * ch as usize)
            .map(|i| (2.0 * PI * 330.0 * i as f32 / 48000.0).sin() * 0.4)
            .collect();
        let src = super::super::BufferedSource::new(samples.clone(), ch, 48000);

        // The decoded length must match the input exactly: if `frames * ch` were
        // mutated to `frames + ch` / `frames / ch`, the per-read byte count fed to
        // ffmpeg would be wrong and the round-trip frame count would diverge.
        // (ffmpeg's flac encoder quantises f32→s16, so content is close but not
        // bit-exact — the length is the load-bearing invariant here.)
        encode_via_ffmpeg(src, &out, "flac").expect("F24: encode");

        let decoded = decode_via_ffmpeg(&out).expect("F24: decode back");
        assert_eq!(decoded.sample_rate, 48000, "F24: rate preserved");
        assert_eq!(decoded.channels, ch, "F24: channels preserved");
        assert_eq!(
            decoded.samples.len(),
            samples.len(),
            "F24: every input frame round-trips (frames*ch sizing)"
        );
        // Content survives within 16-bit quantisation error.
        let max_err = decoded
            .samples
            .iter()
            .zip(samples.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_err < 1e-3,
            "F24: content preserved within s16 error: {max_err}"
        );
    }

    // -----------------------------------------------------------------------
    // F25 — encode_via_ffmpeg removes the partial output file when the encode
    //        fails (bad codec name → ffmpeg non-zero exit → FfmpegFailed).
    // -----------------------------------------------------------------------

    #[test]
    fn f25_encode_failure_removes_partial_output() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("bad.out");

        let src = super::super::BufferedSource::new(vec![0.0f32; 256], 1, 48000);
        let err = encode_via_ffmpeg(src, &out, "no_such_codec_xyz")
            .expect_err("F25: bad codec should fail");
        assert!(
            matches!(err, AudioError::FfmpegFailed { .. }),
            "F25: expected FfmpegFailed, got {err:?}"
        );
        assert!(!out.exists(), "F25: partial output must be removed");
    }

    // -----------------------------------------------------------------------
    // F26 — Dropping an FfmpegSource removes its stderr temp file and reaps the
    //        child (kills the `drop` → () mutation).
    // -----------------------------------------------------------------------

    #[test]
    fn f26_drop_cleans_up_stderr_tempfile() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("drop.wav");
        let samples: Vec<f32> = (0..48000).map(|i| (i % 50) as f32 / 50.0).collect();
        write_wav_f32(&path, 48000, 1, &samples);

        let src = FfmpegSource::open(&path).expect("F26: open");
        let stderr_path = src.stderr_path.clone();
        assert!(
            stderr_path.exists(),
            "F26: stderr temp file created on open"
        );

        drop(src);
        assert!(
            !stderr_path.exists(),
            "F26: Drop must remove the stderr temp file"
        );
    }

    // -----------------------------------------------------------------------
    // F27 — read() with a buffer NOT aligned to a power-of-two byte count and a
    //        partial-frame remainder seam: assert read never returns a partial
    //        frame (out_pos / ch) and that the leftover bytes are carried. This
    //        exercises the +/- arithmetic on the request/remainder math
    //        (lines around `need_samples * 4 - start`).
    // -----------------------------------------------------------------------

    #[test]
    fn f27_read_partial_frame_and_remainder_arithmetic() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rem.wav");

        // Stereo so a single f32 is half a frame: any miscounting of the
        // per-call request/remainder shows up as a frame-count mismatch.
        let frames = 1500usize;
        let ch = 2u16;
        let samples: Vec<f32> = (0..frames * ch as usize)
            .map(|i| (i % 257) as f32 / 128.0 - 1.0)
            .collect();
        write_wav_f32(&path, 44100, ch, &samples);

        let oracle = decode_via_ffmpeg(&path).expect("F27: oracle");

        // A single-frame buffer forces start/remainder bookkeeping on every
        // call; a 5-frame buffer (20 bytes, not 4096-aligned) stresses the
        // `need_samples * 4 - start` request sizing.
        for buf_frames in [1usize, 5] {
            let mut src = FfmpegSource::open(&path).expect("F27: open");
            let mut out = vec![0.0f32; buf_frames * ch as usize];
            let mut collected = Vec::new();
            loop {
                let n = src.read(&mut out).expect("F27: read");
                if n == 0 {
                    break;
                }
                assert!(n <= buf_frames, "F27: never over-fills (buf={buf_frames})");
                collected.extend_from_slice(&out[..n * ch as usize]);
            }
            assert_eq!(
                collected, oracle.samples,
                "F27: exact reconstruction (buf={buf_frames})"
            );
        }
    }
}
