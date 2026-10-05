import type {
  DecodeEvent,
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
  resolve: (result: DecodeResult) => void
  reject: (error: Error) => void
  events: DecodeEvent[]
  handlers: DecodeHandlers
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
      for (const [id, p] of this.pending) {
        p.reject(new Error(`Worker 错误：${e.message}`))
        this.pending.delete(id)
      }
    }
  }

  private handle(message: WorkerToMain): void {
    // `modes` 由 listModes 的专用监听器处理，这里直接忽略。
    if (message.type === 'modes') return
    const pending = this.pending.get(message.requestId)
    if (!pending) return
    switch (message.type) {
      case 'event':
        pending.events.push(message.event)
        pending.handlers.onEvent?.(message.event)
        break
      case 'progress':
        pending.handlers.onProgress?.(message.fedSamples, message.totalSamples)
        break
      case 'done':
        pending.resolve({ events: pending.events, elapsedMs: message.elapsedMs })
        this.pending.delete(message.requestId)
        break
      case 'error':
        pending.reject(new Error(message.message))
        this.pending.delete(message.requestId)
        break
    }
  }

  private send(message: MainToWorker, transfer?: Transferable[]): void {
    this.worker.postMessage(message, transfer ?? [])
  }

  /** 查询 wasm 侧支持的全部模式。 */
  async listModes(): Promise<ModeInfo[]> {
    return new Promise<ModeInfo[]>((resolve, reject) => {
      const requestId = this.nextId++
      const listener = (e: MessageEvent<WorkerToMain>) => {
        const message = e.data
        if (message.type === 'modes' && message.requestId === requestId) {
          this.worker.removeEventListener('message', listener)
          resolve(message.modes)
        } else if (message.type === 'error' && message.requestId === requestId) {
          this.worker.removeEventListener('message', listener)
          reject(new Error(message.message))
        }
      }
      this.worker.addEventListener('message', listener)
      this.send({ type: 'listModes', requestId })
    })
  }

  private run(message: MainToWorker, transfer: Transferable[] | undefined, handlers: DecodeHandlers): Promise<DecodeResult> {
    return new Promise<DecodeResult>((resolve, reject) => {
      this.pending.set(message.requestId, { resolve, reject, events: [], handlers })
      this.send(message, transfer)
    })
  }

  /** 解码由 wasm 生成的合成音频（开发自测用，需要 dev-synth 构建）。 */
  decodeSynth(mode: string, withVis: boolean, handlers: DecodeHandlers = {}): Promise<DecodeResult> {
    return this.run({ type: 'synth', requestId: this.nextId++, mode, withVis }, undefined, handlers)
  }

  /** 解码一段单声道 f32 音频；`mode` 提供时走强制模式。 */
  decode(
    params: {
      sampleRate: number
      audio: Float32Array
      mode?: string
      startSecs?: number
      endSecs?: number
    },
    handlers: DecodeHandlers = {},
  ): Promise<DecodeResult> {
    // 复制一份，避免 transfer 后调用方的缓冲区被 detach。
    const audio = params.audio.slice()
    return this.run(
      {
        type: 'decode',
        requestId: this.nextId++,
        sampleRate: params.sampleRate,
        audio,
        mode: params.mode,
        startSecs: params.startSecs,
        endSecs: params.endSecs,
      },
      [audio.buffer],
      handlers,
    )
  }

  dispose(): void {
    this.worker.terminate()
    this.pending.clear()
  }
}
