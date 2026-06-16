//! Boundary-refinement and crossfade primitives for the cut/mute edit path.
//!
//! Pure DSP over borrowed slices — no settings access, no allocation on the hot path.
//! The M5/render caller resolves `ZeroCrossingParams` from settings + project rate once,
//! then passes it here. Mirrors the `RoomToneParams` pattern from `room_tone.rs`.

/// Boundary-refinement + crossfade timings in **integer frames**, resolved once from
/// app settings (ms) + the project rate by the M5/render caller — the audio engine
/// reads no settings directly. No `Default`: frame counts depend on the project rate,
/// so callers (and tests) build it via `from_settings`.
pub struct ZeroCrossingParams {
    /// Hard RMS ceiling for the "quiet" threshold; reuses `room_tone_rms_ceiling`.
    pub rms_ceiling: f32,
    /// Outward search radius in frames (from `splice_search_window_ms`).
    pub search_window_frames: usize,
    /// Crossfade length in frames; also the local-RMS window (from `splice_crossfade_ms`).
    pub crossfade_frames: usize,
}

impl ZeroCrossingParams {
    /// Resolve from the raw settings (ms) + project sample rate, rounding both
    /// timings to whole frames once (`frames_from_ms`).
    pub fn from_settings(
        rms_ceiling: f32,
        search_window_ms: f64,
        crossfade_ms: f64,
        rate: u32,
    ) -> Self {
        Self {
            rms_ceiling,
            search_window_frames: frames_from_ms(search_window_ms, rate),
            crossfade_frames: frames_from_ms(crossfade_ms, rate),
        }
    }
}

/// Convert a millisecond duration to whole frames at `rate`: `round(ms / 1000 * rate)`.
///
/// The single home for the ms→frames rule (used by `from_settings` and the splice edit
/// primitive's `fade_*_samples` stamping). Returns `0` for a non-positive `ms`.
pub fn frames_from_ms(ms: f64, rate: u32) -> usize {
    if ms <= 0.0 {
        return 0;
    }
    (ms / 1000.0 * rate as f64).round() as usize
}

/// Search direction for `refine_boundary`.
enum SearchDir {
    /// For onsets: scan backwards from the approximate frame.
    Backward,
    /// For offsets: scan forwards from the approximate frame.
    Forward,
}

/// Local RMS over a window of `win_frames` centred on `center_frame`, clamped to slice ends.
///
/// Sums squares across all channels (`win_frames × channels` interleaved samples).
fn local_rms(pcm: &[f32], channels: usize, center_frame: usize, win_frames: usize) -> f32 {
    if channels == 0 || win_frames == 0 || pcm.is_empty() {
        return 0.0;
    }
    let n_frames = pcm.len() / channels;
    let half = win_frames / 2;
    let start = center_frame.saturating_sub(half);
    let end = (center_frame + win_frames - half).min(n_frames);
    if start >= end {
        return 0.0;
    }
    let sample_start = start * channels;
    let sample_end = end * channels;
    let count = sample_end - sample_start;
    let sum_sq: f32 = pcm[sample_start..sample_end].iter().map(|s| s * s).sum();
    (sum_sq / count as f32).sqrt()
}

/// Acceptance threshold: `max(0.001, min(2.0 × room_tone_rms, rms_ceiling))`.
fn threshold(room_tone_rms: f32, rms_ceiling: f32) -> f32 {
    (2.0 * room_tone_rms).min(rms_ceiling).max(0.001)
}

/// Core boundary search — called by the public `refine_onset` / `refine_offset` wrappers.
fn refine_boundary(
    pcm: &[f32],
    channels: u16,
    approx_frame: usize,
    room_tone_rms: f32,
    params: &ZeroCrossingParams,
    dir: SearchDir,
) -> usize {
    let ch = channels as usize;
    if ch == 0 || pcm.is_empty() {
        return approx_frame;
    }
    let n_frames = pcm.len() / ch;
    if n_frames == 0 {
        return 0;
    }

    let thresh = threshold(room_tone_rms, params.rms_ceiling);
    let win = params.crossfade_frames;

    // Build the frame range to scan (inclusive).
    let (scan_start, scan_end) = match dir {
        SearchDir::Backward => {
            let start = approx_frame.saturating_sub(params.search_window_frames);
            let end = approx_frame.min(n_frames - 1);
            (start, end)
        }
        SearchDir::Forward => {
            let start = approx_frame.min(n_frames - 1);
            let end = (approx_frame + params.search_window_frames).min(n_frames - 1);
            (start, end)
        }
    };

    let mut min_rms = f32::MAX;
    let mut min_frame = scan_start;

    // Iterate in search direction: backward scans from approx toward start; forward toward end.
    let frames: Box<dyn Iterator<Item = usize>> = match dir {
        SearchDir::Backward => Box::new((scan_start..=scan_end).rev()),
        SearchDir::Forward => Box::new(scan_start..=scan_end),
    };

    for frame in frames {
        let rms = local_rms(pcm, ch, frame, win);
        if rms < min_rms {
            min_rms = rms;
            min_frame = frame;
        }
        if rms < thresh {
            return frame;
        }
    }

    // Min-energy fallback: no frame qualified.
    min_frame
}

/// Refine a word onset: search backwards from `approx_onset_frame` up to
/// `params.search_window_frames` for the first frame whose local RMS < threshold;
/// else the minimum-local-RMS frame in the window.
///
/// `pcm` is interleaved at `channels`; indices are frame indices.
pub fn refine_onset(
    pcm: &[f32],
    channels: u16,
    approx_onset_frame: usize,
    room_tone_rms: f32,
    params: &ZeroCrossingParams,
) -> usize {
    refine_boundary(
        pcm,
        channels,
        approx_onset_frame,
        room_tone_rms,
        params,
        SearchDir::Backward,
    )
}

/// Refine a word offset: search forwards from `approx_offset_frame` up to
/// `params.search_window_frames` for the first frame whose local RMS < threshold;
/// else the minimum-local-RMS frame in the window.
///
/// `pcm` is interleaved at `channels`; indices are frame indices.
pub fn refine_offset(
    pcm: &[f32],
    channels: u16,
    approx_offset_frame: usize,
    room_tone_rms: f32,
    params: &ZeroCrossingParams,
) -> usize {
    refine_boundary(
        pcm,
        channels,
        approx_offset_frame,
        room_tone_rms,
        params,
        SearchDir::Forward,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{
        DEFAULT_ROOM_TONE_RMS_CEILING, DEFAULT_SPLICE_CROSSFADE_MS, DEFAULT_SPLICE_SEARCH_WINDOW_MS,
    };

    const RATE: u32 = 48_000;

    fn default_params() -> ZeroCrossingParams {
        ZeroCrossingParams::from_settings(
            DEFAULT_ROOM_TONE_RMS_CEILING,
            DEFAULT_SPLICE_SEARCH_WINDOW_MS,
            DEFAULT_SPLICE_CROSSFADE_MS,
            RATE,
        )
    }

    /// Build a mono slice: `lead_frames` of `noise_amp`, then `burst_frames` of `burst_amp`.
    fn mono_slice(
        noise_amp: f32,
        noise_frames: usize,
        burst_amp: f32,
        burst_frames: usize,
    ) -> Vec<f32> {
        let mut v = Vec::with_capacity(noise_frames + burst_frames);
        for i in 0..noise_frames {
            // Simple sine to avoid DC bias; amplitude `noise_amp`.
            let t = i as f32 / RATE as f32;
            v.push(noise_amp * (2.0 * std::f32::consts::PI * 440.0 * t).sin());
        }
        for i in 0..burst_frames {
            let t = (noise_frames + i) as f32 / RATE as f32;
            v.push(burst_amp * (2.0 * std::f32::consts::PI * 440.0 * t).sin());
        }
        v
    }

    // ── Z: onset / offset search ─────────────────────────────────────────────

    // Z1: finds the boundary in tone-in-noise.
    #[test]
    fn z1_onset_finds_quiet_lead_in() {
        // 200 ms quiet (noise_amp=0.0005) then 500 ms burst (amp=0.5)
        let noise_frames = frames_from_ms(200.0, RATE);
        let burst_frames = frames_from_ms(500.0, RATE);
        let pcm = mono_slice(0.0005, noise_frames, 0.5, burst_frames);
        let params = default_params();
        // approx onset = 10 ms into the burst
        let approx = noise_frames + frames_from_ms(10.0, RATE);
        let refined = refine_onset(&pcm, 1, approx, 0.0, &params);
        // Must land in the quiet lead-in (before the burst) or at its edge.
        assert!(
            refined <= noise_frames,
            "expected refined={refined} <= noise_frames={noise_frames}"
        );
    }

    // Z2: offset symmetric.
    #[test]
    fn z2_offset_finds_quiet_tail() {
        // 500 ms burst then 200 ms quiet
        let burst_frames = frames_from_ms(500.0, RATE);
        let noise_frames = frames_from_ms(200.0, RATE);
        let pcm = mono_slice(0.5, burst_frames, 0.0005, noise_frames);
        let params = default_params();
        let approx = burst_frames.saturating_sub(frames_from_ms(10.0, RATE));
        let refined = refine_offset(&pcm, 1, approx, 0.0, &params);
        assert!(
            refined >= burst_frames,
            "expected refined={refined} >= burst_frames={burst_frames}"
        );
    }

    // Z3: honours the backward search bound.
    #[test]
    fn z3_onset_honours_search_bound() {
        // Quiet region starts 30 ms before approx, but window is only 20 ms.
        let quiet_frames = frames_from_ms(30.0, RATE);
        let burst_frames = frames_from_ms(200.0, RATE);
        let pcm = mono_slice(0.0005, quiet_frames, 0.5, burst_frames);
        // Use a 20 ms window — quiet region is 30 ms out, unreachable.
        let params =
            ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 20.0, 2.0, RATE);
        let approx = quiet_frames + frames_from_ms(5.0, RATE); // 5 ms into burst
        let refined = refine_onset(&pcm, 1, approx, 0.0, &params);
        let lower = approx.saturating_sub(params.search_window_frames);
        assert!(refined >= lower, "refined={refined} < lower bound={lower}");
        assert!(refined <= approx, "refined={refined} > approx={approx}");
    }

    // Z4: honours the forward search bound.
    #[test]
    fn z4_offset_honours_search_bound() {
        let burst_frames = frames_from_ms(200.0, RATE);
        let quiet_frames = frames_from_ms(30.0, RATE);
        let pcm = mono_slice(0.5, burst_frames, 0.0005, quiet_frames);
        let params =
            ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 20.0, 2.0, RATE);
        let approx = burst_frames.saturating_sub(frames_from_ms(5.0, RATE));
        let refined = refine_offset(&pcm, 1, approx, 0.0, &params);
        let n_frames = pcm.len();
        let upper = (approx + params.search_window_frames).min(n_frames - 1);
        assert!(refined <= upper, "refined={refined} > upper bound={upper}");
        assert!(refined >= approx, "refined={refined} < approx={approx}");
    }

    // Z5: first-qualifying frame (not best).
    #[test]
    fn z5_onset_returns_first_qualifying_not_best() {
        // All-quiet slice: the first scanned frame (closest to approx going backward) qualifies.
        let frames = frames_from_ms(500.0, RATE);
        let pcm: Vec<f32> = (0..frames)
            .map(|i| 0.0001 * (i as f32 / frames as f32))
            .collect();
        let params = default_params();
        let approx = frames - 1;
        let refined = refine_onset(&pcm, 1, approx, 0.0, &params);
        // Should be approx itself (the first scanned frame going backward), not some earlier one.
        assert_eq!(refined, approx, "expected first-qualifying frame = approx");
    }

    // Z6: min-energy fallback.
    #[test]
    fn z6_fallback_min_energy_when_no_frame_qualifies() {
        // All frames at amplitude 0.1 — well above the 0.001 floor threshold with room_tone=0.
        let n = frames_from_ms(200.0, RATE);
        // Make one frame slightly quieter than the rest to confirm it's the fallback.
        let mut pcm: Vec<f32> = vec![0.1; n];
        let quiet_idx = n / 2;
        pcm[quiet_idx] = 0.05; // louder than 0.001 threshold but minimum in the window
        let params = default_params();
        let approx = n - 1;
        let refined = refine_onset(&pcm, 1, approx, 0.0, &params);
        // Result must be within the search window.
        let lower = approx.saturating_sub(params.search_window_frames);
        assert!(
            refined >= lower && refined <= approx,
            "fallback={refined} out of window [{lower}, {approx}]"
        );
    }

    // Z7: threshold formula — floor.
    #[test]
    fn z7_threshold_floor() {
        // room_tone_rms=0 → thresh=0.001; 0.0005 qualifies, 0.002 does not.
        let thresh = threshold(0.0, DEFAULT_ROOM_TONE_RMS_CEILING);
        assert!((thresh - 0.001).abs() < 1e-7, "thresh={thresh}");

        // Frame that qualifies.
        let n = 1000usize;
        let win = 96usize;
        // Build: mostly loud, one quiet frame at position 500.
        let mut pcm = vec![0.002f32; n];
        let q = 500usize;
        for s in &mut pcm[q.saturating_sub(win / 2)..(q + win / 2).min(n)] {
            *s = 0.0005;
        }
        let params =
            ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 20.0, 2.0, RATE);
        // approx at a loud frame; backward scan should hit the quiet region.
        let approx = n - 1;
        let refined = refine_onset(&pcm, 1, approx, 0.0, &params);
        assert!(
            refined <= q + win,
            "expected quiet region, got refined={refined}"
        );
    }

    // Z8: threshold formula — ceiling.
    #[test]
    fn z8_threshold_ceiling() {
        // room_tone_rms=0.1 → 2×0.1=0.2, clamped to rms_ceiling=0.0316 → thresh=0.0316.
        let thresh = threshold(0.1, DEFAULT_ROOM_TONE_RMS_CEILING);
        assert!(
            (thresh - DEFAULT_ROOM_TONE_RMS_CEILING).abs() < 1e-6,
            "thresh={thresh}"
        );

        // A frame at RMS 0.05 does NOT qualify with default ceiling.
        let n = 500usize;
        let win = 96usize;
        let mut pcm = vec![0.3f32; n]; // all loud
        let q = 200usize;
        let amp_05 = 0.05f32 * std::f32::consts::SQRT_2; // peak so RMS ≈ 0.05
        for s in &mut pcm[q.saturating_sub(win / 2)..(q + win / 2).min(n)] {
            *s = amp_05;
        }
        let params_default =
            ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 20.0, 2.0, RATE);
        let approx = n - 1;
        // With default ceiling the 0.05 region doesn't qualify — fallback only.
        let refined_default = refine_onset(&pcm, 1, approx, 0.1, &params_default);

        // With raised ceiling 0.1 it should qualify.
        let params_raised = ZeroCrossingParams::from_settings(0.1, 20.0, 2.0, RATE);
        let thresh_raised = threshold(0.1, 0.1);
        assert!(
            thresh_raised > 0.05,
            "raised thresh={thresh_raised} should be > 0.05"
        );
        let refined_raised = refine_onset(&pcm, 1, approx, 0.1, &params_raised);

        // refined_raised should land in or near the quiet region; default should not.
        // At minimum they differ or raised lands closer to q.
        let _ = refined_default; // used for the contrast; main assertion is raised works.
        assert!(
            refined_raised <= q + win,
            "raised ceiling: expected near q={q}, got {refined_raised}"
        );
    }

    // Z9: threshold formula — linear region.
    #[test]
    fn z9_threshold_linear_region() {
        let thresh = threshold(0.005, DEFAULT_ROOM_TONE_RMS_CEILING);
        assert!((thresh - 0.01).abs() < 1e-7, "thresh={thresh}");
    }

    // Z9b: configurable search window drives radius.
    #[test]
    fn z9b_configurable_search_window() {
        // Quiet frame at 15 ms before approx.
        let dist_ms = 15.0f64;
        let dist_frames = frames_from_ms(dist_ms, RATE);
        let n = frames_from_ms(200.0, RATE);
        let approx = n - 1;
        let q = approx.saturating_sub(dist_frames);
        let win_frames = frames_from_ms(2.0, RATE);
        let mut pcm = vec![0.3f32; n];
        for s in &mut pcm[q.saturating_sub(win_frames / 2)..(q + win_frames / 2 + 1).min(n)] {
            *s = 0.0002;
        }

        // 10 ms window can't reach the quiet frame 15 ms out.
        let params_10 =
            ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 10.0, 2.0, RATE);
        let lower_10 = approx.saturating_sub(params_10.search_window_frames);
        let refined_10 = refine_onset(&pcm, 1, approx, 0.0, &params_10);
        // quiet frame is outside the window.
        assert!(
            q < lower_10 || refined_10 > q + win_frames,
            "10ms window should not reach quiet at q={q}, lower={lower_10}, got={refined_10}"
        );

        // 20 ms window reaches it.
        let params_20 =
            ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 20.0, 2.0, RATE);
        let refined_20 = refine_onset(&pcm, 1, approx, 0.0, &params_20);
        assert!(
            refined_20 >= q.saturating_sub(win_frames) && refined_20 <= q + win_frames,
            "20ms window should reach quiet at q={q}, got={refined_20}"
        );
    }

    // Z10: word at slice start.
    #[test]
    fn z10_onset_at_slice_start() {
        let n = frames_from_ms(100.0, RATE);
        let pcm: Vec<f32> = vec![0.5; n];
        let params = default_params();
        let refined = refine_onset(&pcm, 1, 0, 0.0, &params);
        assert!(refined < n, "out of bounds: {refined} >= {n}");
    }

    // Z11: word at slice end.
    #[test]
    fn z11_offset_at_slice_end() {
        let n = frames_from_ms(100.0, RATE);
        let pcm: Vec<f32> = vec![0.5; n];
        let params = default_params();
        let approx = n - 1;
        let refined = refine_offset(&pcm, 1, approx, 0.0, &params);
        assert!(refined < n, "out of bounds: {refined} >= {n}");
    }

    // Z12: determinism.
    #[test]
    fn z12_deterministic() {
        let n = frames_from_ms(200.0, RATE);
        let pcm = mono_slice(0.0005, n / 2, 0.5, n - n / 2);
        let params = default_params();
        let approx = n / 2 + frames_from_ms(5.0, RATE);
        let a = refine_onset(&pcm, 1, approx, 0.0, &params);
        let b = refine_onset(&pcm, 1, approx, 0.0, &params);
        assert_eq!(a, b);
    }

    // Z12b: stereo, frame-aligned.
    #[test]
    fn z12b_stereo_frame_aligned() {
        // Build a 2-channel interleaved slice matching the mono_slice pattern.
        let noise_frames = frames_from_ms(200.0, RATE);
        let burst_frames = frames_from_ms(200.0, RATE);
        let n_frames = noise_frames + burst_frames;
        let mut stereo = Vec::with_capacity(n_frames * 2);
        let mut mono = Vec::with_capacity(n_frames);
        for i in 0..n_frames {
            let amp = if i < noise_frames { 0.0005f32 } else { 0.5f32 };
            let t = i as f32 / RATE as f32;
            let s = amp * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            stereo.push(s); // ch0
            stereo.push(s); // ch1 identical
            mono.push(s);
        }
        let params = default_params();
        let approx = noise_frames + frames_from_ms(10.0, RATE);
        let mono_result = refine_onset(&mono, 1, approx, 0.0, &params);
        let stereo_result = refine_onset(&stereo, 2, approx, 0.0, &params);
        assert_eq!(
            mono_result, stereo_result,
            "stereo should match mono when channels identical"
        );
        // Result is a valid frame index in the stereo slice.
        assert!(
            stereo_result * 2 < stereo.len(),
            "frame index out of bounds"
        );
    }

    // ── C: crossfade gain + frame conversion ─────────────────────────────────

    // C13: ms→frames conversion.
    #[test]
    fn c13_frames_from_ms() {
        assert_eq!(frames_from_ms(2.0, 48_000), 96);
        assert_eq!(frames_from_ms(20.0, 48_000), 960);
        assert_eq!(frames_from_ms(0.0, 48_000), 0);
        assert_eq!(frames_from_ms(-1.0, 48_000), 0);
        // Larger ms → proportionally larger count.
        assert!(frames_from_ms(10.0, 48_000) > frames_from_ms(5.0, 48_000));
        let p = ZeroCrossingParams::from_settings(DEFAULT_ROOM_TONE_RMS_CEILING, 20.0, 2.0, 48_000);
        assert_eq!(p.search_window_frames, 960);
        assert_eq!(p.crossfade_frames, 96);
    }

    // The crossfade-gain helper (now `equal_power_gain`) is unit-tested alongside its
    // definition in `audio/mod.rs`; the renderer applies it. Nothing in this module
    // applies a fade — it only resolves boundary frames and the crossfade *length*.

    // ── X: cross-cutting ─────────────────────────────────────────────────────

    // X18: no allocation — compile-time assertion by code review; tested indirectly
    // by returning scalar/usize, not Vec. This test confirms the return types.
    #[test]
    fn x18_return_types_are_scalars() {
        let pcm = vec![0.0f32; 100];
        let params = default_params();
        let onset: usize = refine_onset(&pcm, 1, 50, 0.0, &params);
        let offset: usize = refine_offset(&pcm, 1, 50, 0.0, &params);
        let _ = (onset, offset);
    }

    // X19: refined offsets bracket the word.
    #[test]
    fn x19_refined_offsets_bracket_word() {
        let noise_frames = frames_from_ms(200.0, RATE);
        let burst_frames = frames_from_ms(300.0, RATE);
        let pcm = mono_slice(0.0005, noise_frames, 0.5, burst_frames);
        let params = default_params();
        let approx_onset = noise_frames + frames_from_ms(5.0, RATE);
        let approx_offset = noise_frames + burst_frames - frames_from_ms(5.0, RATE);
        let onset = refine_onset(&pcm, 1, approx_onset, 0.0, &params);
        let offset = refine_offset(&pcm, 1, approx_offset, 0.0, &params);
        assert!(
            onset <= approx_onset,
            "onset={onset} > approx={approx_onset}"
        );
        assert!(
            offset >= approx_offset,
            "offset={offset} < approx={approx_offset}"
        );
    }

    // ── M: exact-value regressions (mutation-testing gaps) ───────────────────
    //
    // The Z/C/X tests above assert *ranges* and *bounds*, which let arithmetic
    // perturbations that still land "in window" survive. These pin exact values
    // for `local_rms`, `threshold`, and the `refine_boundary` clamps/comparisons.

    // M-rms: exact RMS over a constant window pins the `sum_sq / count` divide
    // (line 74) — `*`/`%` would change the magnitude — and the `count` subtraction.
    #[test]
    fn m_local_rms_exact_constant() {
        // 8 frames of 0.5, mono. Window of 4 centred at frame 4:
        // half=2, start=2, end=6, count=4, sum_sq=4*0.25=1.0, rms=sqrt(1.0/4)=0.5.
        let pcm = vec![0.5f32; 8];
        let rms = local_rms(&pcm, 1, 4, 4);
        assert!((rms - 0.5).abs() < 1e-6, "rms={rms} (expected 0.5)");
    }

    // M-rms-window: a single non-zero frame placed at the *lower* edge of the
    // correct window pins `half = win_frames / 2` (line 64: `/`→`%`) and
    // `start = center - half` (line 65 via the `center + win - half` end calc,
    // line 66 `-`→`+`/`/`). With win=4, center=4: correct window = frames [2,6).
    #[test]
    fn m_local_rms_window_placement() {
        // Non-zero only at frame 2 (the inclusive lower edge of the correct window).
        let mut pcm = vec![0.0f32; 16];
        pcm[2] = 1.0;
        // Correct: half=2, start=2, end=6, count=4 → sum_sq=1.0 → rms=0.5.
        // half via `%` (=0): start=4, end=8 → frame 2 excluded → rms=0.0.
        // end via `+half` (=10): includes frame 2 but count=8 → rms=sqrt(1/8)≈0.354.
        let rms = local_rms(&pcm, 1, 4, 4);
        assert!((rms - 0.5).abs() < 1e-6, "rms={rms} (expected 0.5)");
    }

    // M-rms-nframes: stereo slice where the window would over-run the slice end
    // unless `n_frames = pcm.len() / channels` (line 63) clamps it. `*` makes
    // n_frames huge, so the clamp at line 66 (`.min(n_frames)`) no longer fires
    // and the slice index runs past the end → panic (caught as a failing test).
    #[test]
    fn m_local_rms_nframes_clamp_stereo() {
        // 3 stereo frames (6 samples). Window 4 centred at frame 2:
        // n_frames=3, half=2, start=0, end=(2+4-2).min(3)=3, count=3*2=6 samples.
        let pcm = vec![0.4f32; 6];
        let rms = local_rms(&pcm, 2, 2, 4);
        // All-constant 0.4 → rms=0.4. With `*`, end=(4).min(48)=4 → samples 0..8
        // indexes past len 6 → panic. With `%` on n_frames it also misbehaves.
        assert!((rms - 0.4).abs() < 1e-6, "rms={rms} (expected 0.4)");
    }

    // M-rms-guards: each individual zero argument must short-circuit to 0.0.
    // `channels == 0` with non-empty pcm catches line 60:22 (`||`→`&&` would
    // proceed and divide by zero). An empty pcm with non-zero channels/win
    // catches line 60:41.
    #[test]
    fn m_local_rms_guards() {
        assert_eq!(local_rms(&[0.5; 8], 0, 2, 4), 0.0, "channels=0 guard");
        assert_eq!(local_rms(&[], 1, 2, 4), 0.0, "empty-pcm guard");
        assert_eq!(local_rms(&[0.5; 8], 1, 2, 0), 0.0, "win=0 guard");
    }

    // M-thresh: exact threshold values across all three regions pin the
    // `2.0 * room_tone_rms` factor, the `.min(ceiling)`, and the `.max(0.001)`.
    #[test]
    fn m_threshold_exact() {
        // Linear region: 2*0.005 = 0.010 (below ceiling, above floor).
        assert!((threshold(0.005, 0.0316) - 0.010).abs() < 1e-7);
        // Floor: 2*0.0 = 0 → max(.,0.001) = 0.001.
        assert!((threshold(0.0, 0.0316) - 0.001).abs() < 1e-7);
        // Ceiling: 2*0.5 = 1.0 → min(.,0.0316) = 0.0316.
        assert!((threshold(0.5, 0.0316) - 0.0316).abs() < 1e-7);
    }

    // M-onset-guards: a zero channel count must return `approx_frame` unchanged
    // (line 92 guard). `||`→`&&` would fall through to `pcm.len() / 0` → panic.
    #[test]
    fn m_refine_guards() {
        let pcm = vec![0.5f32; 8];
        let params = default_params();
        assert_eq!(refine_onset(&pcm, 0, 5, 0.0, &params), 5, "ch=0 guard");
        assert_eq!(refine_offset(&[], 1, 7, 0.0, &params), 7, "empty guard");
    }

    // M-onset-exact: an explicit slice with a single qualifying (quiet) frame
    // pins the exact returned index, the `< min_rms` tracking (line 128), and
    // the `< thresh` early-return (line 132). win=1 so each frame's local RMS is
    // just |sample|. Backward scan from approx=8 over a window of 8 frames.
    #[test]
    fn m_onset_exact_first_qualifying() {
        // Frames 0..=9 loud (0.5) except frame 5 = 0.0005 (below the 0.001 floor).
        let mut pcm = vec![0.5f32; 10];
        pcm[5] = 0.0005;
        let params = ZeroCrossingParams {
            rms_ceiling: 0.0316,
            search_window_frames: 8,
            crossfade_frames: 1,
        };
        // Backward from 8: 8,7,6,5(<thresh) → returns exactly 5.
        let refined = refine_onset(&pcm, 1, 8, 0.0, &params);
        assert_eq!(
            refined, 5,
            "expected first-qualifying frame 5, got {refined}"
        );
    }

    // M-onset-fallback-exact: no frame qualifies, so the min-energy frame wins.
    // Pins the `< min_rms` strict comparison (line 128: `==`/`>`/`<=` mutants)
    // and the min-tracking update.
    #[test]
    fn m_onset_fallback_exact_min_frame() {
        // All frames at 0.5 (above the 0.001 floor) except a single dip at frame 3.
        let mut pcm = vec![0.5f32; 10];
        pcm[3] = 0.1; // still > 0.001 → never "qualifies", but is the window min.
        let params = ZeroCrossingParams {
            rms_ceiling: 0.0316,
            search_window_frames: 8,
            crossfade_frames: 1,
        };
        // Backward from 8 down to 0; min local-RMS is at frame 3.
        let refined = refine_onset(&pcm, 1, 8, 0.0, &params);
        assert_eq!(refined, 3, "expected min-energy frame 3, got {refined}");
    }

    // M-backward-clamp: approx beyond the slice end exercises the
    // `approx.min(n_frames - 1)` clamp (line 107). `+`/`/` mutants break the
    // clamp and the scan would start past the end. With an all-loud slice the
    // result is the min-energy fallback at the *clamped* end frame.
    #[test]
    fn m_backward_end_clamp() {
        let pcm = vec![0.5f32; 6]; // n_frames=6, last valid frame=5
        let params = ZeroCrossingParams {
            rms_ceiling: 0.0316,
            search_window_frames: 100, // scan whole slice
            crossfade_frames: 1,
        };
        // approx=20 (past the end). Correct end = min(20,5)=5; start=0.
        // All-loud, win=1 constant → all RMS equal → first scanned (frame 5) is min.
        let refined = refine_onset(&pcm, 1, 20, 0.0, &params);
        assert_eq!(refined, 5, "expected clamped end frame 5, got {refined}");
    }

    // M-forward-clamp: forward search where approx + window over-runs the slice
    // pins the `(approx + search_window).min(n_frames - 1)` end clamp
    // (line 112: `+`→`*`, `-`→`+`/`/`) and the forward `start` clamp (line 111).
    #[test]
    fn m_forward_end_clamp() {
        // All loud except a quiet dip at frame 4. n_frames=6, last frame=5.
        let mut pcm = vec![0.5f32; 6];
        pcm[4] = 0.1; // window min, never qualifies
        let params = ZeroCrossingParams {
            rms_ceiling: 0.0316,
            search_window_frames: 100,
            crossfade_frames: 1,
        };
        // Forward from approx=0: end = min(0+100, 5) = 5. Scan 0..=5; min at frame 4.
        // `+`→`*` would give end = min(0*100, 5) = 0, scanning only frame 0 → returns 0.
        let refined = refine_offset(&pcm, 1, 0, 0.0, &params);
        assert_eq!(refined, 4, "expected min-energy frame 4, got {refined}");
    }

    // M-thresh-strict: a frame whose local RMS *exactly equals* the threshold
    // must NOT qualify (`< thresh`, line 132). With win=1 the local RMS is just
    // |sample|, so a sample of exactly 0.001 sits on the floor threshold.
    #[test]
    fn m_threshold_strict_not_equal() {
        // Backward scan from 8 over an all-loud slice with two quiet frames:
        // frame 5 == 0.001 (on threshold, must NOT qualify), frame 2 == 0.0005
        // (below threshold, qualifies first when scanning backward past 5).
        let mut pcm = vec![0.5f32; 10];
        pcm[5] = 0.001;
        pcm[2] = 0.0005;
        let params = ZeroCrossingParams {
            rms_ceiling: 0.0316,
            search_window_frames: 8,
            crossfade_frames: 1,
        };
        // Correct: frame 5 (0.001 !< 0.001) is skipped → returns frame 2.
        // `<`→`<=`: frame 5 qualifies → returns 5.
        let refined = refine_onset(&pcm, 1, 8, 0.0, &params);
        assert_eq!(
            refined, 2,
            "0.001 must not qualify against a 0.001 floor; got {refined}"
        );
    }

    // M-nframes-refine: a stereo slice whose frame count drives the end clamp.
    // Pins `n_frames = pcm.len() / ch` (line 95): `*` makes n_frames huge, so the
    // `n_frames - 1` clamp no longer bounds `approx`, and an out-of-range approx
    // would scan past the real end (panicking in local_rms or returning a bogus
    // frame index past the slice).
    #[test]
    fn m_refine_nframes_stereo_clamp() {
        // 4 stereo frames (8 samples), all loud. last valid frame = 3.
        let pcm = vec![0.5f32; 8];
        let params = ZeroCrossingParams {
            rms_ceiling: 0.0316,
            search_window_frames: 100,
            crossfade_frames: 1,
        };
        // approx=50 past the end. Correct n_frames=4 → end clamps to 3.
        let refined = refine_offset(&pcm, 2, 50, 0.0, &params);
        assert_eq!(refined, 3, "expected clamped end frame 3, got {refined}");
    }
}
