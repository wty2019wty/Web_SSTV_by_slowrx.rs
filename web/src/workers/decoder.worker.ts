/// <reference lib="webworker" />
// 解码 / 频谱图 Worker：所有 wasm 计算都跑在这里，避免阻塞主线程。
//
// 会话模型：本 Worker 持有当前载入的 PCM，频谱图在此计算（复用 wasm 内
// rustfft），选区解码时直接切片并末尾补静音（方案 4.3、6.3、6.5）。

import init, { WasmDecoder, listModes, computeSpectrogram } from '../wasm/slowrx_wasm.js'
// 显式带上 wasm 的 URL，交给 Vite 处理（dev 与 build 都能正确定位资源）。
import wasmUrl from '../wasm/slowrx_wasm_bg.wasm?url'
import type {
  DecodeEvent,
  MainToWorker,
  ModeInfo,
  SpectrogramInfo,
  WorkerToMain,
} from '../lib/protocol'

const ctx = self as unknown as DedicatedWorkerGlobalScope

/** wasm 初始化只做一次（首次消息时惰性触发）。 */
let ready: Promise<void> | null = null
function ensureReady(): Promise<void> {
  if (!ready) {
    ready = init({ module_or_path: wasmUrl }).then(() => undefined)
  }
  return ready
}

function post(message: WorkerToMain, transfer?: Transferable[]): void {
  ctx.postMessage(message, transfer ?? [])
}

interface Session {
  sampleRate: number
  audio: Float32Array
}

let session: Session | null = null

/** STFT 参数：11025 Hz 下频率分辨率约 10.8 Hz，75% 重叠。 */
const FFT_SIZE = 1024
const HOP = 256
const MAX_HZ = 4000
/** 每批推入解码器的采样点数。 */
const FEED_CHUNK = 32768
/** 选区末尾补静音时长（秒），保证解码器能凑满一帧触发解码（方案 4.3）。 */
const PAD_SECONDS = 1.0

/** 加载 dev-synth 才存在的合成音频导出（生产构建下给出明确提示）。 */
async function synthAudio(mode: string, withVis: boolean): Promise<Float32Array> {
  const mod = (await import('../wasm/slowrx_wasm.js')) as typeof import('../wasm/slowrx_wasm.js') & {
    synthTestAudio?: (mode: string, withVis: boolean) => Float32Array
  }
  if (typeof mod.synthTestAudio !== 'function') {
    throw new Error('当前 wasm 构建不含合成音频工具（请用 build-wasm.ps1 -Dev 构建）')
  }
  return mod.synthTestAudio(mode, withVis)
}

function computeSpectrogramInfo(sampleRate: number, audio: Float32Array): SpectrogramInfo {
  const spec = computeSpectrogram(audio, sampleRate, FFT_SIZE, HOP, MAX_HZ)
  try {
    return {
      columns: spec.columns,
      bins: spec.bins,
      hop: spec.hop,
      sampleRate: spec.sampleRate,
      maxHz: spec.maxHz,
      secondsPerColumn: spec.secondsPerColumn,
      data: spec.data(),
    }
  } finally {
    spec.free()
  }
}

async function storeAndReport(requestId: number, sampleRate: number, audio: Float32Array): Promise<void> {
  session = { sampleRate, audio }
  const spectrogram = computeSpectrogramInfo(sampleRate, audio)
  post(
    {
      type: 'loaded',
      requestId,
      sampleRate,
      totalSamples: audio.length,
      duration: audio.length / sampleRate,
      spectrogram,
    },
    [spectrogram.data.buffer],
  )
}

async function decodeSelection(
  requestId: number,
  startSample: number | undefined,
  endSample: number | undefined,
  mode: string | undefined,
): Promise<void> {
  await ensureReady()
  if (!session) throw new Error('尚未载入音频')
  const { sampleRate, audio } = session

  const start = Math.max(0, Math.min(startSample ?? 0, audio.length))
  const end = Math.max(start, Math.min(endSample ?? audio.length, audio.length))
  const padLength = Math.round(sampleRate * PAD_SECONDS)
  const slice = new Float32Array(end - start + padLength)
  slice.set(audio.subarray(start, end), 0) // 其余保持 0（静音）

  console.log(`[worker] 选区解码：${end - start} 采样 + ${padLength} 静音 @ ${sampleRate} Hz`)
  const startedAt = performance.now()
  const decoder = new WasmDecoder(sampleRate)
  try {
    if (mode) decoder.setForcedMode(mode, 0, undefined)
    for (let offset = 0; offset < slice.length; offset += FEED_CHUNK) {
      const stop = Math.min(offset + FEED_CHUNK, slice.length)
      const events = decoder.pushAudio(slice.subarray(offset, stop)) as DecodeEvent[]
      for (const event of events) post({ type: 'event', requestId, event })
      post({ type: 'progress', requestId, fedSamples: stop, totalSamples: slice.length })
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
      case 'loadSynth': {
        await ensureReady()
        const audio = await synthAudio(message.mode, message.withVis)
        await storeAndReport(message.requestId, 11025, audio)
        break
      }
      case 'loadAudio': {
        await ensureReady()
        await storeAndReport(message.requestId, message.sampleRate, message.audio)
        break
      }
      case 'decode': {
        await decodeSelection(
          message.requestId,
          message.startSample,
          message.endSample,
          message.mode,
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
