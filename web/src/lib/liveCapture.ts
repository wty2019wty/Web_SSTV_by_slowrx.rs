// 麦克风实时采集：getUserMedia → AudioWorklet → 单声道 f32 块回调。
//
// 与解码完全解耦：本类只负责把麦克风音频按固定大小切块交给 `onChunk`，
// 由 App 转发给 Worker 内的常驻 `WasmDecoder`（方案「A 档：流式接收」）。
//
// 关键点：显式关闭回声消除 / 降噪 / 自动增益——这些语音处理会严重破坏
// SSTV 的音频包络，导致无法解码。采集源为空或权限被拒时抛错。

/** Worklet 处理器地址（public/ 下，构建时原样拷贝并随 base 变化）。 */
const WORKLET_URL = `${import.meta.env.BASE_URL}live-capture.worklet.js`

export interface LiveCaptureHandlers {
  /** 每个单声道音频块（长度约为 `chunkSize`）。回调后该数组不应再被持有。 */
  onChunk: (samples: Float32Array) => void
  /** Worklet 层面的运行错误。 */
  onError?: (error: Error) => void
}

/** 采集参数。 */
export interface LiveCaptureOptions {
  /** 指定输入设备（`MediaDeviceInfo.deviceId`）；缺省用系统默认。 */
  deviceId?: string
  /** 每块帧数，缺省 4096（≈85 ms @48 kHz）。 */
  chunkSize?: number
}

/** 枚举可用的音频输入设备（需已授权才能拿到设备名）。 */
export async function listAudioInputs(): Promise<MediaDeviceInfo[]> {
  if (!navigator.mediaDevices?.enumerateDevices) return []
  const devices = await navigator.mediaDevices.enumerateDevices()
  return devices.filter((device) => device.kind === 'audioinput')
}

export class LiveCapture {
  private stream: MediaStream | null = null
  private context: AudioContext | null = null
  private source: MediaStreamAudioSourceNode | null = null
  private node: AudioWorkletNode | null = null
  private sink: GainNode | null = null
  private track: MediaStreamTrack | null = null
  /** 采集所用 AudioContext 的采样率（Hz）。 */
  sampleRate = 0
  active = false

  /**
   * 请求麦克风并开始采集，返回实际采样率。
   *
   * # Errors
   * 浏览器不支持、权限被拒、Worklet 加载失败时抛出。
   */
  async start(
    handlers: LiveCaptureHandlers,
    options: LiveCaptureOptions = {},
  ): Promise<number> {
    if (this.active) throw new Error('麦克风采集已在进行中')
    if (!navigator.mediaDevices?.getUserMedia) {
      throw new Error('当前环境不支持麦克风采集（需要 HTTPS 或 localhost）')
    }

    const audio: MediaTrackConstraints = {
      echoCancellation: false,
      noiseSuppression: false,
      autoGainControl: false,
      channelCount: 1,
    }
    if (options.deviceId) audio.deviceId = { exact: options.deviceId }
    const stream = await navigator.mediaDevices.getUserMedia({ audio })
    const track = stream.getAudioTracks()[0] ?? null

    const context = new AudioContext()
    // 尽早记录采样率：Worklet 可能在 `start()` 返回前就开始回传音频块。
    this.sampleRate = context.sampleRate
    try {
      await context.audioWorklet.addModule(WORKLET_URL)
      const source = context.createMediaStreamSource(stream)
      const node = new AudioWorkletNode(context, 'live-capture', {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        channelCount: 1,
        channelCountMode: 'explicit',
        processorOptions: { chunkSize: options.chunkSize ?? 4096 },
      })
      node.port.onmessage = (event: MessageEvent<Float32Array>) => handlers.onChunk(event.data)
      node.onprocessorerror = () => handlers.onError?.(new Error('AudioWorklet 处理器出错'))

      // 经 0 增益连接到 destination：保证音频图被拉取、Worklet 会运行，
      // 同时不会把麦克风声音回放出来（避免啸叫）。
      const sink = context.createGain()
      sink.gain.value = 0
      source.connect(node)
      node.connect(sink)
      sink.connect(context.destination)
      await context.resume()

      this.stream = stream
      this.context = context
      this.source = source
      this.node = node
      this.sink = sink
      this.track = track
      this.sampleRate = context.sampleRate
      this.active = true
      return context.sampleRate
    } catch (error) {
      stream.getTracks().forEach((track) => track.stop())
      await context.close().catch(() => undefined)
      throw error
    }
  }

  /** 实际生效的音频轨道设置（浏览器可能覆盖请求的约束，如强开 AGC）。 */
  settings(): MediaTrackSettings | null {
    return this.track?.getSettings() ?? null
  }

  /** 输入设备名（首次授权后才有）。 */
  label(): string {
    return this.track?.label ?? ''
  }

  /** 停止采集并释放麦克风与音频上下文。可重复调用。 */
  async stop(): Promise<void> {
    this.active = false
    if (this.node) {
      this.node.port.onmessage = null
      this.node.onprocessorerror = null
    }
    try {
      this.source?.disconnect()
      this.node?.disconnect()
      this.sink?.disconnect()
    } catch {
      /* 节点可能已断开 */
    }
    this.stream?.getTracks().forEach((track) => track.stop())

    const context = this.context
    this.stream = null
    this.context = null
    this.source = null
    this.node = null
    this.sink = null
    this.track = null
    if (context) await context.close().catch(() => undefined)
  }
}
