import type {
  DecodeEvent,
  ForcedAnchor,
  LoadedInfo,
  MainToWorker,
  ModeInfo,
  WorkerToMain,
} from './protocol'

/** 解码过程中的回调。 */
export interface DecodeHandlers {
  onEvent?: (event: DecodeEvent) => void
  onProgress?: (fedSamples: number, totalSamples: number) => void
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
    }
  }

  private handle(message: WorkerToMain): void {
    const pending = this.pending.get(message.requestId)
    if (!pending) return
    switch (message.type) {
      case 'event':
        pending.events.push(message.event)
        pending.handlers?.onEvent?.(message.event)
        break
      case 'progress':
        pending.handlers?.onProgress?.(message.fedSamples, message.totalSamples)
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
  async loadSynth(mode: string, withVis: boolean): Promise<LoadedInfo> {
    const { message } = await this.request({
      type: 'loadSynth',
      requestId: this.nextId++,
      mode,
      withVis,
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
    params: { startSample?: number; endSample?: number; mode?: string; anchor?: ForcedAnchor },
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

  dispose(): void {
    this.worker.terminate()
    this.pending.clear()
  }
}
