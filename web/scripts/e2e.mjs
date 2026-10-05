// 浏览器端到端验证：启动 Vite 开发服务器，用本机 Edge/Chrome（puppeteer-core）
// 打开页面，点击“解码合成音频”，断言 Worker 内 wasm 解码后 Canvas 出图。
//
// 前置：.\\build-wasm.ps1 -Dev  （需要 dev-synth 才能生成合成音频）
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

const browser = await puppeteer.launch({ executablePath, headless: true, args: ['--no-sandbox'] })
let failures = 0
try {
  const page = await browser.newPage()
  page.on('pageerror', (e) => console.error('  [pageerror]', e.message))
  page.on('console', (m) => console.log('  [page]', m.type(), m.text()))
  page.on('workercreated', (w) =>
    w.on('console', (m) => console.log('  [worker]', m.type(), m.text())),
  )
  page.on('response', (r) => {
    if (r.status() >= 400) console.error('  [http]', r.status(), r.url())
  })

  await page.goto(url, { waitUntil: 'networkidle0', timeout: 60_000 })
  console.log('页面已加载')

  // 等待模式列表加载完成（下拉框出现选项）。
  await page.waitForFunction(
    () => {
      const select = document.querySelector('select')
      return select && select.options.length > 0
    },
    { timeout: 60_000 },
  )
  console.log('模式列表已加载')

  // 显式选中 pd120，避免依赖列表顺序。
  await page.evaluate(() => {
    const select = document.querySelector('select')
    select.value = 'pd120'
    select.dispatchEvent(new Event('change', { bubbles: true }))
  })

  // 点击“解码合成音频”。
  const clickedAt = Date.now()
  await page.evaluate(() => {
    const buttons = [...document.querySelectorAll('button')]
    const target = buttons.find((b) => b.textContent.includes('解码合成音频'))
    if (!target) throw new Error('未找到解码按钮')
    target.click()
  })
  console.log('已触发解码，等待出图…')

  // 等待 Canvas 变为 PD120 尺寸（640×496）。
  await page.waitForFunction(
    () => {
      const canvas = document.querySelector('canvas')
      return canvas && canvas.width === 640 && canvas.height === 496
    },
    { timeout: 180_000, polling: 500 },
  )

  const status = await page.$eval('.status', (el) => el.textContent ?? '')
  const meanR = await page.evaluate(() => {
    const canvas = document.querySelector('canvas')
    const ctx = canvas.getContext('2d')
    const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data
    let sum = 0
    let n = 0
    for (let i = 0; i < data.length; i += 4) {
      sum += data[i]
      n++
    }
    return sum / n
  })
  const wallMs = Date.now() - clickedAt
  console.log('状态（原始）：', JSON.stringify(status))
  console.log('从点击到出图的墙钟耗时：', wallMs, 'ms')
  console.log('画布平均 R：', meanR.toFixed(1))

  if (status.includes('完成')) {
    console.log('  ✓ 状态显示完成')
  } else {
    failures++
    console.error('  ✗ 状态未显示完成')
  }
  if (meanR > 10) {
    console.log('  ✓ 画布非全黑（解码成功）')
  } else {
    failures++
    console.error('  ✗ 画布疑似全黑')
  }
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
