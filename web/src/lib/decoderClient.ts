import type {
  DecodeEvent,
  DecodeProgress,
  ForcedAnchor,
  LiveInfo,
  LiveSummary,
  LoadedInfo,
  MainToWorker,
  ModeInfo,
  WorkerToMain,
} from './protocol'

/** 解码过程中的回调。 */
export interface DecodeHandlers {
  onEvent?: (event: DecodeEvent) => void
  onProgress?: (progress: DecodeProgress) => void
}

/** 实时接收会话的回调。 */
export interface LiveHandlers {
  /** 本批新产生的频谱列（列优先 8 位强度，长度 `count * bins`）。 */
  onColumns?: (columns: Uint8Array, count: number) => void
  /** 实时解码事件（与文件解码同一套事件）。 */
  onEvent?: (event: DecodeEvent) => void
}

/** 一次解码的完整结果。 */
export interface DecodeResult {
  events: DecodeEvent[]
  elapsedMs: number
}

interface Pending {
  resolve: (message: WorkerToMain, events: DecodeEvent[]) => void
  reject: (error: Error) => void
  events: DecodeEvent[]
  handlers?: DecodeHandlers
}

/**
 * 与解码 Worker 通信的薄客户端：把 postMessage 往返包装成 Promise，
 * 并把解码事件按需回放给调用方。
 */
export class DecoderClient {
  private readonly worker: Worker
  private nextId = 1
  private readonly pending = new Map<number, Pending>()

  // 实时会话（同一时刻至多一个）：单独的 id 与回调，不走 pending 的 Promise 往返。
  private liveId = 0
  private liveHandlers: LiveHandlers | null = null
  private liveStart: { resolve: (info: LiveInfo) => void; reject: (error: Error) => void } | null =
    null
  private liveStop: { resolve: (summary: LiveSummary) => void; reject: (error: Error) => void } | null =
    null
  /** 已发出的停止请求；重复调用复用它，避免覆盖 `liveStop` 导致前一 Promise 永不结算。 */
  private liveStopPromise: Promise<LiveSummary> | null = null

  constructor() {
    this.worker = new Worker(new URL('../workers/decoder.worker.ts', import.meta.url), {
      type: 'module',
    })
    this.worker.onmessage = (e: MessageEvent<WorkerToMain>) => this.handle(e.data)
    this.worker.onerror = (e) => {
      // Worker 脚本层面的致命错误（例如 wasm 加载失败）。
      const error = new Error(`Worker 错误：${e.message}`)
      for (const [id, pending] of this.pending) {
        pending.reject(error)
        this.pending.delete(id)
      }
      this.rejectLive(error)
    }
  }

  private handle(message: WorkerToMain): void {
    if (this.liveId !== 0 && message.requestId === this.liveId) {
      this.handleLive(message)
      return
    }
    const pending = this.pending.get(message.requestId)
    if (!pending) return
    switch (message.type) {
      case 'event':
        pending.events.push(message.event)
        pending.handlers?.onEvent?.(message.event)
        break
      case 'progress':
        pending.handlers?.onProgress?.(message)
        break
      case 'modes':
      case 'loaded':
      case 'done':
        pending.resolve(message, pending.events)
        this.pending.delete(message.requestId)
        break
      case 'error':
        pending.reject(new Error(message.message))
        this.pending.delete(message.requestId)
        break
    }
  }

  private handleLive(message: WorkerToMain): void {
    switch (message.type) {
      case 'liveStarted':
        this.liveStart?.resolve({
          sampleRate: message.sampleRate,
          bins: message.bins,
          hop: message.hop,
          secondsPerColumn: message.secondsPerColumn,
          maxHz: message.maxHz,
        })
        this.liveStart = null
        break
      case 'liveColumns':
        this.liveHandlers?.onColumns?.(message.columns, message.count)
        break
      case 'event':
        this.liveHandlers?.onEvent?.(message.event)
        break
      case 'liveStopped': {
        const stop = this.liveStop
        const summary = { elapsedMs: message.elapsedMs, imageCount: message.imageCount }
        this.resetLive()
        stop?.resolve(summary)
        break
      }
      case 'error':
        this.rejectLive(new Error(message.message))
        break
    }
  }

  /** 实时会话出错：拒绝未决的 start/stop 并复位。 */
  private rejectLive(error: Error): void {
    this.liveStart?.reject(error)
    this.liveStop?.reject(error)
    this.resetLive()
  }

  private resetLive(): void {
    this.liveId = 0
    this.liveHandlers = null
    this.liveStart = null
    this.liveStop = null
    this.liveStopPromise = null
  }

  private request(
    message: MainToWorker,
    transfer?: Transferable[],
    handlers?: DecodeHandlers,
  ): Promise<{ message: WorkerToMain; events: DecodeEvent[] }> {
    return new Promise((resolve, reject) => {
      this.pending.set(message.requestId, {
        resolve: (msg, events) => resolve({ message: msg, events }),
        reject,
        events: [],
        handlers,
      })
      this.worker.postMessage(message, transfer ?? [])
    })
  }

  /** 查询 wasm 侧支持的全部模式。 */
  async listModes(): Promise<ModeInfo[]> {
    const { message } = await this.request({ type: 'listModes', requestId: this.nextId++ })
    if (message.type !== 'modes') throw new Error('协议错误：期望 modes')
    return message.modes
  }

  /** 载入 wasm 生成的合成音频（开发自测，需要 dev-synth 构建）。 */
  async loadSynth(mode: string, withVis: boolean, count = 1): Promise<LoadedInfo> {
    const { message } = await this.request({
      type: 'loadSynth',
      requestId: this.nextId++,
      mode,
      withVis,
      count,
    })
    if (message.type !== 'loaded') throw new Error('协议错误：期望 loaded')
    return message
  }

  /** 载入一段单声道 f32 音频（Worker 会保存并计算频谱图）。 */
  async loadAudio(sampleRate: number, audio: Float32Array): Promise<LoadedInfo> {
    // 复制一份，避免 transfer 后调用方的缓冲区被 detach。
    const copy = audio.slice()
    const { message } = await this.request(
      { type: 'loadAudio', requestId: this.nextId++, sampleRate, audio: copy },
      [copy.buffer],
    )
    if (message.type !== 'loaded') throw new Error('协议错误：期望 loaded')
    return message
  }

  /** 解码选区（缺省整段）；`mode` 提供时走强制模式。 */
  async decode(
    params: {
      startSample?: number
      endSample?: number
      mode?: string
      anchor?: ForcedAnchor
      /** 强制模式的失谐兜底值（Hz）。 */
      hedrShiftHz?: number
    },
    handlers?: DecodeHandlers,
  ): Promise<DecodeResult> {
    const { message, events } = await this.request(
      { type: 'decode', requestId: this.nextId++, ...params },
      undefined,
      handlers,
    )
    if (message.type !== 'done') throw new Error('协议错误：期望 done')
    return { events, elapsedMs: message.elapsedMs }
  }

  /**
   * 开始实时接收会话：Worker 建立常驻解码器与流式频谱图。
   *
   * `sampleRate` 为采集的 AudioContext 采样率，直接透传（Worker 内解码器自带
   * 重采样）。
   */
  async startLive(sampleRate: number, handlers: LiveHandlers = {}): Promise<LiveInfo> {
    if (this.liveId !== 0) throw new Error('实时会话已在进行中')
    const id = this.nextId++
    this.liveId = id
    this.liveHandlers = handlers
    return new Promise<LiveInfo>((resolve, reject) => {
      this.liveStart = { resolve, reject }
      this.worker.postMessage({ type: 'liveStart', requestId: id, sampleRate })
    })
  }

  /** 推入一块实时采集的单声道音频（会 transfer 掉 `samples.buffer`）。 */
  pushLive(samples: Float32Array): void {
    if (this.liveId === 0) return
    this.worker.postMessage({ type: 'livePush', requestId: this.liveId, samples }, [samples.buffer])
  }

  /** 结束实时接收会话；返回本次用时与解出的图像数。重复调用复用同一请求。 */
  stopLive(): Promise<LiveSummary> {
    if (this.liveId === 0) return Promise.resolve({ elapsedMs: 0, imageCount: 0 })
    if (this.liveStopPromise) return this.liveStopPromise
    const id = this.liveId
    this.liveStopPromise = new Promise<LiveSummary>((resolve, reject) => {
      this.liveStop = { resolve, reject }
      this.worker.postMessage({ type: 'liveStop', requestId: id })
    })
    return this.liveStopPromise
  }

  dispose(): void {
    this.worker.terminate()
    this.pending.clear()
    this.resetLive()
  }
}
