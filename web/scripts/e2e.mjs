// 浏览器端到端验证：启动 Vite 开发服务器，用本机 Edge/Chrome（puppeteer-core）
// 驱动页面，验证：
//   1. 生成合成音频后频谱图渲染出内容；
//   2. 音频可播放（播放头推进）；
//   3. 默认自动识模一次解出多张图（结果画廊）；
//   4. 强制模式（锚点=开始 / 结束）各解出一张。
//
// 前置：.\\build-wasm.ps1 -Dev
// 运行：cd web && npm run e2e
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
  console.error('未找到 Edge / Chrome，无法进行浏览器端到端测试')
  process.exit(1)
}
console.log('浏览器：', executablePath)

const server = await createServer({
  root: webRoot,
  logLevel: 'warn',
  server: { host: '127.0.0.1', port: 5199, strictPort: true },
})
await server.listen()
const url = 'http://127.0.0.1:5199/'
console.log('开发服务器：', url)

const browser = await puppeteer.launch({
  executablePath,
  headless: true,
  args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'],
})
let failures = 0
function check(condition, message) {
  if (condition) console.log('  ✓', message)
  else {
    failures++
    console.error('  ✗', message)
  }
}

async function clickButtonByText(page, text) {
  const clicked = await page.evaluate((label) => {
    const button = [...document.querySelectorAll('button')].find((b) => b.textContent.includes(label))
    if (!button) return false
    button.click()
    return true
  }, text)
  if (!clicked) throw new Error(`未找到按钮：${text}`)
}

async function waitStatusContains(page, text, timeout) {
  await page.waitForFunction(
    (needle) => document.querySelector('.status')?.textContent?.includes(needle),
    { timeout, polling: 300 },
    text,
  )
}

/** 按选项文案定位并设置下拉框。 */
async function selectByOptionText(page, optionText, value) {
  await page.evaluate(
    (needle, val) => {
      const select = [...document.querySelectorAll('select')].find((s) =>
        [...s.options].some((o) => o.textContent.includes(needle)),
      )
      if (!select) throw new Error(`未找到包含选项「${needle}」的下拉框`)
      select.value = val
      select.dispatchEvent(new Event('change', { bubbles: true }))
    },
    optionText,
    value,
  )
}

/** 设置合成图片数。 */
async function setSynthCount(page, count) {
  await page.evaluate((n) => {
    const input = document.querySelector("input[type='number']")
    if (!input) throw new Error('未找到图片数输入框')
    input.value = String(n)
    input.dispatchEvent(new Event('input', { bubbles: true }))
  }, count)
}

/** 读取频谱底图绘图区内的最大像素强度。 */
async function spectrogramMax(page) {
  return page.evaluate(() => {
    const canvas = document.querySelector('.canvas-wrap canvas')
    if (!canvas) return -1
    const ctx = canvas.getContext('2d')
    const data = ctx.getImageData(50, 12, canvas.width - 70, 200).data
    let max = 0
    for (let i = 0; i < data.length; i += 4) {
      max = Math.max(max, data[i], data[i + 1], data[i + 2])
    }
    return max
  })
}

/** 等待结果画廊出现恰好 n 张且图片已加载。 */
async function waitForResults(page, expected, timeout = 180_000) {
  await page.waitForFunction(
    (n) => {
      const imgs = document.querySelectorAll('.result img')
      if (imgs.length !== n) return false
      return [...imgs].every((img) => img.naturalWidth > 0)
    },
    { timeout, polling: 300 },
    expected,
  )
}

/** 读取第 index 张结果的尺寸与平均 R。 */
async function resultInfo(page, index) {
  return page.evaluate((i) => {
    const img = document.querySelectorAll('.result img')[i]
    if (!img) return null
    const canvas = document.createElement('canvas')
    canvas.width = img.naturalWidth
    canvas.height = img.naturalHeight
    const ctx = canvas.getContext('2d')
    ctx.drawImage(img, 0, 0)
    const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data
    let sum = 0
    let n = 0
    for (let k = 0; k < data.length; k += 4) {
      sum += data[k]
      n++
    }
    return { width: canvas.width, height: canvas.height, meanR: sum / n }
  }, index)
}

try {
  const page = await browser.newPage()
  page.on('pageerror', (e) => console.error('  [pageerror]', e.message))
  page.on('workercreated', (w) =>
    w.on('console', (m) => console.log('  [worker]', m.type(), m.text())),
  )

  await page.goto(url, { waitUntil: 'networkidle0', timeout: 60_000 })
  await page.waitForFunction(
    () => [...document.querySelectorAll('select option')].some((o) => o.textContent.includes('自动识模')),
    { timeout: 60_000 },
  )
  // 合成模式选 pd120，合成两张图。
  await selectByOptionText(page, 'PD-120', 'pd120')
  await setSynthCount(page, 2)
  console.log('模式列表已加载')

  // --- 生成合成音频 → 频谱图 ---
  await clickButtonByText(page, '生成合成音频')
  await waitStatusContains(page, '已载入', 240_000)
  const maxIntensity = await spectrogramMax(page)
  console.log('频谱图最大强度：', maxIntensity)
  check(maxIntensity > 120, '频谱图渲染出高亮内容')

  // --- 音频播放（播放头推进）---
  await clickButtonByText(page, '▶ 播放')
  const advanced = await page
    .waitForFunction(
      () => {
        const el = [...document.querySelectorAll('.hint')].find((e) =>
          e.textContent.includes('播放头'),
        )
        if (!el) return false
        const match = el.textContent.match(/播放头\s+([\d.]+)s/)
        return match ? parseFloat(match[1]) > 0.3 : false
      },
      { timeout: 15_000, polling: 100 },
    )
    .then(() => true)
    .catch(() => false)
  check(advanced, '播放头随时间推进（音频可播放）')
  await clickButtonByText(page, '⏸ 暂停')

  // --- 默认自动识模：一次解出多张 ---
  await clickButtonByText(page, '解码选区')
  await waitForResults(page, 2)
  const first = await resultInfo(page, 0)
  const second = await resultInfo(page, 1)
  check(
    first?.width === 640 && first?.height === 496,
    `自动识模第 1 张 ${first?.width}×${first?.height}`,
  )
  check(
    second?.width === 640 && second?.height === 496,
    `自动识模第 2 张 ${second?.width}×${second?.height}`,
  )
  check((first?.meanR ?? 0) > 10 && (second?.meanR ?? 0) > 10, '两张图像均非全黑')
  check((await page.$eval('.status', (el) => el.textContent))?.includes('完成'), '自动识模状态完成')

  // --- 强制模式（锚点=选区开始）---
  await selectByOptionText(page, '自动识模', 'pd120')
  await page.waitForFunction(() =>
    [...document.querySelectorAll('select option')].some((o) =>
      o.textContent.includes('选区结束'),
    ),
  )
  await selectByOptionText(page, '选区结束', 'start')
  await clickButtonByText(page, '解码选区')
  await waitForResults(page, 1)
  const forcedStart = await resultInfo(page, 0)
  check((forcedStart?.meanR ?? 0) > 10, `强制模式(锚点=开始)非全黑（平均 R ${forcedStart?.meanR.toFixed(1)}）`)

  // --- 强制模式（锚点=选区结束）---
  await selectByOptionText(page, '选区结束', 'end')
  await clickButtonByText(page, '解码选区')
  await waitForResults(page, 1)
  const forcedEnd = await resultInfo(page, 0)
  check((forcedEnd?.meanR ?? 0) > 10, `强制模式(锚点=结束)非全黑（平均 R ${forcedEnd?.meanR.toFixed(1)}）`)
  check((await page.$eval('.status', (el) => el.textContent))?.includes('完成'), '强制模式状态完成')
} catch (error) {
  failures++
  console.error('E2E 异常：', error instanceof Error ? error.message : error)
} finally {
  await browser.close()
  await server.close()
}

if (failures > 0) {
  console.error(`\n浏览器端到端测试失败：${failures} 项`)
  process.exit(1)
}
console.log('\n浏览器端到端测试通过。')
