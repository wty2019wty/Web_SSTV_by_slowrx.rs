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

/** 常见 App 内置浏览器（WebView）的 UA 特征——它们通常不提供麦克风接口。 */
const IN_APP_BROWSER_UA =
  /MicroMessenger|QQ\/|QQBrowser|Weibo|AlipayClient|DingTalk|Feishu|Lark|aweme|BytedanceMicroApp|FBAN|FBAV|Instagram|Line\//i

/** 当前是否运行在 App 的内置浏览器（WebView）中。 */
export function isInAppBrowser(): boolean {
  return typeof navigator !== 'undefined' && IN_APP_BROWSER_UA.test(navigator.userAgent)
}

/**
 * 返回「当前环境根本无法拿到麦克风」的可读原因；环境正常返回 `null`。
 *
 * 与权限成败无关：这里只判断浏览器是否**可能**弹窗（安全上下文、接口存在、
 * 是否为内置 WebView），用于在界面提前给出可执行的指引，而不是等点击后报错。
 */
export function getMicEnvironmentIssue(): string | null {
  if (typeof window !== 'undefined' && !window.isSecureContext) {
    return '当前页面不是安全上下文（需要 HTTPS 或 localhost），浏览器不会开放麦克风。请改用 HTTPS 访问。'
  }
  if (!navigator.mediaDevices?.getUserMedia) {
    if (isInAppBrowser()) {
      return '检测到当前在 App 内置浏览器中打开，它通常不提供麦克风接口。请点右上角「···」选择「在浏览器打开」，改用系统 Safari / Chrome。'
    }
    return '当前浏览器未提供麦克风接口（可能版本过旧，或系统已禁用媒体设备）。请改用最新版 Safari / Chrome。'
  }
  return null
}

/**
 * 查询麦克风权限状态。
 *
 * Permissions API 对 `microphone` 的支持有限（Safari 不支持），无法查询时返回 `'unknown'`。
 */
export async function queryMicPermission(): Promise<PermissionState | 'unknown'> {
  try {
    const status = await navigator.permissions?.query({
      name: 'microphone' as PermissionName,
    })
    return status?.state ?? 'unknown'
  } catch {
    return 'unknown'
  }
}

/** 把 `getUserMedia` 抛出的异常翻译成可执行的中文指引。 */
export function describeMicError(error: unknown): string {
  if (!(error instanceof DOMException)) {
    return error instanceof Error ? error.message : String(error)
  }
  switch (error.name) {
    case 'NotAllowedError':
      return '麦克风权限被拒绝。若之前误点了「阻止」，浏览器不会再弹窗：请点地址栏的权限图标（或系统设置）允许本站使用麦克风，然后刷新页面重试。'
    case 'NotFoundError':
      return '未找到可用的麦克风设备，请确认设备已连接且未被停用。'
    case 'NotReadableError':
      return '无法读取麦克风：可能被其他应用占用，或被系统 / 隐私设置禁用。请关闭占用麦克风的应用后重试。'
    case 'OverconstrainedError':
      return '所选麦克风不可用（可能已拔出或设备标识失效），请点「刷新设备」后重试，或改用默认设备。'
    case 'SecurityError':
      return '出于安全原因浏览器阻止了麦克风访问，请确认使用 HTTPS 访问。'
    case 'AbortError':
      return '麦克风启动被中断，请重试。'
    default:
      return `麦克风启动失败：${error.name}${error.message ? `（${error.message}）` : ''}`
  }
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
    const envIssue = getMicEnvironmentIssue()
    if (envIssue) throw new Error(envIssue)

    const stream = await this.requestStream(options)
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

  /**
   * 请求麦克风音频流：显式关闭回声消除 / 降噪 / 自动增益。
   *
   * 部分移动端浏览器 / 驱动会把基础约束当成严格约束，直接抛 `OverconstrainedError`
   * （此时**不会弹权限框**）。因此逐级放宽：先去掉失效的设备约束，最后退化为
   * 最宽松的 `{ audio: true }`；实际生效的约束由诊断面板显示。
   */
  private async requestStream(options: LiveCaptureOptions): Promise<MediaStream> {
    const audio: MediaTrackConstraints = {
      echoCancellation: false,
      noiseSuppression: false,
      autoGainControl: false,
      channelCount: 1,
    }
    if (options.deviceId) audio.deviceId = { exact: options.deviceId }
    try {
      return await navigator.mediaDevices.getUserMedia({ audio })
    } catch (error) {
      if (!(error instanceof DOMException) || error.name !== 'OverconstrainedError') throw error
      // 指定设备可能已失效（拔出 / deviceId 变化）：去掉后重试。
      if (options.deviceId) {
        delete audio.deviceId
        try {
          return await navigator.mediaDevices.getUserMedia({ audio })
        } catch (retry) {
          if (!(retry instanceof DOMException) || retry.name !== 'OverconstrainedError') throw retry
        }
      }
      // 约束本身不被支持：退化为最宽松的音频请求。
      return await navigator.mediaDevices.getUserMedia({ audio: true })
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
