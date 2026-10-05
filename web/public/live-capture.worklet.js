// 麦克风采集 AudioWorklet 处理器。
//
// 浏览器以 128 帧为单位调用 `process()`；这里把输入累积到固定大小
// （默认 4096 帧，约 85 ms @48 kHz）后，用 transferable 一次性 postMessage
// 给主线程，避免每 128 帧一次消息带来的开销。
//
// 放在 public/ 下由 Vite 原样拷贝，因此在 Worklet 全局作用域里可以直接运行
// （不经过打包/模块转换），也就没有 import 可用。
class LiveCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super()
    const opts = (options && options.processorOptions) || {}
    this.chunkSize = opts.chunkSize || 4096
    this.buffer = new Float32Array(this.chunkSize)
    this.filled = 0
  }

  process(inputs) {
    const input = inputs[0]
    if (!input || input.length === 0) return true
    const first = input[0]
    if (!first) return true
    const frames = first.length
    const channels = input.length

    for (let i = 0; i < frames; i++) {
      // 多声道取平均 → 单声道。
      let sample = 0
      for (let c = 0; c < channels; c++) sample += input[c][i]
      sample /= channels
      this.buffer[this.filled++] = sample
      if (this.filled === this.chunkSize) {
        const out = this.buffer
        this.port.postMessage(out, [out.buffer])
        this.buffer = new Float32Array(this.chunkSize)
        this.filled = 0
      }
    }
    return true
  }
}

registerProcessor('live-capture', LiveCaptureProcessor)
