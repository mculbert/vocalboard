# Phase 1 · M2 · Step 6 — Splice subdivision + merge (action plan)

Per-step action plan for Step 6 of the M2 milestone from [phase1-m2.md](phase1-m2.md) — the
**write side of the per-turn EDL**, moved from M5 into the audio engine. The authoritative spec is
[audio-pipeline.md § Updating on cut / mute (and uncut / unmute)](../design/audio-pipeline.md#updating-on-cut--mute-and-uncut--unmute);
the splice data model is
[data-model.md § Turn payload](../design/data-model.md#turn-payload-the-unit-stored-in-the-blob-store).

This step lands the **pure functions that transform a turn's `splices` vec**: the **forward**
subdivision a cut or mute produces, and the **inverse** merge an uncut or unmute produces. They are
**deterministic functions of the splice vec + a caller-resolved span (+ the seam crossfade length for
the forward ops only)** — no tree, no DB, no command, no journaling, no settings read, and no `Word`
type. The M5 editing
commands call these, then re-store the new immutable `Turn` via the M1 `encode_turn` + `store::put`
write path; M4 import builds the **initial single-`Source` splice inline** (it is a one-line struct
literal — `Splice { length_samples: turn_duration + post_turn_silence, fade_in_samples: 0,
fade_out_samples: 0, kind: Source { source_start_sample } }` — so it needs no function here).

**Definition of done:** `core/src/audio/splice.rs` exposes `subdivide_on_cut`, `subdivide_on_mute`,
`merge_on_uncut`, `merge_on_unmute` (sharing private `push_coalesced` / `advance_source` /
`emit_restored_source` / `merge_span` helpers); each maintains the **tiling invariant** (Σ
`length_samples` == the turn's current `turn_duration + post_turn_silence`); a within-splice forward
op followed by its inverse returns the **byte-identical** pre-edit vec; the test matrix below passes;
`cargo test -p core audio::`, `cargo clippy -p core -- -D warnings`, `cargo fmt --check` green.

> **This is a translate-and-replay seam ([conventions.md](../design/conventions.md) A4).** The edit is
> *executed* in turn-relative **sample** offsets (`start` / `end`, resolved by the caller from the
> Step-5 zero-crossings) but *persisted* as a **length-only splice vec** whose positions are
> implicit and *replayed* by prefix-sum (here on a second edit, and at read time in Step 7). The
> restored audio's source position travels as a third coordinate — the word's frozen
> `source_onset_sample` — passed straight through. The test matrix carries an explicit **A-group**
> for the A4-mandated boundary-translation, batch-composition, and durable-round-trip cases. Unlike
> the original plan, the **inverse (undo) half of the seam lands here** (`merge_on_uncut` /
> `merge_on_unmute` + the round-trip cases), not in M5 — M5 inherits only the *command/journal-delta*
> round-trip.

## Prerequisite (lands with this step)

`merge_on_*` restores audio at the word's frozen source position, which requires the data-model
change agreed for this revision:

- **`Word::turn_offset_sample: i64` → `source_onset_sample: Option<i64>`** — an *absolute* sample
  offset into the project-rate source/cache timeline (same units as `SpliceKind::Source ::
  source_start_sample`), `None` until its zero-crossing is refined. Project-timeline position is now
  derived, not stored. (See [data-model.md § Turn payload](../design/data-model.md#turn-payload-the-unit-stored-in-the-blob-store).)
- This touches the M1 in-memory `Word`, its V1 wire type, and the `From` conversions in
  `project/turn.rs`, and **regenerates the pinned hex/hash + round-trip fixtures** (allowed pre-1.0;
  V1 "MAY be revised" per the `mod v1` doc-comment — no `v2` migration). `splice.rs` itself does
  **not** depend on `Word` (it takes scalars), so the rename is a sibling change, not a coupling.

## Decisions locked in this step

> **Revision (review):** the primitives were generalized from single-splice/`Source`-only to
> **arbitrary spans that cross any number of boundaries and any kinds** (cutting/muting a multi-word
> selection; restoring source across a previously edited region in one call). The M5 caller owns the
> cut/mute interaction policy and is responsible for correct spans; each op simply performs the span
> operation it is given. Consequences: (a) coalescing generalizes beyond `Source` — adjacent
> `RoomTone`s and adjacent `Silence`s coalesce too (sequential adjacent mutes, or cutting a source
> from between two room tones); (b) the standalone `coalesce_sources` pass is replaced by an inline
> streaming `push_coalesced` (single sweep, single allocation) shared by all four ops, with the two
> merges sharing a `merge_span` helper; (c) `merge_on_uncut` may insert mid-splice (splitting a
> coalesced room tone), so `start` need not be a boundary; (d) preconditions are `debug_assert!`-
> checked (`0 ≤ start ≤ end ≤ total`, source-overlap with the following splice on a merge, etc.) with
> the no-op as the release fallback; zero-length span and empty input stay silent no-ops. The bullets
> below are updated to match; the authoritative spec is
> [audio-pipeline.md § Updating on cut / mute](../design/audio-pipeline.md#updating-on-cut--mute-and-uncut--unmute).

- **The primitives are coordinate-pure, not word-aware.** Cut/mute take a caller-resolved span
  `[start, end)` in **current-vec** turn-relative samples; the policy that resolves them from a word
  (refine the zero-crossing, extend a cut to the next word's onset to swallow inter-word silence,
  translate frozen→current coordinates) lives at the **M5 command layer**. This keeps `splice.rs`
  pure, `Word`-free, and trivially testable. (`subdivide_on_cut` therefore has a single `end`, not
  the former `offset` + `following_word_onset`.)
- **The seam crossfade is passed in as samples**, `crossfade_samples: i64` — resolved once by the
  caller from `splice_crossfade_ms` + the project rate (exactly as Step 5 hands the engine
  `crossfade_frames`). No `rate` parameter and no `frames_from_ms` call here; the audio engine reads
  no settings and does no ms→sample math (the integer-samples invariant). Interior original fades
  are preserved; only newly created seams are stamped. **Only the forward ops (`subdivide_on_cut` /
  `subdivide_on_mute`) take `crossfade_samples`** — they create a genuinely new interior seam where
  neither side yet carries a fade. The **inverse ops (`merge_on_*`) take no crossfade**: every seam a
  restored `Source` touches either coalesces away (interior, dropped) or survives, and a surviving
  seam already carries the original fade on its neighbour, which the merge copies (see below). This
  also means the inverse restores the *original* fade even if `splice_crossfade_ms` changed between
  the edit and its undo (consistent with [audio-pipeline.md](../design/audio-pipeline.md#updating-on-cut--mute-and-uncut--unmute):
  per-boundary fades are independently editable splice state, not recomputed).
- **A cut removes `[start, end)` and shrinks the turn.** The splice containing `start` is trimmed to
  its **head** `[splice_start, start)` (any kind, fades preserved) and the splice containing `end` to
  its **tail** `[end, splice_end)` (a `Source` tail's `source_start_sample` advances by `end −
  splice_start`); every splice strictly inside the span is dropped; zero-length head/tail sides are
  dropped. The new seam carries `crossfade_samples`. The new `turn_duration` is the caller's concern,
  but the returned vec already tiles the shrunk length.
- **A mute replaces `[start, end)` with a single non-`Source` splice and keeps the turn length.**
  Same head/tail trimming as the cut, with a middle of kind `RoomTone` **or** `Silence` (per
  `mute_to_room_tone`) spanning `[start, end)`; both new seams carry `crossfade_samples`. Σ lengths
  unchanged. (Muting a span abutting an existing room tone coalesces into it — see below.)
- **`mute_to_room_tone` is a parameter, not a settings read** (the "mute to silence" preference
  lives at M5). Same purity rationale as the crossfade.
- **Uncut / unmute are *merges*** over the shared `merge_span` helper. `merge_on_uncut` inserts a
  `Source` of length `restore_len` reading `source_start_sample` at `start`, re-growing the turn —
  splitting the containing splice when a prior coalesce merged a room tone over the gap, so `start`
  need **not** be a boundary; `merge_on_unmute` replaces the span `[start, start + restore_len)` with
  such a `Source` (turn length unchanged). Coalescing is inline (see below). Two `debug_assert!`-
  checked invariants: the restored source `[source_start_sample, source_start_sample + restore_len)`
  must **not overlap the following splice's source**, and `start` must **not fall inside a `Source`
  splice** (splitting contiguous source audio — there is no edited region there to restore).
- **Coalescing (`push_coalesced`) generalizes beyond `Source` and runs inline** (streaming
  merge-with-previous; single sweep, single allocation; no standalone pass). Two adjacent splices
  coalesce when both are `Source` and source-contiguous
  (`a.source_start_sample + a.length_samples == b.source_start_sample`), or both `RoomTone`, or both
  `Silence` — `length_samples` summed, `fade_in`/`source_start_sample` from the left, `fade_out` from
  the right, interior seam fade dropped. A cut breaks source-contiguity and a mute interposes a
  non-`Source`, so two adjacent `Source`s become contiguous **only** once the within-splice edit
  between them is undone — so such an inverse returns the vec to its **exact pre-edit shape**. The
  restored `Source` carries **surviving-seam fades copied from the abutting neighbours** (`fade_in` ←
  left neighbour `fade_out`, `fade_out` ← right neighbour `fade_in`, 0 at a turn edge); coalescing
  drops whichever become interior. A seam that *survives* (a restored `Source` abutting a
  `RoomTone`/`Silence`, e.g. uncutting a word between two muted words) keeps the copied fade — the
  value the forward op stamped there. This is why the merges need **no `crossfade_samples`**. **The
  source *position* always comes from the word, never neighbour-recovered** — so there is no
  degenerate case. (Inputs are assumed canonical; the ops preserve canonical form.)
- **Spans cross any boundary and any kind.** A cut/mute/unmute span may straddle multiple splices of
  any kind (e.g. cutting into a `RoomTone` trims its length; cutting a `Source` from between two room
  tones coalesces them). There is no "non-`Source` span" special case — the head/tail trimming and
  coalescing handle every kind uniformly.
- **Pure + allocation-returning.** Each function takes `&[Splice]` (+ scalars) and returns a fresh
  `Vec<Splice>`; the input is never mutated (immutability — the caller re-stores a new `Turn`).

## Module surface

```rust
// audio/splice.rs
use crate::project::turn::{Splice, SpliceKind};

// Doc-comment summaries below are abbreviated; the full, authoritative docs are in
// `audio/splice.rs`. The span `[start, end)` may cross any boundaries/kinds; every op
// coalesces (source-contiguous `Source`, adjacent `RoomTone`/`Silence`).

/// CUT: remove `[start, end)`, shrinking the turn (head/tail trimmed, span dropped, new
/// seam carries `crossfade_samples`).
pub fn subdivide_on_cut(
    splices: &[Splice],
    start: i64,
    end: i64,
    crossfade_samples: i64,
) -> Vec<Splice>;

/// MUTE: replace `[start, end)` with one `RoomTone` (or `Silence` when `mute_to_room_tone
/// == false`); turn length unchanged; both seams carry the crossfade.
pub fn subdivide_on_mute(
    splices: &[Splice],
    start: i64,
    end: i64,
    mute_to_room_tone: bool,
    crossfade_samples: i64,
) -> Vec<Splice>;

/// UNCUT: insert a `Source` of length `restore_len` reading `source_start_sample` at
/// `start`, re-growing the turn (splits the containing splice when `start` is interior);
/// surviving-seam fades copied from neighbours; no crossfade parameter.
pub fn merge_on_uncut(
    splices: &[Splice],
    start: i64,
    restore_len: i64,
    source_start_sample: i64,
) -> Vec<Splice>;

/// UNMUTE: replace the span `[start, start + restore_len)` with a `Source` reading
/// `source_start_sample`; turn length unchanged; surviving-seam fades copied from
/// neighbours; no crossfade parameter.
pub fn merge_on_unmute(
    splices: &[Splice],
    start: i64,
    restore_len: i64,
    source_start_sample: i64,
) -> Vec<Splice>;
```

## Sub-steps

> The sub-step descriptions below are superseded by the **Revision (review)** note above (span ops
> crossing any boundary/kind, generalized inline coalescing, shared `merge_span`). Retained for the
> shared helper and prefix-sum mechanics; defer to the note and the source on behaviour.

### 6a — `subdivide_on_cut`

- Prefix-sum the vec; for each splice push the part outside `[start, end)`: the splice containing
  `start` keeps its **head** `[splice_start, start)` (any kind), the splice containing `end` keeps
  its **tail** `[end, splice_end)` (a `Source` tail advances `source_start_sample` by `end −
  splice_start`), splices strictly inside the span are dropped. Zero-length head/tail sides drop.
  Stamp `crossfade_samples` on the new seam (head `fade_out`, tail `fade_in`). Push through
  `push_coalesced`.

### 6b — `subdivide_on_mute`

- Same head/tail trimming with a middle (`RoomTone`|`Silence`, length `end − start`) between them;
  stamp seam crossfades on both new boundaries. `push_coalesced` extends an abutting room tone rather
  than doubling it. Turn length preserved.

### 6c — `merge_on_uncut` / `merge_on_unmute` (shared `merge_span`)

- `merge_span(splices, start, end, source_len, source_start_sample)`: remove `[start, end)` and
  insert a `Source` of length `source_len` in its place, pushing every emitted splice (head, restored
  source, tail, verbatim) through `push_coalesced`. `merge_on_uncut` calls it with `end == start` (a
  pure insertion that grows the turn, splitting the containing splice when `start` is interior);
  `merge_on_unmute` with `end == start + restore_len` (a replacement preserving the turn length). The
  restored `Source` (via `emit_restored_source`) copies `fade_in` from the left neighbour's
  `fade_out` and `fade_out` from the right neighbour's `fade_in` (0 at a turn edge) and asserts no
  source overlap with the following splice.
- `push_coalesced`: streaming merge-with-previous — coalesces source-contiguous `Source`s and
  adjacent `RoomTone`/`Silence` runs (summed `length_samples`, left `fade_in`/`source_start_sample`,
  right `fade_out`, interior fade dropped) in the same single pass that builds the result. No
  standalone coalesce pass, no re-stamped crossfade.

### 6d — Final pass

- `cargo fmt --check`; `cargo clippy -p core -- -D warnings` (incl. `missing_docs`, `unwrap_used`);
  `cargo test -p core audio::`.
- Confirm [audio-pipeline.md § Updating on cut / mute (and uncut / unmute)](../design/audio-pipeline.md#updating-on-cut--mute-and-uncut--unmute)
  matches; the merge/coalesce, crossfade-in-samples, and `mute_to_room_tone`-as-parameter decisions
  are recorded there + in [data-model.md](../design/data-model.md) (CLAUDE.md doc-sync).
- One commit `1M2-06: splice subdivision + merge` on `claude/1M2`, unsigned. The `Word`-rename +
  pinned-fixture regeneration is part of this commit (the prerequisite above).

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests`. A small helper builds a turn's initial single-`Source` vec and
words at chosen offsets. **The tiling-sum invariant is asserted after every op.** Groups:
C = cut, M = mute, U = uncut, N = unmute, R = round-trip/canonical, V = invariants/edge,
X = cross-cutting, **A = A4 translate-and-replay seam** ([conventions.md](../design/conventions.md) A4 — named
cases).

**C — cut**

1. **Cut the only word (whole turn's speech).** Removes `[start, end)`; the surviving vec tiles
   `(d + s) − (end − start)`; the post-turn silence survives as the after piece.
2. **Cut middle word.** Containing `Source` splits into before + after; Σ shrinks by `end − start`;
   splices outside the containing one untouched.
3. **Cut first word.** before piece is zero-length and dropped; result is the re-based after
   `Source`; Σ shrinks by `end − start`.
4. **Cut last word.** `end` is the last word's offset (the post-turn silence is **not** part of the
   word and stays as the after piece).
5. **Re-based source offsets.** The after `Source`'s `source_start_sample` advances by `end − start`
   so it still reads the correct source audio (assert the new value).
6. **Seam crossfade stamped.** The splices meeting at the new seam carry `fade_*_samples ==
   crossfade_samples`.
7. **Cut at a splice edge → 1 surviving piece, no zero-length splice.** A cut whose `start`
   coincides with a splice start drops the empty before piece.
8. **Multi-cut.** Cut one word, then cut another on the resulting vec → final vec equals the
   hand-computed tiling; Σ correct; deterministic.

**M — mute**

9. **Mute to room tone.** Middle word → before-`Source` / `RoomTone` (length `end − start`) /
   after-`Source`; Σ unchanged (== original `d + s`).
10. **Mute to silence.** `mute_to_room_tone == false` → middle splice is `Silence`.
11. **Mute first / last word.** Edge mute yields **2** splices (zero-length side dropped) with the
    `RoomTone`/`Silence` at start/end; Σ unchanged.
12. **After-`Source` re-based.** The after `Source`'s `source_start_sample` advances past the muted
    span (mute keeps timeline length but reads the post-mute source correctly).
13. **Seam crossfades on both new boundaries** (before↔mute and mute↔after).

**U — uncut (`merge_on_uncut`)**

14. **Uncut middle → coalesce to pristine.** before-`Source`/gap/after-`Source` from a prior cut;
    uncut re-inserts the `Source` at `source_start_sample`; the three coalesce into **one** `Source`
    equal to the pre-cut splice (length, source offset, **outer** fades restored, interior seam fade
    gone).
15. **Uncut first word.** No before-`Source`; the restored `Source` becomes the new head and
    coalesces with the after piece; `fade_in == 0` at the turn start.
16. **Uncut between two mutes (non-coalescing).** A cut word flanked by `RoomTone` splices on both
    sides: the restored `Source` does **not** coalesce; it stands alone, taking its `fade_in` from
    the left `RoomTone`'s `fade_out` and its `fade_out` from the right `RoomTone`'s `fade_in` (the
    surviving seam fades — assert they equal the values the surrounding mutes stamped, with **no**
    crossfade parameter). (This is the former "both neighbors non-`Source`" case — now correct, not
    degenerate, because the source position came from the word.)
17. **Turn re-grows.** Σ increases by exactly `restore_len`.

**N — unmute (`merge_on_unmute`)**

18. **Unmute middle → coalesce to pristine.** before-`Source`/`RoomTone`/after-`Source`; unmute
    replaces the `RoomTone` with a `Source` and the three coalesce into the pre-mute splice;
    Σ unchanged.
19. **Unmute to-silence variant.** Same from a `Silence` middle.
20. **Unmute first / last word.** Edge unmute coalesces with the single neighbour.
21. **Unmute between two mutes (non-coalescing).** Restored `Source` stands alone, taking its
    `fade_in`/`fade_out` from the flanking `RoomTone`s' surviving-seam fades (no crossfade
    parameter); Σ unchanged.

**R — round-trip / canonical form** (the headline merge property)

22. **Cut → uncut == identical.** A *within-splice* `subdivide_on_cut` then `merge_on_uncut` (same
    span, `source_start_sample` = the word's frozen onset) yields the **byte-identical** input vec.
23. **Mute → unmute == identical.** Symmetric.
24. **Order-independent convergence.** Cut word 1 and cut word 3 (frozen-original coordinates,
    translated to current-vec by the harness), then uncut both in either order → the original vec;
    same for a mix of a cut and a mute. Proves canonical form ⇒ content-addressed reuse.

**V — invariants / edge**

25. **Tiling-sum after every op** (asserted by the shared helper for all C/M/U/N tests).
26. **Input not mutated.** The `&[Splice]` argument is unchanged after every call.
27. **Zero-length span.** `start == end` (cut/mute) or `restore_len == 0` (uncut/unmute) → a
    documented no-op (equivalent vec), no panic, Σ preserved.
28. **Empty / single-splice input.** `&[]` is a no-op returning `vec![]` (no index panic in the
    prefix-sum locate); the single-`Source` vec is exercised by C1/M9.
29. **Coalesce only when source-contiguous.** Two adjacent `Source` splices left non-contiguous by a
    *still-applied* cut between them are **not** merged by an unrelated inverse op (negative control
    for `push_coalesced`).
30. **Cross-type / cross-boundary span.** A cut/mute span that lands inside or crosses a
    `RoomTone`/`Silence` trims/replaces it (not a no-op); cutting a `Source` from between two room
    tones, or muting adjacent words, coalesces the room tones; unmuting across multiple splices
    restores pristine; uncut into a coalesced room tone splits it. (Implemented as the `g40`–`g43`
    and `p52`–`p53` cases in `splice.rs`.)

**X — cross-cutting**

31. **No DB / no journaling / no `Word`.** Pure functions over scalars + `&[Splice]`;
    signature-asserted.
32. **Determinism.** Same inputs → identical output vec, twice (supports content-addressing).
33. **Round-trips through the Turn blob.** A turn whose `splices` is replaced by a transform result
    `encode_turn`s and `decode_turn`s back equal (the output is valid `Splice` data for the M1 write
    path).

**A — A4 translate-and-replay seam (dual-coordinate)** ([conventions.md](../design/conventions.md) A4)

These strengthen the groups above from "the in-memory vec is right" to "the *coordinate
translation* is right at the boundaries, composes under a batch resolved against the frozen original,
and survives persist→replay." Getting any wrong is silent until the turn is re-read and re-tiled.

34. **A4 boundary translation — position-0.** Cut **and** mute **and** uncut a span whose `start ==
    0` (coincident with the first splice's start): the before piece is zero-length and dropped (no
    leading zero-length splice), the surviving vec tiles, and the head splice's source offset/fades
    are correct (`fade_in == 0` at the turn start after an uncut). (Generalises C7/C3/U15.)
35. **A4 boundary translation — append/end (half-open).** The signature off-by-one. Cut/mute a span
    whose `end` lands **exactly at the containing `Source`'s end** — for the last speech word,
    exactly at `turn_duration` (the speech↔`post_turn_silence` boundary). Assert: the after piece is
    zero-length and dropped (never a 1-sample sliver or a negative length from treating `[start,
    end)` as inclusive); the surviving `post_turn_silence` is not consumed by the cut; the mute's
    inserted splice ends exactly at the splice end and Σ is unchanged. Pair with an interior splice
    edge to pin that the *only* dropped piece is the truly-empty one.
36. **A4 batch + frozen-original coordinate basis.** A batch of ≥2 interacting cuts/mutes on the
    **same** turn, all word onset/offset values expressed in the **frozen original-turn source
    coordinate system** (the Step-5 outputs against the original PCM). The test MUST make the basis
    explicit: because the forward ops operate on the *current* vec (locate-by-prefix-sum), the second
    op's original-coordinate offsets are translated to current-vec coordinates by the harness (the
    translation M5 will own), and the composed result equals a hand-computed tiling resolved against
    the frozen original. Cover forward order **and** the inverse (R24) — proving the prefix-sum
    replay agrees with the sample-coordinate intent in both directions.
37. **A4 durable round-trip — load-bearing (replay equals live).** Take a multi-edit transform
    result, `encode_turn` → `decode_turn`, then **prefix-sum the reloaded vec back to absolute
    turn-relative positions and assert they equal the live (pre-persist) positions**, and Σ == the
    turn length. A serialization defect shifting a single `length_samples` would round-trip a
    structurally-valid vec (test 33 still passes) yet replay every downstream splice at the wrong
    position. (Strengthens test 33 from "vec equals" to "the translation survives persistence.")

## Out of scope for Step 6

- **The `cut_words` / `mute_words` / `uncut_words` / `unmute_words` commands**, undo stamping,
  overlap validation, frozen→current coordinate translation, lazy zero-crossing refinement at the
  seam, and recomputing `turn_duration` / `post_turn_silence` — M5.
- **Computing the word onset/offset** (the zero-crossing search) — Step 5 (`zero_crossing.rs`); the
  refined value reaches us as the caller's `start`/`end`/`source_start_sample` scalars.
- **Eager first-word refinement + the initial single-`Source` splice** — M4 import (the splice is a
  one-line literal; no function here).
- **Rendering `RoomTone`/`Silence`/`Source` splices to samples** — Step 8 (`render.rs`).
- **Walking the tree / building the cross-turn EDL** — Step 7 (`edl.rs`); this step is per-turn.
