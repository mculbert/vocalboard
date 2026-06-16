# Phase 1 · M2 · Step 8 — EDL renderer (action plan)

Per-step action plan for Step 8 of the M2 milestone from [phase1-m2.md](phase1-m2.md) — turns the
Step-7 cursor's `MixSlice` descriptors into **f32 PCM frames**, shared by playback (Step 9) and export
(Step 10). The authoritative spec is [audio-pipeline.md § Room tone substitution](../design/audio-pipeline.md#room-tone-substitution),
[§ Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade),
[§ Overlapping turns](../design/audio-pipeline.md#overlapping-turns), and the wet/dry blend in
[ml-pipeline.md § Enhancement](../design/ml-pipeline.md#enhancement-pipeline-mp-senet).

It is a **pull-based renderer**: given an `EdlCursor` (Step 7, yielding `MixSlice`s) + lazily-opened
source readers, it produces interleaved **stereo** f32 frames for a requested range on demand. It
reads PCM from the resampled-cache FLAC (the uniform dry source), loops the room-tone blob, blends
the enhanced FLAC for wet/dry, **sums each slice's per-track segments** (the cursor has already done
the boundary alignment), applies **centered equal-power crossfades at splice seams** (the edit
crossfade, the room-tone gap fade — both recorded as splice fades), and clamps. It holds **no SQLite
connection** while producing frames — setting up the real-time invariant Step 9 depends on. **No
cpal, no thread, no command here.**

**Definition of done:** `core/src/audio/render.rs` exposes a `Renderer` that, driven by an
`EdlCursor` + a `SourceProvider`, yields sample-exact stereo f32 by rendering each `MixSlice`'s
Source / Silence / RoomTone segments, applying **centered equal-power seam crossfades** (reading
source *handles* across the seam, carried by a shared fade accumulator), the **full** wet/dry blend,
multi-track sum + clamp, and mono→stereo up-mix; the test matrix below passes; `cargo test -p core
audio::`, `cargo clippy -p core -- -D warnings`, `cargo fmt --check` green.

## The crossfade model (read this first)

A splice carries `fade_in_samples` / `fade_out_samples` on **every** kind (`Source`, `RoomTone`,
`Silence`). These are **not** rendered as fades that stay inside the splice's tiling extent — that
would butt-join a fade-out against the next splice's fade-in over **disjoint** samples, producing a
dip, not a crossfade. A real crossfade requires audio from **outside** the splice's extent (the
"handle" — the source that continues past where the splice was trimmed), overlaid at render time.

**The tiling stays authoritative and unchanged.** Splice extents, absolute positions, and the
project length are exactly what the cursor computes; the crossfade is a **render-time overlay**
layered on top, costing extra source reads per seam and moving **no** positions. No persisted-format
change, no cursor change.

For a seam at project position `E` (the tiling boundary where an outgoing splice ends and a different
incoming splice begins — see *seam detection* below), each side runs **one contiguous equal-power
ramp of its own fade length, centered on `E`**:

- **Outgoing** (`fade_out = FO`): read the outgoing source **contiguously across `E`** over
  `[E − FO/2, E + FO/2)`, applying an equal-power **fade-out** (cos: `1 → 0`). The
  `[E − FO/2, E)` half is the splice's own kept audio (lands in the outgoing's slice); the
  `[E, E + FO/2)` half is its **forward handle** (source *past* the trimmed end — for a cut, the
  removed audio; for room tone, the continued loop) and lands in **later** slices.
- **Incoming** (`fade_in = FI`): read the incoming source **contiguously across `E`** over
  `[E − FI/2, E + FI/2)`, applying an equal-power **fade-in** (sin: `0 → 1`). The `[E − FI/2, E)`
  half is its **backward handle** (source *before* `source_start_sample`) and lands in **earlier**
  slices; the `[E, E + FI/2)` half is the splice's own audio (lands in the incoming's slice).

When `FO == FI` (the symmetric default a single cut/mute stamps) the two equal-power ramps sum to
constant power across the seam — no dip, transition crossing exactly on `E`, the chosen zero-crossing.
`FO ≠ FI` (independently-editable fades, future) is **intentionally** not unity-summing — two
independent centered fades, a small bounded power ripple — and is correct, not a bug.

**Why centered:** the perceived transition (the equal-power crossover) lands on `E`, the low-energy
sample the zero-crossing search deliberately picked. This is the DAW-standard default; forward/pre
(post) placement is reserved here as the **fallback when a handle is unavailable** (see graceful
degradation).

## Where each half lands: body vs accumulator (read this second)

The seam machinery splits into two **disjoint, additive** kinds of contribution — this is what lets a
single shared ring work and keeps the per-segment render (8a) oblivious to seams:

| Half | Output window | Source | Rendered by | Faded |
|---|---|---|---|---|
| Outgoing **kept** | `[E − FO/2, E)` | outgoing splice, **in-extent** | the outgoing segment **body**, faded **inline** | fade-out |
| Incoming **own** | `[E, E + FI/2)` | incoming splice, **in-extent** | the incoming segment **body**, faded **inline** | fade-in |
| Outgoing **forward handle** | `[E, E + FO/2)` | outgoing source **past** its trimmed end | **accumulator**, drained during the incoming body | fade-out |
| Incoming **backward handle** | `[E − FI/2, E)` | incoming source **before** `source_start_sample` | **accumulator** (placed early via look-ahead), drained during the outgoing body | fade-in |

The two **body** halves are in-extent and local, so the segment that owns each applies its own
equal-power ramp **inline**, by output-distance-from-`E` (robust to a half split across tiny
continuation slices — same splice, contiguous reads). The two **handle** halves are out-of-extent
reads that must sum into output frames a **different** segment is emitting, so they go through the
ring. Body windows and handle windows are **disjoint** in (source, position), so each output frame in
the seam is exactly *one body term + one ring term = the crossfade*. Inline fades never touch the
ring; the ring never reads in-extent audio.

### Worked seam (symmetric, `FO = FI = 4`)

Seam at `E = 100`; `equal_power_gain(i, 4)` over `i = 0..4`: fade-out `g_o = [1.000, 0.866, 0.500,
0.000]`, fade-in `g_i = [0.000, 0.500, 0.866, 1.000]` (`g_o² + g_i² = 1`). The ramp covers output
frames `98..102`; the `⌈4/2⌉ = 2` pre-`E` frames are kept / backward-handle, the `⌊4/2⌋ = 2` post-`E`
frames are forward-handle / own:

| out frame | i | body term | ring term | per-frame result | power |
|---|---|---|---|---|---|
| 98 | 0 | outgoing kept × 1.000 | bwd-handle × 0.000 | `O·1.000 + I·0.000` | 1 |
| 99 | 1 | outgoing kept × 0.866 | bwd-handle × 0.500 | `O·0.866 + I·0.500` | 1 |
| 100 (`E`) | 2 | incoming own × 0.866 | fwd-handle × 0.500 | `I·0.866 + O·0.500` | 1 |
| 101 | 3 | incoming own × 1.000 | fwd-handle × 0.000 | `I·1.000 + O·0.000` | 1 |

`O` = the outgoing source read **contiguously across `E`** (its kept tail for 98–99, its forward
handle for 100–101); `I` = the incoming source read **contiguously across `E`** (its backward handle
for 98–99, its own head for 100–101). Power is constant (no dip) and the equal-power crossover
straddles `E` symmetrically. For **odd** `FO` the `⌈⌉/⌊⌋` split puts the extra kept sample pre-`E`.
`O`/`I` shown mono; each term is up-mixed to stereo and wet/dry-blended **before** summing.

## Decisions locked in this step

- **Output is interleaved stereo f32 at the project rate** ([audio-pipeline.md § Output stream](../design/audio-pipeline.md#output-stream)).
  Mono sources up-mix to stereo with **equal gain on both channels** (no −3 dB law; the source is
  already mono room-recorded speech). Stereo sources pass through.
- **Source segments read the resampled cache.** A `Source` segment over a slice of length
  `length_samples` reads `length_samples` frames from the track's resampled-cache FLAC starting at
  `splice.source_start_sample + offset_in_splice` (the slice owns the length; the segment owns the
  in-splice read offset — the splice stays pristine, so the read position is **recomputed at read
  time**). The cache is at project rate, so that project-rate position **is** the cache read position
  directly (no separate decode offset is persisted): the renderer seeks via
  `SymphoniaFrameReader::seek_to_frame` and discards the `required − actual` frames Symphonia's seek
  returns. Reads are **ranged**, not whole-file (Step 3's `decode_flac` is whole-file/test-support;
  this step uses the seekable `SymphoniaFrameReader`). Document the seek-discard contract.
- **Seams are detected locally from the slice — no cursor change.** A track's segment consumes its
  splice's **tail** exactly when `offset_in_splice + slice.length_samples == splice.length_samples`;
  the next slice's segment for that track is then a **different** splice, i.e. a seam at
  `E = start_sample + length_samples`. A mid-splice continuation (a foreign track's boundary or the
  run-length minimum splitting one **pristine** splice — `offset_in_splice + length_samples <
  splice.length_samples`) is **not** a seam: it carries **no** crossfade and reads the source
  contiguously (consecutive `offset_in_splice`), so a split fade renders continuously (no restart,
  no overlay).
- **Centered equal-power seam crossfades, recorded as splice fades.** Each seam runs the outgoing's
  `fade_out` and the incoming's `fade_in` as independent equal-power ramps centered on `E` (see *The
  crossfade model*). **All** fade lengths come from the splice — the renderer holds **no** crossfade
  constants. The `equal_power_gain(i, len) -> f32` helper (`audio/mod.rs`; sin fade-in, complementary
  cos fade-out, `fade_in² + fade_out² == 1`) backs the seam — the **same** helper the room-tone
  stitch + loop fold use. All crossfades in the engine are equal-power; there is **no** linear
  fade-gain helper.
  The `FO/2` split is integer (`⌈FO/2⌉` pre-seam kept audio, `⌊FO/2⌋` post-seam handle; symmetric for
  `FI`), so the renderer is sample-exact.
- **Handles are ordinary ranged reads — no new handle-read method.** The outgoing **forward handle** is
  `dry(track, source_start_sample + splice.length_samples, ⌊FO/2⌋)` (wet/dry-blended like any Source
  read); the incoming **backward handle** is `dry(track, source_start_sample − ⌈FI/2⌉, ⌈FI/2⌉)`. A
  `RoomTone` handle **continues the loop phase** (the renderer carries the segment's running loop
  offset into the handle — it does not restart the tone); a `Silence` handle is zeros (and a silence
  side contributes no audible fade). Handles reuse `dry`/`room_tone`; the **one** added provider entry
  is the read-only `source_len` (above), which bounds them — the renderer requests only **valid**
  ranges (see degradation).
- **The room-tone gap crossfade IS the RoomTone splice's stamped fade** — there is **no** separate
  50 ms gap-fade mechanism and **no** gap-fade constant. A muted span's `RoomTone` splice carries
  the gap-fade length on its edges (stamped by the M5 mute command — a room-tone-gap setting,
  typically longer than the cut crossfade; an M5/settings concern, not the renderer's), and the seam
  machinery above crossfades it with its neighbours like any splice. At a speech→room-tone seam the
  source side pulls its forward handle (the muted content) and the room-tone side's backward handle
  is **free** (the loop); vice-versa at the room-tone→speech seam. The renderer's **only**
  room-tone special-case is **looping** — never the gap fade.
- **Room-tone segments loop the stored (pre-crossfaded) blob.** The Step-4 segment already has its
  head/tail loop crossfade folded in, so the renderer **just repeats it** to fill the segment length
  (wrapping seamlessly), then crossfades at its **outer** boundaries via the splice's `fade_in` /
  `fade_out` (above). A track with `room_tone_hash == None` renders RoomTone segments as **silence**.
- **Silence segments are zeros.**
- **Full wet/dry blend at Source read time** (no Phase-1 stub): `out = enhanced × wet_ratio + dry ×
  (1 − wet_ratio)`, both at project rate, where `dry` is the resampled cache and `enhanced` is the
  enhanced FLAC (`enhanced/<track_id>-enhanced.flac`); `wet_ratio` is the track's `wet_dry_ratio`.
  Implement the blend and the **`enhanced() == None` ⇒ `out = dry`** path now, both exercised by the
  test matrix against in-memory providers. A missing enhanced file falls back to dry **even when
  `wet_ratio > 0`** — there is **no inline regeneration** (enhanced PCM is M3 ML output, off the
  real-time path; a cache miss is detected and repaired at **project open**, not at render time).
  Today `wet_ratio` is always 0 (no command sets it) and `enhanced()` is always `None`, so `out ==
  dry` by two independent routes; the renderer must be correct when neither holds. The handle reads
  of a Source seam get the **same** blend.
- **Multi-track mix = sum then clamp.** Within a `MixSlice` all segments cover the **same** span
  (the cursor aligned the boundaries), so the renderer renders each segment over the slice length and
  sums them; the **seam fade accumulator** (below) is summed on top. The mixed result is clamped to
  `[−1.0, 1.0]` **after** summing ([audio-pipeline.md § Overlapping turns](../design/audio-pipeline.md#overlapping-turns)).
- **One shared, project-wide fade accumulator — not per-track.** Every transform that shapes a seam
  contribution (handle read, wet/dry, up-mix, equal-power ramp) is **linear**, and the only
  nonlinearity (the clamp) is **global and final**, so each seam contribution is shaped to stereo at
  output gain and **summed** into a single ring indexed by absolute output position. Forward handles
  (outgoing) and backward handles (incoming) from **every** track land in the same ring at their
  `[E − FI/2, E + FO/2)` windows and drain as the playhead advances — **independent of how many
  `MixSlice` boundaries those samples cross** (this is what makes a fade spanning ≥ 3 tiny slices, or
  two of a track's own seams within one fade, sum correctly). The ring is sized to the **longest fade
  in play** and **pre-allocated** on the pre-roll side (never the cpal callback) — consistent with
  the real-time invariant. "Per-seam generation, project-wide storage."
- **Bounded look-ahead for the backward half.** To place an incoming backward handle on
  `[E − FI/2, E)` the renderer must know the seam is coming `⌈FI/2⌉` samples early; it peeks the
  cursor's upcoming `MixSlice`s far enough to cover `max ⌈FI/2⌉`. The peek is over **descriptors**
  (lazy, cheap — bounded by the number of edit boundaries in the window, not its sample length), and
  it happens **inside** the renderer, **upstream** of Step 9's ring buffer — so the ~200 ms ring
  depth does **not** cap fade length. Forward handles need no look-ahead (pure carry-forward).
- **Graceful degradation when a handle is unavailable.** Bounds come from `source_len` (origin `0`,
  EOF `source_len`); each side's available handle frames are computed **at seam detection** so the
  body fade knows its placement. Clamp each side's effective fade to the source it can actually supply:
  a forward handle past source EOF (`fwd_n = clamp(source_len − (source_start + len), 0, ⌊FO/2⌋)`), a
  backward handle before the origin (`bwd_n = clamp(source_start, 0, ⌈FI/2⌉)`, its **near-`E`** end
  kept so the ramp index starts at `⌈FI/2⌉ − bwd_n`), or a handle longer than the trimmed region is
  **shortened / zero-padded**, and the fade still completes to 0/1. When a side has **no** handle at all
  (incoming at the very start of its source; outgoing at source EOF), fall back to a **one-sided**
  (pre/post) fade running the full ramp within that side's own extent — out: `[E − FO, E)`, in:
  `[E, E + FI)`. A `RoomTone` side never degrades (the loop always supplies the full ramp). The
  renderer never reads outside a valid source range and never panics. (The M5 command layer additionally
  clamps the **stored** fade to a structural bound so the accumulator's pre-allocation stays bounded —
  see [phase1.md § M5](phase1.md#m5--editing-commands).)
- **Lazily-opened, cached source readers.** The renderer opens each track's cache/enhanced/room-tone
  reader on first use and reuses it; a `SourceProvider` trait abstracts this so tests inject in-memory
  PCM. The provider, not the renderer, owns file handles — but **no `Db`/journal access**.
- **Pull semantics for the real-time path.** `Renderer::render(&mut self, n_frames)` returns up to
  `n_frames` of interleaved stereo, advancing the cursor; at end-of-EDL it returns a short/empty
  buffer (Step 9 maps that to underrun→silence / stop). All allocation happens here on the pre-roll
  side, never in the cpal callback.

## Module surface

```rust
// audio/render.rs

/// Supplies PCM for a track's three render sources. Implementations open the
/// resampled cache / enhanced FLAC / room-tone blob; tests inject in-memory PCM.
/// No SQLite/journal access. All PCM reads are ranged; seam handles are ordinary
/// reads at offsets just outside a splice's extent (the renderer requests only
/// valid ranges and clamps short).
pub trait SourceProvider {
    /// `n` frames of the project-rate dry (resampled-cache) signal for `track_id`
    /// starting at cache offset `from`. Interleaved at the source channel count.
    fn dry(&mut self, track_id: u32, from: i64, n: i64) -> Result<Vec<f32>, AudioError>;
    /// Enhanced signal for the same range, or `None` if no enhanced file exists
    /// (⇒ the renderer uses dry; no regeneration here).
    fn enhanced(&mut self, track_id: u32, from: i64, n: i64) -> Result<Option<Vec<f32>>, AudioError>;
    /// The track's room-tone loop segment (mono or source-ch, project rate), or `None`.
    fn room_tone(&mut self, track_id: u32) -> Result<Option<&[f32]>, AudioError>;
    /// Source channel count for `track_id` (1 ⇒ up-mix to stereo).
    fn channels(&self, track_id: u32) -> u16;
    /// Per-track wet/dry mix in [0, 1].
    fn wet_ratio(&self, track_id: u32) -> f32;
    /// Total dry-cache length in frames for `track_id`. Read-only metadata (not a PCM read):
    /// the renderer clamps seam handles to `[0, source_len)` so it never reads past EOF and can
    /// detect a wholly-unavailable handle (⇒ one-sided in-extent fade) at detection time.
    fn source_len(&self, track_id: u32) -> i64;
}

/// Equal-power crossfade fade-**in** factor at ramp index `i` of `len`: `sin(π/2 · i/(len−1))`.
/// Fade-out is the complementary `cos`, so `fade_in² + fade_out² == 1` (constant power).
/// Shared with the room-tone stitch + loop fold; the engine has no linear fade-gain helper.
pub fn equal_power_gain(i: usize, len: usize) -> f32;

/// Project-wide ring of pending **seam-fade handle** contributions, indexed by absolute output
/// frame (interleaved stereo). Pre-allocated on the pre-roll side to the longest fade in play
/// (`max_fade_samples`); never allocates or grows on the pull path. Every seam on every track
/// deposits its two **handle** halves here (additively — overlapping seams sum); the render loop
/// drains the window for the frames it is about to emit, sums it into the mix, then clamps.
/// In-extent kept/own halves do **not** go here — the segment body fades them inline.
/// "Per-seam generation, project-wide storage."
struct FadeAccumulator { /* ring: Vec<f32>, cap_frames: usize, base_pos: i64 */ }

impl FadeAccumulator {
    /// Pre-allocate for the longest fade (`max_fade_samples` frames). Pre-roll only — no pull-path alloc.
    fn new(max_fade_samples: usize) -> Self;
    /// Add `frames.len() / 2` stereo frames starting at absolute output frame `at` (additive; wraps
    /// modulo capacity). Look-ahead guarantees `at` is within the live `[base_pos, base_pos + cap_frames)`.
    fn deposit(&mut self, at: i64, frames: &[f32]);
    /// Sum the pending contribution for `[from, from + n)` into `out` (interleaved stereo,
    /// `out.len() == 2 * n`), clear those cells, and advance `base_pos` to `from + n`. Empty cells add zero.
    fn drain_add(&mut self, from: i64, n: usize, out: &mut [f32]);
}

/// Pull-based renderer over an `EdlCursor` (yields `MixSlice`s) + source provider. Stereo f32 out.
/// Holds a bounded look-ahead of upcoming slices and one shared, project-wide fade accumulator.
pub struct Renderer<'a, P: SourceProvider> { /* cursor, provider, lookahead queue, fade ring, pos */ }

impl<'a, P: SourceProvider> Renderer<'a, P> {
    /// `max_fade_samples` is the structural fade bound (M5 clamps stored fades below it; Step-8 tests
    /// pass it explicitly); it sizes the accumulator's pre-allocation and the look-ahead depth.
    pub fn new(cursor: EdlCursor<'a>, provider: P, project_length: i64, max_fade_samples: usize) -> Self;
    /// Render up to `n_frames` interleaved stereo frames, advancing the cursor.
    /// Returns fewer than requested only at end-of-EDL.
    pub fn render(&mut self, n_frames: usize) -> Result<Vec<f32>, AudioError>;
}
```

## Sub-steps

Ordered so each lands a working, testable increment without retrofitting: build the **seamless**
renderer end-to-end first (8a–8b), stand up the accumulator **substrate** (8c), then layer the
**common-case** seam crossfade through it (8d), then generalize (8e) and degrade gracefully (8f).
The seam overlay sums *on top* of a renderer that is already correct without it, so nothing built
earlier is rewritten. Run each sub-step as a **separate turn**, landing an **unsigned checkpoint
commit** on `claude/1M2` (`1M2-08a` … `1M2-08f`); these squash into `main` later, so per-sub-step
commit granularity is free. Test IDs reference the master matrix below.

### 8a — per-segment **flat** render (Source / Silence / RoomTone) + up-mix + wet/dry

- Render one `EdlSegment` over its slice's `length_samples`, **no fades, no seam awareness**. Source:
  `dry` read from `source_start_sample + offset_in_splice`, blended with `enhanced` per `wet_ratio`
  (full blend; `enhanced() == None` ⇒ dry). Silence: zeros. RoomTone: loop the provider's segment
  (carry the loop phase), or zeros when `room_tone() == None`. Mono→stereo up-mix with equal gain.
- Tests: R1, R2, R4, R5, F6, F7, W15–W19, X23. Commit `1M2-08a`.

### 8b — multi-track mix + clamp + the pull loop (seamless end-to-end)

- Drive `render(n)` from the cursor's position-ordered `MixSlice` stream: render each slice's per-track
  segments over the slice span, **sum**, clamp to `[−1, 1]`, buffering across slices to satisfy the
  requested frame window. **No seams yet** — every splice edge is a hard butt-join. This is a complete,
  correct renderer for the no-crossfade case; the cursor already aligned boundaries, so it is a straight
  per-slice sum.
- Tests: R3, X20, X21, X22, C28 (no SQLite), C29 (end-of-EDL), C30 (determinism), C31 (finite). Commit `1M2-08b`.

### 8c — fade accumulator substrate (the ring)

- Implement `FadeAccumulator` (`new` / `deposit` / `drain_add`) per the Module surface: a stereo ring
  indexed by absolute output frame, pre-allocated to `max_fade_samples`, additive deposits, draining
  loop that advances `base_pos`. Wire a `drain_add` into 8b's loop **before the clamp** — a no-op until
  8d deposits (drains zero, output unchanged). Assert **no allocation on the pull path**.
- Tests: new ring unit tests (deposit/drain round-trip, **overlapping deposits sum**, wrap, advance) +
  8b matrix still green (drain is a no-op). Commit `1M2-08c`.

### 8d — symmetric centered seam crossfade through the ring (the common case)

- Detect seams locally (`offset_in_splice + length_samples == splice.length_samples`). For each seam at
  `E` with `FO == FI` and **handles available**, per *Where each half lands*: fade the segment body's
  **kept-tail / own-head inline** by distance-from-`E`; read the **forward handle** (wet/dry-blended)
  and `deposit` it fade-**out** at `[E, E + FO/2)`; read the **backward handle** and `deposit` it
  fade-**in** at `[E − FI/2, E)`, using **bounded look-ahead** (peek upcoming slice descriptors until
  cumulative length ≥ `max ⌈FI/2⌉`) so the backward deposit precedes its emission. No crossfade at
  continuation splits.
- Tests: F8 (constant power, no dip), F9 (handles read across the seam), F14 (zero fades → hard join),
  X24 (seam summed pre-clamp), C25 (fade across ≥3 tiny slices), C27 (shared accumulator). Commit `1M2-08d`.

### 8e — generalization & special seams

- Layer onto 8c/8d's substrate (little new machinery): asymmetric `FO ≠ FI` (two independent centered
  ramps, intentional bounded ripple); continuation split carries **no** crossfade and reads contiguously;
  the room-tone gap fade **is** the `RoomTone` splice's stamped fade crossfaded by the same machinery,
  with the room-tone **handle continuing the loop phase** (the only genuinely new code — carry the
  segment's running loop offset into the handle); overlapping own-track seams sum in the ring.
- Tests: F10 (room-tone gap fade + loop continuation), F11 (asymmetric), F12 (continuation split), C26
  (overlapping own-track seams). Commit `1M2-08e`.

### 8f — graceful degradation + final pass

- Clamp each side's effective fade to the handle it can supply (forward past EOF, backward before origin,
  or handle longer than the trimmed region → shorten / zero-pad, fade still completes to 0/1); a side with
  **no** handle falls back to a one-sided (pre/post) fade within its own extent. Never read out of range,
  never panic.
- Then: `cargo fmt --check`; `cargo clippy -p core -- -D warnings`; `cargo test -p core audio::`. Confirm
  [audio-pipeline.md § Room tone substitution](../design/audio-pipeline.md#room-tone-substitution),
  [§ Zero-crossing and crossfade](../design/audio-pipeline.md#zero-crossing-and-crossfade) (centered equal-power,
  fades-as-splice-data) and the wet/dry formula match; record the up-mix-equal-gain, cache-offset,
  **body-vs-accumulator overlay**, handle, shared-accumulator, and look-ahead contracts (CLAUDE.md doc-sync).
- Tests: F13 (degradation) + full matrix green. Commit `1M2-08f`.

## Test cases (for the implementer)

Inline `#[cfg(test)] mod tests` with an in-memory `SourceProvider` (fixed PCM per track, supplying
dry **and** enhanced, and source **handle** material past/before a splice's extent). Assert
**sample-exact** output where the math is exact; assert **constant-power / no-dip** where the
crossfade is symmetric. Groups: R = single-segment render, F = seam crossfades / room tone, W =
wet/dry, X = mix/clamp/up-mix, C = cross-cutting. **Fade lengths in the cases below are illustrative
values carried on the splice — never renderer constants.**

**R — single-segment render**

1. **Source segment, exact.** A `Source` segment over a track whose dry buffer is a known ramp →
   output equals that ramp (up-mixed to stereo: L == R == ramp), away from any seam.
2. **Silence segment.** A `Silence` segment → all-zero frames of the right length.
3. **Segment length honoured.** `render(n)` over a longer EDL returns exactly `n` frames; a partial
   final read returns the remainder, then empty.
4. **Cache offset respected.** A Source segment with `source_start_sample = k` reads the dry buffer
   starting at `k` (assert the first output sample == `dry[k]`), away from any seam.
5. **Stereo passthrough.** A stereo track (channels == 2) renders L/R distinct (no collapse/up-mix).

**F — seam crossfades + room tone**

6. **Room tone loops the stored segment.** A RoomTone segment longer than the provider's room-tone
   buffer repeats it (output[i] == tone[i % tone.len()] away from the seam fades).
7. **Room tone with no blob → silence.** `room_tone()` returns `None` → the RoomTone segment renders
   as zeros, no panic.
8. **Centered equal-power seam is constant-power (no dip).** A symmetric seam (`FO == FI == N`)
   between two Source splices reading **distinct** source → the seam region sums to **constant power**
   (`fade_in² + fade_out² == 1`), with the equal-power crossover centred on `E`. Assert no dip and a
   bounded max abs first-difference (no hard edge).
9. **Handles are read across the seam.** The outgoing fade reads the source **past** the splice end
   (forward handle); the incoming fade reads the source **before** `source_start_sample` (backward
   handle). Construct providers whose handle regions are distinguishable and assert both are pulled
   and ramped (the seam is not a butt-joined fade over disjoint samples).
10. **Room-tone gap fade is the splice fade (no constant).** A `RoomTone` splice with stamped
    `fade_in`/`fade_out = N` crossfades with its Source neighbours via the same machinery (no special
    50 ms behaviour): speech→room-tone pulls the source forward handle while the room-tone backward
    handle continues the loop, and vice-versa; assert continuity and loop-phase continuation into the
    handle.
11. **Asymmetric fades (`FO ≠ FI`).** Two independent centered fades, each placed correctly at `E`;
    bounded power ripple (not unity), no panic — the intentional-asymmetry case.
12. **Continuation split carries no crossfade.** One pristine splice split by a foreign track's
    boundary (`offset + len < splice.length`) renders **continuously** across the slice boundary (no
    overlay, consecutive source reads) — a faded splice resumes its ramp rather than restarting
    (cross-ref Step 7 M24/F16).
13. **Graceful degradation — short / missing handle.** A forward handle past source EOF (or a
    backward handle before source origin, or a handle longer than the trimmed region) is
    shortened/zero-padded and the fade still reaches 0/1; a side with **no** handle falls back to a
    one-sided fade within its extent. No out-of-range read, no panic.
14. **No fade when fades are 0.** A seam whose splices carry zero fades renders a hard join with no
    overlay (and no dip beyond the underlying signal).

**W — wet/dry blend**

15. **Ratio 0 == dry.** `wet_ratio = 0`, enhanced present → output == dry exactly.
16. **Ratio 1 == enhanced.** `wet_ratio = 1`, enhanced present → output == enhanced exactly.
17. **Ratio 0.5 linear.** Output == `0.5·enhanced + 0.5·dry` per sample.
18. **Enhanced absent → dry even when `wet_ratio > 0`.** `enhanced()` returns `None` with
    `wet_ratio = 0.7` → output == dry (no panic, no silence, no regeneration).
19. **Handle reads are blended too.** A Source seam's forward/backward handle is wet/dry-blended like
    the in-extent audio (ratio 1 + enhanced present → handle uses enhanced).

**X — mix / clamp / up-mix**

20. **Two overlapping tracks sum.** Tracks 1 and 2 with constant +0.3 and +0.4 over the same window →
    mixed output == +0.7 (pre-clamp).
21. **Clamp after sum.** Two tracks summing to +1.4 → output clamped to +1.0 (and −1.4 → −1.0);
    clamp is **after** the sum, not per track.
22. **Non-overlapping tracks don't double.** Where only track 1 has audio, the output is track 1's
    signal alone (track 2 contributes silence, not a doubled sample).
23. **Mono up-mix equal gain.** A mono track at +0.5 → stereo L == R == +0.5 (not ±3 dB / not 0.25).
24. **Seam contribution is summed pre-clamp.** A seam fade overlay is summed into the mix and clamped
    with everything else (a unity crossfade does not exceed the louder side's peak).

**C — cross-cutting**

25. **Fade across ≥ 3 tiny foreign slices.** A long fade on track 1 while tracks 2 & 3 fire rapid
    cuts → the seam overlay drains correctly across many short `MixSlice`s (the shared accumulator,
    not slice structure, owns the fade window).
26. **Overlapping own-track seams.** Two of a track's seams within one fade length → their overlays
    sum additively in the accumulator; no panic, graceful (sum of partial fades).
27. **Shared accumulator, not per-track.** Two tracks each with a seam in the same window produce
    identical output whether reasoned about per-track or via the single ring (additive + global
    clamp); assert against a hand-summed reference.
28. **No SQLite connection.** The `SourceProvider` exposes only PCM; a full render runs with no `Db`
    in scope (the real-time invariant precondition).
29. **End-of-EDL.** Rendering past the last segment returns empty (Step 9 maps to stop); padding to
    `project_length` yields trailing silence when requested (the export case, exercised in Step 10).
30. **Determinism.** Same cursor + provider + request → byte-identical frames, twice.
31. **All samples finite + in range.** After clamp, every output sample is finite and within
    `[−1, 1]`, including across seams and degraded handles.

## Out of scope for Step 8

- **cpal stream, ring buffer, pre-roll thread, playhead events, stop semantics** — Step 9.
- **Opening real cache/enhanced/room-tone files end-to-end + tail padding to project length** —
  this step proves the math with in-memory providers; the real `SourceProvider` impl over the FLAC
  cache (`CacheSourceProvider`) is built in **Step 9 (sub-step 9a)** and reused by Step 10 (export)
  and Step 11 (handlers). Tail padding to project length is exercised by Step 10.
- **Producing the enhanced FLAC** and **detecting/repairing a missing enhanced cache at project
  open** — M3 enhancement / open-time sweep.
- **Validating / clamping the stored crossfade length** and the room-tone-gap-fade setting — M5
  command layer ([phase1.md § M5](phase1.md#m5--editing-commands)).
- **Building the cursor / splice vecs** — Steps 6–7.
