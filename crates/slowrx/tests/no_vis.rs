//! Negative real-radio regression — [`slowrx::SstvDecoder`] must not
//! false-positive on non-SSTV audio.
//!
//! **Origin.** An ISS Zarya capture on 2026-05-04 22:38:27 UTC turned
//! out to carry no SSTV (Zarya transmits SSTV only during ARISS-
//! scheduled events). 0 VIS codes detected on the 25-minute recording;
//! a separate spectrum check confirmed the audio sat in the 247–563 Hz
//! band with no energy in SSTV's 1500–2300 Hz pixel band. We could not
//! commit the recording itself (`/tests/fixtures` is gitignored per the
//! project's "no third-party-licensed fixtures in the repo" convention,
//! and the redistribution-license story for community-shared captures
//! is unclear), so the regression coverage is reproduced with two
//! synthetic non-SSTV audio buffers: white noise at the Zarya
//! recording's measured RMS level (~0.3), and pure silence.
//!
//! Both must produce zero `SstvEvent::ImageComplete { partial: false }`
//! events without panicking. This is the no-signal counterpart to the
//! synthetic round-trip suite in `tests/roundtrip.rs` — both must hold
//! for any release.

#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use slowrx::{SstvDecoder, SstvEvent};

/// Sample rate matching what a typical SDR / file capture produces;
/// also exercises the resampler since `WORKING_SAMPLE_RATE_HZ` = `11_025`.
const SAMPLE_RATE_HZ: u32 = 48_000;
const DURATION_SEC: u32 = 10;

/// Deterministic linear-congruential generator. Numerical Recipes'
/// "Quick and Dirty" constants — sufficient for white-noise regression
/// purposes (no cryptographic strength needed). A fixed seed makes the
/// test reproducible across runs / hosts.
struct Lcg(u32);

impl Lcg {
    fn new(seed: u32) -> Self {
        Self(seed)
    }

    fn next_unit(&mut self) -> f32 {
        // state ← state · 1664525 + 1013904223 (mod 2³²)
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // Map u32 → [-0.5, 0.5).
        (self.0 as f32 / u32::MAX as f32) - 0.5
    }
}

fn count_complete_images(events: &[SstvEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, SstvEvent::ImageComplete { partial: false, .. }))
        .count()
}

#[test]
fn decoder_no_vis_on_white_noise() {
    let n = (SAMPLE_RATE_HZ * DURATION_SEC) as usize;
    let mut rng = Lcg::new(0x5EED_5EED);
    // ~0.3 RMS — matches the measured level of the Zarya 2026-05-04
    // recording, so the decoder operates on realistic signal-strength
    // input rather than near-zero amplitudes.
    //
    // `next_unit()` is uniform on `[-0.5, 0.5]`, whose RMS is
    // `1/sqrt(12) ≈ 0.2887`. Scaling by `target_rms * sqrt(12)` makes
    // the resulting noise hit the target RMS exactly (modulo
    // sample-count statistical jitter).
    let noise_scale = 0.3_f32 * 12.0_f32.sqrt();
    let audio: Vec<f32> = (0..n).map(|_| rng.next_unit() * noise_scale).collect();

    let mut decoder = SstvDecoder::new(SAMPLE_RATE_HZ).expect("decoder construct");
    let events = decoder.process(&audio);

    let n_complete = count_complete_images(&events);
    assert_eq!(
        n_complete, 0,
        "decoder false-positive: emitted {n_complete} non-partial \
         ImageComplete event(s) on 10 s of white noise",
    );
}

#[test]
fn decoder_no_vis_on_silence() {
    let n = (SAMPLE_RATE_HZ * DURATION_SEC) as usize;
    let audio = vec![0.0_f32; n];

    let mut decoder = SstvDecoder::new(SAMPLE_RATE_HZ).expect("decoder construct");
    let events = decoder.process(&audio);

    let n_complete = count_complete_images(&events);
    assert_eq!(
        n_complete, 0,
        "decoder false-positive: emitted {n_complete} non-partial \
         ImageComplete event(s) on 10 s of pure silence",
    );
}

/// 回归守卫：**检测到合法 VIS 之后**播放静音（整幅图像时长内探测不到任何
/// 1200 Hz 行同步）也不得 panic。
///
/// 与上面两条的区别：那两条根本不会进入 `State::Decoding`；本测试先用
/// `synth_vis` 骗过检测器进入解码状态，再喂静音，从而真正走到
/// `sync::track_line_starts`。修复前该函数的“候选同步段”为空，
/// `best_chain` 退化成 `Some((0, 1))`，随后 `candidates[0]` 越界 panic。
#[cfg(feature = "test-support")]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[test]
fn vis_then_silence_does_not_panic() {
    use slowrx::WORKING_SAMPLE_RATE_HZ;

    // Robot24：240 行 × 0.150 s = 36 s 标称图像时长。
    let target = (240.0 * 0.150 * f64::from(WORKING_SAMPLE_RATE_HZ)) as usize + 8192;
    let mut audio = slowrx::__test_support::vis::synth_vis(0x04, 0.0);
    audio.extend(std::iter::repeat_n(0.0_f32, target));

    let mut decoder = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder construct");
    // 主契约：不得 panic。VIS 后无同步时旧行为是解出一张黑图（非 partial），
    // 这里只做上界断言，不锁定具体是否产图。
    let events = decoder.process(&audio);
    assert!(
        count_complete_images(&events) <= 1,
        "VIS 后静音最多只应产出一张图",
    );
}
