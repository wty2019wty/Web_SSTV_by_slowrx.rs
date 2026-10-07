//! Internal rational resampler: caller's audio rate → 11025 Hz working rate.
//!
//! Hand-rolled 64-tap Hann-windowed-sinc polyphase FIR with 256 phase
//! positions. Tap rows are precomputed once in
//! [`Resampler::new`] (~64 KB); the hot path in [`Resampler::process`] is
//! a quantized-phase lookup + 64-tap multiply-accumulate — no
//! transcendentals per output sample.
//!
//! We picked this over `rubato` for zero extra deps and a small file.
//! Quality target is "audible loss < 0.1 dB across SSTV-relevant
//! frequencies (1500-2300 Hz)" — easily met at typical input rates
//! (44.1k, 48k). Translated in spirit from slowrx's implicit resampling
//! inside `pcm.c`'s 44.1 kHz read loop.

use crate::error::{Error, Result};

/// Working sample rate the decoder operates at internally. Any caller
/// sample rate is resampled to this before processing.
pub const WORKING_SAMPLE_RATE_HZ: u32 = 11_025;

/// Maximum supported caller input sample rate.
pub const MAX_INPUT_SAMPLE_RATE_HZ: u32 = 192_000;

/// Number of FIR taps. Higher = sharper transition + more CPU.
/// 64 is the sweet spot at our quality target.
const FIR_TAPS: usize = 64;

/// Number of polyphase positions. Each fractional output sample's
/// `frac` is quantized to one of `NUM_PHASES` precomputed tap rows via
/// round-to-nearest with a clamp at the top edge (so the bucket for
/// `frac` very close to 1.0 is one-sided rather than wrapping). 256
/// gives a max sub-sample position error of `1 / (2·NUM_PHASES) = 1/512`
/// sample across the interior, rising to `1/NUM_PHASES = 1/256` at the
/// top bucket (`frac > (NUM_PHASES − 0.5)/NUM_PHASES`); at 11025 Hz this
/// is ≈ 177 ns typical / 354 ns worst-case time error. RMS phase noise
/// on a 2300 Hz tone (SSTV's highest video frequency) is ≈ −52 dB, well
/// below the audible threshold and SSTV's noise floor. Memory cost:
/// `NUM_PHASES × FIR_TAPS × 4 B` = 64 KB per `Resampler`.
const NUM_PHASES: usize = 256;

/// Polyphase FIR resampler. Stateful — holds a tail buffer to avoid
/// glitches across `process` calls.
///
/// **Group delay:** the 64-tap symmetric FIR has linear-phase group delay
/// of `(FIR_TAPS - 1) / 2 = 31.5` input-rate samples (≈ 715 µs at 44.1 kHz,
/// ≈ 2.86 ms at 11.025 kHz). Output is shifted right by this amount
/// relative to input. SSTV's `find_sync` re-anchors the rate against sync
/// pulses, so this is invisible inside the decoder pipeline; standalone
/// consumers should compensate if they need sample-accurate alignment.
#[derive(Debug)]
pub struct Resampler {
    input_rate: u32,
    /// `input_rate / WORKING_SAMPLE_RATE_HZ`, expressed as a stride.
    /// 仅用于 `process` 的输出容量预估；样本位置由 [`Self::out_index`]
    /// 精确推导，不走浮点累加。
    stride: f64,
    /// 已产出的工作率样本数。第 `out_index` 个输出对应的**输入**位置是
    /// `out_index × input_rate / WORKING_SAMPLE_RATE_HZ`（精确有理数）。
    ///
    /// 位置由这个全局序号推导，而不是在 `process` 里对 `phase` 浮点累加：
    /// 累加与「按块回退基准」的组合会让舍入路径随输入分块方式变化，
    /// 同一段音频整段喂入与分块喂入会得到不同的输出（相位量化档翻转，
    /// 实测差异达 1e-3 量级），进而让实时小块解码与离线大块解码的
    /// `has_sync` 轨道不同、图像不一致。
    out_index: u64,
    /// Carry-over input samples from the previous call.
    tail: Vec<f32>,
    /// `tail[0]` 对应的全局输入下标（与 [`Self::out_index`] 同一坐标系）。
    tail_start: u64,
    /// 256-phase polyphase tap bank, indexed by `frac` quantized to
    /// 1/256 sub-sample. Built once in [`Resampler::new`] (~64 KB, static
    /// for the resampler's lifetime). Each row is a Hann-windowed sinc at
    /// the corresponding fractional delay. Raw taps (no normalization
    /// pass) — the windowed-sinc form already sums to ~1.0 at typical
    /// `fc` (the audit's D1 claim of "~6 dB attenuation" was a phantom
    /// finding, verified by the
    /// `exact_rate_preserves_amplitude_and_no_attenuation` test).
    taps: Box<[[f32; FIR_TAPS]; NUM_PHASES]>,
}

/// Cutoff frequency (Hz) for the resampler, derived from the input rate.
/// `min(input_rate, WORKING_SAMPLE_RATE_HZ) × 0.45`, hard-capped at 4500
/// Hz. The 0.45 factor leaves a small transition band below Nyquist of
/// the lower rate; the 4500 Hz cap pins the absolute cutoff at typical
/// input rates (44.1k / 48k → 4961 Hz uncapped → 4500 Hz capped), so the
/// passband easily covers SSTV's 1500–2300 Hz video band with room for
/// the 64-tap transition rolloff.
fn cutoff_hz(input_rate: u32) -> f64 {
    (f64::from(input_rate.min(WORKING_SAMPLE_RATE_HZ)) * 0.45).min(4500.0)
}

/// Compute one Hann-windowed sinc FIR tap value for a given tap index
/// and fractional phase. Called once per `(phase, tap)` pair from
/// [`Resampler::new`] to populate the polyphase tap bank — never called
/// from the hot path.
///
/// `tap_index` is in 0..`FIR_TAPS`. `frac` is in [0, 1) — the sub-sample
/// offset of the output sample's center from the integer input grid.
/// `fc` is the cutoff normalized to input rate (`cutoff_hz / input_rate`).
///
/// Sinc shifts with `frac`; Hann window stays anchored to the tap grid.
/// This is the standard windowed-sinc fractional-delay formulation —
/// see e.g. Smith, "Digital Audio Resampling Home Page" (CCRMA, 2002).
///
/// The taps already sum to ~1.0 at typical `fc` (the audit's D1 claim of
/// "~6 dB attenuation" was a phantom finding — see the
/// `exact_rate_preserves_amplitude_and_no_attenuation` test for the
/// regression guard).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn fir_tap(tap_index: usize, frac: f64, fc: f64) -> f32 {
    let m = FIR_TAPS as f64;
    let n = (tap_index as f64) - (m - 1.0) / 2.0 - frac;
    let sinc = if n.abs() < 1e-12 {
        2.0 * fc
    } else {
        (2.0 * std::f64::consts::PI * fc * n).sin() / (std::f64::consts::PI * n)
    };
    let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * (tap_index as f64) / (m - 1.0)).cos());
    // Tap values are bounded in [-1, 1]; the f32 cast is exact-enough.
    (sinc * w) as f32
}

impl Resampler {
    /// Construct a resampler converting `input_rate` → [`WORKING_SAMPLE_RATE_HZ`].
    ///
    /// # Errors
    /// Returns [`Error::InvalidSampleRate`] if `input_rate` is 0 or
    /// > [`MAX_INPUT_SAMPLE_RATE_HZ`].
    #[must_use = "Resampler::new returns a Result; dropping it silently bypasses rate validation"]
    #[allow(clippy::cast_precision_loss, clippy::large_stack_arrays)]
    pub fn new(input_rate: u32) -> Result<Self> {
        if input_rate == 0 || input_rate > MAX_INPUT_SAMPLE_RATE_HZ {
            return Err(Error::InvalidSampleRate { got: input_rate });
        }
        let cutoff_norm = cutoff_hz(input_rate) / f64::from(input_rate);

        // Build the 256-phase polyphase tap bank — one 64-tap row per
        // quantized fractional phase. Computed once here, looked up in
        // the hot path (no transcendentals per output sample). No
        // normalization pass: raw Hann-windowed-sinc taps already sum
        // to ~1.0 at typical `fc` (audit #87 D1 — phantom finding,
        // verified by `exact_rate_preserves_amplitude_and_no_attenuation`).
        let mut taps: Box<[[f32; FIR_TAPS]; NUM_PHASES]> =
            Box::new([[0.0_f32; FIR_TAPS]; NUM_PHASES]);
        for phase_idx in 0..NUM_PHASES {
            let frac = (phase_idx as f64) / (NUM_PHASES as f64);
            for k in 0..FIR_TAPS {
                taps[phase_idx][k] = fir_tap(k, frac, cutoff_norm);
            }
        }

        Ok(Self {
            input_rate,
            stride: f64::from(input_rate) / f64::from(WORKING_SAMPLE_RATE_HZ),
            out_index: 0,
            tail: Vec::new(),
            tail_start: 0,
            taps,
        })
    }

    /// Resample a chunk of input audio into working-rate output.
    ///
    /// 输出与输入分块方式**严格无关**：第 `k` 个输出的输入位置由
    /// `k × input_rate / WORKING_SAMPLE_RATE_HZ` 精确推导（见
    /// [`Self::out_index`]），整段喂入与任意分块喂入得到逐位相同的输出。
    #[must_use = "the resampled audio Vec must be consumed; dropping it discards the decoder input"]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        // Concatenate carry-over with the new chunk.
        let origin = self.tail_start;
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(input);
        let end = origin + buf.len() as u64;

        // Output length is approximately buf.len() / stride ± 1 (phase
        // carry-over). Pre-sizing saves a reallocation per process()
        // call on every audio chunk. (Audit #92 D9.)
        let expected_out = (buf.len() as f64 / self.stride).ceil() as usize;
        let mut out = Vec::with_capacity(expected_out);
        let work = u64::from(WORKING_SAMPLE_RATE_HZ);
        loop {
            // 第 out_index 个输出的输入位置 = out_index × input_rate / work，
            // 拆成精确的整数部分与有理小数部分（分母 work）。
            let pos_num = self.out_index * u64::from(self.input_rate);
            let whole = pos_num / work;
            // D2b off-by-one fix (#87): the kernel reads
            // `[whole, whole + FIR_TAPS)`，因此需要 `whole + FIR_TAPS`
            // 个全局样本已到齐。
            if whole + FIR_TAPS as u64 > end {
                break;
            }
            let frac = (pos_num % work) as f64 / work as f64;
            let phase_idx = ((frac * NUM_PHASES as f64).round() as usize).min(NUM_PHASES - 1);
            let taps = &self.taps[phase_idx];

            // Convolve using the precomputed taps at this quantized phase.
            // No transcendentals in the hot path. `whole ≥ origin` 恒成立
            // （`tail_start` 就是上一轮记录的下一核起点），且核完全落在
            // `buf` 内（循环条件保证）。
            let start = (whole - origin) as usize;
            debug_assert!(start + FIR_TAPS <= buf.len());
            let mut acc: f32 = 0.0;
            for (k, &tap) in taps.iter().enumerate() {
                acc += tap * buf[start + k];
            }
            out.push(acc);
            self.out_index += 1;
        }

        // Keep the trailing samples that the next call will need: from the
        // next output kernel's start index onward.
        let next_whole = (self.out_index * u64::from(self.input_rate)) / work;
        let drop = next_whole.saturating_sub(origin);
        if drop < buf.len() as u64 {
            self.tail = buf[drop as usize..].to_vec();
            self.tail_start = next_whole;
        } else {
            // 只有 `buf` 为空（全新状态或已排空）才会走到这里；`out_index`
            // 与已产出样本不受影响。由 `empty_input_returns_empty` 覆盖。
            self.tail.clear();
            self.tail_start = end;
        }
        out
    }

    /// Caller-provided input sample rate.
    #[must_use]
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// Clear FIR tail buffer + position counter so a subsequent call to
    /// `process` starts with a clean state (new audio is treated as
    /// starting at input position 0). Keeps the input rate, cutoff, and
    /// stride — the rate doesn't change across `reset_state` calls.
    pub(crate) fn reset_state(&mut self) {
        self.tail.clear();
        self.out_index = 0;
        self.tail_start = 0;
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::float_cmp,
    clippy::expect_used
)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn synth_tone_at(rate: u32, freq_hz: f64, secs: f64) -> Vec<f32> {
        let n = (secs * f64::from(rate)).round() as usize;
        (0..n)
            .map(|i| {
                let t = (i as f64) / f64::from(rate);
                (2.0 * PI * freq_hz * t).sin() as f32
            })
            .collect()
    }

    #[test]
    fn rejects_zero_rate() {
        assert!(matches!(
            Resampler::new(0),
            Err(Error::InvalidSampleRate { got: 0 })
        ));
    }

    #[test]
    fn rejects_oversize_rate() {
        assert!(matches!(
            Resampler::new(MAX_INPUT_SAMPLE_RATE_HZ + 1),
            Err(Error::InvalidSampleRate { .. })
        ));
    }

    #[test]
    fn accepts_common_rates() {
        for rate in [8_000, 11_025, 22_050, 32_000, 44_100, 48_000, 96_000] {
            assert!(Resampler::new(rate).is_ok(), "{rate} should be accepted");
        }
    }

    #[test]
    fn passthrough_when_rate_matches_working_rate() {
        // At equal rates the resampler still applies its FIR (no special-case
        // bypass). Verify the output length is approximately equal to input
        // length and that a 1500 Hz tone survives.
        let mut r = Resampler::new(WORKING_SAMPLE_RATE_HZ).unwrap();
        let in_audio = synth_tone_at(WORKING_SAMPLE_RATE_HZ, 1500.0, 0.1);
        let out = r.process(&in_audio);
        // Allow up to FIR_TAPS samples of length variance (group delay + tail).
        let expected = in_audio.len();
        assert!(
            (out.len() as isize - expected as isize).abs() < 100,
            "len mismatch: out={} expected≈{}",
            out.len(),
            expected
        );
        let p = crate::dsp::goertzel_power(&out, 1500.0);
        let p_off = crate::dsp::goertzel_power(&out, 800.0);
        assert!(p > 10.0 * p_off, "tone should survive: {p} vs {p_off}");
    }

    #[test]
    fn resamples_44100_to_11025_preserves_tone_frequency() {
        // 1 second of 1900 Hz at 44100 Hz → resample → expect 1900 Hz at 11025 Hz.
        let mut r = Resampler::new(44_100).expect("44.1k resampler");
        let in_audio = synth_tone_at(44_100, 1900.0, 1.0);
        let out = r.process(&in_audio);
        // Output should be ~11025 samples (1 second at working rate)
        let expected = WORKING_SAMPLE_RATE_HZ as usize;
        assert!(
            (out.len() as isize - expected as isize).abs() < 200,
            "out.len()={} expected≈{expected}",
            out.len()
        );
        // Goertzel power at 1900 Hz should be much greater than at 1700/2100 Hz.
        let p_target = crate::dsp::goertzel_power(&out, 1900.0);
        let p_off1 = crate::dsp::goertzel_power(&out, 1700.0);
        let p_off2 = crate::dsp::goertzel_power(&out, 2100.0);
        assert!(
            p_target > 10.0 * p_off1.max(p_off2),
            "p1900={p_target} p1700={p_off1} p2100={p_off2}"
        );
    }

    #[test]
    fn resamples_48000_to_11025() {
        let mut r = Resampler::new(48_000).expect("48k resampler");
        let in_audio = synth_tone_at(48_000, 1500.0, 0.5);
        let out = r.process(&in_audio);
        let expected = (WORKING_SAMPLE_RATE_HZ / 2) as usize;
        assert!((out.len() as isize - expected as isize).abs() < 200);
    }

    #[test]
    fn resamples_48000_to_11025_preserves_tone_quality() {
        // 0.5 s of 1900 Hz at 48 kHz, non-integer ratio (4.354...).
        // Pre-fix this test would have shown ~10× signal-to-noise margin
        // around 1900 Hz; with proper polyphase the margin should be 100×+.
        let mut r = Resampler::new(48_000).expect("48k resampler");
        let in_audio = synth_tone_at(48_000, 1900.0, 0.5);
        let out = r.process(&in_audio);
        let p_target = crate::dsp::goertzel_power(&out, 1900.0);
        let p_off1 = crate::dsp::goertzel_power(&out, 1700.0);
        let p_off2 = crate::dsp::goertzel_power(&out, 2100.0);
        // Tighter than the integer-ratio 10× threshold — non-integer
        // ratios with broken polyphase would NOT meet this.
        assert!(
            p_target > 50.0 * p_off1.max(p_off2),
            "p1900={p_target} p1700={p_off1} p2100={p_off2} (polyphase quality)"
        );
    }

    #[test]
    fn streaming_calls_are_consistent() {
        let mut r = Resampler::new(44_100).unwrap();
        let in_audio = synth_tone_at(44_100, 1900.0, 0.5);
        let single = r.process(&in_audio);
        let mut r2 = Resampler::new(44_100).unwrap();
        let mid = in_audio.len() / 2;
        let mut split = r2.process(&in_audio[..mid]);
        split.extend_from_slice(&r2.process(&in_audio[mid..]));
        // 分块不变性（严格）：长度与每个样本都必须逐位一致。
        assert_eq!(single.len(), split.len(), "输出长度应严格一致");
        let first_diff = single
            .iter()
            .zip(split.iter())
            .position(|(x, y)| x.to_bits() != y.to_bits());
        assert!(
            first_diff.is_none(),
            "分块喂入应逐位一致，首个差异在 {first_diff:?}"
        );
    }

    /// Unit-gain regression guard (#87). The audit (D1) claimed the 64
    /// Hann-windowed sinc taps weren't normalized to unit DC gain and the
    /// resampler attenuated by ~6 dB. Empirically false — the
    /// windowed-sinc form `2·fc · sin(2π·fc·n)/(π·n)` already sums to
    /// ~1.0 at typical `fc` (the audit appears to have confused the Hann
    /// *window*'s mean (= 0.5) with the Hann-*windowed-sinc*'s DC gain).
    /// This test passes on current code and stays as a guard against any
    /// future change (rate changes, tap-count tweaks, window swaps) that
    /// breaks unit gain unexpectedly. At
    /// `input_rate == WORKING_SAMPLE_RATE_HZ` the stride is exactly 1.0
    /// and every output sample has `frac == 0`, so the fractional-delay
    /// machinery isn't exercised — gain issues show up cleanly.
    #[test]
    fn exact_rate_preserves_amplitude_and_no_attenuation() {
        let mut r = Resampler::new(WORKING_SAMPLE_RATE_HZ).unwrap();
        // 200 samples at amplitude 0.8 — well past the 64-tap kernel ramp-up.
        let amplitude = 0.8_f32;
        let in_audio: Vec<f32> = (0..200)
            .map(|i| {
                let t = f64::from(i) / f64::from(WORKING_SAMPLE_RATE_HZ);
                (f64::from(amplitude) * (2.0 * PI * 1500.0 * t).sin()) as f32
            })
            .collect();
        let out = r.process(&in_audio);
        // Skip the first FIR_TAPS samples — the kernel is ramping up against
        // the left zero-pad and the peak amplitude is reduced there.
        let mid_start = FIR_TAPS.min(out.len());
        let out_peak = out[mid_start..]
            .iter()
            .fold(0.0_f32, |m, &x| m.max(x.abs()));
        let in_peak = in_audio.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        // Allow ±5 % of input peak. The audit predicted ~50 % attenuation
        // (taps would sum to ~0.5); empirically the ratio is ~1.0 — the
        // windowed-sinc taps already have unity DC gain.
        let ratio = out_peak / in_peak;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "expected ~1.0 output peak/input peak ratio (unit gain), got {ratio} (in_peak={in_peak}, out_peak={out_peak})"
        );
    }

    /// F6 (#87). Upsampling 8 kHz → 11025 Hz exercises the `stride < 1`
    /// path that no existing test hits. Output length should be ~11025
    /// samples (1 second at working rate) ±64; Goertzel power at 1500 Hz
    /// should dominate adjacent off-band bins.
    #[test]
    fn upsampling_8khz_to_11025() {
        let mut r = Resampler::new(8_000).unwrap();
        let in_audio = synth_tone_at(8_000, 1500.0, 1.0);
        let out = r.process(&in_audio);
        let expected = WORKING_SAMPLE_RATE_HZ as usize;
        assert!(
            (out.len() as isize - expected as isize).abs() < 200,
            "out.len()={} expected≈{expected}",
            out.len()
        );
        let p_target = crate::dsp::goertzel_power(&out, 1500.0);
        let p_off1 = crate::dsp::goertzel_power(&out, 1200.0);
        let p_off2 = crate::dsp::goertzel_power(&out, 1800.0);
        assert!(
            p_target > 10.0 * p_off1.max(p_off2),
            "p1500={p_target} p1200={p_off1} p1800={p_off2}"
        );
    }

    /// F6 (#87). 192 kHz input — the max supported rate. Stride ≈ 17.41;
    /// many input samples per output. Just verify no panic, output length
    /// is in the right ballpark, and the tone survives.
    #[test]
    fn max_input_rate_192khz() {
        let mut r = Resampler::new(MAX_INPUT_SAMPLE_RATE_HZ).unwrap();
        let in_audio = synth_tone_at(MAX_INPUT_SAMPLE_RATE_HZ, 2000.0, 0.5);
        let out = r.process(&in_audio);
        // 0.5 s at WORKING_SAMPLE_RATE_HZ.
        let expected = (WORKING_SAMPLE_RATE_HZ / 2) as usize;
        assert!(
            (out.len() as isize - expected as isize).abs() < 200,
            "out.len()={} expected≈{expected}",
            out.len()
        );
        let p_target = crate::dsp::goertzel_power(&out, 2000.0);
        let p_off1 = crate::dsp::goertzel_power(&out, 1700.0);
        let p_off2 = crate::dsp::goertzel_power(&out, 2300.0);
        assert!(
            p_target > 10.0 * p_off1.max(p_off2),
            "p2000={p_target} p1700={p_off1} p2300={p_off2}"
        );
    }

    /// F6 (#87). Tiny chunks: each call passes fewer samples than the
    /// 64-tap kernel needs, so the resampler should accumulate them in
    /// `tail` and emit nothing until `tail.len() >= FIR_TAPS`. Verifies
    /// the streaming-buffer carry-over correctness — the production
    /// decoder's per-call audio chunks can be small.
    #[test]
    fn tiny_chunks_emit_nothing_then_catch_up() {
        let mut r = Resampler::new(44_100).unwrap();
        let chunk = [0.5_f32, 0.5, 0.5];
        let mut emitted_before_threshold = 0;
        // 21 chunks of 3 samples = 63 < FIR_TAPS = 64. No output yet.
        for _ in 0..21 {
            let out = r.process(&chunk);
            emitted_before_threshold += out.len();
        }
        assert_eq!(
            emitted_before_threshold, 0,
            "expected no output before FIR_TAPS samples buffered, got {emitted_before_threshold}"
        );
        // One more chunk pushes us past FIR_TAPS — at least one sample emerges.
        let out_after = r.process(&chunk);
        assert!(
            !out_after.is_empty(),
            "expected at least one output sample after crossing the FIR_TAPS threshold"
        );
    }

    /// 分块不变性：同一段音频整段喂入与按任意大小分块喂入，输出必须
    /// **逐位相同**（实时 4096 块 vs 离线 32768 块 vs 整段喂入）。
    ///
    /// 回归背景：`process` 曾用浮点累加 `phase` 并按块回退基准，舍入路径随
    /// 分块方式变化，11050 Hz 下实测差异达 1.9e-3（一个相位量化档）——
    /// 同一段 PCM 实时解码与离线解码因此得到不同的 `has_sync` 轨道与图像。
    #[test]
    fn chunking_is_bit_exact_across_chunk_sizes() {
        for rate in [11_050_u32, 44_100, 48_000] {
            let audio = synth_tone_at(rate, 1500.0, 1.0);
            let mut a = Resampler::new(rate).unwrap();
            let single = a.process(&audio);
            for chunk in [1, 3, 64, 129, 2048, 32_768] {
                let mut b = Resampler::new(rate).unwrap();
                let mut chunked = Vec::new();
                for c in audio.chunks(chunk) {
                    chunked.extend_from_slice(&b.process(c));
                }
                assert_eq!(
                    chunked.len(),
                    single.len(),
                    "rate={rate} chunk={chunk}: 输出长度不一致"
                );
                let first_diff = single
                    .iter()
                    .zip(chunked.iter())
                    .position(|(x, y)| x.to_bits() != y.to_bits());
                assert!(
                    first_diff.is_none(),
                    "rate={rate} chunk={chunk}: 首个差异在 {first_diff:?}（应逐位一致）"
                );
            }
        }
    }

    /// F6 (#87). Empty input is a no-op — returns an empty Vec and
    /// leaves the resampler state untouched. Plus: an empty call
    /// sandwiched between two non-empty calls doesn't perturb the output
    /// (streaming idempotence).
    #[test]
    fn empty_input_returns_empty() {
        let mut r = Resampler::new(44_100).unwrap();
        assert!(r.process(&[]).is_empty());

        // Sandwich: process(non-empty) → process(empty) → process(non-empty)
        // should produce the same output as process(non-empty ++ non-empty).
        let mut a = Resampler::new(44_100).unwrap();
        let in_audio = synth_tone_at(44_100, 1500.0, 0.2);
        let mid = in_audio.len() / 2;

        let mut sandwiched = a.process(&in_audio[..mid]);
        let empty_call = a.process(&[]);
        assert!(empty_call.is_empty());
        sandwiched.extend_from_slice(&a.process(&in_audio[mid..]));

        let mut b = Resampler::new(44_100).unwrap();
        let combined = b.process(&in_audio);

        // Same length and per-sample values, bit-exact (the empty call must
        // not have moved the FIR's internal state).
        assert_eq!(
            sandwiched.len(),
            combined.len(),
            "sandwiched.len={} combined.len={}",
            sandwiched.len(),
            combined.len()
        );
        let first_diff = sandwiched
            .iter()
            .zip(combined.iter())
            .position(|(x, y)| x.to_bits() != y.to_bits());
        assert!(
            first_diff.is_none(),
            "空调用不应扰动状态，首个差异在 {first_diff:?}"
        );
    }
}
