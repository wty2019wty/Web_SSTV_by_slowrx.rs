<script setup lang="ts">
// 实时接收的滚动瀑布图。
//
// 与 `SpectrogramView` 不同：这里不接受整段频谱矩阵，而是由父组件在收到
// `liveColumns` 后调用 `push(columns, count)` 增量追加新列。渲染采用
// 「离屏画布自滚动 + 右侧写入新列」的方式，每新增一列只做 O(plotH) 的工作，
// 适合持续数分钟的实时流。
//
// 1 列 = 1 像素；新的列从右侧进入，旧的向左滚出，右端即“现在”。
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { MAGMA_LUT } from '../lib/colorMap'

const props = defineProps<{
  /** 每列的频率 bin 数；0 表示尚未开始接收。 */
  bins: number
  /** 显示上限频率（Hz）。 */
  maxHz: number
  /** 每列代表的时间跨度（秒）。 */
  secondsPerColumn: number
}>()

const root = ref<HTMLDivElement | null>(null)
const canvas = ref<HTMLCanvasElement | null>(null)
const canvasHeight = ref(250)
const hasContent = ref(false)

let LEFT = 46
let RIGHT = 10
let TOP = 8
let BOTTOM = 18

let offscreen: HTMLCanvasElement | null = null
let offCtx: CanvasRenderingContext2D | null = null
let columnImage: ImageData | null = null
let plotW = 1
let plotH = 1
let observer: ResizeObserver | null = null
let rafPending = false

const BG = '#0b0b0f'

function applyLayout(width: number): void {
  if (width < 480) {
    LEFT = 30
    RIGHT = 6
    BOTTOM = 16
    canvasHeight.value = 180
  } else if (width < 720) {
    LEFT = 38
    RIGHT = 8
    BOTTOM = 17
    canvasHeight.value = 210
  } else {
    LEFT = 46
    RIGHT = 10
    BOTTOM = 18
    canvasHeight.value = 250
  }
}

/** 清空瀑布（容器尺寸变化 / 重新开始接收时调用）。 */
function clear(): void {
  if (!offCtx || !offscreen) return
  offCtx.fillStyle = BG
  offCtx.fillRect(0, 0, offscreen.width, offscreen.height)
  hasContent.value = false
  scheduleDraw()
}

function resize(): void {
  const el = root.value
  const c = canvas.value
  if (!el || !c) return
  const width = Math.max(240, Math.floor(el.clientWidth))
  applyLayout(width)
  c.width = width
  c.height = canvasHeight.value
  plotW = Math.max(1, width - LEFT - RIGHT)
  plotH = Math.max(1, canvasHeight.value - TOP - BOTTOM)
  offscreen = document.createElement('canvas')
  offscreen.width = plotW
  offscreen.height = plotH
  offCtx = offscreen.getContext('2d')
  columnImage = new ImageData(plotW, plotH)
  clear()
}

/** 追加 `count` 列（列优先 8 位强度，长度 `count * bins`）。 */
function push(columns: Uint8Array, count: number): void {
  const bins = props.bins
  if (!offCtx || !offscreen || !columnImage || bins <= 0 || count <= 0) return
  let usable = Math.min(count, Math.floor(columns.length / bins))
  if (usable <= 0) return
  let startCol = 0
  if (usable > plotW) {
    // 一次来的列多于一屏：只保留最新的 plotW 列，其余丢弃。
    startCol = usable - plotW
    usable = plotW
    offCtx.fillStyle = BG
    offCtx.fillRect(0, 0, plotW, plotH)
  }

  const out = columnImage.data
  const binMax = bins - 1
  const denom = plotH > 1 ? plotH - 1 : 1
  for (let x = 0; x < usable; x++) {
    const base = (startCol + x) * bins
    for (let y = 0; y < plotH; y++) {
      const bin = Math.round((1 - y / denom) * binMax)
      const v = columns[base + bin]
      const o = (y * plotW + x) * 4
      out[o] = MAGMA_LUT[v * 3]
      out[o + 1] = MAGMA_LUT[v * 3 + 1]
      out[o + 2] = MAGMA_LUT[v * 3 + 2]
      out[o + 3] = 255
    }
  }

  // 旧内容整体左移 `usable` 像素，再把新列写到最右侧。
  if (usable < plotW) offCtx.drawImage(offscreen, -usable, 0)
  offCtx.putImageData(columnImage, plotW - usable, 0)
  hasContent.value = true
  scheduleDraw()
}

function niceStep(raw: number): number {
  const safe = Math.max(raw, 1e-6)
  const pow = Math.pow(10, Math.floor(Math.log10(safe)))
  const norm = safe / pow
  const mult = norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 5 ? 5 : 10
  return mult * pow
}

function formatSeconds(s: number): string {
  const m = Math.floor(s / 60)
  const rest = s - m * 60
  return m > 0 ? `${m}:${rest.toFixed(1).padStart(4, '0')}` : `${rest.toFixed(1)}s`
}

function scheduleDraw(): void {
  if (rafPending) return
  rafPending = true
  requestAnimationFrame(() => {
    rafPending = false
    draw()
  })
}

function draw(): void {
  const c = canvas.value
  if (!c) return
  const ctx = c.getContext('2d')
  if (!ctx) return
  const width = c.width
  const height = c.height
  ctx.fillStyle = BG
  ctx.fillRect(0, 0, width, height)
  if (offscreen) ctx.drawImage(offscreen, LEFT, TOP)

  // 频率轴（左）。
  ctx.fillStyle = '#6b7280'
  ctx.strokeStyle = 'rgba(255,255,255,0.08)'
  ctx.font = '10px ui-monospace, monospace'
  ctx.textBaseline = 'middle'
  ctx.textAlign = 'left'
  const maxHz = props.maxHz || 4000
  const freqStep = maxHz > 6000 ? 2000 : 1000
  for (let hz = 0; hz <= maxHz + 1; hz += freqStep) {
    const y = TOP + plotH * (1 - hz / maxHz)
    ctx.beginPath()
    ctx.moveTo(LEFT, y)
    ctx.lineTo(width - RIGHT, y)
    ctx.stroke()
    ctx.fillText(hz >= 1000 ? `${hz / 1000}k` : `${hz}`, 4, y)
  }

  // 时间轴（下）：右端为“现在”，向左为过去。
  const spc = props.secondsPerColumn
  if (spc > 0) {
    ctx.textBaseline = 'top'
    ctx.textAlign = 'center'
    const plotSeconds = plotW * spc
    const step = niceStep(plotSeconds / 6)
    for (let s = 0; s <= plotSeconds + 1e-9; s += step) {
      const x = width - RIGHT - s / spc
      if (x < LEFT) break
      ctx.beginPath()
      ctx.moveTo(x, TOP)
      ctx.lineTo(x, height - BOTTOM)
      ctx.stroke()
      ctx.fillText(s < 1e-6 ? '现在' : `-${formatSeconds(s)}`, x, height - BOTTOM + 3)
    }
    ctx.textAlign = 'left'
  }
}

onMounted(() => {
  observer = new ResizeObserver(resize)
  if (root.value) observer.observe(root.value)
  resize()
})

onBeforeUnmount(() => {
  observer?.disconnect()
})

// 开始新的接收会话（bins 由 0 变为有效值）时重建画布并清空。
watch(
  () => [props.bins, props.maxHz, props.secondsPerColumn],
  () => resize(),
)

defineExpose({ push, clear })
</script>

<template>
  <div class="waterfall">
    <div ref="root" class="waterfall-wrap" :style="{ height: canvasHeight + 'px' }">
      <canvas ref="canvas" class="waterfall-canvas" />
      <p v-if="bins <= 0" class="placeholder">开始接收后显示实时瀑布图</p>
      <p v-else-if="!hasContent" class="placeholder">等待音频…</p>
    </div>
    <p class="hint">
      瀑布图：横轴时间（右端为“现在”），纵轴频率 0–{{ (maxHz / 1000).toFixed(1) }} kHz。
    </p>
  </div>
</template>

<style scoped>
.waterfall {
  margin-top: 0.5rem;
}
.waterfall-wrap {
  position: relative;
  width: 100%;
  height: 250px;
  background: #0b0b0f;
  border: 1px solid #2c3038;
  border-radius: 6px;
  overflow: hidden;
}
.waterfall-canvas {
  display: block;
  position: absolute;
  inset: 0;
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
  pointer-events: none;
}
.hint {
  color: #6b7280;
  font-size: 0.75rem;
  margin: 0.35rem 0 0;
}
</style>
