<script setup lang="ts">
// Audition 风格频谱图 + 时间选区。
//
// - 双层 Canvas：底层画频谱与坐标轴（仅视口变化时重绘），
//   顶层画选区/游标（交互过程中高频重绘，代价很小）。
// - 滚轮缩放（以光标为中心），Shift+滚轮平移，Alt/中键拖拽平移。
// - 在地图上拖拽创建选区；拖动选区内部可移动；拖动两端可缩放。
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import type { SpectrogramInfo, TimeSelection } from '../lib/protocol'

const props = defineProps<{
  spectrogram: SpectrogramInfo | null
  duration: number
  /** 播放头位置（秒）；null 表示不显示。 */
  playhead?: number | null
  /** 为 true 时播放头移出视口会自动跟随。 */
  follow?: boolean
}>()

const emit = defineEmits<{ seek: [time: number] }>()

const selection = defineModel<TimeSelection>({ required: true })

// 画布内边距（像素）。
const LEFT = 46
const RIGHT = 10
const TOP = 8
const BOTTOM = 18
const HEIGHT = 250
// 命中选区的边缘阈值（像素）。
const EDGE_PX = 6
// 选区最小跨度（秒）。
const MIN_SPAN = 0.1

const root = ref<HTMLDivElement | null>(null)
const baseCanvas = ref<HTMLCanvasElement | null>(null)
const overlayCanvas = ref<HTMLCanvasElement | null>(null)

const view = ref<{ t0: number; t1: number }>({ t0: 0, t1: 1 })
const hoverX = ref<number | null>(null)

const totalDuration = computed(() => Math.max(props.duration, 0.001))
const selectionText = computed(() => {
  const start = Math.max(0, Math.min(selection.value.start, totalDuration.value))
  const end = Math.max(start, Math.min(selection.value.end, totalDuration.value))
  return `${start.toFixed(2)}s – ${end.toFixed(2)}s（${(end - start).toFixed(2)}s）`
})

// --- 颜色映射（magma 近似） ---------------------------------------------
const COLOR_STOPS: ReadonlyArray<readonly [number, number, number]> = [
  [0, 0, 4],
  [28, 16, 68],
  [79, 18, 123],
  [129, 37, 129],
  [181, 54, 122],
  [229, 80, 100],
  [251, 135, 97],
  [254, 194, 135],
  [252, 253, 191],
]
const LUT = (() => {
  const lut = new Uint8ClampedArray(256 * 3)
  for (let i = 0; i < 256; i++) {
    const t = (i / 255) * (COLOR_STOPS.length - 1)
    const i0 = Math.floor(t)
    const i1 = Math.min(i0 + 1, COLOR_STOPS.length - 1)
    const f = t - i0
    const a = COLOR_STOPS[i0]
    const b = COLOR_STOPS[i1]
    lut[i * 3] = a[0] + (b[0] - a[0]) * f
    lut[i * 3 + 1] = a[1] + (b[1] - a[1]) * f
    lut[i * 3 + 2] = a[2] + (b[2] - a[2]) * f
  }
  return lut
})()

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value))
}

function plotWidth(): number {
  const canvas = baseCanvas.value
  return canvas ? Math.max(1, canvas.width - LEFT - RIGHT) : 1
}

function timeToPx(t: number): number {
  const span = view.value.t1 - view.value.t0 || 1
  return LEFT + ((t - view.value.t0) / span) * plotWidth()
}

function pxToTime(x: number): number {
  const span = view.value.t1 - view.value.t0 || 1
  return view.value.t0 + ((x - LEFT) / plotWidth()) * span
}

/** 把视口钳制到 [0, duration]，保持跨度。 */
function setView(t0: number, t1: number): void {
  const duration = totalDuration.value
  let span = clamp(t1 - t0, duration / 500, duration)
  let start = t0
  if (start < 0) start = 0
  if (start + span > duration) start = duration - span
  view.value = { t0: start, t1: start + span }
}

function fit(): void {
  view.value = { t0: 0, t1: totalDuration.value }
}

// --- 绘制 ---------------------------------------------------------------
// 复用的 ImageData，避免每次重绘都分配。
let imageCache: ImageData | null = null
let imageCacheW = 0
let imageCacheH = 0

function getImageData(w: number, h: number): ImageData {
  if (!imageCache || imageCacheW !== w || imageCacheH !== h) {
    imageCache = new ImageData(w, h)
    imageCacheW = w
    imageCacheH = h
  }
  return imageCache
}

function drawBase(): void {
  const canvas = baseCanvas.value
  const spec = props.spectrogram
  if (!canvas) return
  const ctx = canvas.getContext('2d')
  if (!ctx) return
  const width = canvas.width
  const height = canvas.height
  const plotW = width - LEFT - RIGHT
  const plotH = height - TOP - BOTTOM

  ctx.fillStyle = '#0b0b0f'
  ctx.fillRect(0, 0, width, height)

  if (spec && spec.columns > 0 && plotW > 0 && plotH > 0) {
    const image = getImageData(plotW, plotH)
    const out = image.data
    const { data, bins, columns, secondsPerColumn } = spec
    const span = view.value.t1 - view.value.t0
    const binMax = bins - 1
    // 先算好每列像素映射到的频谱列，内层只做 bin 采样。
    const colForX = new Int32Array(plotW)
    for (let x = 0; x < plotW; x++) {
      const t = view.value.t0 + (x / plotW) * span
      colForX[x] = clamp(Math.round(t / secondsPerColumn), 0, columns - 1)
    }
    for (let x = 0; x < plotW; x++) {
      const base = colForX[x] * bins
      for (let y = 0; y < plotH; y++) {
        const bin = Math.round((1 - y / (plotH - 1)) * binMax)
        const v = data[base + bin]
        const o = (y * plotW + x) * 4
        out[o] = LUT[v * 3]
        out[o + 1] = LUT[v * 3 + 1]
        out[o + 2] = LUT[v * 3 + 2]
        out[o + 3] = 255
      }
    }
    ctx.putImageData(image, LEFT, TOP)
  }

  // 坐标轴：频率刻度（左）。
  ctx.fillStyle = '#6b7280'
  ctx.strokeStyle = 'rgba(255,255,255,0.08)'
  ctx.font = '10px ui-monospace, monospace'
  ctx.textBaseline = 'middle'
  const maxHz = spec?.maxHz ?? 4000
  const freqStep = maxHz > 6000 ? 2000 : 1000
  for (let hz = 0; hz <= maxHz + 1; hz += freqStep) {
    const y = TOP + plotH * (1 - hz / maxHz)
    ctx.beginPath()
    ctx.moveTo(LEFT, y)
    ctx.lineTo(width - RIGHT, y)
    ctx.stroke()
    ctx.fillText(hz >= 1000 ? `${hz / 1000}k` : `${hz}`, 4, y)
  }

  // 坐标轴：时间刻度（下）。
  const span = view.value.t1 - view.value.t0
  const targetTicks = 8
  const rawStep = span / targetTicks
  const step = niceStep(rawStep)
  ctx.textBaseline = 'top'
  ctx.textAlign = 'center'
  const first = Math.ceil(view.value.t0 / step) * step
  for (let t = first; t <= view.value.t1; t += step) {
    const x = timeToPx(t)
    ctx.beginPath()
    ctx.moveTo(x, TOP)
    ctx.lineTo(x, height - BOTTOM)
    ctx.stroke()
    ctx.fillText(formatTime(t), x, height - BOTTOM + 3)
  }
  ctx.textAlign = 'left'
}

function niceStep(raw: number): number {
  const pow = Math.pow(10, Math.floor(Math.log10(Math.max(raw, 1e-6))))
  const norm = raw / pow
  const mult = norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 5 ? 5 : 10
  return mult * pow
}

function formatTime(t: number): string {
  const m = Math.floor(t / 60)
  const s = t - m * 60
  return m > 0 ? `${m}:${s.toFixed(1).padStart(4, '0')}` : `${s.toFixed(1)}s`
}

function drawOverlay(): void {
  const canvas = overlayCanvas.value
  if (!canvas) return
  const ctx = canvas.getContext('2d')
  if (!ctx) return
  const height = canvas.height
  const plotH = height - TOP - BOTTOM
  ctx.clearRect(0, 0, canvas.width, height)

  // 游标。
  if (hoverX.value !== null) {
    ctx.strokeStyle = 'rgba(255,255,255,0.25)'
    ctx.beginPath()
    ctx.moveTo(hoverX.value, TOP)
    ctx.lineTo(hoverX.value, TOP + plotH)
    ctx.stroke()
  }

  // 播放头。
  if (props.playhead !== null && props.playhead !== undefined) {
    const px = timeToPx(props.playhead)
    if (px >= LEFT && px <= canvas.width - RIGHT) {
      ctx.strokeStyle = '#ffd166'
      ctx.lineWidth = 1.5
      ctx.beginPath()
      ctx.moveTo(px, TOP)
      ctx.lineTo(px, TOP + plotH)
      ctx.stroke()
      ctx.fillStyle = '#ffd166'
      ctx.beginPath()
      ctx.moveTo(px - 4, TOP)
      ctx.lineTo(px + 4, TOP)
      ctx.lineTo(px, TOP + 7)
      ctx.closePath()
      ctx.fill()
    }
  }

  // 选区。
  const sx = timeToPx(selection.value.start)
  const ex = timeToPx(selection.value.end)
  ctx.fillStyle = 'rgba(79,156,249,0.22)'
  ctx.fillRect(sx, TOP, Math.max(1, ex - sx), plotH)
  ctx.strokeStyle = '#4f9cf9'
  ctx.lineWidth = 1.5
  ctx.beginPath()
  ctx.moveTo(sx, TOP)
  ctx.lineTo(sx, TOP + plotH)
  ctx.moveTo(ex, TOP)
  ctx.lineTo(ex, TOP + plotH)
  ctx.stroke()
  // 端点把手。
  ctx.fillStyle = '#4f9cf9'
  ctx.fillRect(sx - 2, TOP, 4, 10)
  ctx.fillRect(ex - 2, TOP, 4, 10)
  ctx.fillRect(sx - 2, TOP + plotH - 10, 4, 10)
  ctx.fillRect(ex - 2, TOP + plotH - 10, 4, 10)
}

function redrawAll(): void {
  scheduleRedraw(true)
}

// 用 rAF 合并重绘请求：交互过程中每帧最多画一次。
let rafPending = false
let needBase = false

function scheduleRedraw(needBaseDraw: boolean): void {
  needBase = needBase || needBaseDraw
  if (rafPending) return
  rafPending = true
  requestAnimationFrame(() => {
    rafPending = false
    if (needBase) {
      drawBase()
      needBase = false
    }
    drawOverlay()
  })
}

// --- 交互 ---------------------------------------------------------------
type Drag =
  | { mode: 'create'; anchor: number }
  | { mode: 'move'; grab: number }
  | { mode: 'resize-start' }
  | { mode: 'resize-end' }
  | { mode: 'pan'; startX: number; startT0: number; startT1: number }

let drag: Drag | null = null
let previousSelection: TimeSelection = { start: 0, end: 1 }

function localX(e: { clientX: number }): number {
  const rect = root.value!.getBoundingClientRect()
  return e.clientX - rect.left
}

function onPointerDown(e: PointerEvent): void {
  if (!props.spectrogram || !root.value) return
  const x = localX(e)
  if (x < LEFT || x > baseCanvas.value!.width - RIGHT) return
  root.value.setPointerCapture(e.pointerId)
  previousSelection = { ...selection.value }

  if (e.altKey || e.button === 1) {
    drag = { mode: 'pan', startX: x, startT0: view.value.t0, startT1: view.value.t1 }
    return
  }
  const t = clamp(pxToTime(x), 0, totalDuration.value)
  const sx = timeToPx(selection.value.start)
  const ex = timeToPx(selection.value.end)
  if (Math.abs(x - sx) <= EDGE_PX) {
    drag = { mode: 'resize-start' }
  } else if (Math.abs(x - ex) <= EDGE_PX) {
    drag = { mode: 'resize-end' }
  } else if (x > sx && x < ex) {
    drag = { mode: 'move', grab: t - selection.value.start }
  } else {
    drag = { mode: 'create', anchor: t }
    selection.value = { start: t, end: t }
  }
}

function onPointerMove(e: PointerEvent): void {
  if (!root.value || !baseCanvas.value) return
  const x = localX(e)
  hoverX.value = x
  if (!drag) {
    updateCursor(x)
    return
  }
  const duration = totalDuration.value
  const t = clamp(pxToTime(x), 0, duration)
  switch (drag.mode) {
    case 'pan': {
      const dt = ((drag.startX - x) / plotWidth()) * (drag.startT1 - drag.startT0)
      setView(drag.startT0 + dt, drag.startT1 + dt)
      break
    }
    case 'create': {
      selection.value = { start: Math.min(drag.anchor, t), end: Math.max(drag.anchor, t) }
      break
    }
    case 'move': {
      const length = selection.value.end - selection.value.start
      const start = clamp(t - drag.grab, 0, Math.max(0, duration - length))
      selection.value = { start, end: start + length }
      break
    }
    case 'resize-start':
      selection.value = { start: clamp(t, 0, selection.value.end), end: selection.value.end }
      break
    case 'resize-end':
      selection.value = { start: selection.value.start, end: clamp(t, selection.value.start, duration) }
      break
  }
}

function onPointerUp(e: PointerEvent): void {
  if (root.value?.hasPointerCapture(e.pointerId)) {
    root.value.releasePointerCapture(e.pointerId)
  }
  if (drag?.mode === 'create') {
    const span = selection.value.end - selection.value.start
    if (span < MIN_SPAN) {
      // 视为“点击定位”：移动播放头并保留原选区。
      emit('seek', drag.anchor)
      selection.value = previousSelection
    }
  }
  // 规范化选区顺序并限制范围。
  selection.value = {
    start: clamp(Math.min(selection.value.start, selection.value.end), 0, totalDuration.value),
    end: clamp(Math.max(selection.value.start, selection.value.end), 0, totalDuration.value),
  }
  drag = null
}

function updateCursor(x: number): void {
  const el = root.value
  if (!el) return
  const sx = timeToPx(selection.value.start)
  const ex = timeToPx(selection.value.end)
  if (Math.abs(x - sx) <= EDGE_PX || Math.abs(x - ex) <= EDGE_PX) {
    el.style.cursor = 'ew-resize'
  } else if (x > sx && x < ex) {
    el.style.cursor = 'grab'
  } else {
    el.style.cursor = 'crosshair'
  }
}

function onWheel(e: WheelEvent): void {
  if (!props.spectrogram) return
  e.preventDefault()
  const x = localX(e)
  if (e.shiftKey) {
    const dt = ((e.deltaY || e.deltaX) / plotWidth()) * (view.value.t1 - view.value.t0)
    setView(view.value.t0 + dt, view.value.t1 + dt)
    return
  }
  const t = clamp(pxToTime(x), 0, totalDuration.value)
  const factor = e.deltaY > 0 ? 1.25 : 1 / 1.25
  const oldSpan = view.value.t1 - view.value.t0
  const span = clamp(oldSpan * factor, totalDuration.value / 500, totalDuration.value)
  const t0 = t - (t - view.value.t0) * (span / oldSpan)
  setView(t0, t0 + span)
}

function zoomBy(factor: number): void {
  const center = (view.value.t0 + view.value.t1) / 2
  const span = view.value.t1 - view.value.t0
  const next = clamp(span * factor, totalDuration.value / 500, totalDuration.value)
  setView(center - next / 2, center + next / 2)
}

// --- 尺寸与生命周期 -----------------------------------------------------
let observer: ResizeObserver | null = null

function resize(): void {
  const el = root.value
  const base = baseCanvas.value
  const overlay = overlayCanvas.value
  if (!el || !base || !overlay) return
  const width = Math.max(240, Math.floor(el.clientWidth))
  for (const canvas of [base, overlay]) {
    canvas.width = width
    canvas.height = HEIGHT
  }
  redrawAll()
}

onMounted(() => {
  observer = new ResizeObserver(resize)
  if (root.value) observer.observe(root.value)
  resize()
})

onBeforeUnmount(() => {
  observer?.disconnect()
})

watch(
  () => [props.spectrogram, props.duration],
  () => {
    fit()
    resize()
  },
)
watch(view, () => scheduleRedraw(true))
watch(selection, () => scheduleRedraw(false), { deep: true })
watch(hoverX, () => scheduleRedraw(false))
watch(
  () => props.playhead,
  (t) => {
    if (t === null || t === undefined) {
      scheduleRedraw(false)
      return
    }
    if (props.follow && (t < view.value.t0 || t > view.value.t1)) {
      // 播放头移出视口：平移视口使其回到左侧 15% 处。
      const span = view.value.t1 - view.value.t0
      setView(t - span * 0.15, t + span * 0.85)
    } else {
      scheduleRedraw(false)
    }
  },
)
</script>

<template>
  <div class="spectro">
    <div class="toolbar">
      <button type="button" :disabled="!spectrogram" @click="fit">全览</button>
      <button type="button" :disabled="!spectrogram" @click="zoomBy(1 / 1.5)">放大</button>
      <button type="button" :disabled="!spectrogram" @click="zoomBy(1.5)">缩小</button>
      <span class="selection">选区 {{ selectionText }}</span>
      <span class="hint">滚轮缩放 · Shift+滚轮平移 · Alt/中键拖拽平移</span>
    </div>
    <div
      ref="root"
      class="canvas-wrap"
      @pointerdown="onPointerDown"
      @pointermove="onPointerMove"
      @pointerup="onPointerUp"
      @pointercancel="onPointerUp"
      @pointerleave="hoverX = null"
      @wheel="onWheel"
    >
      <canvas ref="baseCanvas" class="layer" />
      <canvas ref="overlayCanvas" class="layer overlay" />
      <p v-if="!spectrogram" class="placeholder">载入音频后显示频谱图</p>
    </div>
  </div>
</template>

<style scoped>
.spectro {
  margin-top: 0.5rem;
}
.toolbar {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  align-items: center;
  margin-bottom: 0.4rem;
}
.toolbar button {
  background: #2b3038;
  color: inherit;
  border: 1px solid #3a4048;
  border-radius: 6px;
  padding: 0.25rem 0.6rem;
  cursor: pointer;
}
.toolbar button:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}
.selection {
  color: #9fb3c8;
  font-size: 0.82rem;
}
.hint {
  color: #6b7280;
  font-size: 0.75rem;
  margin-left: auto;
}
.canvas-wrap {
  position: relative;
  width: 100%;
  height: 250px;
  background: #0b0b0f;
  border: 1px solid #2c3038;
  border-radius: 6px;
  overflow: hidden;
  touch-action: none;
}
.layer {
  display: block;
  position: absolute;
  inset: 0;
}
.overlay {
  pointer-events: none;
}
.placeholder {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  color: #6b7280;
  font-size: 0.85rem;
  margin: 0;
}
</style>
