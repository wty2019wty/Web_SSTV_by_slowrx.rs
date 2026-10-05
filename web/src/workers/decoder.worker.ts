/// <reference lib="webworker" />
// 解码 Worker：所有 wasm 解码与（后续的）STFT 都跑在这里，避免阻塞主线程。
//
// 方案 4.1：slowrx 是“两遍式”解码 —— 缓冲满约一张图后才爆发式计算，
// 因此这里的工作模式是“喂入整段切片 → 收完整事件”，进度按喂入量估算。

import init, { WasmDecoder, listModes } from '../wasm/slowrx_wasm.js'
// 显式带上 wasm 的 URL，交给 Vite 处理（dev 与 build 都能正确定位资源），
// 避免依赖 wasm-bindgen 默认的 `new URL(..., import.meta.url)` 猜测。
import wasmUrl from '../wasm/slowrx_wasm_bg.wasm?url'
import type { MainToWorker, WorkerToMain, ModeInfo, DecodeEvent } from '../lib/protocol'

const ctx = self as unknown as DedicatedWorkerGlobalScope

/** wasm 初始化只做一次（首次消息时惰性触发）。 */
let ready: Promise<void> | null = null
function ensureReady(): Promise<void> {
  if (!ready) {
    ready = init({ module_or_path: wasmUrl }).then(() => undefined)
  }
  return ready
}

function post(message: WorkerToMain): void {
  ctx.postMessage(message)
}

/** 每批推入的采样点数（约 3 秒 @11025 Hz）。 */
const FEED_CHUNK = 32768

async function runDecode(
  requestId: number,
  sampleRate: number,
  audio: Float32Array,
  mode?: string,
  startSecs?: number,
  endSecs?: number,
): Promise<void> {
  await ensureReady()
  const startedAt = performance.now()
  const decoder = new WasmDecoder(sampleRate)
  console.log(`[worker] 待解码 ${audio.length} 采样 @ ${sampleRate} Hz`)
  try {
    if (mode) {
      decoder.setForcedMode(mode, startSecs, endSecs)
    }
    for (let offset = 0; offset < audio.length; offset += FEED_CHUNK) {
      const end = Math.min(offset + FEED_CHUNK, audio.length)
      const events = decoder.pushAudio(audio.subarray(offset, end)) as DecodeEvent[]
      for (const event of events) {
        post({ type: 'event', requestId, event })
      }
      post({
        type: 'progress',
        requestId,
        fedSamples: end,
        totalSamples: audio.length,
      })
    }
    post({ type: 'done', requestId, elapsedMs: performance.now() - startedAt })
    console.log(`[worker] 解码完成，耗时 ${(performance.now() - startedAt).toFixed(0)} ms`)
  } finally {
    decoder.free()
  }
}

ctx.onmessage = async (e: MessageEvent<MainToWorker>) => {
  const message = e.data
  try {
    switch (message.type) {
      case 'listModes': {
        await ensureReady()
        post({ type: 'modes', requestId: message.requestId, modes: listModes() as ModeInfo[] })
        break
      }
      case 'synth': {
        await ensureReady()
        // `dev-synth` 构建才有该导出；生产构建下给出明确提示而非崩溃。
        const mod = (await import('../wasm/slowrx_wasm.js')) as typeof import('../wasm/slowrx_wasm.js') & {
          synthTestAudio?: (mode: string, withVis: boolean) => Float32Array
        }
        if (typeof mod.synthTestAudio !== 'function') {
          throw new Error('当前 wasm 构建不含合成音频工具（请用 build-wasm.ps1 -Dev 构建）')
        }
        const audio = mod.synthTestAudio(message.mode, message.withVis)
        await runDecode(message.requestId, 11025, audio)
        break
      }
      case 'decode': {
        await runDecode(
          message.requestId,
          message.sampleRate,
          message.audio,
          message.mode,
          message.startSecs,
          message.endSecs,
        )
        break
      }
    }
  } catch (error) {
    post({
      type: 'error',
      requestId: message.requestId,
      message: error instanceof Error ? error.message : String(error),
    })
  }
}
