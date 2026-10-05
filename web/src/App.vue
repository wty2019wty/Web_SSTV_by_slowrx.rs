<script setup lang="ts">
// M2 开发自测界面：在 Worker 内解码一张（合成或文件）SSTV 图像并渲染。
// 频谱图 / 时间选区 / 麦克风将在 M3–M5 里程碑接入。
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
import { DecoderClient } from './lib/decoderClient'
import type { DecodeEvent, ModeInfo } from './lib/protocol'

const client = shallowRef<DecoderClient | null>(null)
const modes = ref<ModeInfo[]>([])
const selectedMode = ref('pd120')
const withVis = ref(true) // true = VIS 自动识模；false = 强制模式（选区不含 VIS）

const busy = ref(false)
const status = ref('就绪')
const progress = ref(0)
const elapsedMs = ref<number | null>(null)
const log = ref<string[]>([])
const lastImage = ref<{ mode: string; width: number; height: number } | null>(null)

const canvasRef = ref<HTMLCanvasElement | null>(null)

const currentMode = computed(() => modes.value.find((m) => m.shortName === selectedMode.value))

function pushLog(line: string) {
  const time = new Date().toLocaleTimeString()
  log.value = [`[${time}] ${line}`, ...log.value].slice(0, 50)
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
      renderImage(event)
      pushLog(`图像完成：${event.mode} ${event.width}×${event.height}`)
      break
    default:
      break
  }
}

function renderImage(event: Extract<DecodeEvent, { type: 'image' }>) {
  const canvas = canvasRef.value
  if (!canvas) return
  canvas.width = event.width
  canvas.height = event.height
  const ctx = canvas.getContext('2d')
  if (!ctx) return
  // 复制成带 ArrayBuffer 的 Uint8ClampedArray，满足 ImageData 的类型要求。
  const clamped = new Uint8ClampedArray(event.rgba)
  ctx.putImageData(new ImageData(clamped, event.width, event.height), 0, 0)
  lastImage.value = { mode: event.mode, width: event.width, height: event.height }
}

async function runSynth() {
  const c = client.value
  if (!c || busy.value) return
  busy.value = true
  progress.value = 0
  elapsedMs.value = null
  status.value = '正在生成合成音频并解码…'
  try {
    const result = await c.decodeSynth(selectedMode.value, withVis.value, {
      onEvent: handleEvent,
      onProgress: (fed, total) => {
        progress.value = total > 0 ? Math.round((fed / total) * 100) : 0
      },
    })
    elapsedMs.value = result.elapsedMs
    status.value = `完成（${result.elapsedMs.toFixed(0)} ms）`
  } catch (error) {
    status.value = `失败：${error instanceof Error ? error.message : String(error)}`
  } finally {
    busy.value = false
  }
}

async function onFileChange(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file || !client.value || busy.value) return
  busy.value = true
  progress.value = 0
  elapsedMs.value = null
  status.value = `正在解析 ${file.name}…`
  try {
    const arrayBuffer = await file.arrayBuffer()
    const audioContext = new AudioContext()
    const buffer = await audioContext.decodeAudioData(arrayBuffer)
    const mono = toMono(buffer)
    await audioContext.close()
    status.value = `正在解码（${buffer.sampleRate} Hz，${mono.length} 采样）…`
    const result = await client.value.decode(
      {
        sampleRate: buffer.sampleRate,
        audio: mono,
        mode: withVis.value ? undefined : selectedMode.value,
        startSecs: withVis.value ? undefined : 0,
      },
      {
        onEvent: handleEvent,
        onProgress: (fed, total) => {
          progress.value = total > 0 ? Math.round((fed / total) * 100) : 0
        },
      },
    )
    elapsedMs.value = result.elapsedMs
    status.value = `完成（${result.elapsedMs.toFixed(0)} ms）`
  } catch (error) {
    status.value = `失败：${error instanceof Error ? error.message : String(error)}`
  } finally {
    busy.value = false
    input.value = ''
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

onMounted(async () => {
  const c = new DecoderClient()
  client.value = c
  try {
    modes.value = await c.listModes()
    if (modes.value.length > 0) selectedMode.value = modes.value[0].shortName
    pushLog(`已加载 ${modes.value.length} 个模式`)
  } catch (error) {
    status.value = `初始化失败：${error instanceof Error ? error.message : String(error)}`
  }
})

onBeforeUnmount(() => {
  client.value?.dispose()
})
</script>

<template>
  <main class="app">
    <h1>Web SSTV 解码工具</h1>
    <p class="subtitle">解码核心为 slowrx（Rust → WASM），全部在浏览器本地完成，不上传音频。</p>

    <section class="panel">
      <div class="row">
        <label>
          模式
          <select v-model="selectedMode" :disabled="busy">
            <option v-for="mode in modes" :key="mode.shortName" :value="mode.shortName">
              {{ mode.name }}（{{ mode.width }}×{{ mode.height }}）
            </option>
          </select>
        </label>
        <label class="check">
          <input v-model="withVis" type="checkbox" :disabled="busy" />
          VIS 自动识模（取消则按所选模式强制解码）
        </label>
      </div>

      <div class="row">
        <button :disabled="busy" @click="runSynth">解码合成音频</button>
        <label class="file">
          选择音频文件
          <input type="file" accept="audio/*" :disabled="busy" @change="onFileChange" />
        </label>
      </div>

      <p class="status">{{ status }}</p>
      <div class="progress"><div class="bar" :style="{ width: progress + '%' }" /></div>
      <p v-if="currentMode" class="hint">
        当前模式标称图像时长约 {{ currentMode.imageSeconds.toFixed(1) }} 秒
        <span v-if="elapsedMs !== null">· 解码耗时 {{ elapsedMs.toFixed(0) }} ms</span>
      </p>
    </section>

    <section class="panel">
      <h2>解码结果</h2>
      <p v-if="lastImage" class="hint">
        {{ lastImage.mode }} · {{ lastImage.width }}×{{ lastImage.height }}
      </p>
      <canvas ref="canvasRef" class="preview" />
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
  max-width: 900px;
  margin: 0 auto;
  padding: 1.5rem;
  font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  color: #e6e6e6;
  background: #16181d;
  min-height: 100vh;
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
input[type='file'] {
  background: #2b3038;
  color: inherit;
  border: 1px solid #3a4048;
  border-radius: 6px;
  padding: 0.35rem 0.6rem;
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
.preview {
  max-width: 100%;
  background: #000;
  border-radius: 4px;
  image-rendering: pixelated;
}
.log {
  margin: 0;
  padding-left: 1rem;
  color: #9fb3c8;
  font-size: 0.82rem;
  max-height: 200px;
  overflow: auto;
}
</style>
