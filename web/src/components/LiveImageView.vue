<script setup lang="ts">
// 实时接收时逐行绘制的图像画布。
//
// Worker 在渐进解码模式下发来 `line` 事件（每次一行 RGB），这里把该行写入
// 离屏 ImageData 并用 `putImageData` 的脏矩形只刷新那一行。整图末尾 Worker 会
// 用完整 sync 重解并发来全部行（覆盖预览），本组件尺寸不变时不重画，避免闪烁。
import { ref } from 'vue'

const canvas = ref<HTMLCanvasElement | null>(null)
const hasImage = ref(false)
const info = ref<{ mode: string; width: number; height: number } | null>(null)

let imageData: ImageData | null = null

/** 准备画布；尺寸与当前一致且已有缓冲时不重画（用于末尾重解覆盖预览）。 */
function begin(mode: string, width: number, height: number): void {
  const c = canvas.value
  if (!c || width <= 0 || height <= 0) return
  if (imageData && c.width === width && c.height === height) {
    info.value = { mode, width, height }
    return
  }
  c.width = width
  c.height = height
  const ctx = c.getContext('2d')
  if (!ctx) return
  ctx.fillStyle = '#000'
  ctx.fillRect(0, 0, width, height)
  imageData = ctx.createImageData(width, height)
  const d = imageData.data
  for (let i = 3; i < d.length; i += 4) d[i] = 255
  info.value = { mode, width, height }
  hasImage.value = true
}

/** 写入一行（RGB，长度 `width*3`）。 */
function pushLine(lineIndex: number, rgb: Uint8Array): void {
  const c = canvas.value
  if (!c || !imageData) return
  const width = imageData.width
  if (lineIndex < 0 || lineIndex >= imageData.height) return
  const d = imageData.data
  const rowBase = lineIndex * width * 4
  const count = Math.min(width, Math.floor(rgb.length / 3))
  for (let x = 0; x < count; x++) {
    const o = rowBase + x * 4
    d[o] = rgb[x * 3]
    d[o + 1] = rgb[x * 3 + 1]
    d[o + 2] = rgb[x * 3 + 2]
    d[o + 3] = 255
  }
  const ctx = c.getContext('2d')
  if (ctx) ctx.putImageData(imageData, 0, 0, 0, lineIndex, width, 1)
}

/** 清空（开始新会话 / 检测到新一张图的 VIS 时调用）。 */
function clear(): void {
  const c = canvas.value
  if (c) {
    const ctx = c.getContext('2d')
    if (ctx) {
      ctx.fillStyle = '#000'
      ctx.fillRect(0, 0, c.width, c.height)
    }
  }
  imageData = null
  info.value = null
  hasImage.value = false
}

defineExpose({ begin, pushLine, clear })
</script>

<template>
  <div class="live-image">
    <div class="frame">
      <canvas ref="canvas" class="live-image-canvas" />
      <p v-if="!hasImage" class="placeholder">接收中会在此逐行显示图像</p>
    </div>
    <p v-if="info" class="caption">{{ info.mode }} · {{ info.width }}×{{ info.height }}</p>
  </div>
</template>

<style scoped>
.live-image {
  margin-top: 0.5rem;
}
.frame {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 160px;
  background: #0b0b0f;
  border: 1px solid #2c3038;
  border-radius: 6px;
  overflow: hidden;
}
.live-image-canvas {
  display: block;
  max-width: 100%;
  height: auto;
  image-rendering: pixelated;
  background: #000;
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
.caption {
  margin: 0.35rem 0 0;
  color: #6b7280;
  font-size: 0.75rem;
  font-family: ui-monospace, monospace;
}
</style>
