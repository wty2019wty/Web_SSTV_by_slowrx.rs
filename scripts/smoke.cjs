// Node 冒烟测试：直接加载 wasm 绑定，解码合成音频并校验结果。
//
// 先执行：.\build-wasm.ps1 -Node -Dev
// 再执行：node scripts/smoke.cjs
//
// 校验点：
//   1. listModes 覆盖 PD/Robot/Scottie/Martin；
//   2. VIS 自动识模能解出 pd120（640×496）；
//   3. 强制模式（无 VIS）同样能解出；
//   4. 解码结果不是全黑，且保留合成图的水平亮度梯度。
// 同时把结果写成 PPM（out/pd120.ppm）便于人工查看。

const fs = require('node:fs')
const path = require('node:path')

const pkgDir = path.join(__dirname, '..', 'pkg-node')
const pkg = path.join(pkgDir, 'slowrx_wasm.js')
if (!fs.existsSync(pkg)) {
  console.error(`未找到 ${pkg}\n请先运行：.\\build-wasm.ps1 -Node -Dev`)
  process.exit(1)
}

const { WasmDecoder, listModes, synthTestAudio, computeSpectrogram } = require(pkg)

const WORK_RATE = 11025
const FEED_CHUNK = 32768

function decodeAny(audio, { mode, startSecs } = {}) {
  const decoder = new WasmDecoder(WORK_RATE)
  try {
    if (mode) decoder.setForcedMode(mode, startSecs ?? 0, undefined)
    const startedAt = Date.now()
    let image = null
    let sawVis = false
    for (let offset = 0; offset < audio.length; offset += FEED_CHUNK) {
      const end = Math.min(offset + FEED_CHUNK, audio.length)
      for (const event of decoder.pushAudio(audio.subarray(offset, end))) {
        if (event.type === 'vis') sawVis = true
        if (event.type === 'image') image = event
      }
    }
    return { image, sawVis, elapsedMs: Date.now() - startedAt }
  } finally {
    decoder.free()
  }
}

/** 横向亮度梯度检查：左端平均 R 应明显小于右端。 */
function gradientCheck(image) {
  const { width, height, rgba } = image
  const left = { sum: 0, n: 0 }
  const right = { sum: 0, n: 0 }
  const cols = Math.min(10, width)
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const r = rgba[(y * width + x) * 4]
      if (x < cols) {
        left.sum += r
        left.n++
      } else if (x >= width - cols) {
        right.sum += r
        right.n++
      }
    }
  }
  return { leftMean: left.sum / left.n, rightMean: right.sum / right.n }
}

/** 写出 P6 PPM，便于人工目视检查。 */
function writePpm(filePath, image) {
  const { width, height, rgba } = image
  const header = Buffer.from(`P6\n${width} ${height}\n255\n`, 'ascii')
  const rgb = Buffer.alloc(width * height * 3)
  for (let i = 0; i < width * height; i++) {
    rgb[i * 3] = rgba[i * 4]
    rgb[i * 3 + 1] = rgba[i * 4 + 1]
    rgb[i * 3 + 2] = rgba[i * 4 + 2]
  }
  fs.mkdirSync(path.dirname(filePath), { recursive: true })
  fs.writeFileSync(filePath, Buffer.concat([header, rgb]))
}

let failures = 0
function check(condition, message) {
  if (condition) {
    console.log(`  ✓ ${message}`)
  } else {
    failures++
    console.error(`  ✗ ${message}`)
  }
}

console.log('== 模式列表 ==')
const modes = listModes()
const shorts = modes.map((m) => m.shortName)
for (const want of [
  'pd50',
  'pd90',
  'pd120',
  'pd160',
  'pd180',
  'pd240',
  'pd290',
  'robot24',
  'robot36',
  'robot72',
  'scottie1',
  'scottie2',
  'scottiedx',
  'martin1',
  'martin2',
  'sc2180',
]) {
  check(shorts.includes(want), `包含模式 ${want}`)
}
const pd120 = modes.find((m) => m.shortName === 'pd120')
check(!!pd120, 'pd120 元数据存在')

console.log('\n== VIS 自动识模 ==')
const auto = decodeAny(synthTestAudio('pd120', true))
check(auto.sawVis, '产生 VisDetected')
check(!!auto.image, '产生 ImageComplete')
if (auto.image) {
  check(auto.image.mode === 'pd120', `模式为 pd120（实际 ${auto.image.mode}）`)
  check(
    auto.image.width === pd120.width && auto.image.height === pd120.height,
    `尺寸 ${auto.image.width}×${auto.image.height}`,
  )
  const g = gradientCheck(auto.image)
  check(g.rightMean - g.leftMean > 40, `保留亮度梯度（左 ${g.leftMean.toFixed(0)} → 右 ${g.rightMean.toFixed(0)}）`)
  check(g.rightMean > 10, '图像非全黑')
  writePpm(path.join(__dirname, '..', 'out', 'pd120.ppm'), auto.image)
  console.log(`  · 解码耗时 ${auto.elapsedMs} ms，已写出 out/pd120.ppm`)
}

console.log('\n== 强制模式（无 VIS）==')
const forced = decodeAny(synthTestAudio('pd120', false), { mode: 'pd120', startSecs: 0 })
check(!forced.sawVis, '强制模式不产生 VisDetected')
check(!!forced.image && forced.image.mode === 'pd120', '强制模式解出 pd120')
if (forced.image) {
  const g = gradientCheck(forced.image)
  check(g.rightMean - g.leftMean > 40, '强制模式同样保留亮度梯度')
  console.log(`  · 解码耗时 ${forced.elapsedMs} ms`)
}

console.log('\n== 静音窗口不应伪造图像 ==')
const silence = new Float32Array(WORK_RATE * 45)
const onSilence = decodeAny(silence, { mode: 'robot24', startSecs: 0 })
check(!onSilence.image, '静音窗口无 ImageComplete')

console.log('\n== 频谱图（STFT）==')
const specAudio = synthTestAudio('pd120', true)
const spec = computeSpectrogram(specAudio, WORK_RATE, 0, 0, 0)
check(!!spec, '计算得到频谱图')
if (spec) {
  check(spec.columns > 0, `列数 ${spec.columns} > 0`)
  check(spec.bins > 0 && spec.bins <= 513, `bin 数 ${spec.bins} 合法`)
  const data = spec.data()
  check(data.length === spec.columns * spec.bins, '强度矩阵尺寸 = columns×bins')
  let max = 0
  for (const value of data) if (value > max) max = value
  check(max > 120, `存在高亮像素（最大 ${max}）`)
  const expectedSpc = spec.hop / spec.sampleRate
  check(
    Math.abs(spec.secondsPerColumn - expectedSpc) < 1e-9,
    `每列时长 ${spec.secondsPerColumn.toFixed(4)}s`,
  )
  spec.free()
}

if (failures > 0) {
  console.error(`\n冒烟测试失败：${failures} 项`)
  process.exit(1)
}
console.log('\n全部通过。')
