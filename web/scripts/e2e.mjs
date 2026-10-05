// 浏览器端到端验证：启动 Vite 开发服务器，用本机 Edge/Chrome（puppeteer-core）
// 驱动页面，验证：
//   1. 生成合成音频后频谱图渲染出内容；
//   2. VIS 自动识模解码选区出图；
//   3. 强制模式（无 VIS）同样出图。
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

/** 读取频谱底图（第一块 canvas）绘图区内的最大像素强度。 */
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

/** 读取结果 canvas 的平均 R。 */
async function resultMeanR(page) {
  return page.evaluate(() => {
    const canvas = document.querySelector('canvas.preview')
    if (!canvas || !canvas.width) return -1
    const data = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data
    let sum = 0
    let n = 0
    for (let i = 0; i < data.length; i += 4) {
      sum += data[i]
      n++
    }
    return sum / n
  })
}

/** 按索引切换复选框（0=synthWithVis，1=forcedMode）。 */
async function toggleCheckbox(page, index) {
  await page.evaluate((i) => {
    const boxes = document.querySelectorAll('input[type=checkbox]')
    boxes[i]?.click()
  }, index)
}

/** 设置强制模式锚点选择框（按选项文案定位）。 */
async function selectAnchor(page, value) {
  await page.evaluate((val) => {
    const select = [...document.querySelectorAll('select')].find((s) =>
      [...s.options].some((o) => o.textContent.includes('选区结束')),
    )
    if (!select) throw new Error('未找到锚点选择框')
    select.value = val
    select.dispatchEvent(new Event('change', { bubbles: true }))
  }, value)
}

/** 触发解码并断言出图。 */
async function decodeAndCheck(page, label) {
  await clickButtonByText(page, '解码选区')
  await page.waitForFunction(
    () => {
      const c = document.querySelector('canvas.preview')
      return c && c.width === 640 && c.height === 496
    },
    { timeout: 180_000, polling: 500 },
  )
  const meanR = await resultMeanR(page)
  check(meanR > 10, `${label}画布非全黑（平均 R ${meanR.toFixed(1)}）`)
  const status = await page.$eval('.status', (el) => el.textContent ?? '')
  check(status.includes('完成'), `${label}状态完成`)
}

try {
  const page = await browser.newPage()
  page.on('pageerror', (e) => console.error('  [pageerror]', e.message))
  page.on('workercreated', (w) =>
    w.on('console', (m) => console.log('  [worker]', m.type(), m.text())),
  )

  await page.goto(url, { waitUntil: 'networkidle0', timeout: 60_000 })
  await page.waitForFunction(
    () => document.querySelector('select')?.options.length > 0,
    { timeout: 60_000 },
  )
  await page.evaluate(() => {
    const select = document.querySelector('select')
    select.value = 'pd120'
    select.dispatchEvent(new Event('change', { bubbles: true }))
  })
  console.log('模式列表已加载')

  // --- 生成合成音频 → 频谱图 ---
  await clickButtonByText(page, '生成合成音频')
  await waitStatusContains(page, '已载入', 120_000)
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

  // --- 自动识模解码选区 ---
  await decodeAndCheck(page, '自动识模')

  // --- 强制模式（合成无 VIS），锚点=选区开始 ---
  await toggleCheckbox(page, 0) // 关闭“包含 VIS 头”
  await clickButtonByText(page, '生成合成音频')
  await waitStatusContains(page, '已载入', 120_000)
  await toggleCheckbox(page, 1) // 打开“强制模式”
  await selectAnchor(page, 'start')
  await decodeAndCheck(page, '强制模式(锚点=开始)')

  // --- 强制模式，锚点=选区结束 ---
  await selectAnchor(page, 'end')
  await decodeAndCheck(page, '强制模式(锚点=结束)')
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
