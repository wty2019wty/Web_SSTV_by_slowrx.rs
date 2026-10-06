//! 时基偏差下的端到端解码回归测试。
//!
//! 模拟发射端行周期与标称值的偏差——真实场景里 MMSSTV 的 PD-240 在
//! 44.1 kHz 下每行 44102 个样本而非标称的 44100（约 45 ppm），这里按
//! **每行 +1 样本**施加（更严酷一倍）。偏差未被逐行相位跟踪时，每个
//! 通道首尾的像素会读到行同步脉冲或相邻通道，在图像一侧留下高饱和的
//! 彩色竖条纹。
//!
//! 判据是**边缘列的平均色差**：条纹集中在左右边缘列，其色差远高于图像
//! 内部；跟踪生效后边缘列应回到与内部列同量级。整体平均色差作为兜底。
//!
//! 覆盖全部 11 个模式（四种通道布局）：PD 系列（行对 + YCrCb）、Robot
//! 系列（逐行 + YUV）、Scottie 系列（行中同步 + RGB）、Martin 系列
//! （行首同步 + RGB）。

#![cfg(feature = "test-support")]
#![allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use slowrx::{SstvDecoder, SstvEvent, SstvMode, WORKING_SAMPLE_RATE_HZ};

/// 边缘列宽度（左右各多少列参与边缘色差统计）。
const EDGE_COLS: u32 = 16;

/// 把音频按"每行多 1 个样本"拉伸，模拟发射端行周期偏长。
fn drift_audio(audio: &[f32], line_samples: f64) -> Vec<f32> {
    let k = 1.0 + 1.0 / line_samples;
    let n_out = (audio.len() as f64 * k).ceil() as usize;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let pos = i as f64 / k;
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let s0 = audio.get(i0).copied().unwrap_or(0.0);
        let s1 = audio.get(i0 + 1).copied().unwrap_or(0.0);
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

/// 合成测试图：水平亮度渐变 + **行间恒定**的色度。
///
/// 色度刻意不随行变化：整体行号偏移会让解码图像平移一两行，这属于允许
/// 行为（只影响行号、不影响画质），不应被判成质量下降；而通道/行内相位
/// 错位（读到同步脉冲或相邻通道）会体现在亮度与色度的错值上，仍能被
/// 下面的边缘色差判据抓住。
#[allow(clippy::cast_possible_truncation)]
fn test_image(mode: SstvMode) -> (u32, u32, Vec<[u8; 3]>) {
    let spec = slowrx::for_mode(mode);
    let w = spec.line_pixels;
    let h = spec.image_lines;
    let mut ycrcb = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let lum = ((f64::from(x)) / (f64::from(w)) * 255.0) as u8;
            ycrcb.push([lum, 168, 112]);
            let _ = y;
        }
    }
    (w, h, ycrcb)
}

/// 编码 → 施加时基偏差 → 解码 → 断言边缘无条纹。
fn run_drifted(mode: SstvMode) {
    let spec = slowrx::for_mode(mode);
    let (w, h, ycrcb) = test_image(mode);

    // 编码：Scottie/Martin 走 RGB 编码器，其余走 YCrCb 编码器。
    let mut audio = slowrx::__test_support::vis::synth_vis(spec.vis_code, 0.0);
    let layout = spec.channel_layout;
    let src_rgb: Vec<[u8; 3]> = if layout == slowrx::modespec::ChannelLayout::RgbSequential {
        let rgb: Vec<[u8; 3]> = ycrcb
            .iter()
            .map(|p| slowrx::__test_support::mode_pd::ycbcr_to_rgb(p[0], p[1], p[2]))
            .collect();
        audio.extend(slowrx::__test_support::mode_scottie::encode_scottie(
            mode, &rgb,
        ));
        rgb
    } else if layout == slowrx::modespec::ChannelLayout::RobotYuv {
        audio.extend(slowrx::__test_support::mode_robot::encode_robot(
            mode, &ycrcb,
        ));
        ycrcb
            .iter()
            .map(|p| slowrx::__test_support::mode_pd::ycbcr_to_rgb(p[0], p[1], p[2]))
            .collect()
    } else {
        // PdYcbcr 及未来的行首同步 YCrCb 布局。
        audio.extend(slowrx::__test_support::mode_pd::encode_pd(mode, &ycrcb));
        ycrcb
            .iter()
            .map(|p| slowrx::__test_support::mode_pd::ycbcr_to_rgb(p[0], p[1], p[2]))
            .collect()
    };
    // 吸收重采样群延迟的尾部余量（R72 每行有 ~2.6 ms 空档，需要更多）。
    audio.extend(std::iter::repeat_n(0.0_f32, 8192));

    // 每行 +1 样本的行周期偏差。
    let line_samples = spec.line_seconds * f64::from(WORKING_SAMPLE_RATE_HZ);
    let drifted = drift_audio(&audio, line_samples);

    let mut d = SstvDecoder::new(WORKING_SAMPLE_RATE_HZ).expect("decoder");
    let events = d.process(&drifted);
    let img = events
        .iter()
        .find_map(|e| match e {
            SstvEvent::ImageComplete {
                image,
                partial: false,
            } => Some(image.clone()),
            _ => None,
        })
        .expect("ImageComplete event");
    assert_eq!(img.mode, mode);
    assert_eq!(img.width, w);
    assert_eq!(img.height, h);

    // 边缘列 vs 内部列的平均色差。
    let mut edge_sum = 0_u64;
    let mut edge_n = 0_u64;
    let mut inner_sum = 0_u64;
    let mut inner_n = 0_u64;
    let mut all_sum = 0_u64;
    for y in 0..h {
        for x in 0..w {
            let src = src_rgb[(y * w + x) as usize];
            let dec = img.pixels[(y * w + x) as usize];
            let mut diff = 0_u32;
            for ch in 0..3 {
                diff += u32::from((i32::from(src[ch]) - i32::from(dec[ch])).unsigned_abs() as u8);
            }
            all_sum += u64::from(diff);
            if x < EDGE_COLS || x >= w - EDGE_COLS {
                edge_sum += u64::from(diff);
                edge_n += 1;
            } else {
                inner_sum += u64::from(diff);
                inner_n += 1;
            }
        }
    }
    let edge_mean = edge_sum as f64 / edge_n as f64;
    let inner_mean = inner_sum as f64 / inner_n as f64;
    let all_mean = all_sum as f64 / f64::from(3 * w * h);

    // 判据：边缘列的色差不应显著高于内部。未跟踪时通道首尾像素读到同步
    // 脉冲/相邻通道，每行 +1 样本的偏差累积到 248 行≈59 像素宽的条纹，
    // edge−inner 在 70 量级；跟踪后残余约 1~2 像素的轻微越界，edge−inner
    // 在 15 以内（实测最差 Scottie2 ≈ 12.6）。
    assert!(
        edge_mean < inner_mean + 15.0,
        "{mode:?}: 边缘列色差 {edge_mean:.2} 显著高于内部 {inner_mean:.2}（每行 +1 样本的时基偏差未被逐行跟踪？）"
    );
    assert!(
        all_mean < 10.0,
        "{mode:?}: 整体平均色差 {all_mean:.2} 过高（edge={edge_mean:.2} inner={inner_mean:.2}）"
    );
}

#[test]
fn pd120_drifted_timing() {
    run_drifted(SstvMode::Pd120);
}
#[test]
fn pd180_drifted_timing() {
    run_drifted(SstvMode::Pd180);
}
#[test]
fn pd240_drifted_timing() {
    run_drifted(SstvMode::Pd240);
}
#[test]
fn robot24_drifted_timing() {
    run_drifted(SstvMode::Robot24);
}
#[test]
fn robot36_drifted_timing() {
    run_drifted(SstvMode::Robot36);
}
#[test]
fn robot72_drifted_timing() {
    run_drifted(SstvMode::Robot72);
}
#[test]
fn scottie1_drifted_timing() {
    run_drifted(SstvMode::Scottie1);
}
#[test]
fn scottie2_drifted_timing() {
    run_drifted(SstvMode::Scottie2);
}
#[test]
fn scottie_dx_drifted_timing() {
    run_drifted(SstvMode::ScottieDx);
}
#[test]
fn martin1_drifted_timing() {
    run_drifted(SstvMode::Martin1);
}
#[test]
fn martin2_drifted_timing() {
    run_drifted(SstvMode::Martin2);
}
