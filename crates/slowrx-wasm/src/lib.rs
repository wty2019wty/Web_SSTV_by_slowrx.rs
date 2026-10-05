//! `slowrx-wasm`：把 `slowrx` 的 SSTV 解码核心封装成浏览器可用的 WebAssembly 模块。
//!
//! 分层设计：
//! - [`core`]：纯 Rust 的解码封装，可在原生环境单元测试；
//! - 本模块：`#[wasm_bindgen]` 绑定层，把 [`core::CoreEvent`] 转成 JS 对象，
//!   像素以 `Uint8Array` 传递，避免 JSON 数字数组的巨大开销。
//!
//! 典型用法（Worker 内）：
//! ```js
//! const d = new WasmDecoder(sampleRate);
//! d.setForcedMode("pd120", 0.0, undefined); // 可选：选区不含 VIS 时强制模式
//! const events = d.pushAudio(chunk);        // Float32Array
//! // events 中 type === "image" 的项含 rgba / width / height
//! ```

mod core;
mod spectrogram;

use core::{CoreDecoder, CoreEvent};
use wasm_bindgen::prelude::*;

/// 安全地给 JS 对象设置属性（失败静默，避免污染返回值）。
fn set(object: &js_sys::Object, key: &str, value: JsValue) {
    let _ = js_sys::Reflect::set(object, &JsValue::from_str(key), &value);
}

/// 把核心事件转换成 JS 对象。
fn event_to_js(event: CoreEvent) -> JsValue {
    let object = js_sys::Object::new();
    match event {
        CoreEvent::VisDetected {
            mode,
            sample_offset,
            hedr_shift_hz,
        } => {
            set(&object, "type", JsValue::from_str("vis"));
            set(&object, "mode", JsValue::from_str(mode));
            set(
                &object,
                "sampleOffset",
                JsValue::from_f64(sample_offset as f64),
            );
            set(&object, "hedrShiftHz", JsValue::from_f64(hedr_shift_hz));
        }
        CoreEvent::UnknownVis {
            code,
            sample_offset,
            hedr_shift_hz,
        } => {
            set(&object, "type", JsValue::from_str("unknownVis"));
            set(&object, "code", JsValue::from_f64(f64::from(code)));
            set(
                &object,
                "sampleOffset",
                JsValue::from_f64(sample_offset as f64),
            );
            set(&object, "hedrShiftHz", JsValue::from_f64(hedr_shift_hz));
        }
        CoreEvent::Line {
            mode,
            line_index,
            rgb,
        } => {
            set(&object, "type", JsValue::from_str("line"));
            set(&object, "mode", JsValue::from_str(mode));
            set(
                &object,
                "lineIndex",
                JsValue::from_f64(f64::from(line_index)),
            );
            set(
                &object,
                "rgb",
                js_sys::Uint8Array::from(rgb.as_slice()).into(),
            );
        }
        CoreEvent::Image {
            mode,
            width,
            height,
            rgba,
        } => {
            set(&object, "type", JsValue::from_str("image"));
            set(&object, "mode", JsValue::from_str(mode));
            set(&object, "width", JsValue::from_f64(f64::from(width)));
            set(&object, "height", JsValue::from_f64(f64::from(height)));
            set(
                &object,
                "rgba",
                js_sys::Uint8Array::from(rgba.as_slice()).into(),
            );
        }
    }
    object.into()
}

/// 流式 SSTV 解码器。
#[wasm_bindgen]
pub struct WasmDecoder {
    inner: CoreDecoder,
}

#[wasm_bindgen]
impl WasmDecoder {
    /// 构造解码器。`sample_rate_hz` 为输入音频采样率（0–192000）。
    ///
    /// # Errors
    /// 采样率非法时抛出错误。
    #[wasm_bindgen(constructor)]
    pub fn new(sample_rate_hz: u32) -> Result<WasmDecoder, JsValue> {
        let inner = CoreDecoder::new(sample_rate_hz, false).map_err(|e| JsValue::from_str(&e))?;
        Ok(WasmDecoder { inner })
    }

    /// 配置强制模式 + 解码窗口（选区不含 VIS 头时使用）。
    ///
    /// `start_secs` / `end_secs` 相对“喂入流的第一帧”计秒，至少提供一个。
    ///
    /// # Errors
    /// 模式名无法解析或缺少锚点时抛出错误。
    #[wasm_bindgen(js_name = setForcedMode)]
    pub fn set_forced_mode(
        &mut self,
        mode: &str,
        start_secs: Option<f64>,
        end_secs: Option<f64>,
    ) -> Result<(), JsValue> {
        self.inner
            .set_forced_mode(mode, start_secs, end_secs)
            .map_err(|e| JsValue::from_str(&e))
    }

    /// 关闭强制模式，恢复 VIS 自动识模。
    #[wasm_bindgen(js_name = clearForcedMode)]
    pub fn clear_forced_mode(&mut self) {
        self.inner.clear_forced_mode();
    }

    /// 丢弃进行中的图像并复位状态（保留强制模式设置）。
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// 推入一段单声道 f32 音频，返回本批事件数组。
    #[wasm_bindgen(js_name = pushAudio)]
    pub fn push_audio(&mut self, samples: &[f32]) -> js_sys::Array {
        let events = self.inner.push_audio(samples);
        let array = js_sys::Array::new();
        for event in events {
            array.push(&event_to_js(event));
        }
        array
    }

    /// 已处理的输入采样点总数（用于估算进度）。
    #[wasm_bindgen(js_name = samplesProcessed)]
    pub fn samples_processed(&self) -> f64 {
        self.inner.samples_processed() as f64
    }
}

/// 列出本构建支持的全部 SSTV 模式。
#[wasm_bindgen(js_name = listModes)]
#[must_use]
pub fn list_modes() -> js_sys::Array {
    let array = js_sys::Array::new();
    for info in core::list_modes() {
        let object = js_sys::Object::new();
        set(&object, "shortName", JsValue::from_str(info.short_name));
        set(&object, "name", JsValue::from_str(info.name));
        set(
            &object,
            "visCode",
            JsValue::from_f64(f64::from(info.vis_code)),
        );
        set(&object, "width", JsValue::from_f64(f64::from(info.width)));
        set(&object, "height", JsValue::from_f64(f64::from(info.height)));
        set(
            &object,
            "imageSeconds",
            JsValue::from_f64(info.image_seconds),
        );
        array.push(&object.into());
    }
    array
}

/// 生成指定模式的合成测试音频（11025 Hz），仅 `dev-synth` 构建可用。
///
/// `with_vis` 为 `true` 时前置 VIS 头，`false` 时只有图像体。
///
/// # Errors
/// 模式名无法识别时抛出错误。
#[cfg(feature = "dev-synth")]
#[wasm_bindgen(js_name = synthTestAudio)]
pub fn synth_test_audio(mode: &str, with_vis: bool) -> Result<Vec<f32>, JsValue> {
    core::synth_test_audio(mode, with_vis).ok_or_else(|| JsValue::from_str("无法识别的模式"))
}

/// 频谱图强度矩阵（列优先、8 位）。
#[wasm_bindgen]
pub struct Spectrogram {
    inner: spectrogram::Spectrogram,
}

#[wasm_bindgen]
impl Spectrogram {
    /// 时间列数。
    #[wasm_bindgen(getter)]
    pub fn columns(&self) -> u32 {
        self.inner.columns()
    }

    /// 每列的频率 bin 数。
    #[wasm_bindgen(getter)]
    pub fn bins(&self) -> u32 {
        self.inner.bins()
    }

    /// 帧移（输入采样点）。
    #[wasm_bindgen(getter)]
    pub fn hop(&self) -> u32 {
        self.inner.hop()
    }

    /// 输入采样率。
    #[wasm_bindgen(getter, js_name = sampleRate)]
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    /// 显示上限频率（Hz）。
    #[wasm_bindgen(getter, js_name = maxHz)]
    pub fn max_hz(&self) -> f64 {
        self.inner.max_hz()
    }

    /// 每列代表的时间跨度（秒）。
    #[wasm_bindgen(getter, js_name = secondsPerColumn)]
    pub fn seconds_per_column(&self) -> f64 {
        self.inner.seconds_per_column()
    }

    /// 列优先的强度矩阵，长度 `columns * bins`。
    pub fn data(&self) -> js_sys::Uint8Array {
        js_sys::Uint8Array::from(self.inner.data())
    }
}

/// 流式 STFT（实时接收时的滚动瀑布图）。
///
/// 与 [`Spectrogram`] 使用相同的加窗与归一化；按块 `push` 音频，只返回本批
/// **新产生的**频谱列，避免为持续数分钟的实时流反复重算整段。
#[wasm_bindgen]
pub struct StreamingSpectrogram {
    inner: spectrogram::StreamingSpectrogram,
}

#[wasm_bindgen]
impl StreamingSpectrogram {
    /// 构造流式 STFT。`fft_size`、`hop`、`max_hz` 传 0 时使用默认值。
    ///
    /// # Errors
    /// 参数非法时抛出错误。
    #[wasm_bindgen(constructor)]
    pub fn new(
        sample_rate: u32,
        fft_size: u32,
        hop: u32,
        max_hz: f64,
    ) -> Result<StreamingSpectrogram, JsValue> {
        let fft_size = if fft_size == 0 {
            spectrogram::DEFAULT_FFT_SIZE
        } else {
            fft_size as usize
        };
        let hop = if hop == 0 {
            // 实时默认 50% 重叠：列更少、滚动更平滑，且频率分辨率不变。
            fft_size / 2
        } else {
            hop as usize
        };
        let max_hz = if max_hz <= 0.0 {
            spectrogram::DEFAULT_MAX_HZ
        } else {
            max_hz
        };
        spectrogram::StreamingSpectrogram::new(sample_rate, fft_size, hop, max_hz)
            .map(|inner| StreamingSpectrogram { inner })
            .ok_or_else(|| JsValue::from_str("流式频谱图参数非法"))
    }

    /// 推入一段音频，返回本批新产生的频谱列（列优先 8 位强度，长度
    /// `新列数 × bins`）。不足一帧时返回空数组。
    pub fn push(&mut self, samples: &[f32]) -> js_sys::Uint8Array {
        let columns = self.inner.push(samples);
        js_sys::Uint8Array::from(columns.as_slice())
    }

    /// 每列的频率 bin 数。
    #[wasm_bindgen(getter)]
    pub fn bins(&self) -> u32 {
        self.inner.bins()
    }

    /// 帧移（输入采样点）。
    #[wasm_bindgen(getter)]
    pub fn hop(&self) -> u32 {
        self.inner.hop()
    }

    /// 输入采样率。
    #[wasm_bindgen(getter, js_name = sampleRate)]
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    /// 显示上限频率（Hz）。
    #[wasm_bindgen(getter, js_name = maxHz)]
    pub fn max_hz(&self) -> f64 {
        self.inner.max_hz()
    }

    /// 每列代表的时间跨度（秒）。
    #[wasm_bindgen(getter, js_name = secondsPerColumn)]
    pub fn seconds_per_column(&self) -> f64 {
        self.inner.seconds_per_column()
    }
}

/// 计算 STFT 频谱图（在 Worker 内调用）。
///
/// # Errors
/// 音频短于一帧或参数非法时抛出错误。
#[wasm_bindgen(js_name = computeSpectrogram)]
pub fn compute_spectrogram(
    samples: &[f32],
    sample_rate: u32,
    fft_size: u32,
    hop: u32,
    max_hz: f64,
) -> Result<Spectrogram, JsValue> {
    // 传 0 表示使用默认参数。
    let fft_size = if fft_size == 0 {
        spectrogram::DEFAULT_FFT_SIZE
    } else {
        fft_size as usize
    };
    let hop = if hop == 0 {
        spectrogram::DEFAULT_HOP
    } else {
        hop as usize
    };
    let max_hz = if max_hz <= 0.0 {
        spectrogram::DEFAULT_MAX_HZ
    } else {
        max_hz
    };
    spectrogram::Spectrogram::compute(samples, sample_rate, fft_size, hop, max_hz)
        .map(|inner| Spectrogram { inner })
        .ok_or_else(|| JsValue::from_str("音频过短或参数非法，无法计算频谱图"))
}
