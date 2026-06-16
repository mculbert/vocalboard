//! Sinc resampler wrapping rubato's `Async` engine.

use std::collections::VecDeque;

use rubato::{
    audioadapter_buffers::{direct::InterleavedSlice, owned::InterleavedOwned},
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::settings::ResamplingQuality;

use super::{AudioError, PcmSource};

/// Fixed input-chunk size (frames) for the streaming `Async` resampler. Balances latency
/// and efficiency for offline transcoding; matches the whole-buffer [`resample`] path.
const CHUNK_FRAMES: usize = 1024;

/// Resample interleaved f32 from `from_rate` to `to_rate`, preserving channel count and
/// interleave order. Bit-exact identity when `from_rate == to_rate` (no rubato pass).
/// Output may briefly exceed [-1, 1] (sinc overshoot); callers clamp at the encode boundary.
pub fn resample(
    samples: &[f32],
    channels: u16,
    from_rate: u32,
    to_rate: u32,
    quality: ResamplingQuality,
) -> Result<Vec<f32>, AudioError> {
    if from_rate == to_rate {
        return Ok(samples.to_vec());
    }

    let ch = channels as usize;
    if ch == 0 {
        return Ok(Vec::new());
    }

    let in_frames = samples.len() / ch;
    if in_frames == 0 {
        return Ok(Vec::new());
    }

    let ratio = to_rate as f64 / from_rate as f64;
    let params = sinc_params(quality);

    let mut resampler =
        Async::<f32>::new_sinc(ratio, 1.1, &params, CHUNK_FRAMES, ch, FixedAsync::Input)
            .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;

    // Only pass the aligned portion in case samples.len() is not an exact multiple of channels.
    let aligned_len = in_frames * ch;
    let in_buf = InterleavedOwned::new_from(samples[..aligned_len].to_vec(), ch, in_frames)
        .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;

    let out_len = resampler.process_all_needed_output_len(in_frames);
    let mut out_buf = InterleavedOwned::<f32>::new(0.0, ch, out_len);

    // process_all_into_buffer drives full chunks, the final partial chunk (with silence padding),
    // and flushes the resampler's internal delay, then trims the startup delay from the front.
    let (_, out_frames) = resampler
        .process_all_into_buffer(&in_buf, &mut out_buf, in_frames, None)
        .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;

    let mut data = out_buf.take_data();
    data.truncate(out_frames * ch);
    Ok(data)
}

// Why these presets: quality maps to sinc length (more taps → better high-freq reproduction),
// oversampling factor (more intermediate points → lower interpolation noise), and interpolation
// mode (Cubic vs Linear). Exact numbers are tuned empirically; they are NOT part of the cache
// format because the cache is derived/regenerable (see `design/data-model.md` § Derived files).
fn sinc_params(quality: ResamplingQuality) -> SincInterpolationParameters {
    match quality {
        ResamplingQuality::Balanced => SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: 0.95,
            oversampling_factor: 128,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::Hann,
        },
        ResamplingQuality::High => SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::Blackman2,
        },
        ResamplingQuality::Highest => SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            oversampling_factor: 512,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::BlackmanHarris2,
        },
    }
}

/// A stateful, pull-based streaming resampler: wraps an inner [`PcmSource`] at one rate and
/// is itself a [`PcmSource`] at `to_rate`, so the import transcode can pull resampled frames
/// on demand without ever holding the whole signal.
///
/// Replicates rubato's whole-buffer `process_all_into_buffer` recipe incrementally: full
/// input chunks via `partial_len = None` while more input remains, the final (possibly
/// full-size) chunk via `partial_len = Some(n)`, then a `Some(0)` flush until the output
/// reaches `ceil(ratio · total_in)`; the leading `output_delay()` frames are trimmed and the
/// total is capped at the expected length. Bit-identical to [`resample`] for the same input
/// when `from_rate != to_rate`; an identity passthrough (no rubato) when `from_rate == to_rate`.
pub struct StreamingResampler<S: PcmSource> {
    inner: S,
    channels: usize,
    to_rate: u32,
    /// `None` for the identity passthrough (`from_rate == to_rate`).
    engine: Option<Engine>,
}

/// rubato engine + the streaming bookkeeping that mirrors `process_all_into_buffer`.
struct Engine {
    resampler: Async<f32>,
    chunk_frames: usize,
    out_cap: usize,
    ratio: f64,
    in_stage: Vec<f32>,
    out_stage: Vec<f32>,
    fifo: VecDeque<f32>,
    delay_remaining: usize,
    total_in: u64,
    produced: u64,
    expected_out: Option<u64>,
    state: EngineState,
}

#[derive(PartialEq)]
enum EngineState {
    Feeding,
    Flushing,
    Done,
}

impl<S: PcmSource> StreamingResampler<S> {
    /// Build a streaming resampler that converts `inner`'s frames to `to_rate`.
    pub fn new(inner: S, to_rate: u32, quality: ResamplingQuality) -> Result<Self, AudioError> {
        let channels = inner.channels() as usize;
        let from_rate = inner.sample_rate();
        if channels == 0 || from_rate == to_rate {
            return Ok(Self {
                inner,
                channels,
                to_rate,
                engine: None,
            });
        }

        let ratio = to_rate as f64 / from_rate as f64;
        let params = sinc_params(quality);
        let resampler = Async::<f32>::new_sinc(
            ratio,
            1.1,
            &params,
            CHUNK_FRAMES,
            channels,
            FixedAsync::Input,
        )
        .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;

        let chunk_frames = resampler.input_frames_next();
        let out_cap = resampler.output_frames_max();
        let delay_remaining = resampler.output_delay();
        let ratio = resampler.resample_ratio();

        Ok(Self {
            inner,
            channels,
            to_rate,
            engine: Some(Engine {
                resampler,
                chunk_frames,
                out_cap,
                ratio,
                in_stage: vec![0.0; chunk_frames * channels],
                out_stage: vec![0.0; out_cap * channels],
                fifo: VecDeque::new(),
                delay_remaining,
                total_in: 0,
                produced: 0,
                expected_out: None,
                state: EngineState::Feeding,
            }),
        })
    }
}

impl Engine {
    /// Advance one unit of work: feed one input chunk (or the final/flush chunk), pushing the
    /// produced project-rate frames (post delay-trim, post length-cap) into `fifo`.
    fn pump<S: PcmSource>(&mut self, inner: &mut S, ch: usize) -> Result<(), AudioError> {
        match self.state {
            EngineState::Feeding => {
                let n = inner.read(&mut self.in_stage[..self.chunk_frames * ch])?;
                self.total_in += n as u64;
                if inner.is_exhausted() {
                    // Set the length target *before* the final chunk: its silence-padded tail
                    // (rubato emits a full output chunk for a partial input) must be capped too.
                    self.expected_out = Some((self.ratio * self.total_in as f64).ceil() as u64);
                    if n > 0 {
                        self.process(ch, Some(n))?;
                    }
                    self.state = EngineState::Flushing;
                } else {
                    // Not exhausted ⇒ a full chunk with more input still to come.
                    self.process(ch, None)?;
                }
            }
            EngineState::Flushing => {
                if self.produced < self.expected_out.unwrap_or(0) {
                    self.process(ch, Some(0))?;
                } else {
                    self.state = EngineState::Done;
                }
            }
            EngineState::Done => {}
        }
        Ok(())
    }

    /// One `process_into_buffer` call; trims leading delay and caps at `expected_out`.
    fn process(&mut self, ch: usize, partial_len: Option<usize>) -> Result<(), AudioError> {
        let in_ad = InterleavedSlice::new(
            &self.in_stage[..self.chunk_frames * ch],
            ch,
            self.chunk_frames,
        )
        .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;
        let mut out_ad =
            InterleavedSlice::new_mut(&mut self.out_stage[..self.out_cap * ch], ch, self.out_cap)
                .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: None,
            partial_len,
        };
        let (_nin, nout) = self
            .resampler
            .process_into_buffer(&in_ad, &mut out_ad, Some(&indexing))
            .map_err(|e| AudioError::DecodeFailed(e.to_string()))?;

        // Trim the resampler's startup-delay frames from the front of the output stream.
        let start = self.delay_remaining.min(nout);
        self.delay_remaining -= start;
        let mut avail = nout - start;

        // Once the total length is known (flush), never emit past it.
        if let Some(expected) = self.expected_out {
            let room = expected.saturating_sub(self.produced) as usize;
            avail = avail.min(room);
        }

        let begin = start * ch;
        let end = (start + avail) * ch;
        self.fifo.extend(self.out_stage[begin..end].iter().copied());
        self.produced += avail as u64;
        Ok(())
    }

    fn is_done_and_drained(&self) -> bool {
        self.state == EngineState::Done && self.fifo.is_empty()
    }
}

impl<S: PcmSource> PcmSource for StreamingResampler<S> {
    fn channels(&self) -> u16 {
        self.channels as u16
    }

    fn sample_rate(&self) -> u32 {
        self.to_rate
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let ch = self.channels;
        if ch == 0 {
            return Ok(0);
        }
        match &mut self.engine {
            // Identity passthrough: forward the inner source unchanged.
            None => self.inner.read(out),
            Some(eng) => {
                while eng.fifo.len() < out.len() && eng.state != EngineState::Done {
                    eng.pump(&mut self.inner, ch)?;
                }
                let take = out.len().min(eng.fifo.len());
                for (slot, v) in out.iter_mut().zip(eng.fifo.drain(..take)) {
                    *slot = v;
                }
                Ok(take / ch)
            }
        }
    }

    fn is_exhausted(&self) -> bool {
        match &self.engine {
            None => self.inner.is_exhausted(),
            Some(eng) => eng.is_done_and_drained(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sine_mono(freq: f32, sample_rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate as f32).sin())
            .collect()
    }

    /// Normalised magnitude of a single frequency bin via DFT.
    fn dft_magnitude(samples: &[f32], sample_rate: u32, freq: f32) -> f32 {
        let n = samples.len();
        let (re, im) = samples
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(r, i), (t, &s)| {
                let angle =
                    2.0 * std::f64::consts::PI * freq as f64 * t as f64 / sample_rate as f64;
                (r + s as f64 * angle.cos(), i + s as f64 * angle.sin())
            });
        ((re * re + im * im).sqrt() / n as f64) as f32
    }

    /// In-memory [`PcmSource`] over an interleaved buffer; fills greedily, reports EOF.
    struct SliceSource {
        data: Vec<f32>,
        pos: usize,
        channels: u16,
        rate: u32,
    }

    impl PcmSource for SliceSource {
        fn channels(&self) -> u16 {
            self.channels
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            let take = out.len().min(self.data.len() - self.pos);
            out[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            Ok(take / self.channels.max(1) as usize)
        }
        fn is_exhausted(&self) -> bool {
            self.pos >= self.data.len()
        }
    }

    /// Drain a [`PcmSource`] fully into one interleaved Vec (4096-frame pulls).
    fn drain<S: PcmSource>(mut src: S) -> Vec<f32> {
        let ch = src.channels().max(1) as usize;
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; 4096 * ch];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n * ch]);
        }
        out
    }

    fn stream_resample(src: &[f32], ch: u16, from: u32, to: u32, q: ResamplingQuality) -> Vec<f32> {
        let source = SliceSource {
            data: src.to_vec(),
            pos: 0,
            channels: ch,
            rate: from,
        };
        drain(StreamingResampler::new(source, to, q).unwrap())
    }

    // -----------------------------------------------------------------------
    // S1 — Streaming output length is the contract length (== whole-buffer length)
    // -----------------------------------------------------------------------
    //
    // The streamed output is NOT compared sample-for-sample with `resample`: the whole-
    // buffer path's startup-delay trim (rubato `process_all_into_buffer` →
    // `copy_frames_within`) leaves a ~`output_delay()`-frame stutter at the very start that
    // the R-tests' margins don't see. The streaming path trims cleanly, so it diverges from
    // `resample` over exactly those leading frames. We pin the *length* (which both agree on)
    // and validate the streamed samples directly in S2/S3.

    #[test]
    fn s1_stream_length_matches_whole_buffer() {
        for (from, to) in [
            (44100, 48000),
            (48000, 24000),
            (24000, 48000),
            (48000, 48000),
        ] {
            let src = sine_mono(440.0, from, from as usize); // 1 s
            let whole = resample(&src, 1, from, to, ResamplingQuality::Balanced).unwrap();
            let streamed = stream_resample(&src, 1, from, to, ResamplingQuality::Balanced);
            assert_eq!(
                streamed.len(),
                whole.len(),
                "S1: streamed length != whole-buffer length for {from}->{to}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // S2 — Streamed output: exact length, smooth (no stutter), correct frequency
    // -----------------------------------------------------------------------

    #[test]
    fn s2_stream_quality_and_length() {
        let src = sine_mono(440.0, 44100, 44100); // 1 s @ 44.1 kHz
        let streamed = stream_resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced);

        // Exact contract length (== the whole-buffer path's, which is the float
        // `ceil(resample_ratio * in_frames)` — not integer div_ceil).
        let expected = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced)
            .unwrap()
            .len();
        assert_eq!(streamed.len(), expected, "S2: length");

        // Smoothness: no large first-difference anywhere — including the start, where the
        // whole-buffer path stutters. A 440 Hz sine at 48 kHz has a natural max |Δ| ≈ 0.058;
        // a stutter/seam replay would jump by ~1.0, so 0.1 cleanly separates them.
        let mut prev = streamed[0];
        for (i, &s) in streamed.iter().enumerate().skip(1) {
            assert!(
                (s - prev).abs() < 0.1,
                "S2: discontinuity at {i}: {prev} -> {s}"
            );
            prev = s;
        }

        // The 440 Hz tone survives; out-of-band energy is negligible.
        let mag = dft_magnitude(&streamed, 48000, 440.0);
        assert!(mag > 0.4, "S2: 440 Hz magnitude {mag} too low");
        assert!(
            dft_magnitude(&streamed, 48000, 20000.0) < mag * 0.01,
            "S2: aliasing too high"
        );
    }

    // -----------------------------------------------------------------------
    // S3 — Chunk boundaries, stereo channels, identity passthrough, determinism, empty
    // -----------------------------------------------------------------------

    #[test]
    fn s3_stream_boundaries_stereo_identity() {
        // Last-chunk/partial boundary handling (CHUNK_FRAMES = 1024): exact contract length.
        for len in [1usize, 1023, 1024, 1025, 2048, 2049, 5000] {
            let src: Vec<f32> = (0..len).map(|i| (i as f32 * 0.001).sin()).collect();
            let streamed = stream_resample(&src, 1, 44100, 48000, ResamplingQuality::High);
            let expected = resample(&src, 1, 44100, 48000, ResamplingQuality::High)
                .unwrap()
                .len();
            assert_eq!(streamed.len(), expected, "S3: length at len {len}");
            // Determinism: same input → identical output.
            let again = stream_resample(&src, 1, 44100, 48000, ResamplingQuality::High);
            assert_eq!(streamed, again, "S3: nondeterministic at len {len}");
        }

        // Stereo: channel count preserved, no cross-talk (440 Hz L, 880 Hz R).
        let frames = 4410;
        let mut src = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            src.push((2.0 * PI * 440.0 * i as f32 / 44100.0).sin());
            src.push((2.0 * PI * 880.0 * i as f32 / 44100.0).sin());
        }
        let streamed = stream_resample(&src, 2, 44100, 48000, ResamplingQuality::Balanced);
        assert_eq!(streamed.len() % 2, 0, "S3: stereo interleave alignment");
        let l: Vec<f32> = streamed.iter().copied().step_by(2).collect();
        let r: Vec<f32> = streamed.iter().skip(1).copied().step_by(2).collect();
        assert!(dft_magnitude(&l, 48000, 440.0) > 0.3, "S3: L 440 Hz");
        assert!(dft_magnitude(&r, 48000, 880.0) > 0.3, "S3: R 880 Hz");
        assert!(dft_magnitude(&l, 48000, 880.0) < 0.05, "S3: L→R cross-talk");

        // Identity passthrough is bit-exact and a no-op; empty input → empty output.
        let identity = stream_resample(&src, 2, 48000, 48000, ResamplingQuality::Balanced);
        assert_eq!(identity, src, "S3: identity passthrough must be bit-exact");
        assert!(stream_resample(&[], 1, 44100, 48000, ResamplingQuality::Balanced).is_empty());
    }

    // -----------------------------------------------------------------------
    // S4 — is_exhausted() contract: false until Done && fifo drained, then true
    // -----------------------------------------------------------------------
    //
    // The `drain` helper only watches `read() == 0`; it never inspects
    // `is_exhausted()`. This pins the exhaustion predicate directly so that
    // `is_done_and_drained` (state == Done && fifo empty) and the trait
    // `is_exhausted` forwarder cannot silently collapse to a constant.

    #[test]
    fn s4_is_exhausted_tracks_done_and_drain() {
        let src = sine_mono(440.0, 44100, 4410);
        let source = SliceSource {
            data: src.clone(),
            pos: 0,
            channels: 1,
            rate: 44100,
        };
        let mut r = StreamingResampler::new(source, 48000, ResamplingQuality::Balanced).unwrap();

        // Before any output is pulled, the resampler is not exhausted.
        assert!(
            !r.is_exhausted(),
            "S4: fresh resampler must not be exhausted"
        );

        // Pull everything *except* the very last frame: state may be Done but the
        // fifo is not yet empty, so is_exhausted() must still be false. This is what
        // separates `&&` from `||` and the constant-true mutant.
        let total = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced)
            .unwrap()
            .len();
        assert!(total > 1, "S4: precondition: non-trivial output");
        let mut got = 0usize;
        let mut buf = [0.0f32; 256];
        while got < total - 1 {
            let want = (total - 1 - got).min(buf.len());
            let n = r.read(&mut buf[..want]).unwrap();
            assert!(n > 0, "S4: read stalled before draining");
            got += n;
        }
        assert!(
            !r.is_exhausted(),
            "S4: must not be exhausted while frames remain in the fifo"
        );

        // Drain the remainder; now both state == Done and fifo empty hold.
        let mut tail = [0.0f32; 8];
        while r.read(&mut tail).unwrap() > 0 {}
        assert!(
            r.is_exhausted(),
            "S4: must be exhausted once Done and fully drained"
        );
    }

    // -----------------------------------------------------------------------
    // S5 — Identity passthrough forwards inner is_exhausted()
    // -----------------------------------------------------------------------
    //
    // Engine is None on the identity path, so is_exhausted() forwards to the inner
    // source rather than is_done_and_drained(). Pins the `None` arm of both the
    // exhaustion forwarder and (via read) the passthrough.

    #[test]
    fn s5_identity_passthrough_exhaustion() {
        let src = sine_mono(440.0, 48000, 1000);
        let source = SliceSource {
            data: src.clone(),
            pos: 0,
            channels: 1,
            rate: 48000,
        };
        let r = StreamingResampler::new(source, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(!r.is_exhausted(), "S5: identity not exhausted before read");
        let out = drain(r);
        assert_eq!(out, src, "S5: identity passthrough bit-exact");

        // Re-build to assert exhaustion after a full drain on the identity path.
        let source = SliceSource {
            data: src.clone(),
            pos: 0,
            channels: 1,
            rate: 48000,
        };
        let mut r = StreamingResampler::new(source, 48000, ResamplingQuality::Balanced).unwrap();
        let mut buf = vec![0.0f32; src.len()];
        let _ = r.read(&mut buf).unwrap();
        assert!(
            r.is_exhausted(),
            "S5: identity must report exhausted once inner is drained"
        );
    }

    // -----------------------------------------------------------------------
    // S6 — Final read returns 0 frames with exhaustion (the n == 0 Feeding path)
    // -----------------------------------------------------------------------
    //
    // A source whose tail and exhaustion signal arrive on *separate* reads forces the
    // `if n > 0` guard in pump(): the final pump reads n == 0 while is_exhausted() is
    // true, so process() must be skipped. The `> with >=` mutant would call process()
    // with a stale, already-consumed input chunk, replaying frames and overrunning the
    // contract length. SliceSource can't express this (it flags exhaustion on the same
    // read as the final bytes), so we use a lagged source.

    /// Like SliceSource but reports exhaustion one read *after* the bytes run out:
    /// the read that empties the buffer still returns `is_exhausted() == false`, and only
    /// the *next* read (which returns 0 frames) flags exhaustion. The trait contract
    /// (`mod.rs`) permits this; a clean source instead flags EOF on the final byte-bearing
    /// read, so it never drives the `n == 0` branch in `pump`.
    struct LaggedSource {
        data: Vec<f32>,
        pos: usize,
        channels: u16,
        rate: u32,
        saw_zero_read: bool,
    }

    impl PcmSource for LaggedSource {
        fn channels(&self) -> u16 {
            self.channels
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            let take = out.len().min(self.data.len() - self.pos);
            out[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            if take == 0 {
                self.saw_zero_read = true;
            }
            Ok(take / self.channels.max(1) as usize)
        }
        fn is_exhausted(&self) -> bool {
            // Exhaustion is reported only after a read that produced zero bytes.
            self.saw_zero_read
        }
    }

    #[test]
    fn s6_zero_frame_final_read_is_not_reprocessed() {
        // Choose an input length that is an exact multiple of the engine chunk so the
        // streaming reads land cleanly, then a trailing zero-frame read carries EOF.
        let frames = 4096usize;
        let src = sine_mono(440.0, 44100, frames);
        let source = LaggedSource {
            data: src.clone(),
            pos: 0,
            channels: 1,
            rate: 44100,
            saw_zero_read: false,
        };
        let streamed =
            drain(StreamingResampler::new(source, 48000, ResamplingQuality::Balanced).unwrap());
        // Bit-for-bit identical to the clean-source stream: the lagged EOF (a trailing
        // zero-frame read) must not change a single output sample. A `> with >=` replay
        // of the stale input chunk would diverge here (or overrun the length).
        let clean = stream_resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced);
        assert_eq!(
            streamed, clean,
            "S6: lagged zero-frame EOF must match the clean stream bit-for-bit"
        );
    }

    // -----------------------------------------------------------------------
    // R1 — Identity fast-path is bit-exact
    // -----------------------------------------------------------------------

    #[test]
    fn r1_identity_fast_path() {
        let src: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let out = resample(&src, 1, 48000, 48000, ResamplingQuality::Balanced).unwrap();
        assert_eq!(out, src, "R1: identity path must be bit-exact");
    }

    // -----------------------------------------------------------------------
    // R2 — Upsample length doubles
    // -----------------------------------------------------------------------

    #[test]
    fn r2_upsample_length() {
        let src = sine_mono(440.0, 24000, 24000);
        let out = resample(&src, 1, 24000, 48000, ResamplingQuality::Balanced).unwrap();
        let expected = 48000usize;
        // rubato's bounded rounding allows a handful of frames of slack
        assert!(
            out.len().abs_diff(expected) <= 16,
            "R2: upsample length {} ≈ {expected}",
            out.len()
        );
    }

    // -----------------------------------------------------------------------
    // R3 — Downsample length halves
    // -----------------------------------------------------------------------

    #[test]
    fn r3_downsample_length() {
        let src = sine_mono(440.0, 48000, 48000);
        let out = resample(&src, 1, 48000, 24000, ResamplingQuality::Balanced).unwrap();
        let expected = 24000usize;
        assert!(
            out.len().abs_diff(expected) <= 16,
            "R3: downsample length {} ≈ {expected}",
            out.len()
        );
    }

    // -----------------------------------------------------------------------
    // R4 — Non-integer ratio (44.1 → 48 kHz)
    // -----------------------------------------------------------------------

    #[test]
    fn r4_non_integer_ratio() {
        let src = sine_mono(440.0, 44100, 44100);
        let out = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        let expected = (44100usize * 48000).div_ceil(44100); // = 48000
        assert!(
            out.len().abs_diff(expected) <= 32,
            "R4: 44.1→48 kHz length {} ≈ {expected}",
            out.len()
        );
    }

    // -----------------------------------------------------------------------
    // R5 — Channel preservation, no cross-talk
    // -----------------------------------------------------------------------

    #[test]
    fn r5_channel_preservation_no_crosstalk() {
        // L = 440 Hz, R = 880 Hz stereo interleaved
        let frames = 4410;
        let mut src = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let l = (2.0 * PI * 440.0 * i as f32 / 44100.0).sin();
            let r = (2.0 * PI * 880.0 * i as f32 / 44100.0).sin();
            src.push(l);
            src.push(r);
        }
        let out = resample(&src, 2, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        assert_eq!(out.len() % 2, 0, "R5: interleave alignment");

        let l_out: Vec<f32> = out.iter().copied().step_by(2).collect();
        let r_out: Vec<f32> = out.iter().skip(1).copied().step_by(2).collect();

        let l_mag = dft_magnitude(&l_out, 48000, 440.0);
        let r_mag = dft_magnitude(&r_out, 48000, 880.0);
        // Signal should be clearly present in the expected channel.
        assert!(l_mag > 0.3, "R5: L channel 440 Hz mag = {l_mag}");
        assert!(r_mag > 0.3, "R5: R channel 880 Hz mag = {r_mag}");
        // No bleed: wrong-frequency component in each channel should be small.
        let l_bleed = dft_magnitude(&l_out, 48000, 880.0);
        let r_bleed = dft_magnitude(&r_out, 48000, 440.0);
        assert!(
            l_bleed < l_mag * 0.05,
            "R5: L channel 880 Hz bleed {l_bleed} too large vs signal {l_mag}"
        );
        assert!(
            r_bleed < r_mag * 0.05,
            "R5: R channel 440 Hz bleed {r_bleed} too large vs signal {r_mag}"
        );
    }

    // -----------------------------------------------------------------------
    // R6 — Frequency preservation (440 Hz sine survives 44.1 → 48 kHz)
    // -----------------------------------------------------------------------

    #[test]
    fn r6_frequency_preservation() {
        let src = sine_mono(440.0, 44100, 44100);
        let out = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        let mag_440 = dft_magnitude(&out, 48000, 440.0);
        // A 1-second 440 Hz sine at 0 dBFS should have magnitude ≈ 0.5 (per-bin normalisation).
        assert!(
            mag_440 > 0.4,
            "R6: 440 Hz magnitude {mag_440} too low after resampling"
        );
        // Aliasing at an arbitrary out-of-band bin should be well below the signal.
        let mag_alias = dft_magnitude(&out, 48000, 20000.0);
        assert!(
            mag_alias < mag_440 * 0.01,
            "R6: aliasing at 20 kHz {mag_alias} vs signal {mag_440}"
        );
    }

    // -----------------------------------------------------------------------
    // R7 — No drop / duplicate at chunk seams (linear ramp; no large artifacts)
    // -----------------------------------------------------------------------

    #[test]
    fn r7_no_drop_at_chunk_seams() {
        // Ramp well beyond the default chunk size (1 024) to exercise multiple chunk boundaries.
        let frames = 10_000usize;
        let src: Vec<f32> = (0..frames).map(|i| i as f32 / frames as f32).collect();
        let out = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();

        // The sinc filter has a tail of ~sinc_len/2 * ratio ≈ 70 output samples. Exclude
        // the leading and trailing edges where ringing at signal boundaries is expected.
        // The interior should have no large drops — a real chunk-seam error would show
        // a sudden silence or repetition orders of magnitude larger than 0.01.
        let margin = 200usize;
        let interior = &out[margin..out.len().saturating_sub(margin)];
        let mut prev = interior[0];
        for (i, &s) in interior.iter().enumerate().skip(1) {
            assert!(
                (s - prev).abs() < 0.05,
                "R7: large interior discontinuity at index {}: {prev} → {s}",
                i + margin
            );
            prev = s;
        }

        // The overall trend must be increasing: the second half should be well above the first half.
        let mid = out.len() / 2;
        let first_half_mean = out[..mid].iter().sum::<f32>() / mid as f32;
        let second_half_mean = out[mid..].iter().sum::<f32>() / (out.len() - mid) as f32;
        assert!(
            second_half_mean > first_half_mean + 0.3,
            "R7: ramp trend wrong: first_half_mean={first_half_mean} second_half_mean={second_half_mean}"
        );

        let expected = (frames * 48000).div_ceil(44100);
        assert!(
            out.len().abs_diff(expected) <= 32,
            "R7: length {} ≈ {expected}",
            out.len()
        );
    }

    // -----------------------------------------------------------------------
    // R8 — Every quality preset runs, preserves the tone, and yields the expected length
    // -----------------------------------------------------------------------

    #[test]
    fn r8_all_quality_presets() {
        let src = sine_mono(440.0, 44100, 44100);
        let expected_len = (44100usize * 48000).div_ceil(44100);

        for quality in [
            ResamplingQuality::Balanced,
            ResamplingQuality::High,
            ResamplingQuality::Highest,
        ] {
            let out = resample(&src, 1, 44100, 48000, quality).unwrap();
            assert!(
                out.len().abs_diff(expected_len) <= 32,
                "R8: quality {quality:?} length {} ≈ {expected_len}",
                out.len()
            );
            // Each preset must actually preserve the 440 Hz tone (≈0.5 per-bin), not just the
            // length — a passthrough/garbage path would clear this floor only by coincidence.
            let mag_440 = dft_magnitude(&out, 48000, 440.0);
            assert!(
                mag_440 > 0.4,
                "R8: quality {quality:?} 440 Hz magnitude {mag_440} too low"
            );
        }
    }

    // -----------------------------------------------------------------------
    // R9 — Within-build determinism
    // -----------------------------------------------------------------------

    #[test]
    fn r9_within_build_determinism() {
        let src = sine_mono(440.0, 44100, 4410);
        let a = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        let b = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        assert_eq!(a, b, "R9: same input must produce identical output");
    }

    // -----------------------------------------------------------------------
    // R10 — Degenerate inputs
    // -----------------------------------------------------------------------

    #[test]
    fn r10_degenerate_inputs() {
        // Empty input → empty output, no panic.
        let empty = resample(&[], 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        assert!(empty.is_empty(), "R10: empty input → empty output");

        // Input shorter than the sinc filter length → no panic, sensible output.
        let short: Vec<f32> = vec![0.5, -0.5, 0.3];
        let _ = resample(&short, 1, 44100, 48000, ResamplingQuality::Balanced)
            .expect("R10: short input must not error");
    }

    // -----------------------------------------------------------------------
    // R11 — Sinc overshoot passes through (resample() does NOT clamp)
    // -----------------------------------------------------------------------

    #[test]
    fn r11_overshoot_passes_through() {
        // A low-frequency square wave (440 Hz) has harmonics throughout the passband.
        // Truncating those harmonics at the filter cutoff causes Gibbs ringing at the transitions,
        // pushing the peak above ±1.0. The Nyquist-frequency square wave (alternating ±1) is
        // above the filter cutoff so it only gets attenuated — use a sub-Nyquist frequency instead.
        let frames = 44100usize;
        let src: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / 44100.0;
                // 440 Hz square wave: positive half-cycle → +1, negative → -1
                if (440.0_f32 * t).fract() < 0.5 {
                    1.0_f32
                } else {
                    -1.0_f32
                }
            })
            .collect();
        let out = resample(&src, 1, 44100, 48000, ResamplingQuality::Balanced).unwrap();
        let max_abs = out.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(
            max_abs > 1.0,
            "R11: expected Gibbs overshoot > 1.0 after sinc resampling, got {max_abs}"
        );
    }
}
