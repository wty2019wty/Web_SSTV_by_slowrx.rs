# Web 端 SSTV 解码工具

在浏览器中运行的 SSTV（慢扫描电视）解码工具。解码核心复用 [`slowrx`](https://github.com/wty2019wty/slowrx.rs/tree/sstv-web)
（Rust 库），编译为 WebAssembly 在**本地**运行；音频不上传任何服务器。

配套方案文档见 `slowrx` 仓库 `sstv-web` 分支的 `docs/`（本仓库不复制）。

---

## 目录结构

```
.
├── Cargo.toml                 # Rust 工作区
├── rust-toolchain.toml        # 固定 Rust 1.95.0（含 wasm 目标）
├── .cargo/config.toml         # wasm32 开启 SIMD128
├── build-wasm.ps1             # 构建 wasm + 生成 JS 绑定
├── crates/slowrx-wasm/        # slowrx 的 WebAssembly 包装 crate
│   └── src/
│       ├── core.rs            # 纯 Rust 解码封装（可原生测试）
│       └── lib.rs             # #[wasm_bindgen] 绑定层
├── scripts/smoke.cjs          # Node 冒烟测试（加载 wasm 解码合成音频）
└── web/                       # Vite + Vue 3 前端
    ├── src/
    │   ├── workers/decoder.worker.ts   # 解码 Worker（wasm 在这里运行）
    │   ├── lib/{protocol,decoderClient}.ts
    │   ├── wasm/              # 构建产物（gitignore，由 build-wasm.ps1 生成）
    │   └── App.vue
    └── scripts/e2e.mjs        # 浏览器端到端测试（本机 Edge/Chrome）
```

## 环境要求

| 组件 | 版本 | 说明 |
|---|---|---|
| Rust | 1.95.0 | 由 `rust-toolchain.toml` 固定；默认 stable 未必装 wasm 目标 |
| wasm-bindgen-cli | **0.2.129** | 必须与 `wasm-bindgen` crate 版本一致 |
| Node.js | 24.x | 见 `node_env.ps1` |
| wasm32-unknown-unknown | — | 已随工具链配置声明 |

安装 wasm-bindgen-cli：

```powershell
cargo install wasm-bindgen-cli --version 0.2.129 --locked
```

## 构建与运行

```powershell
# 1) 构建 wasm 并生成前端绑定（-Dev 启用合成测试音频）
.\build-wasm.ps1 -Dev

# 2) 安装前端依赖（只需一次）
npm run install:web

# 3) 启动开发服务器（根目录或 web/ 均可）
npm run dev
```

打开 <http://127.0.0.1:5173/>，选择模式后点“生成合成音频”，在频谱图上拖拽选区后点“解码选区”，
或“选择音频文件”载入本地录音。

> 根目录的 `package.json` 只是脚本入口（`npm run dev` / `build` / `e2e` / `wasm` 等会委托到
> `web/`）。前端项目本体在 `web/`，也可以 `cd web` 后直接用 `npm run dev`。

> `slowrx` 通过 git 依赖锁定在 fork 的 `sstv-web` 分支（含强制模式 / 解码窗口
> `#113`/`#114`）。构建需能访问 GitHub。

## 验证

```powershell
# Rust 单元测试（原生，覆盖自动识模 / 强制模式 / 静音门限 / 模式列表）
cargo test -p slowrx-wasm

# Node 冒烟测试：直接加载 wasm 解码合成音频并写出 out/pd120.ppm
.\build-wasm.ps1 -Node -Dev
node scripts\smoke.cjs

# 浏览器端到端：Vite + 本机 Edge/Chrome，Worker 内解码后断言 Canvas 出图
cd web
npm run e2e
```

实测（本机，wasm SIMD128）：PD120 合成音频 → 出图约 **6.0–6.4 s**（Node 6.0–6.4 s /
无头 Edge 5.9–6.1 s），与方案文档中“wasm SIMD 约 2.5–2.6× native”的预期一致。

## 里程碑状态

- [x] **M2** `slowrx-wasm` 包装 crate + Worker：Worker 内解码一张合成分片出图
- [x] **M3** 音频输入：文件（`decodeAudioData` → 单声道 f32）与合成音频；选区由 Worker
  切片刻取并**末尾补 1 s 静音**（方案 4.3）
- [x] **M4** 强制模式路径（`setForcedMode`）已打通并验证
- [x] **M5** 频谱图 + 时间选区：Worker 内用 wasm 的 rustfft 算 STFT，主线程双层 Canvas
  渲染，支持缩放/平移与选区创建/移动/两端缩放
- [x] **M6** 结果 Canvas 渲染 + PNG 下载 + 状态/进度反馈（进度为估算，见设计要点）
- [ ] **M7** V8 性能收尾（含 `wasm-opt` 体积优化、无头浏览器耗时复测）

## 设计要点

- 解码是“两遍式”（缓冲约一整张图后爆发式计算），故进度为估算；解码必须放
  Web Worker（方案 4.1）。
- 强制模式必须携带解码窗口；WS 自动识模与强制模式两条路径在 UI 上可切换。
- wasm 目标依赖 `rustfft/wasm_simd` + `-C target-feature=+simd128`，实测有效。
- 音频用 `postMessage` + transferable 传递，不引入 SharedArrayBuffer。
- 会话模型：Worker 持有当前 PCM，频谱图与选区解码都引用它，避免重复搬运大数组；
  选区解码统一在 Worker 内切片 + 补静音。
- 频谱图为 8 位强度矩阵（列优先），绝对 dBFS 参考归一化 + 伽马校正，传输/内存开销小。
- 已知体积：wasm 约 1.05 MB（生产）/ 1.08 MB（dev-synth），M7 可用 `wasm-opt` 压缩。
- 浏览器实测：无头 Edge 与 Node 下 PD120 均约 6 s（曾观察到并行压测时升高，属测量干扰）。
