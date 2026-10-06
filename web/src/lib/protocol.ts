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
  | { type: 'mistuning'; hedrShiftHz: number; fromVis: boolean }
  | { type: 'line'; mode: string; lineIndex: number; rgb: Uint8Array }
  | {
      type: 'image'
      mode: string
      width: number
      height: number
      rgba: Uint8Array
      /** 是否为不完整图（实时接收中途停止时的收尾结果）。 */
      partial: boolean
    }

/** 载入完成后的会话信息。 */
export interface LoadedInfo {
  sampleRate: number
  totalSamples: number
  /** 总时长（秒）。 */
  duration: number
  spectrogram: SpectrogramInfo
  /** PCM 副本（主线程用于播放；Worker 另存原始副本用于解码）。 */
  audio: Float32Array
}

/** 强制模式的锚点端点：选区开始或结束（方案 4.2/6.5）。 */
export type ForcedAnchor = 'start' | 'end'

/** 实时接收会话建立后，Worker 回传的频谱参数。 */
export interface LiveInfo {
  sampleRate: number
  /** 每列的频率 bin 数。 */
  bins: number
  /** 帧移（输入采样点）。 */
  hop: number
  /** 每列代表的时间跨度（秒）。 */
  secondsPerColumn: number
  /** 显示上限频率（Hz）。 */
  maxHz: number
}

/** 实时接收会话结束时的小结。 */
export interface LiveSummary {
  elapsedMs: number
  imageCount: number
}

/** 解码进度快照（Worker 在扫描/解码期间持续回传）。 */
export interface DecodeProgress {
  /** 已扫描（喂入解码器）的音频秒数。 */
  scannedSeconds: number
  /** 待扫描的音频总秒数（含末尾补静音）。 */
  scanSpanSeconds: number
  /** 已解码的图像音频秒数（多图累计）。 */
  decodedSeconds: number
  /** 已知图像窗口的总秒数（多图累计，随新图发现而增加）。 */
  decodeSpanSeconds: number
  /** 扫描阶段累计墙钟毫秒（用于估算扫描速率）。 */
  scanMs: number
  /** 解码阶段累计墙钟毫秒（用于估算解码速率）。 */
  decodeMs: number
}

/** 主线程 -> Worker。 */
export type MainToWorker =
  | { type: 'listModes'; requestId: number }
  | { type: 'loadSynth'; requestId: number; mode: string; withVis: boolean; count: number }
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
      /** 强制模式的失谐兜底值（Hz）：窗口内无 VIS 头时用于补偿电台失谐。 */
      hedrShiftHz?: number
    }
  | { type: 'liveStart'; requestId: number; sampleRate: number }
  | { type: 'livePush'; requestId: number; samples: Float32Array }
  | { type: 'liveStop'; requestId: number }

/** Worker -> 主线程。 */
export type WorkerToMain =
  | { type: 'modes'; requestId: number; modes: ModeInfo[] }
  | ({ type: 'loaded'; requestId: number } & LoadedInfo)
  | { type: 'event'; requestId: number; event: DecodeEvent }
  | ({ type: 'progress'; requestId: number } & DecodeProgress)
  | { type: 'done'; requestId: number; elapsedMs: number }
  | ({ type: 'liveStarted'; requestId: number } & LiveInfo)
  | {
      type: 'liveColumns'
      requestId: number
      /** 本批新列（列优先 8 位强度，长度 `count * bins`）。 */
      columns: Uint8Array
      count: number
      bins: number
    }
  | ({ type: 'liveStopped'; requestId: number } & LiveSummary)
  | { type: 'error'; requestId: number; message: string }
