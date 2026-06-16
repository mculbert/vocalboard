//! Playback engine: SPSC ring buffer, backend abstraction, pre-roll thread, events.
//!
//! Components:
//! - `Shared`, the `Backend` trait, `InMemoryBackend`, ring split at construction,
//!   drain/flush/silence contract.
//! - `CpalBackend` — real cpal output stream + device-rate negotiation.
//! - pre-roll thread, `'static + Send` renderer, project→device `StreamingResampler`,
//!   shutdown/join protocol.
//! - `PlayheadUpdate` emission from the pre-roll thread.
//! - `play_from` / `pause` / `stop` state machine.
//!
//! See design/audio-pipeline.md § Playback engine.
//!
//! Two sample-rate clocks: the renderer/EDL/playhead semantics run at the **project rate**;
//! the ring, callback, and `frames_played` run at the negotiated **device rate**. The pre-roll
//! thread bridges them with a `StreamingResampler` (passthrough when the rates match).
//!
//! # Liveness / deadlock-freedom
//!
//! Three actors touch shared state: the **output callback** (consumer — `drain_contract`, run by
//! cpal or synchronously by [`PlaybackEngine::pull`]), the **pre-roll thread** (producer —
//! `run_preroll`), and the **control thread** ([`PlaybackEngine::play_from`] / `pause` / `stop`,
//! which spawns and joins the pre-roll thread). Playback is deadlock-free because no cycle can form
//! in the wait-for graph, which rests on four invariants — **preserve all four when editing this
//! module**:
//!
//! 1. **The consumer never waits.** `drain_contract` touches only the lock-free `rtrb` consumer
//!    and the [`Shared`] atomics — no mutex, `join`, `park`, allocation, or blocking I/O. An empty
//!    ring yields silence and returns rather than waiting, so the reader makes unconditional forward
//!    progress regardless of the other actors. (This is also the real-time-safety guarantee.)
//! 2. **The producer only does *timed* waits, gated on the stop flag.** The pre-roll thread can
//!    block only in back-pressure (ring full) or the natural-stop drain-wait; both are
//!    `park_timeout(1ms)` loops that re-check `stop_requested` every iteration. There is no untimed
//!    `park`, so the producer can never be permanently parked waiting on the consumer.
//! 3. **The two mutexes (`session`, `producer`) form no cycle.** They are always taken in the order
//!    `session` → `producer`, are *never* taken on the callback path, and — critically — the
//!    pre-roll thread takes *neither* (its `Producer` is moved in by value and returned via
//!    `PrerollReturn` on join). So when the control thread joins the pre-roll thread, the joinee
//!    holds no lock the joiner wants: the edge that would close the cycle does not exist.
//! 4. **`join` is bounded.** Control sets `stop_requested = true` (and `playing = false`) *before*
//!    `join()`, and by (1)+(2) the pre-roll thread then reaches a return within ~1ms regardless of
//!    consumer state; the `producer` mutex is taken only *after* `join()` returns, never across it.
//!
//! The worst case is therefore a bounded delay (~1ms polling granularity), never a permanent stall.
//!
//! **Contract on the emit closures:** `emit_update` / `emit_stopped` (passed to
//! [`PlaybackEngine::play_from`]) **must not re-enter the engine**. `emit_stopped` is invoked from
//! *inside* the pre-roll thread on the natural-stop path, so calling [`PlaybackEngine::stop`] from
//! it would self-join the pre-roll thread (panic/hang). They are event *sinks* — push onto a
//! channel / Tauri event bus, nothing more.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};

use super::render::Renderer;
use super::resample::StreamingResampler;
use super::source_provider::CacheSourceProvider;
use super::{AudioError, PcmSource};
use crate::settings::ResamplingQuality;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Ring buffer depth in milliseconds. Sized at `PlaybackEngine::new` to the negotiated
/// device rate (the rate the callback drains at). Not a user setting — internal
/// latency/underrun tradeoff.
pub const RING_MS: u64 = 200;

/// Pre-roll render chunk in milliseconds. The pre-roll thread renders this much per loop.
pub const RENDER_CHUNK_MS: u64 = 10;

/// Playhead update event cadence in milliseconds of *played* (not rendered) audio.
pub const PLAYHEAD_INTERVAL_MS: u64 = 50;

// ---------------------------------------------------------------------------
// Shared atomic control state
// ---------------------------------------------------------------------------

/// Atomic state shared between the pre-roll thread (writer) and the output backend (reader).
///
/// All fields are atomics — this struct is never locked and is safe on the real-time callback
/// path.
pub struct Shared {
    /// Stereo frames delivered to the output this session. Only incremented for real frames
    /// popped from the ring, not for silence padding. Reset to 0 at each `play_from`.
    pub(crate) frames_played: AtomicU64,
    /// When `false` the callback pops and discards frames (inter-session flush) rather than
    /// copying them to the output. Set to `true` only by `play_from` after resetting
    /// `frames_played`.
    pub(crate) playing: AtomicBool,
}

impl Shared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            frames_played: AtomicU64::new(0),
            playing: AtomicBool::new(false),
        })
    }
}

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// Abstracts the consumer side of the ring so the engine is hardware-independent.
///
/// The production implementation drives a cpal data callback; the in-memory implementation
/// is driven synchronously by [`InMemoryBackend::pull`] for tests and CI (no audio device
/// required).
///
/// Two-phase: [`Backend::negotiate`] runs **before** the ring is allocated (the ring is sized
/// to the *device* rate, which may differ from the project rate), then [`Backend::start`]
/// receives the ring consumer once it exists. See [`PlaybackEngine::new`].
pub trait Backend: Send {
    /// Resolve the output-device sample rate, given the project's desired (`requested`) rate.
    ///
    /// Returns the rate the device will actually run at. For cpal this prefers an exact-rate
    /// device config and falls back to the device default when none matches; for the in-memory
    /// backend it returns `requested` (passthrough) unless a rate was forced for testing.
    /// Called exactly once, before the ring is allocated, at [`PlaybackEngine::new`].
    fn negotiate(&mut self, requested: u32) -> Result<u32, AudioError>;

    /// Hand the ring consumer and shared state to the backend and start delivering audio.
    ///
    /// For cpal: builds the output stream and moves them into the data callback.
    /// For in-memory: stores them for synchronous [`InMemoryBackend::pull`] calls.
    /// Called exactly once at [`PlaybackEngine::new`], after [`Backend::negotiate`].
    fn start(&mut self, consumer: Consumer<f32>, shared: Arc<Shared>) -> Result<(), AudioError>;
}

/// Selects the output backend for [`PlaybackEngine::new`].
pub enum BackendKind {
    /// Real cpal audio device output. Not available on CI / headless hosts.
    Cpal,
    /// Synchronous in-memory consumer for integration tests. Drive via [`PlaybackEngine::pull`].
    /// Negotiates to the project rate (passthrough — no resampling).
    InMemory,
    /// Test-only in-memory backend that negotiates to `device_rate` **regardless** of the
    /// requested project rate, to exercise the resampling (two-clock) path without audio
    /// hardware. The pre-roll thread then bridges project rate → `device_rate`.
    InMemoryAtRate(u32),
}

// ---------------------------------------------------------------------------
// Drain / flush / silence contract  (§ Detailed design A)
// ---------------------------------------------------------------------------

/// Run the drain/flush/silence contract over one output buffer.
///
/// Contract (shared by both the cpal callback and [`InMemoryBackend::pull`]):
///
/// 1. Repeatedly read the largest available frame-aligned chunk from `consumer`.
/// 2. If `playing`: copy frames into `out` and increment `frames_played` by the number of
///    stereo frames copied. If **not** `playing`: pop-and-commit without copying
///    (inter-session flush — drains stale frames from a prior session).
/// 3. When `out` is not yet full and the ring is empty: fill the remainder with `0.0`.
/// 4. No allocation, no locking, no blocking, no event emission.
///
/// `out.len()` must be even (interleaved stereo — two f32 samples per frame).
pub(crate) fn drain_contract(consumer: &mut Consumer<f32>, shared: &Arc<Shared>, out: &mut [f32]) {
    debug_assert_eq!(
        out.len() % 2,
        0,
        "output buffer must be frame-aligned (even len)"
    );
    let total = out.len();
    let mut pos = 0usize; // samples written into `out`

    loop {
        // Always consume whole stereo frames (mask off any odd remainder).
        let avail_aligned = consumer.slots() & !1usize;

        if avail_aligned == 0 {
            // Ring empty (or a stray single sample): fill the rest with silence.
            out[pos..].fill(0.0);
            return;
        }

        // `playing` is re-read each iteration, so a concurrent play_from/stop can flip it
        // mid-buffer. This is intentionally tolerated: control always sets the flag and
        // (re)spawns/joins the pre-roll thread around it, and frames_played is reset before
        // playing=true, so the worst case is one buffer of mixed flush/silence — bounded and
        // non-corrupting; the played-position count never advances against a stale session.
        if shared.playing.load(Ordering::Acquire) {
            if pos >= total {
                // Output buffer is full; leave the remaining ring data for the next pull.
                return;
            }
            let want = (total - pos).min(avail_aligned);
            // `want` is even: min of two even values (total-pos is even iff pos is even,
            // and pos starts at 0 and only increments by even `want`; avail_aligned is even).
            // read_chunk succeeds because slots() >= avail_aligned >= want. If it somehow
            // fails (unexpected internal error), degrade to silence for RT safety.
            let Ok(chunk) = consumer.read_chunk(want) else {
                out[pos..].fill(0.0);
                return;
            };
            let (s1, s2) = chunk.as_slices();
            let n1 = s1.len();
            out[pos..pos + n1].copy_from_slice(s1);
            if !s2.is_empty() {
                out[pos + n1..pos + n1 + s2.len()].copy_from_slice(s2);
            }
            shared
                .frames_played
                .fetch_add((want / 2) as u64, Ordering::Release);
            chunk.commit_all();
            pos += want;
        } else {
            // Flush path: pop and discard — do not copy, do not advance `pos`.
            // If read_chunk fails unexpectedly, treat ring as empty and fill silence.
            let Ok(chunk) = consumer.read_chunk(avail_aligned) else {
                out[pos..].fill(0.0);
                return;
            };
            chunk.commit_all();
            // Continue looping: drain until the ring is empty, then fill output with silence.
        }
    }
}

// ---------------------------------------------------------------------------
// Cpal backend (real audio device)
// ---------------------------------------------------------------------------

/// Real cpal output stream backend.
///
/// Implements the §A drain/flush/silence contract inside the cpal data callback. Created via
/// [`BackendKind::Cpal`]. `negotiate` prefers a stereo config at the project rate; when no such
/// config is supported it falls back to the device default rate (and the pre-roll thread resamples
/// to bridge the two clocks). `start` opens the stream and keeps it alive inside `_stream`.
pub struct CpalBackend {
    /// Device resolved at `negotiate` time; consumed by `start` to build the stream.
    device: Option<cpal::Device>,
    /// Stream config resolved at `negotiate` time; consumed by `start`.
    stream_config: Option<cpal::StreamConfig>,
    /// The live cpal output stream. Kept here so it is not dropped until the engine is.
    #[allow(dead_code)]
    _stream: Option<cpal::Stream>,
}

impl CpalBackend {
    fn new() -> Self {
        Self {
            device: None,
            stream_config: None,
            _stream: None,
        }
    }
}

/// Whether a device output config can serve `requested` Hz in stereo: stereo channels and a
/// rate range that contains `requested` exactly. Extracted from [`CpalBackend::negotiate`]'s
/// config scan so the (hardware-independent) matching rule is unit-testable without a device.
fn config_matches_stereo(channels: u16, min_rate: u32, max_rate: u32, requested: u32) -> bool {
    channels == 2 && min_rate <= requested && requested <= max_rate
}

impl Backend for CpalBackend {
    fn negotiate(&mut self, requested: u32) -> Result<u32, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::DeviceError("no default output device".to_string()))?;

        // cpal::SampleRate is a type alias for u32 in cpal 0.17.
        let project_sr: cpal::SampleRate = requested;

        // Prefer a stereo config whose rate range contains the project rate exactly.
        // If supported_output_configs() fails (unusual), treat as "no match" and fall through.
        let exact_stereo_match = device
            .supported_output_configs()
            .ok()
            .map(|mut configs| {
                configs.any(|c| {
                    config_matches_stereo(
                        c.channels(),
                        c.min_sample_rate(),
                        c.max_sample_rate(),
                        project_sr,
                    )
                })
            })
            .unwrap_or(false);

        let (device_rate, config) = if exact_stereo_match {
            (
                requested,
                cpal::StreamConfig {
                    channels: 2,
                    sample_rate: project_sr,
                    buffer_size: cpal::BufferSize::Default,
                },
            )
        } else {
            // Fall back to the device default rate; the pre-roll thread will resample.
            let default_cfg = device
                .default_output_config()
                .map_err(|e| AudioError::DeviceError(e.to_string()))?;
            let rate = default_cfg.sample_rate();
            (
                rate,
                cpal::StreamConfig {
                    channels: 2,
                    sample_rate: default_cfg.sample_rate(),
                    buffer_size: cpal::BufferSize::Default,
                },
            )
        };

        self.device = Some(device);
        self.stream_config = Some(config);
        Ok(device_rate)
    }

    fn start(&mut self, consumer: Consumer<f32>, shared: Arc<Shared>) -> Result<(), AudioError> {
        let device = self
            .device
            .take()
            .ok_or_else(|| AudioError::DeviceError("start called before negotiate".to_string()))?;
        let config = self
            .stream_config
            .take()
            .ok_or_else(|| AudioError::DeviceError("start called before negotiate".to_string()))?;

        let mut consumer = consumer;
        let stream = device
            .build_output_stream::<f32, _, _>(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    drain_contract(&mut consumer, &shared, out);
                },
                |err| {
                    tracing::error!("cpal stream error: {err}");
                },
                None,
            )
            .map_err(|e| AudioError::DeviceError(e.to_string()))?;

        stream
            .play()
            .map_err(|e| AudioError::DeviceError(e.to_string()))?;
        self._stream = Some(stream);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-memory backend (test / CI use)
// ---------------------------------------------------------------------------

/// Synchronous in-memory backend for integration tests.
///
/// After [`PlaybackEngine::new`] with [`BackendKind::InMemory`], drive playback by calling
/// [`PlaybackEngine::pull`] instead of waiting for a hardware callback. Each `pull(frames)`
/// call is equivalent to one cpal callback invocation of that output buffer size.
pub struct InMemoryBackend {
    consumer: Option<Consumer<f32>>,
    shared: Option<Arc<Shared>>,
    /// When `Some`, [`Backend::negotiate`] returns this rate instead of the requested one,
    /// forcing the resampling path for tests (via [`BackendKind::InMemoryAtRate`]).
    forced_rate: Option<u32>,
}

impl InMemoryBackend {
    fn new(forced_rate: Option<u32>) -> Self {
        Self {
            consumer: None,
            shared: None,
            forced_rate,
        }
    }

    /// Pull `frames` stereo frames synchronously, obeying the drain/flush/silence contract.
    ///
    /// Returns `frames * 2` interleaved f32 samples — exactly what a hardware buffer of this
    /// size would have received. Panics if called before [`Backend::start`] — that is a
    /// programming error (the engine always calls `start` at construction).
    #[allow(clippy::expect_used)] // programming-error guard: start() is always called by new()
    pub fn pull(&mut self, frames: usize) -> Vec<f32> {
        let consumer = self
            .consumer
            .as_mut()
            .expect("InMemoryBackend::pull called before start()");
        let shared = self
            .shared
            .as_ref()
            .expect("InMemoryBackend::pull called before start()");
        let mut out = vec![0.0f32; frames * 2];
        drain_contract(consumer, shared, &mut out);
        out
    }
}

impl Backend for InMemoryBackend {
    fn negotiate(&mut self, requested: u32) -> Result<u32, AudioError> {
        Ok(self.forced_rate.unwrap_or(requested))
    }

    fn start(&mut self, consumer: Consumer<f32>, shared: Arc<Shared>) -> Result<(), AudioError> {
        self.consumer = Some(consumer);
        self.shared = Some(shared);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// Emitted periodically from the pre-roll thread to report playback position.
///
/// Reports the *played* position (what the listener is hearing), not the rendered position
/// ahead in the ring buffer.
pub struct PlayheadUpdate {
    /// Current playback position in project samples.
    pub position_samples: i64,
}

/// Emitted once when playback stops (natural end-of-EDL, `end_sample` reached, or user stop).
pub struct PlaybackStopped {
    /// Final delivered position in project samples at the moment audio delivery ceased.
    pub position_samples: i64,
}

// ---------------------------------------------------------------------------
// Pre-roll thread
// ---------------------------------------------------------------------------

/// Map device frames to a project-clock sample position.
///
/// `start + round(frames_played × project_rate / device_rate)`.
/// Identity when the rates match (avoids integer rounding).
fn project_pos(start: i64, frames_played: u64, project_rate: u32, device_rate: u32) -> i64 {
    if project_rate == device_rate {
        start + frames_played as i64
    } else {
        // Round-half-up integer division.
        start
            + (frames_played as i64 * project_rate as i64 + device_rate as i64 / 2)
                / device_rate as i64
    }
}

/// Owned state captured by one pre-roll thread invocation. Returned on exit so the
/// engine can return the `Producer` to its mutex for the next session.
struct PrerollReturn {
    producer: Producer<f32>,
}

/// Run the pre-roll loop. Returns the `Producer` so the engine can reuse the ring.
///
/// Exits when:
/// - `stop_requested` is set (user stop/pause), or
/// - the resampler signals end-of-stream (end_sample / end-of-EDL).
///
/// For a natural stop the function does the drain-wait and emits `playback_stopped`
/// before returning. For a user stop the caller (`stop()` / `pause()`) handles emission.
// The pre-roll thread bridges the two clocks and owns several distinct collaborators (producer,
// renderer, rates, quality, shared atomics, stop flags, both emit sinks); threading them through
// a struct would not improve clarity over this single call site (`play_from`).
#[allow(clippy::too_many_arguments)]
fn run_preroll(
    mut producer: Producer<f32>,
    renderer: Renderer<CacheSourceProvider>,
    start: i64,
    device_rate: u32,
    project_rate: u32,
    quality: ResamplingQuality,
    shared: Arc<Shared>,
    stop_requested: Arc<AtomicBool>,
    stop_emitted: Arc<AtomicBool>,
    emit_update: impl Fn(PlayheadUpdate),
    emit_stopped: Arc<dyn Fn(PlaybackStopped) + Send + Sync>,
) -> PrerollReturn {
    // The renderer is itself the project-rate `PcmSource`; the `[start, end)` window is owned by
    // its `EdlCursor`, so the resampler simply drains it to end-of-stream.
    let mut resampler = match StreamingResampler::new(renderer, device_rate, quality) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("pre-roll resampler init failed: {e}");
            return PrerollReturn { producer };
        }
    };

    let chunk_frames = ((device_rate as u64 * RENDER_CHUNK_MS / 1000) as usize).max(1);
    let mut chunk_buf = vec![0.0f32; chunk_frames * 2];
    let mut produced = 0u64;
    let mut last_playhead_dev_frames = 0u64;
    let playhead_interval_dev_frames = (device_rate as u64 * PLAYHEAD_INTERVAL_MS / 1000).max(1);

    // true = reached end-of-stream naturally (not user stop).
    let natural_stop = loop {
        if stop_requested.load(Ordering::Acquire) {
            break false;
        }

        let frames_read = match resampler.read(&mut chunk_buf) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("pre-roll read error: {e}");
                break false;
            }
        };

        if frames_read == 0 {
            break true; // end-of-stream
        }

        // Push chunk to ring with back-pressure.
        let mut remaining = &chunk_buf[..frames_read * 2];
        while !remaining.is_empty() {
            if stop_requested.load(Ordering::Acquire) {
                return PrerollReturn { producer };
            }
            let slots = producer.slots();
            if slots == 0 {
                std::thread::park_timeout(Duration::from_millis(1));
                continue;
            }
            let n = slots.min(remaining.len());
            for &s in &remaining[..n] {
                // push cannot fail: we checked slots >= n above and are the sole writer
                let _ = producer.push(s);
            }
            remaining = &remaining[n..];
        }

        produced += frames_read as u64;

        // Emit playhead update if the interval has elapsed.
        let fp = shared.frames_played.load(Ordering::Acquire);
        if fp >= last_playhead_dev_frames + playhead_interval_dev_frames {
            let pos = project_pos(start, fp, project_rate, device_rate);
            emit_update(PlayheadUpdate {
                position_samples: pos,
            });
            last_playhead_dev_frames = fp;
        }
    };

    if natural_stop {
        // Drain-wait: wait until the callback has delivered all produced frames.
        loop {
            let fp = shared.frames_played.load(Ordering::Acquire);
            if fp >= produced || stop_requested.load(Ordering::Acquire) {
                break;
            }
            std::thread::park_timeout(Duration::from_millis(1));
        }

        // Emit playback_stopped exactly once (CAS prevents double-emit if stop() races).
        if !stop_emitted.swap(true, Ordering::AcqRel) {
            let fp = shared.frames_played.load(Ordering::Acquire);
            emit_stopped(PlaybackStopped {
                position_samples: project_pos(start, fp, project_rate, device_rate),
            });
        }
        shared.playing.store(false, Ordering::Release);
    }

    PrerollReturn { producer }
}

// ---------------------------------------------------------------------------
// PlaybackEngine
// ---------------------------------------------------------------------------

/// Per-session state owned by the engine while a play session is active.
struct Session {
    /// Join handle for the pre-roll thread. Joining returns the `Producer<f32>` for reuse.
    handle: JoinHandle<PrerollReturn>,
    /// Signal the pre-roll thread to stop (stop/pause).
    stop_requested: Arc<AtomicBool>,
    /// Callback to emit `playback_stopped` — shared with the pre-roll thread so the natural-
    /// stop path (thread) and the user-stop path (`stop()`) can both use it.
    emit_stopped: Arc<dyn Fn(PlaybackStopped) + Send + Sync + 'static>,
    /// CAS flag: true once `playback_stopped` has been emitted for this session.
    stop_emitted: Arc<AtomicBool>,
    /// Project sample at which this session started.
    start: i64,
}

/// Owns the cpal output stream (or in-memory equivalent) and the SPSC ring buffer.
///
/// Created once at project open at the project's locked sample rate. Play/stop cycles reuse
/// the same ring — no per-play stream reopen. The inter-session flush (§ Detailed design C)
/// discards stale ring frames from the prior session via the `playing` flag.
pub struct PlaybackEngine {
    /// The project's locked sample rate — the rate the [`Renderer`] emits at.
    project_rate: u32,
    /// The negotiated output-device rate. The ring, callback, and `frames_played` count frames
    /// in this rate. Equals `project_rate` when no resampling is needed.
    device_rate: u32,
    /// Sinc quality for the project→device `StreamingResampler` built per `play_from`.
    quality: ResamplingQuality,
    /// Producer half of the SPSC ring. `None` while the pre-roll thread holds it.
    pub(crate) producer: Mutex<Option<Producer<f32>>>,
    /// Shared atomic state for the backend/callback and the pre-roll thread.
    pub(crate) shared: Arc<Shared>,
    /// In-memory backend; `Some` iff constructed with `BackendKind::InMemory{,AtRate}`.
    in_memory: Option<InMemoryBackend>,
    /// Real cpal backend; `Some` iff constructed with `BackendKind::Cpal`.
    #[allow(dead_code)]
    cpal_backend: Option<CpalBackend>,
    /// Active play session, if any (control-path Mutex — never held on the RT path).
    session: Mutex<Option<Session>>,
    /// Last played project-sample position (set on stop/pause; 0 before first play).
    last_pos: AtomicI64,
}

impl PlaybackEngine {
    /// Open the output ring, sized to the **negotiated device rate**.
    ///
    /// `sample_rate` is the project's locked rate (the renderer's output rate). The backend
    /// negotiates the actual device rate first ([`Backend::negotiate`]); the ring is then
    /// pre-allocated to [`RING_MS`] ms of stereo f32 frames **at the device rate**, because the
    /// callback drains it into a device buffer running at that rate. When the two rates differ
    /// the pre-roll thread resamples project → device before pushing to the ring.
    ///
    /// For `BackendKind::InMemory` the device rate equals `sample_rate` (passthrough) and the
    /// consumer is stored inside [`InMemoryBackend`] for [`Self::pull`]; for `BackendKind::Cpal`
    /// the consumer is moved into the cpal data callback.
    ///
    /// `quality` is the sinc preset for the project→device resampler the pre-roll thread builds
    /// per `play_from`; it is ignored whenever the rates match (passthrough).
    pub fn new(
        sample_rate: u32,
        backend: BackendKind,
        quality: ResamplingQuality,
    ) -> Result<Self, AudioError> {
        // For Cpal, negotiate through the real device; for in-memory, use the passthrough backend.
        if matches!(backend, BackendKind::Cpal) {
            let mut b = CpalBackend::new();
            let device_rate = b.negotiate(sample_rate)?;
            let capacity = (device_rate as u64 * RING_MS / 1000 * 2) as usize;
            let (producer, consumer) = RingBuffer::<f32>::new(capacity);
            let shared = Shared::new();
            b.start(consumer, shared.clone())?;
            return Ok(Self {
                project_rate: sample_rate,
                device_rate,
                quality,
                producer: Mutex::new(Some(producer)),
                shared,
                in_memory: None,
                cpal_backend: Some(b),
                session: Mutex::new(None),
                last_pos: AtomicI64::new(0),
            });
        }

        let forced_rate = match backend {
            BackendKind::InMemory => None,
            BackendKind::InMemoryAtRate(rate) => Some(rate),
            // Cpal is handled by the early return above; only the in-memory backends reach here.
            BackendKind::Cpal => unreachable!(),
        };

        // Negotiate the device rate BEFORE sizing the ring — the ring counts device-rate frames.
        let mut b = InMemoryBackend::new(forced_rate);
        let device_rate = b.negotiate(sample_rate)?;

        // Ring capacity: RING_MS of stereo (× 2 samples per frame) at the device rate.
        let capacity = (device_rate as u64 * RING_MS / 1000 * 2) as usize;
        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        let shared = Shared::new();
        b.start(consumer, shared.clone())?;

        Ok(Self {
            project_rate: sample_rate,
            device_rate,
            quality,
            producer: Mutex::new(Some(producer)),
            shared,
            in_memory: Some(b),
            cpal_backend: None,
            session: Mutex::new(None),
            last_pos: AtomicI64::new(0),
        })
    }

    /// The project's locked sample rate (the [`Renderer`](super::render::Renderer) output rate).
    pub fn project_rate(&self) -> u32 {
        self.project_rate
    }

    /// The negotiated output-device sample rate. Differs from [`Self::project_rate`] when the
    /// default device cannot open at the project rate; the pre-roll thread resamples to bridge
    /// them. The ring and `frames_played` count frames in this rate.
    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    /// Pull `frames` stereo frames from the in-memory backend.
    ///
    /// Equivalent to one cpal callback invocation of this output buffer size. Drives the
    /// drain/flush/silence contract synchronously. Panics if constructed with
    /// `BackendKind::Cpal` — that is a programming error (only use `pull` in tests).
    #[allow(clippy::expect_used)] // programming-error guard: only valid with BackendKind::InMemory
    pub fn pull(&mut self, frames: usize) -> Vec<f32> {
        self.in_memory
            .as_mut()
            .expect("PlaybackEngine::pull requires BackendKind::InMemory")
            .pull(frames)
    }

    // -----------------------------------------------------------------------
    // Playback control
    // -----------------------------------------------------------------------

    /// Start playback of `renderer` (already `'static + Send`). The play window is whatever
    /// `renderer`'s `EdlCursor` was built over (`[start, end)`); `start` here is only the
    /// project-clock origin for playhead reporting and must match the cursor's start.
    ///
    /// If a prior session is live it is stopped and joined first (single producer invariant).
    /// `emit_update` fires every [`PLAYHEAD_INTERVAL_MS`] of *played* audio.
    /// `emit_stopped` fires once when playback stops (natural end or [`Self::stop`]), reporting
    /// the position reached (derived from `frames_played`) on a natural stop.
    ///
    /// `emit_stopped` must be `Sync` so the engine and the pre-roll thread can share it.
    // The control-path mutexes (`session`, `producer`) are only ever held briefly on this thread;
    // a poisoned lock means a prior control call panicked mid-update, leaving the engine in an
    // unrecoverable state, so propagating the panic (unwrap) is correct here.
    #[allow(clippy::unwrap_used)]
    pub fn play_from<EU, ES>(
        &self,
        start: i64,
        renderer: Renderer<CacheSourceProvider>,
        emit_update: EU,
        emit_stopped: ES,
    ) -> Result<(), AudioError>
    where
        EU: Fn(PlayheadUpdate) + Send + 'static,
        ES: Fn(PlaybackStopped) + Send + Sync + 'static,
    {
        let mut session_guard = self.session.lock().unwrap();

        // Join any prior session first (SPSC invariant: only one producer at a time).
        if let Some(prev) = session_guard.take() {
            // Signal the thread to stop and join.
            prev.stop_requested.store(true, Ordering::Release);
            self.shared.playing.store(false, Ordering::Release);
            if let Ok(ret) = prev.handle.join() {
                *self.producer.lock().unwrap() = Some(ret.producer);
            }
        }

        // Reset session state.
        self.shared.frames_played.store(0, Ordering::Release);

        // Grab the producer; the pre-roll thread will own it for the duration.
        let producer = self
            .producer
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| AudioError::DeviceError("ring producer unavailable".to_string()))?;

        let stop_requested = Arc::new(AtomicBool::new(false));
        let stop_emitted = Arc::new(AtomicBool::new(false));
        let emit_stopped_arc: Arc<dyn Fn(PlaybackStopped) + Send + Sync + 'static> =
            Arc::new(emit_stopped);

        // Mark playing=true BEFORE spawning so the callback starts delivering frames.
        self.shared.playing.store(true, Ordering::Release);

        let shared_clone = Arc::clone(&self.shared);
        let stop_req_clone = Arc::clone(&stop_requested);
        let stop_em_clone = Arc::clone(&stop_emitted);
        let emit_stopped_thread = Arc::clone(&emit_stopped_arc);
        let device_rate = self.device_rate;
        let project_rate = self.project_rate;
        let quality = self.quality;

        let handle = std::thread::Builder::new()
            .name("vocalboard-preroll".to_string())
            .spawn(move || {
                run_preroll(
                    producer,
                    renderer,
                    start,
                    device_rate,
                    project_rate,
                    quality,
                    shared_clone,
                    stop_req_clone,
                    stop_em_clone,
                    emit_update,
                    emit_stopped_thread,
                )
            })
            .map_err(|e| AudioError::DeviceError(e.to_string()))?;

        *session_guard = Some(Session {
            handle,
            stop_requested,
            emit_stopped: emit_stopped_arc,
            stop_emitted,
            start,
        });

        Ok(())
    }

    /// Stop the pre-roll thread and retain the last played position. Does **not** emit
    /// `playback_stopped`. Returns the last played position in project samples.
    pub fn pause(&self) -> i64 {
        let pos = self.stop_session(false);
        self.last_pos.store(pos, Ordering::Release);
        pos
    }

    /// Stop the pre-roll thread and emit `playback_stopped` exactly once. Idempotent:
    /// if no session is live (or it already naturally stopped), this is a no-op that
    /// returns the last position without re-emitting. Returns the last played position.
    pub fn stop(&self) -> i64 {
        let pos = self.stop_session(true);
        self.last_pos.store(pos, Ordering::Release);
        pos
    }

    /// Internal teardown: signal + join the session thread, return the played position.
    /// When `emit` is true, fires `playback_stopped` (via the session's shared callback)
    /// if it hasn't been emitted already.
    // See `play_from`: a poisoned control-path mutex is an unrecoverable invariant violation.
    #[allow(clippy::unwrap_used)]
    fn stop_session(&self, emit: bool) -> i64 {
        let mut session_guard = self.session.lock().unwrap();
        let Some(session) = session_guard.take() else {
            return self.last_pos.load(Ordering::Acquire);
        };

        session.stop_requested.store(true, Ordering::Release);
        self.shared.playing.store(false, Ordering::Release);

        let producer = match session.handle.join() {
            Ok(ret) => Some(ret.producer),
            Err(_) => {
                tracing::error!("pre-roll thread panicked");
                None
            }
        };

        if let Some(p) = producer {
            *self.producer.lock().unwrap() = Some(p);
        }

        let fp = self.shared.frames_played.load(Ordering::Acquire);
        let pos = project_pos(session.start, fp, self.project_rate, self.device_rate);

        if emit && !session.stop_emitted.swap(true, Ordering::AcqRel) {
            (session.emit_stopped)(PlaybackStopped {
                position_samples: pos,
            });
        }

        pos
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    // Helper: create a ring, push interleaved stereo frames, return (consumer, shared).
    fn filled_ring(frames: &[f32]) -> (Consumer<f32>, Arc<Shared>) {
        let (mut prod, cons) = RingBuffer::<f32>::new(frames.len().max(2) * 4);
        for &s in frames {
            prod.push(s).unwrap();
        }
        (cons, Shared::new())
    }

    // Helper: make a ring of `capacity` samples and return both halves + shared.
    fn empty_ring(capacity: usize) -> (Producer<f32>, Consumer<f32>, Arc<Shared>) {
        let (prod, cons) = RingBuffer::<f32>::new(capacity);
        (prod, cons, Shared::new())
    }

    // -----------------------------------------------------------------------
    // Drain contract unit tests (test drain_contract directly)
    // -----------------------------------------------------------------------

    // Playing=true: drain copies frames from ring to output and increments frames_played.
    #[test]
    fn drain_playing_copies_frames() {
        let frames: Vec<f32> = (0..8).map(|i| i as f32 * 0.1).collect(); // 4 stereo frames
        let (mut cons, shared) = filled_ring(&frames);
        shared.playing.store(true, Ordering::SeqCst);

        let mut out = vec![0.0f32; 8];
        drain_contract(&mut cons, &shared, &mut out);

        assert_eq!(out, frames, "drain must copy ring contents to output");
        assert_eq!(
            shared.frames_played.load(Ordering::SeqCst),
            4,
            "frames_played must equal stereo frames copied"
        );
        assert_eq!(cons.slots(), 0, "ring must be empty after full drain");
    }

    // Playing=true, ring empties before output is full: remainder filled with silence.
    #[test]
    fn drain_underrun_pads_with_silence() {
        let frames = vec![0.1f32, 0.2, 0.3, 0.4]; // 2 stereo frames
        let (mut cons, shared) = filled_ring(&frames);
        shared.playing.store(true, Ordering::SeqCst);

        let mut out = vec![9.9f32; 8]; // 4 frames requested, only 2 available
        drain_contract(&mut cons, &shared, &mut out);

        assert_eq!(&out[0..4], &frames[..], "first 2 frames must be copied");
        assert_eq!(
            &out[4..8],
            &[0.0, 0.0, 0.0, 0.0],
            "underrun must pad with silence"
        );
        assert_eq!(shared.frames_played.load(Ordering::SeqCst), 2);
    }

    // Playing=true, ring empty from the start: entire output is silence.
    #[test]
    fn drain_empty_ring_all_silence() {
        let (_prod, mut cons, shared) = empty_ring(64);
        shared.playing.store(true, Ordering::SeqCst);

        let mut out = vec![9.9f32; 8];
        drain_contract(&mut cons, &shared, &mut out);

        assert!(
            out.iter().all(|&s| s == 0.0),
            "empty ring must produce all silence"
        );
        assert_eq!(shared.frames_played.load(Ordering::SeqCst), 0);
    }

    // Playing=false: ring is drained (flushed) but output is entirely silence; frames_played
    // stays at 0 (no real frames were delivered).
    #[test]
    fn drain_not_playing_flushes_ring_and_fills_silence() {
        let frames: Vec<f32> = (0..8).map(|i| i as f32 * 0.1).collect();
        let (mut cons, shared) = filled_ring(&frames);
        // playing stays false (default)

        let mut out = vec![9.9f32; 8];
        drain_contract(&mut cons, &shared, &mut out);

        assert!(
            out.iter().all(|&s| s == 0.0),
            "not-playing must produce silence"
        );
        assert_eq!(
            shared.frames_played.load(Ordering::SeqCst),
            0,
            "not-playing must not increment frames_played"
        );
        assert_eq!(cons.slots(), 0, "flush must drain the ring");
    }

    // Playing=true, output buffer exactly matches ring content: no silence, no leftover.
    #[test]
    fn drain_exact_fill_no_silence_no_leftover() {
        let frames: Vec<f32> = (0..16).map(|i| i as f32 * 0.01).collect(); // 8 stereo frames
        let (mut cons, shared) = filled_ring(&frames);
        shared.playing.store(true, Ordering::SeqCst);

        let mut out = vec![0.0f32; 16];
        drain_contract(&mut cons, &shared, &mut out);

        assert_eq!(out, frames);
        assert_eq!(cons.slots(), 0);
        assert_eq!(shared.frames_played.load(Ordering::SeqCst), 8);
    }

    // Ring wraps: ring has more data than one output buffer; second pull gets the rest.
    #[test]
    fn drain_partial_pull_leaves_remainder() {
        let (mut prod, mut cons, shared) = empty_ring(32);
        shared.playing.store(true, Ordering::SeqCst);

        // Push 8 stereo frames (16 samples).
        let all: Vec<f32> = (0..16).map(|i| i as f32 * 0.1).collect();
        for &s in &all {
            prod.push(s).unwrap();
        }

        // Pull only 4 frames (8 samples).
        let mut out1 = vec![0.0f32; 8];
        drain_contract(&mut cons, &shared, &mut out1);
        assert_eq!(&out1, &all[..8], "first pull: first 4 frames");
        assert_eq!(shared.frames_played.load(Ordering::SeqCst), 4);

        // Pull remaining 4 frames.
        let mut out2 = vec![0.0f32; 8];
        drain_contract(&mut cons, &shared, &mut out2);
        assert_eq!(&out2, &all[8..], "second pull: remaining 4 frames");
        assert_eq!(shared.frames_played.load(Ordering::SeqCst), 8);
    }

    // Transition not-playing → playing: flush then play. Proves inter-session flush.
    #[test]
    fn drain_flush_then_play_no_stale_frames() {
        let (mut prod, mut cons, shared) = empty_ring(64);

        // Push 4 "stale" frames from a prior session.
        for i in 0..8 {
            prod.push(i as f32 * 0.1 + 1.0).unwrap(); // values > 1.0 so we'd notice them
        }

        // First pull: not playing → flush.
        let mut out = vec![9.9f32; 8];
        drain_contract(&mut cons, &shared, &mut out);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "flush: output must be silence"
        );
        assert_eq!(cons.slots(), 0, "stale frames must be discarded");

        // Push 4 fresh frames for the new session.
        shared.frames_played.store(0, Ordering::SeqCst);
        shared.playing.store(true, Ordering::SeqCst);
        let fresh: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        for &s in &fresh {
            prod.push(s).unwrap();
        }

        // Second pull: playing → should get only fresh frames.
        let mut out2 = vec![0.0f32; 8];
        drain_contract(&mut cons, &shared, &mut out2);
        assert_eq!(out2, fresh, "new session must deliver fresh frames only");
        assert_eq!(shared.frames_played.load(Ordering::SeqCst), 4);
    }

    // -----------------------------------------------------------------------
    // PlaybackEngine construction
    // -----------------------------------------------------------------------

    // Ring capacity at construction is RING_MS × sample_rate × 2 samples (stereo).
    #[test]
    fn engine_ring_capacity_matches_ring_ms() {
        let rate = 48_000u32;
        let engine =
            PlaybackEngine::new(rate, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let expected_capacity = (rate as u64 * RING_MS / 1000 * 2) as usize;

        // Verify by pushing exactly `expected_capacity` samples — it must succeed.
        let mut prod = engine.producer.lock().unwrap();
        let prod = prod.as_mut().unwrap();
        assert_eq!(
            prod.slots(),
            expected_capacity,
            "ring must have RING_MS × rate × 2 sample capacity"
        );
    }

    // InMemory backend negotiates passthrough: device rate == project rate.
    #[test]
    fn engine_in_memory_negotiates_passthrough() {
        let engine =
            PlaybackEngine::new(44_100, BackendKind::InMemory, ResamplingQuality::Balanced)
                .unwrap();
        assert_eq!(engine.project_rate(), 44_100);
        assert_eq!(
            engine.device_rate(),
            44_100,
            "in-memory backend must negotiate to the project rate (passthrough)"
        );
    }

    // Forced device rate: ring is sized to the DEVICE rate, not the project rate, so the
    // pre-roll resampler can bridge project → device. Exercises the two-clock path.
    #[test]
    fn engine_forced_rate_sizes_ring_at_device_rate() {
        let project_rate = 44_100u32;
        let device_rate = 48_000u32;
        let engine = PlaybackEngine::new(
            project_rate,
            BackendKind::InMemoryAtRate(device_rate),
            ResamplingQuality::Balanced,
        )
        .unwrap();

        assert_eq!(engine.project_rate(), project_rate);
        assert_eq!(engine.device_rate(), device_rate);

        // Ring capacity must follow the DEVICE rate (callback drains at the device rate).
        let expected_capacity = (device_rate as u64 * RING_MS / 1000 * 2) as usize;
        let mut prod = engine.producer.lock().unwrap();
        let prod = prod.as_mut().unwrap();
        assert_eq!(
            prod.slots(),
            expected_capacity,
            "ring must be sized to the device rate, not the project rate"
        );
    }

    // InMemory backend: push frames through producer, pull via engine, assert match.
    #[test]
    fn engine_in_memory_pull_round_trip() {
        let rate = 48_000u32;
        let mut engine =
            PlaybackEngine::new(rate, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        engine.shared.playing.store(true, Ordering::SeqCst);

        // Push 4 stereo frames (8 samples) into the producer.
        let input: Vec<f32> = (0..8).map(|i| i as f32 * 0.1).collect();
        {
            let mut guard = engine.producer.lock().unwrap();
            let prod = guard.as_mut().unwrap();
            for &s in &input {
                prod.push(s).unwrap();
            }
        }

        let output = engine.pull(4);
        assert_eq!(output, input, "pull must return exactly the pushed frames");
        assert_eq!(
            engine.shared.frames_played.load(Ordering::SeqCst),
            4,
            "frames_played must be 4 after pulling 4 stereo frames"
        );
    }

    // Underrun via engine: pull more frames than the ring contains → silence pads.
    // Corresponds to test P8 (underrun → silence, never block).
    #[test]
    fn p8_underrun_silence_never_block() {
        let rate = 48_000u32;
        let mut engine =
            PlaybackEngine::new(rate, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        engine.shared.playing.store(true, Ordering::SeqCst);

        // Push only 2 stereo frames.
        let input = vec![0.1f32, 0.2, 0.3, 0.4];
        {
            let mut guard = engine.producer.lock().unwrap();
            let prod = guard.as_mut().unwrap();
            for &s in &input {
                prod.push(s).unwrap();
            }
        }

        // Pull 8 frames — must not block and must pad with silence.
        let output = engine.pull(8);
        assert_eq!(output.len(), 16, "pull always returns frames * 2 samples");
        assert_eq!(&output[0..4], &input[..], "available frames are delivered");
        assert!(
            output[4..].iter().all(|&s| s == 0.0),
            "underrun remainder must be silence"
        );
    }

    // Ring is bounded: producer cannot overflow past the pre-allocated capacity.
    // Corresponds to test P9 (bounded / back-pressure).
    #[test]
    fn p9_ring_bounded_by_capacity() {
        let rate = 8_000u32; // small rate for a manageable capacity
        let engine =
            PlaybackEngine::new(rate, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let capacity = (rate as u64 * RING_MS / 1000 * 2) as usize;

        let mut guard = engine.producer.lock().unwrap();
        let prod = guard.as_mut().unwrap();

        // Fill to capacity.
        for _ in 0..capacity {
            assert!(
                prod.push(0.0f32).is_ok(),
                "push within capacity must succeed"
            );
        }
        // One more push must fail (ring full).
        assert!(
            prod.push(0.0f32).is_err(),
            "push beyond capacity must fail (ring is bounded)"
        );
    }

    // project_pos maps device frames to project samples with round-half-up division.
    // Pins the `+ device/2` rounding bias (430:59) and the `/ device` divisor (430:80).
    #[test]
    fn project_pos_rounds_half_up_under_resampling() {
        // Identity branch (matched rates): no rounding.
        assert_eq!(project_pos(100, 50, 48_000, 48_000), 150);
        // Resampling: start + round(frames_played × project / device).
        // round(100 × 48000 / 44100) = round(108.84) = 109.
        assert_eq!(project_pos(0, 100, 48_000, 44_100), 109);
        // Exact half: (1×3 + 2/2) / 2 = 4/2 = 2 (1.5 rounds up). `-` bias → (3−1)/2 = 1.
        assert_eq!(project_pos(0, 1, 3, 2), 2);
        // `start` is added on the resampling branch too.
        assert_eq!(project_pos(1000, 1, 3, 2), 1002);
    }

    // config_matches_stereo accepts stereo configs whose [min,max] rate range contains the
    // requested rate, and rejects everything else. Pins each conjunct and inclusive bound
    // (the rate-match rule lifted out of the hardware-only CpalBackend::negotiate).
    #[test]
    fn config_matches_stereo_rules() {
        // In-range stereo, including the inclusive endpoints.
        assert!(config_matches_stereo(2, 44_100, 48_000, 48_000)); // == max
        assert!(config_matches_stereo(2, 44_100, 48_000, 44_100)); // == min
        assert!(config_matches_stereo(2, 8_000, 96_000, 48_000));
        // Wrong channel count.
        assert!(!config_matches_stereo(1, 8_000, 96_000, 48_000));
        assert!(!config_matches_stereo(6, 8_000, 96_000, 48_000));
        // Requested below the min / above the max.
        assert!(!config_matches_stereo(2, 48_000, 96_000, 44_100));
        assert!(!config_matches_stereo(2, 8_000, 44_100, 48_000));
    }
}

// ---------------------------------------------------------------------------
// Integration tests — drive `play_from` end to end over the in-memory backend
// (no audio hardware). Sibling of `tests`, so it can reach the module-private
// `run_preroll` and the `pub(crate)` `shared`/`producer` fields.
// Test groups: P = pre-roll/ring, E = events, S = stop/pause,
// R = real-time invariant, X = cross-cutting, D = device-rate resampling.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    use tempfile::TempDir;

    use crate::audio::cache::resampled_cache_path;
    use crate::audio::edl::{EdlCursor, TrackCursor};
    use crate::audio::flac::encode_flac_24;
    use crate::audio::source_provider::TrackSource;
    use crate::project::tree::ImplicitTimelineTree;
    use crate::project::turn::{encode_turn, Splice, SpliceKind, Turn};

    const RATE: u32 = 48_000;

    // --- Synthetic-project helpers -----------------------------------------

    /// A smooth, distinctive mono signal. Sine-based (FLAC-friendly — the resampled cache only
    /// ever holds smooth audio) with two incommensurate components so the value is effectively
    /// unique per frame over the lengths used here, making any seek/range/leak error detectable.
    fn signal(frames: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                let t = i as f32;
                ((t * freq).sin() * 0.7 + (t * freq * 0.37 + 1.0).sin() * 0.3) * amp
            })
            .collect()
    }

    /// Write a mono dry FLAC for track 1 and return (tempdir, vbdata, samples).
    fn synth_project(frames: usize) -> (TempDir, std::path::PathBuf, Vec<f32>) {
        write_track(frames, 1, 0.013, 0.4)
    }

    /// Write a mono dry FLAC for `track_id` with a distinctive smooth signal.
    fn write_track(
        frames: usize,
        track_id: u32,
        freq: f32,
        amp: f32,
    ) -> (TempDir, std::path::PathBuf, Vec<f32>) {
        let dir = TempDir::new().unwrap();
        let vbdata = dir.path().to_path_buf();
        std::fs::create_dir_all(vbdata.join("resampled")).unwrap();
        let data = signal(frames, freq, amp);
        encode_flac_24(&data, RATE, 1, &resampled_cache_path(&vbdata, track_id)).unwrap();
        (dir, vbdata, data)
    }

    /// One-turn timeline covering `[0, frames)` of `track_id`'s source (no edits).
    fn turn_tree(turn_id: u64, frames: i64) -> ImplicitTimelineTree<Turn> {
        let turn = Turn {
            id: turn_id,
            speaker_id: None,
            turn_duration: frames,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![Splice {
                length_samples: frames,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            }],
        };
        let (h, _) = encode_turn(&turn).unwrap();
        ImplicitTimelineTree::new()
            .insert_at(0, h, Arc::new(turn))
            .unwrap()
    }

    /// Build a fresh single-track renderer over `track_id == 1`, windowed to `[start, end)` by
    /// its `EdlCursor` (`end == None` walks to the track end). The cursor owns the play window;
    /// `play_from` no longer caps the feed.
    fn make_renderer(
        vbdata: &Path,
        tree: &ImplicitTimelineTree<Turn>,
        frames: i64,
        start: i64,
        end: Option<i64>,
    ) -> Renderer<CacheSourceProvider> {
        let cursor = TrackCursor::at(tree, 1, 0, start);
        let edl = EdlCursor::new(vec![cursor], start, end);
        let provider = CacheSourceProvider::new(
            vbdata.to_path_buf(),
            vec![TrackSource::new(1, 1, 0.0, frames, None)],
        );
        Renderer::new(edl, provider, 0, RATE)
    }

    /// Ground-truth render of `[start, end)`: a renderer windowed to the same range, drained to
    /// end-of-stream — equals the played sequence at matched rates.
    fn reference_render(
        vbdata: &Path,
        tree: &ImplicitTimelineTree<Turn>,
        frames: i64,
        start: i64,
        end: i64,
    ) -> Vec<f32> {
        let mut r = make_renderer(vbdata, tree, frames, start, Some(end));
        let mut out = Vec::new();
        loop {
            let chunk = r.render(1024).unwrap();
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
        }
        out
    }

    /// Drain a streaming resampler fully into an interleaved buffer (reference for the
    /// two-clock device-frame count).
    fn drain_resampler(rs: &mut StreamingResampler<Renderer<CacheSourceProvider>>) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; 1024 * 2];
        loop {
            let n = rs.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n * 2]);
        }
        out
    }

    // --- Driving harness ----------------------------------------------------

    /// Drive the in-memory backend until `expected` real (device) frames have been delivered,
    /// returning the exact delivered-frame sequence.
    ///
    /// Each `pull` is one callback invocation. Because the contract copies real frames as a
    /// contiguous *prefix* of the output and only then pads silence, the delta in
    /// `frames_played` after a pull tells us exactly how many of the returned samples are real —
    /// so interior underrun (the consumer outrunning the producer) is reconstructed losslessly,
    /// which also exercises P8 (underrun → silence, resume cleanly) on every run.
    fn drive(engine: &mut PlaybackEngine, expected: usize, chunk: usize) -> Vec<f32> {
        let mut captured = Vec::with_capacity(expected * 2);
        let mut prev_fp = 0u64;
        let deadline = Instant::now() + Duration::from_secs(20);
        while captured.len() / 2 < expected {
            assert!(
                Instant::now() < deadline,
                "drive timed out at {}/{expected} frames",
                captured.len() / 2
            );
            let out = engine.pull(chunk);
            let fp = engine.shared.frames_played.load(Ordering::Acquire);
            let new_real = (fp - prev_fp) as usize;
            if new_real > 0 {
                captured.extend_from_slice(&out[..new_real * 2]);
                prev_fp = fp;
            } else {
                std::thread::park_timeout(Duration::from_millis(1));
            }
        }
        captured
    }

    /// Emulate the free-running callback's inter-session flush: with `playing == false`, a single
    /// `pull` pops-and-discards the whole ring (contract step 2). Call between a stop/pause and
    /// the next `play_from` so stale already-rendered frames never leak into the new session.
    fn flush_ring(engine: &mut PlaybackEngine) {
        let _ = engine.pull(64);
    }

    type Sink = Arc<Mutex<Vec<i64>>>;
    fn sink() -> Sink {
        Arc::new(Mutex::new(Vec::new()))
    }
    fn push_pos(s: &Sink) -> impl Fn(i64) + Send + Sync + 'static {
        let s = s.clone();
        move |p| s.lock().unwrap().push(p)
    }

    // --- P — pre-roll + ring ------------------------------------------------

    // P6: a single-track play delivers exactly the renderer output for the range.
    #[test]
    fn p6_frame_sequence_matches_renderer() {
        let frames = 5000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;
        let reference = reference_render(&vbdata, &tree, frames as i64, 0, end);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let u = sink();
        let s = sink();
        let (eu, es) = (push_pos(&u), push_pos(&s));
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                move |p: PlayheadUpdate| eu(p.position_samples),
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();

        let captured = drive(&mut engine, end as usize, 512);
        engine.stop();

        assert_eq!(captured.len(), reference.len(), "P6 frame count");
        assert_eq!(
            captured, reference,
            "P6 delivered frames == renderer output"
        );
    }

    // P7: two overlapping tracks deliver the mixed/clamped renderer output.
    #[test]
    fn p7_multi_track_mixes() {
        let frames = 4000usize;
        let (_dir, vbdata, _) = write_track(frames, 1, 0.017, 0.3);
        // Second track in the same vbdata dir, distinct frequency.
        let d2 = signal(frames, 0.011, 0.25);
        encode_flac_24(&d2, RATE, 1, &resampled_cache_path(&vbdata, 2)).unwrap();
        let tree1 = turn_tree(1, frames as i64);
        let tree2 = turn_tree(2, frames as i64);
        let end = frames as i64;

        let make_multi = |vbdata: &Path| {
            let c1 = TrackCursor::at(&tree1, 1, 0, 0);
            let c2 = TrackCursor::at(&tree2, 2, 0, 0);
            let edl = EdlCursor::new(vec![c1, c2], 0, None);
            let provider = CacheSourceProvider::new(
                vbdata.to_path_buf(),
                vec![
                    TrackSource::new(1, 1, 0.0, frames as i64, None),
                    TrackSource::new(2, 1, 0.0, frames as i64, None),
                ],
            );
            Renderer::new(edl, provider, 0, RATE)
        };

        // Reference: drain the same renderer offline.
        let mut rref = make_multi(&vbdata);
        let mut reference = Vec::new();
        loop {
            let c = rref.render(1024).unwrap();
            if c.is_empty() {
                break;
            }
            reference.extend_from_slice(&c);
        }

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        engine
            .play_from(
                0,
                make_multi(&vbdata),
                |_p: PlayheadUpdate| {},
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let captured = drive(&mut engine, end as usize, 400);
        engine.stop();

        assert_eq!(captured, reference, "P7 mixed frames == renderer output");
    }

    // P10: a bounded play feeds exactly `end - start` project frames and stops — no frames past
    // `end`. After the natural stop, no further real frames are ever delivered.
    #[test]
    fn p10_exact_length_no_overrun() {
        let frames = 6000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let start = 1000i64;
        let end = 4000i64;
        let reference = reference_render(&vbdata, &tree, frames as i64, start, end);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        engine
            .play_from(
                start,
                make_renderer(&vbdata, &tree, frames as i64, start, Some(end)),
                |_p: PlayheadUpdate| {},
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let captured = drive(&mut engine, (end - start) as usize, 512);

        assert_eq!(
            captured.len() / 2,
            (end - start) as usize,
            "P10 exact device-frame count"
        );
        assert_eq!(
            captured, reference,
            "P10 frames == renderer output for the range"
        );

        // No frames past `end`: further pulls deliver only silence (frames_played frozen).
        let fp_before = engine.shared.frames_played.load(Ordering::Acquire);
        for _ in 0..4 {
            let _ = engine.pull(512);
        }
        let fp_after = engine.shared.frames_played.load(Ordering::Acquire);
        assert_eq!(fp_before, fp_after, "P10 no real frames past end_sample");
        engine.stop();
    }

    // --- E — events ---------------------------------------------------------

    // E11: playhead positions are monotonic, begin at/after start, and advance at least one
    // PLAYHEAD_INTERVAL of *played* audio between emissions.
    #[test]
    fn e11_playhead_cadence_and_monotonic() {
        let frames = 30_000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let u = sink();
        let eu = push_pos(&u);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                move |p: PlayheadUpdate| eu(p.position_samples),
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let _ = drive(&mut engine, end as usize, 256);
        engine.stop();

        let ups = u.lock().unwrap().clone();
        assert!(!ups.is_empty(), "E11 expected playhead updates");
        let interval = (RATE as u64 * PLAYHEAD_INTERVAL_MS / 1000) as i64;
        assert!(
            ups[0] >= 0 && ups[0] <= end,
            "E11 first in range: {}",
            ups[0]
        );
        for i in 1..ups.len() {
            assert!(ups[i] >= ups[i - 1], "E11 monotonic at {i}");
            assert!(ups[i] <= end, "E11 in range at {i}: {}", ups[i]);
            assert!(
                ups[i] - ups[i - 1] >= interval,
                "E11 cadence >= interval at {i}: {} -> {}",
                ups[i - 1],
                ups[i]
            );
        }
    }

    // E12: playhead reports the *played* position (`frames_played`), never the *rendered*
    // position buffered ahead in the ring. We deliver a bounded number of frames, then stop
    // consuming and let the producer fill RING_MS ahead: no emitted position may exceed what was
    // actually played (a rendered-position bug would report up to RING_MS beyond).
    #[test]
    fn e12_playhead_played_not_rendered() {
        let frames = 40_000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let u = sink();
        let eu = push_pos(&u);
        engine
            // Open-ended: the producer keeps rendering ahead after we stop consuming.
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, None),
                move |p: PlayheadUpdate| eu(p.position_samples),
                |_p: PlaybackStopped| {},
            )
            .unwrap();

        // Deliver a bounded run (a few intervals' worth), then stop consuming.
        let _ = drive(&mut engine, 5000, 256);
        let played = engine.shared.frames_played.load(Ordering::Acquire) as i64;
        // Let the producer fill the ring far ahead (rendered position ≈ played + RING_MS) and emit.
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            engine.shared.frames_played.load(Ordering::Acquire) as i64,
            played,
            "E12 consumer stopped — frames_played frozen"
        );
        engine.stop();

        let ups = u.lock().unwrap().clone();
        assert!(!ups.is_empty(), "E12 expected playhead updates");
        let max_up = *ups.iter().max().unwrap();
        let ring_frames = RATE as i64 * RING_MS as i64 / 1000;
        // Reports played (≤ played), not rendered (which would reach ≈ played + ring_frames).
        assert!(
            max_up <= played,
            "E12 playhead must report played ({played}), not rendered-ahead; max={max_up} \
             (a rendered bug would reach ~{})",
            played + ring_frames
        );
        // Non-vacuous: playback did progress past at least one interval.
        let interval = (RATE as u64 * PLAYHEAD_INTERVAL_MS / 1000) as i64;
        assert!(
            max_up >= interval,
            "E12 playhead advanced past one interval"
        );
    }

    // E13: events are emitted only from the pre-roll thread, never from the callback (which here
    // runs on the test thread via `pull`).
    #[test]
    fn e13_no_event_from_callback_thread() {
        let frames = 4000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;
        let main_id = std::thread::current().id();
        let ids = Arc::new(Mutex::new(Vec::<std::thread::ThreadId>::new()));

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let iu = ids.clone();
        let is = ids.clone();
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                move |_p: PlayheadUpdate| iu.lock().unwrap().push(std::thread::current().id()),
                move |_p: PlaybackStopped| is.lock().unwrap().push(std::thread::current().id()),
            )
            .unwrap();
        let _ = drive(&mut engine, end as usize, 256);
        engine.stop();

        let recorded = ids.lock().unwrap().clone();
        assert!(!recorded.is_empty(), "E13 expected at least one event");
        for tid in recorded {
            assert_ne!(
                tid, main_id,
                "E13 events must not fire on the callback thread"
            );
        }
    }

    // --- S — stop / pause ---------------------------------------------------

    // S14: a bounded play stops at end_sample and emits playback_stopped { end_sample } once.
    #[test]
    fn s14_stop_at_end_sample() {
        let frames = 5000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = 3500i64;

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let s = sink();
        let es = push_pos(&s);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();
        let _ = drive(&mut engine, end as usize, 400);
        engine.stop(); // joins the pre-roll thread; natural-stop emit already done.

        let stops = s.lock().unwrap().clone();
        assert_eq!(stops, vec![end], "S14 one stop emit at end_sample");
    }

    // S15: an open-ended play stops at the project/tree end and reports it.
    #[test]
    fn s15_stop_at_end_of_edl() {
        let frames = 4000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let s = sink();
        let es = push_pos(&s);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, None),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();
        let _ = drive(&mut engine, frames, 400);
        engine.stop();

        let stops = s.lock().unwrap().clone();
        assert_eq!(
            stops,
            vec![frames as i64],
            "S15 stop at end-of-EDL (project end)"
        );
    }

    // S16: a user stop mid-playback halts and reports the last *played* position (not the
    // rendered position buffered ahead in the ring).
    #[test]
    fn s16_user_stop_mid_playback() {
        let frames = 40_000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let s = sink();
        let es = push_pos(&s);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, None),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();
        // Deliver a partial run, then stop well before the end.
        let _ = drive(&mut engine, 2000, 256);
        let played = engine.shared.frames_played.load(Ordering::Acquire) as i64;
        let pos = engine.stop();

        assert_eq!(pos, played, "S16 stop returns last played position");
        assert!(
            (pos as usize) < frames,
            "S16 must stop mid-stream, not at the end"
        );
        let stops = s.lock().unwrap().clone();
        assert_eq!(
            stops,
            vec![played],
            "S16 one stop emit at the played position"
        );
    }

    // S17: pause retains position and does not emit; a following play_from resumes the sequence
    // seamlessly (matched rate ⇒ no resampler seam).
    #[test]
    fn s17_pause_retains_position_and_resumes() {
        let frames = 20_000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let full = reference_render(&vbdata, &tree, frames as i64, 0, frames as i64);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let s = sink();
        let es = push_pos(&s);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, None),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();
        let cap1 = drive(&mut engine, 3000, 256);
        // An open-ended play keeps producing, so `drive` may overshoot the 3000-frame target by
        // up to one chunk; the retained position is whatever was actually delivered.
        let played = engine.shared.frames_played.load(Ordering::Acquire) as i64;
        let pos = engine.pause();

        assert!(
            played >= 3000,
            "S17 delivered at least the requested frames"
        );
        assert_eq!(cap1.len() / 2, played as usize, "S17 captured == delivered");
        assert_eq!(
            pos, played,
            "S17 pause returns the retained (last played) position"
        );
        assert!(
            s.lock().unwrap().is_empty(),
            "S17 pause must not emit playback_stopped"
        );

        // Emulate the free-running callback flushing the buffered-ahead frames, then resume.
        flush_ring(&mut engine);
        let s2 = sink();
        let es2 = push_pos(&s2);
        engine
            .play_from(
                pos,
                make_renderer(&vbdata, &tree, frames as i64, pos, None),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es2(p.position_samples),
            )
            .unwrap();
        let cap2 = drive(&mut engine, frames - pos as usize, 256);
        engine.stop();

        let mut joined = cap1;
        joined.extend_from_slice(&cap2);
        assert_eq!(
            joined, full,
            "S17 paused+resumed sequence == continuous render"
        );
    }

    // S18: stop is idempotent — repeated stops do not double-emit or panic.
    #[test]
    fn s18_stop_idempotent() {
        let frames = 4000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let s = sink();
        let es = push_pos(&s);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();
        let _ = drive(&mut engine, end as usize, 400);
        let p1 = engine.stop();
        let p2 = engine.stop();
        let p3 = engine.stop();

        assert_eq!(p1, end);
        assert_eq!(p2, p1, "S18 idempotent stop returns last position");
        assert_eq!(p3, p1);
        assert_eq!(s.lock().unwrap().len(), 1, "S18 exactly one stop emit");
    }

    // S19: after a stop mid-stream, a new play_from plays only the new session's frames — no
    // stale frames from the stopped ring leak in.
    #[test]
    fn s19_inter_session_flush() {
        let frames = 40_000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();

        // Session 1: play a bit of [0, 8000) then stop — leaving buffered-ahead stale frames.
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(8000)),
                |_p: PlayheadUpdate| {},
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let _ = drive(&mut engine, 1000, 256);
        engine.stop();
        flush_ring(&mut engine); // free-running callback would discard the stale ring contents.

        // Session 2: a disjoint range. If stale frames leaked, the prefix would not match.
        let start2 = 20_000i64;
        let end2 = 24_000i64;
        let reference2 = reference_render(&vbdata, &tree, frames as i64, start2, end2);
        engine
            .play_from(
                start2,
                make_renderer(&vbdata, &tree, frames as i64, start2, Some(end2)),
                |_p: PlayheadUpdate| {},
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let cap2 = drive(&mut engine, (end2 - start2) as usize, 256);
        engine.stop();

        assert_eq!(cap2, reference2, "S19 new session has no stale frames");
    }

    // --- R / X — real-time + cross-cutting ---------------------------------

    // R20: the drain path is allocation-free by construction — the cpal callback / `pull` closure
    // captures only the `Consumer<f32>` and `Arc<Shared>`, and every buffer (ring + output) is
    // pre-allocated. This drives the production steady state (a reused output buffer over a live
    // ring) to assert correctness without per-call allocation; the no-alloc guarantee itself is
    // structural (reviewed against the CLAUDE.md RT invariant), not runtime-asserted.
    #[test]
    fn r20_drain_steady_state_reuses_buffers() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(2048);
        let shared = Shared::new();
        shared.playing.store(true, Ordering::SeqCst);
        let mut out = vec![0.0f32; 256]; // pre-allocated once, reused across every drain
        for round in 0..1000u32 {
            // Producer refills; the drain never grows `out` or allocates scratch.
            while prod.push((round % 7) as f32 * 0.1).is_ok() {}
            drain_contract(&mut cons, &shared, &mut out);
            assert_eq!(out.len(), 256, "R20 output buffer is fixed-size / reused");
        }
    }

    // R21: producer/consumer are SPSC — the engine holds exactly one producer (the other half is
    // owned by the backend/callback). The split is type-enforced by rtrb's move-only handles.
    #[test]
    fn r21_single_producer_handle() {
        let engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        assert!(
            engine.producer.lock().unwrap().is_some(),
            "R21 exactly one producer handle on the engine"
        );
    }

    // X22: no SQLite on the audio path — a full play runs against `CacheSourceProvider` only,
    // which holds no `Db`/journal handle (extends the no-blocking-I/O audio-path invariant onto
    // the RT path).
    #[test]
    fn x22_no_sqlite_on_audio_path() {
        let frames = 2000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;
        let reference = reference_render(&vbdata, &tree, frames as i64, 0, end);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                |_p: PlayheadUpdate| {},
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let captured = drive(&mut engine, end as usize, 400);
        engine.stop();
        assert_eq!(
            captured, reference,
            "X22 playback runs purely off the FLAC cache provider"
        );
    }

    // X23: same project + range yields the same delivered-frame sequence twice. (Event positions
    // depend on thread-scheduling checkpoints, so determinism is asserted on the frame sequence —
    // the user-observable signal — while events are checked for monotonicity/range.)
    #[test]
    fn x23_determinism_matched_rate() {
        // Longer than the ring (RING_MS) on purpose: a project that fits entirely in the
        // ring can be buffered by the pre-roll thread before the consumer pulls a single
        // frame (frames_played stays 0 → no interval update ever emitted). Exceeding the
        // ring forces producer/consumer lockstep via back-pressure, so frames_played
        // crosses several playhead intervals while the producer is still alive — making
        // the update emission deterministic across platforms.
        let frames = 24000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;

        let run = || {
            let mut engine =
                PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced)
                    .unwrap();
            let u = sink();
            let eu = push_pos(&u);
            engine
                .play_from(
                    0,
                    make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                    move |p: PlayheadUpdate| eu(p.position_samples),
                    |_p: PlaybackStopped| {},
                )
                .unwrap();
            let cap = drive(&mut engine, end as usize, 333);
            engine.stop();
            let ups = u.lock().unwrap().clone();
            (cap, ups)
        };

        let (c1, u1) = run();
        let (c2, _u2) = run();
        assert_eq!(c1, c2, "X23 deterministic delivered frames");
        assert!(
            !u1.is_empty() && u1.windows(2).all(|w| w[1] >= w[0]),
            "X23 events monotonic"
        );
    }

    // X24: the stream/ring open once and are reused across play/stop cycles — no per-play reopen,
    // ring capacity is stable, and the engine drops cleanly.
    #[test]
    fn x24_stream_lifecycle_reuse() {
        let frames = 4000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;
        let reference = reference_render(&vbdata, &tree, frames as i64, 0, end);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let cap0 = engine.producer.lock().unwrap().as_ref().unwrap().slots();

        for cycle in 0..3 {
            engine
                .play_from(
                    0,
                    make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                    |_p: PlayheadUpdate| {},
                    |_p: PlaybackStopped| {},
                )
                .unwrap();
            let captured = drive(&mut engine, end as usize, 400);
            engine.stop();
            assert_eq!(
                captured, reference,
                "X24 cycle {cycle} delivers the same frames"
            );
            flush_ring(&mut engine);
            let cap = engine.producer.lock().unwrap().as_ref().unwrap().slots();
            assert_eq!(
                cap, cap0,
                "X24 ring capacity stable (no reopen) at cycle {cycle}"
            );
        }
    }

    // --- D — device-rate resampling (two-clock) ----------------------------

    // D27: under a forced device rate, the delivered device-frame sequence equals draining a
    // StreamingResampler over the same windowed renderer directly.
    #[test]
    fn d27_resampled_frame_count() {
        let frames = 8000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let start = 0i64;
        let end = frames as i64;
        let device_rate = 44_100u32;

        // Reference: drain a resampler directly over the same windowed renderer.
        let ref_renderer = make_renderer(&vbdata, &tree, frames as i64, start, Some(end));
        let mut rs =
            StreamingResampler::new(ref_renderer, device_rate, ResamplingQuality::Balanced)
                .unwrap();
        let reference = drain_resampler(&mut rs);
        let expected = reference.len() / 2;

        let mut engine = PlaybackEngine::new(
            RATE,
            BackendKind::InMemoryAtRate(device_rate),
            ResamplingQuality::Balanced,
        )
        .unwrap();
        assert_eq!(engine.device_rate(), device_rate, "D27 forced device rate");
        engine
            .play_from(
                start,
                make_renderer(&vbdata, &tree, frames as i64, start, Some(end)),
                |_p: PlayheadUpdate| {},
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let captured = drive(&mut engine, expected, 512);
        engine.stop();

        assert_eq!(captured.len(), reference.len(), "D27 device-frame count");
        assert_eq!(captured, reference, "D27 resampled samples bit-identical");
    }

    // D28: under a forced device rate, playhead positions are reported in *project* samples
    // (device→project converted), monotonic, and bounded by end_sample.
    #[test]
    fn d28_playhead_reports_project_samples_under_resampling() {
        let frames = 12_000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let start = 0i64;
        let end = frames as i64;
        let device_rate = 32_000u32; // < project rate ⇒ fewer device frames than project frames

        let ref_renderer = make_renderer(&vbdata, &tree, frames as i64, start, Some(end));
        let mut rs =
            StreamingResampler::new(ref_renderer, device_rate, ResamplingQuality::Balanced)
                .unwrap();
        let expected = drain_resampler(&mut rs).len() / 2;

        let mut engine = PlaybackEngine::new(
            RATE,
            BackendKind::InMemoryAtRate(device_rate),
            ResamplingQuality::Balanced,
        )
        .unwrap();
        let u = sink();
        let eu = push_pos(&u);
        engine
            .play_from(
                start,
                make_renderer(&vbdata, &tree, frames as i64, start, Some(end)),
                move |p: PlayheadUpdate| eu(p.position_samples),
                |_p: PlaybackStopped| {},
            )
            .unwrap();
        let _ = drive(&mut engine, expected, 256);
        engine.stop();

        let ups = u.lock().unwrap().clone();
        assert!(!ups.is_empty(), "D28 expected playhead updates");
        for i in 0..ups.len() {
            assert!(
                ups[i] >= 0 && ups[i] <= end,
                "D28 in project range: {}",
                ups[i]
            );
            if i > 0 {
                assert!(ups[i] >= ups[i - 1], "D28 monotonic at {i}");
            }
        }
        // Positions are project samples, so the max exceeds the device-frame count.
        assert!(
            *ups.iter().max().unwrap() > expected as i64,
            "D28 positions are in project samples, not device frames"
        );
    }

    // D29: same project + range + forced device rate yields the same device-frame sequence twice.
    #[test]
    fn d29_determinism_under_resampling() {
        let frames = 8000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);
        let end = frames as i64;
        let device_rate = 44_100u32;

        let ref_renderer = make_renderer(&vbdata, &tree, frames as i64, 0, Some(end));
        let mut rs =
            StreamingResampler::new(ref_renderer, device_rate, ResamplingQuality::Balanced)
                .unwrap();
        let expected = drain_resampler(&mut rs).len() / 2;

        let run = || {
            let mut engine = PlaybackEngine::new(
                RATE,
                BackendKind::InMemoryAtRate(device_rate),
                ResamplingQuality::Balanced,
            )
            .unwrap();
            engine
                .play_from(
                    0,
                    make_renderer(&vbdata, &tree, frames as i64, 0, Some(end)),
                    |_p: PlayheadUpdate| {},
                    |_p: PlaybackStopped| {},
                )
                .unwrap();
            let cap = drive(&mut engine, expected, 400);
            engine.stop();
            cap
        };

        assert_eq!(run(), run(), "D29 deterministic resampled frames");
    }

    // S15b: the natural end-of-stream stop must emit *autonomously*, without a user stop().
    // The drain-wait breaks on `fp >= produced || stop_requested`; the `|| → &&` mutation
    // (536) makes the natural emit hostage to a user stop that never comes here.
    #[test]
    fn s15b_natural_stop_emits_without_user_stop() {
        let frames = 4000usize;
        let (_dir, vbdata, _) = synth_project(frames);
        let tree = turn_tree(1, frames as i64);

        let mut engine =
            PlaybackEngine::new(RATE, BackendKind::InMemory, ResamplingQuality::Balanced).unwrap();
        let s = sink();
        let es = push_pos(&s);
        engine
            .play_from(
                0,
                make_renderer(&vbdata, &tree, frames as i64, 0, None),
                |_p: PlayheadUpdate| {},
                move |p: PlaybackStopped| es(p.position_samples),
            )
            .unwrap();
        let _ = drive(&mut engine, frames, 400);

        // Do NOT call stop(): wait for the autonomous natural-stop emit.
        let mut stops = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            stops = s.lock().unwrap().clone();
            if !stops.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            stops,
            vec![frames as i64],
            "natural stop must emit without a user stop()"
        );
    }
}
