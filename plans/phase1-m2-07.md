# Phase 1 · M2 · Step 7 — EDL cursor (action plan)

Per-step action plan for Step 7 of the M2 milestone from [phase1-m2.md](phase1-m2.md) — the
**read-side core of both playback and export**. The authoritative spec is
[audio-pipeline.md § Building the playback / export EDL](../design/audio-pipeline.md#building-the-playback--export-edl)
and [§ Overlapping turns](../design/audio-pipeline.md#overlapping-turns); it consumes the M1 timeline tree
([data-model.md § Temporal query](../design/data-model.md#temporal-query): `element_at_sample`, `iter`).

> **The cursor is a transient iterator, not a persisted structure.** The per-turn splice vecs remain
> the single source of truth; the cursor *resolves* them into render-ready descriptors only while a
> consumer pulls. It does three things the splices alone don't: resolves implicit positions to
> **absolute project samples**, synthesizes a **lead-in gap** before a track's first content, and
> **merges all tracks into sample-aligned mix slices**. It yields lazily (no materialized
> whole-timeline list) so it scales to multi-hour projects and feeds the real-time pre-roll thread
> cheaply, holding **no SQLite connection** while iterating (it walks the in-RAM `Arc` tree).

The data model splits **horizontal** from **vertical**:

- **`EdlSegment` is vertical** — *what one track plays* over a span: a windowed reference to one
  pristine splice (`{ track_id, splice, offset_in_splice }`). It carries **no** project position and
  **no** length; both are properties of the span it sits in, not of the track's contribution.
- **`MixSlice` is horizontal** — *one span of the merged project timeline*
  (`{ start_sample, length_samples, segments }`), shared by every track: at a given merged position
  all tracks share the same start and the same length, and each contributes one `EdlSegment`
  (absent track ⇒ silence ⇒ 0). The renderer (Step 8) **sums** a slice's segments and writes
  `length_samples` frames at `start_sample`.

Pre-split into **7a (per-track walk)** and **7b (merge)**.

**Definition of done:**
- `core/src/project/tree.rs` gains `ImplicitTimelineTree::iter_from(sample) -> TreeIter`, a
  **seeking** in-order iterator positioned at the element covering `sample` (the per-track walk
  cannot build a mid-tree `TreeIter` from outside the module — `TreeIter::stack` is private — so the
  seek lives here). `iter_from` reuses `TreeIter::next` unchanged.
- `core/src/audio/edl.rs` exposes `EdlSegment` (slim, vertical), `MixSlice` (horizontal), a
  per-track `TrackCursor` seekable to a project sample and yielding ordered `EdlSegment`s, and the
  `EdlCursor` that merges a set of `TrackCursor`s into one position-ordered stream of `MixSlice`s.
- The test matrix below passes; `cargo test -p core audio:: project::tree::`,
  `cargo clippy -p core -- -D warnings`, `cargo fmt --check` green.

**No samples are read here** — the cursor emits *descriptors*; Step 8 reads PCM, applies gain, sums.

> **This is the read/replay side of a translate-and-replay seam ([conventions.md](../design/conventions.md)
> A4).** The cursor *replays* two implicit coordinate systems into absolute project samples: the
> `Location`-anchored (`After(predecessor)`) timeline tree (accumulated by the tree walk) and the
> length-only splice vecs (recovered by prefix-sum). The half-open `[start, end)` bound against an
> implicit-position stream is exactly where A4's append/end off-by-one bites, so the test matrix
> carries an explicit **A-group** for the boundary-translation and durable-round-trip cases. A4's
> *batch + inverse* category does **not** apply here — the cursor is read-only with no edit to
> invert (that half of the seam lives on the write side, Step 6 / M5); the A-group notes the
> intentional omission so it reads as deliberate, not a gap.

## Decisions locked in this step

- **`EdlSegment` is a slim, vertical descriptor: `{ track_id, splice, offset_in_splice }`.** The
  `splice` is a **pristine** copy of the persisted splice (original length, fades, and
  `source_start_sample` — unmodified); `offset_in_splice` is where *this* track starts reading
  within it. No `start_sample`, no `length_samples` — those belong to the enclosing `MixSlice`
  (below). `EdlSegment` only ever appears as a slice member.
- **`MixSlice` is the horizontal span: `{ start_sample, length_samples, segments: Vec<EdlSegment> }`.**
  `start_sample` is the absolute project position; `length_samples` is the span length; `segments`
  is one `EdlSegment` per track active in the span, ordered by ascending `track_id`. The renderer
  reads `length_samples` frames for each segment and sums them.
- **Reuse `SpliceKind` — no parallel `SegmentKind`.** A splice's kind *is* the segment's kind
  (`Source { source_start_sample }` / `RoomTone` / `Silence`); a splice is a splice whether it sits
  in a turn or is pointed at by the cursor. `SpliceKind` (`project::turn`) is already the in-memory
  *latest* type (the frozen wire form is `v1::SpliceKindV1`), so reusing it couples the render path
  to the logical enum, not the wire format — correct and intended.
- **`offset_in_splice` is the one resolved "position" a renderer needs per track.** For frame `i` of
  a slice (`0 <= i < slice.length_samples`), `p = segment.offset_in_splice + i` is the frame's
  position *within the original splice*. The renderer reads source at `splice.source_start_sample +
  p` (the splice stays pristine — the read offset is **recomputed at read time**, matching
  [audio-pipeline.md § Building the EDL](../design/audio-pipeline.md#building-the-playback--export-edl)). The
  same `p` anchors the seam fades: the renderer applies the splice's `fade_in_samples` /
  `fade_out_samples` as **centered equal-power crossfades** at the splice boundaries, reading source
  handles across the seam — the cursor's job is only to carry the pristine fade lengths + `p` that
  let Step 8 (`phase1-m2-08.md`) reconstruct the correct (possibly partial) crossfade state.
- **Length is never stored per track — it is derived, then chosen by the slice.** A track's
  run-length until its next boundary is `splice.length_samples - offset_in_splice` (the splice
  carries its own length). The merge takes the **minimum** run-length across active tracks and
  clamps it to `end`; that minimum is the slice's `length_samples`. The two truncations that make a
  span shorter than a splice — the `end` bound and a foreign track's boundary — are both
  slice-level, so they live on `MixSlice.length_samples`, not on any `EdlSegment`.
- **The per-track walk is seek-then-stream, via `iter_from`.** `iter()` builds its stack from the
  root's **left spine** (base 0) — it is hard-wired to the timeline origin and cannot start
  mid-tree, and `element_at_sample` returns an element with **no traversal state**. So 7a adds
  `tree.iter_from(start)`: descend from the root building the `TreeIter` stack so the first `next()`
  yields the element covering `start` with its true accumulated start sample (`O(log n)`, no
  re-traversal; `next()` is reused verbatim). A `TrackCursor`'s first emitted segment is windowed by
  the seek (its `offset_in_splice` is the in-splice offset of `start`); the pristine splice —
  including `source_start_sample` — is untouched.
- **Mid-splice / mid-seam resume falls out of `offset_in_splice`.** Because each segment carries the
  pristine splice plus its read offset, a span that begins partway through a splice renders
  correctly: a continuation split (a foreign boundary mid-splice) reads the source contiguously and
  carries no seam fade, so a pristine splice spanning several slices is seamless; and a seam's
  centered crossfade region, split across slices by foreign boundaries, is reconstructed by Step 8
  from the same `offset_in_splice` (its shared fade accumulator owns the crossfade window, not the
  slice structure). This is a **correctness** requirement for the merge — a slice boundary that
  splits a faded splice or a crossfade region must not click — and a free consequence of the
  representation.
- **Lead-in gap (per track).** A `TrackCursor` whose `project_start_sample` sits later than the
  requested `start` first emits a synthetic `Silence` segment — a `Splice` of length
  `project_start_sample - start`, zero fades, `offset_in_splice == 0` — before its first real
  segment. Inter-turn silence is already part of each turn's splice tiling (`post_turn_silence`), so
  it is **not** re-synthesized; the only intra-track gap is this lead-in.
- **The merge is sample-aligned lockstep, not interleaving.** Summing across tracks is vertical and
  sample-aligned, so the `EdlCursor` advances all tracks together from one shared running `pos`:
  each `next()` emits the span `[pos, pos + len)` where `len` is the minimum run-length across active
  tracks, clamped to `end`; it windows each track's current segment to that span (advancing that
  track's `offset_in_splice` by `len`, pulling the next segment from that `TrackCursor` when a
  splice is used up), and advances `pos`. The result is a `MixSlice`. The cursor does **boundary
  alignment** (where the absolute positions already live); only the f32 **sum** is left to the
  Step-8 mixer — honouring the spec's "merge by project-timeline position, mixing samples" / "merge
  at the mix step" without forcing the renderer to re-derive boundaries.
- **Absence is zero; trailing tracks just stop contributing.** A track with no content in a span
  contributes no segment (the mixer sums zero for it) — so a short track ending before a long one
  needs no synthesized trailing silence; it simply drops out of later slices. Each `TrackCursor`
  lead-pads itself from `start` to its `project_start_sample`, so every track contributes across
  `[start, its_end)` and the boundary union covers the stream up to the **last** track's end (within
  `[start, end)`). Single-track playback is the degenerate `k == 1` merge: every `MixSlice` has one
  segment.
- **Empty / degenerate.** An empty tree contributes nothing (only its lead-in, if any); a `start`
  past the end yields nothing; a zero-length `[s, s)` range yields nothing; a merge of
  all-empty/exhausted tracks yields nothing. None panic.

## Module surface

```rust
// project/tree.rs (addition)
impl<T: Tilable> ImplicitTimelineTree<T> {
    /// In-order iterator positioned at the element covering `sample`.
    ///
    /// The first `next()` yields the element whose interval contains `sample` (or the
    /// element starting exactly at `sample`), carrying its true accumulated start sample;
    /// the walk then proceeds in timeline order. `sample <= 0` reproduces `iter()`;
    /// `sample >= total_duration()` yields an empty iterator. `O(log n)`; `next()` reused.
    pub fn iter_from(&self, sample: i64) -> TreeIter<'_, T>;
}
```

```rust
// audio/edl.rs
use crate::project::turn::{Splice, Turn};   // SpliceKind reused via Splice.kind
use crate::project::tree::ImplicitTimelineTree;

/// Vertical: one track's contribution over a span — a window into one pristine splice.
///
/// Carries no project position or length (those are the enclosing `MixSlice`'s). The
/// renderer reads the slice's `length_samples` frames starting at in-splice offset
/// `offset_in_splice`, applying the splice's fades anchored to the *original* splice edges
/// (so a span beginning mid-fade resumes the ramp rather than restarting it).
pub struct EdlSegment {
    pub track_id: u32,
    pub splice: Splice,           // PRISTINE: original length, fades, source_start_sample
    pub offset_in_splice: i64,    // source-read offset + fade phase (0 = splice head)
}

/// Horizontal: one sample-aligned span of the merged project timeline.
///
/// All `segments` cover exactly `[start_sample, start_sample + length_samples)`; an absent
/// track contributes no segment (its samples are zero in the mix).
pub struct MixSlice {
    pub start_sample: i64,
    pub length_samples: i64,
    pub segments: Vec<EdlSegment>,   // one per active track, ascending track_id
}

/// Per-track engine: a seekable, pull-based walk over one track's timeline tree. Holds no
/// DB connection. Yields full-splice windows (lead-in `Silence`, seek-clipped first segment);
/// the `EdlCursor` re-slices these to sample-aligned spans.
///
/// **Superseded in Step 9 (sub-step 9d):** `TrackCursor`/`EdlCursor` (and thus `Renderer`) no
/// longer borrow the tree (`<'a>` is gone); they own their traversal state via `Arc<Node>` clones
/// from the new `ImplicitTimelineTree::owned_iter_from` (the immutable tree is already `Arc`-backed,
/// so this is cheap structural sharing). This makes the built `Renderer` `'static + Send` so it can
/// be moved into the playback pre-roll thread. `TrackCursor::at` still borrows the tree only during
/// construction. The signatures below show the original Step-7 borrowing form.
pub struct TrackCursor<'a> { /* iter_from state + current turn splices + offset + track_id + project_start */ }

impl<'a> TrackCursor<'a> {
    /// Position a per-track cursor at project sample `start` over one track.
    /// `project_start_sample` offsets the track on the project timeline (lead-in is synthesized
    /// when `project_start_sample > start`). The `end` bound is the `EdlCursor`'s, not the track's.
    pub fn at(
        tree: &'a ImplicitTimelineTree<Turn>,
        track_id: u32,
        project_start_sample: i64,
        start: i64,
    ) -> Self;
}

impl<'a> Iterator for TrackCursor<'a> {
    type Item = EdlSegment;          // full-splice windows in timeline order
}

/// Merges a set of per-track cursors into one position-ordered stream of mix slices.
pub struct EdlCursor<'a> { /* per-track cursors + per-track current segment + running pos + end */ }

impl<'a> EdlCursor<'a> {
    /// Merge `tracks` over `[start, end)` (`end == None` walks to the last track's end).
    pub fn new(tracks: Vec<TrackCursor<'a>>, start: i64, end: Option<i64>) -> Self;
}

impl<'a> Iterator for EdlCursor<'a> {
    type Item = MixSlice;            // ordered by start_sample; windowed to [start, end)
}
```

## Sub-steps

### 7a — per-track walk (`TrackCursor` + `iter_from`)

- `ImplicitTimelineTree::iter_from(sample)`: descend from the root building the `TreeIter` stack.
  At each node `node_start = base + left_subtree_sum`: if `sample < node_start`, push `(node, base)`
  and recurse left (same base); if `sample >= node_start + total_duration()`, do **not** push and
  recurse right with `base = node_start + total_duration()`; otherwise push `(node, base)` and stop.
  The existing `TreeIter::next` then yields the target element first, with the correct accumulated
  start sample, and the rest of the walk follows. (`TreeIter`/`stack` are derived, never serialized —
  no persistence/migration impact.)
- `TrackCursor::at`: take `tree.iter_from(start - project_start_sample)` (track-local) and remember
  `track_id`, `project_start_sample`, and the track-local seek offset; buffer the current turn's
  splices + a per-turn prefix-sum position. `next()` yields the next splice as a slim `EdlSegment`
  (pristine splice + `offset_in_splice`): `0` for a whole splice, the in-splice seek offset for the
  first real segment after a mid-splice `start`. When `project_start_sample > start`, the **first**
  `next()` yields the synthetic lead-in `Silence`. The cursor does **not** apply `end` — it walks to
  the track's own end; the `EdlCursor` owns the `end` clamp.

### 7b — merge (`EdlCursor` + `MixSlice`)

- `EdlCursor::new(tracks, start, end)`: prime one **current segment** per track by pulling each
  `TrackCursor`'s first `EdlSegment`; set `pos = start`.
- `next()`: drop exhausted tracks; if none remain (or `pos >= end`), return `None`. Compute each
  active track's run-length `splice.length_samples - offset_in_splice`; let `len = min(run-lengths)`
  clamped to `end - pos`. Emit `MixSlice { start_sample: pos, length_samples: len, segments }` where
  `segments` clones each active track's current `EdlSegment` (ascending `track_id`). Then advance
  every active track: add `len` to its `offset_in_splice`; when that reaches the splice end, pull the
  track's next `EdlSegment` (or mark it exhausted). Set `pos += len`. No mixing — boundary alignment
  + provenance only.

### 7c — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings` (incl. `missing_docs`, `unwrap_used`);
  `cargo test -p core audio:: project::tree::`.
- Confirm [audio-pipeline.md § Building the EDL](../design/audio-pipeline.md#building-the-playback--export-edl)
  matches; record (a) the lead-in-only gap-synthesis clarification and (b) that **boundary alignment
  for multi-track mixing lives in the cursor (`MixSlice`), the f32 sum in the Step-8 mixer** — if
  either needs saying there (CLAUDE.md doc-sync).
- One commit `1M2-07: EDL cursor (per-track walk + merge)` on `claude/1M2`, unsigned.

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests`. Helpers build `Turn`s with chosen splice vecs and assemble a
tree via the M1 `insert_at`. The `iter_from` cases live in `project::tree` tests; the rest in
`audio::edl`. Groups: I = `iter_from`, S = per-track resolution, K = seek, F = fades, G = lead-in,
M = merge / slices / `end`, X = cross-cutting, **A = A4 translate-and-replay seam**
([conventions.md](../design/conventions.md) A4 — named cases).

**I — `iter_from` (in `project::tree` tests)**

1. **Seek into element interior.** `iter_from(mid_of_element_k)` → first item is element `k` with
   its true `start_sample` (= element start, not the seek sample); the remaining items are
   `k+1, k+2, …` with correct start samples.
2. **Seek to an exact element boundary.** `iter_from(start_of_element_k)` → element `k` first, whole.
3. **`iter_from(0)` equals `iter()`** — identical hash + start-sample sequence.
4. **`iter_from(sample <= 0)`** reproduces the full walk; **`iter_from(total_duration())`** and
   **`iter_from(> total_duration())`** yield empty iterators. Empty tree → empty for any `sample`.
5. **Random seeks match a linear scan.** For a randomized tree, `iter_from(s)` equals
   `iter().skip_while(start + dur <= s)` for many `s` (start samples + hashes identical).

**S — per-track resolution (`TrackCursor`)**

6. **One turn, one Source splice.** A `TrackCursor` over the track yields a single `EdlSegment`:
   pristine splice, `offset_in_splice == 0`, the splice's `source_start_sample`, and `track_id`.
7. **Multi-splice turn (post-cut).** A turn with before-Source / RoomTone / after-Source splices
   yields three segments in order, each pristine, `offset_in_splice == 0`.
8. **Two turns concatenated.** Turn A then turn B → segments are emitted A-then-B in timeline order
   (positions are stamped later, by the slice — see M).
9. **post_turn_silence is part of the turn, not a synthesized gap.** A turn with a trailing Silence
   splice covering `post_turn_silence` yields that Silence segment from the turn itself; no extra
   gap segment is injected between it and the next turn.
10. **Fades carried through.** A splice with non-zero `fade_in_samples`/`fade_out_samples` is yielded
    pristine (fades unchanged).
11. **Walk covers the whole track.** Σ `splice.length_samples` over the walk == `tree.total_duration()`.

**K — seek (`TrackCursor`)**

12. **Seek to a mid-splice start.** `TrackCursor::at(.., start = project_start + mid_of_splice)` →
    the first yielded segment has `offset_in_splice == in_splice_offset` and a **pristine** splice
    (original length, fades, and `source_start_sample` unchanged — the renderer adds
    `offset_in_splice` to read the right audio).
13. **Start at exact element boundary.** `start == a turn's project start` → no leading partial; the
    turn's first splice is yielded whole (`offset_in_splice == 0`).
14. **Start past end of track** → no segments (after any lead-in).

**F — fade survival across spans**

15. **Seek into a fade-in resumes mid-ramp (data, not gain).** A splice with `fade_in_samples = N`
    seeked at in-splice offset `k < N` → the segment carries the pristine `fade_in_samples == N` and
    `offset_in_splice == k`, so `equal_power_gain(offset_in_splice + i, N)` at `i == 0` is the
    **partial** gain `sin(π/2 · k/(N−1))`, not `0`. (Step 8 applies the gain; here we pin that the
    cursor preserves the inputs that make mid-ramp resume possible.)
16. **A slice boundary splitting a fade is seamless (cross-ref M).** See M24: the two consecutive
    slices over a fade region carry consecutive `offset_in_splice` values for the same splice, so
    `equal_power_gain` is continuous across the boundary (no restart, no skip).

**G — lead-in gap**

17. **Lead-in silence.** A `TrackCursor` with `project_start_sample == P > start` (e.g. `start == 0`)
    yields a leading `Silence` segment first (synthetic splice of length `P - start`, zero fades,
    `offset_in_splice == 0`) before the track's first real segment.
18. **No spurious gaps in a contiguous track** (covered by S9/S11): a contiguous track yields no
    synthesized `Silence` between turns.

**M — merge / slices / `end` (`EdlCursor` → `MixSlice`)**

19. **Single track, full walk.** `EdlCursor::new(vec![one cursor], 0, None)` → each `MixSlice` has
    exactly one segment; `start_sample`s are the running prefix sum; Σ `length_samples` ==
    `track total_duration`; consecutive slices are contiguous (each `start + length` == next
    `start`).
20. **Seek + `start_sample` stamping.** Starting at a mid-splice `start` → the first `MixSlice` has
    `start_sample == start` and its one segment's `offset_in_splice` == the in-splice offset.
21. **Bounded `[start, end)`.** With `end` inside a span, the walk stops there; the final
    `MixSlice.length_samples` is clamped so the slice ends exactly at `end`; nothing past `end`.
    **Zero-length `[s, s)`** → no slices. **`end == None`** walks to the last track's end.
22. **Two non-overlapping tracks.** Track 1 `[0, 10s)`, track 2 at `project_start_sample = 10s` →
    single-segment slices: track-1 content (with track-2's lead-in `Silence` during `[0, 10s)`),
    then track-2 content — position-ordered, spans tiling `[0, end)` gaplessly.
23. **Overlapping tracks share a slice.** Track 1 and track 2 both have content over `[0, 5s)` with
    **different internal boundaries** → each `MixSlice` covers `[pos, min-next-boundary)` and its
    `segments` has one entry **per track**; assert spans tile `[0, 5s)` gaplessly and every slice in
    the overlap has 2 segments (summed by Step 8, not the cursor).
24. **Partial splice across a foreign boundary (+ fade continuity).** Track 1 has a single long
    splice with `fade_in_samples = N`; track 2 forces a boundary at in-splice offset `k`. The merge
    splits track 1 into consecutive slices whose track-1 segments carry `offset_in_splice` 0 then `k`
    and **consecutive source reads** (`source_start_sample + 0`, then `+ k`) — proving the
    partial-splice advance and the seamless fade (F16).
25. **Segment order within a slice by `track_id`.** A slice with two segments emits them ascending by
    `track_id` (deterministic).
26. **`project_start_sample` honoured per track.** A track offset by `P` has its content shifted by
    `P` in the merged stream; its lead-in `[start, P)` is its `Silence` segment.
27. **Empty / exhausted track.** An empty track contributes nothing (beyond any lead-in); a short
    track that ends early drops out of later slices (no synthesized trailing silence), and the merged
    stream runs to the **last** track's end. A `start` past every track's end → no slices.

**X — cross-cutting**

28. **No SQLite connection while iterating.** Asserted by lifetime/signature
    (`&ImplicitTimelineTree` only) + a test that drives a full merged walk with no `Db`/`Connection`
    in scope.
29. **Lazy.** Constructing the cursor does not materialize all slices (a cursor over a large
    synthetic tree allocates `O(1)`/`O(log n)` per track — `O(k)` total for the per-track current
    segments, not `O(n)` — before the first `next()`; assert via a bounded peek or by taking only the
    first slice from a large tree without OOM/timeout).
30. **Determinism.** Two identical merged walks yield identical `MixSlice` sequences (positions,
    lengths, and each segment's `track_id`/`splice`/`offset_in_splice`).

**A — A4 translate-and-replay seam (read/replay side)** ([conventions.md](../design/conventions.md) A4)

These pin the half-open `[start, end)` bound against the implicit-position stream — the append/end
boundary where the two coordinate systems disagree by one — and the durable round-trip that proves
the prefix-sum/tree-walk replay survives persistence. (A4's *batch + inverse* category is N/A for a
read-only cursor — see the seam note above.)

31. **A4 append/end boundary — `end == total_duration()` (one-past-the-last, half-open).** A bounded
    walk with `end == tree.total_duration()` yields **through the last sample**: the final
    `MixSlice` ends exactly at `total_duration` (no dropped last sample, no extra empty trailing
    slice), and Σ `length_samples` == `total_duration`. Pin against `end == None` (test 21) yielding
    the identical stream.
32. **A4 append/end boundary — `start == total_duration()` (exactly at end).** Distinct from "start
    past end" (test 27): seeking to *exactly* the end sample yields **no slices** under half-open
    `[start, end)` — the one-past-the-last position is not a valid element start. (`iter_from`
    returns an empty iterator there; see I4.)
33. **A4 append/end boundary — `end` exactly on a splice/turn boundary.** With `end` equal to a
    splice's (and, separately, a turn's) **start sample**, the slice ending at `end` is yielded whole
    and the slice starting at `end` is **excluded** (half-open). Pair with `end == that boundary − 1`
    asserting a final slice clipped one sample shorter, and `end == that boundary + 1` asserting a
    1-sample slice past the boundary — the trio that distinguishes half-open from an inclusive
    endpoint.
34. **A4 durable round-trip — load-bearing (replay equals live).** Build the tree(s), capture the
    full `MixSlice` stream (per slice: `start_sample`, `length_samples`, and each segment's
    `track_id`, kind, source offset, fades, `offset_in_splice`). Persist the turns/tree through the
    M1 `store`→`load` round-trip (`encode_turn`/`decode_turn` + tree rebuild), construct a fresh
    cursor over the reloaded tree(s), and assert a **byte-identical slice stream**. This is the
    load-bearing assertion: a splice-`length_samples` serialization defect would round-trip a
    structurally-valid tree yet shift every absolute `start_sample` downstream — silent until reopen
    (user data loss, [conventions.md § G](../design/conventions.md#g-data--persistence-integrity)). No
    intervening snapshot of the correct in-memory tree. (Strengthens the determinism case from "same
    tree → same walk" to "persisted tree → same walk".)

## Out of scope for Step 7

- **Reading PCM / room-tone looping / wet-dry / fade-gain application / mixing (the f32 sum) /
  clamp** — Step 8 (`render.rs`) consumes these `MixSlice`s.
- **The cpal stream + ring buffer + real-time thread** — Step 9.
- **Tail padding to project length for export** — Step 10 builds it on top of the bounded cursor.
- **Building/maintaining the splice vecs** — Step 6 (write side); this step only reads them.
