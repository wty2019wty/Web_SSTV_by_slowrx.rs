// 主线程 <-> 解码 Worker 的消息协议。
//
// 会话模型：Worker 持有当前载入的 PCM（来自文件或合成），后续的频谱图计算与
// 选区解码都引用这份音频，避免反复搬运大数组（方案 5、6.3）。
// 音频与像素均以二进制（Float32Array / Uint8Array）传递，配合 transferable。

/** 时间选区（秒），start < end。 */
export interface TimeSelection {
  start: number
  end: number
}

/** 由 wasm 侧 `listModes()` 返回的模式元数据。 */
export interface ModeInfo {
  shortName: string
  name: string
  visCode: number
  width: number
  height: number
  /** 图像体标称时长（秒，不含 VIS 头）。 */
  imageSeconds: number
}

/** 频谱图信息（强度矩阵列优先）。 */
export interface SpectrogramInfo {
  columns: number
  bins: number
  hop: number
  sampleRate: number
  maxHz: number
  secondsPerColumn: number
  /** 长度 columns*bins，列优先 `data[col*bins + bin]`，取值 0–255。 */
  data: Uint8Array
}

/** wasm 核心事件在 JS 侧的表示（与 slowrx-wasm/src/lib.rs 一一对应）。 */
export type DecodeEvent =
  | { type: 'vis'; mode: string; sampleOffset: number; hedrShiftHz: number }
  | { type: 'unknownVis'; code: number; sampleOffset: number; hedrShiftHz: number }
  | { type: 'line'; mode: string; lineIndex: number; rgb: Uint8Array }
  | { type: 'image'; mode: string; width: number; height: number; rgba: Uint8Array }

/** 载入完成后的会话信息。 */
export interface LoadedInfo {
  sampleRate: number
  totalSamples: number
  /** 总时长（秒）。 */
  duration: number
  spectrogram: SpectrogramInfo
}

/** 强制模式的锚点端点：选区开始或结束（方案 4.2/6.5）。 */
export type ForcedAnchor = 'start' | 'end'

/** 主线程 -> Worker。 */
export type MainToWorker =
  | { type: 'listModes'; requestId: number }
  | { type: 'loadSynth'; requestId: number; mode: string; withVis: boolean }
  | { type: 'loadAudio'; requestId: number; sampleRate: number; audio: Float32Array }
  | {
      type: 'decode'
      requestId: number
      /** 选区（输入采样点）；缺省为整段。 */
      startSample?: number
      endSample?: number
      /** 提供时走强制模式（选区不含 VIS 头）；否则 VIS 自动识模。 */
      mode?: string
      /** 强制模式的锚点端点，默认 `start`。 */
      anchor?: ForcedAnchor
    }

/** Worker -> 主线程。 */
export type WorkerToMain =
  | { type: 'modes'; requestId: number; modes: ModeInfo[] }
  | ({ type: 'loaded'; requestId: number } & LoadedInfo)
  | { type: 'event'; requestId: number; event: DecodeEvent }
  | { type: 'progress'; requestId: number; fedSamples: number; totalSamples: number }
  | { type: 'done'; requestId: number; elapsedMs: number }
  | { type: 'error'; requestId: number; message: string }
