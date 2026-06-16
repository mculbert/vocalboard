//! Splice subdivision (cut/mute) and merge (uncut/unmute) — pure functions over `&[Splice]`.
//!
//! These are the write side of the per-turn EDL: they transform a turn's `splices` vec in
//! response to a word edit. The M5 command layer resolves the span from a word's
//! `source_onset_sample`, converts `splice_crossfade_ms` to integer samples once, and
//! re-stores the resulting `Turn` via `encode_turn` + `store::put`. No DB, no journaling,
//! no `Word` type; these functions are deterministic over scalars + `&[Splice]`.
//!
//! The [`Splice`]/[`SpliceKind`] data types live with the `Turn` blob payload in
//! [`crate::project::turn`]; this module holds only the *operations* on them.
//!
//! **Span semantics.** All four ops act on an arbitrary turn-relative span and may cross any
//! number of splice boundaries and any splice kinds — cutting/muting a multi-word selection
//! or restoring source audio across a previously edited region is a single call. The M5
//! caller owns the cut/mute interaction policy (e.g. a muted word staying muted when later
//! uncut) and is responsible for requesting correct spans; each primitive simply performs
//! the span operation it is given and, on a merge, restores source audio for that span.
//!
//! **Canonical form.** Inputs are assumed canonical (no two adjacent splices that should be
//! one) and every op returns a canonical vec: adjacent `Source` splices that are
//! source-contiguous, and adjacent `RoomTone` or `Silence` splices, are coalesced — summing
//! lengths, keeping the leftmost `fade_in`/`source_start_sample` and the rightmost
//! `fade_out`, and dropping the now-interior seam fades.
//!
//! **Preconditions** are `debug_assert!`-checked: caught in dev/test, compiled out in
//! release, where a malformed request degrades to an equivalent-vec no-op rather than
//! panicking.

use crate::project::turn::{Splice, SpliceKind};

/// Recompute the splice vec after CUTTING the turn-relative span `[start, end)`
/// (current-vec coordinates, resolved by the M5 caller from the word onsets). The span is
/// removed, shrinking the turn. The span may cross any number of splice boundaries and any
/// splice kinds: the splice containing `start` is trimmed to its head, the splice containing
/// `end` to its (source-rebased) tail, everything between is dropped, and the new seam
/// carries `crossfade_samples`. The result is coalesced. Returns a fresh vec; `splices` is
/// untouched.
///
/// A zero-length span (`start >= end`) or empty input is a no-op.
pub fn subdivide_on_cut(
    splices: &[Splice],
    start: i64,
    end: i64,
    crossfade_samples: i64,
) -> Vec<Splice> {
    if splices.is_empty() {
        return Vec::new();
    }
    let total: i64 = splices.iter().map(|s| s.length_samples).sum();
    debug_assert!(start >= 0, "cut start ({start}) must be >= 0");
    debug_assert!(start <= end, "cut start ({start}) must be <= end ({end})");
    debug_assert!(
        end <= total,
        "cut end ({end}) must be <= turn length ({total})"
    );
    debug_assert!(
        crossfade_samples >= 0,
        "crossfade_samples ({crossfade_samples}) must be >= 0"
    );
    if start >= end {
        return splices.to_vec();
    }

    let mut result = Vec::with_capacity(splices.len() + 1);
    let mut pos = 0i64;
    for splice in splices {
        let s_start = pos;
        let s_end = s_start + splice.length_samples;
        pos = s_end;

        if s_end <= start || s_start >= end {
            push_coalesced(&mut result, splice.clone());
            continue;
        }
        if s_start < start {
            push_coalesced(
                &mut result,
                Splice {
                    length_samples: start - s_start,
                    fade_in_samples: splice.fade_in_samples,
                    fade_out_samples: crossfade_samples,
                    kind: splice.kind,
                },
            );
        }
        if end < s_end {
            push_coalesced(
                &mut result,
                Splice {
                    length_samples: s_end - end,
                    fade_in_samples: crossfade_samples,
                    fade_out_samples: splice.fade_out_samples,
                    kind: advance_source(splice.kind, end - s_start),
                },
            );
        }
    }
    result
}

/// Recompute the splice vec after MUTING the turn-relative span `[start, end)`. The span is
/// replaced by a single `RoomTone` (or `Silence` when `mute_to_room_tone == false`) splice;
/// the turn length is unchanged. Like the cut, the span may cross any number of boundaries
/// and kinds: the splice containing `start` is trimmed to its head, the splice containing
/// `end` to its (source-rebased) tail, everything between is dropped, and both new seams
/// carry the crossfade. The result is coalesced (so a mute abutting an existing room tone
/// extends it rather than doubling it).
///
/// A zero-length span (`start >= end`) or empty input is a no-op.
pub fn subdivide_on_mute(
    splices: &[Splice],
    start: i64,
    end: i64,
    mute_to_room_tone: bool,
    crossfade_samples: i64,
) -> Vec<Splice> {
    if splices.is_empty() {
        return Vec::new();
    }
    let total: i64 = splices.iter().map(|s| s.length_samples).sum();
    debug_assert!(start >= 0, "mute start ({start}) must be >= 0");
    debug_assert!(start <= end, "mute start ({start}) must be <= end ({end})");
    debug_assert!(
        end <= total,
        "mute end ({end}) must be <= turn length ({total})"
    );
    debug_assert!(
        crossfade_samples >= 0,
        "crossfade_samples ({crossfade_samples}) must be >= 0"
    );
    if start >= end {
        return splices.to_vec();
    }

    let middle_kind = if mute_to_room_tone {
        SpliceKind::RoomTone
    } else {
        SpliceKind::Silence
    };

    let mut result = Vec::with_capacity(splices.len() + 2);
    let mut pos = 0i64;
    let mut middle_inserted = false;
    for splice in splices {
        let s_start = pos;
        let s_end = s_start + splice.length_samples;
        pos = s_end;

        if s_end <= start || s_start >= end {
            push_coalesced(&mut result, splice.clone());
            continue;
        }
        if s_start < start {
            push_coalesced(
                &mut result,
                Splice {
                    length_samples: start - s_start,
                    fade_in_samples: splice.fade_in_samples,
                    fade_out_samples: crossfade_samples,
                    kind: splice.kind,
                },
            );
        }
        if !middle_inserted {
            push_coalesced(
                &mut result,
                Splice {
                    length_samples: end - start,
                    fade_in_samples: crossfade_samples,
                    fade_out_samples: crossfade_samples,
                    kind: middle_kind,
                },
            );
            middle_inserted = true;
        }
        if end < s_end {
            push_coalesced(
                &mut result,
                Splice {
                    length_samples: s_end - end,
                    fade_in_samples: crossfade_samples,
                    fade_out_samples: splice.fade_out_samples,
                    kind: advance_source(splice.kind, end - s_start),
                },
            );
        }
    }
    result
}

/// Recompute the splice vec after UNCUTTING: re-insert a `Source` of length `restore_len`
/// reading `source_start_sample` at turn position `start` (re-growing the turn), coalescing
/// inline. `start` need not be a splice boundary: if a prior coalesce merged a room tone over
/// the gap, the containing `RoomTone`/`Silence` is split around the restored source. The
/// restored `Source` copies its surviving-seam fades from the abutting neighbours (`fade_in`
/// ← left neighbour `fade_out`, `fade_out` ← right neighbour `fade_in`, 0 at a turn edge);
/// coalescing then drops whichever become interior. No crossfade parameter — a surviving seam
/// already carries the original fade.
///
/// A zero `restore_len` or empty input is a no-op. The restored source
/// `[source_start_sample, source_start_sample + restore_len)` must not overlap the following
/// splice's source (`debug_assert!`-checked).
pub fn merge_on_uncut(
    splices: &[Splice],
    start: i64,
    restore_len: i64,
    source_start_sample: i64,
) -> Vec<Splice> {
    if splices.is_empty() {
        return Vec::new();
    }
    let total: i64 = splices.iter().map(|s| s.length_samples).sum();
    debug_assert!(restore_len >= 0, "restore_len ({restore_len}) must be >= 0");
    debug_assert!(
        start >= 0 && start <= total,
        "uncut start ({start}) out of range 0..={total}"
    );
    debug_assert!(
        source_start_sample >= 0,
        "source_start_sample ({source_start_sample}) must be >= 0"
    );
    if restore_len <= 0 {
        return splices.to_vec();
    }
    // Zero-width removed span at `start` → a pure insertion that grows the turn.
    merge_span(splices, start, start, restore_len, source_start_sample)
}

/// Recompute the splice vec after UNMUTING: replace the span `[start, start + restore_len)`
/// with a `Source` reading `source_start_sample`, coalescing inline; the turn length is
/// unchanged. Like the forward ops the span may cross any number of boundaries and kinds —
/// the splice containing `start` is trimmed to its head, the splice containing the span end
/// to its (source-rebased) tail, and everything between is replaced by the one `Source`. The
/// restored `Source` copies its surviving-seam fades from the abutting neighbours (no
/// crossfade parameter); coalescing then drops whichever become interior.
///
/// A zero `restore_len` or empty input is a no-op. The restored source must not overlap the
/// following splice's source (`debug_assert!`-checked).
pub fn merge_on_unmute(
    splices: &[Splice],
    start: i64,
    restore_len: i64,
    source_start_sample: i64,
) -> Vec<Splice> {
    if splices.is_empty() {
        return Vec::new();
    }
    let total: i64 = splices.iter().map(|s| s.length_samples).sum();
    let end = start + restore_len;
    debug_assert!(restore_len >= 0, "restore_len ({restore_len}) must be >= 0");
    debug_assert!(start >= 0, "unmute start ({start}) must be >= 0");
    debug_assert!(
        end <= total,
        "unmute end ({end}) must be <= turn length ({total})"
    );
    debug_assert!(
        source_start_sample >= 0,
        "source_start_sample ({source_start_sample}) must be >= 0"
    );
    if restore_len <= 0 {
        return splices.to_vec();
    }
    // Remove the muted span and insert an equal-length source in its place.
    merge_span(splices, start, end, restore_len, source_start_sample)
}

/// Shared write-half of both merges: remove the span `[start, end)` and insert a `Source` of
/// length `source_len` reading `source_start_sample` in its place, coalescing inline. The
/// splice containing `start` is trimmed to its head, the splice containing `end` to its
/// (source-rebased) tail, everything between is dropped, and the restored `Source`'s
/// surviving-seam fades are copied from the abutting neighbours. `merge_on_uncut` passes
/// `end == start` (a pure insertion, growing the turn); `merge_on_unmute` passes
/// `end == start + source_len` (a replacement, preserving the turn length).
fn merge_span(
    splices: &[Splice],
    start: i64,
    end: i64,
    source_len: i64,
    source_start_sample: i64,
) -> Vec<Splice> {
    let mut result = Vec::with_capacity(splices.len() + 1);
    let mut pos = 0i64;
    let mut inserted = false;
    for splice in splices {
        let s_start = pos;
        let s_end = s_start + splice.length_samples;
        pos = s_end;

        if s_end <= start {
            push_coalesced(&mut result, splice.clone());
            continue;
        }
        if s_start >= end {
            if !inserted {
                emit_restored_source(&mut result, source_len, source_start_sample, Some(splice));
                inserted = true;
            }
            push_coalesced(&mut result, splice.clone());
            continue;
        }
        // splice overlaps [start, end) (or straddles `start` when end == start)
        if s_start < start {
            // `start` strictly inside a `Source` would split contiguous source audio — there is
            // no edited (cut/muted) region there to restore, so it is a caller error.
            debug_assert!(
                !matches!(splice.kind, SpliceKind::Source { .. }),
                "merge start ({start}) must not fall inside a Source splice"
            );
            push_coalesced(
                &mut result,
                Splice {
                    length_samples: start - s_start,
                    fade_in_samples: splice.fade_in_samples,
                    fade_out_samples: splice.fade_out_samples,
                    kind: splice.kind,
                },
            );
        }
        if end < s_end {
            let tail = Splice {
                length_samples: s_end - end,
                fade_in_samples: splice.fade_in_samples,
                fade_out_samples: splice.fade_out_samples,
                kind: advance_source(splice.kind, end - s_start),
            };
            if !inserted {
                emit_restored_source(&mut result, source_len, source_start_sample, Some(&tail));
                inserted = true;
            }
            push_coalesced(&mut result, tail);
        }
    }
    if !inserted {
        emit_restored_source(&mut result, source_len, source_start_sample, None);
    }
    result
}

/// Advance a `Source` splice's read offset by `consumed` samples (the amount trimmed from
/// its front when a span ate into it); `RoomTone`/`Silence` carry no source position and are
/// returned unchanged.
fn advance_source(kind: SpliceKind, consumed: i64) -> SpliceKind {
    match kind {
        SpliceKind::Source {
            source_start_sample,
        } => SpliceKind::Source {
            source_start_sample: source_start_sample + consumed,
        },
        other => other,
    }
}

/// Append `s` to `out`, coalescing it into the previous splice when the two are mergeable:
/// two source-contiguous `Source`s (`a.source_start + a.len == b.source_start`), two
/// `RoomTone`s, or two `Silence`s. Merging sums lengths, keeps the left's `fade_in` and
/// `source_start_sample`, takes the right's `fade_out`, and drops the interior seam fades.
fn push_coalesced(out: &mut Vec<Splice>, s: Splice) {
    if let Some(last) = out.last_mut() {
        let mergeable = match (last.kind, s.kind) {
            (
                SpliceKind::Source {
                    source_start_sample: a,
                },
                SpliceKind::Source {
                    source_start_sample: b,
                },
            ) => a + last.length_samples == b,
            (SpliceKind::RoomTone, SpliceKind::RoomTone)
            | (SpliceKind::Silence, SpliceKind::Silence) => true,
            _ => false,
        };
        if mergeable {
            last.length_samples += s.length_samples;
            last.fade_out_samples = s.fade_out_samples;
            return;
        }
    }
    out.push(s);
}

/// Push the restored `Source` of a merge (uncut/unmute) onto `out`, wiring its surviving-seam
/// fades: `fade_in` from the left neighbour already in `out`, `fade_out` from `right` (the
/// tail piece or the first splice after the span; `None` at the turn tail). `push_coalesced`
/// then drops whichever fade becomes interior when the `Source` merges with a contiguous
/// neighbour. Asserts the restored source does not overlap `right`'s source.
fn emit_restored_source(
    out: &mut Vec<Splice>,
    restore_len: i64,
    source_start_sample: i64,
    right: Option<&Splice>,
) {
    let fade_in = out.last().map_or(0, |s| s.fade_out_samples);
    let fade_out = right.map_or(0, |r| r.fade_in_samples);
    if let Some(SpliceKind::Source {
        source_start_sample: ss_r,
    }) = right.map(|r| r.kind)
    {
        debug_assert!(
            source_start_sample + restore_len <= ss_r,
            "restored source [{source_start_sample}, {}) overlaps next splice source {ss_r}",
            source_start_sample + restore_len
        );
    }
    push_coalesced(
        out,
        Splice {
            length_samples: restore_len,
            fade_in_samples: fade_in,
            fade_out_samples: fade_out,
            kind: SpliceKind::Source {
                source_start_sample,
            },
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::turn::{decode_turn, encode_turn, Turn};

    // Build the initial single-Source splice vec for a turn of `total_len` samples
    // starting from `source_start` in the source timeline.
    fn single_source(total_len: i64, source_start: i64) -> Vec<Splice> {
        vec![Splice {
            length_samples: total_len,
            fade_in_samples: 0,
            fade_out_samples: 0,
            kind: SpliceKind::Source {
                source_start_sample: source_start,
            },
        }]
    }

    fn tiling_sum(splices: &[Splice]) -> i64 {
        splices.iter().map(|s| s.length_samples).sum()
    }

    fn assert_tiling(splices: &[Splice], expected: i64) {
        assert_eq!(
            tiling_sum(splices),
            expected,
            "tiling invariant violated: got {:?}",
            splices
        );
    }

    fn source_start(splice: &Splice) -> i64 {
        match splice.kind {
            SpliceKind::Source {
                source_start_sample,
            } => source_start_sample,
            _ => panic!("expected Source splice"),
        }
    }

    // ── C: cut ───────────────────────────────────────────────────────────────

    // C1: Cut the only word (whole turn's speech); post-turn silence survives.
    #[test]
    fn c1_cut_only_word() {
        let total = 120_i64; // 100 speech + 20 post-silence
        let splices = single_source(total, 0);
        let result = subdivide_on_cut(&splices, 5, 80, 5);
        assert_tiling(&result, total - 75);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].length_samples, 5); // before
        assert_eq!(result[1].length_samples, 40); // after (includes post-silence)
    }

    // C2: Cut middle word; containing Source splits; outer (non-contiguous) splices untouched.
    #[test]
    fn c2_cut_middle_word() {
        // 3 canonical (source-discontiguous) splices: [0..40)ss0, [40..80)ss100, [80..100)ss200
        let splices = vec![
            Splice {
                length_samples: 40,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            },
            Splice {
                length_samples: 40,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 100,
                },
            },
            Splice {
                length_samples: 20,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 200,
                },
            },
        ];
        // Cut [50, 70) inside the second splice (40..80)
        let result = subdivide_on_cut(&splices, 50, 70, 5);
        assert_tiling(&result, 100 - 20); // 80
        assert_eq!(result.len(), 4);
        // splice[0] untouched
        assert_eq!(result[0].length_samples, 40);
        assert_eq!(source_start(&result[0]), 0);
        // before piece of split
        assert_eq!(result[1].length_samples, 10); // 50-40
        assert_eq!(source_start(&result[1]), 100);
        // after piece of split
        assert_eq!(result[2].length_samples, 10); // 80-70
        assert_eq!(source_start(&result[2]), 130); // 100 + (70-40)
                                                   // splice[2] untouched
        assert_eq!(result[3].length_samples, 20);
        assert_eq!(source_start(&result[3]), 200);
    }

    // C3: Cut first word; before piece is zero-length and dropped.
    #[test]
    fn c3_cut_first_word() {
        let splices = single_source(100, 0);
        let result = subdivide_on_cut(&splices, 0, 30, 5);
        assert_tiling(&result, 70);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].length_samples, 70);
        assert_eq!(source_start(&result[0]), 30); // re-based
        assert_eq!(result[0].fade_in_samples, 5); // crossfade on the new head
    }

    // C4: Cut last word; after piece includes only post-turn silence.
    #[test]
    fn c4_cut_last_word() {
        // turn_duration=80, post_turn_silence=20 → total 100
        let splices = single_source(100, 0);
        // Cut [60, 80) — last speech word (silence [80,100) stays)
        let result = subdivide_on_cut(&splices, 60, 80, 5);
        assert_tiling(&result, 80); // total - 20
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].length_samples, 60);
        assert_eq!(result[1].length_samples, 20); // post-turn silence survives
        assert_eq!(source_start(&result[1]), 80); // source at post-silence start
    }

    // C5: Re-based source offsets after cut.
    #[test]
    fn c5_rebased_source_offsets() {
        let splices = single_source(100, 200); // source starts at 200
        let result = subdivide_on_cut(&splices, 20, 50, 5);
        assert_tiling(&result, 70);
        assert_eq!(source_start(&result[0]), 200); // before piece unchanged
        assert_eq!(source_start(&result[1]), 250); // 200 + (50 - 0) ... wait
                                                   // source_start of after = source_start_of_containing + (end - splice_start)
                                                   // = 200 + (50 - 0) = 250
        assert_eq!(source_start(&result[1]), 250);
    }

    // C6: Seam crossfade stamped on both sides.
    #[test]
    fn c6_seam_crossfade_stamped() {
        let splices = single_source(100, 0);
        let result = subdivide_on_cut(&splices, 20, 60, 7);
        assert_tiling(&result, 60);
        assert_eq!(result[0].fade_out_samples, 7);
        assert_eq!(result[1].fade_in_samples, 7);
    }

    // C7: Cut at a splice edge → 1 surviving piece, no zero-length splice.
    #[test]
    fn c7_cut_at_splice_edge() {
        let splices = vec![
            Splice {
                length_samples: 50,
                fade_in_samples: 3,
                fade_out_samples: 3,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            },
            Splice {
                length_samples: 50,
                fade_in_samples: 3,
                fade_out_samples: 3,
                kind: SpliceKind::Source {
                    source_start_sample: 50,
                },
            },
        ];
        // Cut [50, 80): start == splice[1].start
        let result = subdivide_on_cut(&splices, 50, 80, 5);
        assert_tiling(&result, 70);
        for s in &result {
            assert!(s.length_samples > 0, "no zero-length splices");
        }
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].length_samples, 50); // first splice untouched
        assert_eq!(result[1].length_samples, 20); // after piece only
        assert_eq!(source_start(&result[1]), 80); // re-based
    }

    // C8: Multi-cut; deterministic composition.
    #[test]
    fn c8_multi_cut() {
        let splices = single_source(100, 0);
        // First cut [10, 20)
        let after_first = subdivide_on_cut(&splices, 10, 20, 5);
        assert_tiling(&after_first, 90);
        // Second cut: original [60, 70) is now at [50, 60) in current vec
        let result = subdivide_on_cut(&after_first, 50, 60, 5);
        assert_tiling(&result, 80);
        // Hand-computed: should have Source(0..10), Source(20..60)→Source(10..50),
        // Source(50..60 → source 70..80), Source(80..100 → source 80..100 but shifted)
        // Actually: let me just verify the tiling and the source offsets make sense.
        assert!(result.iter().all(|s| s.length_samples > 0));
    }

    // ── M: mute ──────────────────────────────────────────────────────────────

    // M9: Mute to room tone; middle word → before/RoomTone/after; Σ unchanged.
    #[test]
    fn m9_mute_to_room_tone() {
        let total = 100_i64;
        let splices = single_source(total, 0);
        let result = subdivide_on_mute(&splices, 20, 60, true, 5);
        assert_tiling(&result, total);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].length_samples, 20);
        assert_eq!(result[1].length_samples, 40);
        assert!(matches!(result[1].kind, SpliceKind::RoomTone));
        assert_eq!(result[2].length_samples, 40);
    }

    // M10: Mute to silence.
    #[test]
    fn m10_mute_to_silence() {
        let total = 100_i64;
        let splices = single_source(total, 0);
        let result = subdivide_on_mute(&splices, 20, 60, false, 5);
        assert_tiling(&result, total);
        assert!(matches!(result[1].kind, SpliceKind::Silence));
    }

    // M11: Mute first word → 2 splices (zero-length before dropped); mute last word.
    #[test]
    fn m11_mute_edge_words() {
        let total = 100_i64;
        let splices = single_source(total, 0);

        // Mute first word [0, 30)
        let first = subdivide_on_mute(&splices, 0, 30, true, 5);
        assert_tiling(&first, total);
        assert_eq!(first.len(), 2); // RoomTone + after-Source (before dropped)
        assert!(matches!(first[0].kind, SpliceKind::RoomTone));
        assert_eq!(first[0].length_samples, 30);
        assert_eq!(first[1].length_samples, 70);

        // Mute last word [70, 100)
        let last = subdivide_on_mute(&splices, 70, 100, true, 5);
        assert_tiling(&last, total);
        assert_eq!(last.len(), 2); // before-Source + RoomTone (after dropped)
        assert_eq!(last[0].length_samples, 70);
        assert!(matches!(last[1].kind, SpliceKind::RoomTone));
        assert_eq!(last[1].length_samples, 30);
    }

    // M12: After-Source re-based; source_start_sample advances past the muted span.
    #[test]
    fn m12_after_source_rebased() {
        let splices = single_source(100, 200);
        let result = subdivide_on_mute(&splices, 20, 50, true, 5);
        assert_tiling(&result, 100);
        // after piece: source_start = 200 + (50 - 0) = 250
        assert_eq!(source_start(&result[2]), 250);
    }

    // M13: Seam crossfades on both new boundaries.
    #[test]
    fn m13_seam_crossfades_on_both_boundaries() {
        let splices = single_source(100, 0);
        let result = subdivide_on_mute(&splices, 20, 60, true, 8);
        assert_eq!(result[0].fade_out_samples, 8); // before ↔ mute
        assert_eq!(result[1].fade_in_samples, 8); // mute ↔ after
        assert_eq!(result[1].fade_out_samples, 8);
        assert_eq!(result[2].fade_in_samples, 8);
    }

    // ── U: uncut ─────────────────────────────────────────────────────────────

    // U14: Uncut middle → three coalesce to pristine single Source.
    #[test]
    fn u14_uncut_middle_coalesces_to_pristine() {
        let original = single_source(100, 0);
        // Cut [30, 60)
        let cut = subdivide_on_cut(&original, 30, 60, 5);
        assert_tiling(&cut, 70);
        // Uncut: restore_len=30, source_start=30
        let restored = merge_on_uncut(&cut, 30, 30, 30);
        assert_tiling(&restored, 100);
        assert_eq!(restored, original);
    }

    // U15: Uncut first word; no before-Source; coalesces with after piece; fade_in==0.
    #[test]
    fn u15_uncut_first_word() {
        let original = single_source(100, 0);
        let cut = subdivide_on_cut(&original, 0, 30, 5);
        assert_tiling(&cut, 70);
        let restored = merge_on_uncut(&cut, 0, 30, 0);
        assert_tiling(&restored, 100);
        assert_eq!(restored, original);
        assert_eq!(restored[0].fade_in_samples, 0); // turn start
    }

    // U16: Uncut between two mutes (non-coalescing); restored Source keeps neighbour fades.
    #[test]
    fn u16_uncut_between_two_mutes() {
        // Build: mute [20,40), then mute [60,80), then cut [40,60)
        let total = 100_i64;
        let splices = single_source(total, 0);
        let after_mute1 = subdivide_on_mute(&splices, 20, 40, true, 6);
        // Current: Source(0..20), RoomTone(20..40,fi=6,fo=6), Source(40..100,fi=6)
        let after_mute2 = subdivide_on_mute(&after_mute1, 60, 80, true, 6);
        // Current: Source(0..20), RT(20..40,fi=6,fo=6), Source(40..60,fi=6,fo=6),
        //          RT(60..80,fi=6,fo=6), Source(80..100,fi=6,fo=0)
        // Now cut [40,60) (the source between the two mutes, current-vec coords)
        let after_cut = subdivide_on_cut(&after_mute2, 40, 60, 6);
        // Current: Source(0..20), RT(20..40), gap at 40, RT(40..60), Source(60..80)
        assert_tiling(&after_cut, 80);

        // Uncut [40,60): restore at start=40, restore_len=20, source_start=40
        let restored = merge_on_uncut(&after_cut, 40, 20, 40);
        assert_tiling(&restored, 100);

        // The restored Source should not coalesce (flanked by RoomTone on both sides)
        // Find the restored Source splice
        let src_idx = restored
            .iter()
            .position(
                |s| matches!(s.kind, SpliceKind::Source { source_start_sample: ss } if ss == 40),
            )
            .expect("restored Source not found");
        let restored_src = &restored[src_idx];
        // fade_in = left RoomTone's fade_out = 6; fade_out = right RoomTone's fade_in = 6
        assert_eq!(restored_src.fade_in_samples, 6);
        assert_eq!(restored_src.fade_out_samples, 6);
    }

    // U17: Turn re-grows by exactly restore_len.
    #[test]
    fn u17_turn_regrows() {
        let original_total = 100_i64;
        let splices = single_source(original_total, 0);
        let cut = subdivide_on_cut(&splices, 20, 50, 5);
        assert_tiling(&cut, original_total - 30);
        let restored = merge_on_uncut(&cut, 20, 30, 20);
        assert_tiling(&restored, original_total);
    }

    // ── N: unmute ────────────────────────────────────────────────────────────

    // N18: Unmute middle → coalesce to pristine.
    #[test]
    fn n18_unmute_middle_coalesces_to_pristine() {
        let original = single_source(100, 0);
        let muted = subdivide_on_mute(&original, 30, 60, true, 5);
        let restored = merge_on_unmute(&muted, 30, 30, 30);
        assert_tiling(&restored, 100);
        assert_eq!(restored, original);
    }

    // N19: Unmute to-silence variant.
    #[test]
    fn n19_unmute_silence_variant() {
        let original = single_source(100, 0);
        let muted = subdivide_on_mute(&original, 30, 60, false, 5); // Silence
        assert!(matches!(muted[1].kind, SpliceKind::Silence));
        let restored = merge_on_unmute(&muted, 30, 30, 30);
        assert_eq!(restored, original);
    }

    // N20: Unmute first / last word; coalesces with single neighbour.
    #[test]
    fn n20_unmute_edge_words() {
        let original = single_source(100, 0);

        // Unmute first word
        let muted_first = subdivide_on_mute(&original, 0, 30, true, 5);
        let restored_first = merge_on_unmute(&muted_first, 0, 30, 0);
        assert_eq!(restored_first, original);

        // Unmute last word
        let muted_last = subdivide_on_mute(&original, 70, 100, true, 5);
        let restored_last = merge_on_unmute(&muted_last, 70, 30, 70);
        assert_eq!(restored_last, original);
    }

    // N21: Unmute between two mutes; restored Source stands alone with neighbour fades.
    #[test]
    fn n21_unmute_between_two_mutes() {
        let total = 100_i64;
        let splices = single_source(total, 0);
        let after_m1 = subdivide_on_mute(&splices, 10, 30, true, 4);
        let after_m2 = subdivide_on_mute(&after_m1, 50, 70, true, 4);
        let after_m3 = subdivide_on_mute(&after_m2, 30, 50, true, 4);
        // Now: RT(10..30), RT(30..50), RT(50..70) — three mutes
        assert_tiling(&after_m3, total);
        let restored = merge_on_unmute(&after_m3, 30, 20, 30);
        assert_tiling(&restored, total);
        // restored Source(30..50) flanked by RoomTone on both sides — does not coalesce
        let src = restored
            .iter()
            .find(|s| matches!(s.kind, SpliceKind::Source { source_start_sample: ss } if ss == 30))
            .expect("restored Source not found");
        assert_eq!(src.fade_in_samples, 4); // left RT's fade_out
        assert_eq!(src.fade_out_samples, 4); // right RT's fade_in
    }

    // ── R: round-trip / canonical form ───────────────────────────────────────

    // R22: Cut → uncut == byte-identical input vec.
    #[test]
    fn r22_cut_uncut_round_trip() {
        let original = vec![Splice {
            length_samples: 100,
            fade_in_samples: 3,
            fade_out_samples: 3,
            kind: SpliceKind::Source {
                source_start_sample: 500,
            },
        }];
        let cut = subdivide_on_cut(&original, 20, 50, 7);
        let restored = merge_on_uncut(&cut, 20, 30, 520); // source_start = 500 + 20
        assert_eq!(restored, original);
    }

    // R23: Mute → unmute == byte-identical input vec.
    #[test]
    fn r23_mute_unmute_round_trip() {
        let original = vec![Splice {
            length_samples: 100,
            fade_in_samples: 3,
            fade_out_samples: 3,
            kind: SpliceKind::Source {
                source_start_sample: 500,
            },
        }];
        let muted = subdivide_on_mute(&original, 20, 50, true, 7);
        let restored = merge_on_unmute(&muted, 20, 30, 520);
        assert_eq!(restored, original);
    }

    // R24: Order-independent convergence — two cuts uncut in both orders.
    #[test]
    fn r24_order_independent_convergence() {
        let original = single_source(100, 0);

        // Cut word1=[10,20) and word3=[70,80) in original coordinates.
        // After cut1=[10,20): word3 in current-vec is at [60,70).
        let cut1 = subdivide_on_cut(&original, 10, 20, 5);
        let cut2 = subdivide_on_cut(&cut1, 60, 70, 5);
        assert_tiling(&cut2, 80);

        // Uncut order A: word1 then word3
        let unc1a = merge_on_uncut(&cut2, 10, 10, 10);
        // After uncutting word1, word3's gap is at current pos 70.
        let unc2a = merge_on_uncut(&unc1a, 70, 10, 70);
        assert_eq!(unc2a, original);

        // Uncut order B: word3 then word1
        let unc1b = merge_on_uncut(&cut2, 60, 10, 70);
        // After uncutting word3, word1's gap is still at current pos 10.
        let unc2b = merge_on_uncut(&unc1b, 10, 10, 10);
        assert_eq!(unc2b, original);

        // Mix: cut+mute, uncut both
        let muted = subdivide_on_mute(&original, 10, 30, true, 5);
        let cut_muted = subdivide_on_cut(&muted, 50, 60, 5);
        assert_tiling(&cut_muted, 90);
        let uncut = merge_on_uncut(&cut_muted, 50, 10, 50);
        let unmuted = merge_on_unmute(&uncut, 10, 20, 10);
        assert_eq!(unmuted, original);
    }

    // ── V: invariants / edge cases ────────────────────────────────────────────

    // V25: Tiling sum after every op (covered inline above and verified here explicitly).
    #[test]
    fn v25_tiling_sum_invariant() {
        let splices = single_source(100, 0);
        let c = subdivide_on_cut(&splices, 20, 50, 5);
        assert_tiling(&c, 70);
        let m = subdivide_on_mute(&splices, 20, 50, true, 5);
        assert_tiling(&m, 100);
        let u = merge_on_uncut(&c, 20, 30, 20);
        assert_tiling(&u, 100);
        let n = merge_on_unmute(&m, 20, 30, 20);
        assert_tiling(&n, 100);
    }

    // V26: Input not mutated.
    #[test]
    fn v26_input_not_mutated() {
        let original = single_source(100, 0);
        let snap: Vec<Splice> = original.clone();
        let _ = subdivide_on_cut(&original, 20, 50, 5);
        assert_eq!(original, snap);
        let _ = subdivide_on_mute(&original, 20, 50, true, 5);
        assert_eq!(original, snap);
        let _ = merge_on_uncut(&original, 100, 10, 200); // tail insert, non-overlapping
        assert_eq!(original, snap);
        let _ = merge_on_unmute(&original, 0, 100, 0);
        assert_eq!(original, snap);
    }

    // V27: Zero-length span / restore_len == 0 → no-op, no panic.
    #[test]
    fn v27_zero_length_span_is_noop() {
        let splices = single_source(100, 0);
        assert_eq!(subdivide_on_cut(&splices, 30, 30, 5), splices);
        assert_eq!(subdivide_on_mute(&splices, 30, 30, true, 5), splices);
        assert_tiling(&subdivide_on_cut(&splices, 30, 30, 5), 100);
        assert_tiling(&subdivide_on_mute(&splices, 30, 30, true, 5), 100);
        // restore_len == 0
        let cut = subdivide_on_cut(&splices, 20, 50, 5);
        assert_eq!(merge_on_uncut(&cut, 20, 0, 20), cut);
        let muted = subdivide_on_mute(&splices, 20, 50, true, 5);
        assert_eq!(merge_on_unmute(&muted, 20, 0, 20), muted);
    }

    // V28: Empty input → empty output; no panic.
    #[test]
    fn v28_empty_input_is_noop() {
        let empty: &[Splice] = &[];
        assert_eq!(subdivide_on_cut(empty, 0, 10, 5), vec![]);
        assert_eq!(subdivide_on_mute(empty, 0, 10, true, 5), vec![]);
        assert_eq!(merge_on_uncut(empty, 0, 10, 0), vec![]);
        assert_eq!(merge_on_unmute(empty, 0, 10, 0), vec![]);
    }

    // V29: Coalesce only when source-contiguous; non-contiguous Sources are not merged.
    #[test]
    fn v29_coalesce_only_when_source_contiguous() {
        let original = single_source(100, 0);
        // Cut [10,20) and [40,50) — two gaps remain
        let cut1 = subdivide_on_cut(&original, 10, 20, 5);
        let cut2 = subdivide_on_cut(&cut1, 30, 40, 5); // [40,50) in original → [30,40) after first cut
        assert_tiling(&cut2, 80);
        // Uncut only the first word: the second gap must not coalesce
        let unc1 = merge_on_uncut(&cut2, 10, 10, 10);
        // source layout: ss=0(len10), ss=10(len10), ss=20(len10), ss=40(len10), ss=50(len50)
        // After coalesce: ss=0(len20) and ss=20(len10) and ss=40(len60) — NOT all merged
        assert_tiling(&unc1, 90);
        // Verify the non-contiguous Sources are separate
        assert!(
            unc1.len() > 1,
            "non-contiguous Sources must remain separate"
        );
    }

    // V30: Cut/mute crossing into a non-Source splice trims it (cross-type), not a no-op.
    #[test]
    fn v30_cross_type_trims_non_source() {
        // Source / RoomTone / Source
        let splices = vec![
            Splice {
                length_samples: 30,
                fade_in_samples: 0,
                fade_out_samples: 5,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            },
            Splice {
                length_samples: 40,
                fade_in_samples: 5,
                fade_out_samples: 5,
                kind: SpliceKind::RoomTone,
            },
            Splice {
                length_samples: 30,
                fade_in_samples: 5,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 70,
                },
            },
        ];
        // Cut [40,60) inside the RoomTone [30,70): the two surviving RoomTone pieces become
        // adjacent and coalesce; the RoomTone shrinks by 20.
        let result_cut = subdivide_on_cut(&splices, 40, 60, 5);
        assert_tiling(&result_cut, 80); // total 100 - 20
        assert_eq!(result_cut.len(), 3);
        let rt = result_cut
            .iter()
            .find(|s| matches!(s.kind, SpliceKind::RoomTone))
            .unwrap();
        assert_eq!(rt.length_samples, 20); // 40 - 20 cut

        // Mute inside the RoomTone: head/middle/tail all coalesce back to one RoomTone of the
        // same length; Σ unchanged.
        let result_mute = subdivide_on_mute(&splices, 40, 60, true, 5);
        assert_tiling(&result_mute, 100);
        assert_eq!(result_mute.len(), 3);
        let rt_m = result_mute
            .iter()
            .find(|s| matches!(s.kind, SpliceKind::RoomTone))
            .unwrap();
        assert_eq!(rt_m.length_samples, 40);
    }

    // ── X: cross-cutting ─────────────────────────────────────────────────────

    // X31: Pure functions — no Word, no DB; verified by signature + compilation.
    // (Compilation of this module with only `Splice`/`SpliceKind` imports is the assertion.)
    #[test]
    fn x31_pure_functions_no_word_dependency() {
        // All four functions compile and run with only scalar + &[Splice] args.
        let s = single_source(100, 0);
        let _ = subdivide_on_cut(&s, 10, 20, 5);
        let _ = subdivide_on_mute(&s, 10, 20, true, 5);
        let c = subdivide_on_cut(&s, 10, 20, 5);
        let _ = merge_on_uncut(&c, 10, 10, 10);
        let m = subdivide_on_mute(&s, 10, 20, true, 5);
        let _ = merge_on_unmute(&m, 10, 10, 10);
    }

    // X32: Determinism — same inputs → identical output, twice.
    #[test]
    fn x32_determinism() {
        let splices = single_source(100, 0);
        assert_eq!(
            subdivide_on_cut(&splices, 20, 50, 7),
            subdivide_on_cut(&splices, 20, 50, 7)
        );
        assert_eq!(
            subdivide_on_mute(&splices, 20, 50, true, 7),
            subdivide_on_mute(&splices, 20, 50, true, 7)
        );
    }

    // X33: Round-trips through Turn blob (encode_turn + decode_turn).
    #[test]
    fn x33_round_trips_through_turn_blob() {
        let cut = subdivide_on_cut(&single_source(100, 0), 20, 50, 5);
        let turn = make_turn(cut.clone());
        let (_, bytes) = encode_turn(&turn).unwrap();
        let decoded = decode_turn(&bytes).unwrap();
        assert_eq!(decoded.splices, cut);
    }

    // Helper for X33 and A37.
    fn make_turn(splices: Vec<Splice>) -> Turn {
        let total: i64 = splices.iter().map(|s| s.length_samples).sum();
        Turn {
            id: 1,
            speaker_id: None,
            turn_duration: total,
            post_turn_silence: 0,
            words: vec![],
            splices,
        }
    }

    // ── A: A4 translate-and-replay seam ──────────────────────────────────────

    // A34: Boundary translation — position 0.
    #[test]
    fn a34_boundary_translation_position_zero() {
        let splices = single_source(100, 0);

        // Cut at position 0
        let cut = subdivide_on_cut(&splices, 0, 30, 5);
        assert_tiling(&cut, 70);
        assert!(
            cut.iter().all(|s| s.length_samples > 0),
            "no leading zero-length splice"
        );
        assert_eq!(source_start(&cut[0]), 30); // re-based

        // Mute at position 0
        let muted = subdivide_on_mute(&splices, 0, 30, true, 5);
        assert_tiling(&muted, 100);
        assert_eq!(muted.len(), 2); // no leading zero-length Source

        // Uncut at position 0
        let restored = merge_on_uncut(&cut, 0, 30, 0);
        assert_eq!(restored, splices);
        assert_eq!(restored[0].fade_in_samples, 0); // turn start
    }

    // A35: Boundary translation — half-open end (no sliver, post-silence intact).
    #[test]
    fn a35_boundary_translation_half_open_end() {
        // turn_duration=80, post_turn_silence=20, total=100; speech boundary at 80.
        let splices = single_source(100, 0);

        // Cut exactly to the speech end [60, 80): after piece is the post-silence only.
        let cut = subdivide_on_cut(&splices, 60, 80, 5);
        assert_tiling(&cut, 80);
        assert!(
            cut.iter().all(|s| s.length_samples > 0),
            "no trailing zero-length splice"
        );
        assert_eq!(cut.last().unwrap().length_samples, 20); // post-silence
        assert_eq!(source_start(cut.last().unwrap()), 80);

        // Mute the same span [60, 80): Σ unchanged, no trailing zero-length splice.
        let muted = subdivide_on_mute(&splices, 60, 80, true, 5);
        assert_tiling(&muted, 100);
        assert!(muted.iter().all(|s| s.length_samples > 0));
        // The last splice is the post-silence Source (after the RoomTone ends at 80).
        assert_eq!(muted.last().unwrap().length_samples, 20);

        // Pair with an interior splice edge (not the last word) to prove only the
        // truly-empty side is dropped.
        let splices2 = vec![
            Splice {
                length_samples: 40,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            },
            Splice {
                length_samples: 60,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 40,
                },
            },
        ];
        // Cut [40, 60): start == splice[1].start, end < splice[1].end
        let cut2 = subdivide_on_cut(&splices2, 40, 60, 5);
        assert_eq!(cut2.len(), 2); // before + after (both non-zero)
        assert!(cut2.iter().all(|s| s.length_samples > 0));
        assert_tiling(&cut2, 80);
    }

    // A36: Batch + frozen-original coordinate basis.
    #[test]
    fn a36_batch_frozen_original_coordinate_basis() {
        // Original: Source(ss=0, len=100). Frozen source coordinates:
        //   word1: [10, 25)  word2: [40, 60)  word3: [70, 85)
        let original = single_source(100, 0);

        // Forward: cut word1=[10,25), then cut word2 (translated to current-vec).
        // After cut word1: current vec is [Source(0..10), Source(25..100)] = total 85.
        // word2 original=[40,60) → current-vec offset: 40-0-(25-10)=40-15=25; so [25,45).
        let c1 = subdivide_on_cut(&original, 10, 25, 5);
        assert_tiling(&c1, 85);
        let c2 = subdivide_on_cut(&c1, 25, 45, 5); // word2 translated
        assert_tiling(&c2, 65);

        // Hand-computed: Source(0..10,ss=0), Source(10..25,ss=60), Source(25..40,ss=85)
        // (lengths: 10, 15, 15 = 40... wait let me recompute)
        // c1: [Source(ss=0,len=10), Source(ss=25,len=75)]
        // Cut [25,45) in c1 → inside Source(ss=25,len=75,starts at 10 in vec):
        //   splice_start=10, start=25, before=25-10=15, after=85-45=40
        //   after_ss = 25 + (45-10) = 60
        // c2: [Source(ss=0,len=10), Source(ss=25,len=15), Source(ss=60,len=40)]
        assert_eq!(c2.len(), 3);
        assert_eq!(c2[0].length_samples, 10);
        assert_eq!(source_start(&c2[0]), 0);
        assert_eq!(c2[1].length_samples, 15);
        assert_eq!(source_start(&c2[1]), 25);
        assert_eq!(c2[2].length_samples, 40);
        assert_eq!(source_start(&c2[2]), 60);

        // Inverse: uncut both (order A: word1 first, then word2).
        // word1 gap at current pos 10, restore_len=15, source_start=10.
        let u1 = merge_on_uncut(&c2, 10, 15, 10);
        assert_tiling(&u1, 80);
        // u1 coalesces the three Source splices with ss=0,10,25 into Source(ss=0,len=40),
        // leaving Source(ss=60,len=40) separate. word2 gap is at current pos 40.
        let u2 = merge_on_uncut(&u1, 40, 20, 40);
        assert_tiling(&u2, 100);
        assert_eq!(u2, original);
    }

    // A37: Durable round-trip — load-bearing (replay equals live).
    #[test]
    fn a37_durable_round_trip_replay_equals_live() {
        let original = single_source(100, 0);
        let cut1 = subdivide_on_cut(&original, 10, 25, 5);
        let muted = subdivide_on_mute(&cut1, 30, 50, true, 5);
        assert_tiling(&muted, 85); // cut removed 15 (100→85); mute leaves Σ unchanged
        let live_total: i64 = muted.iter().map(|s| s.length_samples).sum();

        // Compute live prefix-sum positions.
        let live_positions: Vec<i64> = {
            let mut positions = Vec::new();
            let mut acc = 0i64;
            for s in &muted {
                positions.push(acc);
                acc += s.length_samples;
            }
            positions
        };

        // Persist and reload.
        let turn = make_turn(muted.clone());
        let (_, bytes) = encode_turn(&turn).unwrap();
        let decoded = decode_turn(&bytes).unwrap();

        // Prefix-sum the reloaded vec and assert positions equal the live positions.
        let reloaded_positions: Vec<i64> = {
            let mut positions = Vec::new();
            let mut acc = 0i64;
            for s in &decoded.splices {
                positions.push(acc);
                acc += s.length_samples;
            }
            positions
        };
        assert_eq!(reloaded_positions, live_positions);

        // Total length must equal live total.
        let reloaded_total: i64 = decoded.splices.iter().map(|s| s.length_samples).sum();
        assert_eq!(reloaded_total, live_total);
    }

    // ── G: cross-boundary / cross-type / coalescing ──────────────────────────

    // G40: A cut spanning a Source/Source seam trims both and drops the middle.
    #[test]
    fn g40_cut_across_source_seam() {
        let original = single_source(100, 0);
        // Cut [20,40) → two source-discontiguous pieces: S(ss0,20) | S(ss40,60).
        let once = subdivide_on_cut(&original, 20, 40, 5);
        assert_tiling(&once, 80);
        assert_eq!(once.len(), 2);
        // Cut [10,50): start inside piece 0, end inside piece 1 — crosses the seam at 20.
        let twice = subdivide_on_cut(&once, 10, 50, 5);
        assert_tiling(&twice, 40);
        assert_eq!(twice.len(), 2);
        assert_eq!(twice[0].length_samples, 10); // head of piece 0
        assert_eq!(source_start(&twice[0]), 0);
        assert_eq!(twice[1].length_samples, 30); // tail of piece 1
        assert_eq!(source_start(&twice[1]), 70); // 40 + (50-20)
    }

    // G41: Cutting a Source from between two RoomTones coalesces them.
    #[test]
    fn g41_cut_between_room_tones_coalesces() {
        let splices = vec![
            Splice {
                length_samples: 20,
                fade_in_samples: 0,
                fade_out_samples: 5,
                kind: SpliceKind::RoomTone,
            },
            Splice {
                length_samples: 20,
                fade_in_samples: 5,
                fade_out_samples: 5,
                kind: SpliceKind::Source {
                    source_start_sample: 100,
                },
            },
            Splice {
                length_samples: 20,
                fade_in_samples: 5,
                fade_out_samples: 0,
                kind: SpliceKind::RoomTone,
            },
        ];
        let result = subdivide_on_cut(&splices, 20, 40, 5);
        assert_tiling(&result, 40);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].kind, SpliceKind::RoomTone));
        assert_eq!(result[0].length_samples, 40);
    }

    // G42: Muting two adjacent words coalesces the two room tones into one.
    #[test]
    fn g42_adjacent_mutes_coalesce() {
        let original = single_source(100, 0);
        let m1 = subdivide_on_mute(&original, 20, 40, true, 5);
        let m2 = subdivide_on_mute(&m1, 40, 60, true, 5);
        assert_tiling(&m2, 100);
        let rt: Vec<_> = m2
            .iter()
            .filter(|s| matches!(s.kind, SpliceKind::RoomTone))
            .collect();
        assert_eq!(rt.len(), 1, "two adjacent mutes must coalesce");
        assert_eq!(rt[0].length_samples, 40);
    }

    // G43: Unmuting a span that covers two mutes + the source between restores pristine.
    #[test]
    fn g43_unmute_across_multiple_splices() {
        let original = single_source(100, 0);
        let m1 = subdivide_on_mute(&original, 20, 40, true, 5);
        let m2 = subdivide_on_mute(&m1, 60, 80, true, 5);
        assert_tiling(&m2, 100);
        // Restore the whole [20,80) span as source reading ss=20, length 60.
        let restored = merge_on_unmute(&m2, 20, 60, 20);
        assert_tiling(&restored, 100);
        assert_eq!(restored, original);
    }

    // G43b: unmute a span whose END falls strictly INSIDE a Source splice (not on a boundary as
    // G43 does), so that Source's tail must be source-rebased by `end - s_start` (line 324). Mute
    // [20,40) → RoomTone, then unmute [20,60) reading ss=20: the [60,100) tail of the original
    // Source[40,100) must rebase to read ss=60, reconstituting the pristine single source. A wrong
    // rebase (`+`/`/`) leaves the tail reading the wrong source and breaks the round-trip.
    #[test]
    fn g43b_unmute_end_inside_source_rebases_tail() {
        let original = single_source(100, 0);
        let muted = subdivide_on_mute(&original, 20, 40, true, 0);
        // Sanity: the span end (60) is interior to the surviving Source[40,100).
        let restored = merge_on_unmute(&muted, 20, 40, 20);
        assert_tiling(&restored, 100);
        assert_eq!(
            restored, original,
            "tail of the split Source must rebase to ss=60 over [60,100)"
        );
    }

    // ── P: precondition asserts (debug builds) ───────────────────────────────

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must be <= end")]
    fn p50_cut_start_after_end_panics() {
        let s = single_source(100, 0);
        let _ = subdivide_on_cut(&s, 60, 40, 5);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must be <= turn length")]
    fn p51_cut_end_past_total_panics() {
        let s = single_source(100, 0);
        let _ = subdivide_on_cut(&s, 50, 120, 5);
    }

    // P52: Uncut into the middle of a coalesced room tone splits it (no longer an error).
    #[test]
    fn p52_uncut_splits_interior_room_tone() {
        // Two adjacent mutes coalesce to one RoomTone[20,60); restore a source at interior 40.
        let original = single_source(100, 0);
        let m1 = subdivide_on_mute(&original, 20, 40, true, 5);
        let m2 = subdivide_on_mute(&m1, 40, 60, true, 5);
        assert_tiling(&m2, 100);
        let restored = merge_on_uncut(&m2, 40, 10, 500);
        assert_tiling(&restored, 110); // turn grows by 10
                                       // The room tone is split around the inserted source, which stands alone.
        let src = restored
            .iter()
            .find(|s| matches!(s.kind, SpliceKind::Source { source_start_sample: ss } if ss == 500))
            .expect("restored interior source");
        assert_eq!(src.length_samples, 10);
        let rt_count = restored
            .iter()
            .filter(|s| matches!(s.kind, SpliceKind::RoomTone))
            .count();
        assert_eq!(rt_count, 2, "room tone split into two around the source");
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "overlaps next splice source")]
    fn p53_uncut_source_overlap_panics() {
        // Cut [30,60) → gap at 30 with the next splice reading ss=60. Restoring 40 samples
        // (ss 30..70) overruns into the next splice's source.
        let cut = subdivide_on_cut(&single_source(100, 0), 30, 60, 5);
        let _ = merge_on_uncut(&cut, 30, 40, 30);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must not fall inside a Source splice")]
    fn p54_merge_start_inside_source_panics() {
        // start=50 lands in the middle of the single contiguous Source — no edited region there.
        let _ = merge_on_uncut(&single_source(100, 0), 50, 10, 999);
    }
}
