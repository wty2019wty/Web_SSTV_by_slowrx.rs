// 浏览器 V8 解码性能基准：启动 Vite，用本机 Edge/Chrome 在 Worker 内
// 解码各模式的合成音频，统计耗时。
//
// 前置：.\\build-wasm.ps1 -Dev
// 运行：cd web && npm run bench
import { existsSync } from 'node:fs'
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

// 合成模式的显示名（用于选择下拉项）。
const MODES = [
  ['PD-120', 124.0],
  ['PD-180', 180.0],
  ['PD-240', 240.0],
  ['Robot 36', 36.0],
  ['Robot 72', 72.0],
  ['Scottie 1', 110.0],
  ['Martin 1', 114.0],
]

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

  console.log('模式'.padEnd(12), '图像(s)'.padStart(8), '解码(ms)'.padStart(10), '倍数'.padStart(8))
  const rows = []
  for (const [label, imageSeconds] of MODES) {
    await loadMode(page, label)
    const ms = await decodeOnce(page)
    const ratio = imageSeconds > 0 ? (ms / 1000 / imageSeconds).toFixed(2) : '—'
    rows.push([label, imageSeconds, ms, ratio])
    console.log(label.padEnd(12), String(imageSeconds).padStart(8), String(ms).padStart(10), String(ratio).padStart(8))
  }

  console.log('\n说明：解码耗时由 Worker 内计时（仅解码循环，不含频谱图/传输）。')
} finally {
  await browser.close()
  await server.close()
}
