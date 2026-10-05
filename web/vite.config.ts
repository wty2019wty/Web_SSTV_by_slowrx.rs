import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// Web 端 SSTV 工具。纯前端、无后端；解码在 Web Worker 内的 wasm 中完成。
export default defineConfig({
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
