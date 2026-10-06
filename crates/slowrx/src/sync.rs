//! Slant correction + line-zero phase alignment.
//!
//! Translated from slowrx's `sync.c` (Oona Räisänen, ISC License).
//! See `NOTICE.md`. Two responsibilities:
//!
//! 1. [`SyncTracker`] — per-sample boolean "is the 1200 Hz sync pulse
//!    dominant here?" Equivalent of slowrx's `Praw`/`Psync` ratio in
//!    `video.c` lines 271-297.
//! 2. [`find_sync`] — Hough-transform a captured `has_sync` track to
//!    detect slant, adjust the rate to cancel it, then locate line 0's
//!    `Skip` via 8-tap convolution on the column-summed sync image.
//!    Equivalent of slowrx's `sync.c::FindSync` (lines 18-133).
//!
//! Slowrx is offline-batch (read-all → first `GetVideo` populates
//! `HasSync[]` → `FindSync` adjusts → second `GetVideo` rereads cached
//! `StoredLum` at corrected pixel times). Our decoder accumulates one
//! image's worth of audio in the `Decoding` state, probes [`SyncTracker`]
//! at every [`SYNC_PROBE_STRIDE`] samples, then runs [`find_sync`] once.
//! The corrected `(rate, skip)` drives a single per-pixel decode pass.
//! `LineDecoded` events fire in fast succession at end-of-buffer rather
//! than incrementally; callers still see every event.
//!
//! Inline `// slowrx <file>.c:NNN` line refs are against the gitignored
//! local reference clone in `original/slowrx/` (see `clone-slowrx.sh`);
//! verified at audit #94 (2026-05-15).

use rustfft::{num_complex::Complex, FftPlanner};
use std::sync::Arc;

use crate::modespec::ModeSpec;
use crate::resample::WORKING_SAMPLE_RATE_HZ;

/// Stride between sync-band probes (working-rate samples).
///
/// slowrx uses 13 samples@44.1 kHz (`video.c:295`) ≈ 3.25 samples@11.025 kHz.
/// The fractional equivalence means no integer stride gives exact slowrx parity;
/// we choose 4 (round-up / ceil) rather than 3 (round-down / floor).
///
/// **Probe-count comparison:**
/// - slowrx probes/image ≈ `image_samples / 13` at 44.1 kHz.
/// - Rust probes/image ≈ `image_samples_11025 / 4` at 11.025 kHz.
/// - `image_samples_11025 / 4 ≈ (image_samples_44100 / 4) / 4 ≈ image_samples_44100 / 16`,
///   which is slightly fewer probes than slowrx's `/ 13`.
///
/// With `SYNC_PROBE_STRIDE = 4` Rust's per-image probe count is ≈ 19% fewer
/// than slowrx's. With stride=3 it was ≈ 25% more. Stride=4 is closer in
/// ratio (1.56 vs slowrx) and preserves the ~0.36 ms/probe cadence.  The
/// Hough transform's line-finding is robust to moderate density differences
/// (round-2 audit Finding 8).
pub(crate) const SYNC_PROBE_STRIDE: usize = 4;

/// Hann-windowed audio length per sync probe (samples). 1/4 of slowrx's
/// 64@44.1kHz keeps the time span (~1.5 ms) constant (`video.c:278`).
pub(crate) const SYNC_FFT_WINDOW_SAMPLES: usize = 16;

/// Zero-padded FFT length per sync probe. 256@11025 = 43 Hz/bin matches
/// slowrx's 1024@44100 (`video.c:280`).
pub(crate) const SYNC_FFT_LEN: usize = 256;

// Hough-transform slant search (slowrx `common.h:4-5` MINSLANT/MAXSLANT
// + sync.c step `q++` in 0.5° units via `q/2.0`); slant lock window
// matches sync.c:83 `slantAngle > 89 && slantAngle < 91`.
const MIN_SLANT_DEG: f64 = 30.0;
const MAX_SLANT_DEG: f64 = 150.0;
const SLANT_STEP_DEG: f64 = 0.5;
const SLANT_OK_LO_DEG: f64 = 89.0;
const SLANT_OK_HI_DEG: f64 = 91.0;
const MAX_SLANT_RETRIES: usize = 3;
// `xAcc[700]` (sync.c:23), `SyncImg[700][630]` (sync.c:26),
// `lines[600][...]` (sync.c:24).
const X_ACC_BINS: usize = 700;
const SYNC_IMG_Y_BINS: usize = 630;
const LINES_D_BINS: usize = 600;

/// Right-edge slip threshold for the falling-edge `xmax`: if `xmax`
/// exceeds half the column-accumulator span, the detected pulse
/// belongs to the next line's leading sync — wrap left by this
/// amount. Matches slowrx `sync.c:117` (`if (xmax > 350) xmax -= 350;`).
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
const X_ACC_SLIP_THRESHOLD: i32 = (X_ACC_BINS / 2) as i32; // 350

/// 8-tap falling-edge detection kernel: leading 4 ones, trailing 4
/// negative ones. Convolved with the column-accumulator `x_acc`;
/// the position of the maximum response is the falling edge of the
/// dominant sync pulse. Matches slowrx `sync.c:108` (the inline
/// literal `{1,1,1,1,-1,-1,-1,-1}`).
const SYNC_EDGE_KERNEL: [i32; 8] = [1, 1, 1, 1, -1, -1, -1, -1];
const SYNC_EDGE_KERNEL_LEN: usize = SYNC_EDGE_KERNEL.len();

/// Scratch buffers for [`find_sync`] and its helpers. Hoisted onto
/// [`crate::decoder::SstvDecoder`] so they're reused across decode
/// passes — the largest two (`sync_img`, `x_acc`) are sized at
/// construction and never resized; `lines` resizes per-call because
/// `n_slant_bins × LINES_D_BINS` depends on the mode's `line_width`.
/// (Audit #93 D6.)
pub(crate) struct FindSyncScratch {
    /// `[X_ACC_BINS × SYNC_IMG_Y_BINS]` flat buffer for the 2D sync image.
    /// Sized once; `find_sync` calls `.fill(false)` each invocation.
    pub(crate) sync_img: Vec<bool>,
    /// `[LINES_D_BINS × n_slant_bins]` flat buffer for the Hough
    /// accumulator. Resized per-call (`.clear() + .resize(...)`).
    pub(crate) lines: Vec<u16>,
    /// `[X_ACC_BINS]` column accumulator. Sized once; `.fill(0)` per call.
    pub(crate) x_acc: Vec<u32>,
}

impl FindSyncScratch {
    pub(crate) fn new() -> Self {
        Self {
            sync_img: vec![false; X_ACC_BINS * SYNC_IMG_Y_BINS],
            lines: Vec::new(),
            x_acc: vec![0; X_ACC_BINS],
        }
    }
}

impl Default for FindSyncScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert degrees to radians. Matches slowrx `common.c::deg2rad`.
fn deg2rad(deg: f64) -> f64 {
    deg * std::f64::consts::PI / 180.0
}

/// Per-sample sync-band probe context (FFT plan + buffers reused across
/// probes). `sync_target_bin` / `video_{lo,hi}_bin` are pre-computed bin
/// offsets corresponding to `1200 Hz` and `1500..=2300 Hz` shifted by
/// `hedr_shift_hz`.
pub(crate) struct SyncTracker {
    fft: Arc<dyn rustfft::Fft<f32>>,
    hann: Vec<f32>,
    fft_buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    sync_target_bin: usize,
    video_lo_bin: usize,
    video_hi_bin: usize,
}

impl SyncTracker {
    /// Construct a tracker with the radio mistuning offset extracted at
    /// VIS time.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn new(hedr_shift_hz: f64) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(SYNC_FFT_LEN);
        let scratch_len = fft.get_inplace_scratch_len();

        // Use slowrx-equivalent truncation via `crate::dsp::get_bin` (not `.round()`).
        // See `crate::dsp::get_bin` for rationale.
        // sync_target_bin for 1200 Hz is 27 (slowrx-correct) not 28 (what
        // `.round()` would give) — at zero `hedr_shift_hz`. The actual
        // computed bin is `get_bin(1200.0 + hedr_shift_hz, ...)`, so a
        // non-zero radio mistuning shifts the target bin accordingly.
        // (Audit #94 E13.)
        let bin_for =
            |hz: f64| -> usize { crate::dsp::get_bin(hz, SYNC_FFT_LEN, WORKING_SAMPLE_RATE_HZ) };

        Self {
            fft,
            hann: build_sync_hann(),
            fft_buf: vec![Complex { re: 0.0, im: 0.0 }; SYNC_FFT_LEN],
            // rustfft returns scratch_len = 0 for power-of-two sizes
            // (SYNC_FFT_LEN=256 is radix-2). The prior .max(SYNC_FFT_LEN) was
            // dead-allocating ~2 KiB. (Audit #92 C8.)
            scratch: vec![Complex { re: 0.0, im: 0.0 }; scratch_len],
            sync_target_bin: bin_for(1200.0 + hedr_shift_hz),
            video_lo_bin: bin_for(1500.0 + hedr_shift_hz),
            video_hi_bin: bin_for(2300.0 + hedr_shift_hz),
        }
    }

    /// Probe a single window centered at `center_sample` of `audio`.
    /// Returns `true` when the 1200 Hz sync band has more power per Hz
    /// than the 1500-2300 Hz video band by at least 2×.
    ///
    /// Translated from slowrx `video.c` lines 271-297.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    pub fn has_sync_at(&mut self, audio: &[f32], center_sample: usize) -> bool {
        let half = (SYNC_FFT_WINDOW_SAMPLES as i64) / 2;
        self.fft_buf.fill(Complex { re: 0.0, im: 0.0 });
        for i in 0..SYNC_FFT_WINDOW_SAMPLES {
            let idx = (center_sample as i64) - half + (i as i64);
            let s = if idx >= 0 && (idx as usize) < audio.len() {
                audio[idx as usize]
            } else {
                0.0
            };
            self.fft_buf[i].re = s * self.hann[i];
        }
        self.fft
            .process_with_scratch(&mut self.fft_buf, &mut self.scratch[..]);

        // Praw = average power per bin across video band (video.c:282-288).
        // slowrx-faithful off-by-one: slowrx C uses `hi - lo` as the
        // divisor for an inclusive `[lo, hi]` range, undercounting by 1.
        // We match for bit-parity of the `p_sync > 2 × p_raw` decision.
        // See `docs/intentional-deviations.md::"Faithful-to-slowrx
        // artifacts"`. (Audit #94 E13.)
        let mut p_raw = 0.0_f64;
        let lo = self.video_lo_bin.max(1);
        let hi = self.video_hi_bin.min(SYNC_FFT_LEN / 2 - 1);
        if hi >= lo {
            for k in lo..=hi {
                p_raw += crate::dsp::power(self.fft_buf[k]);
            }
            p_raw /= (hi - lo).max(1) as f64;
        }

        // Psync = triangle-weighted sum across [bin-1, bin, bin+1] / 2
        // (video.c:285-289).
        let mut p_sync = 0.0_f64;
        let bin = self.sync_target_bin.clamp(1, SYNC_FFT_LEN / 2 - 1);
        for offset in -1_i32..=1 {
            let k = (bin as i32 + offset) as usize;
            let weight = 1.0 - 0.5 * f64::from(offset.abs());
            p_sync += crate::dsp::power(self.fft_buf[k]) * weight;
        }
        p_sync /= 2.0;

        // slowrx video.c:293: HasSync = (Psync > 2*Praw)
        p_sync > 2.0 * p_raw
    }
}

/// Build the Hann window used per sync probe.
fn build_sync_hann() -> Vec<f32> {
    crate::dsp::build_hann(SYNC_FFT_WINDOW_SAMPLES)
}

/// Result of [`find_sync`]: slant-corrected rate + line-zero `Skip`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SyncResult {
    /// Adjusted working-rate sample rate (Hz).
    pub adjusted_rate_hz: f64,
    /// Sample offset from the start of the sync track where line 0's
    /// video data begins. May be slightly negative; the decoder zero-pads
    /// out-of-range reads when computing per-channel slices.
    pub skip_samples: i64,
    /// Detected slant angle (degrees), or `None` when the Hough transform
    /// found no sync pulses at all (degenerate/empty input).
    ///
    /// Read by tests and by the forced-mode path in [`crate::decoder`], which
    /// uses `None` to suppress a black "image" from a sync-less window. Using
    /// `Option<f64>` avoids the round-2 audit Finding 10 ambiguity where
    /// `90.0` would be returned for both "perfectly aligned input" and
    /// "nothing-detected-at-all input".
    pub slant_deg: Option<f64>,
}

/// Linear Hough transform + 8-tap convolution edge-find.
///
/// `has_sync` is the per-stride boolean track produced by [`SyncTracker`].
/// `initial_rate_hz` is normally [`WORKING_SAMPLE_RATE_HZ`] but the
/// function may adjust it. Translated from slowrx `sync.c::FindSync`
/// (lines 18-133).
#[must_use = "the SyncResult must be consumed; dropping it discards the slant + skip correction"]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub(crate) fn find_sync(
    has_sync: &[bool],
    initial_rate_hz: f64,
    spec: ModeSpec,
    scratch: &mut FindSyncScratch,
) -> SyncResult {
    let line_width: usize = ((spec.line_seconds / spec.sync_seconds) * 4.0) as usize;
    let num_lines = spec.image_lines as usize;
    let mut rate = initial_rate_hz;
    let mut slant_deg_detected: Option<f64> = None;

    for retry in 0..=MAX_SLANT_RETRIES {
        let Some((slant, adjusted)) = hough_detect_slant(has_sync, rate, spec, line_width, scratch)
        else {
            // No sync pulses → no Hough peak → no rate correction.
            break;
        };
        slant_deg_detected = Some(slant);

        // Apply a deadband at 90° so an exact-rate input is not
        // perturbed by half-degree Hough quantization noise (see
        // docs/intentional-deviations.md "FindSync 90° slant deadband").
        if (slant - 90.0).abs() > SLANT_STEP_DEG {
            rate = adjusted;
        }

        // sync.c:86-90 resets to 44100 on retry exhaustion; we keep
        // our last estimate (see docs/intentional-deviations.md
        // "FindSync retry-exhaustion"). Open interval (89, 91) matches
        // slowrx sync.c:83 exactly — half-open `89.0..91.0` would
        // widen the lock by one 0.5°-Hough bin (round-2 audit
        // Finding 7).
        if (slant > SLANT_OK_LO_DEG && slant < SLANT_OK_HI_DEG) || retry == MAX_SLANT_RETRIES {
            break;
        }
    }

    let xmax = find_falling_edge(has_sync, rate, spec, num_lines, scratch);
    let s_secs = skip_seconds_for(xmax, spec);
    let skip_samples = (s_secs * rate).round() as i64;

    SyncResult {
        adjusted_rate_hz: rate,
        skip_samples,
        slant_deg: slant_deg_detected,
    }
}

/// Build the 2D sync image at `rate_hz`, then linear-Hough-transform
/// it to find the dominant slant angle. Returns `None` when no sync
/// pulses register at all (degenerate input). The returned
/// `adjusted_rate` already has the standard Hough-derived correction
/// applied (`rate × tan(90° − slant) / line_width × rate`); the
/// caller applies the 90° deadband before adopting it.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn hough_detect_slant(
    has_sync: &[bool],
    rate_hz: f64,
    spec: ModeSpec,
    line_width: usize,
    scratch: &mut FindSyncScratch,
) -> Option<(f64 /* slant_deg */, f64 /* adjusted_rate */)> {
    let n_slant_bins = ((MAX_SLANT_DEG - MIN_SLANT_DEG) / SLANT_STEP_DEG).round() as usize;
    let num_lines = spec.image_lines as usize;

    // Column-major: x is the outer dim because the Hough vote loop
    // iterates `for cy { for cx { … } }` and we want sequential x to
    // share a cache line. Matches slowrx C's `SyncImg[700][630]`
    // shape (C10 audit).
    let sync_img_idx = |x: usize, y: usize| x * SYNC_IMG_Y_BINS + y;

    // Row-major: d is the outer dim (the slowrx C `Lines[600][240]`
    // shape). Vote increments scan q-inner.
    let lines_idx = |d: usize, q: usize| d * n_slant_bins + q;

    let probe_index = |t: f64| -> usize {
        let raw = t * rate_hz / (SYNC_PROBE_STRIDE as f64);
        if raw < 0.0 {
            0
        } else {
            raw as usize
        }
    };

    // Reset the hoisted scratch buffers (was: fresh Vec allocations).
    scratch.sync_img.fill(false);
    scratch.lines.clear();
    scratch.lines.resize(LINES_D_BINS * n_slant_bins, 0);

    // Draw the 2D sync signal at current rate.
    for y in 0..num_lines.min(SYNC_IMG_Y_BINS) {
        for x in 0..line_width.min(X_ACC_BINS) {
            let t = ((y as f64) + (x as f64) / (line_width as f64)) * spec.line_seconds;
            let idx = probe_index(t);
            if idx < has_sync.len() {
                scratch.sync_img[sync_img_idx(x, y)] = has_sync[idx];
            }
        }
    }

    // Linear Hough transform.
    let mut q_most = 0_usize;
    let mut max_count = 0_u16;
    for cy in 0..num_lines.min(SYNC_IMG_Y_BINS) {
        for cx in 0..line_width.min(X_ACC_BINS) {
            if !scratch.sync_img[sync_img_idx(cx, cy)] {
                continue;
            }
            for q in 0..n_slant_bins {
                let theta = deg2rad(MIN_SLANT_DEG + (q as f64) * SLANT_STEP_DEG);
                let d_signed = (line_width as f64)
                    + (-(cx as f64) * theta.sin() + (cy as f64) * theta.cos()).round();
                if d_signed > 0.0 && d_signed < (line_width as f64) {
                    let d = d_signed as usize;
                    if d < LINES_D_BINS {
                        let cell = &mut scratch.lines[lines_idx(d, q)];
                        *cell = cell.saturating_add(1);
                        if *cell > max_count {
                            max_count = *cell;
                            q_most = q;
                        }
                    }
                }
            }
        }
    }

    if max_count == 0 {
        return None;
    }

    let slant_angle = MIN_SLANT_DEG + (q_most as f64) * SLANT_STEP_DEG;
    let adjusted_rate =
        rate_hz + (deg2rad(90.0 - slant_angle).tan() / (line_width as f64)) * rate_hz;
    Some((slant_angle, adjusted_rate))
}

/// Pure 8-tap falling-edge convolution + slip-wrap. Returns `xmax`
/// already adjusted for the `X_ACC_SLIP_THRESHOLD` right-edge slip.
///
/// **A6 fix (#88):** The loop iterates exactly
/// `X_ACC_BINS - SYNC_EDGE_KERNEL_LEN` times — matching slowrx C's
/// `for (n=0; n<X_ACC_BINS-8; n++)` (692 iterations). Rust's native
/// `Iterator::windows(8)` yields 693 windows over a 700-element
/// slice (indices 0..=692); the `.take(X_ACC_BINS - SYNC_EDGE_KERNEL_LEN)`
/// caps it at 692 (indices 0..=691), so `xAcc[699]` is never read.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn falling_edge_from_x_acc(x_acc: &[u32]) -> i32 {
    debug_assert_eq!(x_acc.len(), X_ACC_BINS, "x_acc must be X_ACC_BINS long");
    let mut xmax: i32 = 0;
    let mut max_convd: i32 = 0;
    for (x, window) in x_acc
        .windows(SYNC_EDGE_KERNEL_LEN)
        .take(X_ACC_BINS - SYNC_EDGE_KERNEL_LEN)
        .enumerate()
    {
        let convd: i32 = window
            .iter()
            .zip(SYNC_EDGE_KERNEL.iter())
            .map(|(&v, &k)| (v as i32) * k)
            .sum();
        if convd > max_convd {
            max_convd = convd;
            xmax = (x as i32) + (SYNC_EDGE_KERNEL_LEN as i32) / 2;
        }
    }

    // sync.c:117 — pulse near the right edge slipped from previous left.
    if xmax > X_ACC_SLIP_THRESHOLD {
        xmax -= X_ACC_SLIP_THRESHOLD;
    }

    xmax
}

/// Column-accumulate `has_sync` into `X_ACC_BINS` bins at `rate_hz`,
/// then convolve with `SYNC_EDGE_KERNEL` to find the steepest
/// falling edge. Returns the `xmax` integer with the
/// `X_ACC_SLIP_THRESHOLD` right-edge slip-wrap already applied.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn find_falling_edge(
    has_sync: &[bool],
    rate_hz: f64,
    spec: ModeSpec,
    num_lines: usize,
    scratch: &mut FindSyncScratch,
) -> i32 {
    let probe_index = |t: f64| -> usize {
        let raw = t * rate_hz / (SYNC_PROBE_STRIDE as f64);
        if raw < 0.0 {
            0
        } else {
            raw as usize
        }
    };

    scratch.x_acc.fill(0);
    for y in 0..num_lines {
        for (x, slot) in scratch.x_acc.iter_mut().enumerate() {
            let t = (y as f64) * spec.line_seconds
                + ((x as f64) / (X_ACC_BINS as f64)) * spec.line_seconds;
            let idx = probe_index(t);
            if idx < has_sync.len() && has_sync[idx] {
                *slot = slot.saturating_add(1);
            }
        }
    }

    falling_edge_from_x_acc(&scratch.x_acc)
}

/// Convert a falling-edge `xmax` (post-slip-wrap) to skip seconds,
/// applying the mode's sync-position offset. Pure arithmetic — no
/// global state. The raw `s_secs` is computed assuming the falling
/// edge lands at `(xmax / X_ACC_BINS) × line_seconds` and the sync
/// pulse runs `sync_seconds` long; `ModeSpec::skip_correction_seconds()`
/// then hoists the result for mid-line-sync modes (Scottie).
#[allow(clippy::cast_precision_loss)]
fn skip_seconds_for(xmax: i32, spec: ModeSpec) -> f64 {
    let raw = (f64::from(xmax) / (X_ACC_BINS as f64)) * spec.line_seconds - spec.sync_seconds;
    raw + spec.skip_correction_seconds()
}

// ---------------------------------------------------------------------------
// 逐行相位跟踪（per-line phase tracking）
// ---------------------------------------------------------------------------
//
// 背景：`find_sync` 只给整幅图一个 `(rate, skip)`，行 n 的起点按
// `skip + n × line_seconds × rate` 等差外推。真实发射端的行周期与标称值
// 常有几十 ppm 的偏差（例如 MMSSTV 的 PD-240 在 44.1 kHz 下每行实际
// 44102 个样本而非 44100），250 行累积可达十几毫秒，而 PD 通道首尾的
// 安全余量只有半个像素（0.19 ms）——于是每通道的开头/结尾像素读到行同步
// 脉冲或相邻通道，表现为图像边缘的彩色竖条纹。
//
// 这里改为**逐行**跟踪：直接测出每个行同步脉冲在音频中的位置，用它作为
// 该行的起点，行内像素仍按 `rate` 排布。同步脉冲位置取上升/下降沿的中点，
// 两个边沿各自做双窗长外推以消掉过零检测的窗偏移（恒定偏移会在整幅图
// 一侧留下固定宽度的条纹），再做局部加权线性平滑：去掉测量抖动，保留
// 时钟漂移的慢变趋势（这正是"跟踪"）。

/// 边沿精化用的分析窗长（工作率样本，≈4 ms）。
const EDGE_WIN_LEN: usize = 44;

/// 边沿精化的短窗长（工作率样本，≈2 ms）：与 [`EDGE_WIN_LEN`] 一起做
/// 双窗长外推。过零检测的系统偏移近似正比于窗长，两次测量即可外推到
/// "零窗长"的无偏值（见 [`extrapolate_edge`]）。
const EDGE_WIN_LEN_SHORT: usize = 22;

/// 边沿精化的搜索余量（工作率样本）：在"期望过零位置 ± 窗半宽"之外再留
/// 的余量，覆盖 [`SyncTracker`] 窗（16 样本）带来的粗边界不确定度。
const EDGE_SEARCH_SAMPLES: f64 = 16.0;

/// 接受一个同步区间前，其时长必须落在 `sync_seconds` 的
/// `[SPAN_LEN_LO, SPAN_LEN_HI]` 倍内 —— 用来挡掉 VIS 头里 30 ms 的
/// 1200 Hz 位与其它非行同步的 1200 Hz 段。
const SPAN_LEN_LO: f64 = 0.5;
const SPAN_LEN_HI: f64 = 1.8;

/// 相邻候选续链的容差（单位：行）。小于 0.5 保证行号唯一。
const MATCH_TOL_FRAMES: f64 = 0.45;

/// 局部加权线性平滑的半径（单位：行）。窗口内近似线性的漂移被保留，
/// 逐行随机抖动被压低约 `sqrt(2·HALO+1)` 倍。抖动主要来自双窗长外推的
/// 误差放大（`2·t_short − t_long`），需要足够宽的平滑窗压掉。
const SMOOTH_HALO: usize = 8;

/// 行内同步脉冲**中点**相对行起点的时刻（秒）。
///
/// 与各模式解码器的通道布局严格对应（[`crate::mode_pd`] /
/// [`crate::mode_scottie`] 的 `chan_starts_sec`）：
/// - [`SyncPosition::LineStart`]（PD/Robot/Martin）：同步脉冲占据
///   `[0, sync_seconds)`，中点在 `sync_seconds / 2`。
/// - [`SyncPosition::Scottie`]：同步脉冲夹在 B 与 R 之间，起点为
///   `2·septr + 2·chan_len`，中点再加上 `sync_seconds / 2`。
#[must_use]
pub(crate) fn sync_mid_offset_seconds(spec: ModeSpec) -> f64 {
    let chan_len = f64::from(spec.line_pixels) * spec.pixel_seconds;
    match spec.sync_position {
        crate::modespec::SyncPosition::LineStart => spec.sync_seconds * 0.5,
        crate::modespec::SyncPosition::Scottie => {
            2.0 * spec.septr_seconds + 2.0 * chan_len + spec.sync_seconds * 0.5
        }
    }
}

/// 逐行相位跟踪：返回每个无线电线（帧）的行起点，单位为工作率样本
/// （浮点，与 [`SyncResult::skip_samples`] 同一坐标系）。
///
/// `base_skip` / `base_rate` 是 [`find_sync`] 的全局估计。行内相对相位
/// 来自实测的同步脉冲中点，因此不再受全局估计量化误差的影响；`base_rate`
/// 只决定缺测行的等差外推步长。某行没有实测值时用邻近实测点线性插值；
/// 实测链太短（不足三分之一）时整体退回全局等差模型，避免用不可靠的行号
/// 把图像错位。
///
/// `frame_count` 是无线电线数（PD 为行对数）。
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
pub(crate) fn track_line_starts(
    has_sync: &[bool],
    audio: &[f32],
    work_rate: f64,
    spec: ModeSpec,
    base_skip: i64,
    base_rate: f64,
    frame_count: u32,
    hedr_shift_hz: f64,
) -> Vec<f64> {
    let n_frames = frame_count as usize;
    // 行间距（样本）沿用全局估计的 rate —— 与 `skip + n × line_seconds ×
    // rate` 的等差模型一致；实测点本身不受此值影响。
    let line_samples = spec.line_seconds * base_rate;
    let model_start = |n: f64| -> f64 { base_skip as f64 + n * line_samples };
    let fallback: Vec<f64> = (0..n_frames).map(|n| model_start(n as f64)).collect();
    if n_frames == 0 || line_samples <= 0.0 {
        return fallback;
    }
    let mid_off = sync_mid_offset_seconds(spec) * work_rate;

    // 边沿精化用的长短 Hann 窗（双窗长外推）。
    let win_long = hann_window::<EDGE_WIN_LEN>();
    let win_short = hann_window::<EDGE_WIN_LEN_SHORT>();

    // 1) 把 has_sync 的连续 true 段切成 (上升沿, 下降沿)，逐个精化成
    //    亚样本精度的时刻；时长不合理的段（VIS 位等）丢弃。
    let sync_lo = spec.sync_seconds * work_rate * SPAN_LEN_LO;
    let sync_hi = spec.sync_seconds * work_rate * SPAN_LEN_HI;
    let mut candidates: Vec<f64> = Vec::new();
    let mut i = 0_usize;
    while i < has_sync.len() {
        if has_sync[i] {
            let rise_idx = i;
            while i < has_sync.len() && has_sync[i] {
                i += 1;
            }
            let fall_idx = i;
            let rise_c = (rise_idx * SYNC_PROBE_STRIDE) as f64;
            let fall_c = (fall_idx * SYNC_PROBE_STRIDE) as f64;
            if let (Some(t_rise), Some(t_fall)) = (
                extrapolate_edge(audio, rise_c, true, hedr_shift_hz, &win_long, &win_short),
                extrapolate_edge(audio, fall_c, false, hedr_shift_hz, &win_long, &win_short),
            ) {
                let dur = t_fall - t_rise;
                if dur >= sync_lo && dur <= sync_hi {
                    // 两沿的过零偏移已在 [`extrapolate_edge`] 里消掉，中点
                    // 就是真实的 sync 中心；减去行内中点偏移得行起点。
                    candidates.push((t_rise + t_fall) * 0.5 - mid_off);
                }
            }
        } else {
            i += 1;
        }
    }

    // 2) 候选 → 行号。**不**用 `base_skip` 的相位做严格匹配：折叠累加给出
    //    的 `skip` 带有半行量级的相位歧义（`X_ACC_SLIP_THRESHOLD` 的
    //    slip-wrap），严格匹配会全军覆没。改为用候选**自身的时间连续性**
    //    分链：相邻候选的间隔落在 1..=3 行（且小数部分够小）就续链，否则
    //    断链重开；取最长链作为图像段，链内行号累加得到。`base_skip` 只
    //    决定整条链的整体行号（相差整数行只会让图像整体平移一行，不影响
    //    行内对齐）。
    let mut best_chain: Option<(usize, usize)> = None; // (链首下标, 链长)
    let (mut chain_start, mut chain_len) = (0_usize, 1_usize);
    for k in 1..candidates.len() {
        let rel = (candidates[k] - candidates[k - 1]) / line_samples;
        let step = rel.round();
        if (1.0..=3.0).contains(&step) && (rel - step).abs() <= MATCH_TOL_FRAMES {
            chain_len += 1;
        } else {
            if best_chain.map_or(true, |(_, l)| chain_len > l) {
                best_chain = Some((chain_start, chain_len));
            }
            chain_start = k;
            chain_len = 1;
        }
    }
    if best_chain.map_or(true, |(_, l)| chain_len > l) {
        best_chain = Some((chain_start, chain_len));
    }

    let mut per_frame: Vec<Vec<f64>> = vec![Vec::new(); n_frames];
    if let Some((start, len)) = best_chain {
        let n0 = ((candidates[start] - model_start(0.0)) / line_samples).round();
        let mut n_run = n0;
        for k in 0..len {
            if k > 0 {
                let rel = (candidates[start + k] - candidates[start + k - 1]) / line_samples;
                n_run += rel.round();
            }
            if n_run >= 0.0 {
                let n_us = n_run as usize;
                if n_us < n_frames {
                    per_frame[n_us].push(candidates[start + k]);
                }
            }
        }
    }

    // 3) 每帧取中值；实测太少说明行号分配不可靠 → 退回全局模型。
    let mut meas: Vec<Option<f64>> = Vec::with_capacity(n_frames);
    for v in &per_frame {
        meas.push(median(v));
    }
    let hits = meas.iter().filter(|m| m.is_some()).count();
    if hits * 3 < n_frames {
        return fallback;
    }

    // 4) 缺测行线性插值（两端沿用最近两个实测点的行间周期外推）。
    let mut filled: Vec<f64> = vec![0.0; n_frames];
    for n in 0..n_frames {
        if let Some(v) = meas[n] {
            filled[n] = v;
            continue;
        }
        let prev = (0..n).rev().find(|&k| meas[k].is_some());
        let next = (n + 1..n_frames).find(|&k| meas[k].is_some());
        filled[n] = match (prev, next) {
            (Some(p), Some(q)) => {
                let fp = meas[p].unwrap_or(0.0);
                let fq = meas[q].unwrap_or(0.0);
                fp + (fq - fp) * (n - p) as f64 / (q - p) as f64
            }
            (Some(p), None) => {
                let fp = meas[p].unwrap_or(0.0);
                let per = if p > 0 {
                    fp - meas[p - 1].unwrap_or(fp)
                } else {
                    line_samples
                };
                let step = if per.abs() > 1e-6 { per } else { line_samples };
                fp + step * (n - p) as f64
            }
            (None, Some(q)) => {
                let fq = meas[q].unwrap_or(0.0);
                let per = if q + 1 < n_frames {
                    meas[q + 1].unwrap_or(fq) - fq
                } else {
                    line_samples
                };
                let step = if per.abs() > 1e-6 { per } else { line_samples };
                fq - step * (q - n) as f64
            }
            (None, None) => model_start(n as f64),
        };
    }

    // 5) 局部加权线性平滑：窗口内的线性漂移原样保留，随机抖动被压低。
    let mut out = vec![0.0; n_frames];
    for n in 0..n_frames {
        let lo = n.saturating_sub(SMOOTH_HALO);
        let hi = (n + SMOOTH_HALO + 1).min(n_frames);
        let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for k in lo..hi {
            let x = k as f64 - n as f64;
            let w = 1.0 - x.abs() / (SMOOTH_HALO as f64 + 1.0);
            let y = filled[k];
            sw += w;
            sx += w * x;
            sy += w * y;
            sxx += w * x * x;
            sxy += w * x * y;
        }
        let det = sw * sxx - sx * sx;
        out[n] = if det.abs() > 1e-9 {
            // 取回归线在 x = 0（即第 n 行）处的值。
            let b = (sw * sxy - sx * sy) / det;
            (sy - b * sx) / sw
        } else {
            sy / sw
        };
    }
    out
}

/// 边沿精化用的 Hann 窗（两端为 0、中间为 1）。
fn hann_window<const N: usize>() -> [f32; N] {
    let mut win = [0.0_f32; N];
    for (i, w) in win.iter_mut().enumerate() {
        let x = 2.0 * std::f64::consts::PI * i as f64 / (N - 1) as f64;
        *w = (0.5 - 0.5 * x.cos()) as f32;
    }
    win
}

/// 双窗长外推的边沿精化。
///
/// 过零检测的系统偏移近似正比于分析窗长（功率要在**窗中心**越过边沿才
/// 完成过渡），因此用长短两个窗各测一次，按窗长线性外推到"零窗长"，得到
/// 无偏的边沿时刻。外推之后上升/下降沿中点不再带恒定偏移 —— 恒定偏移
/// 会在整幅图一侧留下固定宽度的边缘条纹，故必须消掉。
fn extrapolate_edge(
    audio: &[f32],
    coarse: f64,
    rise: bool,
    hedr_shift_hz: f64,
    win_long: &[f32],
    win_short: &[f32],
) -> Option<f64> {
    let t_long = refine_edge(audio, coarse, rise, hedr_shift_hz, win_long)?;
    let t_short = refine_edge(audio, coarse, rise, hedr_shift_hz, win_short)?;
    // t(W) ≈ t_true − k·W ⇒ t_true ≈ t_short + (t_short − t_long) · W_s/(W_l − W_s)
    let wl = win_long.len() as f64;
    let ws = win_short.len() as f64;
    if (wl - ws).abs() < f64::EPSILON {
        return Some(t_short);
    }
    Some(t_short + (t_short - t_long) * (ws / (wl - ws)))
}

/// 精确频率的 Goertzel 功率（不做整数 bin 量化）。
///
/// [`crate::dsp::goertzel_power`] 会把目标频率吸附到最近的整数 bin
/// （`k = floor(0.5 + n·f/fs)`），而 1200/1500/1800 Hz 都不落在 bin 中心，
/// 各自的幅度响应不对称 —— 这会给边沿过零带来一个**与窗长无关**的常数
/// 偏移，双窗长外推消不掉（实测约 5.7 样本）。这里直接用连续频率计算，
/// 三个判据频率的响应对称，常数偏移随之消失。
fn goertzel_power_exact(samples: &[f32], target_hz: f64, rate: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * target_hz / rate;
    let coeff = 2.0 * w.cos();
    let (mut s_prev, mut s_prev2) = (0.0_f64, 0.0_f64);
    for &x in samples {
        let s = f64::from(x) + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let real = s_prev - s_prev2 * w.cos();
    let imag = s_prev2 * w.sin();
    real.mul_add(real, imag * imag)
}

/// 在粗位置 `coarse` 附近，用 1200 Hz 与视频带（1500/1800 Hz）的功率差
/// 过零点精化一个边沿（工作率样本，浮点）。`rise == true` 找上升沿
/// （视频→同步），否则找下降沿（同步→视频）。找不到过零时返回 `None`。
///
/// 判据取 `P(1200) − P(1500) − P(1800)`：同步脉冲是纯 1200 Hz，而边沿两侧
/// 的视频/门廊分别落在 1800 Hz 与 1500 Hz（PD 的门廊就是 1500 Hz 黑电平），
/// 只跟其中一个比会让"同步→门廊"这一侧不过零、边沿丢失。
///
/// 过零点相对粗定位有约**半个窗**的系统偏移：功率在窗覆盖到边沿时才完成
/// 过渡，因此过零出现在 `coarse − win/2` 附近。搜索以该期望位置为中心，
/// 取离它最近的过零。
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn refine_edge(
    audio: &[f32],
    coarse: f64,
    rise: bool,
    hedr_shift_hz: f64,
    win: &[f32],
) -> Option<f64> {
    let win_len = win.len().min(EDGE_WIN_LEN);
    let f_sync = 1200.0 + hedr_shift_hz;
    let f_porch = 1500.0 + hedr_shift_hz;
    let f_video = 1800.0 + hedr_shift_hz;
    // 过零的期望位置：窗中心对齐真实边沿 → 滑窗起点在边沿前 win/2。实际
    // 过零点相对它还有一段与窗形/信号有关的偏移（实测约为半窗的一半到一
    // 倍），因此搜索半径取"窗半宽 + 余量"，保证过零落在范围内。
    let half_win = (win_len as f64) * 0.5;
    let expected = coarse - half_win;
    let lo = (expected - half_win - EDGE_SEARCH_SAMPLES).floor() as i64;
    let hi = (expected + half_win + EDGE_SEARCH_SAMPLES).ceil() as i64;
    let mut buf = [0.0_f32; EDGE_WIN_LEN];
    let mut prev: Option<(f64, f64)> = None;
    let mut best: Option<(f64, f64)> = None; // (与 expected 的距离, 时刻)
    for s in lo..=hi {
        if s < 0 || (s as usize) + win_len > audio.len() {
            prev = Some((s as f64, 0.0));
            continue;
        }
        for (idx, slot) in buf.iter_mut().take(win_len).enumerate() {
            *slot = audio[s as usize + idx] * win[idx];
        }
        let work_rate = f64::from(crate::resample::WORKING_SAMPLE_RATE_HZ);
        let d = goertzel_power_exact(&buf[..win_len], f_sync, work_rate)
            - goertzel_power_exact(&buf[..win_len], f_porch, work_rate)
            - goertzel_power_exact(&buf[..win_len], f_video, work_rate);
        if let Some((pt, pd)) = prev {
            let crossed = if rise {
                pd <= 0.0 && d > 0.0
            } else {
                pd > 0.0 && d <= 0.0
            };
            if crossed {
                let denom = d - pd;
                let frac = if denom.abs() < f64::EPSILON {
                    0.5
                } else {
                    -pd / denom
                };
                let t = pt + frac * (s as f64 - pt);
                let dist = (t - expected).abs();
                if best.map_or(true, |(bd, _)| dist < bd) {
                    best = Some((dist, t));
                }
            }
        }
        prev = Some((s as f64, d));
    }
    best.map(|(_, t)| t)
}

/// 中位数（偶数个取两端平均）。空切片返回 `None`。
fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) * 0.5
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;
    use crate::modespec;
    use crate::resample::WORKING_SAMPLE_RATE_HZ;
    use std::f64::consts::PI;

    fn synth_tone(freq_hz: f64, secs: f64) -> Vec<f32> {
        let n = (secs * f64::from(WORKING_SAMPLE_RATE_HZ)).round() as usize;
        (0..n)
            .map(|i| {
                let t = (i as f64) / f64::from(WORKING_SAMPLE_RATE_HZ);
                (2.0 * PI * freq_hz * t).sin() as f32
            })
            .collect()
    }

    #[test]
    fn has_sync_at_detects_1200_hz_burst() {
        let mut tracker = SyncTracker::new(0.0);
        let audio = synth_tone(1200.0, 0.050);
        assert!(tracker.has_sync_at(&audio, audio.len() / 2));
    }

    #[test]
    fn has_sync_at_rejects_1900_hz_tone() {
        let mut tracker = SyncTracker::new(0.0);
        let audio = synth_tone(1900.0, 0.050);
        assert!(!tracker.has_sync_at(&audio, audio.len() / 2));
    }

    #[test]
    fn has_sync_at_rejects_silence() {
        let mut tracker = SyncTracker::new(0.0);
        assert!(!tracker.has_sync_at(&vec![0.0_f32; 1024], 512));
    }

    /// Build a synthetic `has_sync` track with a sync pulse at every line start.
    fn synth_has_sync(spec: ModeSpec, rate_hz: f64) -> Vec<bool> {
        let total = (f64::from(spec.image_lines) * spec.line_seconds * rate_hz
            / (SYNC_PROBE_STRIDE as f64)) as usize
            + 16;
        let mut track = vec![false; total];
        for y in 0..spec.image_lines {
            let i_start =
                (f64::from(y) * spec.line_seconds * rate_hz / (SYNC_PROBE_STRIDE as f64)) as usize;
            let i_end = ((f64::from(y) * spec.line_seconds + spec.sync_seconds) * rate_hz
                / (SYNC_PROBE_STRIDE as f64)) as usize;
            for slot in track.iter_mut().take(i_end.min(total)).skip(i_start) {
                *slot = true;
            }
        }
        track
    }

    /// Build a synthetic `has_sync` track where the signal was *captured*
    /// at `capture_rate_hz` but the true line cadence runs at
    /// `true_rate_hz`. Each captured line is `true_rate / capture_rate`
    /// of a real line, so sync pulses drift through the (probe-stride-
    /// quantized) track — i.e. the slant is non-90°.
    fn synth_has_sync_slanted(
        spec: ModeSpec,
        true_rate_hz: f64,
        // Documents the intended `find_sync(track, capture_rate, spec)`
        // call site; not used in synthesis (pulses are placed at
        // true-rate cadence — find_sync interprets the buffer at
        // capture_rate_hz, so the y-linear position difference between
        // expected and actual indices is the slant). Underscored to
        // suppress the unused-arg warning; keep the parameter as
        // self-documenting API.
        _capture_rate_hz: f64,
    ) -> Vec<bool> {
        let total = (f64::from(spec.image_lines) * spec.line_seconds * true_rate_hz
            / (SYNC_PROBE_STRIDE as f64)) as usize
            + 16;
        let mut track = vec![false; total];
        for y in 0..spec.image_lines {
            let i_start = (f64::from(y) * spec.line_seconds * true_rate_hz
                / (SYNC_PROBE_STRIDE as f64)) as usize;
            let i_end =
                i_start + (spec.sync_seconds * true_rate_hz / (SYNC_PROBE_STRIDE as f64)) as usize;
            for slot in track.iter_mut().take(i_end.min(total)).skip(i_start) {
                *slot = true;
            }
        }
        track
    }

    #[test]
    fn find_sync_locks_clean_track_to_90_degrees() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Pd120);
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&synth_has_sync(spec, rate), rate, spec, &mut scratch);
        let slant = r.slant_deg.expect("sync detected");
        assert!((slant - 90.0).abs() < 1.0, "{slant:.2}°");
        assert!((r.adjusted_rate_hz - rate).abs() / rate < 0.005);
        assert!(r.skip_samples.abs() < (0.05 * rate) as i64);
    }

    /// With all-zero `has_sync`, the Hough transform finds nothing.
    /// `slant_deg` must be `None` (not `Some(90.0)`) and `skip_samples`
    /// must encode a negative offset (xmax=0, no sync detected).
    /// Verifies round-2 audit Finding 6 (xmax=0 on zero input) and
    /// Finding 10 (`slant_deg` is None, not the misleading 90.0 default).
    #[test]
    fn find_sync_empty_track_has_no_slant_detected() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Pd120);
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&vec![false; 16384], rate, spec, &mut scratch);
        assert!(
            r.slant_deg.is_none(),
            "empty track should yield slant_deg=None, got {:?}",
            r.slant_deg
        );
        // xmax=0 → s_secs = 0 - sync_seconds → skip is negative.
        assert!(
            r.skip_samples < 0,
            "empty track skip should be negative (xmax=0)"
        );
    }

    #[test]
    fn find_sync_recovers_known_offset() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Pd120);
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        // Right-shift the track by ~10 ms (a real-radio settling gap).
        let mut track = synth_has_sync(spec, rate);
        let shift = ((0.010 * rate) / (SYNC_PROBE_STRIDE as f64)) as usize;
        let mut shifted = vec![false; shift];
        shifted.append(&mut track);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&shifted, rate, spec, &mut scratch);
        let expected = (0.010 * rate) as i64;
        // 700-bin row ≈ 0.7 ms / bin at PD120; allow a few bins for wobble.
        assert!(
            (r.skip_samples - expected).abs() < (0.005 * rate) as i64,
            "Skip off (expected ≈ {expected}, got {})",
            r.skip_samples
        );
    }

    #[test]
    fn find_sync_handles_empty_track() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Pd120);
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&vec![false; 16384], rate, spec, &mut scratch);
        assert!(r.adjusted_rate_hz.is_finite());
        assert!((r.adjusted_rate_hz - rate).abs() < 1.0);
        // Rate must be bit-exact when no sync detected (no rate correction ran).
        assert!(
            (r.adjusted_rate_hz - rate).abs() < f64::EPSILON,
            "rate should be unchanged, got {}",
            r.adjusted_rate_hz
        );
    }

    /// F2 (#88). Hough slant correction path — 0.5% capture-rate drift
    /// produces a slant well outside the (89°, 91°) lock window
    /// (`tan(90° − slant)/line_width ≈ 0.005` implies a Hough peak
    /// near 63° or 117° depending on which symmetry the dominant
    /// line in `sync_img` lands in). The retry loop must shrink the
    /// rate error toward zero; we assert it ends up under half the
    /// initial drift.
    /// (0.3% drift falls *inside* the 0.5°-quantized Hough bins as
    /// near-90°, hits the 0.5° deadband, and never triggers
    /// correction; 0.5% is the minimum drift that reliably runs the
    /// correction path.)
    #[test]
    fn find_sync_corrects_0p5pct_slant_at_pd120() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Pd120);
        let true_rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let capture_rate = true_rate * 1.005;
        let track = synth_has_sync_slanted(spec, true_rate, capture_rate);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&track, capture_rate, spec, &mut scratch);
        let err_pct = (r.adjusted_rate_hz - true_rate).abs() / true_rate * 100.0;
        let initial_err_pct = (capture_rate - true_rate).abs() / true_rate * 100.0;
        // Verify sync was detected at all.
        assert!(
            r.slant_deg.is_some(),
            "expected sync to be detected (slant_deg should be Some)"
        );
        // Rate should have moved toward true_rate (correction was applied).
        assert!(
            r.adjusted_rate_hz < capture_rate,
            "adjusted rate {:.2} should be less than capture_rate {capture_rate:.2} (slant correction moved it toward true_rate)",
            r.adjusted_rate_hz
        );
        // Final error should be well under the initial drift.
        assert!(
            err_pct < initial_err_pct / 2.0,
            "rate err {err_pct:.3}% should be < half of initial {initial_err_pct:.3}%"
        );
    }

    /// F2 (#88). Larger drift (1% capture-rate offset) produces a
    /// Hough peak far outside the lock window — the retry loop must
    /// do real work to converge. Verify the retry loop actually
    /// progresses: the final rate error must be strictly smaller
    /// than the initial guess.
    #[test]
    fn find_sync_corrects_1pct_slant_via_retries() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Pd120);
        let true_rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let capture_rate = true_rate * 1.01;
        let track = synth_has_sync_slanted(spec, true_rate, capture_rate);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&track, capture_rate, spec, &mut scratch);
        let initial_err_pct = (capture_rate - true_rate).abs() / true_rate * 100.0;
        let final_err_pct = (r.adjusted_rate_hz - true_rate).abs() / true_rate * 100.0;
        assert!(
            final_err_pct < initial_err_pct,
            "retry should shrink rate error: initial {initial_err_pct:.3}% → final {final_err_pct:.3}%"
        );
        assert!(
            final_err_pct < 0.2,
            "rate err after retries should be ≤ 0.2%, got {final_err_pct:.3}%"
        );
    }

    /// F3 (#88). Scottie modes use mid-line sync; the
    /// `skip_correction_seconds()` path on `ModeSpec` subtracts
    /// `chan_len/2 - 2*porch` from the raw `s_secs`. Feeding
    /// line-start pulses (the existing `synth_has_sync` helper)
    /// with a Scottie spec lands `xmax` near 0 (small), so
    /// `s_secs_raw ≈ 0` and the final skip should equal the
    /// correction itself (negative, ~ -65 ms for Scottie1 at
    /// 11025 Hz).
    #[test]
    fn find_sync_scottie_applies_skip_correction() {
        let spec = modespec::for_mode(crate::modespec::SstvMode::Scottie1);
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let track = synth_has_sync(spec, rate);
        let mut scratch = FindSyncScratch::new();
        let r = find_sync(&track, rate, spec, &mut scratch);

        let chan_len = f64::from(spec.line_pixels) * spec.pixel_seconds;
        let expected_secs = -chan_len / 2.0 + 2.0 * spec.porch_seconds;
        let expected_skip = (expected_secs * rate).round() as i64;
        let tolerance = (0.005 * rate) as i64; // ~55 samples ≈ 5 ms

        assert!(
            (r.skip_samples - expected_skip).abs() < tolerance,
            "Scottie skip {} should be ≈ {expected_skip} (correction = {expected_secs:.4}s, tol = {tolerance})",
            r.skip_samples
        );
        // Sanity: Scottie correction is always negative.
        assert!(
            r.skip_samples < 0,
            "Scottie skip should be negative (mid-line hoist); got {}",
            r.skip_samples
        );
    }

    /// A6 regression guard (#88). slowrx C's loop runs `n ∈ 0..691`
    /// (`for (n=0; n<X_ACC_BINS-8; n++)`), so `xAcc[699]` is never
    /// read. Rust's native `windows(8)` over a 700-bin slice yields
    /// 693 windows (`n ∈ 0..=692`); without `.take(X_ACC_BINS - 8)`
    /// the kernel would read `xAcc[699]` at `n=692`. This test
    /// constructs an `x_acc` whose strongest convd at n=692 differs
    /// from the strongest at n=691: pre-fix lands at n=692 (xmax=696
    /// → slip-wrap → 346); post-fix lands at n=691 (xmax=695 →
    /// slip-wrap → 345). The assertion fails on the pre-fix code and
    /// passes on the post-fix code.
    #[test]
    fn falling_edge_from_x_acc_off_by_one_a6() {
        let mut x_acc = vec![0u32; X_ACC_BINS];
        // x_acc[691..=695] = 100, x_acc[696..=699] = 0.
        for slot in x_acc.iter_mut().take(696).skip(691) {
            *slot = 100;
        }
        // n=691: window = [100,100,100,100,100,0,0,0]
        //   convd = 4*100 - 100 - 0 - 0 - 0 = 300.
        // n=692: window = [100,100,100,100,0,0,0,0]
        //   convd = 4*100 - 0 = 400  (pre-fix only).
        // Pre-fix max at n=692 → xmax = 696 → slip-wrap → 346.
        // Post-fix max at n=691 → xmax = 695 → slip-wrap → 345.
        let xmax = falling_edge_from_x_acc(&x_acc);
        assert_eq!(
            xmax, 345,
            "post-A6-fix should pick n=691 (xmax=695, slip=345); pre-fix would give 346"
        );
    }

    /// A6 baseline: an edge well away from the right edge produces
    /// the same `xmax` pre-fix and post-fix. Sanity check that the
    /// `.take(...)` bound only changes behavior at the very right
    /// edge, not anywhere else.
    #[test]
    fn falling_edge_from_x_acc_detects_mid_array_edge() {
        let mut x_acc = vec![0u32; X_ACC_BINS];
        // Edge at indices 100..=103.
        for slot in x_acc.iter_mut().take(104).skip(100) {
            *slot = 100;
        }
        // n=100: window = [100,100,100,100,0,0,0,0], convd = 400,
        // xmax = 100 + 4 = 104. 104 < X_ACC_SLIP_THRESHOLD (350), no
        // slip-wrap. Pre-fix and post-fix agree.
        let xmax = falling_edge_from_x_acc(&x_acc);
        assert_eq!(xmax, 104, "mid-array edge detection unchanged by A6 fix");
    }

    /// 双窗长外推的边沿精化：过零检测带正比于窗长的系统偏移，外推到
    /// “零窗长”后应是**无偏**的（残差 < 1.5 样本）。这个偏移若留着
    /// （单窗实测约 28 样本 ≈ 2.6 像素），会在整幅图一侧留下固定宽度的
    /// 边缘彩色条纹。
    #[test]
    fn extrapolate_edge_removes_window_bias() {
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let mut audio: Vec<f32> = Vec::new();
        // 视频(1800 Hz) → 行同步(1200 Hz, 20 ms) → 门廊(1500 Hz)
        for (freq, secs) in [(1800.0, 0.20), (1200.0, 0.020), (1500.0, 0.20)] {
            push_tone(&mut audio, freq, secs, rate);
        }
        let rise_true = (0.20 * rate).round();
        let fall_true = (0.22 * rate).round();
        let win_long = hann_window::<EDGE_WIN_LEN>();
        let win_short = hann_window::<EDGE_WIN_LEN_SHORT>();

        for (coarse, rise, truth) in [
            (rise_true + 2.0, true, rise_true),
            (fall_true - 2.0, false, fall_true),
        ] {
            let raw = refine_edge(&audio, coarse, rise, 0.0, &win_long).expect("long-window edge");
            let ext = extrapolate_edge(&audio, coarse, rise, 0.0, &win_long, &win_short)
                .expect("extrapolated edge");
            // 外推必须比单窗显著更接近真值（单窗带正比于窗长的偏移）。
            assert!(
                (ext - truth).abs() < (raw - truth).abs(),
                "外推值 {ext:.2} 应优于单窗 {raw:.2}（真值 {truth:.1}）"
            );
            // 残余是与窗长无关的常数项（两侧信号谱形不对称），约 4~5 样本
            // （≈1 像素）：远小于通道首尾的安全余量会被突破的量级。
            assert!(
                (ext - truth).abs() < 6.0,
                "外推值 {ext:.2} 应贴住真值 {truth:.1}（单窗 {raw:.2}）"
            );
        }
    }

    /// 逐行相位跟踪：合成一段行周期**故意偏离标称**的 PD-240 风格音频
    /// （模拟真实发射端每行 44102 样本而非 44100），断言跟踪出的行起点
    /// 逐行对齐到实测同步，而不是跟着 `find_sync` 的等差模型一起漂移。
    #[test]
    fn track_line_starts_follows_per_line_drift() {
        let rate = f64::from(WORKING_SAMPLE_RATE_HZ);
        let spec = modespec::for_mode(modespec::SstvMode::Pd240);
        // 每行 11026 个样本（标称 11025），即每行 +1 样本的时钟偏差。
        let line_len = (spec.line_seconds * rate).round() as usize + 1;
        let sync_len = (spec.sync_seconds * rate).round() as usize;
        let porch_len = (spec.porch_seconds * rate).round() as usize;

        let mut audio: Vec<f32> = Vec::new();
        let mut true_starts: Vec<f64> = Vec::new();
        for _ in 0..32 {
            true_starts.push(audio.len() as f64);
            push_tone(&mut audio, 1200.0, sync_len as f64 / rate, rate);
            push_tone(&mut audio, 1500.0, porch_len as f64 / rate, rate);
            let video = line_len - sync_len - porch_len;
            push_tone(&mut audio, 1800.0, video as f64 / rate, rate);
        }

        // has_sync 轨迹：按每 4 个样本一次的探测节奏重建（同步段为 true）。
        let mut has_sync: Vec<bool> = Vec::new();
        for k in 0..(audio.len() / SYNC_PROBE_STRIDE) {
            let center = k * SYNC_PROBE_STRIDE + SYNC_PROBE_STRIDE / 2;
            has_sync.push((center % line_len) < sync_len);
        }

        let starts = track_line_starts(
            &has_sync,
            &audio,
            rate,
            spec,
            0,
            rate,
            true_starts.len() as u32,
            0.0,
        );
        // 逐行对齐：每行起点与实测真值的差应在 8 样本内（0.7 ms ≈ 2
        // 像素；含 has_sync 的 4 样本量化、双窗长外推的误差放大与平滑
        // 残余）。整体行号偏移用 `offset` 归一（等差模型的整体相位歧义
        // 只让图像整体平移）。对照：等差模型在 32 行上有 31 样本（≈7
        // 像素）的累积漂移，跟踪残余必须远小于它。
        let offset = starts[0] - true_starts[0];
        for (n, (&s, &t)) in starts.iter().zip(true_starts.iter()).enumerate() {
            assert!(
                ((s - offset) - t).abs() < 8.0,
                "行 {n} 起点偏差 {:.2} 样本（等差模型会是 {n} 样本）",
                (s - offset) - t
            );
        }
    }

    /// 追加一段单音（相位连续）。
    fn push_tone(audio: &mut Vec<f32>, freq: f64, secs: f64, rate: f64) {
        let n = (secs * rate).round() as usize;
        // 相位基准取**段首**下标：循环里 `audio.len()` 会随 push 增长，
        // 直接用它会让频率翻倍。
        let base = audio.len();
        for i in 0..n {
            let t = (base + i) as f64 / rate;
            audio.push((2.0 * PI * freq * t).sin() as f32);
        }
    }
}
