/// <reference lib="webworker" />
// 解码 / 频谱图 Worker：所有 wasm 计算都跑在这里，避免阻塞主线程。
//
// 会话模型：本 Worker 持有当前载入的 PCM，频谱图在此计算（复用 wasm 内
// rustfft）。解码分两条路径（方案 6.5）：
//   - 自动识模：只喂入选区切片 + 末尾静音，由 VIS 头定位图像；
//   - 强制模式：只需一个锚点（选区开始或结束），从锚点一直喂到文件末尾
//     + 末尾静音，由 slowrx 按模式标称时长截取解码窗口。

import init, {
  WasmDecoder,
  StreamingSpectrogram,
  listModes,
  computeSpectrogram,
} from '../wasm/slowrx_wasm.js'
// 显式带上 wasm 的 URL，交给 Vite 处理（dev 与 build 都能正确定位资源）。
import wasmUrl from '../wasm/slowrx_wasm_bg.wasm?url'
import type {
  DecodeEvent,
  ForcedAnchor,
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

/** 实时接收会话（同一时刻至多一个）：常驻解码器 + 流式频谱图。 */
interface LiveSession {
  sampleRate: number
  decoder: WasmDecoder
  spec: StreamingSpectrogram
  bins: number
  /** 已产出的累计列数（用于给每批标注 firstColumn）。 */
  columns: number
  images: number
  startedAt: number
}
let live: LiveSession | null = null

/** 实时瀑布图的显示上限频率（Hz）。 */
const LIVE_MAX_HZ = 4000
/** 目标频率分辨率（Hz/bin），据此按采样率挑选 FFT 长度。 */
const LIVE_TARGET_BIN_HZ = 10.8

/**
 * 按采样率挑选 2 的幂 FFT 长度，使 bin 宽度接近 {@link LIVE_TARGET_BIN_HZ}。
 * 例如 48 kHz → 4096 点（≈11.7 Hz/bin），11.025 kHz → 1024 点。
 */
function pickLiveFftSize(sampleRate: number): number {
  const target = Math.round(sampleRate / LIVE_TARGET_BIN_HZ)
  let fft = 1024
  while (fft < target && fft < 8192) fft <<= 1
  return fft
}

/** STFT 参数：11025 Hz 下频率分辨率约 10.8 Hz，75% 重叠。 */
const FFT_SIZE = 1024
const HOP = 256
const MAX_HZ = 4000
/** 每批推入解码器的采样点数。 */
const FEED_CHUNK = 32768
/** 末尾补静音时长（秒），保证解码器能凑满一帧触发解码（方案 4.3）。
 *  取 2 s 而非 1 s：`ending_at` 推导出的起点可能贴近窗口边界，需要余量。 */
const PAD_SECONDS = 2.0

/** 加载 dev-synth 才存在的合成音频导出（生产构建下给出明确提示）。 */
async function synthAudio(mode: string, withVis: boolean, count: number): Promise<Float32Array> {
  const mod = (await import('../wasm/slowrx_wasm.js')) as typeof import('../wasm/slowrx_wasm.js') & {
    synthTestAudio?: (mode: string, withVis: boolean) => Float32Array
  }
  if (typeof mod.synthTestAudio !== 'function') {
    throw new Error('当前 wasm 构建不含合成音频工具（请用 build-wasm.ps1 -Dev 构建）')
  }
  const single = mod.synthTestAudio(mode, withVis)
  if (count <= 1) return single
  // 重复拼接（每段都带自己的 VIS 头），供自动识模一次解出多张图。
  const out = new Float32Array(single.length * count)
  for (let i = 0; i < count; i++) out.set(single, i * single.length)
  return out
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

async function storeAndReport(
  requestId: number,
  sampleRate: number,
  audio: Float32Array,
): Promise<void> {
  session = { sampleRate, audio }
  const spectrogram = computeSpectrogramInfo(sampleRate, audio)
  // 回传一份 PCM 副本给主线程做播放（Worker 自己保留原始数组用于解码）。
  const playback = audio.slice()
  post(
    {
      type: 'loaded',
      requestId,
      sampleRate,
      totalSamples: audio.length,
      duration: audio.length / sampleRate,
      spectrogram,
      audio: playback,
    },
    [spectrogram.data.buffer, playback.buffer],
  )
}

/** 依次喂入若干音频段，回传事件与进度。 */
function feedSegments(decoder: WasmDecoder, requestId: number, segments: Float32Array[]): void {
  const total = segments.reduce((sum, segment) => sum + segment.length, 0)
  let fed = 0
  for (const segment of segments) {
    for (let offset = 0; offset < segment.length; offset += FEED_CHUNK) {
      const stop = Math.min(offset + FEED_CHUNK, segment.length)
      const events = decoder.pushAudio(segment.subarray(offset, stop)) as DecodeEvent[]
      for (const event of events) post({ type: 'event', requestId, event })
      fed += stop - offset
      post({ type: 'progress', requestId, fedSamples: fed, totalSamples: total })
    }
  }
}

function decodeSelection(
  requestId: number,
  startSample: number | undefined,
  endSample: number | undefined,
  mode: string | undefined,
  anchor: ForcedAnchor,
): void {
  if (!session) throw new Error('尚未载入音频')
  const { sampleRate, audio } = session
  const start = Math.max(0, Math.min(startSample ?? 0, audio.length))
  const end = Math.max(start, Math.min(endSample ?? audio.length, audio.length))

  // 末尾补静音，保证能凑满解码窗口。
  const pad = new Float32Array(Math.round(sampleRate * PAD_SECONDS))
  const startedAt = performance.now()
  const decoder = new WasmDecoder(sampleRate)
  try {
    let segments: Float32Array[]
    if (mode) {
      // 强制模式：只用一个锚点，从锚点一直喂到文件末尾（slowrx 自行截取
      // 标称图像长度）。锚点时间相对“喂入流的第一帧”，即音频起点。
      const anchorSample = anchor === 'end' ? end : start
      const anchorSecs = anchorSample / sampleRate
      decoder.setForcedMode(
        mode,
        anchor === 'start' ? anchorSecs : undefined,
        anchor === 'end' ? anchorSecs : undefined,
      )
      segments = [audio, pad]
      console.log(
        `[worker] 强制解码：模式 ${mode}，锚点 ${anchor} @ ${anchorSecs.toFixed(3)}s，喂入至文件末尾`,
      )
    } else {
      // 自动识模：只喂入选区，避免误识别选区外的 VIS。
      segments = [audio.subarray(start, end), pad]
      console.log(`[worker] 自动识模：选区 ${end - start} 采样 @ ${sampleRate} Hz`)
    }
    feedSegments(decoder, requestId, segments)
    post({ type: 'done', requestId, elapsedMs: performance.now() - startedAt })
    console.log(`[worker] 解码完成，耗时 ${(performance.now() - startedAt).toFixed(0)} ms`)
  } finally {
    decoder.free()
  }
}

/** 建立实时接收会话：常驻解码器（自动识模）+ 流式频谱图。 */
function startLive(requestId: number, sampleRate: number): void {
  if (live) stopLiveInternal()
  const fftSize = pickLiveFftSize(sampleRate)
  const hop = Math.max(1, fftSize >> 1)
  const spec = new StreamingSpectrogram(sampleRate, fftSize, hop, LIVE_MAX_HZ)
  const decoder = new WasmDecoder(sampleRate)
  live = {
    sampleRate,
    decoder,
    spec,
    bins: spec.bins,
    columns: 0,
    images: 0,
    startedAt: performance.now(),
  }
  post({
    type: 'liveStarted',
    requestId,
    sampleRate,
    bins: spec.bins,
    hop: spec.hop,
    secondsPerColumn: spec.secondsPerColumn,
    maxHz: spec.maxHz,
  })
}

/** 推入一块实时音频：喂解码器并产出瀑布图新列。 */
function pushLive(requestId: number, samples: Float32Array): void {
  const current = live
  if (!current) return

  // 1) 常驻解码器：自动识模，收满一张图即产出事件。
  const events = current.decoder.pushAudio(samples) as DecodeEvent[]
  for (const event of events) {
    if (event.type === 'image') current.images++
    post({ type: 'event', requestId, event })
  }

  // 2) 流式频谱图：只回传本批新产生的列。
  const columns = current.spec.push(samples)
  const count = columns.length / current.bins
  if (count > 0) {
    const firstColumn = current.columns
    current.columns += count
    post(
      { type: 'liveColumns', requestId, columns, firstColumn, count, bins: current.bins },
      [columns.buffer],
    )
  }
}

/** 结束实时会话并释放 wasm 资源。 */
function stopLiveInternal(): { elapsedMs: number; imageCount: number } {
  const current = live
  live = null
  if (!current) return { elapsedMs: 0, imageCount: 0 }
  current.decoder.free()
  current.spec.free()
  return { elapsedMs: performance.now() - current.startedAt, imageCount: current.images }
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
        const audio = await synthAudio(message.mode, message.withVis, message.count)
        await storeAndReport(message.requestId, 11025, audio)
        break
      }
      case 'loadAudio': {
        await ensureReady()
        await storeAndReport(message.requestId, message.sampleRate, message.audio)
        break
      }
      case 'decode': {
        await ensureReady()
        decodeSelection(
          message.requestId,
          message.startSample,
          message.endSample,
          message.mode,
          message.anchor ?? 'start',
        )
        break
      }
      case 'liveStart': {
        await ensureReady()
        startLive(message.requestId, message.sampleRate)
        break
      }
      case 'livePush': {
        pushLive(message.requestId, message.samples)
        break
      }
      case 'liveStop': {
        const summary = stopLiveInternal()
        post({ type: 'liveStopped', requestId: message.requestId, ...summary })
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
