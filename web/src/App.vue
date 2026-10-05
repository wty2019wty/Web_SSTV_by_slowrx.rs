<script setup lang="ts">
// Web SSTV 解码工具主界面。
//
// 流程：载入音频（文件 / 合成）→ Worker 计算频谱图 → 在频谱图上选时间段 →
// 解码选区（默认「自动识模」，可选具体模式强制解码）→ 结果画廊 + 逐张下载。
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
import { DecoderClient } from './lib/decoderClient'
import type {
  DecodeEvent,
  ForcedAnchor,
  LoadedInfo,
  ModeInfo,
  TimeSelection,
} from './lib/protocol'
import SpectrogramView from './components/SpectrogramView.vue'

/** 一张已解码图像。 */
interface DecodedImage {
  mode: string
  width: number
  height: number
  dataUrl: string
}

const client = shallowRef<DecoderClient | null>(null)
const modes = ref<ModeInfo[]>([])
/** 合成音频使用的模式。 */
const synthMode = ref('pd120')
/** 解码模式：`auto` = VIS 自动识模；其余为强制模式。 */
const decodeMode = ref('auto')
const forcedAnchor = ref<ForcedAnchor>('start')
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
const progress = ref(0)
const elapsedMs = ref<number | null>(null)
const log = ref<string[]>([])

const playing = ref(false)
const playhead = ref(0)

const isForced = computed(() => decodeMode.value !== 'auto')
const synthModeInfo = computed(() => modes.value.find((m) => m.shortName === synthMode.value))
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
  })
}

function beginJob(message: string) {
  busy.value = true
  progress.value = 0
  elapsedMs.value = null
  status.value = message
}

function endJob(result: { elapsedMs: number }) {
  elapsedMs.value = result.elapsedMs
  status.value = `完成（${result.elapsedMs.toFixed(0)} ms，${results.value.length} 张）`
  busy.value = false
}

function failJob(error: unknown) {
  status.value = `失败：${error instanceof Error ? error.message : String(error)}`
  busy.value = false
}

function onProgress(fed: number, total: number) {
  progress.value = total > 0 ? Math.round((fed / total) * 100) : 0
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
  if (!clientValue || busy.value || !loaded.value) return
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
  beginJob(isForced.value ? '正在强制解码…' : '正在自动识模解码…')
  try {
    const result = await clientValue.decode(
      {
        startSample,
        endSample,
        mode: isForced.value ? decodeMode.value : undefined,
        anchor: isForced.value ? forcedAnchor.value : undefined,
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
    if (modes.value.length > 0) synthMode.value = modes.value[0].shortName
    pushLog(`已加载 ${modes.value.length} 个模式`)
  } catch (error) {
    status.value = `初始化失败：${error instanceof Error ? error.message : String(error)}`
  }
})

onBeforeUnmount(() => {
  finishPlayback()
  void audioContext?.close()
  client.value?.dispose()
})
</script>

<template>
  <main class="app">
    <h1>Web SSTV 解码工具</h1>
    <p class="subtitle">解码核心为 slowrx（Rust → WASM），全部在浏览器本地完成，不上传音频。</p>

    <section class="panel">
      <h2>1. 音频来源</h2>
      <div class="row">
        <label class="file">
          选择音频文件
          <input
            type="file"
            accept="audio/*"
            :disabled="busy"
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
            <select v-model="synthMode" :disabled="busy">
              <option v-for="mode in modes" :key="mode.shortName" :value="mode.shortName">
                {{ mode.name }}（{{ mode.width }}×{{ mode.height }}）
              </option>
            </select>
          </label>
          <label class="check">
            <input v-model="synthWithVis" type="checkbox" :disabled="busy" />
            包含 VIS 头
          </label>
          <label>
            图片数
            <input v-model.number="synthCount" type="number" min="1" max="20" :disabled="busy" />
          </label>
          <button :disabled="busy" @click="loadSynth">生成合成音频</button>
        </div>
        <p class="hint">
          用 slowrx 的合成编码器生成已知内容的 SSTV 音频，便于在没有真实录音时验证解码。
          <template v-if="synthModeInfo">
            {{ synthModeInfo.name }} 标称图像时长约 {{ synthModeInfo.imageSeconds.toFixed(1) }} 秒。
          </template>
          需要 dev-synth 构建（<code>npm run wasm</code>），生产构建下不可用。
        </p>
      </details>
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
        <button :disabled="busy || !loaded" @click="decodeSelection">解码选区</button>
      </div>
      <p v-if="isForced" class="hint">
        直接用播放头当锚点：在频谱图上点击/拖拽（或播放到某处暂停）把播放头放到
        {{ forcedAnchor === 'end' ? '图像数据末尾' : '图像起点' }}；解码长度由模式标称时长决定。
        强制模式下不显示范围选区。
      </p>
      <p v-else class="hint">
        自动识模会按选区范围内的 VIS 头依次解码，可能得到多张图像。
      </p>
      <p class="status">{{ status }}</p>
      <div class="progress"><div class="bar" :style="{ width: progress + '%' }" /></div>
      <p v-if="elapsedMs !== null" class="hint">解码耗时 {{ elapsedMs.toFixed(0) }} ms</p>
    </section>

    <section class="panel">
      <h2>4. 解码结果（{{ results.length }}）</h2>
      <p v-if="results.length === 0" class="hint">暂无结果</p>
      <div class="gallery">
        <figure v-for="(image, index) in results" :key="index" class="result">
          <img :src="image.dataUrl" :alt="`${image.mode} ${image.width}x${image.height}`" />
          <figcaption>
            <span>{{ image.mode }} · {{ image.width }}×{{ image.height }}</span>
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
label {
  display: flex;
  gap: 0.4rem;
  align-items: center;
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
