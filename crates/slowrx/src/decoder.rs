//! [`SstvDecoder`] — public state machine driving the decode pipeline.
//!
//! Two-pass per-image flow: VIS detection (via [`crate::vis`]) →
//! buffer audio in `Decoding` state until ~one image's worth → run
//! `crate::sync::find_sync` once to recover the slant-corrected
//! rate + line-zero `Skip` → burst-decode every row (via
//! `crate::demod::decode_one_channel_into` in the per-mode glue
//! for PD/Robot/Scottie/Martin) → emit `LineDecoded` events and a
//! final `ImageComplete`. Multi-image streaming is supported in one
//! `process()` call (issue #90).
//!
//! Forced mode ([`SstvDecoder::with_mode`]) bypasses VIS mode detection and
//! decodes exactly one image located by a caller-supplied [`DecodeWindow`]
//! (issue #114): the anchor is authoritative, absorbing a VIS header if one
//! begins there, and there is no search for where the image starts.
//!
//! Translated in spirit from slowrx's `slowrx.c::Listen()` loop +
//! `vis.c::GetVIS()` + `video.c::GetVideo()`. ISC License — see
//! `NOTICE.md`. Inline `// slowrx <file>.c:NNN` references throughout
//! point at the gitignored local reference clone under `original/slowrx/`
//! (see `clone-slowrx.sh`); verified at audit #94 (2026-05-15).

use crate::error::Result;
use crate::image::SstvImage;
use crate::modespec::SstvMode;
use crate::resample::Resampler;
use crate::sync::{find_sync, SyncTracker, SYNC_PROBE_STRIDE};

/// One observable event emitted by [`SstvDecoder::process`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SstvEvent {
    /// VIS header parsed and a known mode dispatched.
    VisDetected {
        /// Mode identified by the VIS bits.
        mode: SstvMode,
        /// Working-rate (11025 Hz) sample offset where the VIS stop bit ended.
        /// Useful for callers that want to align audio captures with decoder events.
        sample_offset: u64,
        /// Radio mistuning offset in Hz: `observed_leader_hz - 1900`. The
        /// decoder applies this offset internally to per-pixel demod so the
        /// downstream pixel band shifts with the radio's tuning. Surfaced
        /// here purely for caller diagnostics; consumers do not need to do
        /// anything with it. Translated from slowrx's `CurrentPic.HedrShift`
        /// (`vis.c` line 106 → `video.c` line 406).
        hedr_shift_hz: f64,
    },
    /// A VIS header parsed and passed parity, but its 7-bit code maps to no
    /// SSTV mode this build can decode (reserved / undefined, or a mode not
    /// yet implemented). The decoder discards the burst and resumes scanning
    /// for the next VIS — equivalent to slowrx's `printf("Unknown VIS %d")`
    /// plus its retry-`GetVIS()` loop.
    UnknownVis {
        /// The 7-bit VIS code that did not resolve.
        code: u8,
        /// Radio mistuning offset in Hz: `observed_leader_hz - 1900` (the
        /// same quantity as [`SstvEvent::VisDetected`]'s `hedr_shift_hz`).
        /// Surfaced for diagnostics; the burst is dropped, so it does not
        /// feed any decode.
        hedr_shift_hz: f64,
        /// Working-rate (11025 Hz) sample offset where the VIS stop bit ended.
        sample_offset: u64,
    },
    /// Forced-mode only: the radio-mistuning offset (Hz) resolved for the
    /// window, emitted once right after the one-shot VIS probe.
    ///
    /// `from_vis` is `true` when an absorbed VIS header supplied the offset
    /// (automatic compensation) and `false` when the caller's fallback was
    /// used. Surfaced for diagnostics/logging; the offset has already been
    /// applied to the sync bins and per-pixel demod band.
    MistuningResolved {
        /// Adopted radio-mistuning offset in Hz (`observed_leader − 1900`).
        hedr_shift_hz: f64,
        /// `true` if the offset came from a VIS header, `false` if it is the
        /// caller-supplied fallback.
        from_vis: bool,
    },
    /// One scan line completed (callers may render incrementally).
    ///
    /// For PD and Robot 72: `pixels` is fully composed at emission time
    /// (own Y/U/V or Y(odd)/Cr/Cb/Y(even) for the row).
    ///
    /// For Robot 36 / Robot 24: `pixels` reflects the image buffer state
    /// at emission time, which has a transient cross-row dependency due
    /// to chroma alternation. Each radio line writes its own Cr-or-Cb
    /// AND duplicates that chroma to the NEXT image row. So row N is
    /// emitted with: own Y, own Cr-or-Cb (per row parity), and the
    /// OTHER chroma channel duplicated from the previous radio line.
    /// Row 0 is the exception — its `Cb` channel is zero-init at
    /// emission time (no row -1 to duplicate from), giving a transient
    /// color cast on the very top row. Faithful to slowrx C, which
    /// `calloc`'s its image buffer and never writes row 0's Cb.
    /// `ImageComplete` carries the final populated state for all rows
    /// `1..image_lines` and the same row-0 Cb-zero artifact.
    LineDecoded {
        /// Mode currently being decoded.
        mode: SstvMode,
        /// 0-based row index for this line.
        line_index: u32,
        /// `line_pixels` RGB triplets for this row, taken from the
        /// in-progress image at emission time.
        pixels: Vec<[u8; 3]>,
    },
    /// Image complete (a `LineDecoded` for the final decoded row was just
    /// emitted). `partial` is `true` when the image was flushed before every
    /// row arrived — currently only [`SstvDecoder::finalize`] does this, for a
    /// mid-stream stop of real-time reception; the rows that never arrived
    /// stay black. Full-image decodes set `partial: false`. `reset()`
    /// discards in-flight images silently without emitting any event.
    ImageComplete {
        /// Final pixel buffer. When `partial` is `true`, un-decoded rows
        /// remain the black-filled default.
        image: SstvImage,
        /// `true` when the image is incomplete (flushed early by
        /// [`SstvDecoder::finalize`]); `false` for a full-image decode.
        partial: bool,
    },
}

/// A caller-specified decode window for forced-mode decoding (issue #114).
///
/// Times are in **seconds from the first sample fed to the decoder** (the
/// input recording timeline). Only one endpoint is required: the decode
/// length is the mode's nominal image duration, so the other endpoint is
/// derived automatically.
///
/// * [`DecodeWindow::starting_at`] — the anchor points at the image's first
///   line, or at the transmission start just before an optional VIS header.
///   The decoder absorbs a VIS header when one begins at the anchor;
///   otherwise the anchor itself is taken as line 0.
/// * [`DecodeWindow::ending_at`] — the anchor points at the last sample of
///   the image *data*; the start is `end − nominal image duration`.
///
/// A forced mode **always** carries a window (issue #114 follow-up): use
/// [`SstvDecoder::with_mode`] or
/// [`SstvDecoder::set_forced_mode`]. There is no mode-only
/// (whole-stream scan) entry point.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecodeWindow {
    /// Image/transmission start in seconds, if the caller anchored on the
    /// start side.
    pub start_secs: Option<f64>,
    /// Image-data end in seconds, if the caller anchored on the end side.
    pub end_secs: Option<f64>,
}

impl DecodeWindow {
    /// Anchor on the image's first line (or the transmission start just
    /// before a VIS header).
    #[must_use]
    pub fn starting_at(start_secs: f64) -> Self {
        Self {
            start_secs: Some(start_secs),
            end_secs: None,
        }
    }

    /// Anchor on the last sample of the image *data* (the VIS header is not
    /// counted). The start is derived from the mode's nominal image duration;
    /// as with any nominal-timing anchor, a transmitter clock error larger
    /// than about one line can shift the top rows.
    #[must_use]
    pub fn ending_at(end_secs: f64) -> Self {
        Self {
            start_secs: None,
            end_secs: Some(end_secs),
        }
    }
}

/// Internal state of the decoder.
enum State {
    AwaitingVis,
    /// Boxed because [`DecodingState`] contains the working FFT plans +
    /// audio buffer and dwarfs the unit `AwaitingVis` variant; clippy
    /// warns about size disparity otherwise.
    Decoding(Box<DecodingState>),
}

/// Sub-phase of a forced-mode [`DecodingState`] (issues #113/#114).
///
/// A forced window is located by the caller's `--start`/`--end` anchor, so the
/// decoder never *searches* for where the image begins. `ManualProbe` probes
/// the leading audio once for a VIS header purely to absorb it (so a
/// transmission-start anchor does not count the header toward the image);
/// when none is found the anchor itself is taken as line 0. `Collecting` then
/// accumulates one image plus margin and decodes it.
///
/// The VIS path always starts in `Collecting` (the VIS stop bit already marks
/// line 0), so this phase is only consulted when [`SstvDecoder::forced_mode`]
/// is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForcedPhase {
    /// Probe the leading audio once for a VIS header; absorb it if present,
    /// otherwise take the anchor as line 0.
    ManualProbe,
    /// Accumulate one full image plus margin, then decode.
    Collecting,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AwaitingVis => write!(f, "AwaitingVis"),
            Self::Decoding(d) => f
                .debug_struct("Decoding")
                .field("mode", &d.mode)
                .field("audio_samples", &d.audio.len())
                .field("has_sync_probes", &d.has_sync.len())
                .field("next_probe_sample", &d.next_probe_sample)
                .field("target_audio_samples", &d.target_audio_samples)
                .field("hedr_shift_hz", &d.hedr_shift_hz)
                .finish_non_exhaustive(),
        }
    }
}

/// Two-pass decoding state.
///
/// While `audio.len() < target_audio_samples`, the decoder accumulates
/// audio and probes the 1200 Hz sync band into `has_sync`. When the
/// buffer is full, [`find_sync`] runs once to recover the
/// slant-corrected rate + line-zero `Skip`; per-pair decode then runs in
/// a single fast burst, emitting [`SstvEvent::LineDecoded`] for every
/// row.
struct DecodingState {
    mode: SstvMode,
    spec: crate::modespec::ModeSpec,
    image: SstvImage,
    /// Working-rate audio captured from VIS-stop-bit forward.
    audio: Vec<f32>,
    /// Per-stride boolean track from [`SyncTracker::has_sync_at`]. One
    /// entry per [`SYNC_PROBE_STRIDE`] working-rate samples.
    has_sync: Vec<bool>,
    /// Next sample index in `audio` to probe. Always a multiple of
    /// [`SYNC_PROBE_STRIDE`].
    next_probe_sample: usize,
    /// Sync-band tracker. Constructed when `Decoding` is entered so the
    /// hedr-shift bin offsets match the detected mistuning.
    sync_tracker: SyncTracker,
    /// Radio mistuning offset in Hz extracted at VIS time. Plumbed to
    /// per-pixel demod so the pixel band shifts with radio tuning.
    hedr_shift_hz: f64,
    /// Total audio samples we must accumulate before running
    /// [`find_sync`] and per-pair decode in the batch path. Computed at
    /// state-entry as `radio_frames_per_image(spec) × line_seconds ×
    /// FINDSYNC_AUDIO_HEADROOM × work_rate` (PD packs two image rows per
    /// radio frame, so the frame count differs by channel layout).
    target_audio_samples: usize,
    /// Per-mode chroma planes side buffer.
    ///
    /// `Some([cr_plane, cb_plane])` for `SstvMode::Robot24` and
    /// `SstvMode::Robot36`. Each plane is `image_lines * line_pixels`
    /// bytes, populated as radio lines are decoded: each radio line N
    /// writes its own chroma + duplicates to the next row's chroma slot
    /// (slowrx `video.c:421-425`); RGB composition for row N reads the
    /// duplicated-from-N-1 chroma channel that the line N-1 decode
    /// wrote earlier.
    ///
    /// `None` for every other mode. PD composes RGB in-place per pair
    /// (see `mode_pd::decode_pd_line_pair`); Robot 72 and Scottie 1/2/DX
    /// also compose RGB in-place per radio line (see
    /// `mode_robot::decode_r72_line` and `mode_scottie::decode_line`).
    /// `SstvMode` is `#[non_exhaustive]`; any future mode that does
    /// need cross-radio-line chroma state will need to extend the
    /// constructor's match in `process` to opt in.
    chroma_planes: Option<[Vec<u8>; 2]>,
    /// Forced-mode acquisition phase. Always `Collecting` for the VIS path.
    /// See [`ForcedPhase`].
    phase: ForcedPhase,
    /// 渐进（实时）解码：不等整图，锁定同步后逐行产出 `LineDecoded`。
    /// 仅当 [`SstvDecoder::progressive`] 为真且非强制模式时启用。
    progressive: bool,
    /// 渐进解码当前的同步估计 `(rate, skip)`；`None` = 尚未锁定。
    sync_est: Option<(f64, i64)>,
    /// 上次重估同步时的可用线数（用于节流，见 [`PROGRESSIVE_REFINE_FRAMES`]）。
    est_frames: u32,
    /// 渐进解码下一个待解码的无线电线索引（PD 为行对）。
    next_frame: u32,
}

impl DecodingState {
    /// Probe every newly available stride window of `self.audio` against the
    /// sync tracker, extending `self.has_sync`. Idempotent: only samples at or
    /// past `self.next_probe_sample` are examined.
    ///
    /// Factored out so the forced-mode VIS probe can re-run it after adopting
    /// the header's mistuning (which rebuilds `self.sync_tracker`).
    fn probe_sync(&mut self) {
        while self.next_probe_sample + SYNC_PROBE_STRIDE * 2 <= self.audio.len() {
            let center = self.next_probe_sample + SYNC_PROBE_STRIDE / 2;
            let has = self.sync_tracker.has_sync_at(&self.audio, center);
            self.has_sync.push(has);
            self.next_probe_sample += SYNC_PROBE_STRIDE;
        }
    }
}

/// Headroom factor on the buffered audio length before [`find_sync`]
/// runs. 1.00 = exactly the nominal image length. The Hough transform
/// re-anchors the rate against whatever sync pulses are present, so
/// trailing audio beyond the last line is not strictly required. We
/// keep this knob in case future modes (Scottie pre-line skip) want to
/// pad the buffer to absorb additional offset.
const FINDSYNC_AUDIO_HEADROOM: f64 = 1.00;

/// Extra trailing audio (seconds) buffered beyond one image in *forced mode*,
/// so the sync locator and the last line's pixel windows always have audio
/// ahead of them. Applies only when a mode was forced via
/// [`SstvDecoder::with_mode`] / [`SstvDecoder::set_forced_mode`].
const FORCED_MODE_TRAILING_MARGIN_SECONDS: f64 = 1.0;

/// Length of the leading audio probed for a VIS header when a manual window
/// anchors on the start side (issue #114). A real VIS burst is ~0.6–0.9 s
/// (leader + break + bits); 1.1 s covers it with slack. When the anchor is
/// instead the image's first line this probe simply finds no header and the
/// decoder takes the anchor as line 0.
const MANUAL_VIS_PROBE_SECONDS: f64 = 1.1;

/// Number of radio lines on the wire in one image of `spec`. PD packs two
/// image rows per line; Robot and Scottie/Martin pack one.
fn radio_frames_per_image(spec: crate::modespec::ModeSpec) -> u32 {
    match spec.channel_layout {
        crate::modespec::ChannelLayout::PdYcbcr => spec.image_lines / 2,
        crate::modespec::ChannelLayout::RobotYuv
        | crate::modespec::ChannelLayout::RgbSequential => spec.image_lines,
    }
}

/// Fresh per-mode chroma side planes (zeroed). `Some` only for the
/// chroma-alternation modes (Robot 24/36) that need cross-radio-line state;
/// `None` for every other mode. Shared by image-state construction and the
/// progressive `finalize` re-decode so both start from a clean buffer.
fn fresh_chroma_planes(spec: crate::modespec::ModeSpec) -> Option<[Vec<u8>; 2]> {
    match spec.mode {
        crate::modespec::SstvMode::Robot24 | crate::modespec::SstvMode::Robot36 => {
            let n = (spec.image_lines as usize) * (spec.line_pixels as usize);
            Some([vec![0_u8; n], vec![0_u8; n]])
        }
        _ => None,
    }
}

/// Nominal airtime of one image, in seconds (un-rounded).
fn nominal_image_seconds(spec: crate::modespec::ModeSpec) -> f64 {
    f64::from(radio_frames_per_image(spec)) * spec.line_seconds
}

/// How many `ModeSpec` line-times (`spec.line_seconds` — the per-radio-line
/// duration; for PD, where a radio frame carries two image rows, that's
/// twice as many *image* scan lines) of the *just-decoded* image audio to
/// keep when re-arming the VIS detector after `ImageComplete` (issue #90 D4).
/// A back-to-back transmission's VIS leader starts right after the image's
/// last line; carrying back this much audio absorbs a fast transmitter clock
/// (4 line-times ≈ 1.5–2 % of any mode's airtime — far more than any real
/// clock error, <0.1 %) so the leader is always inside the carry-forward
/// window. For a single transmission this is just the image's last few
/// lines plus trailing silence; the fresh detector finds nothing and waits
/// for more audio.
const MULTI_IMAGE_CARRYBACK_LINES: u32 = 4;

/// 渐进（实时）解码：开始逐行产出前需要的最少无线电线数（PD 为行对）。
/// 取 2：一行即可给出 sync 下降沿以锁定 `skip`，两行更稳。
const PROGRESSIVE_MIN_FRAMES: u32 = 2;

/// 渐进（实时）解码：采用 Hough 斜率修正后的 `rate` 前需要的最少线数。
/// 线数太少时 Hough 的倾斜角估计不可靠，此时先用标称 rate（`skip` 仍取自下降沿）。
const PROGRESSIVE_RATE_LOCK_FRAMES: u32 = 8;

/// 渐进（实时）解码：每新增这么多可用线，就用当前已收的 sync 轨道重估一次
/// rate/skip（只影响尚未解码的行）。
const PROGRESSIVE_REFINE_FRAMES: u32 = 8;

/// 渐进（实时）解码：解码第 N 线时，音频至少要多覆盖这么多线，保证该线最后一个
/// 像素的 FFT 窗口有前瞻音频（否则行尾部会偏暗）。
const PROGRESSIVE_LOOKAHEAD_FRAMES: u32 = 2;

/// `|c| crate::modespec::lookup(c).is_some()` as an `fn` pointer — the
/// "is this VIS code one we can decode?" predicate handed to every
/// [`crate::vis::VisDetector`] (issue #89 A3). The closure captures nothing,
/// so it coerces to `fn(u8) -> bool` in const context.
const IS_KNOWN_VIS: fn(u8) -> bool = |c| crate::modespec::lookup(c).is_some();

/// Streaming SSTV decoder. Push audio buffers in via
/// [`Self::process`]; consume the returned events.
pub struct SstvDecoder {
    resampler: Resampler,
    vis: crate::vis::VisDetector,
    channel_demod: crate::demod::ChannelDemod,
    /// SNR estimator. Owns its own FFT plan (separate from `channel_demod`)
    /// so the per-pixel demod's scratch buffer is never aliased. SNR
    /// is re-estimated periodically inside
    /// [`crate::mode_pd::decode_pd_line_pair`] (every
    /// [`crate::demod::SNR_REESTIMATE_STRIDE`] samples).
    snr_est: crate::snr::SnrEstimator,
    /// Scratch buffers for `find_sync` (`sync_img` / `lines` / `x_acc`).
    /// Hoisted here so they're reused across decode passes instead of
    /// being allocated fresh per call. (Audit #93 D6.)
    find_sync_scratch: crate::sync::FindSyncScratch,
    state: State,
    samples_processed: u64,
    /// Cumulative working-rate samples emitted by the resampler.
    /// Used as the unit for `SstvEvent::VisDetected.sample_offset` so
    /// that value is consistent regardless of caller's input rate.
    ///
    /// **Informational only** — this counter counts samples the resampler
    /// has produced and does NOT get decremented when
    /// [`crate::vis::VisDetector::take_residual_buffer`] transfers post-stop-bit
    /// audio back to the decoder's `Decoding` state. Those residual samples
    /// were already counted here when the resampler emitted them; the
    /// residual transfer is a borrow, not a retraction. Consequently the
    /// counter may be slightly ahead of what the image decoder has consumed.
    ///
    /// The counter is used to anchor the VIS detector each time a fresh one
    /// is constructed (initial, post-image, post-unknown-VIS). Note: for the
    /// *first* detection on a freshly-built decoder, `DetectedVis::end_sample`
    /// (→ `SstvEvent::VisDetected.sample_offset` / `UnknownVis.sample_offset`)
    /// is an absolute working-rate index from sample 0. After a *restart*
    /// (post-image — see `restart_vis_detection` — or post-unknown-VIS) the
    /// fresh detector counts hops from 0, so a later detection's `sample_offset`
    /// is relative to where the carry-forward audio began, not absolute. That
    /// is acceptable — `sample_offset` is informational only — and tracked in
    /// issue #99 (fixing it needs a `VisDetector` API change). The counter
    /// here does not gate any decode logic.
    ///
    /// If mid-image VIS detection is ever re-activated (see the TODO in
    /// `process`), and a single `SstvDecoder` is reused across detections,
    /// the slight inflation is harmless: each new detection uses the then-
    /// current resampler-output count as its anchor, and the residual buffer
    /// is handed to a fresh `VisDetector::new()`.
    ///
    /// Closes #29 and #34 (both are the same observation from different angles).
    working_samples_emitted: u64,
    /// When `Some`, VIS detection is bypassed and exactly one image is decoded
    /// as this mode, located by the forced window (issue #113/#114).
    /// Set at construction via [`SstvDecoder::with_mode`] or at
    /// runtime via [`SstvDecoder::set_forced_mode`]; cleared via
    /// [`SstvDecoder::clear_forced_mode`]. `None` restores automatic VIS
    /// detection. Always `Some`/`None` in lock-step with `forced_window`.
    forced_mode: Option<SstvMode>,
    /// The caller-specified window for the forced mode (issue #114). Always
    /// `Some` exactly when [`Self::forced_mode`] is `Some`.
    forced_window: Option<DecodeWindow>,
    /// Fallback radio-mistuning offset (Hz) for a forced mode when the window
    /// carries no usable VIS header. When a VIS header *is* absorbed its own
    /// detected `hedr_shift_hz` takes precedence; this value is only used as
    /// the initial estimate passed to [`Self::start_decoding`].
    forced_hedr_shift_hz: f64,
    /// `true` once the single manual-window image has been attempted, so
    /// subsequent audio is ignored.
    manual_done: bool,
    /// Input-rate samples still to discard before feeding the resampler;
    /// positions the manual window's anchor.
    manual_skip_input: u64,
    /// Maximum number of input-rate samples to feed after the anchor. Caps the
    /// window so a wrong anchor cannot drift onto a later image.
    manual_feed_budget: Option<u64>,
    /// 渐进（实时）解码开关。见 [`Self::set_progressive`]。
    progressive: bool,
}

impl SstvDecoder {
    /// Construct a decoder consuming audio at `input_sample_rate_hz`.
    ///
    /// # Errors
    /// Returns [`crate::Error::InvalidSampleRate`] if the rate is 0 or
    /// > [`crate::resample::MAX_INPUT_SAMPLE_RATE_HZ`].
    pub fn new(input_sample_rate_hz: u32) -> Result<Self> {
        Ok(Self {
            resampler: Resampler::new(input_sample_rate_hz)?,
            vis: crate::vis::VisDetector::new(IS_KNOWN_VIS),
            channel_demod: crate::demod::ChannelDemod::new(),
            snr_est: crate::snr::SnrEstimator::new(),
            find_sync_scratch: crate::sync::FindSyncScratch::new(),
            state: State::AwaitingVis,
            samples_processed: 0,
            working_samples_emitted: 0,
            forced_mode: None,
            forced_window: None,
            forced_hedr_shift_hz: 0.0,
            manual_done: false,
            manual_skip_input: 0,
            manual_feed_budget: None,
            progressive: false,
        })
    }

    /// Construct a forced-mode decoder restricted to a [`DecodeWindow`]
    /// (issues #113/#114).
    ///
    /// This is the **only** way to force a mode: a forced mode always carries
    /// a window, so there is no "scan the whole stream" entry point. It decodes
    /// **one** image located by the caller:
    ///
    /// * [`DecodeWindow::starting_at`] anchors on the image's first line or
    ///   on the transmission start. If a VIS header begins at the anchor it
    ///   is detected and absorbed (its duration is skipped) so the visible
    ///   image is not shifted; if no usable header is present the anchor
    ///   itself is taken as line 0.
    /// * [`DecodeWindow::ending_at`] anchors on the image-data end; the start
    ///   is `end − nominal image duration`.
    ///
    /// The decode length is always the mode's **nominal image duration**
    /// (VIS not counted), so only one endpoint is needed. The decoder does
    /// **not** search for where the image begins: the anchor is authoritative
    /// (after an optional VIS absorption). After this image is attempted the
    /// decoder stops.
    ///
    /// The window's sync gate still applies: a window with no detectable sync
    /// pulses yields no image. No [`SstvEvent::VisDetected`] is emitted even
    /// when a header is absorbed (the forced mode, not the header, selects the
    /// mode).
    ///
    /// **Mistuning:** a VIS header beginning at the anchor is probed for its
    /// leader frequency, so the detected `hedr_shift_hz` is adopted and the
    /// pixel demod band (and sync bins) shift with the real tuning. When no
    /// header is present the offset defaults to zero; use
    /// [`Self::with_mode_and_hedr_shift`] to supply a known fallback. Either
    /// way a single [`SstvEvent::MistuningResolved`] reports the offset used.
    ///
    /// # Errors
    /// Returns [`crate::Error::InvalidSampleRate`] if the rate is 0 or
    /// > [`crate::resample::MAX_INPUT_SAMPLE_RATE_HZ`].
    pub fn with_mode(
        input_sample_rate_hz: u32,
        mode: SstvMode,
        window: DecodeWindow,
    ) -> Result<Self> {
        Self::with_mode_and_hedr_shift(input_sample_rate_hz, mode, window, 0.0)
    }

    /// Construct a forced-mode decoder like [`Self::with_mode`], but with an
    /// explicit `hedr_shift_hz` fallback for windows that contain no VIS
    /// header.
    ///
    /// The fallback is the radio's mistuning offset (`observed 1900 Hz leader
    /// − 1900`); it is used until/unless a VIS header at the anchor is
    /// detected, in which case the detected offset supersedes it.
    ///
    /// # Errors
    /// Returns [`crate::Error::InvalidSampleRate`] if the rate is 0 or
    /// > [`crate::resample::MAX_INPUT_SAMPLE_RATE_HZ`].
    pub fn with_mode_and_hedr_shift(
        input_sample_rate_hz: u32,
        mode: SstvMode,
        window: DecodeWindow,
        hedr_shift_hz: f64,
    ) -> Result<Self> {
        let mut decoder = Self::new(input_sample_rate_hz)?;
        decoder.forced_mode = Some(mode);
        decoder.forced_window = Some(window);
        decoder.forced_hedr_shift_hz = hedr_shift_hz;
        decoder.refresh_manual_window();
        Ok(decoder)
    }

    /// Set the forced mode and its [`DecodeWindow`] together, discarding any
    /// in-flight image.
    ///
    /// Both are required: a forced mode without a window is not a supported
    /// state. Use [`Self::clear_forced_mode`] to return to automatic VIS
    /// detection.
    ///
    /// The fallback mistuning offset set via [`Self::set_forced_hedr_shift_hz`]
    /// is left unchanged.
    pub fn set_forced_mode(&mut self, mode: SstvMode, window: DecodeWindow) {
        self.forced_mode = Some(mode);
        self.forced_window = Some(window);
        // Discard any in-flight image and restart detection with a fresh
        // detector (honors the `#40` re-anchor contract).
        self.state = State::AwaitingVis;
        self.vis = crate::vis::VisDetector::new(IS_KNOWN_VIS);
        self.refresh_manual_window();
    }

    /// Set the fallback radio-mistuning offset (Hz) for the forced mode.
    ///
    /// Used when the window contains no VIS header; a detected header's own
    /// offset always wins. Store it before feeding audio so the decoder starts
    /// with the right demod band. Passing zero restores the untuned default.
    pub fn set_forced_hedr_shift_hz(&mut self, hedr_shift_hz: f64) {
        self.forced_hedr_shift_hz = hedr_shift_hz;
    }

    /// The forced-mode fallback mistuning offset (Hz); zero unless set via
    /// [`Self::set_forced_hedr_shift_hz`] / [`Self::with_mode_and_hedr_shift`].
    #[must_use]
    pub fn forced_hedr_shift_hz(&self) -> f64 {
        self.forced_hedr_shift_hz
    }

    /// Clear the forced mode and window, restoring automatic VIS detection.
    /// Any in-flight image is discarded.
    pub fn clear_forced_mode(&mut self) {
        self.forced_mode = None;
        self.forced_window = None;
        self.state = State::AwaitingVis;
        self.vis = crate::vis::VisDetector::new(IS_KNOWN_VIS);
        self.refresh_manual_window();
    }

    /// The manual decode window, if one is set.
    #[must_use]
    pub fn decode_window(&self) -> Option<DecodeWindow> {
        self.forced_window
    }

    /// Recompute the manual-window input skip/budget from the current forced
    /// mode + window. Called whenever either changes so `process` only has to
    /// apply precomputed counters.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn refresh_manual_window(&mut self) {
        self.manual_done = false;
        self.manual_skip_input = 0;
        self.manual_feed_budget = None;
        let (Some(mode), Some(window)) = (self.forced_mode, self.forced_window) else {
            return;
        };
        let image_secs = nominal_image_seconds(crate::modespec::for_mode(mode));
        // `start_secs` is authoritative; with only `end_secs` the anchor is
        // one image back from the image-data end (VIS not counted).
        let anchor_secs = match (window.start_secs, window.end_secs) {
            (Some(start), _) => start,
            (None, Some(end)) => (end - image_secs).max(0.0),
            (None, None) => {
                // Malformed window: clear the forced mode rather than leave
                // the (mode, window) invariant broken.
                self.forced_mode = None;
                self.forced_window = None;
                return;
            }
        };
        let anchor_secs = anchor_secs.max(0.0);
        let input_rate = f64::from(self.resampler.input_rate());
        let anchor_input = (anchor_secs * input_rate).round() as u64;
        self.manual_skip_input = anchor_input.saturating_sub(self.samples_processed);
        // Allow a VIS header + one image + trailing margin after the anchor.
        let budget_secs =
            MANUAL_VIS_PROBE_SECONDS + image_secs + FORCED_MODE_TRAILING_MARGIN_SECONDS;
        self.manual_feed_budget = Some((budget_secs * input_rate).round() as u64);
    }

    /// The mode forced for decoding, if any. `None` means automatic VIS
    /// detection.
    #[must_use]
    pub fn forced_mode(&self) -> Option<SstvMode> {
        self.forced_mode
    }

    /// 开启/关闭**渐进（实时）解码**（默认关闭）。
    ///
    /// 关闭时：攒满约一整张图的音频后一次性爆发解码（离线批处理）。
    /// 开启时：从引导音频里锁定 `rate`/`skip` 后，**每收够一行的音频就解一行**
    /// 并发出 [`SstvEvent::LineDecoded`]，整图末尾再发 [`SstvEvent::ImageComplete`]；
    /// 延迟约 1–2 行，且把解码 CPU 摊到整段接收时间上（不再有整图爆发卡顿）。
    ///
    /// 渐进模式下早期若干行使用的是「仅前几行 sync」估计的 rate/skip；随着音频
    /// 到达会用更多 sync 重估并用于后续行。对常见的小时钟误差（<0.05%）与批处理
    /// 结果基本一致。强制模式（[`Self::with_mode`]）始终走批处理路径。
    ///
    /// 设为 `true` 不会立即影响已在进行中的图像，只影响之后开始的图像。
    pub fn set_progressive(&mut self, enabled: bool) {
        self.progressive = enabled;
    }

    /// 是否开启渐进（实时）解码。
    #[must_use]
    pub fn progressive(&self) -> bool {
        self.progressive
    }

    /// Process a chunk of mono `f32` audio samples in caller's rate.
    ///
    /// Returns events produced during this call's processing window.
    // `too_many_lines`: `process` is the decoder's state-machine loop; splitting
    // it (e.g. extracting `DecodingState::new`) is tracked in the code-review
    // audit (B14, epic #97).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    pub fn process(&mut self, audio: &[f32]) -> Vec<SstvEvent> {
        self.samples_processed = self.samples_processed.saturating_add(audio.len() as u64);

        // Manual window (issue #114): drop everything before the anchor, then
        // hard-cap the feed at one image's worth so a wrong anchor cannot
        // drift forward onto a later transmission. Both counters are inert
        // (zero / `None`) unless a window is set.
        let mut feed: &[f32] = audio;
        if self.manual_skip_input > 0 {
            let drop = (self.manual_skip_input as usize).min(feed.len());
            feed = &feed[drop..];
            self.manual_skip_input -= drop as u64;
        }
        if let Some(budget) = self.manual_feed_budget.as_mut() {
            let take = (*budget as usize).min(feed.len());
            feed = &feed[..take];
            *budget -= take as u64;
        }

        let working = self.resampler.process(feed);
        self.working_samples_emitted = self
            .working_samples_emitted
            .saturating_add(working.len() as u64);

        let mut out = Vec::new();
        let mut remaining: &[f32] = working.as_slice();
        loop {
            match &mut self.state {
                State::AwaitingVis => {
                    // Forced mode always carries a window (issue #114) and
                    // anchors at the caller's position. `ManualProbe` probes
                    // once for a VIS header to absorb; the anchor is otherwise
                    // authoritative (no search for where the image begins).
                    // Once its single image has been attempted the decoder
                    // stops. Mistuning starts from the caller's fallback and is
                    // overridden by any VIS header the probe finds.
                    if let Some(mode) = self.forced_mode {
                        if self.manual_done {
                            break;
                        }
                        let spec = crate::modespec::for_mode(mode);
                        self.state = Self::start_decoding(
                            spec,
                            self.forced_hedr_shift_hz,
                            Vec::new(),
                            0,
                            ForcedPhase::ManualProbe,
                            false,
                        );
                        continue;
                    }
                    self.vis.process(remaining, self.working_samples_emitted);
                    remaining = &[];
                    if let Some(detected) = self.vis.take_detected() {
                        if let Some(spec) = crate::modespec::lookup(detected.code) {
                            out.push(SstvEvent::VisDetected {
                                mode: spec.mode,
                                sample_offset: detected.end_sample,
                                hedr_shift_hz: detected.hedr_shift_hz,
                            });
                            // Recover any post-stop-bit audio that the VIS
                            // detector buffered but did not consume — it is
                            // the leading edge of the image data.
                            let residual = self.vis.take_residual_buffer();
                            let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
                            let nominal_samples =
                                (nominal_image_seconds(spec) * work_rate) as usize;
                            let target =
                                ((nominal_samples as f64) * FINDSYNC_AUDIO_HEADROOM) as usize;
                            self.state = Self::start_decoding(
                                spec,
                                detected.hedr_shift_hz,
                                residual,
                                target,
                                ForcedPhase::Collecting,
                                self.progressive,
                            );
                            continue; // re-enter loop to process leftover audio
                        }
                        // Unknown VIS code: surface it so stream-monitoring
                        // callers know a burst arrived, then reseed the
                        // detector (the `#40` re-anchor contract) on the
                        // post-stop-bit residue and re-enter the loop — a
                        // back-to-back VIS in the residue then surfaces in
                        // this same `process` call. Mirrors the known-code
                        // branch's `continue`.
                        out.push(SstvEvent::UnknownVis {
                            code: detected.code,
                            hedr_shift_hz: detected.hedr_shift_hz,
                            sample_offset: detected.end_sample,
                        });
                        let residual = self.vis.take_residual_buffer();
                        Self::restart_vis_detection(
                            &mut self.vis,
                            self.working_samples_emitted,
                            &residual,
                        );
                        continue;
                    }
                    break;
                }
                State::Decoding(d) => {
                    let forced = self.forced_mode.is_some();
                    // TODO(future): mid-image VIS detection. When a new VIS
                    // burst arrives during decoding the spec calls for flushing
                    // the in-flight image as `partial: true` and restarting.
                    // The straightforward approach — running `self.vis` against
                    // `audio` each call — fails because the decoding buffer is
                    // not aligned to 30 ms window boundaries: the residual from
                    // the previous VIS detection starts at an arbitrary sample
                    // offset, so the first classifier window is a mix of silence
                    // and leader tone and does not reliably pass the 5× dominance
                    // threshold. A correct implementation would re-align the VIS
                    // window scan to the next 30 ms boundary, or run a separate
                    // correlator tuned to the 1900 Hz leader. Deferred to PR-3.

                    d.audio.extend_from_slice(remaining);

                    // Probe sync-band for every newly available stride
                    // window. The probe needs SYNC_FFT_WINDOW_SAMPLES/2
                    // trailing samples beyond the center; rather than
                    // depend on that constant, we conservatively wait
                    // until the audio extends `SYNC_PROBE_STRIDE * 2`
                    // beyond the next probe center.
                    d.probe_sync();

                    // Forced window (issue #114): probe the leading audio once
                    // for a VIS header. A real header marks the image's start
                    // as its stop-bit end, so the header is absorbed (not
                    // counted toward the image length). When no header is
                    // present, the caller's anchor itself is taken as line 0 —
                    // the decoder does not search for where the image begins.
                    if forced && d.phase == ForcedPhase::ManualProbe {
                        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
                        let probe_len = (MANUAL_VIS_PROBE_SECONDS * work_rate) as usize;
                        if d.audio.len() < probe_len {
                            break; // need a full probe window
                        }
                        let mut det = crate::vis::VisDetector::new(IS_KNOWN_VIS);
                        det.process(&d.audio[..probe_len], probe_len as u64);
                        if let Some(detected) = det.take_detected() {
                            // Drain a whole number of probes so `has_sync`
                            // stays aligned with `audio`.
                            let drain_probes = ((detected.end_sample as usize) / SYNC_PROBE_STRIDE)
                                .min(d.has_sync.len());
                            let drain = drain_probes * SYNC_PROBE_STRIDE;
                            d.audio.drain(0..drain);
                            // The absorbed header carries the radio's
                            // mistuning. Adopt its offset so both the sync
                            // bins and the per-pixel demod band shift with the
                            // real tuning, then re-probe the (small) buffered
                            // audio against the corrected tracker — the first
                            // probe pass ran with the caller's fallback.
                            d.hedr_shift_hz = detected.hedr_shift_hz;
                            d.sync_tracker = SyncTracker::new(detected.hedr_shift_hz);
                            d.has_sync.clear();
                            d.next_probe_sample = 0;
                            d.probe_sync();
                            out.push(SstvEvent::MistuningResolved {
                                hedr_shift_hz: detected.hedr_shift_hz,
                                from_vis: true,
                            });
                        } else {
                            out.push(SstvEvent::MistuningResolved {
                                hedr_shift_hz: d.hedr_shift_hz,
                                from_vis: false,
                            });
                        }
                        let nominal_samples = (nominal_image_seconds(d.spec) * work_rate) as usize;
                        let margin_samples =
                            (FORCED_MODE_TRAILING_MARGIN_SECONDS * work_rate) as usize;
                        d.target_audio_samples = nominal_samples + margin_samples;
                        d.phase = ForcedPhase::Collecting;
                    }

                    // 渐进（实时）解码：锁定同步后逐行产出，不等整图。强制模式
                    // 仍走下面的批处理路径。
                    let progressive_now = d.progressive && !forced;
                    if progressive_now {
                        let State::Decoding(d_box) =
                            std::mem::replace(&mut self.state, State::AwaitingVis)
                        else {
                            unreachable!("outer match arm is Decoding");
                        };
                        let mut d = *d_box;
                        let finished = Self::progressive_step(
                            &mut d,
                            &mut self.find_sync_scratch,
                            &mut self.channel_demod,
                            &mut self.snr_est,
                            &mut out,
                        );
                        if finished {
                            // 预览已经逐行出全图。最终整图解码交给下面的批处理
                            // 路径：用**整段** sync 轨道重跑 find_sync 并整图重解，
                            // 保证输出与离线批处理逐像素一致（预览行用的是局部
                            // 估计，可能略有偏差，会被这次重解覆盖）。关闭渐进
                            // 标志后放回状态，下轮循环即走批处理。
                            d.progressive = false;
                            self.state = State::Decoding(Box::new(d));
                            remaining = &[];
                            continue;
                        }
                        self.state = State::Decoding(Box::new(d));
                        break;
                    }

                    if d.audio.len() < d.target_audio_samples {
                        break;
                    }

                    // Buffer is full → run FindSync once, then decode every
                    // line. For VIS mode we capture the carryback audio before
                    // moving `d` into `run_findsync_and_decode` (which consumes
                    // it by value so `d.image` can move directly into the
                    // `ImageComplete` event without a fresh black-image
                    // realloc — Audit #93 D6.2).
                    let carry_audio: Vec<f32> = if forced {
                        Vec::new()
                    } else {
                        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
                        let carryback = (f64::from(MULTI_IMAGE_CARRYBACK_LINES)
                            * d.spec.line_seconds
                            * work_rate) as usize;
                        let carry_from = d.target_audio_samples.saturating_sub(carryback);
                        d.audio[carry_from..].to_vec()
                    };
                    // Now extract the Box<DecodingState> by value. The
                    // `mem::replace` swap leaves `self.state` as `AwaitingVis`
                    // so the next loop iteration re-enters the AwaitingVis arm
                    // (no explicit assignment needed below).
                    let State::Decoding(d_box) =
                        std::mem::replace(&mut self.state, State::AwaitingVis)
                    else {
                        unreachable!("outer match arm is Decoding");
                    };
                    Self::run_findsync_and_decode(
                        *d_box,
                        &mut self.channel_demod,
                        &mut self.snr_est,
                        &mut self.find_sync_scratch,
                        &mut out,
                        forced,
                    );

                    if forced {
                        // Forced mode is a one-shot window (issue #114): stop
                        // after the single image. `AwaitingVis` observes
                        // `manual_done` and breaks.
                        self.manual_done = true;
                    } else {
                        // Image complete. Re-arm VIS detection in place (no
                        // `break`! — the loop re-iterates into `AwaitingVis`) — a
                        // back-to-back transmission's VIS leader starts right after
                        // this image's last line, so feed the fresh detector only
                        // the tail of the image audio (a few lines, to absorb a
                        // fast TX clock) plus everything past it; the rest is
                        // decoded video tones a VIS burst can't hide in. Falling
                        // through (vs the old `break`) means that next VIS — and
                        // the image after it — surface in this same `process()`
                        // call, mirroring the known/unknown-code branches above.
                        // Closes #31; #90 (A2 + D4). (`sample_offset` on detections
                        // after the first is relative to the carry-forward start,
                        // not absolute — #99.)
                        Self::restart_vis_detection(
                            &mut self.vis,
                            self.working_samples_emitted,
                            &carry_audio,
                        );
                    }
                    remaining = &[]; // already folded into d.audio → now inside the fresh detector
                }
            }
        }
        out
    }

    /// Discard `vis` and start a fresh detector on `leftover_audio`
    /// (post-stop-bit residue, or trailing image audio). Honors the `#40`
    /// re-anchor contract documented on
    /// [`crate::vis::VisDetector::take_residual_buffer`] — a spent detector's
    /// `hops_completed` / `history` state is never reset, so it must be
    /// replaced rather than re-used. `working_samples_emitted` is the
    /// decoder's running working-rate output count (used to anchor the fresh
    /// detector).
    fn restart_vis_detection(
        vis: &mut crate::vis::VisDetector,
        working_samples_emitted: u64,
        leftover_audio: &[f32],
    ) {
        *vis = crate::vis::VisDetector::new(IS_KNOWN_VIS);
        vis.process(leftover_audio, working_samples_emitted);
    }

    /// Build the [`State::Decoding`] for `spec` with `residual` already
    /// buffered and a total `target_audio_samples` window. Shared by the VIS
    /// path (residual = post-stop-bit audio, phase = `Collecting`) and the
    /// forced-mode path (residual = empty, phase = `ManualProbe`, which sizes
    /// the window once the one-shot VIS probe resolves).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn start_decoding(
        spec: crate::modespec::ModeSpec,
        hedr_shift_hz: f64,
        residual: Vec<f32>,
        target_audio_samples: usize,
        phase: ForcedPhase,
        progressive: bool,
    ) -> State {
        let image = SstvImage::new(spec.mode, spec.line_pixels, spec.image_lines);
        State::Decoding(Box::new(DecodingState {
            mode: spec.mode,
            spec,
            image,
            audio: {
                // D6.1: keep the residual move + pre-reserve the remaining
                // capacity. Avoids copying residual bytes AND avoids Vec
                // growth reallocs over the burst.
                let mut v = residual;
                v.reserve(target_audio_samples.saturating_sub(v.len()));
                v
            },
            has_sync: Vec::with_capacity(target_audio_samples / SYNC_PROBE_STRIDE),
            next_probe_sample: 0,
            sync_tracker: SyncTracker::new(hedr_shift_hz),
            hedr_shift_hz,
            target_audio_samples,
            phase,
            chroma_planes: fresh_chroma_planes(spec),
            progressive,
            sync_est: None,
            est_frames: 0,
            next_frame: 0,
        }))
    }

    /// Run [`find_sync`] over the buffered sync track, then decode every
    /// PD line pair against the corrected `(rate, skip)`. Pushes
    /// [`SstvEvent::LineDecoded`] for every row + a final
    /// [`SstvEvent::ImageComplete`] into `out`.
    ///
    /// **Lookahead note (#33):** Each call to
    /// [`crate::mode_pd::decode_pd_line_pair`] receives `&d.audio` — the
    /// entire image audio buffer, not a slice ending at the pair's nominal
    /// end sample. This means the FFT window for the last pixel of the last
    /// channel of each line pair can freely extend rightward into subsequent
    /// pair audio (or zero if the buffer ends). The lookahead is therefore
    /// *implicit*: the full-buffer pass-through provides the context that a
    /// naive `&audio[..pair_end]` slice would lose. No explicit `lookahead`
    /// variable is required, and none should be added. (Issue #33 noted
    /// a now-deleted `lookahead` variable that was dead code; Phase 3's
    /// rewrite eliminated it by design.)
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::too_many_lines
    )]
    fn run_findsync_and_decode(
        mut d: DecodingState,
        channel_demod: &mut crate::demod::ChannelDemod,
        snr_est: &mut crate::snr::SnrEstimator,
        find_sync_scratch: &mut crate::sync::FindSyncScratch,
        out: &mut Vec<SstvEvent>,
        forced: bool,
    ) {
        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
        let result = find_sync(&d.has_sync, work_rate, d.spec, find_sync_scratch);
        let rate = result.adjusted_rate_hz;
        // A Hough peak means at least some sync pulses registered. In VIS mode
        // this is informational; in a forced window it gates emission so a
        // window of silence does not produce a black "image".
        let sync_found = result.slant_deg.is_some();
        if forced && !sync_found {
            // Forced window on a sync-less window (silence, noise, or trailing
            // padding): emit nothing. The one-shot decoder then stops.
            return;
        }

        // The VIS path starts at the stop bit and a forced window at the
        // caller's anchor (after any absorbed VIS header), so `find_sync`'s
        // line-relative skip is already relative to line 0 for both.
        let skip = result.skip_samples;

        // 逐行相位跟踪：用音频里实测的行同步位置替代 `skip + n×line` 的
        // 等差外推，消除发射端行周期偏差（几十 ppm）在 250 行上的累积。
        let frame_starts = Self::compute_frame_starts(&d, skip, rate);

        // Image-complete burst: image_lines LineDecoded events + 1 ImageComplete.
        // Pre-reserve to avoid Vec growth reallocs. (Audit #93 D5.)
        out.reserve(d.spec.image_lines as usize + 1);
        let total_frames = radio_frames_per_image(d.spec);
        Self::decode_frame_range(
            &mut d,
            skip,
            rate,
            &frame_starts,
            0,
            total_frames,
            out,
            channel_demod,
            snr_est,
        );

        // Move the now-populated image into the ImageComplete event. The
        // by-value `d` (audit #93 D6.2) lets `d.image` move directly
        // into the event without a fresh black-image realloc.
        out.push(SstvEvent::ImageComplete {
            image: d.image,
            partial: false,
        });
    }

    /// Decode radio frames `[first_frame, end_frame)` into `d.image`, pushing a
    /// [`SstvEvent::LineDecoded`] per image row.
    ///
    /// `first_frame`/`end_frame` are in **radio-frame** units: line pairs for PD
    /// (two image rows each), image lines for Robot/Scottie/Martin. `end_frame`
    /// is clamped to the mode's frame count, so callers may pass the nominal
    /// total for a full-image burst or a smaller partial bound for progressive
    /// decoding.
    ///
    /// `frame_starts` 是逐行相位跟踪给出的每帧起点（工作率样本，见
    /// [`crate::sync::track_line_starts`]）。传空切片时退回 `skip + n ×
    /// line_seconds × rate` 的全局等差模型。无论哪条路径，像素时刻都仍是
    /// `skip + round(rate × (帧内偏移 + 通道起点 + …))` 的单次 `round()`，
    /// 与 slowrx `video.c:140-142` 一致。
    ///
    /// `d.audio` is passed whole to each per-line decoder (never sliced), so the
    /// last pixel's FFT window keeps its implicit rightward lookahead — see the
    /// lookahead note on [`Self::run_findsync_and_decode`].
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_arguments
    )]
    fn decode_frame_range(
        d: &mut DecodingState,
        skip: i64,
        rate: f64,
        frame_starts: &[f64],
        first_frame: u32,
        end_frame: u32,
        out: &mut Vec<SstvEvent>,
        channel_demod: &mut crate::demod::ChannelDemod,
        snr_est: &mut crate::snr::SnrEstimator,
    ) {
        let line_pixels = d.spec.line_pixels as usize;
        // 帧内时间偏移（秒）：逐行实测优先，缺失/空表退回 `帧号 × 行长`。
        let frame_offset = |frame: u32| -> f64 {
            frame_starts.get(frame as usize).map_or_else(
                || f64::from(frame) * d.spec.line_seconds,
                |&t| (t - skip as f64) / rate,
            )
        };
        match d.spec.channel_layout {
            crate::modespec::ChannelLayout::PdYcbcr => {
                let pair_count = (d.spec.image_lines / 2).min(end_frame);
                for pair in first_frame..pair_count {
                    // slowrx `video.c:140-142` computes pixel time as
                    // `Skip + round(Rate * (y/2 * LineTime + ChanStart +
                    // PixelTime * (x + 0.5)))`. `pair_seconds` 就是这里的
                    // `y/2 * LineTime`（未取整），交给
                    // [`crate::mode_pd::decode_pd_line_pair`] 折进它自己的
                    // `round()`，因此逐对的取整误差不会累积；逐行跟踪时它
                    // 换成实测起点相对 `skip` 的偏移。
                    let pair_seconds = frame_offset(pair);
                    crate::mode_pd::decode_pd_line_pair(
                        d.spec,
                        pair,
                        &d.audio,
                        skip,
                        pair_seconds,
                        rate,
                        &mut d.image,
                        channel_demod,
                        snr_est,
                        d.hedr_shift_hz,
                    );
                    let row0 = pair * 2;
                    let row1 = row0 + 1;
                    for r in [row0, row1] {
                        let start = (r as usize) * line_pixels;
                        let end = start + line_pixels;
                        out.push(SstvEvent::LineDecoded {
                            mode: d.mode,
                            line_index: r,
                            pixels: d.image.pixels[start..end].to_vec(),
                        });
                    }
                }
            }
            crate::modespec::ChannelLayout::RobotYuv => {
                // Robot is per-line (no PD line-pairing). For R36/R24 the
                // chroma-duplication writes to the next image row; that's
                // handled inside mode_robot::decode_line. LineDecoded for image
                // row N is emitted after radio-line N's decode — for R36/R24
                // row 0 the Cb channel is at zero-init at this point (slowrx
                // C does the same; final ImageComplete carries the populated
                // state).
                let line_count = d.spec.image_lines.min(end_frame);
                for line in first_frame..line_count {
                    let line_seconds_offset = frame_offset(line);
                    crate::mode_robot::decode_line(
                        d.spec,
                        d.mode,
                        line,
                        &d.audio,
                        skip,
                        line_seconds_offset,
                        rate,
                        &mut d.image,
                        d.chroma_planes.as_mut(),
                        channel_demod,
                        snr_est,
                        d.hedr_shift_hz,
                    );
                    let start = (line as usize) * line_pixels;
                    let end = start + line_pixels;
                    out.push(SstvEvent::LineDecoded {
                        mode: d.mode,
                        line_index: line,
                        pixels: d.image.pixels[start..end].to_vec(),
                    });
                }
            }
            crate::modespec::ChannelLayout::RgbSequential => {
                // Scottie family. Mid-line sync handling lives inside
                // mode_scottie::decode_line. No chroma_planes — RGB is composed
                // in-place per line (no deferred chroma like R36/R24).
                let line_count = d.spec.image_lines.min(end_frame);
                for line in first_frame..line_count {
                    let line_seconds_offset = frame_offset(line);
                    crate::mode_scottie::decode_line(
                        d.spec,
                        line,
                        &d.audio,
                        skip,
                        line_seconds_offset,
                        rate,
                        &mut d.image,
                        channel_demod,
                        snr_est,
                        d.hedr_shift_hz,
                    );
                    let start = (line as usize) * line_pixels;
                    let end = start + line_pixels;
                    out.push(SstvEvent::LineDecoded {
                        mode: d.mode,
                        line_index: line,
                        pixels: d.image.pixels[start..end].to_vec(),
                    });
                }
            }
        }
    }

    /// 逐行相位跟踪：用缓冲音频里实测的行同步位置给出每帧起点。
    ///
    /// `base_skip` / `base_rate` 来自 [`find_sync`]，只用于给检测到的同步
    /// 脉冲分配行号；实测不足时 [`crate::sync::track_line_starts`] 内部会
    /// 退回 `base_skip + n × line_seconds × base_rate` 的等差模型。
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn compute_frame_starts(d: &DecodingState, base_skip: i64, base_rate: f64) -> Vec<f64> {
        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
        crate::sync::track_line_starts(
            &d.has_sync,
            &d.audio,
            work_rate,
            d.spec,
            base_skip,
            base_rate,
            radio_frames_per_image(d.spec),
            d.hedr_shift_hz,
        )
    }

    /// 渐进（实时）解码：音频已覆盖的无线电线数（含当前可能不完整的一线）。
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn covered_frames(d: &DecodingState, work_rate: f64) -> u32 {
        let per = d.spec.line_seconds * work_rate;
        if per <= 0.0 {
            return 0;
        }
        (d.audio.len() as f64 / per) as u32
    }

    /// 渐进（实时）解码的一步：必要时锁定/重估同步，然后把新到齐的行解出来。
    /// 返回整张图像是否已完成。
    ///
    /// 同步锁定策略：收够 [`PROGRESSIVE_MIN_FRAMES`] 线即可从下降沿得到 `skip`
    /// 并开始出图；`rate` 的斜率修正要等 [`PROGRESSIVE_RATE_LOCK_FRAMES`] 线
    /// （线数太少时 Hough 估计不可靠）。之后每新增
    /// [`PROGRESSIVE_REFINE_FRAMES`] 线用更多 sync 重估一次，只影响尚未解码的行。
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn progressive_step(
        d: &mut DecodingState,
        find_sync_scratch: &mut crate::sync::FindSyncScratch,
        channel_demod: &mut crate::demod::ChannelDemod,
        snr_est: &mut crate::snr::SnrEstimator,
        out: &mut Vec<SstvEvent>,
    ) -> bool {
        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
        let total_frames = radio_frames_per_image(d.spec);
        let per = d.spec.line_seconds * work_rate;
        let sync_frames = if per > 0.0 {
            ((d.has_sync.len() as f64) * (crate::sync::SYNC_PROBE_STRIDE as f64) / per) as u32
        } else {
            0
        };

        if sync_frames >= PROGRESSIVE_MIN_FRAMES
            && (d.sync_est.is_none()
                || sync_frames >= d.est_frames.saturating_add(PROGRESSIVE_REFINE_FRAMES))
        {
            let result = find_sync(&d.has_sync, work_rate, d.spec, find_sync_scratch);
            let rate = if sync_frames < PROGRESSIVE_RATE_LOCK_FRAMES {
                work_rate
            } else {
                result.adjusted_rate_hz
            };
            d.sync_est = Some((rate, result.skip_samples));
            d.est_frames = sync_frames;
        }

        let Some((rate, skip)) = d.sync_est else {
            return false;
        };

        let covered = Self::covered_frames(d, work_rate);
        let decodable = covered
            .saturating_sub(PROGRESSIVE_LOOKAHEAD_FRAMES)
            .min(total_frames);
        if decodable > d.next_frame {
            // 逐行相位跟踪：每次重估同步后按当前可用的 sync 轨道重算行起点。
            let frame_starts = Self::compute_frame_starts(d, skip, rate);
            Self::decode_frame_range(
                d,
                skip,
                rate,
                &frame_starts,
                d.next_frame,
                decodable,
                out,
                channel_demod,
                snr_est,
            );
            d.next_frame = decodable;
        }

        d.next_frame >= total_frames
    }

    /// 收尾：若正在解码一张图，用**已收集到的完整 sync 轨道**重解已到齐的行，
    /// 发出 `LineDecoded` 与 `ImageComplete { partial: true }`（未收到的行保持
    /// 黑色）。用于实时接收中途停止时，把预览替换为一次精修结果。
    ///
    /// 未在解码中、或 sync 尚不足以定位（`find_sync` 未找到任何 sync 脉冲）时
    /// 返回空，避免用瞎猜的 `skip` 解出垃圾图。
    ///
    /// 无论是否产出事件，调用后都会丢弃进行中的解码状态（复位为
    /// `AwaitingVis`）；它是一次性的收尾操作。
    #[must_use]
    pub fn finalize(&mut self) -> Vec<SstvEvent> {
        let State::Decoding(d_box) = std::mem::replace(&mut self.state, State::AwaitingVis) else {
            return Vec::new();
        };
        let mut d = *d_box;
        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
        let result = find_sync(&d.has_sync, work_rate, d.spec, &mut self.find_sync_scratch);
        if result.slant_deg.is_none() {
            return Vec::new();
        }
        let covered = Self::covered_frames(&d, work_rate).min(radio_frames_per_image(d.spec));
        if covered == 0 {
            return Vec::new();
        }
        // 干净的图 + 干净的色度平面，用整段 sync 重解已到齐的行。
        d.image = SstvImage::new(d.spec.mode, d.spec.line_pixels, d.spec.image_lines);
        d.chroma_planes = fresh_chroma_planes(d.spec);
        let mut out = Vec::new();
        let frame_starts =
            Self::compute_frame_starts(&d, result.skip_samples, result.adjusted_rate_hz);
        Self::decode_frame_range(
            &mut d,
            result.skip_samples,
            result.adjusted_rate_hz,
            &frame_starts,
            0,
            covered,
            &mut out,
            &mut self.channel_demod,
            &mut self.snr_est,
        );
        out.push(SstvEvent::ImageComplete {
            image: d.image,
            partial: true,
        });
        out
    }

    /// Reset to `AwaitingVis`; discard any in-flight image. The forced-decoding
    /// mode + window (if set via [`Self::with_mode`] /
    /// [`Self::set_forced_mode`]) is preserved.
    pub fn reset(&mut self) {
        self.state = State::AwaitingVis;
        self.samples_processed = 0;
        self.working_samples_emitted = 0;
        self.vis = crate::vis::VisDetector::new(IS_KNOWN_VIS);
        self.resampler.reset_state();
        self.channel_demod = crate::demod::ChannelDemod::new();
        self.snr_est = crate::snr::SnrEstimator::new();
        self.refresh_manual_window();
    }

    /// Total samples processed since construction (or last `reset`).
    #[must_use]
    pub fn samples_processed(&self) -> u64 {
        self.samples_processed
    }
}

impl std::fmt::Debug for SstvDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SstvDecoder")
            .field("rate", &self.resampler.input_rate())
            .field("state", &self.state)
            .field("samples_processed", &self.samples_processed)
            .field("working_samples_emitted", &self.working_samples_emitted)
            .field("forced_mode", &self.forced_mode)
            .field("forced_window", &self.forced_window)
            .finish_non_exhaustive()
    }
}

/// Estimate the dominant tone frequency in `window` (working-rate samples).
/// Returns the estimated frequency in Hz, biased toward 1500-2300 Hz
/// (the SSTV video band).
///
/// Algorithm: Goertzel-bank evaluated at 25-Hz steps from 1450 to 2350 Hz,
/// then quadratic peak interpolation around the maximum bin.
#[must_use]
#[allow(clippy::cast_precision_loss, dead_code)]
pub(crate) fn estimate_freq(window: &[f32]) -> f64 {
    const STEP_HZ: f64 = 25.0;
    const FIRST_HZ: f64 = 1450.0;
    const N_BINS: usize = 37; // 1450..2350 inclusive at 25 Hz steps

    let mut powers = [0.0_f64; N_BINS];
    for (i, p) in powers.iter_mut().enumerate() {
        let f = FIRST_HZ + (i as f64) * STEP_HZ;
        *p = crate::dsp::goertzel_power(window, f);
    }
    let (mut max_i, mut max_p) = (0_usize, powers[0]);
    for (i, &p) in powers.iter().enumerate().skip(1) {
        if p > max_p {
            max_p = p;
            max_i = i;
        }
    }
    let center_hz = FIRST_HZ + (max_i as f64) * STEP_HZ;
    // Quadratic interpolation if we have both neighbours.
    if max_i > 0 && max_i < N_BINS - 1 && max_p > 0.0 {
        let a = powers[max_i - 1];
        let b = max_p;
        let c = powers[max_i + 1];
        let denom = a - 2.0 * b + c;
        if denom.abs() > 1e-12 {
            let delta = 0.5 * (a - c) / denom;
            return center_hz + delta * STEP_HZ;
        }
    }
    center_hz
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::resample::{MAX_INPUT_SAMPLE_RATE_HZ, WORKING_SAMPLE_RATE_HZ};

    #[test]
    fn rejects_invalid_sample_rates() {
        assert!(matches!(
            SstvDecoder::new(0),
            Err(Error::InvalidSampleRate { got: 0 })
        ));
        assert!(matches!(
            SstvDecoder::new(MAX_INPUT_SAMPLE_RATE_HZ + 1),
            Err(Error::InvalidSampleRate { .. })
        ));
    }

    #[test]
    fn accepts_common_rates() {
        assert!(SstvDecoder::new(11_025).is_ok());
        assert!(SstvDecoder::new(44_100).is_ok());
        assert!(SstvDecoder::new(48_000).is_ok());
    }

    #[test]
    fn process_advances_sample_counter() {
        let mut d = SstvDecoder::new(11_025).expect("decoder");
        assert_eq!(d.samples_processed(), 0);
        let _ = d.process(&[0.0_f32; 1024]);
        assert_eq!(d.samples_processed(), 1024);
        let _ = d.process(&[0.0_f32; 256]);
        assert_eq!(d.samples_processed(), 1280);
    }

    #[test]
    fn process_returns_no_events_for_silence() {
        let mut d = SstvDecoder::new(11_025).expect("decoder");
        // Silence produces no VIS match.
        let events = d.process(&[0.5_f32; 512]);
        assert!(events.is_empty());
    }

    #[test]
    fn process_emits_vis_detected_for_pd120_burst() {
        use crate::vis::tests::synth_vis;
        let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
        // Pad with trailing silence so the polyphase FIR's ~64-sample group
        // delay still yields a full set of stop-bit windows (PR-2 T2.1).
        let mut burst = synth_vis(0x5F, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        let hedr = events
            .iter()
            .find_map(|e| match e {
                SstvEvent::VisDetected {
                    mode: SstvMode::Pd120,
                    hedr_shift_hz,
                    ..
                } => Some(*hedr_shift_hz),
                _ => None,
            })
            .expect("expected VisDetected for PD120");
        assert!(
            hedr.abs() < 10.0,
            "synthetic burst should report ~0 Hz shift, got {hedr}"
        );
    }

    #[test]
    fn process_emits_vis_detected_for_pd180_burst() {
        use crate::vis::tests::synth_vis;
        let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
        let mut burst = synth_vis(0x60, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        let hedr = events
            .iter()
            .find_map(|e| match e {
                SstvEvent::VisDetected {
                    mode: SstvMode::Pd180,
                    hedr_shift_hz,
                    ..
                } => Some(*hedr_shift_hz),
                _ => None,
            })
            .expect("expected VisDetected for PD180");
        assert!(hedr.abs() < 10.0);
    }

    #[test]
    fn process_emits_vis_detected_for_pd240_burst() {
        use crate::vis::tests::synth_vis;
        let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
        let mut burst = synth_vis(0x61, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        let hedr = events
            .iter()
            .find_map(|e| match e {
                SstvEvent::VisDetected {
                    mode: SstvMode::Pd240,
                    hedr_shift_hz,
                    ..
                } => Some(*hedr_shift_hz),
                _ => None,
            })
            .expect("expected VisDetected for PD240");
        assert!(hedr.abs() < 10.0);
    }

    #[test]
    fn process_emits_vis_detected_for_robot24_burst() {
        use crate::vis::tests::synth_vis;
        let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
        let mut burst = synth_vis(0x04, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        let hedr = events
            .iter()
            .find_map(|e| match e {
                SstvEvent::VisDetected {
                    mode: SstvMode::Robot24,
                    hedr_shift_hz,
                    ..
                } => Some(*hedr_shift_hz),
                _ => None,
            })
            .expect("expected VisDetected for Robot24");
        assert!(hedr.abs() < 10.0);
    }

    #[test]
    fn process_emits_vis_detected_for_robot36_burst() {
        use crate::vis::tests::synth_vis;
        let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
        let mut burst = synth_vis(0x08, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        let hedr = events
            .iter()
            .find_map(|e| match e {
                SstvEvent::VisDetected {
                    mode: SstvMode::Robot36,
                    hedr_shift_hz,
                    ..
                } => Some(*hedr_shift_hz),
                _ => None,
            })
            .expect("expected VisDetected for Robot36");
        assert!(hedr.abs() < 10.0);
    }

    #[test]
    fn process_emits_vis_detected_for_robot72_burst() {
        use crate::vis::tests::synth_vis;
        let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
        let mut burst = synth_vis(0x0C, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        let hedr = events
            .iter()
            .find_map(|e| match e {
                SstvEvent::VisDetected {
                    mode: SstvMode::Robot72,
                    hedr_shift_hz,
                    ..
                } => Some(*hedr_shift_hz),
                _ => None,
            })
            .expect("expected VisDetected for Robot72");
        assert!(hedr.abs() < 10.0);
    }

    #[test]
    fn reset_clears_sample_counter() {
        let mut d = SstvDecoder::new(11_025).expect("decoder");
        let _ = d.process(&[0.0_f32; 1024]);
        d.reset();
        assert_eq!(d.samples_processed(), 0);
    }

    // 40 ms tones make every 25-Hz bank bin map to a unique Goertzel k
    // (11025/441 = 25.0). Production windows are ~5 ms; ~50 Hz suffices.
    fn synth_tone_at_working(freq_hz: f64, secs: f64) -> Vec<f32> {
        let sr = f64::from(WORKING_SAMPLE_RATE_HZ);
        let n = (secs * sr).round() as usize;
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq_hz * (i as f64) / sr).sin() as f32)
            .collect()
    }

    #[test]
    fn estimate_freq_recovers_known_tone() {
        for &f in &[1500.0_f64, 1700.0, 1900.0, 2100.0, 2300.0] {
            let window = synth_tone_at_working(f, 0.040);
            let est = estimate_freq(&window);
            assert!((est - f).abs() < 30.0, "freq={f} estimate={est}");
        }
    }

    #[test]
    fn estimate_freq_no_interp_at_left_boundary() {
        // Tone at 1450 Hz lands on bin 0; no left neighbour → no interp.
        let window = synth_tone_at_working(1450.0, 0.040);
        let est = estimate_freq(&window);
        assert!((est - 1450.0).abs() < 30.0, "expected ≈1450, got {est}");
    }

    #[test]
    fn reset_during_decoding_emits_partial_via_subsequent_process() {
        let mut d = SstvDecoder::new(crate::resample::WORKING_SAMPLE_RATE_HZ).unwrap();
        // Push a VIS so the decoder transitions to Decoding. Trailing zeros
        // accommodate the FIR group delay so the burst actually triggers
        // detection (without the padding the test would mask Finding 1
        // by never entering Decoding).
        let mut burst = crate::vis::tests::synth_vis(0x5F, 0.0);
        burst.extend(std::iter::repeat_n(0.0_f32, 512));
        let events = d.process(&burst);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SstvEvent::VisDetected { .. })),
            "expected VIS detection before reset, got {events:?}"
        );
        // We're now in Decoding state.
        d.reset();
        // After reset, the decoder is back in AwaitingVis with FIR resampler
        // and ChannelDemod state cleared. The next process call with quiet audio
        // yields no events.
        let events = d.process(&[0.0_f32; 100]);
        assert!(
            events.is_empty(),
            "reset should clear in-flight; got {events:?}"
        );
    }

    // TODO(future/PR-3): mid_image_vis_emits_partial_then_new_vis
    //
    // When a new VIS burst arrives during Decoding the spec calls for
    // emitting `ImageComplete { partial: true }` for the in-flight image,
    // then transitioning to AwaitingVis.
    //
    // The naive approach (running `self.vis` against the decoding buffer
    // each call) fails because the residual buffer from a previous VIS
    // detection is not aligned to 30 ms window boundaries: the first
    // classifier window is a mix of silence and leader tone and does not
    // reliably pass the 5× dominance threshold. A correct implementation
    // would re-align the scan to the next 30 ms boundary or run a separate
    // 1900 Hz energy detector. Deferred to PR-3 (cross-validation).
}
