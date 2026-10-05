# Web 端 SSTV 解码工具

在浏览器中运行的 SSTV（慢扫描电视）解码工具。解码核心复用 [`slowrx`](https://github.com/jasonherald/slowrx.rs)
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

# 2) 安装前端依赖并启动开发服务器
. .\node_env.ps1
cd web
npm install
npm run dev
```

打开 <http://127.0.0.1:5173/>，选择模式后点“解码合成音频”，或选择本地音频文件。

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

实测（本机，wasm SIMD128）：PD120 合成音频 → 出图约 **5.8–5.9 s**（Node 5.7 s /
浏览器 5.9 s），与方案文档中“wasm SIMD 约 2.5–2.6× native”的预期一致。

## 里程碑状态

- [x] **M2** `slowrx-wasm` 包装 crate + Worker：Worker 内解码一张合成分片出图
- [~] **M3** 音频文件输入已接入（`decodeAudioData` → 单声道 f32）；缺少选取/补静音 UI
- [~] **M4** 强制模式路径（`setForcedMode`）已打通并验证
- [ ] **M5** 频谱图 + 时间选区交互
- [ ] **M6** 结果渲染/下载/状态完善
- [ ] **M7** V8 性能收尾

## 设计要点

- 解码是“两遍式”（缓冲约一整张图后爆发式计算），故进度为估算；解码必须放
  Web Worker（方案 4.1）。
- 强制模式必须携带解码窗口；WS 自动识模与强制模式两条路径在 UI 上可切换。
- wasm 目标依赖 `rustfft/wasm_simd` + `-C target-feature=+simd128`，实测有效。
- 音频用 `postMessage` + transferable 传递，不引入 SharedArrayBuffer。
