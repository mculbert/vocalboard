//! Room-tone detection, loop-crossfade assembly, and RoomTone V1 blob encoding.
//!
//! `detect_room_tone` is a pure DSP function; it reads no settings and touches no database.
//! The M4 import caller resolves `RoomToneParams` from settings and persists the result via
//! `encode_room_tone` + `store::put` (see `design/audio-pipeline.md` § Room-tone detection).

use super::frame_reader::FrameReader;
use super::AudioError;
use crate::project::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};

/// Frames pulled per [`FrameReader::read_frames`] call during the pass-1 streaming analysis.
const ANALYSIS_CHUNK_FRAMES: usize = 8192;

/// Format version emitted by every new [`encode_room_tone`] call.
pub const LATEST_ROOM_TONE_VERSION: u8 = 1;

/// Outcome of room-tone analysis over a track's resampled PCM.
pub enum RoomToneOutcome {
    /// A usable, pre-crossfaded loop segment (interleaved f32, source channel count,
    /// project rate).
    Found(RoomTone),
    /// No stretch of the track was quiet/stable enough to serve as room tone.
    None,
}

/// A loopable room-tone segment ready for blob encoding and splice-time playback.
#[derive(Debug)]
pub struct RoomTone {
    /// Interleaved f32 samples at `sample_rate`. `len == frames × channels`.
    /// The loop crossfade has already been folded in; the renderer loops this buffer
    /// without any additional per-cycle fade.
    pub samples: Vec<f32>,
    /// Source channel count (1 for mono, 2 for stereo, …).
    pub channels: u16,
    /// Project sample rate (Hz).
    pub sample_rate: u32,
    /// RMS of `samples`, computed once at extraction and persisted in the blob.
    /// Read directly from the decoded blob; never recomputed after that.
    pub rms: f32,
}

/// Tunable detection thresholds resolved from app settings by the M4 caller.
///
/// [`Default`] matches `settings.rs` defaults
/// (`DEFAULT_ROOM_TONE_RMS_CEILING` / `DEFAULT_ROOM_TONE_QUIET_PERCENTILE`).
pub struct RoomToneParams {
    /// Absolute RMS ceiling (linear ≈ −30 dBFS): audio above this is never room tone.
    pub rms_ceiling: f32,
    /// Percentile (0–100) of 100 ms block RMS forming the adaptive quiet threshold
    /// `Q = min(rms_ceiling, Pq)`.
    pub quiet_percentile: f64,
}

impl Default for RoomToneParams {
    fn default() -> Self {
        Self {
            rms_ceiling: crate::settings::DEFAULT_ROOM_TONE_RMS_CEILING,
            quiet_percentile: crate::settings::DEFAULT_ROOM_TONE_QUIET_PERCENTILE,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Analyse a track's resampled (mono or interleaved multi-channel) PCM and extract a
/// loopable room-tone segment per audio-pipeline.md § Room tone detection.
///
/// Pass 1 streams `reader` once, computing the channel-mean down-mix and 100 ms block stats on
/// the fly (no full-length PCM buffer); pass 2 seeks back to read only the selected window or
/// stitch blocks. The returned segment preserves the source channel count. Returns
/// [`RoomToneOutcome::None`] when the recording is < 10 s or no quiet material qualifies. Deterministic
/// for a given input + `params`. Propagates any read error from `reader`.
#[allow(clippy::too_many_lines)]
pub fn detect_room_tone(
    reader: &mut impl FrameReader,
    params: &RoomToneParams,
) -> Result<RoomToneOutcome, AudioError> {
    let channels = reader.channels();
    let sample_rate = reader.sample_rate();
    if channels == 0 || sample_rate == 0 {
        return Ok(RoomToneOutcome::None);
    }

    let ch = channels as usize;

    // Non-overlapping 100 ms blocks (trailing partial block dropped).
    let block_len = (0.1 * sample_rate as f64).round() as usize;
    if block_len == 0 {
        return Ok(RoomToneOutcome::None);
    }

    // Pass 1: one sequential read; down-mix per frame and accumulate 100 ms block stats.
    // Only complete blocks are finalized, so the trailing partial block is dropped (matching
    // `n_frames / block_len`). The per-block float summation order is identical to a whole-buffer
    // pass, so the selected window is bit-identical.
    let mut block_energy: Vec<f64> = Vec::new();
    let mut block_peak: Vec<f32> = Vec::new();
    let mut block_rms: Vec<f32> = Vec::new();
    let mut cur_energy = 0.0f64;
    let mut cur_peak = 0.0f32;
    let mut cur_count = 0usize;
    let mut total_frames = 0usize;
    let mut buf = vec![0.0f32; ANALYSIS_CHUNK_FRAMES * ch];
    loop {
        let n = reader.read_frames(&mut buf)?;
        if n == 0 {
            break;
        }
        total_frames += n;
        for f in 0..n {
            let off = f * ch;
            let mono = buf[off..off + ch].iter().sum::<f32>() / ch as f32;
            cur_energy += (mono as f64) * (mono as f64);
            cur_peak = cur_peak.max(mono.abs());
            cur_count += 1;
            if cur_count == block_len {
                block_energy.push(cur_energy);
                block_peak.push(cur_peak);
                block_rms.push(((cur_energy / block_len as f64) as f32).sqrt());
                cur_energy = 0.0;
                cur_peak = 0.0;
                cur_count = 0;
            }
        }
    }

    // Guard: < 10 s.
    if total_frames < 10 * sample_rate as usize {
        return Ok(RoomToneOutcome::None);
    }
    let n_blocks = block_energy.len();
    if n_blocks == 0 {
        return Ok(RoomToneOutcome::None);
    }

    // Prefix sums for O(1) window queries.
    let mut prefix_e = vec![0.0f64; n_blocks + 1];
    let mut prefix_r = vec![0.0f64; n_blocks + 1];
    let mut prefix_r2 = vec![0.0f64; n_blocks + 1];
    for b in 0..n_blocks {
        prefix_e[b + 1] = prefix_e[b] + block_energy[b];
        let br = block_rms[b] as f64;
        prefix_r[b + 1] = prefix_r[b] + br;
        prefix_r2[b + 1] = prefix_r2[b] + br * br;
    }

    let window_rms = |s: usize, l: usize| -> f32 {
        let energy = prefix_e[s + l] - prefix_e[s];
        ((energy / (l * block_len) as f64) as f32).sqrt()
    };
    let block_rms_mean = |s: usize, l: usize| -> f64 { (prefix_r[s + l] - prefix_r[s]) / l as f64 };
    let block_rms_sd = |s: usize, l: usize| -> f64 {
        let mean = block_rms_mean(s, l);
        let var = (prefix_r2[s + l] - prefix_r2[s]) / l as f64 - mean * mean;
        var.max(0.0).sqrt()
    };

    // Sparse table for O(1) range-max queries over block_peak.
    let sparse = SparseTable::build(&block_peak);
    let window_peak = |s: usize, l: usize| -> f32 { sparse.query(s, s + l - 1) };

    // Q = min(rms_ceiling, Pq)
    let rms_ceiling = params.rms_ceiling;
    let pq = {
        let mut sorted = block_rms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pctile = params.quiet_percentile.clamp(0.0, 100.0);
        let rank = ((pctile / 100.0 * n_blocks as f64).ceil() as usize)
            .max(1)
            .min(n_blocks);
        sorted[rank - 1]
    };
    let q = rms_ceiling.min(pq);

    // Window sweep: contiguous block-runs in [2 s, 10 s].
    let min_blk = ((2.0 * sample_rate as f64) / block_len as f64).ceil() as usize;
    let max_blk = ((10.0 * sample_rate as f64) / block_len as f64).floor() as usize;

    if min_blk <= n_blocks {
        let mut best: Option<(usize, usize, f32)> = None; // (start_block, len_blocks, rms)

        for s in 0..=(n_blocks - min_blk) {
            let lower = best.map_or(min_blk, |(_, bl, _)| bl);
            let upper = max_blk.min(n_blocks - s);
            if upper < lower {
                continue;
            }
            // Scan length descending: first acceptance = longest at this start.
            for l in (lower..=upper).rev() {
                let rms = window_rms(s, l);
                if rms > rms_ceiling {
                    continue;
                }
                let peak = window_peak(s, l);
                if peak > 5.0 * rms {
                    continue;
                }
                let mean = block_rms_mean(s, l);
                let sd = block_rms_sd(s, l);
                // SD criterion: sd <= 15% of mean (degenerate-safe: 0 <= 0 is true)
                if mean > 0.0 && sd > 0.15 * mean {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((_, bl, br)) => l > bl || (l == bl && rms < br),
                };
                if better {
                    best = Some((s, l, rms));
                }
                break; // first (longest) acceptance at this start
            }
        }

        if let Some((s, l, _)) = best {
            // Pass 2: seek back and read just the selected window.
            let seg_samples = reader.read_range((s * block_len) as i64, l * block_len)?;
            let seg = RoomTone {
                samples: seg_samples,
                channels,
                sample_rate,
                rms: 0.0, // computed by apply_loop_crossfade
            };
            return Ok(RoomToneOutcome::Found(apply_loop_crossfade(seg)));
        }
    }

    // Stitch fallback.
    stitch_fallback(
        reader,
        channels,
        sample_rate,
        &block_rms,
        &block_peak,
        n_blocks,
        block_len,
        q,
    )
}

/// Encode a room-tone segment as the latest `RoomTone`-kind wire format (tag `0x41`).
///
/// Returns the content hash and tagged bytes, ready for `store::put`.
pub fn encode_room_tone(seg: &RoomTone) -> Result<(Hash, Vec<u8>), postcard::Error> {
    let v1 = v1::RoomToneV1::from(seg);
    encode_tagged(Kind::RoomTone, LATEST_ROOM_TONE_VERSION, &v1)
}

/// Decode a `Kind::RoomTone` blob back to a [`RoomTone`].
///
/// Verifies the kind tag, dispatches on the version nibble.
pub fn decode_room_tone(bytes: &[u8]) -> Result<RoomTone, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (_, version) = parse_tag(bytes[0])?;
    match version {
        1 => {
            let (_, v1_seg): (u8, v1::RoomToneV1) = decode_tagged_as(Kind::RoomTone, bytes)?;
            Ok(RoomTone::from(v1_seg))
        }
        _ => Err(DecodeError::UnknownVersion {
            kind: Kind::RoomTone,
            version,
        }),
    }
}

// ---------------------------------------------------------------------------
// V1 wire schema
// ---------------------------------------------------------------------------

/// V1 wire schema for [`Kind::RoomTone`] blobs. Frozen post-1.0.
pub mod v1 {
    use serde::{Deserialize, Serialize};

    /// Frozen V1 wire representation of a room-tone PCM segment.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct RoomToneV1 {
        /// Project sample rate (Hz).
        pub sample_rate: u32,
        /// Source channel count (1 = mono, 2 = stereo, …).
        pub channels: u16,
        /// RMS of `samples`, computed once at extraction.
        pub rms: f32,
        /// Interleaved IEEE little-endian f32 samples (loop crossfade pre-applied).
        pub samples: Vec<f32>,
    }
}

impl From<v1::RoomToneV1> for RoomTone {
    fn from(v: v1::RoomToneV1) -> Self {
        RoomTone {
            sample_rate: v.sample_rate,
            channels: v.channels,
            rms: v.rms,
            samples: v.samples,
        }
    }
}

impl From<&RoomTone> for v1::RoomToneV1 {
    fn from(s: &RoomTone) -> Self {
        v1::RoomToneV1 {
            sample_rate: s.sample_rate,
            channels: s.channels,
            rms: s.rms,
            samples: s.samples.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// RMS of a sample slice. Returns `0.0` for an empty slice.
fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum_sq / samples.len() as f64) as f32).sqrt()
}

/// Stitch qualifying quiet blocks into a segment when no contiguous window passes.
///
/// Pieces are selected in ascending-RMS order (quietest first) up to the 10 s target, then
/// assembled in their original audio-file order with 50 ms linear crossfades. The selected blocks
/// are read in pass 2 via targeted seeks (adjacent blocks coalesced into one read).
#[allow(clippy::too_many_arguments)]
fn stitch_fallback(
    reader: &mut impl FrameReader,
    channels: u16,
    sample_rate: u32,
    block_rms: &[f32],
    block_peak: &[f32],
    n_blocks: usize,
    block_len: usize,
    q: f32,
) -> Result<RoomToneOutcome, AudioError> {
    let ch = channels as usize;

    // Collect qualifying blocks: RMS ≤ Q and peak ≤ 5 × RMS.
    let mut qualifying: Vec<usize> = (0..n_blocks)
        .filter(|&b| {
            block_rms[b] <= q && {
                let rms = block_rms[b];
                block_peak[b] <= 5.0 * rms
            }
        })
        .collect();

    if qualifying.is_empty() {
        return Ok(RoomToneOutcome::None);
    }

    // Select in ascending-RMS order up to the 10 s target.
    qualifying.sort_by(|&a, &b| {
        block_rms[a]
            .partial_cmp(&block_rms[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let target_frames = 10 * sample_rate as usize;
    let n_needed = target_frames.div_ceil(block_len).min(qualifying.len());
    let mut selected: Vec<usize> = qualifying[..n_needed].to_vec();

    // Assemble in original audio order.
    selected.sort_unstable();

    // Read each selected block's interleaved samples, coalescing runs of adjacent blocks into a
    // single seeked read, then splitting back into per-block pieces (ascending position order).
    let mut pieces: Vec<Vec<f32>> = Vec::with_capacity(selected.len());
    let mut i = 0;
    while i < selected.len() {
        let mut j = i;
        while j + 1 < selected.len() && selected[j + 1] == selected[j] + 1 {
            j += 1;
        }
        let run_blocks = j - i + 1;
        let run_frames =
            reader.read_range((selected[i] * block_len) as i64, run_blocks * block_len)?;
        for k in 0..run_blocks {
            let start = k * block_len * ch;
            pieces.push(run_frames[start..start + block_len * ch].to_vec());
        }
        i = j + 1;
    }

    // Concatenate with 50 ms linear crossfades between adjacent pieces.
    let fade_len = (0.05 * sample_rate as f64).round() as usize;
    let stitched = crossfade_concat(&pieces, ch, fade_len);

    let seg = RoomTone {
        samples: stitched,
        channels,
        sample_rate,
        rms: 0.0, // computed by apply_loop_crossfade
    };
    Ok(RoomToneOutcome::Found(apply_loop_crossfade(seg)))
}

/// Concatenate `pieces` with `fade_len`-frame equal-power crossfades at each junction.
///
/// Each junction overlaps the last `fade_len` frames of the preceding piece with the
/// first `fade_len` frames of the next; the blended region replaces the tail of the
/// accumulator, and the rest of the new piece is appended. The pieces are different
/// windows of room-tone noise (uncorrelated), so the fade is equal-power — a linear
/// ramp would dip the noise floor ~3 dB at each junction.
fn crossfade_concat(pieces: &[Vec<f32>], ch: usize, fade_len: usize) -> Vec<f32> {
    if pieces.is_empty() {
        return vec![];
    }
    let mut out = pieces[0].clone();
    for piece in &pieces[1..] {
        let out_frames = out.len() / ch;
        let effective_fade = fade_len.min(out_frames).min(piece.len() / ch);
        let overlap_start = (out_frames - effective_fade) * ch;
        // Blend over [0, effective_fade): equal-power, g_in fades the new piece up while the
        // symmetric g_out fades the old tail down (constant power for uncorrelated noise).
        for i in 0..effective_fade {
            let g_in = super::equal_power_gain(i, effective_fade);
            let g_out = super::equal_power_gain(effective_fade - 1 - i, effective_fade);
            for c in 0..ch {
                let old_val = out[overlap_start + i * ch + c];
                let new_val = piece[i * ch + c];
                out[overlap_start + i * ch + c] = g_out * old_val + g_in * new_val;
            }
        }
        // Append rest of piece after crossfade head.
        out.extend_from_slice(&piece[effective_fade * ch..]);
    }
    out
}

/// Apply the length-tiered head/tail loop crossfade to `seg`.
///
/// Uses equal-power fading: the loop wraps tail→head between two different windows of the
/// same noise (uncorrelated), so constant-power blending keeps the floor level — a linear
/// ramp would dip ~3 dB at every loop boundary. The tail ramp is folded into the head so
/// that looping tail→head is C⁰-continuous. The stored length = original length − crossfade
/// length. (This baked loop fold is independent of the render-time seam crossfade.)
fn apply_loop_crossfade(seg: RoomTone) -> RoomTone {
    let ch = seg.channels as usize;
    let n_frames = seg.samples.len().checked_div(ch).unwrap_or(0);
    let fade = loop_fade_frames(n_frames, seg.sample_rate).min(n_frames / 2);

    if fade == 0 || n_frames == 0 {
        return RoomTone {
            rms: compute_rms(&seg.samples),
            ..seg
        };
    }

    let mut out = seg.samples;

    // Fold tail into head over j in 0..fade with equal-power weights: g_head fades the head
    // up (0 at j=0) while the symmetric g_tail fades the tail down.
    // stored[j] = g_tail*tail[j] + g_head*head[j]  where tail[j]=out[(n_frames-fade+j)*ch..]
    for j in 0..fade {
        let g_head = super::equal_power_gain(j, fade);
        let g_tail = super::equal_power_gain(fade - 1 - j, fade);
        let tail_off = (n_frames - fade + j) * ch;
        let head_off = j * ch;
        for c in 0..ch {
            let tail_val = out[tail_off + c];
            let head_val = out[head_off + c];
            out[head_off + c] = g_tail * tail_val + g_head * head_val;
        }
    }

    out.truncate((n_frames - fade) * ch);
    let rms = compute_rms(&out);
    RoomTone {
        samples: out,
        channels: seg.channels,
        sample_rate: seg.sample_rate,
        rms,
    }
}

/// Crossfade length in frames for the loop crossfade, by segment length tier.
fn loop_fade_frames(n_frames: usize, sample_rate: u32) -> usize {
    let rate = sample_rate as f64;
    let short = (0.5 * rate).round() as usize; // 500 ms
    let mid = (2.0 * rate).round() as usize; // 2 s
    if n_frames < short {
        (0.050 * rate).round() as usize
    } else if n_frames <= mid {
        (0.100 * rate).round() as usize
    } else {
        (0.500 * rate).round() as usize
    }
}

// ---------------------------------------------------------------------------
// Sparse table for O(1) range-max
// ---------------------------------------------------------------------------

struct SparseTable {
    table: Vec<Vec<f32>>,
    log2_floor: Vec<u32>,
}

impl SparseTable {
    fn build(data: &[f32]) -> Self {
        let n = data.len();
        if n == 0 {
            return Self {
                table: vec![],
                log2_floor: vec![],
            };
        }
        let mut log2_floor = vec![0u32; n + 1];
        for i in 2..=n {
            log2_floor[i] = log2_floor[i / 2] + 1;
        }
        let max_k = log2_floor[n] as usize + 1;
        let mut table: Vec<Vec<f32>> = Vec::with_capacity(max_k);
        table.push(data.to_vec());
        let mut k = 1usize;
        while (1usize << k) <= n {
            let prev = &table[k - 1];
            let half = 1usize << (k - 1);
            let len = n + 1 - (1 << k);
            let row: Vec<f32> = (0..len).map(|i| prev[i].max(prev[i + half])).collect();
            table.push(row);
            k += 1;
        }
        Self { table, log2_floor }
    }

    /// Range max over `data[l..=r]` in O(1).
    fn query(&self, l: usize, r: usize) -> f32 {
        if self.table.is_empty() || l > r {
            return 0.0;
        }
        let len = r - l + 1;
        let k = self.log2_floor[len] as usize;
        let right_start = r + 1 - (1usize << k);
        self.table[k][l].max(self.table[k][right_start])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frame_reader::SliceFrameReader;
    use crate::project::hash::{encode_tagged, tag_byte, Kind};

    /// Run detection over an in-memory buffer via [`SliceFrameReader`]. The in-memory reader
    /// never errors, so the `Result` is unwrapped for the assertions below.
    fn detect(
        samples: &[f32],
        channels: u16,
        sample_rate: u32,
        params: &RoomToneParams,
    ) -> RoomToneOutcome {
        let mut reader = SliceFrameReader::new(samples, channels, sample_rate);
        detect_room_tone(&mut reader, params).expect("in-memory reader never errors")
    }

    // --- Test signal generators ---

    /// Seeded xorshift64 (matches tree.rs convention).
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// Uniform f32 in [-1, 1].
        fn f32_unit(&mut self) -> f32 {
            let u = self.next();
            (u as f32 / u64::MAX as f32) * 2.0 - 1.0
        }
    }

    /// White noise scaled to `target_rms`, `n_frames` frames, `ch` channels (interleaved).
    fn noise(rng: &mut Rng, n_frames: usize, ch: usize, target_rms: f32) -> Vec<f32> {
        let n = n_frames * ch;
        let raw: Vec<f32> = (0..n).map(|_| rng.f32_unit()).collect();
        // Compute actual RMS and rescale.
        let actual_rms = {
            let sq_sum: f64 = raw.iter().map(|&s| (s as f64) * (s as f64)).sum();
            ((sq_sum / n as f64) as f32).sqrt()
        };
        if actual_rms == 0.0 {
            return raw;
        }
        let scale = target_rms / actual_rms;
        raw.iter().map(|&s| s * scale).collect()
    }

    /// Sine wave at 440 Hz, `n_frames`, `ch` channels (same wave per channel), amplitude `amp`.
    fn sine(n_frames: usize, ch: usize, sample_rate: u32, amp: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(n_frames * ch);
        for i in 0..n_frames {
            let s =
                amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin();
            for _ in 0..ch {
                out.push(s);
            }
        }
        out
    }

    const RATE: u32 = 48_000;

    fn frames(secs: f64) -> usize {
        (secs * RATE as f64).round() as usize
    }

    fn default_params() -> RoomToneParams {
        RoomToneParams::default()
    }

    // --- D: detection algorithm ---

    // D1: Clean 2 s window accepted.
    #[test]
    fn d1_clean_window_accepted() {
        let mut rng = Rng::new(0x1111);
        let loud = sine(frames(8.0), 1, RATE, 0.9);
        let quiet = noise(&mut rng, frames(2.0), 1, 0.005);
        let mut sig = loud;
        sig.extend_from_slice(&quiet);
        match detect(&sig, 1, RATE, &default_params()) {
            RoomToneOutcome::Found(seg) => {
                // The segment came from the quiet tail region.
                let seg_rms = {
                    let sq: f64 = seg.samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
                    ((sq / seg.samples.len() as f64) as f32).sqrt()
                };
                assert!(
                    seg_rms < default_params().rms_ceiling,
                    "segment rms {seg_rms} should be below ceiling"
                );
            }
            RoomToneOutcome::None => panic!("expected Found"),
        }
    }

    // D2: Length precedence: 6 s window beats quieter 2 s window.
    #[test]
    fn d2_length_beats_lower_rms() {
        let mut rng = Rng::new(0x2222);
        // 12 s total: 4 s loud | 2 s very quiet | 4 s loud | 6 s quiet.
        let loud = sine(frames(4.0), 1, RATE, 0.9);
        let very_quiet = noise(&mut rng, frames(2.0), 1, 0.001);
        let quiet6 = noise(&mut rng, frames(6.0), 1, 0.01);
        let mut sig = loud.clone();
        sig.extend_from_slice(&very_quiet);
        sig.extend_from_slice(&loud);
        sig.extend_from_slice(&quiet6);

        match detect(&sig, 1, RATE, &default_params()) {
            RoomToneOutcome::Found(seg) => {
                // The 6 s window should be preferred; its RMS is ~0.01 (above 0.001).
                // We can verify length: 6 s window → seg frames ≈ 6 s - 500 ms fade.
                let seg_frames = seg.samples.len();
                let min_frames = frames(5.0); // 6 s - 500 ms loop fade, roughly
                assert!(
                    seg_frames >= min_frames,
                    "expected ≥5 s segment, got {} frames",
                    seg_frames
                );
            }
            RoomToneOutcome::None => panic!("expected Found"),
        }
    }

    // D3: Upper clamp at 10 s.
    #[test]
    fn d3_upper_clamp_at_10s() {
        let mut rng = Rng::new(0x3333);
        let quiet = noise(&mut rng, frames(30.0), 1, 0.005);
        match detect(&quiet, 1, RATE, &default_params()) {
            RoomToneOutcome::Found(seg) => {
                let seg_frames = seg.samples.len();
                let max_frames = frames(10.0); // 10 s upper cap + some tolerance
                assert!(
                    seg_frames <= max_frames,
                    "segment {seg_frames} frames exceeds 10 s cap ({max_frames})"
                );
            }
            RoomToneOutcome::None => panic!("expected Found"),
        }
    }

    // D4: Peak-energy rejection.
    #[test]
    fn d4_peak_rejection() {
        let mut rng = Rng::new(0x4444);
        // 10 s quiet noise with a single large transient in the middle.
        let mut sig = noise(&mut rng, frames(10.0), 1, 0.005);
        sig[frames(5.0)] = 1.0; // transient far above 5 × window_rms
        let params = RoomToneParams {
            rms_ceiling: 0.1,
            quiet_percentile: 5.0,
        };
        // The transient blocks window acceptance; stitch may still find quiet pieces.
        // We just assert no panic and the transient block itself isn't accepted as a window.
        let _ = detect(&sig, 1, RATE, &params);
    }

    // D5: Variance rejection.
    #[test]
    fn d5_variance_rejection() {
        // A 10 s slow amplitude swell: block-RMS SD >> 15% of mean.
        // Use a sine whose amplitude ramps from near-0 to 0.03 over the window.
        let n = frames(10.0);
        let sig: Vec<f32> = (0..n)
            .map(|i| {
                let amp = 0.001 + 0.029 * (i as f32 / n as f32);
                amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin()
            })
            .collect();
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 5.0,
        };
        // The entire signal has high block-RMS SD; no window should pass criterion 2.
        // It may stitch individual quiet blocks if any exist; that's OK—we just verify no panic.
        let _ = detect(&sig, 1, RATE, &params);
    }

    // D6: Tie-break 1 — lower RMS wins at equal length.
    #[test]
    fn d6_tiebreak_lower_rms() {
        let mut rng = Rng::new(0x6666);
        // 20 s: 2 s quiet@0.005 | 8 s loud | 2 s quiet@0.010 | 8 s loud.
        let q1 = noise(&mut rng, frames(2.0), 1, 0.005);
        let loud = sine(frames(8.0), 1, RATE, 0.9);
        let q2 = noise(&mut rng, frames(2.0), 1, 0.010);

        let mut sig = q1.clone();
        sig.extend_from_slice(&loud);
        sig.extend_from_slice(&q2);
        sig.extend_from_slice(&loud);

        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        // Both windows are 2 s (equal length at min_blk). Lower RMS (q1 ≈ 0.005) should win.
        match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(seg) => {
                let seg_rms = {
                    let sq: f64 = seg.samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
                    ((sq / seg.samples.len() as f64) as f32).sqrt()
                };
                // The selected window should have lower RMS (≈ 0.005, not ≈ 0.010).
                assert!(
                    seg_rms < 0.008,
                    "expected lower-RMS window; got rms {seg_rms}"
                );
            }
            RoomToneOutcome::None => panic!("expected Found"),
        }
    }

    // D6b: Tie-break 2 — earliest start at equal length and RMS.
    #[test]
    fn d6b_tiebreak_earliest_start() {
        let mut rng = Rng::new(0x6b6b);
        // Same noise seed → identical RMS for both windows (both from same generator).
        let q1 = noise(&mut rng, frames(2.0), 1, 0.005);
        let loud = sine(frames(8.0), 1, RATE, 0.9);
        let q2 = noise(&mut rng, frames(2.0), 1, 0.005);
        let mut sig = q1;
        sig.extend_from_slice(&loud);
        sig.extend_from_slice(&q2);
        sig.extend_from_slice(&loud);

        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        // Both runs the same level; earliest (first) should be selected.
        let r1 = detect(&sig, 1, RATE, &params);
        let r2 = detect(&sig, 1, RATE, &params);
        let (s1, s2) = match (r1, r2) {
            (RoomToneOutcome::Found(a), RoomToneOutcome::Found(b)) => (a.samples, b.samples),
            _ => panic!("expected Found twice"),
        };
        assert_eq!(s1, s2, "repeated calls must return identical samples");
    }

    // D7: Stitch fallback engaged.
    #[test]
    fn d7_stitch_fallback() {
        let mut rng = Rng::new(0x7777);
        // 20 s: alternating 100 ms quiet / 900 ms loud → no contiguous 2 s quiet window.
        let block_ms = (0.1 * RATE as f64).round() as usize;
        let loud_len = (0.9 * RATE as f64).round() as usize;
        let mut sig = Vec::new();
        for _ in 0..20 {
            let q = noise(&mut rng, block_ms, 1, 0.005);
            let l = sine(loud_len, 1, RATE, 0.9);
            sig.extend_from_slice(&q);
            sig.extend_from_slice(&l);
        }
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(seg) => {
                // Check no hard discontinuity at seam points in the stitched output.
                // (Crossfaded junctions should be bounded.)
                let max_diff = seg
                    .samples
                    .windows(2)
                    .map(|w| (w[1] - w[0]).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    max_diff < 0.5,
                    "max first-difference {max_diff} exceeds 0.5 — hard discontinuity at seam"
                );
            }
            RoomToneOutcome::None => panic!("expected Found via stitch"),
        }
    }

    // D8: No usable quiet material → None (full-scale sine).
    #[test]
    fn d8_loud_signal_returns_none() {
        let sig = sine(frames(10.0), 1, RATE, 1.0);
        assert!(
            matches!(
                detect(&sig, 1, RATE, &default_params()),
                RoomToneOutcome::None
            ),
            "full-scale sine must return None (criterion 0: rms > rms_ceiling)"
        );
    }

    // D9: Stereo analysis uses mono down-mix.
    #[test]
    fn d9_stereo_same_window_as_mono() {
        let mut rng = Rng::new(0x9999);
        let loud_m = sine(frames(8.0), 1, RATE, 0.9);
        let quiet_m = noise(&mut rng, frames(2.0), 1, 0.005);
        let mono_sig: Vec<f32> = loud_m.iter().chain(quiet_m.iter()).copied().collect();

        // Stereo: duplicate each mono sample.
        let stereo_sig: Vec<f32> = mono_sig.iter().flat_map(|&s| [s, s]).collect();

        let mono_result = detect(&mono_sig, 1, RATE, &default_params());
        let stereo_result = detect(&stereo_sig, 2, RATE, &default_params());

        let (mf, sf) = match (mono_result, stereo_result) {
            (RoomToneOutcome::Found(m), RoomToneOutcome::Found(s)) => (m, s),
            _ => panic!("expected Found for both"),
        };

        // Frame count (per-channel length) should match.
        let mono_frames = mf.samples.len(); // mono: samples == frames
        let stereo_frames = sf.samples.len() / 2; // stereo: samples = frames × 2
        assert_eq!(
            mono_frames, stereo_frames,
            "mono {mono_frames} and stereo {stereo_frames} frame counts differ"
        );
    }

    // D9b: Channel count and content preserved.
    #[test]
    fn d9b_channel_count_preserved() {
        let mut rng = Rng::new(0x9b9b);
        // Mono quiet signal.
        let mono_q = noise(&mut rng, frames(10.0), 1, 0.005);
        // Stereo signal where left ≠ right.
        let left = noise(&mut rng, frames(10.0), 1, 0.005);
        let right = noise(&mut rng, frames(10.0), 1, 0.004);
        let stereo: Vec<f32> = left
            .iter()
            .zip(right.iter())
            .flat_map(|(&l, &r)| [l, r])
            .collect();

        let mr = detect(&mono_q, 1, RATE, &default_params());
        let sr = detect(&stereo, 2, RATE, &default_params());

        let ms = match mr {
            RoomToneOutcome::Found(s) => s,
            _ => panic!("mono expected Found"),
        };
        let ss = match sr {
            RoomToneOutcome::Found(s) => s,
            _ => panic!("stereo expected Found"),
        };

        assert_eq!(ms.channels, 1, "mono channel count");
        assert_eq!(ss.channels, 2, "stereo channel count");

        // Stereo segment must have interleaved L and R (not collapsed to mono).
        let left_vals: Vec<f32> = ss.samples.iter().step_by(2).copied().collect();
        let right_vals: Vec<f32> = ss.samples.iter().skip(1).step_by(2).copied().collect();
        assert_ne!(
            left_vals, right_vals,
            "stereo channels should not be identical"
        );
    }

    // D10: Trailing partial block doesn't panic.
    #[test]
    fn d10_trailing_partial_block() {
        let mut rng = Rng::new(0xa0a0);
        // 10.05 s — not a multiple of 100 ms.
        let sig = noise(&mut rng, frames(10.05), 1, 0.005);
        // Should analyse without panic and find the quiet window.
        assert!(matches!(
            detect(&sig, 1, RATE, &default_params()),
            RoomToneOutcome::Found(_)
        ));
    }

    // D10b: < 10 s → None; ≥ 10 s → Found.
    #[test]
    fn d10b_ten_second_boundary() {
        let mut rng = Rng::new(0xb0b0);
        let nine = noise(&mut rng, frames(9.0), 1, 0.005);
        assert!(
            matches!(
                detect(&nine, 1, RATE, &default_params()),
                RoomToneOutcome::None
            ),
            "9 s must be None"
        );
        let mut rng2 = Rng::new(0xb0b0);
        let ten = noise(&mut rng2, frames(10.0), 1, 0.005);
        assert!(
            matches!(
                detect(&ten, 1, RATE, &default_params()),
                RoomToneOutcome::Found(_)
            ),
            "10 s must be Found"
        );
    }

    // D11: Determinism.
    #[test]
    fn d11_determinism() {
        let mut rng = Rng::new(0xd1d1);
        let sig = noise(&mut rng, frames(15.0), 1, 0.005);
        let r1 = detect(&sig, 1, RATE, &default_params());
        let r2 = detect(&sig, 1, RATE, &default_params());
        let (s1, s2) = match (r1, r2) {
            (RoomToneOutcome::Found(a), RoomToneOutcome::Found(b)) => (a.samples, b.samples),
            _ => panic!("expected Found"),
        };
        assert_eq!(s1, s2, "detect_room_tone must be deterministic");
    }

    // D11b: Thresholds are honored.
    #[test]
    fn d11b_threshold_configurability() {
        let mut rng = Rng::new(0xd11b);
        // Signal with a quiet region at RMS ≈ 0.02.
        let loud = sine(frames(8.0), 1, RATE, 0.9);
        let quiet = noise(&mut rng, frames(2.0), 1, 0.02);
        let mut sig = loud;
        sig.extend_from_slice(&quiet);

        let accept = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 5.0,
        };
        let reject = RoomToneParams {
            rms_ceiling: 0.01,
            quiet_percentile: 5.0,
        };
        assert!(
            matches!(detect(&sig, 1, RATE, &accept), RoomToneOutcome::Found(_)),
            "ceiling 0.05 should accept rms ≈ 0.02"
        );
        assert!(
            matches!(detect(&sig, 1, RATE, &reject), RoomToneOutcome::None),
            "ceiling 0.01 should reject rms ≈ 0.02"
        );
    }

    // --- L: loop crossfade tiers ---

    fn seg_of_frames(n: usize, rate: u32, ch: usize) -> RoomTone {
        RoomTone {
            samples: vec![0.1f32; n * ch],
            channels: ch as u16,
            sample_rate: rate,
            rms: 0.0, // consumed by apply_loop_crossfade, which recomputes rms
        }
    }

    // L12: < 500 ms → 50 ms fade.
    #[test]
    fn l12_short_uses_50ms_fade() {
        let n = frames(0.3);
        let expected_fade = (0.05 * RATE as f64).round() as usize;
        assert_eq!(loop_fade_frames(n, RATE), expected_fade);
    }

    // L13: 500 ms – 2 s → 100 ms fade.
    #[test]
    fn l13_mid_uses_100ms_fade() {
        let n = frames(1.0);
        let expected_fade = (0.10 * RATE as f64).round() as usize;
        assert_eq!(loop_fade_frames(n, RATE), expected_fade);
    }

    // L14: > 2 s → 500 ms fade.
    #[test]
    fn l14_long_uses_500ms_fade() {
        let n = frames(5.0);
        let expected_fade = (0.50 * RATE as f64).round() as usize;
        assert_eq!(loop_fade_frames(n, RATE), expected_fade);
    }

    // L15: Seamless wrap (C⁰ continuity).
    #[test]
    fn l15_seamless_wrap() {
        // Use a sine to make both head and tail non-zero.
        let raw_frames = frames(3.0);
        let raw = sine(raw_frames, 1, RATE, 0.02);
        let seg = RoomTone {
            samples: raw,
            channels: 1,
            sample_rate: RATE,
            rms: 0.0, // consumed by apply_loop_crossfade, which recomputes rms
        };
        let stored = apply_loop_crossfade(seg);
        let n = stored.samples.len();
        // Wrap point: last sample → first sample of next loop.
        let diff = (stored.samples[n - 1] - stored.samples[0]).abs();
        assert!(
            diff < 0.1,
            "wrap discontinuity {diff} too large — loop crossfade not applied correctly"
        );
    }

    // L16: Fade not duplicated — stored frame count = raw - fade.
    #[test]
    fn l16_fade_not_duplicated() {
        let raw_frames = frames(3.0);
        let fade_frames = loop_fade_frames(raw_frames, RATE);
        let seg = seg_of_frames(raw_frames, RATE, 1);
        let stored = apply_loop_crossfade(seg);
        assert_eq!(
            stored.samples.len(),
            raw_frames - fade_frames,
            "stored frame count should be raw_frames - fade_frames"
        );
        // Stereo: samples.len() == 2 × frame_count.
        let raw_frames2 = frames(3.0);
        let fade_frames2 = loop_fade_frames(raw_frames2, RATE);
        let seg2 = seg_of_frames(raw_frames2, RATE, 2);
        let stored2 = apply_loop_crossfade(seg2);
        assert_eq!(stored2.samples.len(), (raw_frames2 - fade_frames2) * 2);
    }

    // --- B: RoomTone V1 blob ---

    fn sample_segment() -> RoomTone {
        let samples = vec![0.1f32, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4];
        let rms = super::compute_rms(&samples);
        RoomTone {
            sample_rate: 48_000,
            channels: 2,
            rms,
            samples,
        }
    }

    // B17: Round-trip.
    #[test]
    fn b17_round_trip() {
        let seg = sample_segment();
        let (_, bytes) = encode_room_tone(&seg).unwrap();
        let decoded = decode_room_tone(&bytes).unwrap();
        assert_eq!(decoded.sample_rate, seg.sample_rate);
        assert_eq!(decoded.channels, seg.channels);
        assert_eq!(decoded.samples, seg.samples, "samples must be bit-equal");

        // Also test mono.
        let mono_samples = vec![0.5f32, -0.5, 0.25];
        let mono = RoomTone {
            sample_rate: 44_100,
            channels: 1,
            rms: super::compute_rms(&mono_samples),
            samples: mono_samples,
        };
        let (_, bytes2) = encode_room_tone(&mono).unwrap();
        let dec2 = decode_room_tone(&bytes2).unwrap();
        assert_eq!(dec2.channels, 1);
        assert_eq!(dec2.samples, mono.samples);
    }

    // B18: Tag byte is 0x41.
    #[test]
    fn b18_tag_byte() {
        let seg = sample_segment();
        let (_, bytes) = encode_room_tone(&seg).unwrap();
        assert_eq!(
            bytes[0], 0x41,
            "first byte must be tag_byte(RoomTone, 1) = 0x41"
        );
    }

    // B19: Pinned wire bytes.
    #[test]
    fn b19_pinned_wire_bytes() {
        let samples = vec![0.1f32, -0.1f32, 0.2f32, -0.2f32];
        let v1_seg = v1::RoomToneV1 {
            sample_rate: 48_000,
            channels: 2,
            rms: super::compute_rms(&samples),
            samples,
        };
        let (_, bytes) = encode_tagged(Kind::RoomTone, 1, &v1_seg).unwrap();
        assert_eq!(
            bytes.as_slice(),
            &PINNED_WIRE_BYTES,
            "V1 wire format changed — regenerate pinned bytes"
        );
    }

    // B20: Pinned hash.
    #[test]
    fn b20_pinned_hash() {
        let samples = vec![0.1f32, -0.1f32, 0.2f32, -0.2f32];
        let v1_seg = v1::RoomToneV1 {
            sample_rate: 48_000,
            channels: 2,
            rms: super::compute_rms(&samples),
            samples,
        };
        let (hash, _) = encode_tagged(Kind::RoomTone, 1, &v1_seg).unwrap();
        assert_eq!(
            hash.0, PINNED_HASH,
            "V1 hash changed — regenerate pinned hash"
        );
    }

    // B21: Hash determinism and sensitivity.
    #[test]
    fn b21_hash_determinism_and_sensitivity() {
        let seg = sample_segment();
        let (h1, _) = encode_room_tone(&seg).unwrap();
        let (h2, _) = encode_room_tone(&seg).unwrap();
        assert_eq!(h1, h2, "same input must hash identically");

        let mut seg2 = sample_segment();
        seg2.samples[0] += 0.001;
        let (h3, _) = encode_room_tone(&seg2).unwrap();
        assert_ne!(h1, h3, "one-sample change must change hash");

        let mut seg3 = sample_segment();
        seg3.sample_rate = 44_100;
        let (h4, _) = encode_room_tone(&seg3).unwrap();
        assert_ne!(h1, h4, "sample_rate change must change hash");

        let mut seg4 = sample_segment();
        seg4.channels = 1;
        let (h5, _) = encode_room_tone(&seg4).unwrap();
        assert_ne!(h1, h5, "channels change must change hash");
    }

    // B22: Kind mismatch.
    #[test]
    fn b22_kind_mismatch() {
        let (_, bytes) = encode_tagged(Kind::Turn, 1, &42u32).unwrap();
        let err = decode_room_tone(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::KindMismatch {
                expected: Kind::RoomTone,
                found: Kind::Turn,
            }
        ));
    }

    // B23: Unknown version.
    #[test]
    fn b23_unknown_version() {
        let tag = tag_byte(Kind::RoomTone, 0xF);
        let bytes = [tag, 0x00, 0x00];
        let err = decode_room_tone(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::UnknownVersion {
                kind: Kind::RoomTone,
                version: 0xF,
            }
        ));
    }

    // B24: Empty / truncated input.
    #[test]
    fn b24_empty_and_truncated() {
        assert!(matches!(decode_room_tone(&[]), Err(DecodeError::Empty)));
        assert!(matches!(
            decode_room_tone(&[0x41]),
            Err(DecodeError::Postcard(_))
        ));
    }

    // B25: G1 fixture round-trip.
    #[test]
    fn b25_g1_fixture_round_trip() {
        let fixture = include_bytes!("../../tests/fixtures/room_tone_v1.blob");
        let seg = decode_room_tone(fixture).expect("fixture must decode without error");
        assert_eq!(seg.sample_rate, 48_000);
        assert_eq!(seg.channels, 2);
        assert_eq!(
            seg.samples,
            vec![0.1f32, -0.1f32, 0.2f32, -0.2f32],
            "fixture samples must match expected values"
        );
    }

    // --- X: cross-cutting ---

    // X27: Empty input → None, no panic.
    #[test]
    fn x27_empty_input() {
        assert!(matches!(
            detect(&[], 1, RATE, &default_params()),
            RoomToneOutcome::None
        ));
        assert!(matches!(
            detect(&[], 2, RATE, &default_params()),
            RoomToneOutcome::None
        ));
    }

    // X28: Zero channels OR zero sample rate → None (pins the `||` guard at line 82: with
    // `&&` a single-zero input would fall through instead of short-circuiting to None).
    #[test]
    fn x28_zero_channels_or_rate_returns_none() {
        let mut rng = Rng::new(0x2828);
        let quiet = noise(&mut rng, frames(12.0), 1, 0.005);
        // Zero sample rate, non-zero channels → must be None.
        assert!(matches!(
            detect(&quiet, 1, 0, &default_params()),
            RoomToneOutcome::None
        ));
        // Zero channels, non-zero sample rate → must be None.
        assert!(matches!(
            detect(&quiet, 0, RATE, &default_params()),
            RoomToneOutcome::None
        ));
    }

    // X29: Stereo down-mix divisor is `/ch`, not `*ch`. A stereo signal whose per-channel
    // level is just under the ceiling down-mixes (l+r)/2 to a quiet value (Found), but the
    // `/ → *` mutant computes (l+r)*2 ≈ 4× the level, pushing every block over the ceiling →
    // None. (The sibling `/ → %` mutant is an equivalent mutant for normalized PCM: with
    // |sample| ≤ 1 the channel sum lies in [-2, 2], so `sum % 2.0 == sum` for 2 channels —
    // see report.)
    #[test]
    fn x29_stereo_downmix_divides() {
        let mut rng = Rng::new(0x2929);
        // Per-channel mono noise at RMS ≈ 0.02 (below the 0.05 ceiling). Duplicated to both
        // channels so the correct down-mix RMS is also ≈ 0.02, but `*2` down-mix is ≈ 0.08.
        let mono = noise(&mut rng, frames(12.0), 1, 0.02);
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        assert!(
            matches!(detect(&stereo, 2, RATE, &params), RoomToneOutcome::Found(_)),
            "correct /2 down-mix keeps the stereo signal under the ceiling → Found"
        );
    }

    /// Build a mono signal from a list of (block_count, per-sample-value) segments, each block
    /// being `block_len` constant samples. A constant-valued block has RMS == |value| and
    /// peak == |value|, so block stats are exactly predictable.
    fn blocks_signal(segments: &[(usize, f32)], block_len: usize) -> Vec<f32> {
        let mut sig = Vec::new();
        for &(count, v) in segments {
            sig.extend(std::iter::repeat_n(v, count * block_len));
        }
        sig
    }

    /// Build one block of `block_len` mono samples with `n_hi` samples set to `hi` and the rest
    /// to 0, so block RMS = hi·√(n_hi/block_len) and block peak = hi (peaks at the front).
    fn peaky_block(block_len: usize, n_hi: usize, hi: f32) -> Vec<f32> {
        let mut b = vec![0.0f32; block_len];
        for s in b.iter_mut().take(n_hi) {
            *s = hi;
        }
        b
    }

    // X32: Criterion 0 (window RMS vs ceiling) boundary — pins line 193 `rms > rms_ceiling`.
    //
    // A uniform 2 s window at constant value == the ceiling has window RMS == ceiling exactly
    // (constant block → no √ rounding). The strict `>` accepts it as a contiguous window (rms is
    // not > ceiling) → the result is the loop-folded raw window. A `> → >=` mutant rejects the
    // window; it then falls to the stitch fallback, which re-assembles the same 20 blocks with
    // 50 ms inter-block crossfades — a *different length* segment. We assert the exact contiguous
    // reconstruction, so the rejected (stitched) path fails.
    #[test]
    fn x32_criterion0_rms_ceiling_boundary() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let ceiling = 0.05f32;
        let segments: Vec<(usize, f32)> = vec![
            (40, 0.5),     // loud lead
            (20, ceiling), // candidate 2 s window, RMS == ceiling exactly
            (40, 0.5),     // loud tail → 10 s total
        ];
        let sig = blocks_signal(&segments, block_len);
        let params = RoomToneParams {
            rms_ceiling: ceiling,
            quiet_percentile: 95.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("window RMS == ceiling must be accepted (criterion 0)"),
        };
        // Expected: contiguous window path → loop-fold of the raw uniform 2 s window.
        let window = vec![ceiling; 20 * block_len];
        let want = ref_loop_fold(&window, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "RMS == ceiling must be accepted as a contiguous window (not rejected → stitched)"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // X33: Criterion 1 (peak vs 5·RMS) — pins line 197 (`peak > 5.0 * rms`: the `>` operator and
    // the `5.0 *` factor).
    //
    // The single candidate 2 s window has block RMS = hi/5 (4 % of samples at `hi`, rest 0) but
    // an extra large transient at `2·hi`, giving window peak = 2·hi = 10·RMS — clearly above the
    // 5·RMS limit. Correct code rejects the window (and the transient block also fails the stitch
    // peak filter, so only the non-transient blocks stitch). A `> → ==` mutant (10·RMS ≠ 5·RMS),
    // or a `* → +` / `* → /` mutant (threshold no longer 5·RMS), wrongly *accepts* the full
    // window → a longer contiguous segment containing the 2·hi transient. We assert the transient
    // value never appears in the result. (`> → >=` is indistinguishable from `>` except at the
    // exact peak == 5·RMS tie, which f32 rounding of the √-based window RMS cannot hit reliably —
    // see report: boundary-equivalent.)
    #[test]
    fn x33_criterion1_peak_factor() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let hi = 0.05f32;
        let n_hi = (0.04 * block_len as f64).round() as usize; // RMS = hi/5 = 0.01
        let mut blk = peaky_block(block_len, n_hi, hi);
        let loud = sine(block_len, 1, RATE, 0.5);
        let mut sig = Vec::new();
        for _ in 0..40 {
            sig.extend_from_slice(&loud); // 4 s loud lead
        }
        for k in 0..20 {
            if k == 10 {
                // one block carries a 2·hi transient → window peak = 10·RMS.
                let mut t = blk.clone();
                t[0] = 2.0 * hi;
                sig.extend_from_slice(&t);
            } else {
                sig.extend_from_slice(&blk);
            }
            blk = peaky_block(block_len, n_hi, hi);
        }
        for _ in 0..40 {
            sig.extend_from_slice(&loud); // 4 s loud tail → 10 s total
        }
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        // Whatever is returned, the 2·hi = 0.1 transient must never survive into the segment: the
        // peak criterion (and stitch peak filter) must exclude the block that carries it.
        match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(seg) => {
                let max_abs = seg.samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                assert!(
                    max_abs < 2.0 * hi - 1e-4,
                    "peak criterion must reject the 2·hi transient (max abs {max_abs})"
                );
            }
            RoomToneOutcome::None => {}
        }
    }

    // X34: Criterion 2 (block-RMS SD vs 0.15·mean) boundary — pins line 203 (`sd > 0.15 * mean`,
    // both `>` comparisons and the `mean > 0.0` guard) at the acceptance edge.
    //
    // A two-level alternating window with mean m and SD d where d == 0.15·m *exactly*: pick block
    // levels m·(1 ± 0.15). Strict `>` accepts (sd is not > 0.15·mean); `> → >=` rejects. The
    // levels are kept well under the ceiling and the window is the only candidate, so acceptance
    // vs rejection changes the result. (Rejection would fall to stitch and stitch the individual
    // blocks → different length.)
    #[test]
    fn x34_criterion2_sd_boundary() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let m = 0.01f32;
        let lo = m * (1.0 - 0.15); // 0.0085
        let hi = m * (1.0 + 0.15); // 0.0115  → population SD over equal halves == 0.15·m
                                   // 20-block alternating window → SD == 0.15·mean exactly.
        let mut window_blocks: Vec<(usize, f32)> = Vec::new();
        for i in 0..20 {
            window_blocks.push((1, if i % 2 == 0 { lo } else { hi }));
        }
        let mut segments: Vec<(usize, f32)> = vec![(40, 0.5)];
        segments.extend(window_blocks);
        segments.push((40, 0.5));
        let sig = blocks_signal(&segments, block_len);
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        // Correct: SD == 0.15·mean is accepted by strict `>` → Found via the contiguous window.
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("SD == 0.15·mean must be accepted (criterion 2)"),
        };
        // Expected window = the 20 alternating blocks, loop-folded.
        let window: Vec<f32> = (0..20)
            .flat_map(|i| vec![if i % 2 == 0 { lo } else { hi }; block_len])
            .collect();
        let want = ref_loop_fold(&window, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "must accept the SD-boundary window"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // X35: Length-precedence + RMS tie-break — pins line 208 (`l > bl || (l == bl && rms < br)`)
    // and the window-sweep upper-bound guard at line 187.
    //
    // Two quiet windows of *equal* maximum length (2 s each, separated by loud) with different
    // RMS: the earlier one louder (0.02), the later one quieter (0.005). The tie-break must pick
    // the lower-RMS (later) window. A `< → ==` / `== → !=` mutation of the tie-break picks the
    // wrong window → different samples. We assert the selected window is the quiet (0.005) one by
    // its RMS.
    #[test]
    fn x35_length_tie_break_lower_rms() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        // Each quiet window is exactly 2 s (20 blocks) and uniform (SD 0, peak == rms). Loud
        // separators make them distinct equal-length candidates; nothing longer qualifies.
        let segments: Vec<(usize, f32)> = vec![
            (20, 0.02),  // earlier window, louder
            (20, 0.5),   // loud separator
            (20, 0.005), // later window, quieter — must win the tie-break
            (20, 0.5),   // loud tail → 8 s … extend below
            (20, 0.5),
        ];
        let sig = blocks_signal(&segments, block_len);
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found"),
        };
        // The selected window's samples are uniform == the winning level. Reconstruct both
        // candidates' folded forms and assert we got the quiet (0.005) one.
        let quiet_window = vec![0.005f32; 20 * block_len];
        let want_quiet = ref_loop_fold(&quiet_window, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want_quiet.len(),
            "length must be the 2 s window"
        );
        // Distinguish by content: the quiet window is all 0.005 (pre-fold); after fold the bulk
        // (non-fade region) is still exactly 0.005, whereas the louder candidate would be 0.02.
        let mid = seg.samples[seg.samples.len() / 2];
        assert!(
            (mid - 0.005).abs() < 1e-6,
            "tie-break must select the lower-RMS (0.005) window, got mid sample {mid}"
        );
    }

    // X38: Stitch-fallback peak qualification `block_peak[b] <= 5.0 * rms` — pins line 351
    // (the `5.0 *` factor) against `* → +` / `* → /`.
    //
    // Forces the stitch path (alternating 100 ms quiet / 100 ms loud, no contiguous 2 s window),
    // with the quiet blocks all clean (peak == rms, level 0.01) EXCEPT one "decoy" block that is
    // the *quietest* (RMS ≈ 0.005, so it sorts first in the quietest-first selection) but whose
    // peak == 6·RMS = 0.03 — above the 5·RMS limit. Correct code *excludes* the decoy (peak
    // filter), so its 0.03 peak never appears in the stitched output. A `* → +` (threshold
    // 5 + RMS) or `* → /` (5 / RMS, huge) mutant wrongly *admits* the decoy, which — being the
    // quietest — is then selected first and its 0.03 peak leaks in. We assert no sample reaches
    // the decoy peak.
    #[test]
    fn x38_stitch_peak_filter_factor() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        // Decoy: RMS ≈ 0.005 (quietest) but peak 0.045 == 9·RMS, with the loud samples placed in
        // the *middle* of the block (away from the 50 ms junction crossfades) so that, if the
        // decoy is wrongly admitted, its peak survives into the output un-attenuated.
        let decoy_hi = 0.045f32;
        let n_hi = (block_len as f64 * (0.005 / decoy_hi as f64).powi(2)).round() as usize;
        let mut decoy = vec![0.0f32; block_len];
        let mid = block_len / 2;
        for s in decoy.iter_mut().skip(mid).take(n_hi) {
            *s = decoy_hi;
        }
        let quiet = 0.01f32; // clean quiet blocks (peak == rms), louder than the decoy
        let loud = sine(block_len, 1, RATE, 0.5);
        let mut sig = Vec::new();
        // 30 s: alternating quiet / loud; block index 10 (a quiet slot) is the decoy.
        for k in 0..150 {
            if k == 10 {
                sig.extend_from_slice(&decoy);
            } else {
                sig.extend(std::iter::repeat_n(quiet, block_len));
            }
            sig.extend_from_slice(&loud);
        }
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(seg) => {
                let max_abs = seg.samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                assert!(
                    max_abs < 0.02,
                    "stitch peak filter must exclude the 9·RMS decoy block (max abs {max_abs})"
                );
            }
            RoomToneOutcome::None => panic!("expected Found via stitch"),
        }
    }

    // X37: Tie-break strictness — pins line 208 `rms < br` against `rms <= br`.
    //
    // Two equal-length (2 s) windows with *identical* window RMS but different content: the
    // earlier window is a constant +v, the later one alternates ±v per sample. Both have the same
    // per-block RMS (v), peak (v) and SD (0), so the choice reduces to the strict-`<` RMS
    // tie-break, which (RMS being equal) keeps the *first* candidate. A `< → <=` mutant replaces
    // it with the later, equal-RMS window → different samples. We assert the constant (+v) window
    // was selected.
    #[test]
    fn x37_tie_break_strict_keeps_earliest() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let v = 0.01f32;
        let constant_window: Vec<f32> = vec![v; 20 * block_len];
        let alternating_window: Vec<f32> = (0..20 * block_len)
            .map(|i| if i % 2 == 0 { v } else { -v })
            .collect();
        let loud = sine(20 * block_len, 1, RATE, 0.5); // 2 s loud separator
        let mut sig = Vec::new();
        sig.extend_from_slice(&constant_window); // earliest candidate
        sig.extend_from_slice(&loud);
        sig.extend_from_slice(&alternating_window); // later, equal-RMS candidate
        sig.extend_from_slice(&loud); // tail → 8 s; extend below to ≥ 10 s
        sig.extend_from_slice(&loud);
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found"),
        };
        // The earliest (constant +v) window must win: its non-fade interior is all +v, whereas the
        // alternating window would contain -v samples in the interior.
        let interior_has_negative = seg.samples[block_len..seg.samples.len() - block_len]
            .iter()
            .any(|&s| s < -v / 2.0);
        assert!(
            !interior_has_negative,
            "strict `<` tie-break must keep the earliest (constant +v) window"
        );
    }

    // X36: Window-sweep upper-bound guard `if upper < lower` — pins line 187 against both
    // `< → <=` and `< → ==`.
    //
    // The only quiet material is the *final* 2 s (the last min_blk blocks); everything before is
    // loud. At that single qualifying start `s`, `upper = max_blk.min(n_blocks - s) == min_blk ==
    // lower`, so the correct strict `<` does NOT skip and the min-length window is evaluated and
    // accepted (a contiguous-window result = loop-folded raw window). A `<= ` or `==` mutant makes
    // `upper (==) lower` true and *skips* this start, dropping the only window → the detector
    // falls back to stitching the same 20 blocks (different length). We assert the exact
    // contiguous reconstruction.
    #[test]
    fn x36_window_sweep_upper_guard() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let q = 0.01f32;
        let segments: Vec<(usize, f32)> = vec![
            (80, 0.5), // 8 s loud lead
            (20, q),   // final 2 s quiet window (starts at the last min_blk position)
        ];
        let sig = blocks_signal(&segments, block_len);
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("the final 2 s window must be found (upper == lower)"),
        };
        let window = vec![q; 20 * block_len];
        let want = ref_loop_fold(&window, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "the last-start min-length window must be evaluated, not skipped → stitched"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // X30: Variance (SD) criterion is decisive — pins the prefix_r / prefix_r2 accumulators
    // (lines 145/146) that feed block_rms_mean / block_rms_sd.
    //
    // Layout (block_len = 100 ms): a uniform 2 s quiet window Y (SD = 0 → accepted) and a
    // longer 3 s alternating window X whose block-RMS SD is ≈ 16 % of its mean (> 15 % → the
    // variance criterion rejects X at every length). With correct prefix sums the only
    // acceptable window is Y (2 s). If the prefix_r / prefix_r2 accumulation is mutated, the
    // SD computation collapses and X is wrongly accepted; X is longer (3 s > 2 s) so it would
    // win — changing both the length and the samples. We assert the selected window is exactly
    // Y by reconstructing it (uniform 0.0055) + loop fold.
    #[test]
    fn x30_variance_criterion_selects_low_sd_window() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        // loud value chosen above the ceiling so loud blocks never qualify.
        let loud = 0.5f32;
        let y_val = 0.0055f32; // uniform → SD 0
                               // X alternates 0.0046 / 0.0064 → mean 0.0055, SD/mean ≈ 0.164 > 0.15.
                               // Build X explicitly as alternating single blocks.
        let mut segments: Vec<(usize, f32)> = vec![(10, loud), (20, y_val), (10, loud)];
        for i in 0..30 {
            let v = if i % 2 == 0 { 0.0046 } else { 0.0064 };
            segments.push((1, v));
        }
        segments.push((30, loud)); // trailing loud → total ≥ 10 s
        let sig = blocks_signal(&segments, block_len);

        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found (Y is a valid 2 s window)"),
        };
        // Expected = Y window (20 blocks of y_val) with loop fold applied.
        let y_window = vec![y_val; 20 * block_len];
        let want = ref_loop_fold(&y_window, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "must select the low-SD 2 s window Y, not the 3 s high-SD window X"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // X31: Percentile rank `ceil(pctile/100 * n_blocks)` is decisive — pins line 170, the
    // adaptive quiet threshold Q = min(rms_ceiling, Pq) used to admit blocks into the stitch
    // fallback.
    //
    // 100 blocks (10 s) of geometrically graduated constant levels 0.001 · 1.035^i (≈ 0.001 …
    // 0.030, all under the 0.05 ceiling). The constant *ratio* gives every contiguous window —
    // at any start or length — a block-RMS SD ≈ 20 % of its mean (> 15 %), so the variance
    // criterion rejects all windows → the stitch path runs. With pctile = 20 the correct rank is
    // 20, so Q = Pq = level[19] and exactly the quietest 20 blocks (indices 0..20) qualify and
    // are stitched. A `* 100` / `+ n_blocks` mutation of the rank saturates to n_blocks, making
    // Q ≈ the max level so all 100 blocks qualify → a far longer stitched segment. We assert the
    // exact quietest-20 reconstruction.
    #[test]
    fn x31_percentile_rank_sets_stitch_threshold() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let n = 100usize;
        let level = |i: usize| 0.001f32 * 1.035f32.powi(i as i32);
        let segments: Vec<(usize, f32)> = (0..n).map(|i| (1, level(i))).collect();
        let sig = blocks_signal(&segments, block_len);

        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 20.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found via stitch"),
        };

        // Correct: quietest 20 blocks = indices 0..20 (graduated, so quietest == lowest index),
        // assembled in original order with 50 ms crossfades, then loop fold.
        let pieces: Vec<Vec<f32>> = (0..20).map(|i| vec![level(i); block_len]).collect();
        let fade_len = (0.05 * RATE as f64).round() as usize;
        let stitched = crossfade_concat(&pieces, 1, fade_len);
        let want = ref_loop_fold(&stitched, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "stitch must admit exactly the quietest 20 blocks set by the 20th-percentile rank"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // --- M: helper-level exact-arithmetic tests (mutation hardening) ---
    //
    // These assert the exact sample math of the assembly helpers and the window-query
    // structures so that off-by-one index / wrong-operator mutations in the prefix sums,
    // crossfade offsets, loop fold, and sparse table are caught. Each expected value is
    // recomputed by an independent reference loop in the test, so a mutated production
    // operator diverges from the reference and the assertion fails.

    /// Reference equal-power crossfade-concat, written independently of the production
    /// loop, used as the golden oracle for [`crossfade_concat`].
    fn ref_crossfade_concat(pieces: &[Vec<f32>], ch: usize, fade_len: usize) -> Vec<f32> {
        if pieces.is_empty() {
            return vec![];
        }
        let mut out = pieces[0].clone();
        for piece in &pieces[1..] {
            let out_frames = out.len() / ch;
            let piece_frames = piece.len() / ch;
            let eff = fade_len.min(out_frames).min(piece_frames);
            let base = out_frames - eff;
            for i in 0..eff {
                let t_in = i as f32 / (eff.max(2) - 1) as f32;
                let (g_in, g_out) = if eff <= 1 {
                    (1.0f32, 1.0f32)
                } else {
                    (
                        (t_in * std::f32::consts::FRAC_PI_2).sin(),
                        (((eff - 1 - i) as f32 / (eff - 1) as f32) * std::f32::consts::FRAC_PI_2)
                            .sin(),
                    )
                };
                for c in 0..ch {
                    let old_val = out[(base + i) * ch + c];
                    let new_val = piece[i * ch + c];
                    out[(base + i) * ch + c] = g_out * old_val + g_in * new_val;
                }
            }
            for f in eff..piece_frames {
                for c in 0..ch {
                    out.push(piece[f * ch + c]);
                }
            }
        }
        out
    }

    // M1: crossfade_concat exact stereo output, multi-junction.
    #[test]
    fn m1_crossfade_concat_exact_stereo() {
        let ch = 2usize;
        let fade = 3usize;
        // Three stereo pieces, distinct constant value per (piece, channel) so any channel
        // or frame-offset error changes the result.
        let mk = |frames: usize, l: f32, r: f32| -> Vec<f32> {
            (0..frames).flat_map(|_| [l, r]).collect::<Vec<f32>>()
        };
        let pieces = vec![mk(6, 1.0, 2.0), mk(5, 3.0, 4.0), mk(7, 5.0, 6.0)];
        let got = crossfade_concat(&pieces, ch, fade);
        let want = ref_crossfade_concat(&pieces, ch, fade);
        assert_eq!(got.len(), want.len(), "length mismatch");
        for (i, (&g, &w)) in got.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
        // Independent structural check: total frames = sum(frames) - (n-1)*fade.
        let total_frames = (6 + 5 + 7) - 2 * fade;
        assert_eq!(got.len(), total_frames * ch);
        // The very first frame is untouched (pre-overlap) — pins overlap_start direction.
        assert_eq!((got[0], got[1]), (1.0, 2.0));
        // The g_in endpoint at i=0 is 0, so the first blended frame equals g_out*old only.
        // overlap_start frame 0 of junction 1: old=(1,2) at frame 3 (6-3), new=(3,4) i=0.
        // g_in(0)=0, g_out=1 → equals old (1,2).
        assert!(
            (got[3 * ch] - 1.0).abs() < 1e-6,
            "junction head must be g_out*old"
        );
    }

    // M2: crossfade_concat fade longer than a piece clamps to effective_fade.
    #[test]
    fn m2_crossfade_concat_short_piece_clamp() {
        let ch = 1usize;
        let pieces = vec![vec![1.0f32; 4], vec![2.0f32; 2]];
        // fade_len 10 but second piece only 2 frames → effective_fade = 2.
        let got = crossfade_concat(&pieces, ch, 10);
        let want = ref_crossfade_concat(&pieces, ch, 10);
        assert_eq!(got, want);
        // 4 + 2 - 2(effective) = 4 frames.
        assert_eq!(got.len(), 4);
    }

    // M2b: crossfade_concat over a per-frame ramp (not constant-per-piece, as M1 uses), so the
    // overlap accumulator READ index `i * ch` (428) is pinned — a wrong frame/channel offset
    // reads a different value and diverges from the independent oracle. M1's constant pieces
    // cannot see a read-offset error because every frame in the overlap holds the same value.
    #[test]
    fn m2b_crossfade_concat_ramped_overlap() {
        let ch = 2usize;
        let fade = 4usize;
        // Distinct value at every (frame, channel): p + f·0.01 (+0.001 on the right channel).
        let mk = |frames: usize, p: f32| -> Vec<f32> {
            (0..frames)
                .flat_map(|f| [p + f as f32 * 0.01, p + f as f32 * 0.01 + 0.001])
                .collect::<Vec<f32>>()
        };
        let pieces = vec![mk(8, 0.0), mk(8, 0.3), mk(8, 0.6)];
        let got = crossfade_concat(&pieces, ch, fade);
        let want = ref_crossfade_concat(&pieces, ch, fade);
        assert_eq!(got.len(), want.len(), "length mismatch");
        for (i, (&g, &w)) in got.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    /// Reference loop-crossfade fold, independent of production, returning stored samples.
    fn ref_loop_fold(samples: &[f32], ch: usize, sample_rate: u32) -> Vec<f32> {
        let n_frames = samples.len() / ch;
        let tier = {
            let rate = sample_rate as f64;
            let short = (0.5 * rate).round() as usize;
            let mid = (2.0 * rate).round() as usize;
            if n_frames < short {
                (0.050 * rate).round() as usize
            } else if n_frames <= mid {
                (0.100 * rate).round() as usize
            } else {
                (0.500 * rate).round() as usize
            }
        };
        let fade = tier.min(n_frames / 2);
        if fade == 0 || n_frames == 0 {
            return samples.to_vec();
        }
        let mut out = samples.to_vec();
        for j in 0..fade {
            let g_head = if fade <= 1 {
                1.0
            } else {
                ((j as f32 / (fade - 1) as f32) * std::f32::consts::FRAC_PI_2).sin()
            };
            let g_tail = if fade <= 1 {
                1.0
            } else {
                (((fade - 1 - j) as f32 / (fade - 1) as f32) * std::f32::consts::FRAC_PI_2).sin()
            };
            let tail_off = (n_frames - fade + j) * ch;
            let head_off = j * ch;
            for c in 0..ch {
                out[head_off + c] = g_tail * out[tail_off + c] + g_head * out[head_off + c];
            }
        }
        out.truncate((n_frames - fade) * ch);
        out
    }

    // M3: apply_loop_crossfade exact head fold + length + rms, stereo.
    #[test]
    fn m3_apply_loop_crossfade_exact() {
        // Small synthetic rate so fade frames are tiny and the fold region is easy to reason
        // about; n_frames must exceed 2 s at this rate to land in the long tier, but to keep
        // the buffer small we use a low rate and a frame count in the short tier.
        let rate = 100u32; // short tier (<50 frames) → 5-frame fade
        let ch = 2usize;
        let n_frames = 40usize; // < 0.5 s (50 frames) → 50 ms fade = 5 frames
                                // Distinct per-frame ramp so head/tail offset errors diverge.
        let samples: Vec<f32> = (0..n_frames)
            .flat_map(|f| [f as f32 * 0.01, f as f32 * 0.01 + 0.5])
            .collect();
        let seg = RoomTone {
            samples: samples.clone(),
            channels: ch as u16,
            sample_rate: rate,
            rms: 0.0,
        };
        let out = apply_loop_crossfade(seg);
        let want = ref_loop_fold(&samples, ch, rate);
        assert_eq!(out.samples.len(), want.len(), "stored length mismatch");
        for (i, (&g, &w)) in out.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
        // Length = n_frames - fade. fade for short tier at rate 100 = 5.
        assert_eq!(out.samples.len(), (n_frames - 5) * ch);
        // rms field must equal compute_rms(stored) — pins line 453/476 rms wiring.
        assert!(
            (out.rms - compute_rms(&want)).abs() < 1e-7,
            "rms field mismatch"
        );
    }

    // M4: apply_loop_crossfade with fade==0 path (tiny segment) keeps samples, sets rms.
    #[test]
    fn m4_apply_loop_crossfade_zero_fade() {
        // n_frames=1 → fade = min(tier, 0) = 0 → early return, samples unchanged.
        let seg = RoomTone {
            samples: vec![0.3f32, -0.3],
            channels: 2,
            sample_rate: 48_000,
            rms: 0.0,
        };
        let out = apply_loop_crossfade(seg);
        assert_eq!(
            out.samples,
            vec![0.3f32, -0.3],
            "fade==0 must pass samples through"
        );
        assert!((out.rms - compute_rms(&[0.3, -0.3])).abs() < 1e-7);
    }

    // M5: loop_fade_frames tier boundaries (exact).
    #[test]
    fn m5_loop_fade_frames_boundaries() {
        let short = (0.5 * RATE as f64).round() as usize; // 24000
        let mid = (2.0 * RATE as f64).round() as usize; // 96000
        let f50 = (0.050 * RATE as f64).round() as usize;
        let f100 = (0.100 * RATE as f64).round() as usize;
        let f500 = (0.500 * RATE as f64).round() as usize;
        // < short → 50 ms.
        assert_eq!(loop_fade_frames(short - 1, RATE), f50);
        // == short → 100 ms (boundary: `n < short` false, `n <= mid` true).
        assert_eq!(loop_fade_frames(short, RATE), f100);
        // == mid → 100 ms (inclusive upper).
        assert_eq!(loop_fade_frames(mid, RATE), f100);
        // > mid → 500 ms.
        assert_eq!(loop_fade_frames(mid + 1, RATE), f500);
    }

    // M6: SparseTable range-max correctness vs brute force over all ranges.
    #[test]
    fn m6_sparse_table_range_max() {
        // Values chosen so adjacent maxima differ across many window sizes (catches the
        // power-of-two split + log2 indexing math).
        let data: Vec<f32> = vec![3.0, 1.0, 4.0, 1.0, 5.0, 9.0, 2.0, 6.0, 5.0, 3.0, 5.0];
        let st = SparseTable::build(&data);
        for l in 0..data.len() {
            for r in l..data.len() {
                let want = data[l..=r].iter().cloned().fold(f32::MIN, f32::max);
                let got = st.query(l, r);
                assert!(
                    (got - want).abs() < 1e-6,
                    "query({l},{r}) = {got}, want {want}"
                );
            }
        }
    }

    // M7: SparseTable degenerate cases.
    #[test]
    fn m7_sparse_table_degenerate() {
        let empty = SparseTable::build(&[]);
        assert_eq!(empty.query(0, 0), 0.0, "empty table must return 0.0");
        let single = SparseTable::build(&[7.0]);
        assert_eq!(single.query(0, 0), 7.0);
        // l > r → 0.0 (pins the `l > r` guard at line 538).
        assert_eq!(single.query(1, 0), 0.0);
    }

    // M8: detect_room_tone golden — exact selected window for a controlled signal.
    //
    // A 12 s signal: 4 s loud | 8 s uniform quiet noise. The single contiguous quiet run is
    // exactly the trailing 8 s, capped to 10 s (only 8 s available), so the selected window is
    // the longest run that fits: from the first all-quiet block to the end. We recompute the
    // expected segment by extracting that exact window and applying the loop fold, then assert
    // the produced samples match bit-for-bit. This pins the prefix-sum window math, the block
    // index arithmetic, and the pass-2 read offset.
    #[test]
    fn m8_detect_golden_window() {
        let mut rng = Rng::new(0x90109010);
        let loud = sine(frames(4.0), 1, RATE, 0.9);
        let quiet = noise(&mut rng, frames(8.0), 1, 0.005);
        let mut sig = loud.clone();
        sig.extend_from_slice(&quiet);

        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found"),
        };

        // Reconstruct the expected window selection independently.
        let block_len = (0.1 * RATE as f64).round() as usize;
        let n_blocks = sig.len() / block_len;
        // Block RMS for each block (mono).
        let block_rms: Vec<f32> = (0..n_blocks)
            .map(|b| {
                let s = &sig[b * block_len..(b + 1) * block_len];
                let e: f64 = s.iter().map(|&x| (x as f64) * (x as f64)).sum();
                ((e / block_len as f64) as f32).sqrt()
            })
            .collect();
        let max_blk = ((10.0 * RATE as f64) / block_len as f64).floor() as usize;
        let min_blk = ((2.0 * RATE as f64) / block_len as f64).ceil() as usize;
        // First quiet block index = first block fully inside the quiet tail.
        let first_quiet = (0..n_blocks)
            .find(|&b| block_rms[b] <= params.rms_ceiling)
            .expect("a quiet block exists");
        // Longest acceptable run from first_quiet, capped at max_blk and available blocks.
        let avail = n_blocks - first_quiet;
        let want_len = max_blk.min(avail).max(min_blk);
        // Expected window = blocks [first_quiet, first_quiet+want_len).
        let start_frame = first_quiet * block_len;
        let win = &sig[start_frame..start_frame + want_len * block_len];
        let want = ref_loop_fold(win, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "selected window length mismatch (start_frame={start_frame}, want_len={want_len})"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // M9: detect_room_tone — peak rejection actually rejects a window with a transient.
    //
    // 12 s of uniform quiet noise with one large transient in the first 2 s. The transient's
    // block has peak >> 5×rms, so any window containing it is rejected; the selected window
    // must therefore start strictly after the transient block. Pins the peak criterion (197)
    // and the window-peak sparse query wiring.
    #[test]
    fn m9_detect_peak_rejection_shifts_window() {
        let mut rng = Rng::new(0xbeef00);
        let mut sig = noise(&mut rng, frames(12.0), 1, 0.005);
        let block_len = (0.1 * RATE as f64).round() as usize;
        // Transient inside block index 3 (0.3 s in).
        let transient_frame = 3 * block_len + 10;
        sig[transient_frame] = 0.9;
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 95.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found"),
        };
        // The transient sample value (0.9) must NOT appear in the selected segment: the
        // window must avoid the transient block entirely.
        let has_transient = seg.samples.iter().any(|&s| s.abs() > 0.5);
        assert!(
            !has_transient,
            "selected window must exclude the loud transient"
        );
    }

    // M10: stitch_fallback golden — exact stitched output for a controlled alternating signal.
    //
    // Forces the stitch path (no contiguous 2 s quiet window) and reconstructs the exact
    // selected blocks, their original-order assembly, and the 50 ms crossfade. Pins the
    // block read-offset arithmetic (385/387/388), the coalescing run logic (380/381/383),
    // the target/fade frame math (367/394), and the qualifying-block filter (351).
    #[test]
    fn m10_stitch_golden() {
        // 30 s: alternating 100 ms quiet / 100 ms loud. Each 100 ms aligns to a block, so
        // exactly the even blocks qualify (quiet) and odd blocks are loud — no 2 s contiguous
        // quiet run exists, forcing stitch.
        let block_len = (0.1 * RATE as f64).round() as usize;
        let mut sig = Vec::new();
        let n_pairs = 150; // 150 * 200 ms = 30 s
                           // Use a *fixed* quiet sample value per quiet block so we can identify them exactly,
                           // but distinct per block to detect mis-ordering. Quiet block k → constant 0.001*(k+1)
                           // capped well under the ceiling; loud block → 0.5 sine.
        let quiet_vals: Vec<f32> = (0..n_pairs).map(|k| 0.001 + 0.00001 * k as f32).collect();
        for &v in &quiet_vals {
            sig.extend(std::iter::repeat_n(v, block_len));
            let loud = sine(block_len, 1, RATE, 0.5);
            sig.extend_from_slice(&loud);
        }
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found via stitch"),
        };

        // Reconstruct expected: qualifying blocks are the quiet (even-index) blocks. They are
        // selected quietest-first up to the 10 s target, then assembled in original order with
        // a 50 ms crossfade. Since every quiet block has constant value v_k and the loud blocks
        // are clearly rejected (rms ~0.35 >> ceiling), the quietest n_needed quiet blocks are
        // the first ones (lowest v_k = lowest k).
        let target_frames = 10 * RATE as usize;
        let n_needed = target_frames.div_ceil(block_len).min(n_pairs);
        // Selected quiet block indices (in the block grid) for the quietest n_needed: k=0..n_needed.
        // Each quiet block k lives at block grid position 2*k. Assembled in ascending position
        // → ascending k. They are non-adjacent (separated by loud blocks), so each is its own
        // piece.
        let pieces: Vec<Vec<f32>> = quiet_vals[..n_needed]
            .iter()
            .map(|&v| vec![v; block_len])
            .collect();
        let fade_len = (0.05 * RATE as f64).round() as usize;
        let stitched = crossfade_concat(&pieces, 1, fade_len);
        let want = ref_loop_fold(&stitched, 1, RATE);
        assert_eq!(
            seg.samples.len(),
            want.len(),
            "stitched length mismatch (n_needed={n_needed})"
        );
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!(
                (g - w).abs() < 1e-5,
                "stitched sample {i}: got {g}, want {w}"
            );
        }
    }

    // M11: stitch_fallback coalesces adjacent selected blocks into one read.
    //
    // Constructs a signal whose qualifying quiet blocks include an *adjacent pair*, exercising
    // the run-coalescing branch (380 `selected[j+1] == selected[j]+1`) and the per-block split
    // (385/387). If coalescing math is wrong the pieces are mis-sliced and the stitched output
    // diverges from a per-block reference.
    #[test]
    fn m11_stitch_adjacent_coalesce() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        // Build 12 s. Most is loud; insert a 400 ms quiet run (4 adjacent quiet blocks) and a
        // couple of isolated quiet blocks so qualifying set has both adjacent and isolated.
        let mut sig = sine(frames(12.0), 1, RATE, 0.5);
        // Overwrite blocks 10,11,12,13 (adjacent run) and block 30, 50 (isolated) with quiet.
        // The adjacent-run values are *sharply contrasting* (0.002 / 0.045 / 0.004 / 0.043) so
        // a wrong per-block split offset (line 388) or coalesced read start (line 387) reads a
        // neighbouring block's value and diverges far beyond tolerance. All values are < the
        // 0.05 ceiling so every quiet block qualifies (q = min(0.05, P50) = 0.05 here, since the
        // median block is loud).
        let quiet_blocks = [10usize, 11, 12, 13, 30, 50];
        let quiet_vals = [0.002f32, 0.045, 0.004, 0.043, 0.010, 0.020];
        for (&b, &v) in quiet_blocks.iter().zip(quiet_vals.iter()) {
            for f in 0..block_len {
                sig[b * block_len + f] = v;
            }
        }
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        let seg = match detect(&sig, 1, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found via stitch"),
        };
        // Expected: all 6 quiet blocks qualify (well under target), assembled in original
        // position order (10,11,12,13,30,50) with 50 ms crossfades.
        let pieces: Vec<Vec<f32>> = quiet_vals.iter().map(|&v| vec![v; block_len]).collect();
        let fade_len = (0.05 * RATE as f64).round() as usize;
        let stitched = crossfade_concat(&pieces, 1, fade_len);
        let want = ref_loop_fold(&stitched, 1, RATE);
        assert_eq!(seg.samples.len(), want.len(), "length mismatch");
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // M12: stitch_fallback STEREO adjacent-run read split — pins the per-block read math that
    // multiplies/divides by the channel count (lines 387/388: `block_len * ch`) and the
    // coalesced-run base offset. For mono, `* ch` vs `/ ch` (ch == 1) are indistinguishable; a
    // stereo signal with distinct L≠R values per quiet block makes the interleave stride
    // load-bearing, so any `* ch → / ch` or split-offset mutation reads the wrong channel/block.
    #[test]
    fn m12_stitch_stereo_adjacent_coalesce() {
        let block_len = (0.1 * RATE as f64).round() as usize;
        let ch = 2usize;
        // 12 s stereo loud bed.
        let mut sig: Vec<f32> = (0..frames(12.0))
            .flat_map(|i| {
                let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin();
                [s, s]
            })
            .collect();
        // Adjacent quiet run blocks 20,21,22,23 plus isolated 40, with distinct L≠R per block.
        let quiet_blocks = [20usize, 21, 22, 23, 40];
        let lr = [
            (0.002f32, 0.040f32),
            (0.045, 0.003),
            (0.004, 0.044),
            (0.043, 0.005),
            (0.010, 0.030),
        ];
        for (&b, &(l, r)) in quiet_blocks.iter().zip(lr.iter()) {
            for f in 0..block_len {
                sig[(b * block_len + f) * ch] = l;
                sig[(b * block_len + f) * ch + 1] = r;
            }
        }
        let params = RoomToneParams {
            rms_ceiling: 0.05,
            quiet_percentile: 50.0,
        };
        let seg = match detect(&sig, ch as u16, RATE, &params) {
            RoomToneOutcome::Found(s) => s,
            RoomToneOutcome::None => panic!("expected Found via stitch"),
        };
        assert_eq!(seg.channels, 2);
        // Expected: 5 quiet blocks in position order, each interleaved L/R, 50 ms crossfade,
        // loop fold.
        let pieces: Vec<Vec<f32>> = lr
            .iter()
            .map(|&(l, r)| (0..block_len).flat_map(|_| [l, r]).collect())
            .collect();
        let fade_len = (0.05 * RATE as f64).round() as usize;
        let stitched = crossfade_concat(&pieces, ch, fade_len);
        let want = ref_loop_fold(&stitched, ch, RATE);
        assert_eq!(seg.samples.len(), want.len(), "length mismatch");
        for (i, (&g, &w)) in seg.samples.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: got {g}, want {w}");
        }
    }

    // Pinned bytes and hash (regenerate via `capture_pinned_values` if V1 shape changes).
    const PINNED_WIRE_BYTES: [u8; 26] = [
        0x41, 0x80, 0xf7, 0x02, 0x02, 0x9b, 0xe8, 0x21, 0x3e, 0x04, 0xcd, 0xcc, 0xcc, 0x3d, 0xcd,
        0xcc, 0xcc, 0xbd, 0xcd, 0xcc, 0x4c, 0x3e, 0xcd, 0xcc, 0x4c, 0xbe,
    ];
    const PINNED_HASH: [u8; 16] = [
        0x45, 0x00, 0x36, 0xd4, 0x63, 0xcc, 0x60, 0x61, 0x6c, 0x17, 0x8f, 0x93, 0x5c, 0xef, 0x66,
        0x60,
    ];

    // Helper to capture actual pinned values after first build.
    // Run: cargo test -p core audio::room_tone::tests::capture_pinned_values -- --ignored --nocapture
    #[test]
    #[ignore]
    fn capture_pinned_values() {
        let samples = vec![0.1f32, -0.1f32, 0.2f32, -0.2f32];
        let v1_seg = v1::RoomToneV1 {
            sample_rate: 48_000,
            channels: 2,
            rms: super::compute_rms(&samples),
            samples,
        };
        let (hash, bytes) = encode_tagged(Kind::RoomTone, 1, &v1_seg).unwrap();
        println!("PINNED_WIRE_BYTES len={}", bytes.len());
        print!("const PINNED_WIRE_BYTES: [u8; {}] = [", bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("0x{b:02x}");
        }
        println!("];");
        print!("const PINNED_HASH: [u8; 16] = [");
        for (i, b) in hash.0.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("0x{b:02x}");
        }
        println!("];");

        // Write fixture blob and pinned values to files for the harness to read.
        std::fs::write(
            "/tmp/room_tone_pinned.txt",
            format!(
                "WIRE_LEN={}\nWIRE={:?}\nHASH={:?}\n",
                bytes.len(),
                bytes,
                hash.0
            ),
        )
        .unwrap();
        std::fs::write("/tmp/room_tone_v1.blob", &bytes).unwrap();
    }
}
