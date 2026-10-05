import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// vite.config.ts 运行在 Node 环境；这里声明最小化的 process 类型，
// 免得仅为读取环境变量就引入 @types/node（前端代码本身不依赖 Node API）。
declare const process: { env: Record<string, string | undefined> }

// GitHub Pages 的项目站点部署在 /<repo>/ 子路径下（本仓库为 /Web_SSTV_by_slowrx.rs/）。
// - CI：GitHub 会注入 GITHUB_REPOSITORY=owner/repo，据此自动推导 base；
// - 本地：无该变量时回退到根路径，也可用 VITE_BASE 显式覆盖。
const repo = process.env.GITHUB_REPOSITORY?.split('/')[1]
const base = process.env.VITE_BASE ?? (repo ? `/${repo}/` : '/')

// Web 端 SSTV 工具。纯前端、无后端；解码在 Web Worker 内的 wasm 中完成。
export default defineConfig({
  base,
  plugins: [vue()],
  worker: {
    // Worker 使用 ES 模块，便于直接 import wasm-bindgen 生成的模块。
    format: 'es',
  },
  server: {
    // getUserMedia 需要安全上下文；localhost 默认即为安全上下文。
    host: '127.0.0.1',
    port: 5173,
  },
  build: {
    target: 'es2022',
  },
})
