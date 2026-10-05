// 麦克风授权探针：验证点击「开始接收」时是否真的调用了 getUserMedia。
// 与 e2e.mjs 的区别：**不加** --use-fake-ui-for-media-stream（不自动放行），
// 并在页面加载前包裹 navigator.mediaDevices.getUserMedia 统计调用次数与结果。
// 运行：node scripts/probe-mic.mjs
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
console.log('浏览器：', executablePath)

const server = await createServer({
  root: webRoot,
  logLevel: 'warn',
  server: { host: '127.0.0.1', port: 5198, strictPort: true },
})
await server.listen()
const url = 'http://127.0.0.1:5198/'
console.log('开发服务器：', url)

const browser = await puppeteer.launch({
  executablePath,
  headless: true,
  // 只提供伪设备；默认不自动放行权限，以便观察真实请求。
  // PROBE_FAKE_UI=1 时改用自动放行，验证授权成功路径。
  args: [
    '--no-sandbox',
    '--use-fake-device-for-media-stream',
    ...(process.env.PROBE_FAKE_UI === '1' ? ['--use-fake-ui-for-media-stream'] : []),
  ],
})

try {
  const page = await browser.newPage()
  page.on('pageerror', (e) => console.error('[pageerror]', e.message))
  await page.evaluateOnNewDocument(() => {
    window.__gum = { calls: 0, missing: false, args: [], result: null, error: null }
    const md = navigator.mediaDevices
    if (!md || !md.getUserMedia) {
      window.__gum.missing = true
      return
    }
    const orig = md.getUserMedia.bind(md)
    md.getUserMedia = async (constraints) => {
      window.__gum.calls++
      window.__gum.args.push(JSON.stringify(constraints))
      try {
        const stream = await orig(constraints)
        window.__gum.result = 'resolved'
        return stream
      } catch (e) {
        window.__gum.error = `${e.name}: ${e.message}`
        throw e
      }
    }
  })

  await page.goto(url, { waitUntil: 'networkidle0', timeout: 60_000 })
  await page.waitForFunction(
    () =>
      [...document.querySelectorAll('select option')].some((o) =>
        o.textContent.includes('自动识模'),
      ),
    { timeout: 60_000 },
  )

  const click = (label) =>
    page.evaluate((text) => {
      const b = [...document.querySelectorAll('button')].find((x) => x.textContent.includes(text))
      if (!b) return { found: false, disabled: null }
      const disabled = b.disabled
      b.click()
      return { found: true, disabled }
    }, label)

  console.log('切到麦克风：', await click('麦克风'))
  await new Promise((r) => setTimeout(r, 300))

  const env = await page.evaluate(() => ({
    isSecureContext: window.isSecureContext,
    href: location.href,
    hasMediaDevices: !!navigator.mediaDevices,
    hasGetUserMedia: !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia),
  }))
  const warnHints = await page.evaluate(() =>
    [...document.querySelectorAll('.hint.warn')].map((e) => e.textContent.trim()),
  )
  console.log('环境：', env)
  console.log('环境告警：', warnHints)

  console.log('点开始接收：', await click('开始接收'))
  await new Promise((r) => setTimeout(r, 5000))

  const gum = await page.evaluate(() => window.__gum)
  const liveStatus = await page.$eval('.live-status', (e) => e.textContent.trim()).catch(() => '(无)')
  console.log('getUserMedia 统计：', JSON.stringify(gum, null, 2))
  console.log('实时状态：', liveStatus)
} catch (error) {
  console.error('探针异常：', error)
} finally {
  await browser.close()
  await server.close()
}
