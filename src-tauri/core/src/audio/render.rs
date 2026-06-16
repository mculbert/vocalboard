//! Pull-based EDL renderer: turns `MixSlice` descriptors into stereo f32 PCM frames.
//!
//! Layered from the inside out:
//! - per-segment flat render (Source / Silence / RoomTone, no crossfades), wet/dry blend,
//!   mono→stereo up-mix;
//! - [`Renderer`] — multi-track mix + clamp + the pull loop (seamless, no crossfades);
//! - [`FadeAccumulator`] — project-wide stereo ring for seam-fade handle contributions, wired into
//!   the render loop (`drain_add` before clamp);
//! - symmetric centered seam crossfades — body kept-tail / own-head faded inline, forward/backward
//!   source handles read across the seam, ramped, and deposited into the shared ring, placed via
//!   bounded look-ahead over upcoming slice descriptors;
//! - asymmetric, room-tone, and one-sided degraded seams.
//!
//! See design/audio-pipeline.md § Zero-crossing and crossfade.

use std::collections::{BTreeMap, VecDeque};

use super::edl::{EdlCursor, EdlSegment, MixSlice};
use super::{equal_power_gain, AudioError, PcmSource};
use crate::project::turn::SpliceKind;

// ---------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------

/// Supplies PCM for a track's three render sources (dry cache, enhanced FLAC,
/// room-tone blob). Implementations open files lazily; tests inject in-memory PCM.
/// No SQLite/journal access. All PCM reads are ranged.
pub trait SourceProvider {
    /// `n` frames of the project-rate dry (resampled-cache) signal for `track_id`
    /// starting at cache offset `from`. Interleaved at the source channel count.
    fn dry(&mut self, track_id: u32, from: i64, n: i64) -> Result<Vec<f32>, AudioError>;

    /// Enhanced signal for the same range, or `None` when no enhanced file exists
    /// (the renderer then uses dry; no regeneration on this path).
    fn enhanced(
        &mut self,
        track_id: u32,
        from: i64,
        n: i64,
    ) -> Result<Option<Vec<f32>>, AudioError>;

    /// The track's room-tone loop segment (interleaved at the source channel count,
    /// project rate), or `None` when no room tone is available.
    fn room_tone(&mut self, track_id: u32) -> Result<Option<&[f32]>, AudioError>;

    /// Source channel count for `track_id` (1 ⇒ up-mix to stereo; 2 ⇒ passthrough).
    fn channels(&self, track_id: u32) -> u16;

    /// Per-track wet/dry mix ratio in [0.0, 1.0] (0 = full dry, 1 = full enhanced).
    fn wet_ratio(&self, track_id: u32) -> f32;

    /// Total length in frames of the track's dry (resampled-cache) source. The renderer
    /// clamps seam handles to `[0, source_len)` so it never reads past EOF or before the
    /// origin; a handle with no valid source falls back to a one-sided in-extent fade.
    fn source_len(&self, track_id: u32) -> i64;
}

// ---------------------------------------------------------------------------
// Segment-level render (flat, no fades, no seam awareness)
// ---------------------------------------------------------------------------

/// Render one `EdlSegment` flat (no fades) over `length` project-rate frames.
///
/// Returns `(stereo_frames, new_loop_phase)` where `stereo_frames` is interleaved
/// stereo f32 of length `2 * length`. `loop_phase` carries the room-tone loop
/// position across segment boundaries; it is advanced for `RoomTone` and passed
/// through for `Source` / `Silence`.
///
/// For a `Source` segment the read position is `source_start_sample + offset_in_splice`.
pub(crate) fn render_segment(
    seg: &EdlSegment,
    length: i64,
    provider: &mut impl SourceProvider,
    loop_phase: usize,
) -> Result<(Vec<f32>, usize), AudioError> {
    let n = length as usize;
    let ch = provider.channels(seg.track_id) as usize;

    match seg.splice.kind {
        SpliceKind::Source {
            source_start_sample,
        } => {
            let from = source_start_sample + seg.offset_in_splice;
            let stereo = read_source_blended_stereo(provider, seg.track_id, from, length)?;
            Ok((stereo, loop_phase))
        }
        SpliceKind::Silence => Ok((vec![0.0f32; 2 * n], loop_phase)),
        SpliceKind::RoomTone => {
            // Clone the tone slice so the &mut borrow on `provider` ends before
            // calling upmix_to_stereo (which does not touch `provider`).
            let tone: Option<Vec<f32>> = provider.room_tone(seg.track_id)?.map(|s| s.to_vec());
            match tone {
                None => Ok((vec![0.0f32; 2 * n], loop_phase)),
                Some(tone) => {
                    let n_ch = ch.max(1);
                    let tone_frames = tone.len() / n_ch;
                    if tone_frames == 0 {
                        return Ok((vec![0.0f32; 2 * n], loop_phase));
                    }
                    let mut src = Vec::with_capacity(n * n_ch);
                    for i in 0..n {
                        let frame_start = ((loop_phase + i) % tone_frames) * n_ch;
                        src.extend_from_slice(&tone[frame_start..frame_start + n_ch]);
                    }
                    let new_phase = (loop_phase + n) % tone_frames;
                    Ok((upmix_to_stereo(&src, ch), new_phase))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wet/dry blend: `enhanced * wet_ratio + dry * (1 - wet_ratio)`.
/// Falls back to `dry` unchanged when `enhanced` is absent.
fn blend_wet_dry(dry: Vec<f32>, enhanced: Option<Vec<f32>>, wet_ratio: f32) -> Vec<f32> {
    match enhanced {
        None => dry,
        Some(enh) => {
            let dry_ratio = 1.0 - wet_ratio;
            enh.into_iter()
                .zip(dry)
                .map(|(e, d)| e * wet_ratio + d * dry_ratio)
                .collect()
        }
    }
}

/// Up-mix `src_channels`-channel PCM to interleaved stereo.
///
/// Mono (1 channel) duplicates to L = R with equal gain (not −3 dB).
/// Any other channel count passes through unchanged.
fn upmix_to_stereo(samples: &[f32], src_channels: usize) -> Vec<f32> {
    if src_channels == 1 {
        let mut out = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            out.push(s);
            out.push(s);
        }
        out
    } else {
        samples.to_vec()
    }
}

/// Read `n` frames of a track's `Source` signal at cache offset `from`, wet/dry-blended
/// and up-mixed to interleaved stereo. Shared by the in-extent segment body and the
/// out-of-extent seam handles — both are ordinary ranged reads at the project rate, so a
/// handle gets the identical blend + up-mix as the audio it crossfades with.
fn read_source_blended_stereo(
    provider: &mut impl SourceProvider,
    track_id: u32,
    from: i64,
    n: i64,
) -> Result<Vec<f32>, AudioError> {
    let ch = provider.channels(track_id) as usize;
    let dry = provider.dry(track_id, from, n)?;
    let maybe_enh = provider.enhanced(track_id, from, n)?;
    let wet = provider.wet_ratio(track_id);
    let blended = blend_wet_dry(dry, maybe_enh, wet);
    Ok(upmix_to_stereo(&blended, ch))
}

/// Equal-power fade-**in** gain at ramp index `i` of `len`: rises `0 → 1`.
#[inline]
fn fade_in_gain(i: usize, len: usize) -> f32 {
    equal_power_gain(i, len)
}

/// Equal-power fade-**out** gain at ramp index `i` of `len`: falls `1 → 0`, the complement
/// of [`fade_in_gain`] (`fade_in² + fade_out² == 1`).
#[inline]
fn fade_out_gain(i: usize, len: usize) -> f32 {
    equal_power_gain(len - 1 - i, len)
}

// ---------------------------------------------------------------------------
// Fade accumulator
// ---------------------------------------------------------------------------

/// Project-wide stereo ring of pending seam-fade handle contributions, indexed
/// by absolute output frame (interleaved stereo). Pre-allocated to
/// `max_fade_samples` frames on the pre-roll side; never allocates or grows on
/// the pull path.
///
/// Every seam on every track deposits its two handle halves here (additively);
/// the render loop drains the window for each emitted chunk, sums it into the
/// mix before clamping, then advances the base position.
///
/// In-extent kept/own halves are faded inline by the segment body and never go
/// through this ring. "Per-seam generation, project-wide storage."
struct FadeAccumulator {
    /// Interleaved stereo ring; `ring.len() == cap_frames * 2`.
    ring: Vec<f32>,
    /// Capacity in frames; equal to `max_fade_samples.max(1)`.
    cap_frames: usize,
    /// Absolute output frame that maps to `ring[0]` modulo `cap_frames`.
    base_pos: i64,
}

impl FadeAccumulator {
    /// Pre-allocate the ring for `max_fade_samples` frames.
    ///
    /// Passing 0 is safe: the ring degrades to a 1-frame capacity that always
    /// drains zero until a seam crossfade deposits something.
    fn new(max_fade_samples: usize) -> Self {
        let cap_frames = max_fade_samples.max(1);
        Self {
            ring: vec![0.0f32; cap_frames * 2],
            cap_frames,
            base_pos: 0,
        }
    }

    /// Additively deposit `frames` (interleaved stereo) starting at absolute
    /// output frame `at`.
    ///
    /// The look-ahead contract guarantees `at` is within the live window
    /// `[base_pos, base_pos + cap_frames)` — the caller is responsible for
    /// ensuring this before depositing.
    fn deposit(&mut self, at: i64, frames: &[f32]) {
        let n_frames = frames.len() / 2;
        for i in 0..n_frames {
            // Slot is purely absolute-position modulo capacity; base_pos is not part
            // of the slot formula — it only tracks the live window for the look-ahead
            // contract and is not used here.
            let slot = ((at + i as i64) as usize) % self.cap_frames;
            self.ring[slot * 2] += frames[i * 2];
            self.ring[slot * 2 + 1] += frames[i * 2 + 1];
        }
    }

    /// Sum pending contributions for `[from, from + n)` into `out` (interleaved
    /// stereo, `out.len() == 2 * n`), clear those ring cells, and advance
    /// `base_pos` to `from + n`. Empty cells contribute zero.
    fn drain_add(&mut self, from: i64, n: usize, out: &mut [f32]) {
        for i in 0..n {
            let slot = ((from + i as i64) as usize) % self.cap_frames;
            out[i * 2] += self.ring[slot * 2];
            out[i * 2 + 1] += self.ring[slot * 2 + 1];
            self.ring[slot * 2] = 0.0;
            self.ring[slot * 2 + 1] = 0.0;
        }
        self.base_pos = from + n as i64;
    }
}

// ---------------------------------------------------------------------------
// Seam detection
// ---------------------------------------------------------------------------

/// A detected crossfade seam at project position `e` on one track: the point where the
/// track's outgoing splice (ending at `e`) hands off to a different incoming splice
/// (starting at `e`). Detection is purely structural — both halves carry their **own**
/// fade length, so the seam stores them independently (the centered model never needs
/// to pair `fo` with `fi`; this is what makes asymmetric `fo ≠ fi` fall out for free).
///
/// Records seams whose sides are both non-`Silence` (`Source` ↔ `RoomTone` in either
/// direction — the gap fade — as well as `Source` ↔ `Source`). A `Source` side reads its
/// handle from the cache; a `RoomTone` side continues the loop phase. Silence sides (lead-in,
/// project edges) are degraded one-sided fades.
#[derive(Clone, Copy, Debug)]
struct Seam {
    /// Seam position (outgoing tail end == incoming head start).
    e: i64,
    track_id: u32,
    /// Outgoing splice `fade_out_samples`.
    fo: i64,
    /// Incoming splice `fade_in_samples`.
    fi: i64,
    /// Outgoing splice kind (selects the forward-handle source: cache vs looped tone).
    out_kind: SpliceKind,
    /// Incoming splice kind (selects the backward-handle source).
    in_kind: SpliceKind,
    /// Outgoing `source_start_sample` (forward handle reads past its trimmed end); `Source` only.
    out_source_start: i64,
    /// Outgoing splice length (forward handle = source at `out_source_start + out_splice_len`).
    out_splice_len: i64,
    /// Incoming `source_start_sample` (backward handle reads before it); `Source` only.
    in_source_start: i64,
    /// Forward-handle frames the outgoing side can actually supply (`≤ ⌊fo/2⌋`); a `Source`
    /// side is clamped to the cache EOF, a `RoomTone` side (looping) always supplies the full
    /// `⌊fo/2⌋`. A short value zero-pads the remainder of the handle.
    fwd_n: i64,
    /// Backward-handle frames the incoming side can supply (`≤ ⌈fi/2⌉`); clamped to the source
    /// origin for a `Source` side.
    bwd_n: i64,
    /// Outgoing side has **no** forward handle (Source ending at EOF): the fade-out degrades to
    /// a one-sided pre-fade over `[e − fo, e)` within the splice's own extent.
    out_onesided: bool,
    /// Incoming side has **no** backward handle (Source starting at the origin): the fade-in
    /// degrades to a one-sided post-fade over `[e, e + fi)`.
    in_onesided: bool,
    /// Whether the two handle halves have been deposited into the ring yet.
    deposited: bool,
}

impl Seam {
    /// Earliest output frame this seam touches: the start of the incoming **backward
    /// handle** at `e − ⌈fi/2⌉`. The handle must be deposited the instant emission reaches
    /// this frame (its window then fits exactly within the pre-allocated ring).
    fn deadline(&self) -> i64 {
        self.e - (self.fi + 1) / 2
    }
}

/// Per-track record of the most recently **scanned** segment, used to detect a seam when
/// the next segment for that track arrives: a seam exists when the previous segment
/// consumed its splice's tail and the current one starts a fresh splice.
#[derive(Clone, Copy)]
struct ScanLast {
    kind: SpliceKind,
    splice_len: i64,
    fade_out: i64,
    offset_in_splice: i64,
    covered_len: i64,
    end_pos: i64,
}

/// Apply a seam's **in-extent body** fades to a segment's already-rendered stereo chunk,
/// in place. The kept-tail of an outgoing splice (`[e − ⌈fo/2⌉, e)`) fades out; the own-head
/// of an incoming splice (`[e, e + ⌊fi/2⌋)`) fades in. Both are addressed by absolute output
/// frame, so a fade split across continuation slices ramps continuously. The out-of-extent
/// handle halves do **not** pass through here — they go through the ring.
fn apply_body_fades(
    frames: &mut [f32],
    track_id: u32,
    slice_start: i64,
    slice_len: i64,
    chunk_start: i64,
    chunk_len: i64,
    seams: &BTreeMap<(i64, u32), Seam>,
) {
    // Outgoing kept-tail: this slice ends at the seam `e`. Centered fades reach the splice's
    // own audio over [e − ⌈fo/2⌉, e) (the handle completes the rest); a degraded one-sided
    // pre-fade runs the full ramp over [e − fo, e) within the splice itself.
    let e_out = slice_start + slice_len;
    if let Some(seam) = seams.get(&(e_out, track_id)) {
        let fo = seam.fo;
        if fo > 0 {
            let region_start = if seam.out_onesided {
                e_out - fo
            } else {
                e_out - (fo + 1) / 2
            };
            for local in 0..chunk_len {
                let f = chunk_start + local;
                if f >= region_start && f < e_out {
                    let g = fade_out_gain((f - region_start) as usize, fo as usize);
                    frames[2 * local as usize] *= g;
                    frames[2 * local as usize + 1] *= g;
                }
            }
        }
    }
    // Incoming own-head: this slice starts at the seam `e`. Centered over [e, e + ⌊fi/2⌋)
    // (ramp continues from ⌈fi/2⌉); a degraded one-sided post-fade runs the full ramp over
    // [e, e + fi) from 0.
    if let Some(seam) = seams.get(&(slice_start, track_id)) {
        let fi = seam.fi;
        if fi > 0 {
            let (region_end, ramp_base) = if seam.in_onesided {
                (slice_start + fi, 0)
            } else {
                (slice_start + fi / 2, (fi + 1) / 2)
            };
            for local in 0..chunk_len {
                let f = chunk_start + local;
                if f >= slice_start && f < region_end {
                    let i = (f - slice_start) as usize + ramp_base as usize;
                    let g = fade_in_gain(i, fi as usize);
                    frames[2 * local as usize] *= g;
                    frames[2 * local as usize + 1] *= g;
                }
            }
        }
    }
}

/// Generate `n` stereo frames of a track's room-tone loop starting at loop phase
/// `phase_base` (which may be negative — `rem_euclid` wraps it), continuing the **same**
/// loop the room-tone *body* rides rather than restarting it. No blob ⇒ silence.
fn read_room_tone_handle(
    provider: &mut impl SourceProvider,
    track_id: u32,
    phase_base: i64,
    n: i64,
) -> Result<Vec<f32>, AudioError> {
    let ch = provider.channels(track_id) as usize;
    let tone: Option<Vec<f32>> = provider.room_tone(track_id)?.map(|s| s.to_vec());
    let n_ch = ch.max(1);
    match tone {
        Some(tone) if tone.len() / n_ch > 0 => {
            let frames = (tone.len() / n_ch) as i64;
            let mut src = Vec::with_capacity(n as usize * n_ch);
            for i in 0..n {
                let p = (phase_base + i).rem_euclid(frames) as usize * n_ch;
                src.extend_from_slice(&tone[p..p + n_ch]);
            }
            Ok(upmix_to_stereo(&src, ch))
        }
        _ => Ok(vec![0.0f32; 2 * n as usize]),
    }
}

/// Read, ramp, and deposit a seam's two **handle** halves into the shared ring. Called at
/// the seam's deadline, when emission base is exactly `e − ⌈fi/2⌉`, so the full
/// `[e − ⌈fi/2⌉, e + ⌊fo/2⌋)` window fits within the pre-allocated ring.
///
/// - Forward handle (outgoing): faded **out** at `[e, e + ⌊fo/2⌋)`.
/// - Backward handle (incoming): faded **in** at `[e − ⌈fi/2⌉, e)`.
///
/// A `Source` side reads the cache past / before its extent; a `RoomTone` side continues the
/// loop phase. `loop_phase` is the track's running room-tone phase **at the deadline**; since
/// the loop only advances while room tone is emitted, the loop phase at `e` is `loop_phase`
/// plus the room-tone frames between the deadline and `e` — which is `⌈fi/2⌉` exactly when the
/// **outgoing** side is room tone (it is emitting over that span), and `0` otherwise.
fn deposit_handles(
    seam: &Seam,
    provider: &mut impl SourceProvider,
    acc: &mut FadeAccumulator,
    loop_phase: i64,
) -> Result<(), AudioError> {
    let fo = seam.fo;
    let fi = seam.fi;
    let ceil_fi = (fi + 1) / 2; // ⌈fi/2⌉
    let phase_at_e = loop_phase
        + if matches!(seam.out_kind, SpliceKind::RoomTone) {
            ceil_fi
        } else {
            0
        };

    // Forward handle (outgoing), output [e, e + fwd_n) — `fwd_n` frames of the ideal ⌊fo/2⌋,
    // clamped at cache EOF; the far end zero-pads. One-sided ⇒ no handle.
    let fwd_n = seam.fwd_n;
    if !seam.out_onesided && fwd_n > 0 {
        let mut stereo = match seam.out_kind {
            SpliceKind::Source { .. } => {
                let from = seam.out_source_start + seam.out_splice_len;
                read_source_blended_stereo(provider, seam.track_id, from, fwd_n)?
            }
            SpliceKind::RoomTone => {
                read_room_tone_handle(provider, seam.track_id, phase_at_e, fwd_n)?
            }
            SpliceKind::Silence => vec![0.0f32; 2 * fwd_n as usize],
        };
        let ceil_fo = ((fo + 1) / 2) as usize;
        for j in 0..fwd_n as usize {
            let g = fade_out_gain(ceil_fo + j, fo as usize);
            stereo[2 * j] *= g;
            stereo[2 * j + 1] *= g;
        }
        acc.deposit(seam.e, &stereo);
    }

    // Backward handle (incoming), output [e − bwd_n, e) — `bwd_n` frames of the ideal ⌈fi/2⌉,
    // clamped at the source origin; its near-E end is preserved and the far end dropped, so the
    // ramp index starts at `skip = ⌈fi/2⌉ − bwd_n`. One-sided ⇒ no handle.
    let bwd_n = seam.bwd_n;
    if !seam.in_onesided && bwd_n > 0 {
        let skip = (ceil_fi - bwd_n) as usize;
        let mut stereo = match seam.in_kind {
            SpliceKind::Source { .. } => {
                let from = seam.in_source_start - bwd_n;
                read_source_blended_stereo(provider, seam.track_id, from, bwd_n)?
            }
            SpliceKind::RoomTone => {
                read_room_tone_handle(provider, seam.track_id, phase_at_e - bwd_n, bwd_n)?
            }
            SpliceKind::Silence => vec![0.0f32; 2 * bwd_n as usize],
        };
        for j in 0..bwd_n as usize {
            let g = fade_in_gain(skip + j, fi as usize);
            stereo[2 * j] *= g;
            stereo[2 * j + 1] *= g;
        }
        acc.deposit(seam.e - bwd_n, &stereo);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Renderer — multi-track pull loop + centered seam crossfades
// ---------------------------------------------------------------------------

/// Pull-based renderer over an [`EdlCursor`] + [`SourceProvider`].
///
/// Drives all tracks in lockstep from the cursor's `MixSlice` stream: each slice's
/// segments are rendered flat, the **in-extent** seam fades are applied inline to the kept
/// tail / own head, segments are summed across tracks, the **out-of-extent** seam handles
/// are drained from the shared fade accumulator into the mix, and the result is clamped to
/// `[−1, 1]`. A bounded look-ahead over upcoming slice descriptors places each seam's handle
/// halves in the ring before emission reaches them. Output is interleaved stereo f32 at the
/// project rate. `'static + Send` when `P: Send + 'static` (the [`EdlCursor`] owns its
/// traversal state via `Arc` clones).
pub struct Renderer<P: SourceProvider> {
    cursor: EdlCursor,
    provider: P,
    /// Project sample rate in Hz; reported via [`PcmSource::sample_rate`].
    project_rate: u32,
    /// Structural fade bound: sizes the accumulator and the look-ahead depth.
    max_fade_samples: usize,
    /// Current absolute output frame position.
    pos: i64,
    /// Set once a [`PcmSource::read`] returns a short (under-filled) frame count, i.e. the
    /// cursor reached end-of-EDL / its `end` bound. Drives [`PcmSource::is_exhausted`].
    exhausted: bool,
    /// Slice currently being emitted, with the number of frames already consumed from it.
    current: Option<(MixSlice, i64)>,
    /// Future slices already pulled (and seam-scanned) but not yet emitted.
    lookahead: VecDeque<MixSlice>,
    /// Project position covered by every slice pulled so far (front of `current` through
    /// the back of `lookahead`); the look-ahead has scanned the timeline up to here.
    scanned_end: i64,
    /// Per-track last scanned segment, for structural seam detection across pulls.
    scan_last: BTreeMap<u32, ScanLast>,
    /// Detected seams keyed by `(e, track_id)`, awaiting (or past) their handle deposit.
    seams: BTreeMap<(i64, u32), Seam>,
    /// Per-track room-tone loop phase: frame offset into the loop buffer, threaded across
    /// segment boundaries so the loop is seamless when one splice spans multiple slices.
    loop_phases: BTreeMap<u32, usize>,
    /// Project-wide seam-fade handle ring; drained into the mix before clamping.
    acc: FadeAccumulator,
}

impl<P: SourceProvider> Renderer<P> {
    /// Create a renderer at position 0.
    ///
    /// `max_fade_samples` is the structural fade bound (M5 clamps stored fades to this; tests
    /// pass it explicitly). It pre-allocates the accumulator and sets the look-ahead
    /// depth; passing 0 disables all seam machinery (the pure flat-mix path).
    pub fn new(cursor: EdlCursor, provider: P, max_fade_samples: usize, project_rate: u32) -> Self {
        Self {
            cursor,
            provider,
            project_rate,
            max_fade_samples,
            pos: 0,
            exhausted: false,
            current: None,
            lookahead: VecDeque::new(),
            scanned_end: 0,
            scan_last: BTreeMap::new(),
            seams: BTreeMap::new(),
            loop_phases: BTreeMap::new(),
            acc: FadeAccumulator::new(max_fade_samples),
        }
    }

    /// Render up to `n_frames` interleaved stereo f32 frames into a freshly allocated `Vec`,
    /// advancing the cursor. Thin allocating convenience over
    /// [`read_frames`](Self::read_frames); the returned `Vec` holds exactly the frames
    /// produced (shorter than `n_frames` only at end-of-EDL, empty once the cursor is spent).
    pub fn render(&mut self, n_frames: usize) -> Result<Vec<f32>, AudioError> {
        let mut out = vec![0.0f32; n_frames * 2];
        let got = self.read_frames(&mut out)?;
        out.truncate(got * 2);
        Ok(out)
    }

    /// Fill `out` (length a multiple of two) with up to `out.len() / 2` interleaved stereo
    /// frames, returning the number of **frames** written and advancing the cursor.
    ///
    /// This is the primary render path — [`render`](Self::render) and the [`PcmSource`] impl
    /// both delegate here. The fill is greedy: it writes a short count only at end-of-EDL, so a
    /// short fill unambiguously signals the cursor is spent. The written region is fully
    /// overwritten; bytes past the returned frame count are left untouched.
    pub fn read_frames(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        let n_frames = out.len() / 2;
        let mut written = 0usize; // frames written into `out` so far
        let seam_mode = self.max_fade_samples > 0;
        // Look-ahead depth: enough to see a seam at `e` (and its incoming slice, start `e`)
        // before emission reaches the backward handle at `e − ⌈fi/2⌉`.
        let half_max = (self.max_fade_samples as i64 + 1) / 2;

        while written < n_frames {
            let needed = (n_frames - written) as i64;

            // Take the current slice (pull one if exhausted); held locally so the seam
            // machinery and per-segment render can borrow other fields freely.
            let cur = match self.current.take() {
                Some(c) => c,
                None => {
                    if self.lookahead.is_empty() {
                        self.pull_one();
                    }
                    match self.lookahead.pop_front() {
                        Some(slice) => (slice, 0),
                        None => break, // end-of-EDL
                    }
                }
            };

            let slice_start = cur.0.start_sample;
            let slice_len = cur.0.length_samples;
            let consumed = cur.1;
            let mut chunk = needed.min(slice_len - consumed);

            if seam_mode {
                // Detect any seam whose deposit deadline falls within this chunk: its
                // incoming slice (start `e`) sits at most ⌈max_fade/2⌉ past the deadline.
                self.scan_to(self.pos + chunk + half_max);
                self.deposit_due()?;
                // Stop the chunk on the next undeposited seam's deadline so emission lands
                // exactly there and the deposit window fits the ring.
                if let Some(d) = self.next_deadline() {
                    chunk = chunk.min(d - self.pos);
                }
            }

            // Sum each track's segment (with in-extent seam fades) directly into this chunk's
            // output window; zero it first since `out` is caller-provided.
            let region = &mut out[written * 2..(written + chunk as usize) * 2];
            region.fill(0.0);
            let slice = &cur.0;
            for seg in &slice.segments {
                // Advance the in-splice read offset by frames already emitted from this slice.
                let adjusted = EdlSegment {
                    track_id: seg.track_id,
                    splice: seg.splice.clone(),
                    offset_in_splice: seg.offset_in_splice + consumed,
                };
                let phase = *self.loop_phases.get(&seg.track_id).unwrap_or(&0);
                let (mut frames, new_phase) =
                    render_segment(&adjusted, chunk, &mut self.provider, phase)?;
                self.loop_phases.insert(seg.track_id, new_phase);
                if seam_mode {
                    apply_body_fades(
                        &mut frames,
                        seg.track_id,
                        slice_start,
                        slice_len,
                        self.pos,
                        chunk,
                        &self.seams,
                    );
                }
                for (m, &f) in region.iter_mut().zip(frames.iter()) {
                    *m += f;
                }
            }

            // Drain the seam-handle ring into the window, then clamp in place.
            self.acc.drain_add(self.pos, chunk as usize, region);
            for s in region.iter_mut() {
                *s = s.clamp(-1.0, 1.0);
            }

            written += chunk as usize;
            self.pos += chunk;

            // Park the unconsumed remainder, or drop the fully-emitted slice.
            let new_consumed = consumed + chunk;
            if new_consumed < slice_len {
                self.current = Some((cur.0, new_consumed));
            }

            if seam_mode {
                // Drop seams whose entire output window is behind the playhead.
                let pos = self.pos;
                let maxf = self.max_fade_samples as i64;
                self.seams.retain(|_, s| s.e + maxf > pos);
            }
        }

        Ok(written)
    }

    /// Pull one slice from the cursor into the look-ahead, running seam detection on it.
    fn pull_one(&mut self) {
        if let Some(slice) = self.cursor.next() {
            if self.max_fade_samples > 0 {
                self.detect_seams(&slice);
            }
            self.scanned_end = slice.start_sample + slice.length_samples;
            self.lookahead.push_back(slice);
        }
    }

    /// Pull slices until the look-ahead covers `target` (or the cursor is exhausted).
    fn scan_to(&mut self, target: i64) {
        while self.scanned_end < target {
            let before = self.scanned_end;
            self.pull_one();
            if self.scanned_end == before {
                break; // cursor exhausted
            }
        }
    }

    /// Detect crossfade seams introduced by `slice` against the per-track scan state, then
    /// update that state to this slice's segments. A seam is the structural transition where
    /// the previous segment consumed its splice tail and the current one starts a fresh
    /// splice; it is recorded when **neither** side is `Silence` (the continuation split —
    /// same splice, `offset != 0` — is therefore never a seam, and reads contiguously).
    fn detect_seams(&mut self, slice: &MixSlice) {
        for seg in &slice.segments {
            let t = seg.track_id;
            if let Some(prev) = self.scan_last.get(&t).copied() {
                let prev_tail_consumed =
                    prev.offset_in_splice + prev.covered_len == prev.splice_len;
                let neither_silence = !matches!(prev.kind, SpliceKind::Silence)
                    && !matches!(seg.splice.kind, SpliceKind::Silence);
                if prev_tail_consumed
                    && seg.offset_in_splice == 0
                    && prev.end_pos == slice.start_sample
                    && neither_silence
                {
                    let source_start = |k: SpliceKind| match k {
                        SpliceKind::Source {
                            source_start_sample,
                        } => source_start_sample,
                        _ => 0,
                    };
                    let e = slice.start_sample;
                    let fo = prev.fade_out;
                    let fi = seg.splice.fade_in_samples;
                    let out_ss = source_start(prev.kind);
                    let in_ss = source_start(seg.splice.kind);
                    let src_len = self.provider.source_len(t);
                    // Forward handle: ⌊fo/2⌋ frames, clamped at cache EOF (room tone always
                    // supplies the full ramp via the loop). No frames ⇒ one-sided pre-fade.
                    let fwd_n = match prev.kind {
                        SpliceKind::Source { .. } => {
                            (src_len - (out_ss + prev.splice_len)).clamp(0, fo / 2)
                        }
                        _ => fo / 2,
                    };
                    // Backward handle: ⌈fi/2⌉ frames, clamped at the source origin.
                    let bwd_n = match seg.splice.kind {
                        SpliceKind::Source { .. } => in_ss.clamp(0, (fi + 1) / 2),
                        _ => (fi + 1) / 2,
                    };
                    self.seams.insert(
                        (e, t),
                        Seam {
                            e,
                            track_id: t,
                            fo,
                            fi,
                            out_kind: prev.kind,
                            in_kind: seg.splice.kind,
                            out_source_start: out_ss,
                            out_splice_len: prev.splice_len,
                            in_source_start: in_ss,
                            fwd_n,
                            bwd_n,
                            out_onesided: fo > 0 && fwd_n == 0,
                            in_onesided: fi > 0 && bwd_n == 0,
                            deposited: false,
                        },
                    );
                }
            }
            self.scan_last.insert(
                t,
                ScanLast {
                    kind: seg.splice.kind,
                    splice_len: seg.splice.length_samples,
                    fade_out: seg.splice.fade_out_samples,
                    offset_in_splice: seg.offset_in_splice,
                    covered_len: slice.length_samples,
                    end_pos: slice.start_sample + slice.length_samples,
                },
            );
        }
    }

    /// Deposit the handle halves of every undeposited seam whose deadline is exactly `pos`.
    fn deposit_due(&mut self) -> Result<(), AudioError> {
        let pos = self.pos;
        let due: Vec<Seam> = self
            .seams
            .values()
            .filter(|s| !s.deposited && s.deadline() == pos)
            .copied()
            .collect();
        for seam in &due {
            if let Some(s) = self.seams.get_mut(&(seam.e, seam.track_id)) {
                s.deposited = true;
            }
            let loop_phase = *self.loop_phases.get(&seam.track_id).unwrap_or(&0) as i64;
            deposit_handles(seam, &mut self.provider, &mut self.acc, loop_phase)?;
        }
        Ok(())
    }

    /// Smallest deadline strictly after the current position among undeposited seams.
    fn next_deadline(&self) -> Option<i64> {
        self.seams
            .values()
            .filter(|s| !s.deposited && s.deadline() > self.pos)
            .map(Seam::deadline)
            .min()
    }
}

/// Drives the renderer as a streaming pull source at the project rate: stereo, `project_rate`
/// Hz, filling the caller's buffer. The playback pre-roll thread (and the export pipeline) wrap this
/// in a [`StreamingResampler`](super::resample::StreamingResampler) for device-rate output. The
/// `[start, end)` play window is owned by the [`EdlCursor`]; no frame-count cap lives here.
impl<P: SourceProvider> PcmSource for Renderer<P> {
    fn channels(&self) -> u16 {
        2
    }

    fn sample_rate(&self) -> u32 {
        self.project_rate
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        if self.exhausted {
            return Ok(0);
        }
        let requested = out.len() / 2;
        let got = self.read_frames(out)?;
        // Greedy-fill contract (mirrors `PcmSource::read`): a short fill means the cursor is
        // spent. A full fill landing exactly on end-of-EDL is reported exhausted on the next,
        // zero-frame read — which still satisfies the resampler's pump (`is_exhausted` is only
        // *required* true once a read under-fills).
        if got < requested {
            self.exhausted = true;
        }
        Ok(got)
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }
}

// ---------------------------------------------------------------------------
// MonoSource — stereo → mono collapse (export `mono` option)
// ---------------------------------------------------------------------------

/// Wraps a stereo [`PcmSource`] and collapses each frame to mono (`(L + R) / 2`).
///
/// Used by the export path when `mono = true`: it reports **one** channel and, on each `read`,
/// pulls twice as many interleaved samples from `inner` and averages each L/R pair. The inner
/// source must be stereo (the [`Renderer`] always is).
pub struct MonoSource<P: PcmSource> {
    inner: P,
    /// Scratch for the interleaved stereo pull; grown to the requested size, never shrunk.
    stereo_buf: Vec<f32>,
}

impl<P: PcmSource> MonoSource<P> {
    /// Wrap a stereo `inner` source as a mono one.
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            stereo_buf: Vec::new(),
        }
    }
}

impl<P: PcmSource> PcmSource for MonoSource<P> {
    fn channels(&self) -> u16 {
        1
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        // `out` holds one sample per mono frame; pull that many *stereo* frames from `inner`.
        let frames = out.len();
        let need = frames * 2;
        if self.stereo_buf.len() < need {
            self.stereo_buf.resize(need, 0.0);
        }
        let got = self.inner.read(&mut self.stereo_buf[..need])?;
        for (o, lr) in out[..got]
            .iter_mut()
            .zip(self.stereo_buf[..got * 2].chunks_exact(2))
        {
            *o = (lr[0] + lr[1]) / 2.0;
        }
        Ok(got)
    }

    fn is_exhausted(&self) -> bool {
        self.inner.is_exhausted()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioError;
    use crate::project::turn::{Splice, SpliceKind};
    use std::collections::BTreeMap;

    // --- In-memory test provider ---

    struct MockProvider {
        dry: BTreeMap<u32, Vec<f32>>,
        enhanced: BTreeMap<u32, Vec<f32>>,
        room_tone: BTreeMap<u32, Vec<f32>>,
        channels: BTreeMap<u32, u16>,
        wet_ratio: BTreeMap<u32, f32>,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                dry: BTreeMap::new(),
                enhanced: BTreeMap::new(),
                room_tone: BTreeMap::new(),
                channels: BTreeMap::new(),
                wet_ratio: BTreeMap::new(),
            }
        }

        fn track(mut self, id: u32, ch: u16, dry: Vec<f32>, wet: f32) -> Self {
            self.channels.insert(id, ch);
            self.dry.insert(id, dry);
            self.wet_ratio.insert(id, wet);
            self
        }

        fn with_enhanced(mut self, id: u32, enh: Vec<f32>) -> Self {
            self.enhanced.insert(id, enh);
            self
        }

        fn with_room_tone(mut self, id: u32, tone: Vec<f32>) -> Self {
            self.room_tone.insert(id, tone);
            self
        }
    }

    impl SourceProvider for MockProvider {
        fn dry(&mut self, track_id: u32, from: i64, n: i64) -> Result<Vec<f32>, AudioError> {
            let ch = *self.channels.get(&track_id).unwrap_or(&1) as usize;
            let buf = self.dry.get(&track_id).map_or(&[][..], |v| v.as_slice());
            let start = from as usize * ch;
            let end = start + n as usize * ch;
            Ok(buf[start..end].to_vec())
        }

        fn enhanced(
            &mut self,
            track_id: u32,
            from: i64,
            n: i64,
        ) -> Result<Option<Vec<f32>>, AudioError> {
            match self.enhanced.get(&track_id) {
                Some(buf) => {
                    let ch = *self.channels.get(&track_id).unwrap_or(&1) as usize;
                    let start = from as usize * ch;
                    let end = start + n as usize * ch;
                    Ok(Some(buf[start..end].to_vec()))
                }
                None => Ok(None),
            }
        }

        fn room_tone(&mut self, track_id: u32) -> Result<Option<&[f32]>, AudioError> {
            Ok(self.room_tone.get(&track_id).map(|v| v.as_slice()))
        }

        fn channels(&self, track_id: u32) -> u16 {
            *self.channels.get(&track_id).unwrap_or(&1)
        }

        fn wet_ratio(&self, track_id: u32) -> f32 {
            *self.wet_ratio.get(&track_id).unwrap_or(&0.0)
        }

        fn source_len(&self, track_id: u32) -> i64 {
            let ch = *self.channels.get(&track_id).unwrap_or(&1) as i64;
            self.dry.get(&track_id).map_or(0, |v| v.len() as i64 / ch)
        }
    }

    // --- Segment constructors ---

    fn src_seg(track_id: u32, source_start: i64, offset: i64, len: i64) -> EdlSegment {
        EdlSegment {
            track_id,
            splice: Splice {
                length_samples: len,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: source_start,
                },
            },
            offset_in_splice: offset,
        }
    }

    fn sil_seg(track_id: u32, len: i64) -> EdlSegment {
        EdlSegment {
            track_id,
            splice: Splice {
                length_samples: len,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Silence,
            },
            offset_in_splice: 0,
        }
    }

    fn rt_seg(track_id: u32, len: i64) -> EdlSegment {
        EdlSegment {
            track_id,
            splice: Splice {
                length_samples: len,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::RoomTone,
            },
            offset_in_splice: 0,
        }
    }

    // R1: Source segment, exact — output equals dry up-mixed to stereo.
    #[test]
    fn r1_source_segment_exact() {
        let dry: Vec<f32> = (0..10).map(|i| i as f32 * 0.1).collect();
        let mut p = MockProvider::new().track(1, 1, dry.clone(), 0.0);
        let (out, phase) = render_segment(&src_seg(1, 0, 0, 5), 5, &mut p, 0).unwrap();
        assert_eq!(out.len(), 10);
        assert_eq!(phase, 0);
        for i in 0..5usize {
            assert!(
                (out[i * 2] - dry[i]).abs() < 1e-7,
                "L at frame {i}: expected {}, got {}",
                dry[i],
                out[i * 2]
            );
            assert_eq!(out[i * 2], out[i * 2 + 1], "L == R at frame {i}");
        }
    }

    // R2: Silence segment — all-zero output of the right length.
    #[test]
    fn r2_silence_segment() {
        let mut p = MockProvider::new().track(1, 1, vec![], 0.0);
        let (out, _) = render_segment(&sil_seg(1, 8), 8, &mut p, 0).unwrap();
        assert_eq!(out.len(), 16);
        assert!(out.iter().all(|&s| s == 0.0), "silence must be all zeros");
    }

    // R4: Cache offset respected — first output sample == dry[source_start_sample].
    #[test]
    fn r4_cache_offset_respected() {
        let dry: Vec<f32> = (0..20).map(|i| i as f32 * 0.05).collect();
        let k = 7i64;
        let mut p = MockProvider::new().track(1, 1, dry.clone(), 0.0);
        let (out, _) = render_segment(&src_seg(1, k, 0, 5), 5, &mut p, 0).unwrap();
        assert!(
            (out[0] - dry[k as usize]).abs() < 1e-7,
            "first output sample must be dry[{k}]"
        );
        assert!(
            (out[2] - dry[k as usize + 1]).abs() < 1e-7,
            "second output sample L"
        );
    }

    // R5: Stereo passthrough — L and R are distinct; no collapse or up-mix.
    #[test]
    fn r5_stereo_passthrough() {
        let dry = vec![0.1f32, 0.9, 0.2, 0.8, 0.3, 0.7];
        let mut p = MockProvider::new().track(1, 2, dry, 0.0);
        let (out, _) = render_segment(&src_seg(1, 0, 0, 3), 3, &mut p, 0).unwrap();
        assert_eq!(out.len(), 6);
        assert!((out[0] - 0.1).abs() < 1e-7, "frame 0 L");
        assert!((out[1] - 0.9).abs() < 1e-7, "frame 0 R");
        assert!((out[2] - 0.2).abs() < 1e-7, "frame 1 L");
        assert!((out[3] - 0.8).abs() < 1e-7, "frame 1 R");
        assert!((out[4] - 0.3).abs() < 1e-7, "frame 2 L");
        assert!((out[5] - 0.7).abs() < 1e-7, "frame 2 R");
    }

    // F6: Room tone loops — output[i] == tone[i % tone_frames] (mono, away from fades).
    #[test]
    fn f6_room_tone_loops_the_stored_segment() {
        let tone = vec![0.1f32, 0.2, 0.3]; // 3-frame mono buffer
        let mut p = MockProvider::new()
            .track(1, 1, vec![], 0.0)
            .with_room_tone(1, tone.clone());
        let (out, new_phase) = render_segment(&rt_seg(1, 7), 7, &mut p, 0).unwrap();
        // Expected looping pattern: 0.1 0.2 0.3 0.1 0.2 0.3 0.1
        let expected = [0.1f32, 0.2, 0.3, 0.1, 0.2, 0.3, 0.1];
        for (i, &exp) in expected.iter().enumerate() {
            assert!(
                (out[i * 2] - exp).abs() < 1e-7,
                "frame {i}: expected {exp}, got {}",
                out[i * 2]
            );
            assert_eq!(out[i * 2], out[i * 2 + 1], "L == R at frame {i}");
        }
        // new_phase = (0 + 7) % 3 = 1
        assert_eq!(new_phase, 1);
    }

    // F7: Room tone with no blob → zeros, no panic.
    #[test]
    fn f7_room_tone_no_blob_is_silence() {
        let mut p = MockProvider::new().track(1, 1, vec![], 0.0);
        let (out, _) = render_segment(&rt_seg(1, 5), 5, &mut p, 0).unwrap();
        assert!(
            out.iter().all(|&s| s == 0.0),
            "missing room tone must yield zeros"
        );
    }

    // W15: wet_ratio = 0 with enhanced present → output == dry exactly.
    #[test]
    fn w15_ratio_0_equals_dry() {
        let dry = vec![0.5f32, 0.6, 0.7];
        let enh = vec![0.9f32, 0.8, 0.7];
        let mut p = MockProvider::new()
            .track(1, 1, dry.clone(), 0.0)
            .with_enhanced(1, enh);
        let (out, _) = render_segment(&src_seg(1, 0, 0, 3), 3, &mut p, 0).unwrap();
        for (i, &d) in dry.iter().enumerate() {
            assert!(
                (out[i * 2] - d).abs() < 1e-7,
                "frame {i}: expected {d}, got {}",
                out[i * 2]
            );
        }
    }

    // W16: wet_ratio = 1 with enhanced present → output == enhanced exactly.
    #[test]
    fn w16_ratio_1_equals_enhanced() {
        let dry = vec![0.1f32, 0.2, 0.3];
        let enh = vec![0.9f32, 0.8, 0.7];
        let mut p = MockProvider::new()
            .track(1, 1, dry, 1.0)
            .with_enhanced(1, enh.clone());
        let (out, _) = render_segment(&src_seg(1, 0, 0, 3), 3, &mut p, 0).unwrap();
        for (i, &e) in enh.iter().enumerate() {
            assert!(
                (out[i * 2] - e).abs() < 1e-7,
                "frame {i}: expected {e}, got {}",
                out[i * 2]
            );
        }
    }

    // W17: wet_ratio = 0.5 → output == 0.5·enhanced + 0.5·dry per sample.
    #[test]
    fn w17_ratio_half_linear() {
        let dry = vec![0.0f32, 0.4, 0.8];
        let enh = vec![1.0f32, 0.6, 0.2];
        let mut p = MockProvider::new()
            .track(1, 1, dry.clone(), 0.5)
            .with_enhanced(1, enh.clone());
        let (out, _) = render_segment(&src_seg(1, 0, 0, 3), 3, &mut p, 0).unwrap();
        for i in 0..3 {
            let expected = 0.5 * enh[i] + 0.5 * dry[i];
            assert!(
                (out[i * 2] - expected).abs() < 1e-6,
                "frame {i}: expected {expected}, got {}",
                out[i * 2]
            );
        }
    }

    // W18: Enhanced absent → output == dry even when wet_ratio > 0.
    #[test]
    fn w18_enhanced_absent_falls_back_to_dry() {
        let dry = vec![0.3f32, 0.4, 0.5];
        let mut p = MockProvider::new().track(1, 1, dry.clone(), 0.7);
        // No enhanced track registered.
        let (out, _) = render_segment(&src_seg(1, 0, 0, 3), 3, &mut p, 0).unwrap();
        for (i, &d) in dry.iter().enumerate() {
            assert!(
                (out[i * 2] - d).abs() < 1e-7,
                "frame {i}: should be dry, got {}",
                out[i * 2]
            );
        }
    }

    // W19: Handle reads (non-zero source offset) are wet/dry-blended like in-extent reads.
    #[test]
    fn w19_handle_reads_are_blended() {
        // Simulate a handle read: source_start_sample = 5, wet_ratio = 1 → output == enhanced[5..]
        let dry: Vec<f32> = (0..20).map(|i| i as f32 * 0.01).collect();
        let enh: Vec<f32> = (0..20).map(|i| i as f32 * 0.05 + 0.5).collect();
        let mut p = MockProvider::new()
            .track(1, 1, dry, 1.0)
            .with_enhanced(1, enh.clone());
        let (out, _) = render_segment(&src_seg(1, 5, 0, 4), 4, &mut p, 0).unwrap();
        for i in 0..4 {
            let exp = enh[5 + i];
            assert!(
                (out[i * 2] - exp).abs() < 1e-6,
                "frame {i}: expected {exp}, got {}",
                out[i * 2]
            );
        }
    }

    // X23: Mono up-mix equal gain — L == R == dry sample (not ±3 dB, not halved).
    #[test]
    fn x23_mono_upmix_equal_gain() {
        let dry = vec![0.5f32, 0.7, 0.9];
        let mut p = MockProvider::new().track(1, 1, dry.clone(), 0.0);
        let (out, _) = render_segment(&src_seg(1, 0, 0, 3), 3, &mut p, 0).unwrap();
        for (i, &d) in dry.iter().enumerate() {
            assert!((out[i * 2] - d).abs() < 1e-7, "frame {i} L: expected {d}");
            assert!(
                (out[i * 2 + 1] - d).abs() < 1e-7,
                "frame {i} R: expected {d}"
            );
        }
    }

    // Extra: loop phase is threaded correctly across a segment boundary.
    #[test]
    fn loop_phase_continues_across_render() {
        let tone = vec![0.1f32, 0.2, 0.3, 0.4, 0.5]; // 5-frame mono
        let mut p = MockProvider::new()
            .track(1, 1, vec![], 0.0)
            .with_room_tone(1, tone.clone());
        // First call: 3 frames, starting at phase 0.
        let (out1, phase1) = render_segment(&rt_seg(1, 3), 3, &mut p, 0).unwrap();
        assert_eq!(phase1, 3);
        // Second call: 4 frames, continuing at phase 3.
        let (out2, phase2) = render_segment(&rt_seg(1, 4), 4, &mut p, phase1).unwrap();
        assert_eq!(phase2, 2); // (3 + 4) % 5 = 2
                               // Combined output should be continuous.
        let combined: Vec<f32> = out1.iter().chain(out2.iter()).copied().collect();
        for i in 0..7 {
            let exp = tone[i % 5];
            assert!(
                (combined[i * 2] - exp).abs() < 1e-7,
                "combined frame {i}: expected {exp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Renderer pull-loop tests
    // -----------------------------------------------------------------------

    use std::sync::Arc;

    use crate::audio::edl::{EdlCursor, TrackCursor};
    use crate::project::hash::Hash;
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{encode_turn, Turn};

    /// Build a single-turn tree with the given splices on a single track.
    fn make_turn_h(id: u64, splices: Vec<Splice>) -> (Hash, Arc<Turn>) {
        let total: i64 = splices.iter().map(|s| s.length_samples).sum();
        let turn = Turn {
            id,
            speaker_id: None,
            turn_duration: total,
            post_turn_silence: 0,
            words: vec![],
            splices,
        };
        let (h, _) = encode_turn(&turn).unwrap();
        (h, Arc::new(turn))
    }

    fn build_tree(turns: Vec<(Hash, Arc<Turn>)>) -> ImplicitTimelineTree<Turn> {
        let mut tree = ImplicitTimelineTree::new();
        for (h, t) in turns {
            tree = tree.insert_at(tree.total_duration(), h, t).unwrap();
        }
        tree
    }

    fn src_splice(len: i64, ss: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: 0,
            fade_out_samples: 0,
            kind: SpliceKind::Source {
                source_start_sample: ss,
            },
        }
    }

    /// Build a single-track Renderer backed by a MockProvider.
    fn single_track_renderer(
        tree: &ImplicitTimelineTree<Turn>,
        track_id: u32,
        provider: MockProvider,
    ) -> Renderer<MockProvider> {
        let cursor = TrackCursor::at(tree, track_id, 0, 0);
        let edl = EdlCursor::new(vec![cursor], 0, None);
        Renderer::new(edl, provider, 0, 48_000)
    }

    // R3: render(n) over a longer EDL returns exactly n frames; subsequent calls
    // return the remainder, then empty.
    #[test]
    fn r3_segment_length_honoured() {
        let dry: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        let tree = build_tree(vec![make_turn_h(1, vec![src_splice(100, 0)])]);
        let p = MockProvider::new().track(1, 1, dry, 0.0);
        let mut r = single_track_renderer(&tree, 1, p);

        let out1 = r.render(60).unwrap();
        assert_eq!(out1.len() / 2, 60, "first render: 60 frames");

        let out2 = r.render(60).unwrap();
        assert_eq!(out2.len() / 2, 40, "second render: remaining 40 frames");

        let out3 = r.render(60).unwrap();
        assert!(out3.is_empty(), "past end-of-EDL: empty");
    }

    // X20: Two overlapping tracks sum — +0.3 + +0.4 = +0.7 throughout.
    #[test]
    fn x20_two_overlapping_tracks_sum() {
        let tree1 = build_tree(vec![make_turn_h(1, vec![src_splice(50, 0)])]);
        let tree2 = build_tree(vec![make_turn_h(2, vec![src_splice(50, 0)])]);
        let dry1: Vec<f32> = vec![0.3f32; 50];
        let dry2: Vec<f32> = vec![0.4f32; 50];
        let p = MockProvider::new()
            .track(1, 1, dry1, 0.0)
            .track(2, 1, dry2, 0.0);
        let c1 = TrackCursor::at(&tree1, 1, 0, 0);
        let c2 = TrackCursor::at(&tree2, 2, 0, 0);
        let edl = EdlCursor::new(vec![c1, c2], 0, None);
        let mut r = Renderer::new(edl, p, 0, 48_000);

        let out = r.render(50).unwrap();
        assert_eq!(out.len() / 2, 50);
        for i in 0..50 {
            assert!(
                (out[i * 2] - 0.7).abs() < 1e-5,
                "frame {i}: expected 0.7, got {}",
                out[i * 2]
            );
        }
    }

    // X21: Clamp after sum — +1.4 → +1.0; −1.4 → −1.0; clamp is post-sum.
    #[test]
    fn x21_clamp_after_sum() {
        let tree1 = build_tree(vec![make_turn_h(1, vec![src_splice(4, 0)])]);
        let tree2 = build_tree(vec![make_turn_h(2, vec![src_splice(4, 0)])]);
        // Interleaved: +0.8 for positive, −0.8 for negative (2 frames each).
        let dry1 = vec![0.8f32, 0.8, -0.8, -0.8]; // mono: frame 0=+0.8, frame 1=+0.8, ...
        let dry2 = vec![0.8f32, 0.8, -0.8, -0.8];
        let p = MockProvider::new()
            .track(1, 1, dry1, 0.0)
            .track(2, 1, dry2, 0.0);
        let c1 = TrackCursor::at(&tree1, 1, 0, 0);
        let c2 = TrackCursor::at(&tree2, 2, 0, 0);
        let edl = EdlCursor::new(vec![c1, c2], 0, None);
        let mut r = Renderer::new(edl, p, 0, 48_000);

        let out = r.render(4).unwrap();
        assert_eq!(out.len() / 2, 4);
        // Frames 0–1: +0.8 + +0.8 = +1.6 → clamped to +1.0
        assert!((out[0] - 1.0).abs() < 1e-7, "frame 0 L clamped to +1.0");
        assert!((out[1] - 1.0).abs() < 1e-7, "frame 0 R clamped to +1.0");
        // Frames 2–3: −0.8 + −0.8 = −1.6 → clamped to −1.0
        assert!((out[4] - (-1.0)).abs() < 1e-7, "frame 2 L clamped to −1.0");
        assert!((out[5] - (-1.0)).abs() < 1e-7, "frame 2 R clamped to −1.0");
    }

    // X22: Non-overlapping tracks don't double — where only one track has audio,
    // output equals that track's signal.
    #[test]
    fn x22_non_overlapping_tracks_dont_double() {
        // Track 1: 50 frames of +0.5; Track 2: 50 frames of silence.
        let tree1 = build_tree(vec![make_turn_h(1, vec![src_splice(50, 0)])]);
        let tree2 = build_tree(vec![make_turn_h(
            2,
            vec![Splice {
                length_samples: 50,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Silence,
            }],
        )]);
        let dry1 = vec![0.5f32; 50];
        let p = MockProvider::new()
            .track(1, 1, dry1, 0.0)
            .track(2, 1, vec![], 0.0);
        let c1 = TrackCursor::at(&tree1, 1, 0, 0);
        let c2 = TrackCursor::at(&tree2, 2, 0, 0);
        let edl = EdlCursor::new(vec![c1, c2], 0, None);
        let mut r = Renderer::new(edl, p, 0, 48_000);

        let out = r.render(50).unwrap();
        for i in 0..50 {
            assert!(
                (out[i * 2] - 0.5).abs() < 1e-7,
                "frame {i}: expected 0.5 (no doubling)"
            );
        }
    }

    // C28: No SQLite connection — MockProvider exposes only PCM; render completes with no Db.
    #[test]
    fn c28_no_sqlite_connection() {
        let tree = build_tree(vec![make_turn_h(1, vec![src_splice(10, 0)])]);
        let p = MockProvider::new().track(1, 1, vec![0.1f32; 10], 0.0);
        let mut r = single_track_renderer(&tree, 1, p);
        let out = r.render(10).unwrap();
        assert_eq!(out.len() / 2, 10);
    }

    // C29: End-of-EDL — rendering past the last segment returns empty.
    #[test]
    fn c29_end_of_edl_returns_empty() {
        let tree = build_tree(vec![make_turn_h(1, vec![src_splice(20, 0)])]);
        let p = MockProvider::new().track(1, 1, vec![0.2f32; 20], 0.0);
        let mut r = single_track_renderer(&tree, 1, p);

        let _ = r.render(20).unwrap(); // consume all
        let out = r.render(10).unwrap();
        assert!(out.is_empty(), "past end-of-EDL must return empty");
    }

    // C30: Determinism — same cursor + provider + request → byte-identical output twice.
    #[test]
    fn c30_determinism() {
        let tree = build_tree(vec![make_turn_h(1, vec![src_splice(30, 0)])]);
        let dry: Vec<f32> = (0..30).map(|i| i as f32 * 0.03).collect();

        let make = || {
            let p = MockProvider::new().track(1, 1, dry.clone(), 0.0);
            single_track_renderer(&tree, 1, p)
        };

        let out_a = make().render(30).unwrap();
        let out_b = make().render(30).unwrap();
        assert_eq!(out_a, out_b, "render must be deterministic");
    }

    // -----------------------------------------------------------------------
    // FadeAccumulator ring tests
    // -----------------------------------------------------------------------

    // Deposit then drain recovers the original stereo frames exactly.
    #[test]
    fn acc_deposit_drain_roundtrip() {
        let mut acc = FadeAccumulator::new(8);
        let frames = vec![0.1f32, 0.9, 0.2, 0.8, 0.3, 0.7]; // 3 stereo frames at pos 0
        acc.deposit(0, &frames);
        let mut out = vec![0.0f32; 6];
        acc.drain_add(0, 3, &mut out);
        for (i, (&g, &e)) in out.iter().zip(frames.iter()).enumerate() {
            assert!((g - e).abs() < 1e-7, "sample {i}: expected {e}, got {g}");
        }
    }

    // Two overlapping deposits at the same position sum additively.
    #[test]
    fn acc_overlapping_deposits_sum() {
        let mut acc = FadeAccumulator::new(8);
        // Two deposits at positions 2 and 3.
        acc.deposit(2, &[0.1f32, 0.2, 0.3, 0.4]);
        acc.deposit(2, &[0.5f32, 0.6, 0.7, 0.8]);
        let mut out = vec![0.0f32; 8]; // 4 frames at positions 0..4
        acc.drain_add(0, 4, &mut out);
        // Positions 0 and 1: nothing deposited → zero.
        assert!(out[0].abs() < 1e-7, "pos 0 L should be zero");
        assert!(out[2].abs() < 1e-7, "pos 1 L should be zero");
        // Position 2: (0.1 + 0.5) L, (0.2 + 0.6) R.
        assert!((out[4] - 0.6).abs() < 1e-6, "pos 2 L: {}", out[4]);
        assert!((out[5] - 0.8).abs() < 1e-6, "pos 2 R: {}", out[5]);
        // Position 3: (0.3 + 0.7) L, (0.4 + 0.8) R.
        assert!((out[6] - 1.0).abs() < 1e-6, "pos 3 L: {}", out[6]);
        assert!((out[7] - 1.2).abs() < 1e-6, "pos 3 R: {}", out[7]);
    }

    // Deposits that span the ring wrap boundary are placed and drained correctly.
    #[test]
    fn acc_ring_wraps() {
        let mut acc = FadeAccumulator::new(4); // 4-frame ring
                                               // Advance base_pos to 3 by draining positions 0..3.
        acc.drain_add(0, 3, &mut [0.0f32; 6]);
        assert_eq!(acc.base_pos, 3);
        // Deposit 4 stereo frames at positions 3..7 (wraps from slot 3 back through 0..2).
        let frames = vec![0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        acc.deposit(3, &frames);
        let mut out = vec![0.0f32; 8];
        acc.drain_add(3, 4, &mut out);
        for (i, (&g, &e)) in out.iter().zip(frames.iter()).enumerate() {
            assert!(
                (g - e).abs() < 1e-7,
                "wrapped frame sample {i}: expected {e}, got {g}"
            );
        }
        assert_eq!(acc.base_pos, 7);
    }

    // After drain, base_pos advances and drained slots are zeroed (no double-drain).
    #[test]
    fn acc_drain_advances_and_clears() {
        let mut acc = FadeAccumulator::new(4);
        // Deposit 2 stereo frames at positions 0 and 1.
        acc.deposit(0, &[0.5f32, 0.5, 0.5, 0.5]);
        // Drain frame 0 only.
        let mut out = vec![0.0f32; 2];
        acc.drain_add(0, 1, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-7, "frame 0 L after first drain");
        assert_eq!(acc.base_pos, 1);
        // Drain frame 1; must still get 0.5 (not lost after partial drain).
        let mut out2 = vec![0.0f32; 2];
        acc.drain_add(1, 1, &mut out2);
        assert!((out2[0] - 0.5).abs() < 1e-7, "frame 1 L after second drain");
        assert_eq!(acc.base_pos, 2);
        // Advance to position 4 (slot 0 wraps around); that slot must be zero.
        acc.drain_add(2, 2, &mut [0.0f32; 4]); // base_pos → 4
        let mut out3 = vec![0.0f32; 2];
        acc.drain_add(4, 1, &mut out3); // slot 0 (previously cleared)
        assert!(out3[0].abs() < 1e-7, "cleared + wrapped slot must be zero");
    }

    // drain_add with no prior deposits is a no-op (adds zero to out).
    #[test]
    fn acc_drain_no_deposit_is_noop() {
        let mut acc = FadeAccumulator::new(8);
        let mut out = vec![1.0f32, 2.0, 3.0, 4.0]; // pre-filled sentinel values
        acc.drain_add(0, 2, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-7, "L frame 0 unchanged");
        assert!((out[1] - 2.0).abs() < 1e-7, "R frame 0 unchanged");
        assert!((out[2] - 3.0).abs() < 1e-7, "L frame 1 unchanged");
        assert!((out[3] - 4.0).abs() < 1e-7, "R frame 1 unchanged");
    }

    // The flat-mix render matrix still passes with the accumulator wired in (drain is a no-op).
    // (All existing flat-mix tests r3, x20–x22, c28–c31 continue to run and pass unchanged.)

    // C31: All samples finite and in [−1, 1] after clamp.
    #[test]
    fn c31_all_samples_finite_in_range() {
        // Two loud tracks to force clamping.
        let tree1 = build_tree(vec![make_turn_h(1, vec![src_splice(40, 0)])]);
        let tree2 = build_tree(vec![make_turn_h(2, vec![src_splice(40, 0)])]);
        let dry1 = vec![0.9f32; 40];
        let dry2 = vec![0.9f32; 40];
        let p = MockProvider::new()
            .track(1, 1, dry1, 0.0)
            .track(2, 1, dry2, 0.0);
        let c1 = TrackCursor::at(&tree1, 1, 0, 0);
        let c2 = TrackCursor::at(&tree2, 2, 0, 0);
        let edl = EdlCursor::new(vec![c1, c2], 0, None);
        let mut r = Renderer::new(edl, p, 0, 48_000);

        let out = r.render(40).unwrap();
        for (i, &s) in out.iter().enumerate() {
            assert!(s.is_finite(), "sample {i} not finite: {s}");
            assert!((-1.0..=1.0).contains(&s), "sample {i} out of range: {s}");
        }
    }

    // -----------------------------------------------------------------------
    // Symmetric centered seam crossfades
    // -----------------------------------------------------------------------

    fn faded_src_splice(len: i64, ss: i64, fi: i64, fo: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: fi,
            fade_out_samples: fo,
            kind: SpliceKind::Source {
                source_start_sample: ss,
            },
        }
    }

    // Build a single-track renderer over two source splices forming one seam, with the
    // given per-side fades and source start offsets, over a ramp dry buffer dry[i] = i*scale.
    fn seam_renderer(
        tree: &ImplicitTimelineTree<Turn>,
        dry: Vec<f32>,
        max_fade: usize,
    ) -> Renderer<MockProvider> {
        let p = MockProvider::new().track(1, 1, dry, 0.0);
        let cursor = TrackCursor::at(tree, 1, 0, 0);
        let edl = EdlCursor::new(vec![cursor], 0, None);
        Renderer::new(edl, p, max_fade, 48_000)
    }

    // F8 + F9: a symmetric centered seam between two Source splices reading distinct source.
    // Asserts the renderer reproduces the centered equal-power crossfade sample-exactly:
    // kept-tail / own-head faded inline by output-distance-from-E, and the forward / backward
    // source handles (read *across* the seam, outside either splice's extent) ramped in the
    // ring. The handle samples (dry[110], dry[111], dry[198], dry[199]) lie in neither body's
    // read window, so their presence proves the handles are pulled across the seam.
    #[test]
    fn f8_f9_centered_seam_with_handles_sample_exact() {
        // Splice A: source [100,110); Splice B: source [200,210). Seam at E=10, FO=FI=4.
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(10, 100, 0, 4),
                faded_src_splice(10, 200, 4, 0),
            ],
        )]);
        let dry: Vec<f32> = (0..220).map(|i| i as f32 * 0.001).collect();
        let mut r = seam_renderer(&tree, dry.clone(), 4);
        let out = r.render(20).unwrap();
        assert_eq!(out.len() / 2, 20);

        // Build the expected mono signal from the same gain math.
        let mut exp = [0.0f32; 20];
        for (f, e) in exp.iter_mut().enumerate() {
            if f < 10 {
                // Outgoing body: source 100+f; kept-tail [8,10) fades out.
                let mut v = dry[100 + f];
                if f >= 8 {
                    v *= fade_out_gain(f - 8, 4);
                }
                *e += v;
            } else {
                // Incoming body: source 200+(f-10); own-head [10,12) fades in.
                let mut v = dry[200 + (f - 10)];
                if f < 12 {
                    v *= fade_in_gain((f - 10) + 2, 4);
                }
                *e += v;
            }
        }
        // Forward handle (outgoing): source [110,112) faded out at output [10,12).
        exp[10] += dry[110] * fade_out_gain(2, 4);
        exp[11] += dry[111] * fade_out_gain(3, 4);
        // Backward handle (incoming): source [198,200) faded in at output [8,10).
        exp[8] += dry[198] * fade_in_gain(0, 4);
        exp[9] += dry[199] * fade_in_gain(1, 4);

        for (f, &e) in exp.iter().enumerate() {
            assert!(
                (out[f * 2] - e).abs() < 1e-6,
                "frame {f}: expected {e}, got {}",
                out[f * 2]
            );
            assert_eq!(out[f * 2], out[f * 2 + 1], "L == R at frame {f}");
        }

        // F9 explicitly: the seam carries the handle energy (not a butt-joined fade over
        // disjoint samples) — output at the seam differs from the body-only render.
        assert!(
            (out[18] - dry[199] * fade_in_gain(1, 4) - dry[109] * fade_out_gain(1, 4)).abs() < 1e-6,
            "frame 9 must include the backward handle dry[199]"
        );
    }

    // F8 (constant power, no dip): a symmetric equal-power seam preserves power across E —
    // crossfading a signal with itself never dips below the signal level and shows no hard
    // edge (unlike a linear fade, which would dip ~3 dB in power at the midpoint). With a DC
    // source the crossover only rises smoothly (correlated material sums in amplitude).
    #[test]
    fn f8_centered_seam_constant_power_no_dip() {
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(20, 100, 0, 16),
                faded_src_splice(20, 200, 16, 0),
            ],
        )]);
        let dry = vec![0.5f32; 260];
        let mut r = seam_renderer(&tree, dry, 16);
        let out = r.render(40).unwrap();
        let mono: Vec<f32> = (0..40).map(|f| out[f * 2]).collect();
        // No dip anywhere: the equal-power crossover stays at or above the 0.5 signal level.
        for (f, &v) in mono.iter().enumerate() {
            assert!(v >= 0.5 - 1e-6, "dip below signal at frame {f}: {v}");
        }
        // No hard edge: every step is gradual across the whole window.
        for w in mono.windows(2) {
            assert!(
                (w[1] - w[0]).abs() < 0.051,
                "hard edge: {} -> {}",
                w[0],
                w[1]
            );
        }
        // The crossover genuinely peaks above the signal (a real equal-power overlap, not a
        // butt-join that would stay flat at 0.5).
        assert!(
            mono.iter().cloned().fold(0.0f32, f32::max) > 0.6,
            "expected an equal-power crossover bump"
        );
    }

    // F14: a seam whose splices carry zero fades is a hard butt-join — no overlay, no dip.
    #[test]
    fn f14_zero_fades_hard_join() {
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(10, 100, 0, 0),
                faded_src_splice(10, 200, 0, 0),
            ],
        )]);
        let dry: Vec<f32> = (0..220).map(|i| i as f32 * 0.001).collect();
        let mut r = seam_renderer(&tree, dry.clone(), 4);
        let out = r.render(20).unwrap();
        for f in 0..20 {
            let exp = if f < 10 {
                dry[100 + f]
            } else {
                dry[200 + (f - 10)]
            };
            assert!(
                (out[f * 2] - exp).abs() < 1e-7,
                "frame {f}: hard join expected {exp}, got {}",
                out[f * 2]
            );
        }
    }

    // X24: the seam overlay is summed into the mix and clamped globally (not per component).
    // A correlated (identical-source) equal-power crossfade peaks above the signal level; the
    // result is clamped to +1.0 after summing body + ring.
    #[test]
    fn x24_seam_summed_pre_clamp() {
        // Both splices read a continuous constant 0.8 source; in.ss = out.ss + out.len.
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(10, 100, 0, 4),
                faded_src_splice(10, 110, 4, 0),
            ],
        )]);
        let dry = vec![0.8f32; 130];
        let mut r = seam_renderer(&tree, dry, 4);
        let out = r.render(20).unwrap();

        // Unclamped the crossover frames would be 0.8*(0.866 + 0.5) ≈ 1.0928 > 1.
        let unclamped = 0.8 * (fade_out_gain(1, 4) + fade_in_gain(1, 4));
        assert!(
            unclamped > 1.0,
            "test setup: expected an over-unity crossover"
        );
        assert!((out[9 * 2] - 1.0).abs() < 1e-6, "frame 9 must clamp to 1.0");
        assert!(
            (out[10 * 2] - 1.0).abs() < 1e-6,
            "frame 10 must clamp to 1.0"
        );
        // Away from the crossover, the constant signal is untouched.
        assert!((out[0] - 0.8).abs() < 1e-6, "frame 0 == 0.8");
        for &s in &out {
            assert!((-1.0..=1.0).contains(&s), "every sample within range");
        }
    }

    // C25: a fade spanning ≥3 tiny foreign slices drains correctly — the shared accumulator,
    // not slice structure, owns the fade window. Track 2 fires rapid 2-sample silence cuts
    // across track 1's seam; the output must equal track 1 rendered alone.
    #[test]
    fn c25_fade_across_tiny_foreign_slices() {
        let t1 = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(10, 100, 0, 6),
                faded_src_splice(10, 200, 6, 0),
            ],
        )]);
        let dry: Vec<f32> = (0..220).map(|i| i as f32 * 0.001).collect();

        // Reference: track 1 alone.
        let mut r_alone = seam_renderer(&t1, dry.clone(), 6);
        let alone = r_alone.render(20).unwrap();

        // Track 2: ten 2-sample silence splices forcing tiny MixSlice boundaries.
        let t2 = build_tree(vec![make_turn_h(
            2,
            (0..10)
                .map(|_| Splice {
                    length_samples: 2,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                })
                .collect(),
        )]);
        let p = MockProvider::new()
            .track(1, 1, dry, 0.0)
            .track(2, 1, vec![], 0.0);
        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let edl = EdlCursor::new(vec![c1, c2], 0, None);
        let mut r = Renderer::new(edl, p, 6, 48_000);
        let mixed = r.render(20).unwrap();

        assert_eq!(mixed.len(), alone.len());
        for (i, (&m, &a)) in mixed.iter().zip(alone.iter()).enumerate() {
            assert!(
                (m - a).abs() < 1e-6,
                "sample {i}: tiny foreign slices changed the fade ({m} vs {a})"
            );
        }
    }

    // C27: two tracks each with a seam in the same window sum additively through the single
    // shared ring; the combined render equals the per-track renders summed (no clamp at these
    // amplitudes), proving the accumulator is project-wide, not per-track.
    #[test]
    fn c27_shared_accumulator_not_per_track() {
        let t1 = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(10, 100, 0, 4),
                faded_src_splice(10, 200, 4, 0),
            ],
        )]);
        let t2 = build_tree(vec![make_turn_h(
            2,
            vec![
                faded_src_splice(10, 300, 0, 4),
                faded_src_splice(10, 400, 4, 0),
            ],
        )]);
        let dry1: Vec<f32> = (0..520).map(|i| i as f32 * 1e-4).collect();
        let dry2: Vec<f32> = (0..520).map(|i| 0.05 - i as f32 * 1e-5).collect();

        // Per-track references.
        let p1 = MockProvider::new().track(1, 1, dry1.clone(), 0.0);
        let mut r1 = Renderer::new(
            EdlCursor::new(vec![TrackCursor::at(&t1, 1, 0, 0)], 0, None),
            p1,
            4,
            48_000,
        );
        let out1 = r1.render(20).unwrap();

        let p2 = MockProvider::new().track(2, 1, dry2.clone(), 0.0);
        let mut r2 = Renderer::new(
            EdlCursor::new(vec![TrackCursor::at(&t2, 2, 0, 0)], 0, None),
            p2,
            4,
            48_000,
        );
        let out2 = r2.render(20).unwrap();

        // Combined render through the single shared ring.
        let p = MockProvider::new()
            .track(1, 1, dry1, 0.0)
            .track(2, 1, dry2, 0.0);
        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let mut r = Renderer::new(EdlCursor::new(vec![c1, c2], 0, None), p, 4, 48_000);
        let mixed = r.render(20).unwrap();

        for i in 0..mixed.len() {
            let expect = out1[i] + out2[i];
            assert!(
                (mixed[i] - expect).abs() < 1e-6,
                "sample {i}: shared-ring sum {} != per-track sum {expect}",
                mixed[i]
            );
        }
    }

    // -----------------------------------------------------------------------
    // Generalization & special seams
    // -----------------------------------------------------------------------

    fn room_tone_splice(len: i64, fi: i64, fo: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: fi,
            fade_out_samples: fo,
            kind: SpliceKind::RoomTone,
        }
    }

    // F10: the room-tone gap fade IS the RoomTone splice's stamped fade, crossfaded by the
    // same seam machinery — speech→room-tone pulls the source forward handle while the
    // room-tone backward handle *continues the loop phase* (and vice-versa at room-tone→speech).
    // A short tone loop makes loop-phase continuation observable: the handles must read the
    // tone phases that continue R's loop, not a restart at phase 0.
    #[test]
    fn f10_room_tone_gap_fade_loop_continuation() {
        // Track 1: Source A [0,12) → RoomTone R [12,20) → Source B [20,32). Gap fade = 4.
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(12, 100, 0, 4),
                room_tone_splice(8, 4, 4),
                faded_src_splice(12, 300, 4, 0),
            ],
        )]);
        let dry: Vec<f32> = (0..320).map(|i| i as f32 * 0.001).collect();
        let tone = vec![0.5f32, 0.6, 0.7, 0.8, 0.9]; // L = 5, distinct from any dry sample
        let toneat = |p: i64| tone[p.rem_euclid(5) as usize];
        let p = MockProvider::new()
            .track(1, 1, dry.clone(), 0.0)
            .with_room_tone(1, tone.clone());
        let cursor = TrackCursor::at(&tree, 1, 0, 0);
        let mut r = Renderer::new(EdlCursor::new(vec![cursor], 0, None), p, 4, 48_000);
        let out = r.render(32).unwrap();

        // Expected, built directly from the centered-seam spec. R is the first room tone, so
        // its body rides loop phase (f - 12); the handles continue that same phase.
        let gi = |i: usize| fade_in_gain(i, 4);
        let go = |i: usize| fade_out_gain(i, 4);
        let mut exp = [0.0f32; 32];
        for (f, e) in exp.iter_mut().enumerate() {
            if f < 12 {
                // Source A body; kept-tail [10,12) fades out.
                let mut v = dry[100 + f];
                if f >= 10 {
                    v *= go(f - 10);
                }
                *e += v;
            } else if f < 20 {
                // RoomTone R body; own-head [12,14) fades in, kept-tail [18,20) fades out.
                let mut v = toneat(f as i64 - 12);
                if f < 14 {
                    v *= gi((f - 12) + 2);
                }
                if f >= 18 {
                    v *= go(f - 18);
                }
                *e += v;
            } else {
                // Source B body; own-head [20,22) fades in.
                let mut v = dry[300 + (f - 20)];
                if f < 22 {
                    v *= gi((f - 20) + 2);
                }
                *e += v;
            }
        }
        // A→R seam at E=12: A forward handle (source past 112) + R backward handle (loop
        // continued backward from phase 0 → phases −2, −1).
        exp[12] += dry[112] * go(2);
        exp[13] += dry[113] * go(3);
        exp[10] += toneat(-2) * gi(0);
        exp[11] += toneat(-1) * gi(1);
        // R→B seam at E=20: R forward handle (loop continued forward from phase 8≡3 → 3,4) +
        // B backward handle (source before 300).
        exp[20] += toneat(3) * go(2);
        exp[21] += toneat(4) * go(3);
        exp[18] += dry[298] * gi(0);
        exp[19] += dry[299] * gi(1);

        for (f, &e) in exp.iter().enumerate() {
            assert!(
                (out[f * 2] - e).abs() < 1e-5,
                "frame {f}: expected {e}, got {}",
                out[f * 2]
            );
            assert_eq!(out[f * 2], out[f * 2 + 1], "L == R at frame {f}");
        }
        // Loop continuation made explicit: the room-tone backward handle at frame 11 carries
        // tone phase −1 (== tone[4] = 0.9), not a restart at phase 0 (tone[0] = 0.5).
        assert!(
            (out[11 * 2] - (dry[111] * go(1) + 0.9 * gi(1))).abs() < 1e-5,
            "room-tone backward handle must continue the loop (phase −1 → 0.9)"
        );
    }

    // F11: asymmetric fades (FO ≠ FI) — two independent centered ramps, each placed at E with
    // its own length. No unity-sum is expected; the renderer must still be sample-exact and
    // not panic.
    #[test]
    fn f11_asymmetric_fades() {
        // A fade_out = 4, B fade_in = 2.
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(10, 100, 0, 4),
                faded_src_splice(10, 200, 2, 0),
            ],
        )]);
        let dry: Vec<f32> = (0..220).map(|i| i as f32 * 0.001).collect();
        let mut r = seam_renderer(&tree, dry.clone(), 4);
        let out = r.render(20).unwrap();

        let mut exp = [0.0f32; 20];
        for (f, e) in exp.iter_mut().enumerate() {
            if f < 10 {
                // Outgoing body; kept-tail [8,10) fades out over FO=4.
                let mut v = dry[100 + f];
                if f >= 8 {
                    v *= fade_out_gain(f - 8, 4);
                }
                *e += v;
            } else {
                // Incoming body; own-head [10,11) fades in over FI=2 (⌊2/2⌋ = 1 frame).
                let mut v = dry[200 + (f - 10)];
                if f < 11 {
                    v *= fade_in_gain((f - 10) + 1, 2);
                }
                *e += v;
            }
        }
        // A forward handle over ⌊FO/2⌋ = 2 at [10,12); B backward handle over ⌈FI/2⌉ = 1 at [9,10).
        exp[10] += dry[110] * fade_out_gain(2, 4);
        exp[11] += dry[111] * fade_out_gain(3, 4);
        exp[9] += dry[199] * fade_in_gain(0, 2);

        for (f, &e) in exp.iter().enumerate() {
            assert!(
                (out[f * 2] - e).abs() < 1e-6,
                "frame {f}: expected {e}, got {}",
                out[f * 2]
            );
        }
    }

    // F12: a pristine splice split by a foreign track's boundary is a continuation, not a
    // seam — it reads the source contiguously across the slice boundary with no overlay and
    // no fade (a lone splice's stamped fades stay inert until it borders a different splice).
    #[test]
    fn f12_continuation_split_no_crossfade() {
        // Track 1: one 20-sample source splice (stamped fades, but no neighbouring splice).
        let t1 = build_tree(vec![make_turn_h(1, vec![faded_src_splice(20, 100, 4, 4)])]);
        // Track 2: two silence splices forcing a MixSlice boundary at offset 8 in track 1.
        let t2 = build_tree(vec![make_turn_h(
            2,
            vec![
                Splice {
                    length_samples: 8,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                },
                Splice {
                    length_samples: 12,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                },
            ],
        )]);
        let dry: Vec<f32> = (0..220).map(|i| i as f32 * 0.001).collect();
        let p = MockProvider::new()
            .track(1, 1, dry.clone(), 0.0)
            .track(2, 1, vec![], 0.0);
        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let mut r = Renderer::new(EdlCursor::new(vec![c1, c2], 0, None), p, 4, 48_000);
        let out = r.render(20).unwrap();

        // Contiguous, unfaded source read across the split at frame 8.
        for f in 0..20 {
            assert!(
                (out[f * 2] - dry[100 + f]).abs() < 1e-7,
                "frame {f}: continuation must read contiguously, got {}",
                out[f * 2]
            );
        }
    }

    // C26: two of a track's seams within one fade length — their handle overlays land on the
    // same ring cells and sum additively; the doubly-faded body (own-head × kept-tail) is
    // graceful. The overlap region [8,10) carries A's forward handle + C's backward handle.
    #[test]
    fn c26_overlapping_own_track_seams() {
        // A [0,8) fo=4, B [8,10) fi=4/fo=4 (only 2 samples long), C [10,18) fi=4.
        let tree = build_tree(vec![make_turn_h(
            1,
            vec![
                faded_src_splice(8, 100, 0, 4),
                faded_src_splice(2, 200, 4, 4),
                faded_src_splice(8, 300, 4, 0),
            ],
        )]);
        let dry: Vec<f32> = (0..320).map(|i| i as f32 * 0.001).collect();
        let mut r = seam_renderer(&tree, dry.clone(), 4);
        let out = r.render(18).unwrap();

        let gi = |i: usize| fade_in_gain(i, 4);
        let go = |i: usize| fade_out_gain(i, 4);
        let mut exp = [0.0f32; 18];
        for (f, e) in exp.iter_mut().enumerate() {
            if f < 8 {
                // A body; kept-tail [6,8) fades out (A→B).
                let mut v = dry[100 + f];
                if f >= 6 {
                    v *= go(f - 6);
                }
                *e += v;
            } else if f < 10 {
                // B body, only 2 samples: own-head fade-in (A→B) × kept-tail fade-out (B→C).
                let mut v = dry[200 + (f - 8)];
                v *= gi((f - 8) + 2); // own-head over [8,10)
                v *= go(f - 8); // kept-tail over [8,10)
                *e += v;
            } else {
                // C body; own-head [10,12) fades in (B→C).
                let mut v = dry[300 + (f - 10)];
                if f < 12 {
                    v *= gi((f - 10) + 2);
                }
                *e += v;
            }
        }
        // A→B seam at E=8: A forward handle [8,10), B backward handle [6,8).
        exp[8] += dry[108] * go(2);
        exp[9] += dry[109] * go(3);
        exp[6] += dry[198] * gi(0);
        exp[7] += dry[199] * gi(1);
        // B→C seam at E=10: B forward handle [10,12), C backward handle [8,10) — overlaps the
        // A forward handle in the ring and sums additively.
        exp[10] += dry[202] * go(2);
        exp[11] += dry[203] * go(3);
        exp[8] += dry[298] * gi(0);
        exp[9] += dry[299] * gi(1);

        for (f, &e) in exp.iter().enumerate() {
            assert!(
                (out[f * 2] - e).abs() < 1e-6,
                "frame {f}: expected {e}, got {}",
                out[f * 2]
            );
            assert!(out[f * 2].is_finite() && (-1.0..=1.0).contains(&out[f * 2]));
        }
    }

    // -----------------------------------------------------------------------
    // Graceful degradation
    // -----------------------------------------------------------------------

    // F13: a handle that can't be supplied is shortened/zero-padded or, when wholly
    // unavailable, the side falls back to a one-sided in-extent fade — the renderer never
    // reads out of range and never panics. Three scenarios, each sample-exact.
    #[test]
    fn f13_graceful_degradation() {
        let gi = |i: usize| fade_in_gain(i, 4);
        let go = |i: usize| fade_out_gain(i, 4);

        // (a) Incoming starts at the source origin (ss = 0): no backward handle ⇒ one-sided
        // post fade-in over the full FI within its own extent [E, E+FI). The outgoing side
        // still has its forward handle (centered).
        {
            let tree = build_tree(vec![make_turn_h(
                1,
                vec![
                    faded_src_splice(10, 100, 0, 4),
                    faded_src_splice(10, 0, 4, 0),
                ],
            )]);
            let dry: Vec<f32> = (0..200).map(|i| i as f32 * 0.001).collect();
            let mut r = seam_renderer(&tree, dry.clone(), 4);
            let out = r.render(20).unwrap();

            let mut exp = [0.0f32; 20];
            for (f, e) in exp.iter_mut().enumerate() {
                if f < 10 {
                    let mut v = dry[100 + f];
                    if f >= 8 {
                        v *= go(f - 8); // centered kept-tail [8,10)
                    }
                    *e += v;
                } else {
                    let mut v = dry[f - 10]; // incoming source from 0
                    if f < 14 {
                        v *= gi(f - 10); // one-sided fade-in [10,14)
                    }
                    *e += v;
                }
            }
            exp[10] += dry[110] * go(2); // forward handle present
            exp[11] += dry[111] * go(3);
            assert_degraded(&out, &exp);
        }

        // (b) Outgoing ends at cache EOF: no forward handle ⇒ one-sided pre fade-out over
        // [E−FO, E). The incoming side keeps its (centered) backward handle.
        {
            let tree = build_tree(vec![make_turn_h(
                1,
                vec![
                    faded_src_splice(10, 200, 0, 4),
                    faded_src_splice(10, 50, 4, 0),
                ],
            )]);
            let dry: Vec<f32> = (0..210).map(|i| i as f32 * 0.001).collect();
            let mut r = seam_renderer(&tree, dry.clone(), 4);
            let out = r.render(20).unwrap();

            let mut exp = [0.0f32; 20];
            for (f, e) in exp.iter_mut().enumerate() {
                if f < 10 {
                    let mut v = dry[200 + f];
                    if f >= 6 {
                        v *= go(f - 6); // one-sided fade-out [6,10)
                    }
                    *e += v;
                } else {
                    let mut v = dry[50 + (f - 10)];
                    if f < 12 {
                        v *= gi((f - 10) + 2); // centered own-head [10,12)
                    }
                    *e += v;
                }
            }
            exp[8] += dry[48] * gi(0); // backward handle [8,10)
            exp[9] += dry[49] * gi(1);
            assert_degraded(&out, &exp);
        }

        // (c) Incoming starts at source sample 1 with FI=4: backward handle is shortened to the
        // single available frame (its near-E end), deposited one sample before E with the ramp
        // index skipped forward. Outgoing has zero fade.
        {
            let tree = build_tree(vec![make_turn_h(
                1,
                vec![
                    faded_src_splice(10, 100, 0, 0),
                    faded_src_splice(10, 1, 4, 0),
                ],
            )]);
            let dry: Vec<f32> = (0..200).map(|i| 0.1 + i as f32 * 0.001).collect();
            let mut r = seam_renderer(&tree, dry.clone(), 4);
            let out = r.render(20).unwrap();

            let mut exp = [0.0f32; 20];
            for (f, e) in exp.iter_mut().enumerate() {
                if f < 10 {
                    *e += dry[100 + f]; // outgoing flat (FO = 0)
                } else {
                    let mut v = dry[1 + (f - 10)];
                    if f < 12 {
                        v *= gi((f - 10) + 2); // centered own-head [10,12)
                    }
                    *e += v;
                }
            }
            // Only one backward-handle frame survives (source sample 0), at output 9, with the
            // ramp index skipped to ⌈FI/2⌉ − 1 = 1.
            exp[9] += dry[0] * gi(1);
            assert_degraded(&out, &exp);
        }
    }

    // Assert a degraded render matches `exp` sample-exactly (L == R) and stays finite/in-range.
    fn assert_degraded(out: &[f32], exp: &[f32; 20]) {
        for (f, &e) in exp.iter().enumerate() {
            assert!(
                (out[f * 2] - e).abs() < 1e-6,
                "frame {f}: expected {e}, got {}",
                out[f * 2]
            );
            assert_eq!(out[f * 2], out[f * 2 + 1], "L == R at frame {f}");
            assert!(out[f * 2].is_finite() && (-1.0..=1.0).contains(&out[f * 2]));
        }
    }

    // Cursor-bounded padding: an explicit end past the track content renders trailing silence.
    #[test]
    fn renderer_pads_silence_to_explicit_end() {
        let (h, t) = make_turn_h(1, vec![src_splice(100, 0)]);
        let tree = build_tree(vec![(h, t)]);
        let p = MockProvider::new().track(1, 1, vec![0.5f32; 100], 0.0);
        let edl = EdlCursor::new(vec![TrackCursor::at(&tree, 1, 0, 0)], 0, Some(200));
        let mut r = Renderer::new(edl, p, 0, 48_000);

        let out = r.render(200).unwrap();
        assert_eq!(out.len(), 400, "200 stereo frames rendered");
        for i in 0..100 {
            assert!((out[i * 2] - 0.5).abs() < 1e-6, "content L[{i}]");
        }
        for i in 100..200 {
            assert_eq!(out[i * 2], 0.0, "pad L[{i}] is silence");
            assert_eq!(out[i * 2 + 1], 0.0, "pad R[{i}] is silence");
        }
        assert!(r.render(10).unwrap().is_empty(), "cursor spent after end");
    }

    // -----------------------------------------------------------------------
    // Mutation-testing kills (M2 sweep): direct unit tests on the private render
    // helpers, plus seam/segment scenarios that pin exact output. Golden values
    // are hardcoded (not recomputed through the function under test) where the
    // mutation lives in the value-producing helper itself.
    // -----------------------------------------------------------------------

    // fade_out_gain / fade_in_gain golden values. A same-helper expected value
    // could not catch `len - 1 - i` (175) — the test would mutate with the code —
    // so the constants are independent of the helper.
    #[test]
    fn mut_fade_gain_golden() {
        // fade_out_gain(i,4) = cos(i/3·π/2); fade_in_gain(i,4) = sin(i/3·π/2).
        for (i, exp) in [(0usize, 1.0f32), (1, 0.866_025_4), (2, 0.5), (3, 0.0)] {
            assert!(
                (fade_out_gain(i, 4) - exp).abs() < 1e-6,
                "fade_out_gain({i},4)={} expected {exp}",
                fade_out_gain(i, 4)
            );
        }
        for (i, exp) in [(0usize, 0.0f32), (1, 0.5), (2, 0.866_025_4), (3, 1.0)] {
            assert!(
                (fade_in_gain(i, 4) - exp).abs() < 1e-6,
                "fade_in_gain({i},4)={} expected {exp}",
                fade_in_gain(i, 4)
            );
        }
    }

    // render_segment room tone with a STEREO tone: the channel count makes the
    // tone_frames divisor (94 `/ n_ch`) and the frame_start stride (100 `* n_ch`)
    // observable (mono collapses `*`/`/` by n_ch into no-ops).
    #[test]
    fn mut_render_segment_room_tone_stereo_golden() {
        let tone: Vec<f32> = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0]
            .iter()
            .map(|v| v * 0.1)
            .collect();
        let mut p = MockProvider::new()
            .track(1, 2, vec![], 0.0)
            .with_room_tone(1, tone.clone());
        let (out, phase) = render_segment(&rt_seg(1, 5), 5, &mut p, 0).unwrap();
        assert_eq!(phase, 2, "(0+5) % 3 tone frames");
        // 3 stereo frames (1,2)(3,4)(5,6); loop: f0,f1,f2,f0,f1.
        let exp = [
            tone[0], tone[1], tone[2], tone[3], tone[4], tone[5], tone[0], tone[1], tone[2],
            tone[3],
        ];
        for (k, &e) in exp.iter().enumerate() {
            assert!(
                (out[k] - e).abs() < 1e-7,
                "sample {k}: expected {e}, got {}",
                out[k]
            );
        }
    }

    // read_room_tone_handle stereo exact: pins the `frames` divisor (395) and the
    // `* n_ch` slot stride (398).
    #[test]
    fn mut_read_room_tone_handle_stereo_exact() {
        let tone: Vec<f32> = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0]
            .iter()
            .map(|v| v * 0.1)
            .collect();
        let mut p = MockProvider::new()
            .track(1, 2, vec![], 0.0)
            .with_room_tone(1, tone.clone());
        let out = read_room_tone_handle(&mut p, 1, 0, 5).unwrap();
        let exp = [
            tone[0], tone[1], tone[2], tone[3], tone[4], tone[5], tone[0], tone[1], tone[2],
            tone[3],
        ];
        for (k, &e) in exp.iter().enumerate() {
            assert!(
                (out[k] - e).abs() < 1e-7,
                "sample {k}: expected {e}, got {}",
                out[k]
            );
        }
    }

    // Empty tone ⇒ silence handle: the `tone.len()/n_ch > 0` guard (394) must reject,
    // else `rem_euclid(0)` panics under guard→true / `> → >=`.
    #[test]
    fn mut_read_room_tone_handle_empty_is_silence() {
        let mut p = MockProvider::new()
            .track(1, 1, vec![], 0.0)
            .with_room_tone(1, vec![]);
        let out = read_room_tone_handle(&mut p, 1, 0, 4).unwrap();
        assert_eq!(out, vec![0.0f32; 8]);
    }

    // Direct deposit_handles: a forward Source handle with odd fo pins ceil_fo=(fo+1)/2
    // (449 `+ → *`).
    #[test]
    fn mut_deposit_handles_forward_source_odd_fo() {
        let dry: Vec<f32> = (0..130).map(|i| i as f32 * 0.001).collect();
        let mut p = MockProvider::new().track(1, 1, dry.clone(), 0.0);
        let seam = Seam {
            e: 50,
            track_id: 1,
            fo: 5,
            fi: 0,
            out_kind: SpliceKind::Source {
                source_start_sample: 100,
            },
            in_kind: SpliceKind::Source {
                source_start_sample: 0,
            },
            out_source_start: 100,
            out_splice_len: 10,
            in_source_start: 0,
            fwd_n: 2,
            bwd_n: 0,
            out_onesided: false,
            in_onesided: false,
            deposited: false,
        };
        let mut acc = FadeAccumulator::new(8);
        deposit_handles(&seam, &mut p, &mut acc, 0).unwrap();
        let mut out = vec![0.0f32; 8]; // 4 frames [50,54)
        acc.drain_add(50, 4, &mut out);
        // ceil_fo = (5+1)/2 = 3; forward [50,52) reads dry[110],dry[111] faded out.
        let e0 = dry[110] * fade_out_gain(3, 5);
        let e1 = dry[111] * fade_out_gain(4, 5);
        assert!(
            (out[0] - e0).abs() < 1e-7 && (out[1] - e0).abs() < 1e-7,
            "f0 {} vs {e0}",
            out[0]
        );
        assert!(
            (out[2] - e1).abs() < 1e-7 && (out[3] - e1).abs() < 1e-7,
            "f1 {} vs {e1}",
            out[2]
        );
        assert_eq!(out[4], 0.0, "no frame past the handle");
    }

    // Direct deposit_handles: a forward RoomTone handle reads at phase_at_e = loop_phase +
    // ceil_fi, where ceil_fi = (fi+1)/2 (427 `(fi+1) → fi`). Odd fi makes the ceil decisive.
    #[test]
    fn mut_deposit_handles_roomtone_phase() {
        let tone: Vec<f32> = (0..6).map(|i| (i + 1) as f32 * 0.1).collect(); // 6 mono frames
        let mut p = MockProvider::new()
            .track(1, 1, vec![], 0.0)
            .with_room_tone(1, tone.clone());
        let seam = Seam {
            e: 50,
            track_id: 1,
            fo: 6,
            fi: 3,
            out_kind: SpliceKind::RoomTone,
            in_kind: SpliceKind::Silence,
            out_source_start: 0,
            out_splice_len: 0,
            in_source_start: 0,
            fwd_n: 2,
            bwd_n: 0,
            out_onesided: false,
            in_onesided: false,
            deposited: false,
        };
        let mut acc = FadeAccumulator::new(8);
        deposit_handles(&seam, &mut p, &mut acc, 3).unwrap();
        let mut out = vec![0.0f32; 8];
        acc.drain_add(50, 4, &mut out);
        // ceil_fi=(3+1)/2=2; phase_at_e=3+2=5; forward reads tone[5],tone[0]; ceil_fo=(6+1)/2=3.
        let e0 = tone[5] * fade_out_gain(3, 6);
        let e1 = tone[0] * fade_out_gain(4, 6);
        assert!((out[0] - e0).abs() < 1e-7, "f0 {} vs {e0}", out[0]);
        assert!((out[2] - e1).abs() < 1e-7, "f1 {} vs {e1}", out[2]);
    }

    // Direct apply_body_fades with odd fades: pins the centered region_start for the
    // outgoing kept-tail (345 `(fo+1) → fo`) and the incoming ramp_base (366 `(fi+1) → fi`).
    #[test]
    fn mut_apply_body_fades_odd_regions() {
        // Outgoing seam at e=10, fo=5 (odd); chunk [0,10) constant 1.0.
        let mut seams: BTreeMap<(i64, u32), Seam> = BTreeMap::new();
        seams.insert((10, 1), test_seam(10, 5, 0));
        let mut frames = vec![1.0f32; 20];
        apply_body_fades(&mut frames, 1, 0, 10, 0, 10, &seams);
        // region_start = e - (fo+1)/2 = 7; kept-tail [7,10).
        for f in 0..10i64 {
            let exp = if f >= 7 {
                fade_out_gain((f - 7) as usize, 5)
            } else {
                1.0
            };
            assert!(
                (frames[2 * f as usize] - exp).abs() < 1e-6,
                "out f{f}: {} vs {exp}",
                frames[2 * f as usize]
            );
        }

        // Incoming seam at e=0, fi=5 (odd); chunk [0,10) constant 1.0.
        let mut seams2: BTreeMap<(i64, u32), Seam> = BTreeMap::new();
        seams2.insert((0, 1), test_seam(0, 0, 5));
        let mut frames2 = vec![1.0f32; 20];
        apply_body_fades(&mut frames2, 1, 0, 10, 0, 10, &seams2);
        // region_end = slice_start + fi/2 = 2; ramp_base = (fi+1)/2 = 3; own-head [0,2).
        for f in 0..10i64 {
            let exp = if f < 2 {
                fade_in_gain(f as usize + 3, 5)
            } else {
                1.0
            };
            assert!(
                (frames2[2 * f as usize] - exp).abs() < 1e-6,
                "in f{f}: {} vs {exp}",
                frames2[2 * f as usize]
            );
        }
    }

    // Seam::deadline ceils fi: deadline = e - ⌈fi/2⌉ (304 `(fi+1) → fi`).
    #[test]
    fn mut_seam_deadline_ceils_fi() {
        let seam = test_seam(100, 0, 3);
        assert_eq!(seam.deadline(), 98, "100 - ⌈3/2⌉");
    }

    // A bare Seam for direct-helper tests (Source/Source, no clamping).
    fn test_seam(e: i64, fo: i64, fi: i64) -> Seam {
        Seam {
            e,
            track_id: 1,
            fo,
            fi,
            out_kind: SpliceKind::Source {
                source_start_sample: 0,
            },
            in_kind: SpliceKind::Source {
                source_start_sample: 0,
            },
            out_source_start: 0,
            out_splice_len: 0,
            in_source_start: 0,
            fwd_n: 0,
            bwd_n: 0,
            out_onesided: false,
            in_onesided: false,
            deposited: false,
        }
    }

    // render(n) honours a partial request that ends mid-slice: the `needed`
    // remaining-frames bound (579 `n_frames - written`) must cap the final chunk;
    // `+` over-emits past the output buffer.
    #[test]
    fn mut_render_partial_request_exact_count() {
        let dry: Vec<f32> = (0..40).map(|i| i as f32 * 0.001).collect();
        let (h, t) = make_turn_h(1, vec![src_splice(10, 0), src_splice(10, 20)]);
        let tree = build_tree(vec![(h, t)]);
        let p = MockProvider::new().track(1, 1, dry.clone(), 0.0);
        let mut r = single_track_renderer(&tree, 1, p);
        let out = r.render(15).unwrap();
        assert_eq!(out.len() / 2, 15, "exactly 15 frames");
        for f in 0..10 {
            assert!((out[f * 2] - dry[f]).abs() < 1e-7, "slice0 f{f}");
        }
        for f in 10..15 {
            assert!(
                (out[f * 2] - dry[20 + (f - 10)]).abs() < 1e-7,
                "slice1 f{f}"
            );
        }
    }

    // Source (fully consumed, fo>0) → Silence is NOT a seam: the neither-silence guard
    // (705) and the prev_tail_consumed conjunction (707) keep the source tail unfaded
    // and deposit no handle into the silence.
    #[test]
    fn mut_source_then_silence_is_no_seam() {
        let dry = vec![0.5f32; 40];
        let (h, t) = make_turn_h(
            1,
            vec![
                Splice {
                    length_samples: 10,
                    fade_in_samples: 0,
                    fade_out_samples: 4,
                    kind: SpliceKind::Source {
                        source_start_sample: 0,
                    },
                },
                Splice {
                    length_samples: 10,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                },
            ],
        );
        let tree = build_tree(vec![(h, t)]);
        let p = MockProvider::new().track(1, 1, dry, 0.0);
        let edl = EdlCursor::new(vec![TrackCursor::at(&tree, 1, 0, 0)], 0, None);
        let mut r = Renderer::new(edl, p, 4, 48_000); // seam machinery ON
        let out = r.render(20).unwrap();
        for f in 0..10 {
            assert!(
                (out[f * 2] - 0.5).abs() < 1e-7,
                "source f{f} unfaded: {}",
                out[f * 2]
            );
        }
        for f in 10..20 {
            assert_eq!(out[f * 2], 0.0, "silence f{f}: {}", out[f * 2]);
        }
    }

    // Source → RoomTone seam: the incoming room-tone backward-handle width is
    // bwd_n = (fi+1)/2 (the `_` branch at 734). The `/ → %` mutation makes bwd_n=0 for odd
    // fi, marking the seam in-onesided and collapsing the centered fade-in to a one-sided
    // post-fade that *silences* the seam frame. Source ends exactly at EOF so there is no
    // forward handle to muddy the seam frame.
    #[test]
    fn mut_roomtone_incoming_seam_centered() {
        let dry = vec![0.5f32; 10]; // src_len == 10 ⇒ outgoing side is one-sided, no fwd handle
        let (h, t) = make_turn_h(
            1,
            vec![
                Splice {
                    length_samples: 10,
                    fade_in_samples: 0,
                    fade_out_samples: 4,
                    kind: SpliceKind::Source {
                        source_start_sample: 0,
                    },
                },
                Splice {
                    length_samples: 10,
                    fade_in_samples: 3,
                    fade_out_samples: 0,
                    kind: SpliceKind::RoomTone,
                },
            ],
        );
        let tree = build_tree(vec![(h, t)]);
        let p = MockProvider::new()
            .track(1, 1, dry, 0.0)
            .with_room_tone(1, vec![0.3]);
        let edl = EdlCursor::new(vec![TrackCursor::at(&tree, 1, 0, 0)], 0, None);
        let mut r = Renderer::new(edl, p, 4, 48_000);
        let out = r.render(20).unwrap();
        // Centered own-head: ramp_base=(3+1)/2=2 ⇒ fade_in_gain(2,3)=1.0 ⇒ frame 10 is full tone.
        // One-sided (mutant) would multiply by fade_in_gain(0,3)=0.
        assert!(
            (out[20] - 0.3).abs() < 1e-6,
            "seam frame 10 = {} (expected full tone)",
            out[20]
        );
    }

    // The rendered output of a seam must be independent of read chunk size. Small chunks
    // stress the look-ahead depth (`half_max`, 576), the scan-to target (604), the seam GC
    // retain bound (664), and the scanned-end bookkeeping (677) — all masked by one big read.
    #[test]
    fn mut_seam_chunk_size_invariant() {
        fn build() -> Renderer<MockProvider> {
            let tree = build_tree(vec![make_turn_h(
                1,
                vec![
                    faded_src_splice(10, 100, 0, 3),
                    faded_src_splice(10, 200, 3, 0),
                ],
            )]);
            let dry: Vec<f32> = (0..220).map(|i| i as f32 * 0.001).collect();
            seam_renderer(&tree, dry, 3)
        }
        let whole = build().render(20).unwrap();
        let mut r = build();
        let mut piece = Vec::new();
        loop {
            let chunk = r.render(1).unwrap();
            if chunk.is_empty() {
                break;
            }
            piece.extend_from_slice(&chunk);
        }
        assert_eq!(
            whole.len(),
            piece.len(),
            "frame count differs by chunk size"
        );
        for (i, (&w, &p)) in whole.iter().zip(piece.iter()).enumerate() {
            assert!((w - p).abs() < 1e-9, "sample {i}: whole {w} vs 1-frame {p}");
        }
    }

    // MonoSource collapses (L+R)/2 and delegates rate/exhaustion to the inner source:
    // pins sample_rate (865), the `frames * 2` stereo pull (871), and is_exhausted (886).
    struct VecStereo {
        data: Vec<f32>,
        pos: usize,
        rate: u32,
    }
    impl PcmSource for VecStereo {
        fn channels(&self) -> u16 {
            2
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            let frames = out.len() / 2;
            let avail = self.data.len() / 2 - self.pos;
            let n = frames.min(avail);
            out[..n * 2].copy_from_slice(&self.data[self.pos * 2..(self.pos + n) * 2]);
            self.pos += n;
            Ok(n)
        }
        fn is_exhausted(&self) -> bool {
            self.pos * 2 >= self.data.len()
        }
    }

    #[test]
    fn mut_mono_source_collapses_and_delegates() {
        let inner = VecStereo {
            data: vec![0.2, 0.4, 0.6, 0.8, -0.1, 0.1],
            pos: 0,
            rate: 44_100,
        };
        let mut m = MonoSource::new(inner);
        assert_eq!(m.channels(), 1);
        assert_eq!(m.sample_rate(), 44_100, "delegates inner rate");
        assert!(!m.is_exhausted(), "not exhausted before reading");
        let mut out = [0.0f32; 3];
        let got = m.read(&mut out).unwrap();
        assert_eq!(got, 3, "pulled 3 stereo frames");
        assert!((out[0] - 0.3).abs() < 1e-7, "(0.2+0.4)/2");
        assert!((out[1] - 0.7).abs() < 1e-7, "(0.6+0.8)/2");
        assert!((out[2] - 0.0).abs() < 1e-7, "(-0.1+0.1)/2");
        assert!(m.is_exhausted(), "exhausted after draining");
    }
}
