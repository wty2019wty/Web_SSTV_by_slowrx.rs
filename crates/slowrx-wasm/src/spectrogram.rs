//! 频谱图（STFT）计算。
//!
//! 在 Worker 内用与解码器同源的 `rustfft` 计算，避免引入第二套 FFT 实现或
//! 额外 JS 依赖（方案 6.3）。输出为 8 位强度矩阵（列优先），供主线程按可视
//! 时间窗口渲染，传输与内存开销都很小。

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// 默认 FFT 长度。11025 Hz 下频率分辨率约 10.8 Hz。
pub const DEFAULT_FFT_SIZE: usize = 1024;
/// 默认帧移（75% 重叠）。
pub const DEFAULT_HOP: usize = DEFAULT_FFT_SIZE / 4;
/// 默认显示上限频率（Hz）。SSTV 音频带约 1000–2300 Hz，看到 4 kHz 足够。
pub const DEFAULT_MAX_HZ: f64 = 4000.0;
/// 归一化动态范围（dB）。
const DYNAMIC_RANGE_DB: f32 = 90.0;
/// 伽马校正：<1 提亮暗部，效果更接近 Audition 观感。
const GAMMA: f32 = 0.6;

/// 一张频谱图的强度矩阵。
#[derive(Debug, Clone)]
pub struct Spectrogram {
    columns: u32,
    bins: u32,
    hop: u32,
    sample_rate: u32,
    max_hz: f64,
    /// 列优先：`data[column * bins + bin]`，取值 0–255。
    data: Vec<u8>,
}

impl Spectrogram {
    /// 计算 STFT。`samples` 不足一帧、或参数非法时返回 `None`。
    ///
    /// - `bins` 覆盖 `0..=max_hz`（上限受 Nyquist 限制）；
    /// - `columns = (len - fft_size) / hop + 1`；
    /// - 强度按整图的 dB 峰值归一化并做伽马校正。
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn compute(
        samples: &[f32],
        sample_rate: u32,
        fft_size: usize,
        hop: usize,
        max_hz: f64,
    ) -> Option<Self> {
        if sample_rate == 0 || fft_size < 2 || hop == 0 || samples.len() < fft_size {
            return None;
        }
        let nyquist_bins = fft_size / 2 + 1;
        let bin_hz = f64::from(sample_rate) / fft_size as f64;
        let bins = ((max_hz / bin_hz).floor() as usize + 1).clamp(1, nyquist_bins);
        let columns = (samples.len() - fft_size) / hop + 1;

        // Hann 窗。
        let window: Vec<f32> = (0..fft_size)
            .map(|i| {
                let phase = std::f64::consts::TAU * i as f64 / (fft_size as f64 - 1.0);
                (0.5 - 0.5 * phase.cos()) as f32
            })
            .collect();

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        let mut buffer = vec![Complex::new(0.0_f32, 0.0_f32); fft_size];

        let total = columns * bins;
        let mut mags = vec![0.0_f32; total];

        for column in 0..columns {
            let offset = column * hop;
            for i in 0..fft_size {
                buffer[i] = Complex::new(samples[offset + i] * window[i], 0.0);
            }
            fft.process(&mut buffer);
            let base = column * bins;
            for bin in 0..bins {
                let c = buffer[bin];
                let magnitude = (c.re * c.re + c.im * c.im).sqrt();
                mags[base + bin] = 20.0 * (magnitude + 1e-9).log10();
            }
        }

        // 绝对参考（dBFS）：满量程正弦经 Hann 窗后峰值约为 fft_size/4。
        // 这样静音落到底部、满量程信号到顶，且不同图之间可比。
        let reference_db = 20.0 * (fft_size as f32 / 4.0).log10();
        let data = mags
            .iter()
            .map(|db| {
                let t = ((db - reference_db) / DYNAMIC_RANGE_DB + 1.0).clamp(0.0, 1.0);
                (t.powf(GAMMA) * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();

        Some(Self {
            columns: columns as u32,
            bins: bins as u32,
            hop: hop as u32,
            sample_rate,
            max_hz,
            data,
        })
    }

    /// 时间列数。
    #[must_use]
    pub fn columns(&self) -> u32 {
        self.columns
    }

    /// 每列的频率 bin 数（`0..=max_hz`）。
    #[must_use]
    pub fn bins(&self) -> u32 {
        self.bins
    }

    /// 帧移（输入采样点）。
    #[must_use]
    pub fn hop(&self) -> u32 {
        self.hop
    }

    /// 输入采样率。
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 显示上限频率。
    #[must_use]
    pub fn max_hz(&self) -> f64 {
        self.max_hz
    }

    /// 每列代表的时间跨度（秒）。
    #[must_use]
    pub fn seconds_per_column(&self) -> f64 {
        f64::from(self.hop) / f64::from(self.sample_rate)
    }

    /// 列优先的强度矩阵，长度 `columns * bins`。
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, seconds: f64) -> Vec<f32> {
        let n = (f64::from(rate) * seconds) as usize;
        (0..n)
            .map(|i| (std::f64::consts::TAU * freq * i as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    /// 单频正弦的能量应集中在对应 bin 上。
    #[test]
    fn peak_bin_matches_frequency() {
        let rate = 11025;
        let freq = 1500.0;
        let samples = sine(freq, rate, 1.0);
        let spec = Spectrogram::compute(
            samples.as_slice(),
            rate,
            DEFAULT_FFT_SIZE,
            DEFAULT_HOP,
            DEFAULT_MAX_HZ,
        )
        .expect("spectrogram");
        assert!(spec.columns() > 1);
        let bin_hz = f64::from(rate) / DEFAULT_FFT_SIZE as f64;

        // 取中间某一列，找最大 bin。
        let column = spec.columns() as usize / 2;
        let base = column * spec.bins() as usize;
        let (peak_bin, _) = spec.data()[base..base + spec.bins() as usize]
            .iter()
            .enumerate()
            .max_by_key(|(_, v)| **v)
            .expect("non-empty");
        let peak_hz = peak_bin as f64 * bin_hz;
        assert!(
            (peak_hz - freq).abs() <= bin_hz * 1.5,
            "峰值 {peak_hz} Hz，期望约 {freq} Hz"
        );
    }

    /// 静音应全部落在量程底部。
    #[test]
    fn silence_is_floor() {
        let rate = 11025;
        let samples = vec![0.0_f32; rate as usize];
        let spec = Spectrogram::compute(
            samples.as_slice(),
            rate,
            DEFAULT_FFT_SIZE,
            DEFAULT_HOP,
            DEFAULT_MAX_HZ,
        )
        .expect("spectrogram");
        let max = spec.data().iter().copied().max().unwrap_or(0);
        assert!(max < 40, "静音最大强度 {max}，应接近 0");
    }

    /// 输入过短时返回 None。
    #[test]
    fn too_short_returns_none() {
        let rate = 11025;
        let samples = vec![0.0_f32; 100];
        assert!(Spectrogram::compute(
            samples.as_slice(),
            rate,
            DEFAULT_FFT_SIZE,
            DEFAULT_HOP,
            DEFAULT_MAX_HZ
        )
        .is_none());
    }

    /// 列数、bin 数、时长换算关系正确。
    #[test]
    fn shape_and_timing() {
        let rate = 11025;
        let samples = vec![0.1_f32; rate as usize]; // 1 秒
        let spec = Spectrogram::compute(
            samples.as_slice(),
            rate,
            DEFAULT_FFT_SIZE,
            DEFAULT_HOP,
            DEFAULT_MAX_HZ,
        )
        .expect("spectrogram");
        let expected_columns = ((rate as usize - DEFAULT_FFT_SIZE) / DEFAULT_HOP + 1) as u32;
        assert_eq!(spec.columns(), expected_columns);
        assert_eq!(spec.data().len(), (spec.columns() * spec.bins()) as usize);
        assert!((spec.seconds_per_column() - DEFAULT_HOP as f64 / f64::from(rate)).abs() < 1e-9);
    }
}
