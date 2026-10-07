<script setup lang="ts">
// Web SSTV 解码工具主界面。
//
// 流程：载入音频（文件 / 合成）→ Worker 计算频谱图 → 在频谱图上选时间段 →
// 解码选区（默认「自动识模」，可选具体模式强制解码）→ 结果画廊 + 逐张下载。
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
import { DecoderClient } from './lib/decoderClient'
import {
  LiveCapture,
  describeMicError,
  getMicEnvironmentIssue,
  listAudioInputs,
  queryMicPermission,
} from './lib/liveCapture'
import type {
  DecodeEvent,
  DecodeProgress,
  ForcedAnchor,
  LiveInfo,
  LoadedInfo,
  ModeInfo,
  TimeSelection,
} from './lib/protocol'
import SpectrogramView from './components/SpectrogramView.vue'
import WaterfallView from './components/WaterfallView.vue'
import LiveImageView from './components/LiveImageView.vue'

/** 一张已解码图像。 */
interface DecodedImage {
  mode: string
  width: number
  height: number
  dataUrl: string
  /** 是否为不完整图（实时接收中途停止的收尾结果）。 */
  partial: boolean
}

const client = shallowRef<DecoderClient | null>(null)
const modes = ref<ModeInfo[]>([])
/** 合成音频使用的模式。 */
const synthMode = ref('pd120')
/** 解码模式：`auto` = VIS 自动识模；其余为强制模式。 */
const decodeMode = ref('auto')
const forcedAnchor = ref<ForcedAnchor>('start')
/** 强制模式的失谐兜底值（Hz）：窗口内无 VIS 头时用于补偿电台失谐。 */
const forcedHedrShiftHz = ref(0)
const synthWithVis = ref(true)
/** 合成音频重复的图片数（用于验证多图自动识模）。 */
const synthCount = ref(1)

const loaded = ref<LoadedInfo | null>(null)
/** 当前音频来源名称（文件名或「合成音频」），用于在界面上持久展示。 */
const sourceName = ref('')
const selection = ref<TimeSelection>({ start: 0, end: 1 })
const results = ref<DecodedImage[]>([])

const busy = ref(false)
const status = ref('就绪')
/** 任务工作进度 0–1：扫描 + 解码的音频秒口径。进度条宽度与「已完成 %」同源。 */
const progressRatio = ref(0)
const elapsedMs = ref<number | null>(null)
// 进度信息行的实时数据：已用墙钟与预计剩余。
const progressElapsedMs = ref(0)
const progressEtaMs = ref<number | null>(null)
/** 当前任务是否为解码（只有解码才有时间进度可展示）。 */
const timedJob = ref(false)
/** 任务起始时刻（墙钟，performance.now）。 */
let jobStartedAt = 0
/** 进度刷新定时器：让「已用时间」平滑走动（Worker 忙时事件不会更密）。 */
let progressTimer = 0
/** 扫描 / 解码速率（音频秒 ÷ 墙钟秒）。解码速率要等整图爆发开始才测得到。 */
let scanRate = 0
let decodeRate = 0
/** 尚未处理的扫描 / 解码音频秒数（外推预计剩余用）。 */
let remainingScanSeconds = 0
let remainingDecodeSeconds = 0
/** 平滑后的预计剩余毫秒，抑制逐批进度带来的数值跳动。 */
let smoothedEtaMs = 0
const log = ref<string[]>([])

const playing = ref(false)
const playhead = ref(0)

// --- 实时接收（麦克风） -------------------------------------------------
// 常驻解码器 + 流式瀑布图都在 Worker 内；主线程只负责采集、转发与渲染。
/** 音频来源：本地文件 / 麦克风（实时）。 */
const sourceMode = ref<'file' | 'mic'>('file')
const audioInputs = ref<MediaDeviceInfo[]>([])
const selectedInputId = ref('')
const liveActive = ref(false)
const liveBusy = ref(false)
const liveInfo = ref<LiveInfo | null>(null)
const liveStatus = ref('未开始')
const liveElapsed = ref(0)
const liveImageCount = ref(0)
/** 当前环境下麦克风是否不可用（安全上下文 / 接口缺失 / 内置浏览器）的原因，正常为 null。 */
const micEnvIssue = ref<string | null>(null)
/** 麦克风权限状态（Permissions API 不支持时为 unknown）。 */
const micPermission = ref<PermissionState | 'unknown'>('unknown')
const waterfall = ref<InstanceType<typeof WaterfallView> | null>(null)
const liveImage = ref<InstanceType<typeof LiveImageView> | null>(null)
const capture = shallowRef<LiveCapture | null>(null)
let liveTimer = 0
let liveStartedAt = 0
/** 采集早于实时会话建立时暂存的音频块，会话建立后补投，避免丢掉开头。 */
let livePendingChunks: Float32Array[] = []

// 输入诊断：实际生效的音频约束 + 实时电平/削波 + 时间基，用于定位「实时比录音差」。
interface AudioSettings {
  device: string
  sampleRate: number
  /** 麦克风轨道自身采样率（0 = 未知）；与 `sampleRate` 不等说明浏览器在重采样。 */
  trackSampleRate: number
  channels: number
  echoCancellation: boolean
  noiseSuppression: boolean
  autoGainControl: boolean
}
interface LiveLevel {
  peakDb: number
  rmsDb: number
  clipPct: number
  rangeDb: number
}
/** 实测输入速率（累计样本 / 墙钟），偏差大说明设备时钟不准或有丢块。 */
interface LiveRate {
  measured: number
  ppm: number
}
const audioSettings = ref<AudioSettings | null>(null)
const liveLevel = ref<LiveLevel | null>(null)
const liveRate = ref<LiveRate | null>(null)
let diagPeak = 0
let diagSumSq = 0
let diagCount = 0
let diagClips = 0
let diagChunks = 0
let diagQuietDb = Number.POSITIVE_INFINITY
let diagSamples = 0
let diagStartAt = 0

// 实时录音：停止后可一键用离线（文件）路径重解，得到权威结果。
const RECORD_LIMIT_SECONDS = 300
let recordedChunks: Float32Array[] = []
let recordedRate = 0
let recordedSamples = 0
const lastRecording = shallowRef<{ sampleRate: number; audio: Float32Array } | null>(null)
const redecodeBusy = ref(false)

function onOff(value: boolean): string {
  return value ? '开' : '关'
}

/** 累积一块音频的电平统计，约每秒刷新一次诊断面板。 */
function updateLevelDiag(samples: Float32Array) {
  let peak = 0
  let sumSq = 0
  let clips = 0
  for (let i = 0; i < samples.length; i++) {
    const v = samples[i]
    const a = v < 0 ? -v : v
    if (a > peak) peak = a
    sumSq += v * v
    if (a >= 0.999) clips++
  }
  diagPeak = Math.max(diagPeak, peak)
  diagSumSq += sumSq
  diagCount += samples.length
  diagClips += clips
  diagChunks++
  if (diagStartAt === 0) diagStartAt = performance.now()
  diagSamples += samples.length
  if (diagChunks < 12 || diagCount === 0) return // 4096 帧/块 ≈ 12 块/秒
  const peakDb = 20 * Math.log10(Math.max(diagPeak, 1e-6))
  const rms = Math.sqrt(diagSumSq / diagCount)
  const rmsDb = 20 * Math.log10(Math.max(rms, 1e-6))
  if (rmsDb < diagQuietDb) diagQuietDb = rmsDb
  liveLevel.value = {
    peakDb,
    rmsDb,
    clipPct: (diagClips / diagCount) * 100,
    rangeDb: peakDb - diagQuietDb,
  }
  // 实测输入速率 = 累计样本 / 墙钟；与标称速率的偏差（ppm）即时间基误差，
  // 同时也会暴露采集链路的丢块（偏差显著为负）。
  const nominal = capture.value?.sampleRate ?? 0
  const elapsedSecs = (performance.now() - diagStartAt) / 1000
  if (nominal > 0 && elapsedSecs > 0) {
    const measured = diagSamples / elapsedSecs
    liveRate.value = { measured, ppm: (measured / nominal - 1) * 1e6 }
  }
  diagPeak = 0
  diagSumSq = 0
  diagCount = 0
  diagClips = 0
  diagChunks = 0
}

/** 采集到一块音频：记录（供离线重解）+ 电平诊断，再转发给 Worker。 */
function onLiveChunk(samples: Float32Array) {
  const clientValue = client.value
  if (!clientValue) return
  if (!recordedRate) recordedRate = capture.value?.sampleRate ?? 0
  if (recordedRate > 0 && recordedSamples < recordedRate * RECORD_LIMIT_SECONDS) {
    recordedChunks.push(samples.slice())
    recordedSamples += samples.length
  }
  updateLevelDiag(samples)
  if (liveActive.value) clientValue.pushLive(samples)
  else livePendingChunks.push(samples)
}

/** 切换音频来源；离开麦克风时停止正在进行的接收。 */
function selectSource(mode: 'file' | 'mic') {
  if (mode === sourceMode.value) return
  if (mode === 'file' && liveActive.value) void stopLiveReceive()
  sourceMode.value = mode
  if (mode === 'mic') void checkMicEnvironment()
}

/** 重新枚举麦克风设备（首次授权后设备名才可用）。 */
async function refreshAudioInputs() {
  const devices = await listAudioInputs()
  audioInputs.value = devices
  if (!devices.some((device) => device.deviceId === selectedInputId.value)) {
    selectedInputId.value = devices[0]?.deviceId ?? ''
  }
}

/** 检测麦克风运行环境（安全上下文 / 接口 / 内置 WebView）与权限状态。 */
async function checkMicEnvironment() {
  const issue = getMicEnvironmentIssue()
  micEnvIssue.value = issue
  micPermission.value = issue ? 'unknown' : await queryMicPermission()
}

/** 设备插拔时刷新列表。 */
function onDeviceChange() {
  void refreshAudioInputs()
}

const isForced = computed(() => decodeMode.value !== 'auto')
const synthModeInfo = computed(() => modes.value.find((m) => m.shortName === synthMode.value))
/** 麦克风权限状态的中文描述，直接显示在界面上以便确认是否已授权。 */
const micPermissionText = computed(() => {
  switch (micPermission.value) {
    case 'granted':
      return '已允许'
    case 'denied':
      return '已拒绝（不会再自动弹窗）'
    case 'prompt':
      return '未授权（点「开始接收」时申请）'
    default:
      return '未知（该浏览器不支持查询）'
  }
})
const durationText = computed(() =>
  loaded.value ? `${loaded.value.duration.toFixed(2)}s @ ${loaded.value.sampleRate} Hz` : '—',
)

function pushLog(line: string) {
  const time = new Date().toLocaleTimeString()
  log.value = [`[${time}] ${line}`, ...log.value].slice(0, 80)
}

function handleEvent(event: DecodeEvent) {
  switch (event.type) {
    case 'vis':
      pushLog(`检测到 VIS：${event.mode}（失谐 ${event.hedrShiftHz.toFixed(1)} Hz）`)
      break
    case 'unknownVis':
      pushLog(`未知 VIS 码：0x${event.code.toString(16)}`)
      break
    case 'mistuning': {
      const hz = `${event.hedrShiftHz >= 0 ? '+' : ''}${event.hedrShiftHz.toFixed(1)} Hz`
      pushLog(event.fromVis ? `VIS 自动补偿失谐：${hz}` : `使用失谐兜底：${hz}`)
      break
    }
    case 'image':
      addResult(event)
      pushLog(`图像完成：${event.mode} ${event.width}×${event.height}`)
      break
    default:
      break
  }
}

/** 把一张解码结果转成 PNG dataURL 存入画廊（自动模式可能一次得到多张）。 */
function addResult(event: Extract<DecodeEvent, { type: 'image' }>) {
  const canvas = document.createElement('canvas')
  canvas.width = event.width
  canvas.height = event.height
  const ctx = canvas.getContext('2d')
  if (!ctx) return
  // 复制成带 ArrayBuffer 的 Uint8ClampedArray，满足 ImageData 的类型要求。
  ctx.putImageData(new ImageData(new Uint8ClampedArray(event.rgba), event.width, event.height), 0, 0)
  results.value.push({
    mode: event.mode,
    width: event.width,
    height: event.height,
    dataUrl: canvas.toDataURL('image/png'),
    partial: event.partial,
  })
}

function beginJob(message: string, timed = false) {
  window.clearInterval(progressTimer)
  busy.value = true
  progressRatio.value = 0
  progressElapsedMs.value = 0
  progressEtaMs.value = null
  scanRate = 0
  decodeRate = 0
  remainingScanSeconds = 0
  remainingDecodeSeconds = 0
  smoothedEtaMs = 0
  timedJob.value = timed
  jobStartedAt = performance.now()
  // 只有解码任务才需要时间进度；其余任务（载入、合成）不启动定时器。
  if (timed) progressTimer = window.setInterval(tickProgress, 100)
  elapsedMs.value = null
  status.value = message
}

/** 刷新「已用时间」，并按实测的扫描 / 解码速率外推「预计剩余」。 */
function tickProgress() {
  progressElapsedMs.value = performance.now() - jobStartedAt
  // 整图爆发开始前测不到解码速率（扫描几乎不花时间），此时无法诚实估计，
  // 宁可不显示也不编数字——上一版「预计剩余 0.0s」就是这么骗人的。
  if (decodeRate <= 0) {
    progressEtaMs.value = null
    smoothedEtaMs = 0
    return
  }
  const decodeMs = (remainingDecodeSeconds / decodeRate) * 1000
  const scanMs = scanRate > 0 ? (remainingScanSeconds / scanRate) * 1000 : 0
  const raw = decodeMs + scanMs
  smoothedEtaMs = smoothedEtaMs > 0 ? smoothedEtaMs * 0.7 + raw * 0.3 : raw
  progressEtaMs.value = Math.max(0, smoothedEtaMs)
}

function endJob(result: { elapsedMs: number }) {
  window.clearInterval(progressTimer)
  progressRatio.value = 1
  elapsedMs.value = result.elapsedMs
  status.value = `完成（${result.elapsedMs.toFixed(0)} ms，${results.value.length} 张）`
  busy.value = false
}

function failJob(error: unknown) {
  window.clearInterval(progressTimer)
  status.value = `失败：${error instanceof Error ? error.message : String(error)}`
  busy.value = false
}

function onProgress(update: DecodeProgress) {
  // 进度 =（已扫描 + 已解码）÷（待扫描 + 已知图像窗口），两段都按音频秒计。
  // 扫描阶段推进得快是符合事实的：喂入几乎不花 CPU，真正的耗时在整图爆发里。
  const workDone = update.scannedSeconds + update.decodedSeconds
  const workTotal = update.scanSpanSeconds + update.decodeSpanSeconds
  // 单调不回退：中途才发现新图（分母随之变大）时进度条保持，不倒退。
  const ratio = workTotal > 0 ? Math.min(1, workDone / workTotal) : 0
  progressRatio.value = Math.max(progressRatio.value, ratio)
  remainingScanSeconds = Math.max(0, update.scanSpanSeconds - update.scannedSeconds)
  remainingDecodeSeconds = Math.max(0, update.decodeSpanSeconds - update.decodedSeconds)
  scanRate = update.scanMs > 0 ? update.scannedSeconds / (update.scanMs / 1000) : 0
  decodeRate =
    update.decodeMs > 0 && update.decodedSeconds > 0
      ? update.decodedSeconds / (update.decodeMs / 1000)
      : 0
  status.value =
    update.decodedSeconds > 0
      ? `正在解码图像（已解码 ${update.decodedSeconds.toFixed(1)}s / ${update.decodeSpanSeconds.toFixed(1)}s）…`
      : '正在扫描音频…'
}

/** 进度条下方的时间信息行：文档时间轴上的位置 + 完成度 + 已用 / 预计剩余。 */
const progressMeta = computed(() => {
  const duration = loaded.value?.duration ?? 0
  const percent = Math.min(100, Math.floor(progressRatio.value * 100))
  const parts = [
    `音频 ${(progressRatio.value * duration).toFixed(1)}s / ${duration.toFixed(1)}s`,
    `已完成 ${percent}%`,
    `已用 ${(progressElapsedMs.value / 1000).toFixed(1)}s`,
  ]
  if (progressEtaMs.value !== null) {
    parts.push(`预计剩余 ${(progressEtaMs.value / 1000).toFixed(1)}s`)
  }
  return parts.join(' · ')
})

// --- 实时接收（麦克风） -------------------------------------------------

/** 实时解码事件：逐行绘制实时图像，图像计入实时计数，其余复用文件解码处理。 */
function handleLiveEvent(event: DecodeEvent) {
  switch (event.type) {
    case 'vis':
      // 新一张图的 VIS：清空实时图像，准备逐行绘制。
      liveImage.value?.clear()
      handleEvent(event)
      break
    case 'line': {
      if (event.lineIndex === 0) {
        const meta = modes.value.find((m) => m.shortName === event.mode)
        const width = meta?.width ?? Math.floor(event.rgb.length / 3)
        const height = meta?.height ?? 0
        if (width > 0 && height > 0) liveImage.value?.begin(event.mode, width, height)
      }
      liveImage.value?.pushLine(event.lineIndex, event.rgb)
      break
    }
    case 'image':
      liveImageCount.value++
      handleEvent(event)
      break
    default:
      handleEvent(event)
      break
  }
}

/** 停止并释放麦克风采集。 */
async function stopCapture() {
  window.clearInterval(liveTimer)
  liveTimer = 0
  const cap = capture.value
  capture.value = null
  if (cap) await cap.stop().catch(() => undefined)
}

async function startLiveReceive() {
  const clientValue = client.value
  if (!clientValue || liveBusy.value || liveActive.value) return
  liveBusy.value = true
  liveStatus.value = '正在请求麦克风…'
  pushLog('正在请求麦克风权限…')
  livePendingChunks = []
  results.value = []
  liveImageCount.value = 0
  liveImage.value?.clear()
  recordedChunks = []
  recordedSamples = 0
  recordedRate = 0
  diagPeak = 0
  diagSumSq = 0
  diagCount = 0
  diagClips = 0
  diagChunks = 0
  diagQuietDb = Number.POSITIVE_INFINITY
  diagSamples = 0
  diagStartAt = 0
  liveLevel.value = null
  liveRate.value = null
  audioSettings.value = null
  try {
    const cap = new LiveCapture()
    capture.value = cap
    const sampleRate = await cap.start(
      {
        onChunk: onLiveChunk,
        onError: (error) => pushLog(`采集错误：${error.message}`),
      },
      { deviceId: selectedInputId.value || undefined },
    )
    // 授权成功：清掉环境告警并记录权限状态。
    micPermission.value = 'granted'
    micEnvIssue.value = null
    // 实际生效的约束（浏览器可能覆盖请求；用于诊断 AGC/降噪是否被强开）。
    const settings = cap.settings()
    audioSettings.value = {
      device: cap.label() || '默认设备',
      sampleRate,
      trackSampleRate: cap.trackRate,
      channels: settings?.channelCount ?? 1,
      echoCancellation: settings?.echoCancellation ?? false,
      noiseSuppression: settings?.noiseSuppression ?? false,
      autoGainControl: settings?.autoGainControl ?? false,
    }
    // 首次授权后设备名才可见，刷新一次下拉选项。
    void refreshAudioInputs()
    const info = await clientValue.startLive(sampleRate, {
      onColumns: (columns, count) => waterfall.value?.push(columns, count),
      onEvent: handleLiveEvent,
    })
    liveInfo.value = info
    liveActive.value = true
    liveStartedAt = performance.now()
    liveElapsed.value = 0
    liveTimer = window.setInterval(() => {
      liveElapsed.value = (performance.now() - liveStartedAt) / 1000
    }, 250)
    // 补投建立会话前暂存的音频块（会话建立通常只需几毫秒）。
    for (const chunk of livePendingChunks) clientValue.pushLive(chunk)
    livePendingChunks = []
    liveStatus.value = `实时接收中（${sampleRate} Hz，${(info.secondsPerColumn * 1000).toFixed(0)} ms/列）`
    pushLog(`开始实时接收：${sampleRate} Hz`)
  } catch (error) {
    await stopCapture()
    if (error instanceof DOMException && error.name === 'NotAllowedError') {
      micPermission.value = 'denied'
    }
    const env = getMicEnvironmentIssue()
    micEnvIssue.value = env
    // 优先展示环境级原因（内置浏览器 / 非 HTTPS）；否则翻译 getUserMedia 异常。
    liveStatus.value = `启动失败：${env ?? describeMicError(error)}`
    pushLog(`麦克风启动失败：${error instanceof Error ? error.message : String(error)}`)
  } finally {
    liveBusy.value = false
  }
}

async function stopLiveReceive() {
  const clientValue = client.value
  if (!liveActive.value || !clientValue) return
  liveBusy.value = true
  try {
    await stopCapture()
    saveRecording()
    const summary = await clientValue.stopLive()
    liveImageCount.value = summary.imageCount
    liveStatus.value = `已停止：解出 ${summary.imageCount} 张，用时 ${(summary.elapsedMs / 1000).toFixed(1)}s`
    pushLog(`停止实时接收：${summary.imageCount} 张`)
  } catch (error) {
    liveStatus.value = `停止失败：${error instanceof Error ? error.message : String(error)}`
  } finally {
    liveActive.value = false
    liveInfo.value = null
    livePendingChunks = []
    liveBusy.value = false
  }
}

/** 把已录的实时音频拼成一段，供离线重解。 */
function saveRecording() {
  if (recordedChunks.length === 0 || recordedRate <= 0) return
  const total = recordedChunks.reduce((sum, chunk) => sum + chunk.length, 0)
  const audio = new Float32Array(total)
  let offset = 0
  for (const chunk of recordedChunks) {
    audio.set(chunk, offset)
    offset += chunk.length
  }
  lastRecording.value = { sampleRate: recordedRate, audio }
  recordedChunks = []
  recordedSamples = 0
}

/** 把刚录下的实时音频载入文件解码路径，自动识模重解整段。 */
async function redecodeRecording() {
  const rec = lastRecording.value
  const clientValue = client.value
  if (!rec || !clientValue || busy.value || liveActive.value || redecodeBusy.value) return
  redecodeBusy.value = true
  beginJob('正在载入录音并准备离线解码…', true)
  try {
    const info = await clientValue.loadAudio(rec.sampleRate, rec.audio)
    applyLoaded(info)
    sourceMode.value = 'file'
    sourceName.value = '实时接收录音'
    status.value = '正在用离线路径重解整段录音…'
    const result = await clientValue.decode(
      { startSample: 0, endSample: rec.audio.length },
      { onEvent: handleEvent, onProgress },
    )
    endJob(result)
    pushLog(`离线重解录音：${results.value.length} 张`)
  } catch (error) {
    failJob(error)
  } finally {
    busy.value = false
    redecodeBusy.value = false
  }
}

function toggleLiveReceive() {
  if (liveActive.value) void stopLiveReceive()
  else void startLiveReceive()
}

async function loadSynth() {
  const clientValue = client.value
  if (!clientValue || busy.value) return
  beginJob('正在生成合成音频并计算频谱图…')
  try {
    const info = await clientValue.loadSynth(synthMode.value, synthWithVis.value, synthCount.value)
    applyLoaded(info)
    sourceName.value = '合成音频'
    status.value = `已载入合成音频（${info.duration.toFixed(1)}s）`
  } catch (error) {
    failJob(error)
  } finally {
    busy.value = false
  }
}

/** 打开选择框前清空原生 input：这样重复选择同一个文件也能触发 change，
 *  同时选完后 input 会保留并显示所选文件名。 */
function onFileClick(event: Event) {
  ;(event.target as HTMLInputElement).value = ''
}

async function onFileChange(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file || !client.value || busy.value) return
  // 立即记录所选文件名，避免原生 file input 被重置后无处显示。
  const previousName = sourceName.value
  sourceName.value = file.name
  beginJob(`正在解析 ${file.name}…`)
  try {
    const arrayBuffer = await file.arrayBuffer()
    const audioContext = new AudioContext()
    const buffer = await audioContext.decodeAudioData(arrayBuffer)
    const mono = toMono(buffer)
    await audioContext.close()
    const info = await client.value.loadAudio(buffer.sampleRate, mono)
    applyLoaded(info)
    status.value = `已载入 ${file.name}（${info.duration.toFixed(1)}s @ ${info.sampleRate} Hz）`
  } catch (error) {
    // 载入失败：回退来源显示，保持与实际已载入音频一致。
    sourceName.value = previousName
    failJob(error)
  } finally {
    // 不再清空 input.value：保留原生 input 显示的文件名（见 onFileClick）。
    busy.value = false
  }
}

function applyLoaded(info: LoadedInfo) {
  finishPlayback()
  sourceBuffer = null
  loaded.value = info
  selection.value = { start: 0, end: info.duration }
  playhead.value = 0
  results.value = []
}

async function decodeSelection() {
  const clientValue = client.value
  if (!clientValue || busy.value || liveActive.value || !loaded.value) return
  const rate = loaded.value.sampleRate
  // 强制模式直接用播放头位置当锚点；自动识模用范围选区。
  const anchorTime = playhead.value
  const startSample = Math.round((isForced.value ? anchorTime : selection.value.start) * rate)
  const endSample = Math.round((isForced.value ? anchorTime : selection.value.end) * rate)
  if (!isForced.value && endSample <= startSample) {
    status.value = '选区为空'
    return
  }
  results.value = []
  beginJob(isForced.value ? '正在强制解码…' : '正在自动识模解码…', true)
  try {
    const result = await clientValue.decode(
      {
        startSample,
        endSample,
        mode: isForced.value ? decodeMode.value : undefined,
        anchor: isForced.value ? forcedAnchor.value : undefined,
        hedrShiftHz: isForced.value ? forcedHedrShiftHz.value : undefined,
      },
      { onEvent: handleEvent, onProgress },
    )
    endJob(result)
  } catch (error) {
    failJob(error)
  }
}

/** 多声道取平均，得到单声道 f32。 */
function toMono(buffer: AudioBuffer): Float32Array {
  const channels = buffer.numberOfChannels
  if (channels === 1) return buffer.getChannelData(0).slice()
  const out = new Float32Array(buffer.length)
  for (let ch = 0; ch < channels; ch++) {
    const data = buffer.getChannelData(ch)
    for (let i = 0; i < data.length; i++) out[i] += data[i]
  }
  for (let i = 0; i < out.length; i++) out[i] /= channels
  return out
}

// --- 音频播放 -----------------------------------------------------------
// 用 Web Audio API 播放主线程持有的 PCM 副本；播放头随播放推进，
// 并在频谱图上实时显示（点击频谱图可定位）。
let audioContext: AudioContext | null = null
let sourceNode: AudioBufferSourceNode | null = null
let sourceBuffer: AudioBuffer | null = null
let playbackOffset = 0
let playbackStartCtxTime = 0
let playbackLimit = Number.POSITIVE_INFINITY
let animationHandle = 0

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value))
}

function finishPlayback(): void {
  cancelAnimationFrame(animationHandle)
  if (sourceNode) {
    try {
      sourceNode.stop()
    } catch {
      /* 可能已自然结束 */
    }
    sourceNode.disconnect()
    sourceNode = null
  }
  playing.value = false
}

function startPlayback(from: number, limit: number): void {
  const info = loaded.value
  if (!info) return
  finishPlayback()
  audioContext ??= new AudioContext()
  const ctx = audioContext
  void ctx.resume()
  if (!sourceBuffer || sourceBuffer.length !== info.audio.length) {
    sourceBuffer = ctx.createBuffer(1, info.audio.length, info.sampleRate)
    sourceBuffer.getChannelData(0).set(info.audio)
  }
  const node = ctx.createBufferSource()
  node.buffer = sourceBuffer
  node.connect(ctx.destination)
  const offset = clamp(from, 0, info.duration)
  const stopAt = clamp(limit, offset, info.duration)
  const duration = stopAt - offset
  if (duration > 0.005) node.start(0, offset, duration)
  else node.start(0, offset)
  sourceNode = node
  playbackOffset = offset
  playbackLimit = stopAt
  playbackStartCtxTime = ctx.currentTime
  playing.value = true
  node.onended = () => {
    if (sourceNode === node) {
      playhead.value = playbackLimit
      finishPlayback()
    }
  }
  animationHandle = requestAnimationFrame(updatePlayhead)
}

function updatePlayhead(): void {
  if (!playing.value || !audioContext) return
  const t = playbackOffset + (audioContext.currentTime - playbackStartCtxTime)
  playhead.value = Math.min(t, playbackLimit)
  if (t >= playbackLimit) {
    finishPlayback()
    return
  }
  animationHandle = requestAnimationFrame(updatePlayhead)
}

function togglePlay(): void {
  if (playing.value) finishPlayback()
  else startPlayback(playhead.value, loaded.value?.duration ?? 0)
}

function playSelection(): void {
  if (!loaded.value) return
  if (isForced.value) {
    // 强制模式：播放由锚点与模式标称时长推出的图像区间。
    const modeInfo = modes.value.find((m) => m.shortName === decodeMode.value)
    const imageSeconds = modeInfo?.imageSeconds ?? 0
    const time = playhead.value
    if (forcedAnchor.value === 'start') {
      startPlayback(time, clamp(time + imageSeconds, 0, loaded.value.duration))
    } else {
      startPlayback(clamp(time - imageSeconds, 0, loaded.value.duration), time)
    }
    return
  }
  startPlayback(selection.value.start, selection.value.end)
}

function seekTo(time: number): void {
  if (!loaded.value) return
  playhead.value = clamp(time, 0, loaded.value.duration)
  if (playing.value) startPlayback(playhead.value, loaded.value.duration)
}

onMounted(async () => {
  const value = new DecoderClient()
  client.value = value
  try {
    modes.value = await value.listModes()
    if (modes.value.length > 0) {
      // 默认合成模式固定 PD-120（与上方初值一致），不随模式表顺序变化。
      synthMode.value = modes.value.some((m) => m.shortName === 'pd120')
        ? 'pd120'
        : modes.value[0].shortName
    }
    pushLog(`已加载 ${modes.value.length} 个模式`)
  } catch (error) {
    status.value = `初始化失败：${error instanceof Error ? error.message : String(error)}`
  }
  await refreshAudioInputs()
  await checkMicEnvironment()
  navigator.mediaDevices?.addEventListener('devicechange', onDeviceChange)
})

onBeforeUnmount(() => {
  finishPlayback()
  window.clearInterval(liveTimer)
  window.clearInterval(progressTimer)
  navigator.mediaDevices?.removeEventListener('devicechange', onDeviceChange)
  void audioContext?.close()
  void capture.value?.stop()
  client.value?.dispose()
})
</script>

<template>
  <main class="app">
    <h1>Web SSTV 解码工具</h1>
    <p class="subtitle">解码核心为 slowrx（Rust → WASM），全部在浏览器本地完成，不上传音频。</p>

    <section class="panel">
      <h2>1. 音频来源</h2>
      <div class="row source-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          :class="{ active: sourceMode === 'file' }"
          :disabled="liveActive"
          @click="selectSource('file')"
        >
          📁 本地文件
        </button>
        <button
          type="button"
          role="tab"
          :class="{ active: sourceMode === 'mic' }"
          :disabled="busy"
          @click="selectSource('mic')"
        >
          🎙 麦克风
        </button>
      </div>

      <template v-if="sourceMode === 'file'">
        <div class="row">
          <label class="file">
            <span class="file-text">选择音频文件</span>
            <input
              type="file"
              accept="audio/*"
              :disabled="busy || liveActive"
              @click="onFileClick"
              @change="onFileChange"
            />
          </label>
          <span class="hint">已载入 {{ durationText }}</span>
        </div>
        <p class="source" :class="{ empty: !sourceName }" :title="sourceName">
          <span class="source-label">当前音频：</span>{{ sourceName || '尚未选择' }}
        </p>
        <details class="devtools">
          <summary>开发测试：生成合成音频</summary>
          <div class="row">
            <label>
              合成模式
              <select v-model="synthMode" :disabled="busy || liveActive">
                <option v-for="mode in modes" :key="mode.shortName" :value="mode.shortName">
                  {{ mode.name }}（{{ mode.width }}×{{ mode.height }}）
                </option>
              </select>
            </label>
            <label class="check">
              <input v-model="synthWithVis" type="checkbox" :disabled="busy || liveActive" />
              包含 VIS 头
            </label>
            <label>
              图片数
              <input
                v-model.number="synthCount"
                type="number"
                min="1"
                max="20"
                :disabled="busy || liveActive"
              />
            </label>
            <button :disabled="busy || liveActive" @click="loadSynth">生成合成音频</button>
          </div>
          <p class="hint">
            用 slowrx 的合成编码器生成已知内容的 SSTV 音频，便于在没有真实录音时验证解码。
            <template v-if="synthModeInfo">
              {{ synthModeInfo.name }} 标称图像时长约 {{ synthModeInfo.imageSeconds.toFixed(1) }} 秒。
            </template>
            需要 dev-synth 构建（<code>npm run wasm</code>），生产构建下不可用。
          </p>
        </details>
      </template>

      <template v-else>
        <p class="notice-warn" role="note">
          <strong>⚠ 实验功能 · 开发测试阶段</strong
          >：麦克风实时解码的效果还不理想（同步、识模、抗噪仍在调优），
          出图可能错位、缺行、串色甚至整段解不出来。
          需要可靠结果时，请用「📁 本地文件」走离线解码。
        </p>
        <div class="row">
          <label v-if="audioInputs.length > 0">
            输入设备
            <select v-model="selectedInputId" :disabled="liveActive || liveBusy">
              <option
                v-for="(device, index) in audioInputs"
                :key="device.deviceId"
                :value="device.deviceId"
              >
                {{ device.label || `麦克风 ${index + 1}（设备名未公开）` }}
              </option>
            </select>
          </label>
          <button :disabled="liveBusy || busy" @click="toggleLiveReceive">
            {{ liveActive ? '⏹ 停止接收' : '🎙 开始接收' }}
          </button>
          <button :disabled="liveBusy || liveActive" @click="refreshAudioInputs">刷新设备</button>
          <span class="hint">权限：{{ micPermissionText }}</span>
          <span v-if="liveActive" class="hint">
            已接收 {{ liveElapsed.toFixed(1) }}s · 解出 {{ liveImageCount }} 张
          </span>
          <span v-else class="hint">从麦克风实时解码，边收边出图（自动识模）</span>
        </div>
        <p v-if="micEnvIssue" class="hint warn">{{ micEnvIssue }}</p>
        <p v-else-if="micPermission === 'denied'" class="hint warn">
          浏览器已拒绝本站的麦克风权限，不会再自动弹窗：请点地址栏的权限图标改为「允许」后刷新页面。
        </p>
        <p v-else-if="audioInputs.length === 0" class="hint">
          未检测到麦克风设备（或尚未授权）：首次点击「开始接收」会弹出麦克风授权。
        </p>
        <p class="live-status">{{ liveStatus }}</p>
        <WaterfallView
          ref="waterfall"
          :bins="liveInfo?.bins ?? 0"
          :max-hz="liveInfo?.maxHz ?? 4000"
          :seconds-per-column="liveInfo?.secondsPerColumn ?? 0"
        />
        <LiveImageView ref="liveImage" />

        <details class="devtools" :open="liveActive || !!audioSettings">
          <summary>输入诊断（定位实时质量）</summary>
          <div v-if="audioSettings" class="diag">
            <div class="diag-row">
              <span>设备</span><span>{{ audioSettings.device || '默认设备' }}</span>
            </div>
            <div class="diag-row">
              <span>采样率 / 声道</span>
              <span
                :class="{
                  warn:
                    audioSettings.trackSampleRate > 0 &&
                    audioSettings.trackSampleRate !== audioSettings.sampleRate,
                }"
              >
                {{ audioSettings.sampleRate }} Hz · {{ audioSettings.channels }} ch
                <template
                  v-if="
                    audioSettings.trackSampleRate > 0 &&
                    audioSettings.trackSampleRate !== audioSettings.sampleRate
                  "
                >
                  （轨道 {{ audioSettings.trackSampleRate }} Hz，浏览器重采样中）
                </template>
              </span>
            </div>
            <div v-if="liveRate" class="diag-row">
              <span>时间基</span>
              <span :class="{ warn: Math.abs(liveRate.ppm) > 500 }">
                实测 {{ liveRate.measured.toFixed(1) }} Hz · 偏差
                {{ (liveRate.ppm >= 0 ? '+' : '') + liveRate.ppm.toFixed(0) }} ppm
              </span>
            </div>
            <div class="diag-row">
              <span>音频处理</span>
              <span
                :class="{
                  warn:
                    audioSettings.echoCancellation ||
                    audioSettings.noiseSuppression ||
                    audioSettings.autoGainControl,
                }"
              >
                <template
                  v-if="
                    audioSettings.echoCancellation ||
                    audioSettings.noiseSuppression ||
                    audioSettings.autoGainControl
                  "
                  >⚠ </template
                >回声消除 {{ onOff(audioSettings.echoCancellation) }} · 降噪
                {{ onOff(audioSettings.noiseSuppression) }} · 自动增益
                {{ onOff(audioSettings.autoGainControl) }}
              </span>
            </div>
            <div v-if="liveLevel" class="diag-row">
              <span>电平</span>
              <span :class="{ warn: liveLevel.clipPct > 0.01 || liveLevel.peakDb < -30 }">
                峰值 {{ liveLevel.peakDb.toFixed(1) }} dBFS · RMS
                {{ liveLevel.rmsDb.toFixed(1) }} dBFS · 削波 {{ liveLevel.clipPct.toFixed(2) }}% ·
                动态范围 {{ liveLevel.rangeDb.toFixed(0) }} dB
              </span>
            </div>
            <p class="hint">
              理想情况：音频处理全为「关」。若「自动增益」为开，说明浏览器/驱动覆盖了请求，
              会破坏 SSTV 的频率调制；若削波 &gt; 0 或峰值贴近 0 dBFS，请调低麦克风增益；
              若峰值低于约 -30 dBFS，请调高。时间基偏差（ppm）反映设备时钟与标称采样率的
              差异：数百 ppm 以内可由同步斜率修正吸收，超过约 ±1000 ppm 或持续为负
              （丢块）时图像会倾斜、行错位，建议在系统声音设置里把输入设备采样率固定为
              与上表一致（如 48000 Hz）。
            </p>
          </div>
          <p v-else class="hint">开始接收后显示实际生效的音频约束与实时电平。</p>
        </details>

        <div class="row">
          <button
            :disabled="!lastRecording || busy || liveActive || redecodeBusy"
            @click="redecodeRecording"
          >
            ⏮ 用离线路径重解录音{{
              lastRecording
                ? `（${(lastRecording.audio.length / lastRecording.sampleRate).toFixed(0)}s）`
                : ''
            }}
          </button>
          <span class="hint">把刚录下的实时音频载入文件解码路径重解，得到权威结果（同源对比）。</span>
        </div>

        <p class="hint">
          已请求关闭回声消除/降噪/自动增益，避免破坏 SSTV 信号。
          检测到 VIS 后会逐行绘制，整张收完（或点停止）再用完整同步重解一遍，
          结果同时加入下方「解码结果」画廊。
        </p>
      </template>
    </section>

    <section class="panel">
      <h2>2. 频谱图与时间选区</h2>
      <div class="row">
        <button :disabled="!loaded" @click="togglePlay">{{ playing ? '⏸ 暂停' : '▶ 播放' }}</button>
        <button :disabled="!loaded" @click="playSelection">▶ 播放选区</button>
        <span class="hint">播放头 {{ playhead.toFixed(2) }}s · 点击频谱图可定位</span>
      </div>
      <SpectrogramView
        v-model="selection"
        :spectrogram="loaded?.spectrogram ?? null"
        :duration="loaded?.duration ?? 1"
        :playhead="playhead"
        :follow="playing"
        :interaction="isForced ? 'playhead' : 'range'"
        @seek="seekTo"
      />
    </section>

    <section class="panel">
      <h2>3. 解码</h2>
      <div class="row">
        <label>
          解码模式
          <select v-model="decodeMode" :disabled="busy">
            <option value="auto">自动识模（推荐）</option>
            <option v-for="mode in modes" :key="mode.shortName" :value="mode.shortName">
              {{ mode.name }}
            </option>
          </select>
        </label>
        <label v-if="isForced">
          锚点
          <select v-model="forcedAnchor" :disabled="busy">
            <option value="start">选区开始（图像起点）</option>
            <option value="end">选区结束（图像数据末尾）</option>
          </select>
        </label>
        <label v-if="isForced">
          失谐兜底
          <input
            v-model.number="forcedHedrShiftHz"
            type="number"
            step="0.1"
            :disabled="busy"
          />
          Hz
        </label>
        <button :disabled="busy || !loaded || liveActive" @click="decodeSelection">解码选区</button>
      </div>
      <p v-if="isForced" class="hint">
        直接用播放头当锚点：在频谱图上点击/拖拽（或播放到某处暂停）把播放头放到
        {{ forcedAnchor === 'end' ? '图像数据末尾' : '图像起点' }}；解码长度由模式标称时长决定。
        强制模式下不显示范围选区。若锚点处有 VIS 头，会自动按其测得失谐修正；
        「失谐兜底」仅在窗口内没有 VIS 头时生效（电台偏离 1900 Hz 中频多少就填多少，可正可负）。
      </p>
      <p v-else class="hint">
        自动识模会按选区范围内的 VIS 头依次解码，可能得到多张图像。
      </p>
      <p class="status">{{ status }}</p>
      <div class="progress">
        <div class="bar" :style="{ width: (progressRatio * 100).toFixed(2) + '%' }" />
      </div>
      <p v-if="busy && timedJob" class="hint">{{ progressMeta }}</p>
      <p v-if="elapsedMs !== null" class="hint">解码耗时 {{ elapsedMs.toFixed(0) }} ms</p>
    </section>

    <section class="panel">
      <h2>4. 解码结果（{{ results.length }}）</h2>
      <p v-if="results.length === 0" class="hint">暂无结果</p>
      <div class="gallery">
        <figure v-for="(image, index) in results" :key="index" class="result">
          <img :src="image.dataUrl" :alt="`${image.mode} ${image.width}x${image.height}`" />
          <figcaption>
            <span>
              {{ image.mode }} · {{ image.width }}×{{ image.height }}
              <em v-if="image.partial" class="badge">未完整</em>
            </span>
            <a :href="image.dataUrl" :download="`sstv-${index + 1}-${image.mode}.png`">下载</a>
          </figcaption>
        </figure>
      </div>
    </section>

    <section class="panel">
      <h2>事件日志</h2>
      <ul class="log">
        <li v-for="(line, index) in log" :key="index">{{ line }}</li>
      </ul>
    </section>

    <div id="footer">
      <a class="footerlink" href="https://github.com/wty2019wty/Web_SSTV_by_slowrx.rs/" target="_blank" rel="noopener">
        <svg viewBox="0 0 20 20" width="16" height="16" fill="currentColor" aria-hidden="true"><g transform="translate(-84, -7399)"><path d="M94,7399 C99.523,7399 104,7403.59 104,7409.253 C104,7413.782 101.138,7417.624 97.167,7418.981 C96.66,7419.082 96.48,7418.762 96.48,7418.489 C96.48,7418.151 96.492,7417.047 96.492,7415.675 C96.492,7414.719 96.172,7414.095 95.813,7413.777 C98.04,7413.523 100.38,7412.656 100.38,7408.718 C100.38,7407.598 99.992,7406.684 99.35,7405.966 C99.454,7405.707 99.797,7404.664 99.252,7403.252 C99.252,7403.252 98.414,7402.977 96.505,7404.303 C95.706,7404.076 94.85,7403.962 94,7403.958 C93.15,7403.962 92.295,7404.076 91.497,7404.303 C89.586,7402.977 88.746,7403.252 88.746,7403.252 C88.203,7404.664 88.546,7405.707 88.649,7405.966 C88.01,7406.684 87.619,7407.598 87.619,7408.718 C87.619,7412.646 89.954,7413.526 92.175,7413.785 C91.889,7414.041 91.63,7414.493 91.54,7415.156 C90.97,7415.418 89.522,7415.871 88.63,7414.304 C88.63,7414.304 88.101,7413.319 87.097,7413.247 C87.097,7413.247 86.122,7413.234 87.029,7413.87 C87.029,7413.87 87.684,7414.185 88.139,7415.37 C88.139,7415.37 88.726,7417.2 91.508,7416.58 C91.513,7417.437 91.522,7418.245 91.522,7418.489 C91.522,7418.76 91.338,7419.077 90.839,7418.982 C86.865,7417.627 84,7413.783 84,7409.253 C84,7403.59 88.478,7399 94,7399"/></g></svg>
        GitHub 项目地址：Web_SSTV_by_slowrx.rs
      </a>

      <a class="footerlink" href="https://www.gnu.org/licenses/agpl-3.0.html" target="_blank" rel="noopener">
        开源协议：AGPL-3.0
      </a>
    </div>
  </main>
</template>

<style scoped>
.app {
  max-width: 1000px;
  margin: 0 auto;
  padding: 1.5rem;
  padding-left: calc(1.5rem + env(safe-area-inset-left));
  padding-right: calc(1.5rem + env(safe-area-inset-right));
  padding-bottom: calc(1.5rem + env(safe-area-inset-bottom));
  box-sizing: border-box;
  font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  color: #e6e6e6;
  background: #16181d;
  min-height: 100vh;
  min-height: 100dvh;
  -webkit-text-size-adjust: 100%;
  text-size-adjust: 100%;
}
h1 {
  font-size: 1.5rem;
  margin: 0 0 0.25rem;
}
h2 {
  font-size: 1rem;
  margin: 0 0 0.5rem;
  color: #9fb3c8;
}
.subtitle {
  color: #8b98a5;
  margin-top: 0;
}
.panel {
  background: #1e2128;
  border: 1px solid #2c3038;
  border-radius: 8px;
  padding: 1rem;
  margin-bottom: 1rem;
}
.row {
  display: flex;
  flex-wrap: wrap;
  gap: 1rem;
  align-items: center;
  margin-bottom: 0.75rem;
}
/* 音频来源切换（文件 / 麦克风）。 */
.source-tabs button.active {
  background: #2f6fbc;
  border-color: #4f9cf9;
  color: #fff;
}
label {
  display: flex;
  gap: 0.4rem;
  align-items: center;
}
/* 文件选择行：原生文件控件宽度不可压缩，允许换行并限制最大宽度，避免挤压标签文字或溢出面板。 */
.file {
  flex-wrap: wrap;
  min-width: 0;
}
.file-text {
  white-space: nowrap;
}
.file input[type='file'] {
  min-width: 0;
  max-width: 100%;
  box-sizing: border-box;
}
select,
button,
input[type='file'],
input[type='number'] {
  background: #2b3038;
  color: inherit;
  border: 1px solid #3a4048;
  border-radius: 6px;
  padding: 0.35rem 0.6rem;
}
input[type='number'] {
  width: 4.5rem;
}
button {
  cursor: pointer;
}
button:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.check input {
  accent-color: #4f9cf9;
}
.status {
  margin: 0.25rem 0;
  color: #cbd5e1;
}
/* 实时接收状态：独立于解码面板的 .status，避免 e2e/查询选中错误的元素。 */
.live-status {
  margin: 0.25rem 0;
  color: #cbd5e1;
}
.hint {
  color: #8b98a5;
  font-size: 0.85rem;
}
/* 当前音频来源：长文件名截断并可用 title 查看完整名称。 */
.source {
  margin: 0 0 0.25rem;
  color: #d7dee7;
  font-size: 0.9rem;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.source-label {
  color: #8b98a5;
}
.source.empty {
  color: #6b7280;
}
.devtools {
  margin-top: 0.75rem;
  border-top: 1px solid #2c3038;
  padding-top: 0.5rem;
}
.devtools summary {
  cursor: pointer;
  color: #8b98a5;
  font-size: 0.85rem;
}
.devtools[open] summary {
  margin-bottom: 0.6rem;
}
.devtools code {
  background: #2b3038;
  border-radius: 4px;
  padding: 0 0.25rem;
}
/* 输入诊断面板。 */
.diag {
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  margin: 0.25rem 0 0.5rem;
  font-size: 0.82rem;
  color: #9fb3c8;
}
.diag-row {
  display: flex;
  gap: 0.75rem;
}
.diag-row > span:first-child {
  flex: 0 0 8.5rem;
  color: #6b7280;
}
.warn {
  color: #f0b34a;
}
/* 麦克风实时解码的显眼实验性标注条（与 .hint.warn 区分，避免被探针脚本误当成环境警告）。 */
.notice-warn {
  margin: 0.5rem 0 0.75rem;
  padding: 0.55rem 0.75rem;
  border: 1px solid #6b4a12;
  border-left: 3px solid #f0b34a;
  border-radius: 6px;
  background: #2a2213;
  color: #ffd166;
  font-size: 0.85rem;
  line-height: 1.6;
}
.notice-warn strong {
  color: #ffe08a;
}
/* 不完整图标记。 */
.badge {
  margin-left: 0.35rem;
  padding: 0 0.3rem;
  border-radius: 4px;
  background: #6b4a12;
  color: #ffd166;
  font-size: 0.7rem;
  font-style: normal;
}
/* 标签页里的「开发测试」小标注：去掉按钮内的额外外边距。 */
.source-tabs .badge {
  margin-left: 0.3rem;
}
.progress {
  height: 6px;
  background: #2b3038;
  border-radius: 3px;
  overflow: hidden;
  margin: 0.5rem 0;
}
.bar {
  height: 100%;
  background: #4f9cf9;
  transition: width 0.1s linear;
}
.gallery {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 0.75rem;
}
.result {
  margin: 0;
  background: #14161a;
  border: 1px solid #2c3038;
  border-radius: 6px;
  overflow: hidden;
}
.result img {
  display: block;
  width: 100%;
  background: #000;
  image-rendering: pixelated;
}
.result figcaption {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 0.5rem;
  padding: 0.35rem 0.5rem;
  font-size: 0.8rem;
  color: #9fb3c8;
}
.result a {
  color: #4f9cf9;
}
.log {
  margin: 0;
  padding-left: 1rem;
  color: #9fb3c8;
  font-size: 0.82rem;
  max-height: 200px;
  overflow: auto;
}

/* --- 页脚 ---------------------------------------------------------------- */
#footer {
  display: flex;
  flex-wrap: wrap;
  justify-content: center;
  align-items: center;
  gap: 0.5rem 1.5rem;
  margin-top: 1.5rem;
  padding-top: 1rem;
  border-top: 1px solid #2c3038;
  font-size: 0.85rem;
}
.footerlink {
  display: inline-flex;
  align-items: center;
  gap: 0.35rem;
  color: #4f9cf9;
  text-decoration: none;
}
.footerlink:hover {
  text-decoration: underline;
}
.footerlink svg {
  flex: none;
}

/* --- 移动端 / 窄屏适配 ------------------------------------------------ */
@media (max-width: 720px) {
  .app {
    padding: 1rem;
    padding-left: calc(1rem + env(safe-area-inset-left));
    padding-right: calc(1rem + env(safe-area-inset-right));
    padding-bottom: calc(1rem + env(safe-area-inset-bottom));
  }
}

@media (max-width: 600px) {
  .app {
    padding: 0.75rem;
    padding-left: calc(0.75rem + env(safe-area-inset-left));
    padding-right: calc(0.75rem + env(safe-area-inset-right));
    padding-bottom: calc(0.75rem + env(safe-area-inset-bottom));
  }
  h1 {
    font-size: 1.25rem;
  }
  .subtitle {
    font-size: 0.88rem;
  }
  .panel {
    padding: 0.75rem;
    border-radius: 10px;
  }
  .row {
    gap: 0.6rem;
  }
  /* 触控友好的控件尺寸；input 字号 <16px 时 iOS 聚焦会自动放大页面。 */
  select,
  button,
  input[type='number'],
  input[type='file'] {
    min-height: 44px;
    font-size: 16px;
  }
  label {
    width: 100%;
    justify-content: space-between;
  }
  /* 窄屏下原生文件控件几乎占满一行，标签文字会被挤成竖排并溢出面板，
     因此改为上下堆叠：标签独占一行，文件控件占满整行。 */
  .file {
    flex-direction: column;
    align-items: stretch;
    justify-content: flex-start;
    gap: 0.35rem;
    width: 100%;
  }
  .file input[type='file'] {
    width: 100%;
    max-width: 100%;
    box-sizing: border-box;
  }
  .row button {
    flex: 1 1 auto;
  }
  .result figcaption {
    font-size: 0.78rem;
  }
  .gallery {
    grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
  }
  .log {
    max-height: 160px;
  }
}
</style>

<style>
/* 全局铺底（不加 scoped）：
   .app 为了可读性限宽 1000px 并居中，超出的部分落在 html/body 上，
   而 body 默认是白色画布，宽屏下两侧就会出现白边。
   这里把 html/body/#app 一并刷成与 .app 相同的深色，并清掉 body 默认外边距。 */
html,
body,
#app {
  margin: 0;
  background: #16181d;
}
</style>
