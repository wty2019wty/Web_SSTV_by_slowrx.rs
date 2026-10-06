//! 临时诊断模块（分析完成后删除）：
//! 从录音里独立测出每个行同步脉冲的真实时刻，与解码器模型
//! （find_sync 给出的 rate/skip）对比，定位边缘伪影的成因。

use crate::modespec::{for_mode, SstvMode};
use crate::sync::{find_sync, FindSyncScratch, SyncTracker, SYNC_PROBE_STRIDE};

/// Goertzel 单频功率（带 Hann 窗）。
fn goertzel(win: &[f32], freq: f64, rate: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in win {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let real = s1 - s2 * w.cos();
    let imag = s2 * w.sin();
    real * real + imag * imag
}

/// 在样本窗口 [start, start+win) 上算 1200 Hz 与 1800 Hz 的功率差（Hann 加窗）。
fn band_diff(s44: &[f32], start: usize, win: usize, hann: &[f32]) -> f64 {
    if start + win > s44.len() {
        return 0.0;
    }
    let mut buf = vec![0.0f32; win];
    for i in 0..win {
        buf[i] = s44[start + i] * hann[i];
    }
    goertzel(&buf, 1200.0, 44100.0) - goertzel(&buf, 1800.0, 44100.0)
}

#[test]
#[allow(
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)]
fn diag_edges() {
    let path = std::env::var("DIAG_WAV")
        .unwrap_or_else(|_| "C:/Users/Administrator/Downloads/20261005_230806.wav".to_string());

    // ---- 读 WAV（44.1 kHz 单声道）----
    let mut reader = hound::WavReader::open(&path).expect("open wav");
    let ws = reader.spec();
    println!("wav: {} Hz, {} ch, {} bit", ws.sample_rate, ws.channels, ws.bits_per_sample);
    let s44: Vec<f32> = reader
        .samples::<i16>()
        .map_while(Result::ok)
        .map(|s| f32::from(s) / 32768.0)
        .collect();
    println!("samples44 = {} ({:.2} s)", s44.len(), s44.len() as f64 / 44100.0);

    // ---- 4:1 平均抽取到 11025 Hz（解码器工作率）----
    let s11: Vec<f32> = s44
        .chunks_exact(4)
        .map(|c| (c[0] + c[1] + c[2] + c[3]) * 0.25)
        .collect();

    // ---- 复刻解码器：sync 轨迹 + find_sync ----
    let spec = for_mode(SstvMode::Pd240);
    let mut tracker = SyncTracker::new(0.0);
    let mut has_sync: Vec<bool> = Vec::with_capacity(s11.len() / SYNC_PROBE_STRIDE + 1);
    let mut next_probe = 0usize;
    while next_probe + SYNC_PROBE_STRIDE * 2 <= s11.len() {
        let center = next_probe + SYNC_PROBE_STRIDE / 2;
        has_sync.push(tracker.has_sync_at(&s11, center));
        next_probe += SYNC_PROBE_STRIDE;
    }
    let mut scratch = FindSyncScratch::new();
    let work_rate = 11025.0f64;
    let r = find_sync(&has_sync, work_rate, spec, &mut scratch);
    println!(
        "find_sync: rate={:.4} Hz（标称 11025，偏差 {:+.2} ppm），skip={} 样本 = {:.4} s，slant={:?}",
        r.adjusted_rate_hz,
        (r.adjusted_rate_hz / work_rate - 1.0) * 1e6,
        r.skip_samples,
        r.skip_samples as f64 / work_rate,
        r.slant_deg
    );
    println!(
        "  模型行周期 = rate×line_seconds = {:.6} s；实测通道长 4×{:.3} ms，安全余量 = 半像素 = {:.3} ms",
        r.adjusted_rate_hz / work_rate,
        f64::from(spec.line_pixels) * spec.pixel_seconds * 1000.0,
        f64::from(spec.pixel_seconds) * 500.0
    );

    // ---- 独立测量：1200 Hz 行同步区间（粗，1 ms 网格）----
    let win_len = 176usize; // 4 ms
    let hop = 44usize; // 1 ms
    let hann: Vec<f32> = (0..win_len)
        .map(|i| {
            let x = std::f64::consts::PI * i as f64 / (win_len - 1) as f64;
            (0.5 - 0.5 * x.cos()) as f32
        })
        .collect();
    let mut flags: Vec<bool> = vec![false; s44.len() / hop + 1];
    for (k, f) in flags.iter_mut().enumerate() {
        let start = k * hop;
        if start + win_len > s44.len() {
            break;
        }
        *f = band_diff(&s44, start, win_len, &hann) > 0.0;
    }
    let mut spans: Vec<(f64, f64)> = Vec::new();
    let mut k = 0usize;
    while k < flags.len() {
        if flags[k] {
            let k0 = k;
            while k < flags.len() && flags[k] {
                k += 1;
            }
            let t0 = k0 as f64 * hop as f64 / 44100.0;
            let t1 = k as f64 * hop as f64 / 44100.0;
            if t1 - t0 >= 0.008 {
                spans.push((t0, t1));
            }
        }
        k += 1;
    }
    println!("\n检出 {} 个 1200 Hz 区间（含 VIS 段）", spans.len());
    for (i, sp) in spans.iter().take(8).enumerate() {
        println!(
            "  #{:2}: {:.4} .. {:.4} s（时长 {:.1} ms）",
            i,
            sp.0,
            sp.1,
            (sp.1 - sp.0) * 1000.0
        );
    }

    // ---- 下降沿细化（0.05 ms 网格 + 线性插值）----
    let fine_win = 132usize; // 3 ms
    let fine_hann: Vec<f32> = (0..fine_win)
        .map(|i| {
            let x = std::f64::consts::PI * i as f64 / (fine_win - 1) as f64;
            (0.5 - 0.5 * x.cos()) as f32
        })
        .collect();
    let mut falls: Vec<f64> = Vec::new();
    let mut rises: Vec<f64> = Vec::new();
    for sp in &spans {
        // 下降沿：在粗 t1 附近找 band_diff 过零（正→负）
        let c = (sp.1 * 44100.0) as usize;
        let mut best: Option<(f64, f64, f64)> = None; // (t_a, d_a, t_b)
        for j in 0..60 {
            let t = (c as f64 - 0.003 * 44100.0) + j as f64 * 0.00005 * 44100.0;
            let st = t as usize;
            let d = band_diff(&s44, st, fine_win, &fine_hann);
            if let Some((ta, da, _)) = best {
                if da > 0.0 && d <= 0.0 {
                    best = Some((ta, da, t as f64));
                    break;
                }
                best = Some((t as f64, d, 0.0));
            } else {
                best = Some((t as f64, d, 0.0));
            }
        }
        if let Some((ta, da, tb)) = best {
            if tb > 0.0 && da > 0.0 {
                let frac = da / (da - band_diff(&s44, tb as usize, fine_win, &fine_hann));
                falls.push((ta + frac * (tb - ta)) / 44100.0);
            } else {
                falls.push(sp.1);
            }
        }
        // 上升沿：在粗 t0 附近找过零（负→正）
        let c = (sp.0 * 44100.0) as usize;
        let mut prev: Option<(f64, f64)> = None;
        let mut got = sp.0;
        for j in 0..60 {
            let t = (c as f64 - 0.003 * 44100.0) + j as f64 * 0.00005 * 44100.0;
            let st = t as usize;
            let d = band_diff(&s44, st, fine_win, &fine_hann);
            if let Some((pt, pd)) = prev {
                if pd <= 0.0 && d > 0.0 {
                    let frac = -pd / (d - pd);
                    got = (pt + frac * (t - pt)) / 44100.0;
                    break;
                }
            }
            prev = Some((t as f64, d));
        }
        rises.push(got);
    }

    // 只留图像段（首个 ≥ 1.5 s 的区间起为行 0），但保留前面的打印
    let first_img = spans
        .iter()
        .position(|s| s.0 > 1.2)
        .unwrap_or(0);
    let falls: Vec<f64> = falls[first_img..].to_vec();
    let rises: Vec<f64> = rises[first_img..].to_vec();
    println!(
        "\n图像段：{} 个行同步，首行上升沿 {:.4} s，下降沿 {:.4} s（sync 时长 {:.2} ms）",
        falls.len(),
        rises[0],
        falls[0],
        (falls[0] - rises[0]) * 1000.0
    );

    // 行周期
    let mut gaps: Vec<f64> = Vec::new();
    for i in 1..falls.len() {
        gaps.push(rises[i] - rises[i - 1]);
    }
    let mean_gap = gaps.iter().sum::<f64>() / gaps.len() as f64;
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "行周期（上升沿间隔）：均值 {:.6} s，中位 {:.6} s，min {:.6}，max {:.6}",
        mean_gap,
        sorted[sorted.len() / 2],
        sorted[0],
        sorted[sorted.len() - 1]
    );

    // 线性拟合 rises[n] = a + b*n
    let n_f = rises.len() as f64;
    let sx: f64 = (0..rises.len()).map(|i| i as f64).sum();
    let sxx: f64 = (0..rises.len()).map(|i| (i as f64) * (i as f64)).sum();
    let sy: f64 = rises.iter().sum();
    let sxy: f64 = rises.iter().enumerate().map(|(i, y)| i as f64 * y).sum();
    let det = n_f * sxx - sx * sx;
    let b = (n_f * sxy - sx * sy) / det;
    let _a = (sy - b * sx) / n_f;
    let px_s = f64::from(spec.pixel_seconds);
    println!(
        "拟合行周期 {:.6} s（模型 1.000000）→ 每行 {:+.4} ms = {:+.2} 像素/行，{} 行累积 {:+.1} 像素",
        b,
        (b - 1.0) * 1000.0,
        (b - 1.0) / px_s,
        rises.len(),
        (b - 1.0) * rises.len() as f64 / px_s
    );

    // 每行相位误差：实际行起点 vs 模型（skip + n，mod 1 s）
    let skip_s = r.skip_samples as f64 / work_rate;
    let mut phase: Vec<f64> = Vec::new();
    for i in 0..rises.len() {
        let model = skip_s + i as f64;
        let mut d = rises[i] - model;
        d -= (d + 0.5).floor();
        phase.push(d);
    }
    println!(
        "\n每行相位误差（实际行起点 − 模型时刻，mod 1 s，单位 ms；正=采样滞后→右缘溢出，负=超前→左缘溢出）："
    );
    for i in (0..rises.len()).step_by(16) {
        let bar_len = ((phase[i] / 0.006) * 20.0).round() as i32;
        let bar: String = if bar_len >= 0 {
            " ".repeat(20) + &"=".repeat(bar_len as usize)
        } else {
            " ".repeat((20 + bar_len).max(0) as usize) + &"=".repeat((-bar_len) as usize) + " "
        };
        println!(
            "  y={:3} (行 {:3}) {:+8.3} ms {:+6.1} px |{bar}|",
            i * 2,
            i,
            phase[i] * 1000.0,
            phase[i] / px_s
        );
    }
    // 过零点
    let mut cross: Option<usize> = None;
    for i in 1..phase.len() {
        if phase[i - 1] > 0.0 && phase[i] <= 0.0 {
            cross = Some(i);
            break;
        }
    }
    if let Some(i) = cross {
        println!(
            "  → 相位过零在行 {}（y≈{}），此前右缘溢出、此后左缘溢出",
            i,
            i * 2
        );
    }

    // ---- 图像边缘伪影的奇偶行分布 ----
    let img_path = std::env::var("DIAG_PNG")
        .unwrap_or_else(|_| "G:/Web_SSTV_by_slowrx.rs/out/repro/img-001-pd240.png".to_string());
    let img = image::open(&img_path).expect("open png").to_rgb8();
    let (w, h) = img.dimensions();
    println!("\n图像边缘伪影统计（{}x{}）：", w, h);
    // 右缘 20 列 / 左缘 20 列内，逐行统计“条纹”像素（与左邻像素 RGB 距离 > 45）
    let mut right_rows: Vec<u32> = vec![0; h as usize];
    let mut left_rows: Vec<u32> = vec![0; h as usize];
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(x, y).0;
            let q = img.get_pixel(x.saturating_sub(1), y).0;
            let dist = (i32::from(p[0]) - i32::from(q[0])).abs()
                + (i32::from(p[1]) - i32::from(q[1])).abs()
                + (i32::from(p[2]) - i32::from(q[2])).abs();
            if dist > 90 {
                if x >= w - 20 {
                    right_rows[y as usize] += 1;
                }
                if x < 20 {
                    left_rows[y as usize] += 1;
                }
            }
        }
    }
    let (mut ro, mut re, mut lo, mut le) = (0u32, 0u32, 0u32, 0u32);
    for y in 0..h as usize {
        if y % 2 == 1 {
            ro += right_rows[y];
            lo += left_rows[y];
        } else {
            re += right_rows[y];
            le += left_rows[y];
        }
    }
    println!(
        "  右缘条纹像素：奇数行(Y_even) {}，偶数行(Y_odd) {} —— 奇偶比 {:.2}",
        ro,
        re,
        ro as f64 / re.max(1) as f64
    );
    println!(
        "  左缘条纹像素：奇数行(Y_even) {}，偶数行(Y_odd) {} —— 奇偶比 {:.2}",
        lo,
        le,
        lo as f64 / le.max(1) as f64
    );
    println!("\n  右缘/左缘条纹随行号分布（每 20 行求和）：");
    for y0 in (0..h as usize).step_by(20) {
        let y1 = (y0 + 20).min(h as usize);
        let rr: u32 = right_rows[y0..y1].iter().sum();
        let ll: u32 = left_rows[y0..y1].iter().sum();
        println!(
            "    y {:3}-{:<3} 右 {:4} 左 {:4} |{}{}",
            y0,
            y1 - 1,
            rr,
            ll,
            "#".repeat((rr / 8) as usize),
            ".".repeat((ll / 8) as usize)
        );
    }
}
