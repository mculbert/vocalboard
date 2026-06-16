//! EDL cursor: per-track walk and multi-track merge into mix slices.
//!
//! See `design/audio-pipeline.md` § Building the playback / export EDL for the
//! data-model split (`EdlSegment` is **vertical** — one track's contribution;
//! `MixSlice` is **horizontal** — one sample-aligned span merged from all tracks).
//! No SQLite, no PCM reading here; that belongs to the renderer (`render.rs`).

use std::sync::Arc;

use crate::project::tree::{ImplicitTimelineTree, OwnedElementRef, OwnedTreeIter};
use crate::project::turn::{Splice, SpliceKind, Turn};

// --- find_splice_at helper ---

/// Walk `splices` to find which splice contains `offset` (turn-local prefix-sum
/// position). Returns `(splice_index, in_splice_offset)`.
///
/// If `offset` equals the total length of all splices (i.e. points past the
/// last splice), returns `(splices.len(), 0)`.
fn find_splice_at(splices: &[Splice], offset: i64) -> (usize, i64) {
    let mut prefix = 0i64;
    for (i, splice) in splices.iter().enumerate() {
        if prefix + splice.length_samples > offset {
            return (i, offset - prefix);
        }
        prefix += splice.length_samples;
    }
    (splices.len(), 0)
}

// --- public types ---

/// Vertical: one track's contribution over a span — a window into one pristine splice.
///
/// Carries no project position or length (those belong to the enclosing [`MixSlice`]).
/// The renderer reads [`MixSlice::length_samples`] frames starting at in-splice offset
/// [`offset_in_splice`], applying the splice's fades anchored to the *original* splice
/// edges — so a span beginning mid-fade resumes the ramp rather than restarting it.
///
/// [`offset_in_splice`]: Self::offset_in_splice
#[derive(Clone, Debug, PartialEq)]
pub struct EdlSegment {
    /// Track this segment belongs to.
    pub track_id: u32,
    /// Pristine splice: original length, fades, and `source_start_sample` — unmodified.
    pub splice: Splice,
    /// Source-read offset + fade phase within the splice (`0` = splice head).
    pub offset_in_splice: i64,
}

/// Horizontal: one sample-aligned span of the merged project timeline.
///
/// All [`segments`](Self::segments) cover exactly
/// `[start_sample, start_sample + length_samples)`; an absent track contributes no
/// segment (its samples are zero in the mix).
#[derive(Clone, Debug, PartialEq)]
pub struct MixSlice {
    /// Absolute project sample at which this span begins.
    pub start_sample: i64,
    /// Length of this span in project-rate samples.
    pub length_samples: i64,
    /// One entry per active track, ascending by `track_id`.
    pub segments: Vec<EdlSegment>,
}

/// Per-track engine: seekable, pull-based walk over one track's timeline tree.
///
/// Yields full-splice windows in timeline order — a synthetic lead-in `Silence`
/// first (when the track's project start is later than the requested `start`),
/// then one [`EdlSegment`] per splice from the tree, with `offset_in_splice`
/// set for the seek-clipped first real segment. Holds no DB connection; walks
/// the in-RAM `Arc` tree via owned [`OwnedTreeIter`] clones, making this struct
/// `'static + Send` (suitable for moving into a spawned pre-roll thread).
pub struct TrackCursor {
    track_id: u32,
    tree_iter: OwnedTreeIter<Turn>,
    current_turn: Option<Arc<Turn>>,
    splice_idx: usize,
    /// Offset within the current splice to yield next; 0 for all but the first real segment.
    pending_offset: i64,
    /// Remaining lead-in length; > 0 = lead-in not yet emitted.
    lead_in_len: i64,
}

impl TrackCursor {
    /// Position a per-track cursor at project sample `start` over one track.
    ///
    /// `project_start_sample` offsets the track on the project timeline; a lead-in
    /// silence segment is synthesized when `project_start_sample > start`.  The `end`
    /// bound is owned by [`EdlCursor`], not the track.
    ///
    /// Borrows `tree` only during construction; the returned cursor owns its traversal
    /// state via `Arc` clones and is `'static + Send`.
    pub fn at(
        tree: &ImplicitTimelineTree<Turn>,
        track_id: u32,
        project_start_sample: i64,
        start: i64,
    ) -> Self {
        let lead_in_len = if project_start_sample > start {
            project_start_sample - start
        } else {
            0
        };

        // Track-local seek position (0 when the track starts after `start`).
        let local_seek = (start - project_start_sample).max(0);
        let mut tree_iter = tree.owned_iter_from(local_seek);

        // Prime the first turn.
        let (current_turn, splice_idx, pending_offset) = if let Some(OwnedElementRef {
            start_sample,
            element: turn,
            ..
        }) = tree_iter.next()
        {
            let in_turn_offset = local_seek - start_sample;
            let (idx, off) = find_splice_at(&turn.splices, in_turn_offset);
            (Some(turn), idx, off)
        } else {
            (None, 0, 0)
        };

        TrackCursor {
            track_id,
            tree_iter,
            current_turn,
            splice_idx,
            pending_offset,
            lead_in_len,
        }
    }
}

impl Iterator for TrackCursor {
    type Item = EdlSegment;

    fn next(&mut self) -> Option<Self::Item> {
        // Lead-in silence precedes any real content.
        if self.lead_in_len > 0 {
            let seg = EdlSegment {
                track_id: self.track_id,
                splice: Splice {
                    length_samples: self.lead_in_len,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                },
                offset_in_splice: 0,
            };
            self.lead_in_len = 0;
            return Some(seg);
        }

        loop {
            if let Some(turn_ref) = &self.current_turn {
                let splices = &turn_ref.splices;
                if self.splice_idx < splices.len() {
                    let splice = splices[self.splice_idx].clone();
                    let offset = self.pending_offset;
                    self.pending_offset = 0;
                    self.splice_idx += 1;
                    return Some(EdlSegment {
                        track_id: self.track_id,
                        splice,
                        offset_in_splice: offset,
                    });
                }
            }
            // Current turn exhausted (or not yet started); advance to the next.
            match self.tree_iter.next() {
                Some(OwnedElementRef { element: turn, .. }) => {
                    self.current_turn = Some(turn);
                    self.splice_idx = 0;
                    self.pending_offset = 0;
                }
                None => return None,
            }
        }
    }
}

/// Merges a set of per-track cursors into one position-ordered stream of mix slices.
///
/// Drives all [`TrackCursor`]s in lockstep: each `next()` emits the shortest
/// span that ends on a boundary of at least one track, advancing all tracks by
/// that length. No PCM is read; the result is a sequence of descriptors for
/// the [`Renderer`](super::render::Renderer). `'static + Send` (inherits from [`TrackCursor`]).
pub struct EdlCursor {
    /// Active tracks: cursor + its buffered current segment.
    /// Invariant: every entry has a live segment (exhausted cursors are removed).
    tracks: Vec<(TrackCursor, EdlSegment)>,
    /// Current merged project position.
    pos: i64,
    /// Exclusive end bound (`None` = walk to the last track's end).
    end: Option<i64>,
}

impl EdlCursor {
    /// Merge `tracks` over `[start, end)` (`end == None` walks to the last track's end).
    pub fn new(tracks: Vec<TrackCursor>, start: i64, end: Option<i64>) -> Self {
        let active: Vec<_> = tracks
            .into_iter()
            .filter_map(|mut cursor| {
                let seg = cursor.next()?;
                Some((cursor, seg))
            })
            .collect();
        EdlCursor {
            tracks: active,
            pos: start,
            end,
        }
    }

    /// Build a merged cursor over `tracks` for `[start, end)`.
    ///
    /// Each entry is `(track_id, project_start_sample, &tree)`. `end == None` walks to the last
    /// track's content end (the project end, since no content exists past the longest track); an
    /// explicit `end` past that emits trailing silence (see [`EdlCursor::next`]). Used by both
    /// playback and export so they assemble — and therefore render — identically.
    pub fn build(
        tracks: &[(u32, i64, &ImplicitTimelineTree<Turn>)],
        start: i64,
        end: Option<i64>,
    ) -> Self {
        let cursors: Vec<TrackCursor> = tracks
            .iter()
            .map(|(id, project_start_sample, tree)| {
                TrackCursor::at(tree, *id, *project_start_sample, start)
            })
            .collect();
        EdlCursor::new(cursors, start, end)
    }

    /// The exclusive end bound this cursor was built with (`None` = walks to the last track's
    /// end). The renderer uses it to report its natural-stop position.
    pub fn end(&self) -> Option<i64> {
        self.end
    }
}

impl Iterator for EdlCursor {
    type Item = MixSlice;

    fn next(&mut self) -> Option<Self::Item> {
        let end_pos = self.end.unwrap_or(i64::MAX);
        if self.pos >= end_pos {
            return None;
        }
        if self.tracks.is_empty() {
            // Tracks exhausted. With an explicit `end` past the last track's content, emit a
            // trailing silence span filling `[pos, end)` so a single short track (or a project
            // whose longest track ends before `end`) renders to the prescribed length — the
            // empty segment list makes the renderer zero the region. With `end == None` there
            // is no content past the last track, so the walk simply ends.
            return match self.end {
                Some(e) if self.pos < e => {
                    let slice = MixSlice {
                        start_sample: self.pos,
                        length_samples: e - self.pos,
                        segments: Vec::new(),
                    };
                    self.pos = e;
                    Some(slice)
                }
                _ => None,
            };
        }

        // Minimum run-length across all active tracks.
        let run_min = self
            .tracks
            .iter()
            .map(|(_, seg)| seg.splice.length_samples - seg.offset_in_splice)
            .min()
            .unwrap_or(0); // safe: tracks is non-empty (checked above)

        let len = match self.end {
            Some(e) => run_min.min(e - self.pos),
            None => run_min,
        };

        if len <= 0 {
            return None;
        }

        // Collect and sort segments for the slice.
        let mut segments: Vec<EdlSegment> =
            self.tracks.iter().map(|(_, seg)| seg.clone()).collect();
        segments.sort_unstable_by_key(|s| s.track_id);

        let slice = MixSlice {
            start_sample: self.pos,
            length_samples: len,
            segments,
        };

        // Advance each track by `len`; remove exhausted ones.
        let mut i = 0;
        while i < self.tracks.len() {
            self.tracks[i].1.offset_in_splice += len;
            if self.tracks[i].1.offset_in_splice >= self.tracks[i].1.splice.length_samples {
                match self.tracks[i].0.next() {
                    Some(next_seg) => {
                        self.tracks[i].1 = next_seg;
                        i += 1;
                    }
                    None => {
                        // swap_remove is fine: segment order within tracks is arbitrary
                        // (segments are re-sorted by track_id on each slice).
                        self.tracks.swap_remove(i);
                    }
                }
            } else {
                i += 1;
            }
        }

        self.pos += len;
        Some(slice)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::project::hash::Hash;
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{decode_turn, encode_turn, Splice, SpliceKind, Turn};

    // Per-slice snapshots used to compare merged streams (start, length, per-segment fields).
    type SliceShape = (i64, i64, Vec<(u32, i64, i64)>);
    type SliceCapture = (i64, i64, Vec<(u32, SpliceKind, i64, i64, i64)>);

    // --- helpers ---

    fn src(len: i64, start: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: 0,
            fade_out_samples: 0,
            kind: SpliceKind::Source {
                source_start_sample: start,
            },
        }
    }

    fn faded_src(len: i64, start: i64, fi: i64, fo: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: fi,
            fade_out_samples: fo,
            kind: SpliceKind::Source {
                source_start_sample: start,
            },
        }
    }

    fn rt(len: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: 0,
            fade_out_samples: 0,
            kind: SpliceKind::RoomTone,
        }
    }

    fn sil(len: i64) -> Splice {
        Splice {
            length_samples: len,
            fade_in_samples: 0,
            fade_out_samples: 0,
            kind: SpliceKind::Silence,
        }
    }

    fn make_turn(id: u64, splices: Vec<Splice>) -> (Hash, Arc<Turn>) {
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

    // --- S6 ---

    #[test]
    fn s6_one_turn_one_source_splice() {
        let tree = build_tree(vec![make_turn(1, vec![src(100, 500)])]);
        let mut cursor = TrackCursor::at(&tree, 42, 0, 0);
        let seg = cursor.next().unwrap();
        assert_eq!(seg.track_id, 42);
        assert_eq!(seg.splice.length_samples, 100);
        assert_eq!(
            seg.splice.kind,
            SpliceKind::Source {
                source_start_sample: 500
            }
        );
        assert_eq!(seg.offset_in_splice, 0);
        assert!(cursor.next().is_none());
    }

    // --- S7 ---

    #[test]
    fn s7_multi_splice_turn() {
        let tree = build_tree(vec![make_turn(1, vec![src(40, 0), rt(20), src(30, 60)])]);
        let segs: Vec<_> = TrackCursor::at(&tree, 1, 0, 0).collect();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].splice.length_samples, 40);
        assert_eq!(
            segs[0].splice.kind,
            SpliceKind::Source {
                source_start_sample: 0
            }
        );
        assert_eq!(segs[0].offset_in_splice, 0);
        assert_eq!(segs[1].splice.kind, SpliceKind::RoomTone);
        assert_eq!(segs[1].offset_in_splice, 0);
        assert_eq!(
            segs[2].splice.kind,
            SpliceKind::Source {
                source_start_sample: 60
            }
        );
        assert_eq!(segs[2].offset_in_splice, 0);
    }

    // --- S8 ---

    #[test]
    fn s8_two_turns_in_order() {
        let tree = build_tree(vec![
            make_turn(1, vec![src(50, 0)]),
            make_turn(2, vec![src(80, 1000)]),
        ]);
        let segs: Vec<_> = TrackCursor::at(&tree, 1, 0, 0).collect();
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0].splice.kind,
            SpliceKind::Source {
                source_start_sample: 0
            }
        );
        assert_eq!(
            segs[1].splice.kind,
            SpliceKind::Source {
                source_start_sample: 1000
            }
        );
    }

    // --- S9 ---

    #[test]
    fn s9_post_turn_silence_no_extra_gap() {
        // A turn with a trailing Silence splice (representing post_turn_silence).
        let tree = build_tree(vec![
            make_turn(1, vec![src(60, 0), sil(20)]),
            make_turn(2, vec![src(40, 200)]),
        ]);
        let segs: Vec<_> = TrackCursor::at(&tree, 1, 0, 0).collect();
        // Expect exactly 3 segments: src, sil (from turn 1's splices), src (turn 2)
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[1].splice.kind, SpliceKind::Silence);
        assert_eq!(segs[1].splice.length_samples, 20);
        // No extra silence injected between turn 1 and turn 2.
    }

    // --- S10 ---

    #[test]
    fn s10_fades_carried_through() {
        let tree = build_tree(vec![make_turn(1, vec![faded_src(100, 0, 10, 5)])]);
        let segs: Vec<_> = TrackCursor::at(&tree, 1, 0, 0).collect();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].splice.fade_in_samples, 10);
        assert_eq!(segs[0].splice.fade_out_samples, 5);
    }

    // --- S11 ---

    #[test]
    fn s11_walk_covers_whole_track() {
        let tree = build_tree(vec![
            make_turn(1, vec![src(50, 0), rt(10)]),
            make_turn(2, vec![sil(20), src(30, 100)]),
            make_turn(3, vec![src(15, 200)]),
        ]);
        let total_dur = tree.total_duration();
        let splice_sum: i64 = TrackCursor::at(&tree, 1, 0, 0)
            .map(|s| s.splice.length_samples)
            .sum();
        assert_eq!(splice_sum, total_dur);
    }

    // --- K12 ---

    #[test]
    fn k12_seek_to_mid_splice() {
        // Turn with one 100-sample splice. Seek to sample 30 (inside it).
        let tree = build_tree(vec![make_turn(1, vec![faded_src(100, 500, 10, 10)])]);
        let mut cursor = TrackCursor::at(&tree, 1, 0, 30);
        let seg = cursor.next().unwrap();
        // Splice must be pristine.
        assert_eq!(seg.splice.length_samples, 100);
        assert_eq!(
            seg.splice.kind,
            SpliceKind::Source {
                source_start_sample: 500
            }
        );
        assert_eq!(seg.splice.fade_in_samples, 10);
        assert_eq!(seg.offset_in_splice, 30);
        assert!(cursor.next().is_none());
    }

    // --- K13 ---

    #[test]
    fn k13_seek_to_exact_turn_boundary() {
        let tree = build_tree(vec![
            make_turn(1, vec![src(60, 0)]),
            make_turn(2, vec![src(40, 100)]),
        ]);
        // start = 60 = exact start of turn 2
        let mut cursor = TrackCursor::at(&tree, 1, 0, 60);
        let seg = cursor.next().unwrap();
        assert_eq!(
            seg.splice.kind,
            SpliceKind::Source {
                source_start_sample: 100
            }
        );
        assert_eq!(seg.offset_in_splice, 0);
        assert!(cursor.next().is_none());
    }

    // --- K14 ---

    #[test]
    fn k14_start_past_track_end() {
        let tree = build_tree(vec![make_turn(1, vec![src(50, 0)])]);
        // project_start_sample = 0; start = 50 = exactly at end → no segments
        assert!(TrackCursor::at(&tree, 1, 0, 50).next().is_none());
        // start > total_duration
        assert!(TrackCursor::at(&tree, 1, 0, 200).next().is_none());
    }

    // --- F15 ---

    #[test]
    fn f15_seek_into_fade_in_carries_pristine_fade() {
        // splice with fade_in_samples=20; seek to offset 7 (inside fade ramp)
        let tree = build_tree(vec![make_turn(1, vec![faded_src(50, 0, 20, 0)])]);
        let mut cursor = TrackCursor::at(&tree, 1, 0, 7);
        let seg = cursor.next().unwrap();
        // Pristine fade must survive: the renderer anchors the seam crossfade at
        // equal_power_gain(7 + i, 20).
        assert_eq!(seg.splice.fade_in_samples, 20);
        assert_eq!(seg.offset_in_splice, 7);
        // At i=0 the fade-in is the partial gain equal_power_gain(7, 20) ≈ 0.547, not 0.0.
        let partial = crate::audio::equal_power_gain(7, 20);
        let from_start = crate::audio::equal_power_gain(0, 20);
        assert!(partial > from_start);
    }

    // --- G17 ---

    #[test]
    fn g17_lead_in_silence() {
        let tree = build_tree(vec![make_turn(1, vec![src(80, 0)])]);
        // track starts at sample 30; cursor starts at sample 0 → lead-in of 30
        let mut cursor = TrackCursor::at(&tree, 7, 30, 0);
        let lead = cursor.next().unwrap();
        assert_eq!(lead.splice.kind, SpliceKind::Silence);
        assert_eq!(lead.splice.length_samples, 30);
        assert_eq!(lead.offset_in_splice, 0);
        assert_eq!(lead.track_id, 7);
        // Next segment is the real splice.
        let real = cursor.next().unwrap();
        assert_eq!(
            real.splice.kind,
            SpliceKind::Source {
                source_start_sample: 0
            }
        );
        assert_eq!(real.offset_in_splice, 0);
        assert!(cursor.next().is_none());
    }

    // --- G18 (contiguous track has no extra gaps) already covered by S9 ---

    // --- G19 ---

    #[test]
    fn g19_lead_in_with_nonzero_start() {
        // project_start_sample (50) > start (20) > 0: the lead-in silence must span the
        // *difference* [20, 50) = 30 samples. With start == 0 (g17/m22/m26) a `+`/`-`
        // swap on `project_start_sample - start` is invisible; a non-zero start pins the
        // subtraction.
        let tree = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        let mut cursor = TrackCursor::at(&tree, 9, 50, 20);
        let lead = cursor.next().unwrap();
        assert_eq!(lead.splice.kind, SpliceKind::Silence);
        assert_eq!(lead.splice.length_samples, 30); // 50 - 20, never 50 + 20
                                                    // Real content follows immediately, read from its head.
        let real = cursor.next().unwrap();
        assert_eq!(
            real.splice.kind,
            SpliceKind::Source {
                source_start_sample: 0
            }
        );
        assert_eq!(real.offset_in_splice, 0);
    }

    // --- M19 ---

    #[test]
    fn m19_single_track_full_walk() {
        let tree = build_tree(vec![
            make_turn(1, vec![src(30, 0)]),
            make_turn(2, vec![src(20, 100), rt(10)]),
        ]);
        let cursor = TrackCursor::at(&tree, 1, 0, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![cursor], 0, None).collect();

        // Each slice must have exactly one segment.
        for sl in &slices {
            assert_eq!(sl.segments.len(), 1);
        }

        // start_sample sequence is contiguous.
        let mut pos = 0i64;
        for sl in &slices {
            assert_eq!(sl.start_sample, pos);
            pos += sl.length_samples;
        }

        // Total length == tree duration.
        let total: i64 = slices.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total, tree.total_duration());
    }

    // --- M20 ---

    #[test]
    fn m20_seek_stamps_start_sample_correctly() {
        let tree = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        // Seek to project sample 35 (inside the single 100-sample splice).
        let cursor = TrackCursor::at(&tree, 1, 0, 35);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![cursor], 35, None).collect();
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].start_sample, 35);
        assert_eq!(slices[0].length_samples, 65); // 100 - 35
        assert_eq!(slices[0].segments[0].offset_in_splice, 35);
    }

    // --- M21 ---

    #[test]
    fn m21_bounded_walk_and_zero_range() {
        let tree = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        let cursor = || TrackCursor::at(&tree, 1, 0, 0);

        // end inside the single splice
        let slices: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(60)).collect();
        let last = slices.last().unwrap();
        assert_eq!(last.start_sample + last.length_samples, 60);
        assert!(slices
            .iter()
            .all(|sl| sl.start_sample + sl.length_samples <= 60));

        // zero-length [s, s)
        let empty: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 50, Some(50)).collect();
        assert!(empty.is_empty());

        // end == None walks to track's end
        let full: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, None).collect();
        let total: i64 = full.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total, 100);
    }

    // --- M22 ---

    #[test]
    fn m22_two_non_overlapping_tracks() {
        // Track 1: [0, 60); Track 2: project_start=60, content [0, 40) of track-local
        let t1 = build_tree(vec![make_turn(1, vec![src(60, 0)])]);
        let t2 = build_tree(vec![make_turn(2, vec![src(40, 100)])]);

        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 60, 0); // project_start_sample=60, start=0

        let slices: Vec<MixSlice> = EdlCursor::new(vec![c1, c2], 0, None).collect();

        // All slices must be contiguous.
        let mut pos = 0i64;
        for sl in &slices {
            assert_eq!(sl.start_sample, pos, "gap at {pos}");
            pos += sl.length_samples;
        }
        assert_eq!(pos, 100); // total = 60 + 40

        // First region [0,60): 1 segment from track 1 (track 2 is in lead-in silence)
        let first = slices.iter().find(|sl| sl.start_sample == 0).unwrap();
        // track 2's lead-in contributes a silence segment in the overlap region
        let t2_segs: Vec<_> = first.segments.iter().filter(|s| s.track_id == 2).collect();
        assert_eq!(t2_segs[0].splice.kind, SpliceKind::Silence);
    }

    // --- M23 ---

    #[test]
    fn m23_overlapping_tracks_share_slice() {
        // Both tracks have content from 0..100, but with different internal boundaries.
        // Track 1: one 100-sample splice.
        // Track 2: two splices: 40 + 60.
        let t1 = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        let t2 = build_tree(vec![make_turn(2, vec![src(40, 200), src(60, 250)])]);

        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![c1, c2], 0, None).collect();

        // Every slice has 2 segments.
        for sl in &slices {
            assert_eq!(
                sl.segments.len(),
                2,
                "expected 2 segs at {}",
                sl.start_sample
            );
        }

        // Spans tile [0, 100) gaplessly.
        let mut pos = 0i64;
        for sl in &slices {
            assert_eq!(sl.start_sample, pos);
            pos += sl.length_samples;
        }
        assert_eq!(pos, 100);
    }

    // --- M24 ---

    #[test]
    fn m24_partial_splice_across_foreign_boundary_and_fade_continuity() {
        // Track 1: one 100-sample fade-in splice (fade_in=50).
        // Track 2: boundary at offset 30 (forces a split in track 1's splice).
        let t1 = build_tree(vec![make_turn(1, vec![faded_src(100, 0, 50, 0)])]);
        let t2 = build_tree(vec![make_turn(2, vec![src(30, 0), src(70, 30)])]);

        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![c1, c2], 0, None).collect();

        // Find the two slices that together cover track 1's single splice.
        let t1_slices: Vec<&MixSlice> = slices
            .iter()
            .filter(|sl| sl.segments.iter().any(|s| s.track_id == 1))
            .collect();

        // First slice: track-1 segment has offset_in_splice=0.
        let first_t1 = t1_slices[0]
            .segments
            .iter()
            .find(|s| s.track_id == 1)
            .unwrap();
        assert_eq!(first_t1.offset_in_splice, 0);
        assert_eq!(first_t1.splice.length_samples, 100); // pristine

        // Second slice: track-1 segment has offset_in_splice=30 (consecutive read).
        let second_t1 = t1_slices[1]
            .segments
            .iter()
            .find(|s| s.track_id == 1)
            .unwrap();
        assert_eq!(second_t1.offset_in_splice, 30);
        assert_eq!(second_t1.splice.length_samples, 100); // still pristine

        // Fade is continuous: gain at the boundary must advance, not restart.
        let gain_before = crate::audio::equal_power_gain(29, 50); // last frame of first slice
        let gain_after = crate::audio::equal_power_gain(30, 50); // first frame of second slice
        assert!(gain_after > gain_before, "fade must continue, not restart");
    }

    // --- M25 ---

    #[test]
    fn m25_segment_order_by_track_id() {
        let t1 = build_tree(vec![make_turn(1, vec![src(50, 0)])]);
        let t2 = build_tree(vec![make_turn(2, vec![src(50, 100)])]);

        // Intentionally pass track 2 before track 1.
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![c2, c1], 0, None).collect();

        for sl in &slices {
            let ids: Vec<u32> = sl.segments.iter().map(|s| s.track_id).collect();
            for w in ids.windows(2) {
                assert!(w[0] < w[1], "segments not ascending by track_id");
            }
        }
    }

    // --- M26 ---

    #[test]
    fn m26_project_start_sample_honoured() {
        let t = build_tree(vec![make_turn(1, vec![src(50, 0)])]);
        // Track offset by 40.
        let cursor = TrackCursor::at(&t, 1, 40, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![cursor], 0, None).collect();

        // Lead-in slice at start_sample=0 with Silence.
        let lead = &slices[0];
        assert_eq!(lead.start_sample, 0);
        assert_eq!(lead.segments[0].splice.kind, SpliceKind::Silence);
        assert_eq!(lead.segments[0].splice.length_samples, 40);

        // Real content starts at 40.
        let content = slices.iter().find(|sl| sl.start_sample == 40).unwrap();
        assert_eq!(
            content.segments[0].splice.kind,
            SpliceKind::Source {
                source_start_sample: 0
            }
        );
    }

    // --- M27 ---

    #[test]
    fn m27_empty_and_short_track_behaviour() {
        // Empty tree contributes nothing.
        let empty_tree = ImplicitTimelineTree::<Turn>::new();
        let empty_cursor = TrackCursor::at(&empty_tree, 1, 0, 0);
        let t2 = build_tree(vec![make_turn(2, vec![src(50, 0)])]);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![empty_cursor, c2], 0, None).collect();
        // All slices from track 2 only.
        for sl in &slices {
            assert!(sl.segments.iter().all(|s| s.track_id == 2));
        }
        let total: i64 = slices.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total, 50);

        // Short track ends early; longer track keeps going.
        let short = build_tree(vec![make_turn(1, vec![src(20, 0)])]);
        let long = build_tree(vec![make_turn(2, vec![src(80, 0)])]);
        let cs = TrackCursor::at(&short, 1, 0, 0);
        let cl = TrackCursor::at(&long, 2, 0, 0);
        let slices2: Vec<MixSlice> = EdlCursor::new(vec![cs, cl], 0, None).collect();
        let total2: i64 = slices2.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total2, 80);
        // After sample 20, only track 2 segments appear.
        for sl in slices2.iter().filter(|sl| sl.start_sample >= 20) {
            assert!(!sl.segments.iter().any(|s| s.track_id == 1));
        }

        // start past every track's end → no slices.
        let t = build_tree(vec![make_turn(1, vec![src(30, 0)])]);
        let c = TrackCursor::at(&t, 1, 0, 50);
        let empty_slices: Vec<MixSlice> = EdlCursor::new(vec![c], 50, None).collect();
        assert!(empty_slices.is_empty());
    }

    // --- X28: no DB connection needed (verified by type signature + runtime test) ---

    #[test]
    fn x28_no_db_connection_while_iterating() {
        // This test drives a full merged walk with only &ImplicitTimelineTree in scope —
        // no Db, no Connection.
        let t1 = build_tree(vec![
            make_turn(1, vec![src(40, 0), rt(10)]),
            make_turn(2, vec![sil(20)]),
        ]);
        let t2 = build_tree(vec![make_turn(3, vec![src(70, 500)])]);
        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let count = EdlCursor::new(vec![c1, c2], 0, None).count();
        assert!(count > 0);
    }

    // --- X29: lazy (cursor allocates O(log n) before first next()) ---

    #[test]
    fn x29_lazy_large_tree() {
        // 10,000-turn tree; taking only the first MixSlice must be fast and not OOM.
        let turns: Vec<_> = (0u64..10_000)
            .map(|i| make_turn(i, vec![src(100, i as i64 * 100)]))
            .collect();
        let tree = build_tree(turns);
        let cursor = TrackCursor::at(&tree, 1, 0, 0);
        let first = EdlCursor::new(vec![cursor], 0, None).next();
        assert!(first.is_some());
    }

    // --- X30 ---

    #[test]
    fn x30_determinism() {
        let tree = build_tree(vec![
            make_turn(1, vec![src(50, 0), rt(10)]),
            make_turn(2, vec![sil(20), src(30, 200)]),
        ]);
        let run = || -> Vec<SliceShape> {
            let cursor = TrackCursor::at(&tree, 1, 0, 0);
            EdlCursor::new(vec![cursor], 0, None)
                .map(|sl| {
                    let segs: Vec<_> = sl
                        .segments
                        .iter()
                        .map(|s| (s.track_id, s.splice.length_samples, s.offset_in_splice))
                        .collect();
                    (sl.start_sample, sl.length_samples, segs)
                })
                .collect()
        };
        assert_eq!(run(), run());
    }

    // --- A31: end == total_duration() includes last sample ---

    #[test]
    fn a31_end_equals_total_duration() {
        let tree = build_tree(vec![
            make_turn(1, vec![src(40, 0)]),
            make_turn(2, vec![src(60, 0)]),
        ]);
        let dur = tree.total_duration(); // 100

        let cursor = || TrackCursor::at(&tree, 1, 0, 0);

        // Bounded with end == total_duration
        let bounded: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(dur)).collect();
        let bounded_total: i64 = bounded.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(bounded_total, dur, "bounded walk must reach last sample");
        let last_bounded = bounded.last().unwrap();
        assert_eq!(last_bounded.start_sample + last_bounded.length_samples, dur);

        // end == None should yield identical stream.
        let unbounded: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, None).collect();
        assert_eq!(
            bounded, unbounded,
            "end==total_duration must equal end==None"
        );
    }

    // --- A32: start == total_duration() yields no slices ---

    #[test]
    fn a32_start_equals_total_duration() {
        let tree = build_tree(vec![make_turn(1, vec![src(50, 0)])]);
        let dur = tree.total_duration();
        let cursor = TrackCursor::at(&tree, 1, 0, dur);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![cursor], dur, None).collect();
        assert!(slices.is_empty());
    }

    // --- A33: end exactly on a splice/turn boundary (trio) ---

    #[test]
    fn a33_end_on_splice_boundary_trio() {
        // Tree: two splices [0,40) and [40,100).
        let tree = build_tree(vec![make_turn(1, vec![src(40, 0), src(60, 40)])]);
        let boundary = 40i64;

        let cursor = || TrackCursor::at(&tree, 1, 0, 0);

        // end == boundary: slice ending at boundary is yielded whole; nothing past it.
        let at_boundary: Vec<MixSlice> =
            EdlCursor::new(vec![cursor()], 0, Some(boundary)).collect();
        let total_at: i64 = at_boundary.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total_at, boundary);
        assert!(!at_boundary.is_empty());
        let last_at = at_boundary.last().unwrap();
        assert_eq!(last_at.start_sample + last_at.length_samples, boundary);

        // end == boundary - 1: final slice clipped one sample shorter.
        let before: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(boundary - 1)).collect();
        let total_before: i64 = before.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total_before, boundary - 1);

        // end == boundary + 1: a 1-sample slice appears past the boundary.
        let after: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(boundary + 1)).collect();
        let total_after: i64 = after.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total_after, boundary + 1);
        // That last slice starts at boundary and has length 1.
        let extra = after.last().unwrap();
        assert_eq!(extra.start_sample, boundary);
        assert_eq!(extra.length_samples, 1);
    }

    // --- A34: durable round-trip (persisted tree yields identical slice stream) ---

    #[test]
    fn a34_durable_round_trip() {
        let turns_raw = [
            (1u64, vec![src(40, 100), rt(10)]),
            (2u64, vec![sil(20), src(30, 200)]),
            (3u64, vec![faded_src(50, 300, 5, 5)]),
        ];

        // Build live tree.
        let live_turns: Vec<_> = turns_raw
            .iter()
            .map(|(id, splices)| make_turn(*id, splices.clone()))
            .collect();
        let live_tree = build_tree(live_turns.clone());

        // Capture live stream.
        fn capture(tree: &ImplicitTimelineTree<Turn>) -> Vec<SliceCapture> {
            let cursor = TrackCursor::at(tree, 1, 0, 0);
            EdlCursor::new(vec![cursor], 0, None)
                .map(|sl| {
                    let segs = sl
                        .segments
                        .iter()
                        .map(|s| {
                            (
                                s.track_id,
                                s.splice.kind,
                                s.splice.length_samples,
                                s.splice.fade_in_samples,
                                s.offset_in_splice,
                            )
                        })
                        .collect();
                    (sl.start_sample, sl.length_samples, segs)
                })
                .collect()
        }

        let live_stream = capture(&live_tree);

        // Persist + reload each turn through encode_turn → decode_turn.
        let reloaded: Vec<_> = live_turns
            .iter()
            .map(|(_, arc_turn)| {
                let (_, bytes) = encode_turn(arc_turn).unwrap();
                let decoded = decode_turn(&bytes).unwrap();
                let (h, _) = encode_turn(&decoded).unwrap();
                (h, Arc::new(decoded))
            })
            .collect();
        let reloaded_tree = build_tree(reloaded);

        let reloaded_stream = capture(&reloaded_tree);

        assert_eq!(
            live_stream, reloaded_stream,
            "stream after store→load round-trip must be byte-identical"
        );
    }

    // --- A35: explicit end past track content emits a trailing silence slice ---

    #[test]
    fn a35_explicit_end_pads_with_silence() {
        let tree = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        let cursor = || TrackCursor::at(&tree, 1, 0, 0);

        // end past content: tiles [0,200); the tail [100,200) is one empty-segments slice.
        let padded: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(200)).collect();
        let total: i64 = padded.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(total, 200, "A35: tiles to explicit end");
        let last = padded.last().unwrap();
        assert_eq!(last.start_sample, 100, "A35: silence starts at content end");
        assert_eq!(last.length_samples, 100, "A35: silence fills to end");
        assert!(
            last.segments.is_empty(),
            "A35: padding slice carries no segments"
        );
        for sl in &padded[..padded.len() - 1] {
            assert!(!sl.segments.is_empty(), "A35: content slices are non-empty");
        }

        // end within content: clipped, no silence slice.
        let clipped: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(80)).collect();
        let ct: i64 = clipped.iter().map(|sl| sl.length_samples).sum();
        assert_eq!(ct, 80, "A35: clipped at end < content");
        assert!(
            clipped.iter().all(|sl| !sl.segments.is_empty()),
            "A35: no padding when end < content"
        );

        // end == content: stops exactly, no zero-length padding slice.
        let exact: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, Some(100)).collect();
        assert_eq!(exact.iter().map(|sl| sl.length_samples).sum::<i64>(), 100);
        assert!(
            exact.iter().all(|sl| !sl.segments.is_empty()),
            "A35: no pad at exact end"
        );

        // end == None: stops at content end (100).
        let unbounded: Vec<MixSlice> = EdlCursor::new(vec![cursor()], 0, None).collect();
        assert_eq!(
            unbounded.iter().map(|sl| sl.length_samples).sum::<i64>(),
            100
        );
    }

    // --- A36: multi-track early exhaustion pads from the longest track's end to the end ---

    #[test]
    fn a36_multi_track_pad_to_end() {
        let t1 = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        let t2 = build_tree(vec![make_turn(2, vec![src(150, 0)])]);
        let c1 = TrackCursor::at(&t1, 1, 0, 0);
        let c2 = TrackCursor::at(&t2, 2, 0, 0);
        let slices: Vec<MixSlice> = EdlCursor::new(vec![c1, c2], 0, Some(200)).collect();
        assert_eq!(slices.iter().map(|sl| sl.length_samples).sum::<i64>(), 200);
        let last = slices.last().unwrap();
        assert_eq!(
            last.start_sample, 150,
            "A36: pad starts at longest-track end"
        );
        assert_eq!(last.length_samples, 50);
        assert!(
            last.segments.is_empty(),
            "A36: padding slice carries no segments"
        );
    }

    // --- A37: end() getter reports the bound the cursor was built with ---

    #[test]
    fn a37_end_getter_reports_bound() {
        let tree = build_tree(vec![make_turn(1, vec![src(100, 0)])]);
        // An explicit bound is reported verbatim (not None, not a stand-in constant).
        let bounded = EdlCursor::new(vec![TrackCursor::at(&tree, 1, 0, 0)], 0, Some(60));
        assert_eq!(bounded.end(), Some(60));
        // Unbounded reports None.
        let unbounded = EdlCursor::new(vec![TrackCursor::at(&tree, 1, 0, 0)], 0, None);
        assert_eq!(unbounded.end(), None);
    }

    // --- F16: slice boundary splitting a fade is seamless (covered by M24) ---

    // --- cross-cutting: find_splice_at boundary cases ---

    #[test]
    fn find_splice_at_boundaries() {
        let splices = vec![src(10, 0), rt(20), sil(30)];

        assert_eq!(find_splice_at(&splices, 0), (0, 0));
        assert_eq!(find_splice_at(&splices, 9), (0, 9));
        assert_eq!(find_splice_at(&splices, 10), (1, 0));
        assert_eq!(find_splice_at(&splices, 29), (1, 19));
        assert_eq!(find_splice_at(&splices, 30), (2, 0));
        assert_eq!(find_splice_at(&splices, 59), (2, 29));
        // past end
        assert_eq!(find_splice_at(&splices, 60), (3, 0));

        // empty slice vec
        assert_eq!(find_splice_at(&[], 0), (0, 0));
    }
}
