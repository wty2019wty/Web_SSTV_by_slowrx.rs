// 主线程 <-> 解码 Worker 的消息协议。
//
// 设计原则：音频与像素都以二进制（Float32Array / Uint8Array）传递，
// 主线程与 Worker 之间用 postMessage + transferable，避免 SharedArrayBuffer
// 带来的 COOP/COEP 部署复杂度（方案 5）。

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

/** wasm 核心事件在 JS 侧的表示（与 slowrx-wasm/src/lib.rs 一一对应）。 */
export type DecodeEvent =
  | { type: 'vis'; mode: string; sampleOffset: number; hedrShiftHz: number }
  | { type: 'unknownVis'; code: number; sampleOffset: number; hedrShiftHz: number }
  | { type: 'line'; mode: string; lineIndex: number; rgb: Uint8Array }
  | { type: 'image'; mode: string; width: number; height: number; rgba: Uint8Array }

/** 主线程 -> Worker。 */
export type MainToWorker =
  | { type: 'listModes'; requestId: number }
  | { type: 'synth'; requestId: number; mode: string; withVis: boolean }
  | {
      type: 'decode'
      requestId: number
      sampleRate: number
      audio: Float32Array
      /** 提供时走强制模式；否则 VIS 自动识模。 */
      mode?: string
      startSecs?: number
      endSecs?: number
    }

/** Worker -> 主线程。 */
export type WorkerToMain =
  | { type: 'modes'; requestId: number; modes: ModeInfo[] }
  | { type: 'event'; requestId: number; event: DecodeEvent }
  | { type: 'progress'; requestId: number; fedSamples: number; totalSamples: number }
  | { type: 'done'; requestId: number; elapsedMs: number }
  | { type: 'error'; requestId: number; message: string }
