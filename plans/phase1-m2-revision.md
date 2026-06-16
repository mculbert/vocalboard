# Phase 1 — M2 Revision: streaming pipeline, schema cleanup, room-tone naming

Status: **Commits 1–6 complete** (second review follow-ups landed).
Cross-cutting revision of landed M2 Steps 2–5 (decode, resample, FLAC cache, room tone,
splice primitives), triggered by a review of the early M2 work. This runs **before** the
M4 import/render wiring consumes those signatures, so changing them now is cheap.

A **second review** of the landed streaming work (Commits 1–2) added four follow-ups,
planned below as Commits 3–6: a `FrameReader` rename, a streaming ffmpeg fallback,
demoting whole-buffer decode to test support (+ DRYing the triplicated Symphonia packet
pump), and finishing the deferred **output-streaming** of the FLAC encode.

> **Plan-doc convention note.** This is the first *revision* plan (a change that
> re-opens already-landed steps rather than advancing to the next one). It is named
> `phase1-m2-revision.md` rather than `phase1-m2-NN.md` to signal that. Future
> cross-cutting revisions should follow the `phase1-mX-revision.md` pattern.

## Why

1. **Memory.** The import path (`ensure_resampled`) holds ~3 full-length copies of the
   audio at once: `decode()` → full `Vec<f32>`, `resample()` → full `Vec<f32>`, and
   `encode_flac_24()` → full `Vec<i32>` + full encoded byte buffer. Room-tone detection
   holds two more (the input `samples` and the `mono` down-mix). For an hour of stereo
   48 kHz that is multiple GB resident to push audio that is just going back to disk —
   unacceptable for the audiobook/podcast target.
2. **Redundant persisted fields.** `TrackMeta.resampled_path`, `enhanced_path`, and
   `room_tone_length_samples` are all derivable (path from `track_id` by convention;
   length from the room-tone blob), so they only add drift surface and migration weight.
3. **Naming drift.** The persisted room-tone type is spelled three different ways
   (`RoomToneSegment` in memory, `RoomTonePcmV1` on the wire, `Kind::RoomTonePcm`), and
   `RoomTone` names the *detection outcome* enum rather than the data.
4. **Stale field.** `SpliceKind::Source.source_decode_offset` is a vestige of the
   abandoned decode-from-compressed-source-on-the-fly design; with the transcode-to-FLAC
   cache it is read mechanics that the renderer recomputes at read time, not edit state.
5. **No in-memory room tone.** Nothing holds decoded room-tone PCM, so the renderer would
   have to re-read the store per splice.

## Sequencing

First review — two commits, split on the streaming/cleanup fault line:

- **Commit 1 — streaming** (largest, riskiest; validated against the *current* format and
  names so the format/name churn does not muddy the diff). Split in two for reviewability:
  - **1a — streaming transcode** (§1.1 `transcode_to_cache` + §1.2 `StreamingResampler`,
    wiring `cache.rs`). **✅ landed.**
  - **1b — streaming room-tone** (§1.3 `FrameReader` + §1.4 streaming `detect_room_tone`).
    **✅ landed.**
- **Commit 2 — cleanup** (naming rename, redundant-field drops, `source_decode_offset`
  drop, `ProjectState` room tones, migration, design-doc updates). **✅ landed.**

Second review — four independent commits (each buildable, tests green), see
[Second review — Commits 3–6](#second-review--commits-3-6):

- **Commit 3 — rename** `FlacFrameReader` → `SymphoniaFrameReader` (§3). **✅ landed.**
- **Commit 4 — streaming ffmpeg fallback** (`FfmpegSource: PcmSource`, §4). **✅ landed.**
- **Commit 5 — shared Symphonia packet pump + demote whole-buffer decode to test
  support + streaming length count** (§5). **✅ landed.**
- **Commit 6 — output-streaming FLAC encode** (per-frame writer, §6). **✅ landed.**

**Ordering rationale.** 3 is independent and tiny (do it first so 5's pump-extraction
works on the final name). 4 precedes 5 because 5's demotion of whole-buffer decode can
only remove `decode_via_ffmpeg` from production once `FfmpegSource` has replaced the
`BufferedSource` fallback in `open_source`. 6 is independent of 3–5 (it touches the
encode side) and is sequenced last as the largest/riskiest.

Commit 1 is written using the *current* names (`RoomToneSegment`, `Kind::RoomTonePcm`);
Commit 2 renames them. Reviewers of Commit 1 should expect the old names.

Only `cache.rs::ensure_resampled` consumes the decode→resample→encode trio (confirmed by
grep), so Commit 1 is well contained.

> **Implementation notes (1a, landed).** flacenc streams the *input* via a custom `Source`
> as planned; the resampler's whole-buffer reference path (`resample`) turned out to leave a
> ~`output_delay()`-frame startup *stutter* (rubato `process_all_into_buffer` →
> `copy_frames_within` only shifts `delay` frames), which the R-tests' margins never caught.
> The streaming `StreamingResampler` trims cleanly, so it is **not** bit-identical to
> `resample` over the leading `output_delay` frames — it is *more* correct. The cache (now
> produced by the streaming path) is glitch-free; `resample` is retained only for tests. S1–S3
> therefore pin the streamed output's exact length, smoothness, frequency, and determinism
> rather than a sample-for-sample match. (The pre-existing `resample` artifact is cosmetic and
> now unused in production; left as-is.)

> **Implementation notes (1b, landed).** `FrameReader` (`audio/frame_reader.rs`) ships with both
> impls — `SliceFrameReader` (test/in-memory) and `SymphoniaFrameReader` (Symphonia, prod; renamed
> from `FlacFrameReader` in Commit 3) — plus a
> default `read_range` (seek + read-exactly-N) used by the room-tone pass 2. `detect_room_tone`
> now takes `&mut impl FrameReader` and returns `Result<RoomTone, AudioError>` (reads can fail);
> the in-memory test path unwraps via a `detect()` helper. Pass-1 block stats are bit-identical
> to the prior whole-buffer math (same per-frame down-mix and summation order), so the D/L/B tests
> and the pinned `RoomTonePcm` blob are unchanged.
>
> **Unplanned but required: flacenc → Symphonia FLAC compatibility fix.** 1b is the first code to
> actually *read* the FLAC cache with Symphonia (the prod `FrameReader`), which surfaced a latent
> bug: flacenc 0.5.1 lowers `STREAMINFO.min_block_size` to the (short) final frame, so an
> otherwise fixed-block stream reports `min != max`. Symphonia 0.6 then treats it as a
> variable-blocksize stream and rejects every fixed-coded frame during resync (manifests as
> `UnexpectedEof` → `UnsupportedFormat`). This had been masked all along by `decode()`'s ffmpeg
> fallback — every "FLAC decode" in the prior steps silently went through ffmpeg. Fix:
> `flac::normalize_fixed_block_size` rewrites `min_block_size = max_block_size` before
> `stream.write` (libFLAC/ffmpeg do the same — the final short frame is a permitted exception;
> STREAMINFO carries no CRC, so the rewrite is safe). Applied at both encode sites
> (`flac::encode_flac_24`, `transcode::transcode_to_cache`). The cache now decodes natively via
> Symphonia (no ffmpeg dependency for reads). Cache bytes have no pinned constants, so determinism
> tests (TC3/C17/E23) remain self-consistent.

---

## Commit 1 — streaming transcode + streaming room-tone detection

### 1.1 `transcode_to_cache` orchestrator (`audio/cache.rs`, new `audio/transcode.rs`)

Replace the internals of `ensure_resampled` with a streaming orchestrator:

```
transcode_to_cache(source: &Path, out: &Path, project_rate: u32,
                   quality: ResamplingQuality) -> Result<i64 /* out frames */, AudioError>
```

Pipeline, each stage pulling on demand:

1. **Decode source → f32 chunks.** Symphonia packet pull (the existing decode loop, but
   *yielding* each packet's interleaved f32 instead of accumulating). The ffmpeg fallback
   is a subprocess pipe — read raw PCM in fixed chunks. Both backends expose the same
   "next chunk of interleaved f32 at source rate/channels" interface.
2. **Streaming resampler** (see 1.2) — feed source chunks, pull resampled chunks at
   project rate.
3. **flacenc `Source` impl** that *pulls* resampled chunks and converts f32 → clamped
   int24 on the fly (the clamp currently in `encode_flac_24`). This removes the full
   `Vec<i32>`. Drive `flacenc::encode_with_fixed_block_size(&config, source, 4096)`.

`ensure_resampled` keeps its current signature and `CacheOutcome`
(`relative_path`, `regenerated`, `length_samples`). `length_samples` comes from the
streaming encoder's running output-frame count (or a `probe` of the written file).

**flacenc API (verified, 0.5.1).** `Source` is a pull trait:
`fn read_samples<F: Fill>(&mut self, block_size, dest: &mut F) -> Result<usize>`, where the
impl calls `dest.fill_interleaved(&[i32])` and returns the per-channel frame count;
plus `channels()` / `bits_per_sample()` (= 24) / `sample_rate()` and `len_hint() -> None`
(unknown length is fine — the encoder then sets `total_samples` from the running count).
`encode_with_fixed_block_size`'s loop calls `read_samples` for one `block_size` `FrameBuf`
at a time, so **the input PCM is never materialized** — the dominant memory win, via the
existing high-level entry point.

**Output stays in RAM (interim floor — superseded by Commit 6).** `encode_with_fixed_block_size`
accumulates a `Vec<Frame>` in the returned `Stream` before `write`. **Correction (second
review):** the original estimate here ("≈ compressed FLAC size, ~0.4–0.7 GB") was wrong.
Each `Frame`'s `Residual` stores `quotients: Vec<u32>` **and** `remainders: Vec<u32>` —
one `u32` *each* per sample (~8 bytes/sample/channel) — so the in-RAM `Stream` is ≈ **2× the
f32 PCM** (≈ 2.8 GB for 1-hour stereo, ~8 GB for a 3-hour lecture), *larger* than the f32
buffer the input-streaming removed; with the default `par` feature the parallel path can
additionally populate `precomputed_bitstream` per frame. The input-streaming change still
helped (it removed the duplicate f32/i32 copies), but this floor is higher than recorded.
**Commit 6** removes it with true output-streaming (O(one block)).

**Determinism.** The cache is derived and has **no committed pinned bytes**, so E23
(deterministic regeneration) and C17 (byte-identical re-encode) require only
*self-consistency* — fixed block size 4096 + the unchanged int24 clamp/scale give that.
Matching today's whole-buffer `encode_flac_24` byte-for-byte is a convenient correctness
*oracle*, not a hard requirement — add it as a test where it holds; otherwise rely on the
resampler R-tests for quality.

**Keep** thin whole-buffer `decode()` / `resample()` for tests and any non-streaming need.

### 1.2 Streaming resampler (`audio/resample.rs`)

Introduce a stateful, pull-based streaming resampler wrapping `rubato::Async` +
`FixedAsync::Input` (same `sinc_params` presets). `process_all_into_buffer` (what the
current code uses) is a **default trait method** in rubato 3.0.0; its body is the exact
recipe to replicate for streaming:

- Drive `process_into_buffer(buf_in, buf_out, Some(&Indexing { input_offset,
  output_offset, active_channels_mask: None, partial_len }))`.
- **Full chunks** (`partial_len = None`) while a full `input_frames_next()` (= 1024) chunk
  is buffered *and more input is still coming*.
- **Final chunk** at decode-EOF via `partial_len = Some(remaining)` — note this path is
  taken **even when the last chunk is exactly full-size** (the trait's main loop is
  `while frames_left > input_frames_next()`, strictly greater, so an exact multiple sends
  its last 1024 through the partial path). Then **flush** with `partial_len = Some(0)`
  until cumulative output reaches `ceil(ratio · total_in)`.
- **Leading-delay trim:** discard the first `output_delay()` output frames as they emerge
  (the startup silence), then forward the rest, **capping** forwarded output at
  `ceil(ratio · total_in)`.

Because the transcode pulls (flacenc's `Source` asks for ≤ `block_size` frames), the
wrapper buffers resampler output and serves it on demand, feeding input chunks as needed.
`total_in` is known only at decode-EOF — fine, the final-chunk + flush + length-cap all
happen there.

- **Correctness bar = the resampler R-tests** (R1–R11: identity fast-path, up/down/
  non-integer ratios, channel preservation, length, determinism) run against the *streamed*
  output. Matching `process_all_into_buffer` bit-for-bit is the convenient oracle — add a
  `streamed == resample(whole)` equivalence test where it holds (watch the exact-multiple
  boundary above), but it is not load-bearing since the cache has no pinned bytes.

> **Risk.** Most error-prone part of Commit 1, concentrated in the final-chunk/flush
> boundary and the delay-trim/length-cap bookkeeping. The R-tests + equivalence test catch
> divergence.

### 1.3 `FrameReader` abstraction (`audio/` new module)

A small trait shared by room-tone detection now and the renderer/export later:

- `read_frames(&mut self, out: &mut [f32]) -> usize` — sequential pull at project
  rate/channels.
- `seek_to_frame(&mut self, frame: i64)` + range read — for targeted reads.

Impls:
- **FLAC-backed (prod):** Symphonia FLAC reader. **Verified (0.6):**
  `format.seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id })` returns
  `SeekedTo { required_ts, actual_ts }`; the FLAC demuxer coarse-seeks to the nearest frame
  boundary `≤ ts` (seektable index + binary search), so `actual_ts ≤ required_ts` and the
  reader **decodes forward and discards `required_ts − actual_ts` frames** to land
  sample-accurate. (Same seek+discard pattern the renderer uses for `SpliceKind::Source` —
  Commit 2.3.)
- **In-memory slice (test):** wraps a `&[f32]`; trivially seekable. Keeps room-tone tests
  fast and codec-free. Seek/discard correctness is tested at the decode layer, not in
  every room-tone test.

### 1.4 Streaming room-tone detection (`audio/room_tone.rs`)

Change `detect_room_tone` to take `&mut impl FrameReader` (rewindable: pass 1 sequential,
pass 2 seeked) instead of `&[f32]`.

**Pass 1 — single streaming read:**
- Pull frames; compute the mono down-mix sample *per frame on the fly* (no `mono` Vec).
- Accumulate into the current 100 ms block (running sum-of-squares for energy, running
  max-abs for peak). On each block boundary finalize `(energy, peak, rms)` → flat arrays.
- **Retain all block stats.** They are scalars: ~430 KB/hr, ~10 MB for 24 h — negligible
  vs. the PCM. The percentile `Q` is a *global* statistic over every block's RMS, so all
  `block_rms` must be retained regardless; an approximate streaming percentile is rejected
  (it would break determinism). The expensive thing we eliminate is the *samples*, not the
  stats.

**Detection math — unchanged.** Build the prefix sums + sparse table over the flat arrays
and run the existing window-sweep / percentile / stitch-selection logic verbatim. Keeping
the global prefix-sum arrays preserves the exact floating-point summation order, so the
selected window is **bit-identical** to today (preserves the D-tests and the pinned blob).
Do **not** rewrite the sweep as a bounded ring — it saves ~MB and risks changing results.

- Primary window sweep uses only the fixed `rms_ceiling` (never `Q`) → find best window.
- **Found:** pass 2 — `seek_to_frame(start_frame)`, read the window's frames → existing
  loop crossfade.
- **Not found:** compute `Q` (percentile over retained `block_rms`); the stitch set
  (`rms ≤ Q ∧ peak ≤ 5·rms`, then quietest-up-to-10 s, then sorted by position) is a
  **pure in-memory filter** — no file read to *decide* it. Pass 2 — targeted seeked reads
  of just the selected blocks (coalesce adjacent) → existing `crossfade_concat` → loop
  crossfade.

**Stay decoupled from the transcode.** Do *not* tee the resampler stream to compute pass-1
stats inline. The canonical analysis input is the **post-FLAC cache** bytes (exactly what
playback loops); teeing the pre-quantization resampler output diverges at the ~1e-7 level
and muddies "deterministic." A separate streaming read of the cache (cheap 24-bit FLAC
decode) keeps the modules independent and the input canonical.

### Commit 1 validation

- All existing `audio/` tests green (room-tone D/L/B tests adapted to the in-memory
  `FrameReader`; output bit-identical, so pinned blob B19/B20/B25 unchanged).
- New: streamed-resample R-tests (R1–R11 on streamed output) + `streamed == resample(whole)`
  equivalence where it holds; streamed-transcode == current `encode_flac_24` output (oracle);
  `FrameReader` seek/discard correctness (FLAC impl).
- `cargo fmt`, `clippy -D warnings`, `cargo test --workspace`.

### API findings (verified against vendored sources)

Resolved up front so implementation does not stall on exploration:

- **flacenc 0.5.1** — input streams via the `Source` pull trait (`read_samples` →
  `dest.fill_interleaved(&[i32])`), so the high-level `encode_with_fixed_block_size` never
  materializes the input PCM. Output `Stream` holds `Vec<Frame>` until `write` → peak ≈
  **2× the f32 PCM** (residual `quotients`/`remainders` `Vec<u32>`s; corrected in second
  review — *not* compressed size). `len_hint() -> None` is fine. **Commit 6** drops this to
  O(one block) via the public per-frame API (`encode_fixed_size_frame` coding.rs:584,
  `Frame`/`StreamInfo: BitRepr::write`, `FrameBuf`/`Context` source.rs:115/307 — all
  public; the original "`frames()` is private" blocker does not apply, only `frame_mut`/
  `header_mut` are `pub(crate)`).
- **rubato 3.0.0** — `process_all_into_buffer` is a default trait method; streaming recipe
  is `process_into_buffer` + `Indexing.partial_len` + `output_delay()` trim + zero-pump to
  `ceil(ratio·total_in)` (see 1.2, incl. the exact-multiple last-chunk boundary).
- **symphonia 0.6** — FLAC `seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id })`
  → `SeekedTo { required_ts, actual_ts }`, `actual_ts ≤ ts` on a frame boundary; discard
  `required_ts − actual_ts` (see 1.3).

---

## Commit 2 — schema cleanup, naming, ProjectState room tones, migration, docs

### 2.1 Room-tone rename (format-neutral)

- `RoomToneSegment` → `RoomTone` (in-memory struct).
- `v1::RoomTonePcmV1` → `v1::RoomToneV1`.
- `Kind::RoomTonePcm` → `Kind::RoomTone` — **tag byte stays `0x4`** (`hash.rs:49,134,240`);
  this is a pure variant rename, not a format change.
- `RoomTone` (detection-outcome enum) → `RoomToneOutcome` (`Found(RoomTone) | None`).
- Update `encode_room_tone`/`decode_room_tone`, all refs, doc-comments, and any design-doc
  prose mentioning `RoomTonePcm`.
- **Blob bytes unchanged:** postcard does not encode struct names and the tag byte is the
  same, so `room_tone_v1.blob` and the pinned B19/B20/B25 constants stay valid. Confirm by
  running those tests after the rename — no regeneration expected.

### 2.2 Drop redundant `TrackMeta` fields (format change)

Remove from `TrackMeta`, `v1::TrackMetaV1`, both `From` impls, and every constructor/test
site (`engine.rs:~1989/2000`, `undo.rs:~332/343`, `metadata.rs` constructors + tests):

- `resampled_path` — derived: `resampled/<id>.flac` (existing `resampled_cache_path`);
  "is it generated yet" is transient task state / file existence, not persisted.
- `enhanced_path` — derived: `enhanced/<id>.flac` by convention; "was enhancement run" is
  `models_used.enhancement.is_some()` (+ `wet_dry_ratio`). Codify the path convention in
  the enhancement milestone doc.
- `room_tone_length_samples` — derived from the loaded `RoomTone` (`samples.len()/channels`).

Keep `room_tone_hash` (the blob reference — not derivable).

### 2.3 Drop `source_decode_offset` (format change)

Remove from `SpliceKind::Source`, `v1::SpliceKindV1::Source`, both `From` impls, and all
constructors/tests in `turn.rs`. `SpliceKind::Source { source_start_sample }` is complete:
the renderer seeks the FLAC cache to `source_start_sample` and discards
`required - actual` at read time (the `FrameReader` seek+discard from 1.3). No offset is
persisted in the EDL.

### 2.4 `ProjectState` room tones (in-memory derived state)

- Add `room_tones: BTreeMap<u32, Arc<RoomTone>>` as a **sibling field on `ProjectState`**
  (not in `TimelineState`/undo — room tone is derived from the store and does not ride the
  per-edit clone; the audio path must not allocate/lock, so the pre-roll thread gets cheap
  `Arc` handoffs).
- `open_project`: after metadata resolution, for each track with `room_tone_hash`,
  `store::get` → `decode_room_tone` → insert.
- `new_project`: empty map. Provide an insert method for the M4 import path to call when a
  new room tone is detected/persisted.
- Add an accessor for the renderer/pre-roll.
- Test: open a project carrying a persisted room-tone blob → segment resident; derived
  length matches the blob.

### 2.5 Migration (pre-1.0 revise-in-place)

2.2 + 2.3 are persisted-format changes to `TrackMetaV1` (Metadata blob) and `SpliceKindV1`
(Turn blob). Per the uniform pre-1.0 posture (identical docstring on `metadata.rs:358`,
`turn.rs:153`, etc.): revise the `V1` structs **in place** (no `V2`), then:

- Regenerate pinned consts via the `#[ignore]` `capture_pinned_values` tests:
  `metadata.rs` `PINNED_WIRE_BYTES`[225]/`PINNED_HASH`; `turn.rs`
  `PINNED_WIRE_BYTES`[59]/`PINNED_HASH`.
- Regenerate the committed `project_v1.vocalboard` fixture (it carries `TrackMeta` + turns
  with `SpliceKind`); keep `fixture_roundtrip.rs` green. `room_tone_v1.blob` is unaffected
  (2.1 is format-neutral).
- Bump `min_app_version` default in `core/migrations/0001_initial.sql` from `'0.1.1'` to
  the next patch (e.g. `'0.1.2'`) and regenerate the fixture so it carries the new value.
  *(Decision to confirm at implementation: edit the `0001` default in place — acceptable
  pre-1.0 since the only "database" is the regenerated fixture — vs. a `0002` migration.)*
- Pre-1.0, prior-format fixtures are simply replaced; there is no real user data.

### 2.6 Design-doc updates (same commit)

- `data-model.md`: `TrackMeta` schema (remove the 3 fields; note derived paths + room-tone
  length derivation); `SpliceKind` (remove `source_decode_offset`).
- `audio-pipeline.md`: resample section (`resampled_path` null→derived, line ~33);
  room-tone section (streaming read, remove the `room_tone_length_samples` sentence at
  line ~136); confirm the "splices carry no stored offset" note at line ~61 now covers the
  decode offset too.
- `phase1-m2.md` and Steps 02–05 docs: reflect the streaming transcode + schema/naming.
- Downstream: renderer step(s) (`phase1-m2-06`+) for the `FrameReader` + `ProjectState`
  room tones + seek-discard; enhancement milestone doc for the derived `enhanced/` path.
- This revision doc (mark steps done as they land).

### Commit 2 validation

- Regenerated metadata/turn pinned + `fixture_roundtrip` tests green.
- `ProjectState` room-tone load test.
- `cargo fmt`, `clippy -D warnings`, `cargo test --workspace`; `pnpm check` if bindings
  regenerate from the schema changes.

---

## Second review — Commits 3-6

A second review of the landed streaming work raised four points, each landing as an
independent commit (ordering rationale in [Sequencing](#sequencing)):

1. `FlacFrameReader` is a generic Symphonia-over-`MediaSourceStream` reader — nothing in it
   is FLAC-specific — so the name over-claims a restriction. **(Commit 3.)**
2. The ffmpeg fallback buffers the *entire* decode in RAM (`Command::output()` → `Vec<u8>`,
   then `BufferedSource`), so the "streaming" transcode has a fully-buffered head whenever it
   falls back. **(Commit 4.)**
3. After Commit 4, whole-buffer `decode()`/`decode_flac()` have **no non-test consumer that
   needs the samples** (the only prod caller, `cache.rs` length probe, needs a frame *count*).
   Their remaining job is as the trusted reference implementation in streaming-reader tests, so
   they should be demoted to test support. Separately, the Symphonia open/packet-decode loop is
   triplicated (`decode_symphonia`, `SymphoniaSource`, the renamed `SymphoniaFrameReader`) and
   should be DRYed into one shared pump. **(Commit 5.)**
4. The deferred output-streaming is both **more necessary** than documented (the in-RAM
   `Stream` is ≈2× the PCM, not ≈compressed — see the §1.1 correction) and **cheaper** than
   documented (flacenc 0.5.1 exposes the per-frame encode API publicly). **(Commit 6.)**

### 3 — Rename `FlacFrameReader` → `SymphoniaFrameReader` (`audio/frame_reader.rs`)

Pure rename + doc clarification; no behaviour change.

- Rename the struct (`frame_reader.rs:121`) and its `open`/impl/`map_symphonia_error` refs.
  It wraps `symphonia::default::get_probe()` + `get_codecs()` over a `MediaSourceStream`
  (identical machinery to `decode_symphonia`) — it reads any Symphonia-supported codec, not
  just FLAC. The name now matches the sibling `SymphoniaSource` in `decode.rs`.
- Doc comment: describe it as the seekable Symphonia reader used in production against the
  resampled FLAC cache. **Keep a caveat** that sample-accurate `seek_to_frame` relies on
  Symphonia *accurate* seek landing at-or-before the target (rock-solid for our FLAC cache;
  not validated as sample-accurate across arbitrary codecs).
- Update callers: room-tone wiring (`detect`/`detect_room_tone`), FR3/FR4 tests, and any
  `plans/phase1-m2-06`+ docs that reference the impl by name (EDL cursor / renderer).
- **Validation.** Existing FR1–FR4 cover behaviour; `cargo fmt`, `clippy -D warnings`,
  `cargo test --workspace`.

### 4 — Streaming ffmpeg fallback (`audio/ffmpeg.rs`, `audio/decode.rs::open_source`)

Make the rare ffmpeg fallback stream like the Symphonia path instead of buffering the whole
decode.

- **New `FfmpegSource: PcmSource` in `ffmpeg.rs`:**
  - `probe_via_ffmpeg` first for native `sample_rate`/`channels` (no `-ar`/`-ac` — rubato
    stays the sole resampler).
  - Spawn `ffmpeg -v error -i <path> -map 0:a:0 -f f32le -acodec pcm_f32le -` with
    `stdout(Stdio::piped())`; redirect **stderr to a temp file** (`Stdio::from(File)`) to
    avoid any pipe-fill deadlock without a reader thread (`-v error` keeps it tiny). `stdin`
    null.
  - `read(&mut [f32])`: pull bytes from child stdout into a reusable byte buffer, decode
    complete 4-byte LE groups to f32, and **hold a <4-byte remainder across calls** (pipe
    chunk boundaries don't align to samples). Fill `out`; return frame count
    (`samples/channels`). Greedy fill semantics matching `SymphoniaSource::read`.
  - At EOF: `child.wait()`. Non-zero exit → `FfmpegFailed { detail: redact_path(stderr) }`
    (read the temp file). A leftover partial sample (bytes not a multiple of 4) → `FfmpegFailed`
    (mirrors the existing whole-buffer check). Set `exhausted`.
- **`open_source` (`decode.rs:~389`):** replace the
  `BufferedSource::from(decode_via_ffmpeg(path)?)` branch with `Box::new(FfmpegSource::open(path)?)`.
  Routing unchanged (`SymphoniaSource` first; `UnsupportedFormat` → ffmpeg). `BufferedSource`
  loses its last prod use here — keep it (still handy for tests) or drop if grep shows none.
- **Keep** whole-buffer `decode_via_ffmpeg` for now — still used by whole-buffer `decode()`
  until Commit 5 demotes it.
- **Tests** (gate on `ffmpeg_available()`, early-return when absent — matches F17/F20; CI may
  lack ffmpeg):
  - **FS1** — drive `FfmpegSource` directly over a fixture and assert sample-for-sample
    equality with the whole-buffer `decode_via_ffmpeg` oracle (translate-and-replay: one
    source, two readers — conventions A4).
  - **FS2** — chunk-boundary robustness: read with `out` lengths that are *not* multiples of
    channels/4 bytes; reassembled output equals the oracle (exercises the partial-byte
    remainder seam — A4).
  - **FS3** — bad input → `FfmpegFailed`, path redacted, non-zero exit surfaced (not a silent
    short read).
  - **FS4** — empty/zero-frame source → 0 frames, no error, `is_exhausted()`.
  - *Note:* exercising the **routing** end-to-end needs a Symphonia-unsupported /
    ffmpeg-supported fixture; if none is freely available in CI, drive `FfmpegSource` directly
    (above) and leave the routing path to a manual check. Document this gap.
- **Validation.** `cargo fmt`, `clippy -D warnings`, `cargo test --workspace` (ffmpeg tests
  no-op without the binary).

### 5 — Shared Symphonia packet pump + demote whole-buffer decode to test support

Two coupled cleanups. **(a)** removes triplicated decode logic; **(b)** removes whole-buffer
decode from the shipped surface.

**(a) Extract the shared pump (`audio/decode.rs`).** Replace the open/probe/track-resolve +
`next_packet`/`decode` match arms — currently written 3× in `decode_symphonia`
(`decode.rs:55,102`), `SymphoniaSource::{open,fill_one_packet}` (`decode.rs:228,286`), and
`SymphoniaFrameReader::{open,fill_one_packet}` (`frame_reader.rs:138,194`) — with:
  - `open_symphonia(path) -> Result<SymphoniaOpen, AudioError>` returning `{ format, decoder,
    track_id, channels, sample_rate }` (the shared open/probe/track-resolve + decoder build).
  - `decode_next_packet(format, decoder, track_id) -> Result<Option<Vec<f32>>, AudioError>`
    encapsulating the `next_packet`/`ResetRequired`/`UnexpectedEof`(truncation)/`DecodeError`/
    `IoError` arms and interleaved-f32 copy. `None` = clean EOF.
  - `SymphoniaSource` and `SymphoniaFrameReader` keep their distinct leftover/seek state but
    call these; the test-support whole-buffer decode (below) also builds on them.
  - **Behaviour unification to flag:** today `decode_symphonia`'s `ResetRequired` arm *rebuilds*
    the decoder from updated track params (chained Ogg), while the streaming impls only
    `decoder.reset()`. Unify on the rebuild path (more correct for chained streams) — this
    slightly changes `SymphoniaSource`/`SymphoniaFrameReader` for chained inputs. Add a chained
    fixture test if cheap; otherwise note the change.

**(b) Demote whole-buffer decode to test support.**
  - Move `decode()`, `decode_flac()`, and the now-prod-unused whole-buffer `decode_via_ffmpeg`
    into a `#[cfg(test)] pub(crate)` test-support module (e.g. `audio/test_support.rs`), built
    on the shared pump. These stay the **reference implementations** the streaming tests assert
    against (FR3/FR4, TC2, cache tests) — their value is being an obviously-correct oracle, not
    a shipped capability (CLAUDE.md: no speculative public API).
  - **`probe()` stays in prod** (`decode.rs`) — real use at `cache.rs:75`.
  - **Replace `cache.rs:77`** (`decode_flac(path)?.frames()`) with a streaming count:
    `count_frames(path)` opens a `SymphoniaFrameReader` and sums `read_frames` to EOF. Keeps
    production free of whole-buffer decode. (Near-dead branch — our cache always writes the
    STREAMINFO length — but now it streams.)
  - Grep-gate: confirm no remaining prod (`#[cfg(not(test))]`) caller of the demoted fns.
- **Tests.**
  - Pure-refactor coverage: existing T (decode), TC (transcode), FR (frame reader), B/D/L
    (room tone) tests stay green; the shared-pump outputs must be bit-identical.
  - **CF1** — `count_frames` == `probe().length_frames` == prior whole-buffer `.frames()` for a
    normal cache.
  - **CF2** (A4 seam) — `count_frames` on a file whose length is **not** a 4096 multiple (final
    short frame) returns the exact total.
- **Validation.** `cargo fmt`, `clippy -D warnings`, `cargo test --workspace`.

### 6 — Output-streaming FLAC encode (`audio/flac_stream.rs` new, `audio/transcode.rs`) ✅ landed

Bound transcode peak memory to **O(one block)** by encoding and writing frame-by-frame
instead of accumulating the whole `Stream`. This realizes the output-streaming that
`audio-pipeline.md:37` already (prematurely) describes.

**Why per-frame, not batch-concat (settled in review).** Symphonia's accurate FLAC seek
binary-searches byte offsets assuming frame timestamps rise **monotonically**
(`symphonia-bundle-flac-0.6.0/demuxer.rs` `seek`), reading each frame header's coded number.
`encode_with_fixed_block_size` numbers every batch's frames from 0
(`coding.rs:606`), so concatenating per-batch encodes yields a sawtooth timestamp curve →
broken seek (and `SymphoniaFrameReader::seek_to_frame` is load-bearing for room-tone pass 2 /
FR4 / future splice). flacenc exposes `frame(n)`/`frame_count()`/`Frame::write` publicly but
**not** frame renumbering (`header_mut` is `pub(crate)`), and there is no "encode batch
starting at frame N" entry point — so correct numbering forces the per-frame call. A single
reused `Context` supplies monotonic numbers *and* the running MD5/total for free.

**`WriteBitSink<W: Write>` (impl `flacenc::bitsink::BitSink`).**
  - Maintain a bit accumulator (e.g. `u64` + bit count); implement the 4 required methods
    (`align_to_byte`, `write_lsbs`, `write_msbs`, `write`) flushing whole bytes to a
    `BufWriter<W>`. Defaults (`write_bytes_aligned`/`write_twoc`/`write_zeros`) come free.
    `type Error` wraps `io::Error`.
  - **Bit order must match flacenc's `MemSink<u8>`/`ByteSink` (MSB-first).** SW9 pins this with
    `to_bitstring` parity on small writes so output is bit-correct.

**Streaming encoder (mirrors `coding.rs:659-694`, `add_frame` → `write`+drop).**
  - Open `out` as a seekable read+write `File`; wrap in `WriteBitSink`.
  - Write the header once: `fLaC` magic, then the STREAMINFO metadata block — hand-write the
    4-byte block header (last-metadata-block flag set, type 0, 24-bit length = 34) and a
    **placeholder** STREAMINFO body via `StreamInfo: BitRepr::write` (`bitrepr.rs:240`).
  - Reuse **one** `(FrameBuf, Context)` for the whole file (`FrameBuf::with_size(channels,
    4096)`, `Context::new(bps, channels)`).
  - Loop: `flac_src.read_samples(4096, &mut (framebuf, context))` (the existing
    `FlacPullSource` Fill does f32→int24 + error-stash + frame count — reuse unchanged); on 0
    → break; `encode_fixed_size_frame(cfg, &framebuf, context.current_frame_number(),
    &stream_info)` → `frame.write(&mut sink)` → drop the frame.
  - Track total frames; set STREAMINFO `min_block_size == max_block_size == 4096` directly (the
    final short frame is the permitted exception — this **subsumes**
    `flac::normalize_fixed_block_size`, which the streaming path no longer needs).
  - At EOF: build final `StreamInfo` (`total_samples` + `md5_digest` from `Context`, min=max
    block size; frame-size fields optional/0). `sink.flush()`/`align`, then `File::seek(Start(8))`
    (4 magic + 4 block-header) and rewrite the 34 STREAMINFO bytes via a fresh
    `WriteBitSink`/`StreamInfo::write`. Flush.
  - Return the project-rate frame count (as today).
- **`transcode_to_cache`:** swap `encode_with_fixed_block_size` + `ByteSink` + `fs::write` for
  this writer (writing directly to the `out` `File`). Keep `FlacPullSource`, the `channels == 0`
  degenerate early-return, and the error-stash semantics. Drop the `normalize_fixed_block_size`
  post-pass.
- **`flac::encode_flac_24` stays as-is** (whole-buffer; the test fixture maker / oracle). It
  need not be byte-identical to the streaming writer — FR/TC tests compare *decoded samples*,
  and C17/TC3 determinism need only self-consistency, which the deterministic single-threaded
  writer satisfies.
- **Single-threaded** (no `par`). `par` stays enabled for `encode_flac_24`'s
  `encode_with_fixed_block_size`. Batch parallelism (fill sequential → encode a batch via rayon
  → write in order) is an **additive follow-on** on this infrastructure — see Out of scope —
  to be added only if profiling shows encode (not decode/resample/disk) is the bottleneck.
- **Tests** (A4 seams at block/seek boundaries emphasized):
  - **SW1** — round-trip: streaming-encode a known signal → decode → samples within the 24-bit
    bound (mirrors C12/C13).
  - **SW2** — byte-determinism: same input twice → byte-identical files (C17 analog).
  - **SW3** — STREAMINFO: probe → `codec == "flac"`, rate/channels, `length_frames == Some(total)`
    (backpatch wrote `total_samples`), `min_block_size == max_block_size` (Symphonia-compat
    invariant), MD5 correct (decode + recompute, or assert probe length).
  - **SW4** — seek accuracy across block boundaries and at the final short frame
    (`SymphoniaFrameReader::read_range` vs whole-buffer oracle, FR4 analog) — proves end-to-end
    monotonic frame numbering, the load-bearing property.
  - **SW5** — final short frame: total not a 4096 multiple → exact length, valid stream,
    min=max in STREAMINFO.
  - **SW6** — empty source (0 frames): valid/empty per the `channels==0` early-return; probe
    length `Some(0)`; no panic.
  - **SW7** — single short sub-block (< 4096 frames): one frame; decoded output matches
    `encode_flac_24`.
  - **SW8** — peak memory does not scale with file length (manual profiling note, or a
    feature-gated allocation-counter check — hard to assert in a unit test).
  - **SW9** — `WriteBitSink` bit-order/alignment parity vs flacenc `ByteSink` `to_bitstring`.
  - **SW10** — writer `io::Error` surfaces as `EncodeFailed`/`Io`, not a panic.
  - Existing **TC1–TC4** stay green (transcode now uses the writer; TC4 error-stash still
    propagates through the new loop).
- **Doc updates (this commit).** Correct §1.1 + API findings (done above); remove the
  output-streaming bullet from Out of scope (below); verify `audio-pipeline.md:37` now matches
  reality and update its `normalize_fixed_block_size` sentence (the min=max fixup is now written
  into STREAMINFO directly, not a post-write patch); mark commits done as they land.
- **Validation.** `cargo fmt`, `clippy -D warnings`, `cargo test --workspace`.

---

## Out of scope (noted, not done here)

- ~~**Full output-streaming of the FLAC encode**~~ — **promoted to Commit 6** (§6). The
  original deferral cited private `frames()`/`stream_info_block()` accessors; the per-frame
  encode API is in fact public in 0.5.1, and the in-RAM `Stream` was found to be ≈2× the PCM
  (not ≈compressed), so the win is bigger and the cost smaller than recorded.
- **Batch / parallel output-streaming** (encode a batch of frames via rayon, write in order)
  — additive on Commit 6's per-frame writer. Deferred until profiling shows the FLAC encode,
  rather than decode/resample/disk, is the import bottleneck (doubtful for speech).
- Fusing room-tone pass 1 into the transcode stream (rejected in 1.4 — canonical input is
  the post-FLAC cache).
- The 10 s sample-ring optimization that skips the contiguous-window second read
  (follow-on; the ≤10 s targeted re-read from a local FLAC is cheap).
- Reading already-at-project-rate lossless sources directly to skip the cache duplicate
  (already a deliberate Phase 2 item, `audio-pipeline.md:35`).
