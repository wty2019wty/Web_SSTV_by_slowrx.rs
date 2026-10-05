//! 与 WebAssembly 无关的解码核心封装。
//!
//! 这一层只依赖纯 Rust 的 `slowrx`，返回 Rust 原生结构，因此可以直接在
//! 原生（非 wasm）环境做单元测试。`crate::lib` 里的 `#[wasm_bindgen]`
//! 层负责把这些结构转换成 JS 对象 / `Uint8Array`。

use slowrx::{DecodeWindow, SstvDecoder, SstvEvent, SstvMode};

/// 一次 `push_audio` 可能产生的事件（面向 JS 的精简表示）。
///
/// 像素统一用扁平字节数组承载：
/// - [`CoreEvent::Line`] 的 `rgb` 为 `width * 3` 字节（R,G,B 顺序）；
/// - [`CoreEvent::Image`] 的 `rgba` 为 `width * height * 4` 字节。
#[derive(Debug, Clone)]
pub enum CoreEvent {
    /// 检测到 VIS 头并识别出模式（自动识模路径）。
    VisDetected {
        /// 模式的短名（如 `"pd120"`）。
        mode: &'static str,
        /// 工作采样率（11025 Hz）下的样本偏移。
        sample_offset: u64,
        /// 电台失谐偏移，单位 Hz。
        hedr_shift_hz: f64,
    },
    /// VIS 头通过校验，但对应模式本构建无法解码。
    UnknownVis {
        /// 未识别的 7 位 VIS 码。
        code: u8,
        sample_offset: u64,
        hedr_shift_hz: f64,
    },
    /// 一行扫描线完成（仅在 `emit_lines` 开启时产生）。
    Line {
        mode: &'static str,
        line_index: u32,
        /// 该行像素，RGB 顺序，长度 `width * 3`。
        rgb: Vec<u8>,
    },
    /// 一张图完整解码完成（`partial` 为真表示提前收尾的不完整图）。
    Image {
        mode: &'static str,
        width: u32,
        height: u32,
        /// RGBA，长度 `width * height * 4`。
        rgba: Vec<u8>,
        /// 是否为不完整图（实时接收中途停止时的收尾结果）。
        partial: bool,
    },
}

/// 供 UI 展示的模式元数据。
#[derive(Debug, Clone, Copy)]
pub struct ModeInfo {
    pub short_name: &'static str,
    pub name: &'static str,
    pub vis_code: u8,
    pub width: u32,
    pub height: u32,
    /// 图像体的标称时长（秒，不含 VIS 头）。
    pub image_seconds: f64,
}

/// 列出本构建支持的全部 SSTV 模式。
#[must_use]
pub fn list_modes() -> Vec<ModeInfo> {
    slowrx::all_specs()
        .iter()
        .map(|spec| ModeInfo {
            short_name: spec.short_name,
            name: spec.name,
            vis_code: spec.vis_code,
            width: spec.line_pixels,
            height: spec.image_lines,
            image_seconds: nominal_image_seconds(spec),
        })
        .collect()
}

/// 图像体的标称时长：PD 每条无线电线承载两行，其余每线一行。
fn nominal_image_seconds(spec: &slowrx::ModeSpec) -> f64 {
    let radio_frames = match spec.channel_layout {
        slowrx::ChannelLayout::PdYcbcr => spec.image_lines / 2,
        _ => spec.image_lines,
    };
    f64::from(radio_frames) * spec.line_seconds
}

fn mode_short_name(mode: SstvMode) -> &'static str {
    slowrx::for_mode(mode).short_name
}

/// 把 row-major RGB 像素展平成 RGBA 字节数组（alpha 固定 255）。
fn rgb_to_rgba(pixels: &[[u8; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for p in pixels {
        out.extend_from_slice(&[p[0], p[1], p[2], 255]);
    }
    out
}

/// slowrx 解码器的流式封装。
pub struct CoreDecoder {
    decoder: SstvDecoder,
    emit_lines: bool,
}

impl CoreDecoder {
    /// 构造解码器。`sample_rate_hz` 为输入音频采样率（0–192000）。
    ///
    /// `emit_lines` 为 `true` 时每行都会产生 [`CoreEvent::Line`] 事件；
    /// 实际使用中取整图即可，默认关闭以省去逐行拷贝。
    ///
    /// # Errors
    /// 采样率为 0 或超过上限时返回错误字符串。
    pub fn new(sample_rate_hz: u32, emit_lines: bool) -> Result<Self, String> {
        let decoder = SstvDecoder::new(sample_rate_hz).map_err(|e| e.to_string())?;
        Ok(Self {
            decoder,
            emit_lines,
        })
    }

    /// 配置强制模式 + 解码窗口（选区不含 VIS 头时使用）。
    ///
    /// `start_secs` / `end_secs` 相对“喂入流的第一帧”计秒；至少提供一个。
    /// 二者都提供时以 `start_secs` 为准（与 slowrx 语义一致）。
    ///
    /// # Errors
    /// 模式名无法解析，或未提供任何锚点时返回错误字符串。
    pub fn set_forced_mode(
        &mut self,
        mode: &str,
        start_secs: Option<f64>,
        end_secs: Option<f64>,
    ) -> Result<(), String> {
        let mode =
            slowrx::parse_mode(mode).ok_or_else(|| format!("无法识别的 SSTV 模式：{mode}"))?;
        let window = match (start_secs, end_secs) {
            (Some(start), _) => DecodeWindow::starting_at(start),
            (None, Some(end)) => DecodeWindow::ending_at(end),
            (None, None) => return Err("强制模式必须提供 start_secs 或 end_secs 之一".into()),
        };
        self.decoder.set_forced_mode(mode, window);
        Ok(())
    }

    /// 关闭强制模式，恢复 VIS 自动识模。
    pub fn clear_forced_mode(&mut self) {
        self.decoder.clear_forced_mode();
    }

    /// 开启/关闭渐进（实时）解码；开启时同时打开逐行事件输出
    /// （[`CoreEvent::Line`]），供前端边收边画图像。
    pub fn set_progressive(&mut self, enabled: bool) {
        self.decoder.set_progressive(enabled);
        self.emit_lines = enabled;
    }

    /// 收尾：对进行中的图像做一次精修（用已收集的完整 sync 重解已到齐的行），
    /// 发出逐行事件与一张 `partial` 图。未在解码中时返回空。
    /// 无论是否产出事件，调用后都会丢弃进行中的解码状态。
    pub fn finalize(&mut self) -> Vec<CoreEvent> {
        let emit_lines = self.emit_lines;
        self.decoder
            .finalize()
            .into_iter()
            .filter_map(|event| convert_event(event, emit_lines))
            .collect()
    }

    /// 丢弃进行中的图像并复位状态（保留强制模式设置）。
    pub fn reset(&mut self) {
        self.decoder.reset();
    }

    /// 已处理的输入采样点总数。
    #[must_use]
    pub fn samples_processed(&self) -> u64 {
        self.decoder.samples_processed()
    }

    /// 推入一段单声道 f32 音频，返回本批产生的事件。
    pub fn push_audio(&mut self, samples: &[f32]) -> Vec<CoreEvent> {
        let emit_lines = self.emit_lines;
        self.decoder
            .process(samples)
            .into_iter()
            .filter_map(|event| convert_event(event, emit_lines))
            .collect()
    }
}

/// 把 `SstvEvent` 转成面向 JS 的精简事件；`LineDecoded` 在关闭逐行输出时丢弃。
fn convert_event(event: SstvEvent, emit_lines: bool) -> Option<CoreEvent> {
    match event {
        SstvEvent::VisDetected {
            mode,
            sample_offset,
            hedr_shift_hz,
        } => Some(CoreEvent::VisDetected {
            mode: mode_short_name(mode),
            sample_offset,
            hedr_shift_hz,
        }),
        SstvEvent::UnknownVis {
            code,
            sample_offset,
            hedr_shift_hz,
        } => Some(CoreEvent::UnknownVis {
            code,
            sample_offset,
            hedr_shift_hz,
        }),
        SstvEvent::LineDecoded {
            mode,
            line_index,
            pixels,
        } => emit_lines.then(|| {
            let mut rgb = Vec::with_capacity(pixels.len() * 3);
            for p in &pixels {
                rgb.extend_from_slice(p);
            }
            CoreEvent::Line {
                mode: mode_short_name(mode),
                line_index,
                rgb,
            }
        }),
        SstvEvent::ImageComplete { image, partial } => Some(CoreEvent::Image {
            mode: mode_short_name(image.mode),
            width: image.width,
            height: image.height,
            rgba: rgb_to_rgba(&image.pixels),
            partial,
        }),
        // `SstvEvent` 标注了 #[non_exhaustive]，未来新增变体时忽略即可。
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 开发/测试用合成音频（仅 test 或 dev-synth feature 下编译）
// ---------------------------------------------------------------------------

/// 生成指定模式的合成测试音频（11025 Hz），用于开发自测。
///
/// `with_vis` 为 `true` 时前置 VIS 头，`false` 时只有图像体（配合强制模式）。
#[cfg(any(test, feature = "dev-synth"))]
#[must_use]
pub fn synth_test_audio(mode: &str, with_vis: bool) -> Option<Vec<f32>> {
    let mode = slowrx::parse_mode(mode)?;
    Some(synth_test_audio_for(mode, with_vis))
}

#[cfg(any(test, feature = "dev-synth"))]
fn synth_test_audio_for(mode: SstvMode, with_vis: bool) -> Vec<f32> {
    use slowrx::__test_support;

    let spec = slowrx::for_mode(mode);
    let (w, h) = (spec.line_pixels, spec.image_lines);
    let mut audio = if with_vis {
        __test_support::vis::synth_vis(spec.vis_code, 0.0)
    } else {
        Vec::new()
    };

    match spec.channel_layout {
        slowrx::ChannelLayout::PdYcbcr => {
            let mut ycrcb = Vec::with_capacity((w * h) as usize);
            for y in 0..h {
                for x in 0..w {
                    let lum = ((f64::from(x)) / f64::from(w) * 255.0) as u8;
                    let cr = if y % 4 < 2 { 200 } else { 56 };
                    let cb = if (y / 2) % 2 == 0 { 200 } else { 56 };
                    ycrcb.push([lum, cr, cb]);
                }
            }
            audio.extend(__test_support::mode_pd::encode_pd(mode, &ycrcb));
        }
        slowrx::ChannelLayout::RobotYuv => {
            let mut ycrcb = Vec::with_capacity((w * h) as usize);
            for y in 0..h {
                for x in 0..w {
                    let lum = ((f64::from(x)) / f64::from(w) * 255.0) as u8;
                    let cr = if y % 4 < 2 { 200 } else { 56 };
                    let cb = if (y + 1) % 4 < 2 { 200 } else { 56 };
                    ycrcb.push([lum, cr, cb]);
                }
            }
            audio.extend(__test_support::mode_robot::encode_robot(mode, &ycrcb));
        }
        slowrx::ChannelLayout::RgbSequential => {
            let mut rgb = Vec::with_capacity((w * h) as usize);
            for y in 0..h {
                for x in 0..w {
                    let r = ((f64::from(x)) / f64::from(w) * 255.0) as u8;
                    let g = if y % 8 < 4 { 200 } else { 56 };
                    let b = if (y + 2) % 8 < 4 { 200 } else { 56 };
                    rgb.push([r, g, b]);
                }
            }
            audio.extend(__test_support::mode_scottie::encode_scottie(mode, &rgb));
        }
        _ => {}
    }

    // 末尾补静音：清掉重采样器群延迟并满足强制模式的尾部余量（方案 4.3）。
    audio.extend(std::iter::repeat_n(
        0.0_f32,
        slowrx::WORKING_SAMPLE_RATE_HZ as usize + 8192,
    ));
    audio
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 找到唯一一张完整解码的图，返回其 RGBA。
    fn only_image(events: &[CoreEvent]) -> (&'static str, u32, u32, &[u8]) {
        let mut found = None;
        for e in events {
            if let CoreEvent::Image {
                mode,
                width,
                height,
                rgba,
                ..
            } = e
            {
                assert!(found.is_none(), "期望恰好一张图，出现多张");
                found = Some((*mode, *width, *height, rgba.as_slice()));
            }
        }
        found.expect("应产生一张 ImageComplete")
    }

    /// 分块喂入，模拟 Worker 的真实推送方式。
    fn feed_chunked(decoder: &mut CoreDecoder, audio: &[f32], chunk: usize) -> Vec<CoreEvent> {
        let mut out = Vec::new();
        for part in audio.chunks(chunk) {
            out.extend(decoder.push_audio(part));
        }
        out
    }

    #[test]
    fn list_modes_covers_all_families() {
        let modes = list_modes();
        let shorts: Vec<_> = modes.iter().map(|m| m.short_name).collect();
        for want in [
            "pd120",
            "pd180",
            "pd240",
            "robot24",
            "robot36",
            "robot72",
            "scottie1",
            "scottie2",
            "scottiedx",
            "martin1",
            "martin2",
        ] {
            assert!(shorts.contains(&want), "缺少模式 {want}");
        }
    }

    #[test]
    fn pd120_auto_vis_decodes_chunked() {
        let audio = synth_test_audio("pd120", true).expect("合成音频");
        let mut decoder = CoreDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ, false).expect("decoder");
        let events = feed_chunked(&mut decoder, &audio, 1024);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::VisDetected { .. })),
            "自动识模应产生 VisDetected"
        );
        let (mode, w, h, rgba) = only_image(&events);
        assert_eq!(mode, "pd120");
        let spec = slowrx::for_mode(SstvMode::Pd120);
        assert_eq!((w, h), (spec.line_pixels, spec.image_lines));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // alpha 通道固定 255。
        assert!(rgba.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn pd120_forced_mode_decodes_without_vis() {
        let audio = synth_test_audio("pd120", false).expect("合成音频");
        let mut decoder = CoreDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ, false).expect("decoder");
        decoder
            .set_forced_mode("pd120", Some(0.0), None)
            .expect("配置强制模式");
        let events = feed_chunked(&mut decoder, &audio, 4096);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoreEvent::VisDetected { .. })),
            "强制模式不应产生 VisDetected"
        );
        let (mode, _, _, _) = only_image(&events);
        assert_eq!(mode, "pd120");
    }

    #[test]
    fn forced_mode_requires_anchor() {
        let mut decoder = CoreDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ, false).expect("decoder");
        assert!(decoder.set_forced_mode("pd120", None, None).is_err());
        assert!(decoder
            .set_forced_mode("no-such-mode", Some(0.0), None)
            .is_err());
    }

    #[test]
    fn silence_forced_mode_yields_nothing() {
        let silence = vec![0.0_f32; slowrx::WORKING_SAMPLE_RATE_HZ as usize * 45];
        let mut decoder = CoreDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ, false).expect("decoder");
        decoder
            .set_forced_mode("robot24", Some(0.0), None)
            .expect("配置强制模式");
        let events = decoder.push_audio(&silence);
        assert!(
            !events.iter().any(|e| matches!(e, CoreEvent::Image { .. })),
            "静音窗口不应伪造出图像"
        );
    }

    /// 渐进（实时）解码：应跨多次 `process` 逐步产出 `LineDecoded`，且最终图像
    /// 与批处理逐像素一致。
    #[test]
    fn progressive_streams_lines_and_matches_batch() {
        use slowrx::SstvDecoder;

        let audio = synth_test_audio("pd120", true).expect("合成音频");

        // 批处理参考图。
        let mut batch = SstvDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ).expect("decoder");
        let batch_image = batch
            .process(&audio)
            .into_iter()
            .find_map(|event| match event {
                SstvEvent::ImageComplete { image, .. } => Some(image),
                _ => None,
            })
            .expect("批处理应出图");

        // 渐进解码：分块喂入，统计多少批产生了行事件。
        let mut decoder = SstvDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ).expect("decoder");
        decoder.set_progressive(true);
        assert!(decoder.progressive());
        let mut line_batches = 0_usize;
        let mut progressive_image = None;
        for chunk in audio.chunks(2048) {
            let events = decoder.process(chunk);
            if events
                .iter()
                .any(|e| matches!(e, SstvEvent::LineDecoded { .. }))
            {
                line_batches += 1;
            }
            if progressive_image.is_none() {
                progressive_image = events.into_iter().find_map(|event| match event {
                    SstvEvent::ImageComplete { image, .. } => Some(image),
                    _ => None,
                });
            }
        }

        assert!(
            line_batches > 3,
            "渐进解码应在多个 process 批次逐步产出（实际 {line_batches} 批）"
        );
        let image = progressive_image.expect("渐进解码应出图");
        assert_eq!(image.mode, batch_image.mode);
        assert_eq!(
            (image.width, image.height),
            (batch_image.width, batch_image.height)
        );
        assert_eq!(image.pixels, batch_image.pixels, "渐进结果应与批处理一致");
    }

    /// 收尾精修：只喂入部分音频后 `finalize`，应产出一张标记 `partial` 的图。
    #[test]
    fn finalize_produces_partial_image() {
        let audio = synth_test_audio("pd120", true).expect("合成音频");
        // 只喂入约 40% 的音频（足以检测 VIS 与部分行）。
        let partial = &audio[..(audio.len() * 2 / 5)];

        let mut decoder = CoreDecoder::new(slowrx::WORKING_SAMPLE_RATE_HZ, true).expect("decoder");
        decoder.set_progressive(true);
        let _ = decoder.push_audio(partial);

        let spec = slowrx::for_mode(SstvMode::Pd120);
        let (is_partial, width, height, rgba) = decoder
            .finalize()
            .into_iter()
            .find_map(|event| match event {
                CoreEvent::Image {
                    partial,
                    width,
                    height,
                    rgba,
                    ..
                } => Some((partial, width, height, rgba)),
                _ => None,
            })
            .expect("finalize 应产出 partial 图");

        assert!(is_partial, "finalize 结果应标记为 partial");
        assert_eq!((width, height), (spec.line_pixels, spec.image_lines));
        let nonblack = rgba
            .chunks_exact(4)
            .filter(|px| px[0] > 10 || px[1] > 10 || px[2] > 10)
            .count();
        assert!(nonblack > 0, "应至少解出一部分像素");
    }
}
