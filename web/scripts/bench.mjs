// 浏览器 V8 解码性能基准：启动 Vite，用本机 Edge/Chrome 在 Worker 内
// 解码全部模式的合成音频，统计耗时。模式清单与标称图像时长直接取自
// wasm 的 listModes()（与 Rust 模式表同源），不手抄。
//
// 前置：.\\build-wasm.ps1 -Dev
// 运行：cd web && npm run bench
import { existsSync, readFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'
import puppeteer from 'puppeteer-core'

const here = path.dirname(fileURLToPath(import.meta.url))
const webRoot = path.resolve(here, '..')

const BROWSERS = [
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
  'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
]
const executablePath = BROWSERS.find((p) => existsSync(p))
if (!executablePath) {
  console.error('未找到 Edge / Chrome')
  process.exit(1)
}

// 全部模式（与 wasm 的 listModes() 同源，含标称图像时长）。
// 生成物（build-wasm.ps1 的输出）未就绪时给出友好提示，
// 而不是在 import 期抛出原始的模块找不到错误。
const wasmJs = path.resolve(here, '../src/wasm/slowrx_wasm.js')
const wasmBin = path.resolve(here, '../src/wasm/slowrx_wasm_bg.wasm')
if (!existsSync(wasmJs) || !existsSync(wasmBin)) {
  console.error('未找到 web/src/wasm/ 生成物，请先执行：.\\build-wasm.ps1 -Dev')
  process.exit(1)
}
const { default: init, listModes } = await import('../src/wasm/slowrx_wasm.js')
await init({
  module_or_path: readFileSync(wasmBin),
})
const MODES = listModes().map((m) => ({
  label: m.name,
  dims: `${m.width}×${m.height}`,
  imageSeconds: m.imageSeconds,
}))

async function clickButtonByText(page, text) {
  const ok = await page.evaluate((label) => {
    const button = [...document.querySelectorAll('button')].find((b) => b.textContent.includes(label))
    if (!button) return false
    button.click()
    return true
  }, text)
  if (!ok) throw new Error(`未找到按钮：${text}`)
}

async function selectSynthMode(page, label) {
  await page.evaluate((needle) => {
    const select = [...document.querySelectorAll('select')].find(
      (s) =>
        [...s.options].some((o) => o.textContent.includes(needle)) &&
        ![...s.options].some((o) => o.textContent.includes('自动识模')),
    )
    if (!select) throw new Error(`未找到合成模式下拉框：${needle}`)
    const option = [...select.options].find((o) => o.textContent.includes(needle))
    select.value = option.value
    select.dispatchEvent(new Event('change', { bubbles: true }))
  }, label)
}

async function waitLoaded(page) {
  await page.waitForFunction(
    () => document.querySelector('.status')?.textContent?.includes('已载入'),
    { timeout: 180_000, polling: 200 },
  )
}

/** 选择合成模式并生成音频。 */
async function loadMode(page, label) {
  await selectSynthMode(page, label)
  await clickButtonByText(page, '生成合成音频')
  await waitLoaded(page)
}

async function decodeOnce(page) {
  await clickButtonByText(page, '解码选区')
  // 先确认进入“解码中”，避免把上一次的“完成”误判为本次结果。
  await page.waitForFunction(
    () => document.querySelector('.status')?.textContent?.includes('正在'),
    { timeout: 10_000, polling: 50 },
  )
  await page.waitForFunction(
    () => {
      const status = document.querySelector('.status')?.textContent ?? ''
      const imgs = document.querySelectorAll('.result img')
      return status.includes('完成') && imgs.length >= 1 && [...imgs].every((img) => img.naturalWidth > 0)
    },
    { timeout: 240_000, polling: 200 },
  )
  const status = await page.$eval('.status', (el) => el.textContent ?? '')
  const match = status.match(/(\d+)\s*ms/)
  return match ? Number.parseInt(match[1], 10) : -1
}

const server = await createServer({
  root: webRoot,
  logLevel: 'warn',
  server: { host: '127.0.0.1', port: 5200, strictPort: true },
})
await server.listen()
const url = 'http://127.0.0.1:5200/'

const browser = await puppeteer.launch({
  executablePath,
  headless: true,
  protocolTimeout: 900_000,
  args: ['--no-sandbox'],
})

try {
  const page = await browser.newPage()
  await page.setViewport({ width: 1280, height: 900 })
  await page.goto(url, { waitUntil: 'networkidle0', timeout: 60_000 })
  await page.waitForFunction(
    () => [...document.querySelectorAll('select option')].some((o) => o.textContent.includes('自动识模')),
    { timeout: 60_000 },
  )
  await page.evaluate(() => {
    const details = document.querySelector('details.devtools')
    if (details) details.open = true
  })

  // 预热：先解一次 PD120，让 V8 完成 wasm 分层编译。
  await loadMode(page, 'PD-120')
  await decodeOnce(page)
  console.log('预热完成，开始测量…\n')

  console.log(
    '模式'.padEnd(16),
    '分辨率'.padEnd(10),
    '图像(s)'.padStart(8),
    '解码(ms)'.padStart(10),
    '倍速(×)'.padStart(8),
  )
  const rows = []
  for (const { label, dims, imageSeconds } of MODES) {
    await loadMode(page, label)
    const ms = await decodeOnce(page)
    // 倍速 = 图像时长 ÷ 解码耗时（与 README 基准表口径一致）。
    const ratio = ms > 0 && imageSeconds > 0 ? (imageSeconds / (ms / 1000)).toFixed(2) : '—'
    rows.push([label, dims, imageSeconds, ms, ratio])
    console.log(
      label.padEnd(16),
      dims.padEnd(10),
      imageSeconds.toFixed(1).padStart(8),
      String(ms).padStart(10),
      String(ratio).padStart(8),
    )
  }

  console.log('\n说明：解码耗时由 Worker 内计时（仅解码循环，不含频谱图/传输）。')
} finally {
  await browser.close()
  await server.close()
}
