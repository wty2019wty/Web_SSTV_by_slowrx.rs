//! Synthetic RGB-sequential encoder for round-trip testing —
//! handles Scottie (1/2/DX), Martin (1/2) and Wraase SC2-180.
//!
//! Test-only — gated behind `cfg(any(test, feature = "test-support"))`.
//!
//! **Per-line tone emission order branches on
//! [`crate::modespec::SyncPosition`], channel colour order on
//! [`crate::modespec::RgbOrder`]:**
//!
//! ```text
//! Scottie (sync_position::Scottie, rgb_order::Gbr):
//!   [septr 1500 Hz][G pixels 1500-2300 Hz][septr 1500 Hz]
//!   [B pixels 1500-2300 Hz][SYNC 1200 Hz][porch 1500 Hz]
//!   [R pixels 1500-2300 Hz]
//!
//! Martin (sync_position::LineStart, rgb_order::Gbr):
//!   [SYNC 1200 Hz][porch 1500 Hz][G pixels 1500-2300 Hz]
//!   [septr 1500 Hz][B pixels 1500-2300 Hz][septr 1500 Hz]
//!   [R pixels 1500-2300 Hz]
//!
//! Wraase SC2-180 (sync_position::LineStart, rgb_order::Rgb, septr = 0):
//!   [SYNC 1200 Hz][porch 1500 Hz][R pixels 1500-2300 Hz]
//!   [G pixels 1500-2300 Hz][B pixels 1500-2300 Hz]
//! ```
//!
//! Total per line = `LineTime` exactly (defensive pad fills the
//! boundary if float arithmetic rounds short).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::modespec::SstvMode;
use crate::resample::WORKING_SAMPLE_RATE_HZ;
use crate::test_tone::{lum_to_freq, ToneWriter, PORCH_HZ, SEPTR_HZ, SYNC_HZ};

/// Encode an RGB image as continuous-phase FM audio for Scottie
/// (S1/S2/DX), Martin (M1/M2) or Wraase SC2-180. `rgb` is row-major,
/// `line_pixels × image_lines` `[R, G, B]` triples (320×256 for all
/// supported modes). Returns f32 PCM at [`WORKING_SAMPLE_RATE_HZ`]
/// (`11_025` Hz).
///
/// The per-line tone emission order branches on `spec.sync_position`,
/// the channel colour order on `spec.rgb_order` (see the module doc).
///
/// Panics if `mode` is not one of the six supported variants or if
/// `rgb.len() != line_pixels * image_lines`.
#[must_use]
#[allow(dead_code, clippy::too_many_lines)]
pub(crate) fn encode_scottie(mode: SstvMode, rgb: &[[u8; 3]]) -> Vec<f32> {
    assert!(matches!(
        mode,
        SstvMode::Scottie1
            | SstvMode::Scottie2
            | SstvMode::ScottieDx
            | SstvMode::Martin1
            | SstvMode::Martin2
            | SstvMode::WraaseSc2_180
    ));
    let spec = crate::modespec::for_mode(mode);
    // 发送顺序的 RGB 下标（Gbr → [1,2,0] 即 G、B、R；Rgb → [0,1,2] 即
    // R、G、B）。与解码端共用，保证编解码对称。
    let wire = spec.rgb_order.wire_rgb_indices();
    let w = spec.line_pixels;
    let h = spec.image_lines;
    assert_eq!(rgb.len() as u32, w * h);

    let sr = f64::from(WORKING_SAMPLE_RATE_HZ);
    let mut tone = ToneWriter::new();

    let mut t = 0.0_f64;
    let advance = |t: &mut f64, secs: f64| -> usize {
        *t += secs;
        (*t * sr).round() as usize
    };

    for y in 0..h {
        match spec.sync_position {
            crate::modespec::SyncPosition::Scottie => {
                // Septr 1.
                tone.fill_to(SEPTR_HZ, advance(&mut t, spec.septr_seconds));

                // 通道 0（Gbr→G、Rgb→R）。
                for x in 0..w {
                    let v = rgb[(y * w + x) as usize][wire[0]];
                    tone.fill_to(lum_to_freq(v), advance(&mut t, spec.pixel_seconds));
                }

                // Septr 2.
                tone.fill_to(SEPTR_HZ, advance(&mut t, spec.septr_seconds));

                // 通道 1（Gbr→B、Rgb→G）。
                for x in 0..w {
                    let v = rgb[(y * w + x) as usize][wire[1]];
                    tone.fill_to(lum_to_freq(v), advance(&mut t, spec.pixel_seconds));
                }

                // Sync (mid-line, between B and R).
                tone.fill_to(SYNC_HZ, advance(&mut t, spec.sync_seconds));

                // Porch.
                tone.fill_to(PORCH_HZ, advance(&mut t, spec.porch_seconds));

                // 通道 2（Gbr→R、Rgb→B）。
                for x in 0..w {
                    let v = rgb[(y * w + x) as usize][wire[2]];
                    tone.fill_to(lum_to_freq(v), advance(&mut t, spec.pixel_seconds));
                }
            }
            crate::modespec::SyncPosition::LineStart => {
                // Martin/Wraase 布局：同步脉冲在行首，其后 porch，再依次
                // 发送通道 0/1/2；Wraase 的 septr 为 0（分隔脉冲填充
                // 0 秒即无操作）。

                // Sync.
                tone.fill_to(SYNC_HZ, advance(&mut t, spec.sync_seconds));

                // Porch.
                tone.fill_to(PORCH_HZ, advance(&mut t, spec.porch_seconds));

                // 通道 0（Gbr→G、Rgb→R）。
                for x in 0..w {
                    let v = rgb[(y * w + x) as usize][wire[0]];
                    tone.fill_to(lum_to_freq(v), advance(&mut t, spec.pixel_seconds));
                }

                // Septr 1.
                tone.fill_to(SEPTR_HZ, advance(&mut t, spec.septr_seconds));

                // 通道 1（Gbr→B、Rgb→G）。
                for x in 0..w {
                    let v = rgb[(y * w + x) as usize][wire[1]];
                    tone.fill_to(lum_to_freq(v), advance(&mut t, spec.pixel_seconds));
                }

                // Septr 2.
                tone.fill_to(SEPTR_HZ, advance(&mut t, spec.septr_seconds));

                // 通道 2（Gbr→R、Rgb→B）。
                for x in 0..w {
                    let v = rgb[(y * w + x) as usize][wire[2]];
                    tone.fill_to(lum_to_freq(v), advance(&mut t, spec.pixel_seconds));
                }
            }
        }

        // Defensive pad to the line_seconds boundary (existing logic
        // — shape unchanged).
        let line_end_target = f64::from(y + 1) * spec.line_seconds;
        let pad_secs = line_end_target - t;
        if pad_secs > 0.0 {
            tone.fill_to(PORCH_HZ, advance(&mut t, pad_secs));
        }
    }

    tone.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: `lum_to_freq` endpoint coverage lives in
    // `crate::test_tone::tests::lum_to_freq_endpoints_match_black_and_white`
    // (its canonical home post-#86). Testing it from a consumer is
    // misleading about ownership.

    #[test]
    fn scottie1_encode_total_length() {
        let rgb = vec![[128u8; 3]; 320 * 256];
        let audio = encode_scottie(SstvMode::Scottie1, &rgb);
        let spec = crate::modespec::for_mode(SstvMode::Scottie1);
        let expected_len = (spec.line_seconds
            * f64::from(spec.image_lines)
            * f64::from(WORKING_SAMPLE_RATE_HZ)) as usize;
        // Allow 1-sample rounding slack at end-of-image.
        assert!(audio.len() >= expected_len);
        assert!(audio.len() <= expected_len + 1);
    }
}
