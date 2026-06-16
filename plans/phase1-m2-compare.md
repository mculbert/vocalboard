# Opus vs Sonnet as implementer — output comparison (preliminary)

Companion to the cost study begun in `notes/implementor-test.txt`. That conversation established
the question (under a token-proportional subscription cap, which model is the cheaper *implementer*
of an Opus-written plan?) and a battery spanning the friction range, using the **per-step novelty
rating** in `phase1-m2.md` as the friction axis. Five steps were each implemented twice by each
model from the same plan, on branches `claude/<step>-{sonnet,opus}-v{1,2}`.

This doc records the **output-quality** comparison: holding cost aside, do the four runs of each
step differ in ways that affect the quality or robustness of the project? It complements the cost
table rather than repeating it.

## Cost data (Sonnet-equivalent API units; Opus ×0.6 per-token)

| Step | Novelty | Sonnet 1 | Sonnet 2 | Opus 1 | Opus 2 | Read |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 1M2-10e | low (doc-sync) | 0.33 | 0.48 | 0.49 | 0.34 | wash |
| 1M2-05  | medium (modal) | 1.09 | 1.12 | 1.65 | 1.34 | slight Sonnet |
| 1M2-06  | high | 4.70 | 5.89 | 2.60 | 2.44 | strong Opus |
| 1M2-09b | high | 2.50 | 2.89 | 1.13 | 1.19 | strong Opus |
| 1M1-11b | high (+mutation) | 7.03 | 5.50 | 5.57 | 6.28 | mixed |

Separately measured: under the Pro subscription, Opus drains quota ≈30% faster per token than
Sonnet. Even after that multiplier, Opus stays ahead on 06 and 09b.

## Method and scope

For each step I diffed all four branches against their **common base commit** and compared the
landed code — production logic, the helper structure, the test matrix, and any out-of-source-file
changes (docs, fixtures, settings). I read the production code in full for 05/06/09b and the
load-bearing `apply_batch` for 11b; I focused on *behavioural/robustness* differences and ignored
incidental naming, formatting, doc-comment wording, and equivalent idiom choices, per the request.

Common bases: 10e `d2d6b83`, 05 `84cd43e`, 06 `7eb2ab1`, 09b `ef9dfe6`, 11b `e5fdace`.

**The plans were revised after the experiment — findings are verified against the *original*
plans.** The plan docs on `claude/1M2` reflect revisions made *after* all these steps were first
implemented; the experiment ran against the *original* plans (the versions at each experiment
branch's base commit, which the experiment branches did not themselves modify). Every "deviation
from plan" below was re-checked against the original plan text the implementers actually received
(`git show <base>:design/<plan>.md`), so the verdicts distinguish a real implementer choice from a
mere difference between the revised and original plan. The places that distinction *changed* a
finding are called out inline.

**Limitation.** I compared the code *as committed*; I did not re-run all 20 builds/test suites in
this pass. Each branch is a landed commit under the repo's `clippy -D warnings` + `missing_docs`
gates with its test module present, so all four are green-by-construction; I have not independently
re-verified mutation scores or wall-clock test results. "Green-but-wrong" risks are assessed by
reading, not by re-execution.

## Headline finding

**On these five steps the two models produced output of equivalent quality.** There is no task
where Opus shipped a materially more correct or more robust implementation than Sonnet, or vice
versa, on the *core deliverable*. Both got the hard, CI-uncatchable invariants right (the
real-time drain contract in 09b; descending-order + inverse capture in 11b; the min-energy
fallback and frame-aligned search in 05). Where the four runs diverge, it is at the **margins**:
allocation hygiene, encapsulation, defensive coding, and — most consistently — **process
discipline** (doc-sync, single-pass completeness). Those margins lean slightly toward Opus, but
none is large enough to override the cost signal.

After re-checking against the original plans, the equivalence is *cleaner* than a first read
suggested: on the two high-novelty steps (06, 09b) all four runs **faithfully and correctly
implemented the original plan** — the apparent "shared deviations" I first flagged (Source-only
coalescing, no `debug_assert`, project-rate ring, no device-rate negotiation) were all *in the
original plans* and only look like deviations against the later-revised docs. So 06 and 09b are
clean "equivalent quality, Opus ≈2× cheaper" results with no quality asterisk.

The practical upshot: **the decision is driven by the cost table, not by output quality.** The
quality findings argue only that standardizing on Opus does not *sacrifice* quality on the modal
and high-novelty work — and modestly *improves* process adherence — so "cheaper where it matters,
no quality regression" is the supported reading.

## Per-step findings

### 1M2-10e — doc-sync (floor case): wash

All four edited the same two lines of `audio-pipeline.md` and conveyed the same substantive facts
(FLAC 24-bit default, WAV f32le, ffmpeg for mp3/ogg/aac, "extension wins", `export_unsupported_format`).
Differences are pure verbosity. Opus-v1 was the most detailed (named the streaming `write_frames`
+ `finalize` sink and the `encode_flac_24` reuse); opus-v2 alone kept the vaguer "return an error"
in the last sentence rather than the specific error code. Immaterial. **Confirms the cost wash.**

### 1M2-05 — zero-crossing search + crossfade (modal case): near-wash, one minor blemish

All four are algorithmically identical and correct: same threshold formula
`max(0.001, min(2·rms, ceiling))`, same first-below-threshold scan with min-energy fallback within
the clamped window, same `frames_from_ms` rounding, same inline `crossfade_gain`, same
`ZeroCrossingParams::from_settings`. Test matrices are comparable (20–22 cases, full Z/C/X
coverage). None touched `settings.rs` — correct, the `splice_*_ms` keys already existed at the
base.

One real difference:

- **Sonnet-v1 uses `Box<dyn Iterator>` to pick the scan direction** inside `refine_boundary` — a
  heap allocation + dynamic dispatch on a function the plan explicitly designs to be
  allocation-free (test 18, "no allocation anywhere", is review-asserted, so tests pass anyway).
  The other three (incl. sonnet-v2) use an offset/index loop with no allocation. This is exactly
  the class of green-but-slightly-wrong-on-an-uncatchable-property the cost study anticipated — but
  here it landed on a *Sonnet* run, is minor, and the other Sonnet run avoided it, so it is noise,
  not a model signal.
- Numerical nit: sonnet-v1 accumulates the RMS sum in `f32`; the other three use `f64`. Negligible
  at these window sizes.

Verdict: quality wash. Consistent with the cost read (05 slightly favored Sonnet) reflecting
efficiency, not a quality gap.

### 1M2-06 — splice subdivision + merge (high novelty): faithful and equivalent

All four: 37 tests (matching the 37-case matrix), correct `Word` rename
(`turn_offset_sample` → `source_onset_sample: Option<i64>`) with regenerated pinned wire-bytes/hash
fixtures (the prerequisite), correct forward cut/mute head/tail trimming + source re-basing +
seam-fade stamping, and `Source`+`Source` coalescing for the round-trip property.

> **Correction after checking the original plan.** My first read flagged three "shared deviations"
> — all four naming the helper `coalesce_sources` and coalescing **only** `Source`+`Source` (not
> adjacent `RoomTone`/`Silence`), none using `debug_assert!` preconditions, and none splitting on an
> interior `start`. **All three are wrong as deviations.** The original `phase1-m2-06.md`
> (`7eb2ab1`) specifies exactly that: it names the `coalesce_sources` helper, states "**`coalesce_sources`
> merges adjacent `Source` splices that are source-contiguous**" and "**Only `Source` splices are
> subdivided/coalesced**," and contains no `push_coalesced`, no `debug_assert`, no
> generalized-coalescing "Revision (review)" note, and no interior-`start`/"need not be a boundary"
> language. Those were all added in the **post-experiment revision**. So **all four runs implemented
> the original plan faithfully and correctly** — there is no deviation to attribute to either model.

The one residual inter-run nuance: for `merge_on_uncut`, three runs (sonnet-v1, sonnet-v2, opus-v1)
treat a non-boundary `start` as a **safe no-op**, while **opus-v2** uses a `split_point` that
inserts at the nearest following boundary (which on a non-boundary `start` would misplace rather
than no-op). The original plan only contemplates a boundary `start` for uncut, so this differs only
on out-of-contract input — a marginal robustness detail, not a plan deviation, and the only place
the four diverge at all.

Net: no quality difference; all four are faithful, plan-correct implementations. Opus's ~2× cost
advantage here is pure efficiency on equivalent-quality, equally-faithful output.

### 1M2-09b — ring + Backend + in-memory backend + no-alloc contract (high novelty): equivalent

All four: same `Shared { frames_played: AtomicU64, playing: AtomicBool }`, same
`Backend { start }` trait + `InMemoryBackend` + synchronous `pull`, same RING_MS ring split at
`PlaybackEngine::new`. Crucially, **all four implement the CI-uncatchable drain contract
correctly**: copy real frames and advance `frames_played` only for real frames when `playing`;
pop-and-discard (flush) without advancing when not `playing`; pad the remainder with silence on
underrun. The "cheaper model ships green-but-wrong on the RT invariant" risk **did not
materialize** for either model.

All four implemented 9b with `Backend { start }` only (no `negotiate`), `new(sample_rate, kind)`
with **no `quality` parameter**, and the ring sized at the **project** rate. I confirmed against the
original `phase1-m2-09.md` (`ef9dfe6`) that this is exactly the original 9b scope: it sizes the ring
"at the **project rate**," defines 9b as "`Backend` trait + ring + in-memory backend (with `pull`)
+ drain contract … **No cpal yet**," and contains no `negotiate`/`quality`/`device_rate` — the
two-phase device-rate negotiation was added in the **post-experiment revision**. So all four are
fully faithful to the original; the only thing they "omit" is work the original plan placed in a
later sub-step.

Marginal difference: **sonnet-v1 is the most defensive** (11 tests vs 7; masks odd-sample
remainders with `& !1`; degrades to silence if `read_chunk` unexpectedly errors, with comments
explaining the RT-safety reasoning). The opus runs and sonnet-v2 are more compact (7 tests each).
This is a robustness point *for* a Sonnet run, again showing the within-model spread rivals the
between-model spread.

### 1M1-11b — `apply_batch` producer + mutation testing (high novelty): the one place process discipline diverges

Core logic is equivalent across all four: `apply_batch` sorts the batch **descending by sample over
original-tree coordinates**, captures forward+inverse deltas, writes the blob+delta batch
atomically, swaps `current`, and records the inverse on the undo stack. Inline test counts are
comparable (9–10). This is the M1 producer that sharply discriminated the *cheaper* models in the
`phase1-m1-11-compare.md` study; **neither Opus nor Sonnet got the novel logic wrong.**

Three differences here *do* favor Opus, and they are about discipline rather than the algorithm.
All three were re-checked against the original `phase1-m1-11.md` (`e5fdace`) and hold:

1. **Single-pass completeness (mutation testing).** The original plan explicitly requires it — Step
   11b "gets its own commit **and focused mutation testing**," with "`cargo-mutants` scoped to
   `apply_batch` (ordering + delta/inverse capture)." **Both Opus runs delivered it in one commit;
   both Sonnet runs shipped the producer first and only added the mutation pass after a second
   prompt** (`f7e969a`→`e97560d`, `da61d44`→`b9db9fe`). Sonnet systematically deferred a
   *plan-mandated* deliverable that Opus completed inline. (This is also why 11b's costs are noisy —
   the Sonnet figures bundle a second human round-trip.)

2. **Downstream doc-sync (a standing CLAUDE.md invariant).** This one is *not* spelled out in the
   step plan — it comes from the standing CLAUDE.md rule that a deliberate shortcut/stub must
   immediately update the affected downstream `design/phase*.md`. `apply_batch` is `pub(crate)` and
   dead in the lib-only build until M5 wires it — a deliberate deferred-wiring stub. **Both Opus runs
   honored the invariant**, editing `phase1-m1-11.md` to document the deliberate `#[allow(dead_code)]`
   gap and when the allows come off, and `phase1.md` to record that M5 must call `apply_batch` as the
   producer (opus-v1 also `.gitignore`d the mutant artifacts). **Neither Sonnet run touched the
   design docs.** Because the obligation lives in CLAUDE.md rather than the step plan, this measures
   adherence to standing project norms under load — and there Opus was consistently better across
   both runs.

3. **Encapsulation under lint pressure.** The original plan explicitly specifies `apply_batch` as
   `pub(crate)` ("only tests/M5 call it in M1"). To satisfy the lib-only `clippy -D warnings`
   dead-code gate, sonnet-v1, opus-v1, opus-v2 kept that visibility and used targeted
   `#[allow(dead_code)]` (9 / 12 / 14 allows — more allows = more granular, honest marking).
   **sonnet-v2 instead widened `apply_batch` to fully `pub`** (0 allows), silencing the lint by
   making the whole call chain reachable — but exposing an internal mutation producer as crate-public
   API, against both the spec'd visibility and the "command surface is the only way to mutate state"
   intent. A linter-driven shortcut, not a design choice, and a genuine deviation from the original
   plan's explicit `pub(crate)`.

Offsetting these, **Sonnet wrote more test code** (sonnet-v2 added a 274-line `engine_lifecycle.rs`
and expanded `engine_recovery.rs`). So Sonnet was not *less* thorough on coverage — it was less
disciplined about completing the planned non-code deliverables in one pass and about respecting the
spec'd visibility/doc obligations.

## Cross-cutting observations

- **Within-model variance rivals between-model variance** on quality. The most-defensive run (09b
  sonnet-v1, 11 tests) and the least-defensive interior-`start` handling (06 opus-v2) are both
  *v-specific*, not model-specific. With n=2 the quality signal is mostly noise except where it is
  *systematic across both runs of a model* — which is the bar the three 11b items clear (both Opus
  runs single-pass + doc-synced; both Sonnet runs deferred mutation + skipped doc-sync).
- **Both models implement faithfully.** Every apparent "deviation" on the high-novelty steps
  dissolved once checked against the original plans — all four tracked the original 06/09b specs
  exactly. The real signal is not *whether* they follow the plan (both do) but *how completely they
  honor the surrounding process obligations* (11b mutation pass + CLAUDE.md doc-sync), where Opus
  was systematically more thorough.
- **Cost, not core-code quality, is the deciding variable** on 06 and 09b: equivalent, plan-faithful
  output, Opus ~2× cheaper. The only systematic quality/process edge (11b discipline) also favors
  Opus. Nothing in the outputs argues for keeping the high-novelty work on Sonnet.

## Expanding the modal-task sample (Case B)

1M2-05 is the lone medium-novelty data point and it leaned slightly Sonnet on cost. Because
medium-novelty work is the modal unit for this project, one point is too thin to conclude. Using the
same diagnostic as the original battery — **medium per-step novelty rating + a clean replay seam +
pure-ish functions with a bounded edge surface + a sharp correctness oracle + session-bounded** —
here are the best additional candidates, all from M2 (M1's remaining steps are either
producer/consumer pairs already sampled by 11a/11b, or mechanical plumbing). Each forks cleanly from
its predecessor's commit and has a self-contained sub-step plan, so the replay protocol is identical
to the existing battery.

### Recommended (in priority order)

1. **Step 4 — Room-tone detection (`phase1-m2-04.md`).** The nearest *sibling* to 05: a
   multi-criterion DSP detection over PCM in the **same RMS/threshold/window family**, with a richer
   bounded edge surface (block-RMS scan, quiet-percentile threshold, the peak ≤ 5× and SD ≤ 15%
   acceptance gates, the stitch fallback, the three crossfade tiers) and a **sharp oracle**
   (per-branch synthetic signals + pinned-bytes/hash for the RoomTone V1 blob). *Caveat / why it's
   listed first despite a confound:* it is rated **medium-high** because it also lands a persisted V1
   blob (pinned-bytes + G1 round-trip = the format-discipline friction that already favored Opus in
   06/11b). It therefore partly tests the persistence axis, not the pure-DSP axis. Run it, but read
   its result as "medium DSP **with** a format-discipline tail," and attribute any Opus edge
   accordingly.

2. **Step 10 — transcript formatters, VTT + Markdown (`phase1-m2-10.md`).** The best *purity* match
   and deliberately **off the DSP axis**: pure string-transform functions with a real bounded edge
   surface (timestamp formatting, `include_cut_words`, escaping, segment boundaries, extension
   routing) and a **sharp pinned-expected-string oracle**. Tree access is read-only input. This is
   the cleanest test of whether the 05 trade-off **generalizes beyond numeric DSP** to ordinary
   pure logic — the most decision-relevant of the three, since most "modal" work in later milestones
   is logic, not DSP. (Scope it to the transcript half; the audio-export half pulls in the
   renderer/encoder-sink plumbing and is less pure.)

3. **Step 3 — resample core (`resample.rs` / `flac.rs`, `phase1-m2-03.md`).** A second pure-DSP
   triangulation point: rubato resample with an **identity bit-exact fast-path** and a FLAC
   encode→decode round-trip within the 24-bit quantization bound — both sharp oracles. *Scope it to
   the DSP* (resample + FLAC); the `cache.rs` `ensure_resampled` half is I/O/content-addressing
   plumbing closer to the floor case and would dilute the signal if bundled.

Together these give two DSP points (Steps 4, 3) plus one non-DSP logic point (Step 10) to set
against 05 — enough to tell a 05 fluke from a real medium-band trade-off, and to check whether the
trade-off is DSP-specific.

### Considered and not recommended

- **Step 2 (decode + probe)** and **Step 11 (Tauri wiring)** are medium-rated but **mechanical** —
  I/O-bound decoding and pattern-following plumbing respectively. They sit near the 10e floor
  (low friction, tie-prone), so they would re-measure the doc-sync result rather than the modal
  middle. Skip unless you want a second floor reading.

## Methodology refinements for the next runs

- **Hold effort pinned and equal** (per the original study), and keep n≥2; if a medium case comes
  back within ~15% on cost, add runs there specifically — the medium band is where the decision
  actually hinges and where variance is highest relative to the gap.
- **Score the process deliverables, not just the code.** 11b showed the clearest model signal is
  *completeness in one pass* (mutation testing) and *doc-sync adherence* — both invisible if you
  only diff the producer. For each new case, check explicitly: did the run complete every planned
  deliverable without a second prompt, and did it perform the CLAUDE.md downstream doc-sync?
- **Pin the plan version per run, and verify code against *that* version.** This study's first-pass
  06/09b findings were spurious because the on-branch (`claude/1M2`) plans had been revised after the
  experiment; the conclusions only held up after re-reading the original plan at each branch's base
  commit. The experiment itself correctly ran each model against the same original plan — so when
  scoring future runs, always diff the implementation against `git show <base>:design/<plan>.md`, not
  the current doc, or apparent "deviations" will just be later plan edits.

## Case B results — medium-band triangulation (Steps 3, 4, 10d)

The three recommended medium-novelty steps were each run twice per model. Output was not
re-diffed (the Case A finding — equivalent code, divergence only at the margins — was taken as
established for this band); this pass is **cost-only**.

| Step | Sonnet 1 | Sonnet 2 | Opus 1 | Opus 2 | Sonnet avg | Opus avg | Opus premium |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 1M2-03  | 3.78 | 3.54 | 4.20 | 3.60 | 3.66 | 3.90 | +6.6% |
| 1M2-04  | 2.90 | 3.20 | 3.14 | 3.67 | 3.05 | 3.41 | +11.6% |
| 1M2-10d | 1.44 | 1.20 | 1.52 | 1.49 | 1.32 | 1.51 | +14.0% |

Pooled, Opus runs ~10% more tokens than Sonnet on these three (mean-of-ratios ~11%; the ~13%
quoted in discussion is the same ballpark).

**Read of the data:**

- **The verbosity gap is real but its magnitude is noisy.** Opus is higher on all 3/3 steps,
  matching 05 (4/4 medium points now lean the same direction), so the *direction* is signal. But
  within-model run-to-run spread is 2–18% (Opus on 03: 4.20 vs 3.60, a 17% swing between identical
  runs), which rivals the ~10% between-model gap — consistent with Case A's "within-model variance
  rivals between-model variance." "~10–13% more verbose" is the supportable claim; nothing tighter.
- **On the modal task in isolation, the quota premium is ~45%, not ~13%.** The ~13% is raw tokens;
  under the Pro subscription Opus also drains quota ≈30% faster *per token*. These compound:
  1.13 × 1.30 ≈ **1.47× quota per medium step**. The medium band is the one place Sonnet genuinely
  leads, and the lead is larger in the unit that binds (quota) than in raw verbosity.
- **Two offsets erode that lead, and both are uncounted on the table above.** (1) The table charges
  best-case single-pass Sonnet. On 11b, *both* Sonnet runs needed a second prompt to deliver the
  plan-mandated mutation pass and both skipped the CLAUDE.md doc-sync — a human round-trip the cost
  figures never charged. Any step with a process tail re-incurs that. (2) Sonnet-targeted plans must
  be spec-grade, and that depth is paid in *Opus-planner* tokens (the expensive ones); defaulting
  the implementer to Opus lets plans thin out, recovering planning quota the implementation table
  doesn't see. Net the 1.47× medium-band premium against these plus the ~2× Opus *win* on the
  high-novelty band (06, 09b), and the blended portfolio cost is flat-to-favorable for Opus.

## Take-home conclusion

**Standardize on Opus as the default implementer.** Across both Case A and Case B, Opus ties or
wins on cost everywhere except the raw-token medium band, and even there the lead (a) is noisy at
n=2, (b) shrinks once plan-authoring tokens and Sonnet's occasional second-prompt round-trips are
counted, and (c) is dominated by Opus's ~2× advantage on the high-novelty work that drives
milestone cost. On output, Opus never regressed core quality and was systematically better on
process discipline (single-pass completeness, doc-sync) — which compounds on a convention-heavy
project (mutation-testing norms, doc-sync invariants, the command-surface rule). A single default
also removes the novelty-rating routing risk: mis-rate a step under the old split and you mis-assign
the model.

**Optional hedge, only if quota becomes the binding constraint mid-session:** keep Sonnet for the
*floor* cases — doc-sync (10e-style) and mechanical/I-O plumbing (Steps 2, 11) — where Case A/B
showed a cost-and-quality wash, so the 1.47× multiplier buys nothing. This is a carve-out for the
cheapest, lowest-fidelity-risk work, not a reason to keep Sonnet as the default. If the cap is not
routinely binding, go Opus-everywhere for the simplicity.
