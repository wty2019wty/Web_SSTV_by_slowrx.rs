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
├── crates/slowrx/             # 内联的 slowrx 解码核心（原 fork 的 sstv-web 分支）
│   └── src/decoder.rs         # 含新增的「渐进（实时）解码」模式
├── crates/slowrx-wasm/        # slowrx 的 WebAssembly 包装 crate
│   └── src/
│       ├── core.rs            # 纯 Rust 解码封装（可原生测试）
│       └── lib.rs             # #[wasm_bindgen] 绑定层
├── scripts/smoke.cjs          # Node 冒烟测试（加载 wasm 解码合成音频）
└── web/                       # Vite + Vue 3 前端
    ├── public/
    │   └── live-capture.worklet.js     # 麦克风采集 AudioWorklet（原样拷贝）
    ├── src/
    │   ├── workers/decoder.worker.ts   # 解码 / 频谱 / 实时接收（wasm 在这里运行）
    │   ├── components/SpectrogramView.vue
    │   ├── components/WaterfallView.vue # 实时接收的滚动瀑布图
    │   ├── components/LiveImageView.vue # 实时接收的逐行图像
    │   ├── lib/{protocol,decoderClient,liveCapture,colorMap}.ts
    │   ├── wasm/              # 构建产物（gitignore，由 build-wasm.ps1 生成）
    │   └── App.vue
    └── scripts/
        ├── e2e.mjs            # 浏览器端到端测试（本机 Edge/Chrome，含实时接收）
        ├── bench.mjs          # 浏览器 V8 性能基准
        └── optimize-wasm.mjs  # binaryen（wasm-opt -Oz）体积优化
```

## 环境要求

| 组件 | 版本 | 说明 |
|---|---|---|
| Rust | 1.95.0 | 由 `rust-toolchain.toml` 固定；默认 stable 未必装 wasm 目标 |
| wasm-bindgen-cli | **0.2.129** | 必须与 `wasm-bindgen` crate 版本一致 |
| Node.js | 24.x | 见 `node_env.ps1` |
| binaryen（npm） | 132.x | `build-wasm.ps1` 用它做 `wasm-opt -Oz` 体积优化（`web` 的 devDependency） |
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
或“选择音频文件”载入本地录音；在「1. 音频来源」切到「🎙 麦克风」再点「开始接收」即可实时
逐行接收（需 HTTPS/localhost）。

> 根目录的 `package.json` 只是脚本入口（`npm run dev` / `build` / `e2e` / `wasm` 等会委托到
> `web/`）。前端项目本体在 `web/`，也可以 `cd web` 后直接用 `npm run dev`。

> `slowrx` 解码核心已**内联**到 `crates/slowrx`（原 fork 的 `sstv-web` 分支，含强制模式 /
> 解码窗口 `#113`/`#114`）。内联是为了加入「渐进（实时）解码」模式；不再需要构建时
> 访问 GitHub。若要与上游 fork 同步，需手动合并 `crates/slowrx`。

## 部署（GitHub Pages）

推送到 `main` 或 `page` 分支（也可在 Actions 页面手动触发）会运行
`.github/workflows/deploy-pages.yml`：在 Ubuntu runner 上执行
`cargo build → wasm-bindgen → npm ci → vite build`，并把 `web/dist` 发布到
GitHub Pages。

站点地址：<https://wty2019wty.github.io/Web_SSTV_by_slowrx.rs/>

首次使用需在仓库 **Settings → Pages** 里把 **Source** 设为 **GitHub Actions**。
工作流由 GitHub 注入的 `GITHUB_REPOSITORY` 自动推导 Vite 的 `base`
（`/Web_SSTV_by_slowrx.rs/`），本地开发仍回退到根路径；如部署到别处可用
`VITE_BASE` 覆盖。若从非默认分支（如 `page`）部署，请在
**Settings → Environments → github-pages** 的部署分支策略中放行该分支。

## 验证

```powershell
# Rust 单元测试（原生，覆盖自动识模 / 强制模式 / 静音门限 / 模式列表 / 渐进解码）
cargo test -p slowrx-wasm

# 内联解码核心的完整测试（含各模式合成 round-trip、多图、强制模式）
cargo test -p slowrx --features test-support

# Node 冒烟测试：直接加载 wasm 解码合成音频并写出 out/pd120.ppm
.\build-wasm.ps1 -Node -Dev
node scripts\smoke.cjs

# 浏览器端到端：Vite + 本机 Edge/Chrome，Worker 内解码后断言 Canvas 出图
cd web
npm run e2e

# 浏览器 V8 性能基准：各模式合成音频的解码耗时
npm run bench
```

构建脚本会在 wasm-bindgen 之后自动用 binaryen 做 `wasm-opt -Oz` 体积优化
（`build-wasm.ps1 -NoOpt` 可跳过）。体积对比：

| 产物 | 大小 |
|---|---|
| 优化前（wasm-bindgen 输出，dev-synth） | 1055 KB |
| + `panic=abort` / `strip` | 970 KB |
| + `wasm-opt -Oz` | **683 KB**（dev-synth） / **669 KB**（生产，gzip 193 KB） |

### 浏览器 V8 实测（无头 Edge，wasm SIMD128，仅解码循环）

| 模式 | 图像时长 | 解码耗时 | 与 wasmtime(Cranelift) 基线 |
|---|---|---|---|
| PD-120 | 124 s | 5.95 s | 5.77 s |
| PD-180 | 180 s | 7.55 s | 7.53 s |
| PD-240 | 240 s | 9.14 s | 9.25 s |
| Robot 36 | 36 s | 2.13 s | ~2–3 s |
| Robot 72 | 72 s | 3.70 s | — |
| Scottie 1 | 110 s | 4.77 s | — |
| Martin 1 | 114 s | 5.91 s | — |

结论：**V8 与 wasmtime(Cranelift) 基本持平（差异 < ~3%）**，方案文档中“wasm SIMD 约
2.5–2.6× native、PD120≈5.77 s”的预期在浏览器端得到确认；`PIXEL_FFT_STRIDE` 无需调整。

## 里程碑状态

- [x] **M2** `slowrx-wasm` 包装 crate + Worker：Worker 内解码一张合成分片出图
- [x] **M3** 音频输入：文件（`decodeAudioData` → 单声道 f32）与合成音频；解码统一在
  Worker 内处理，**末尾补 2 s 静音**（方案 4.3）
- [x] **M4** 强制模式路径：**直接用播放头位置当锚点**（在频谱图上点击/拖拽或用播放定位），
  由「起点 / 终点」二选一决定该位置的语义（`starting_at` / `ending_at`）；不使用范围选区，
  从锚点喂到文件末尾，长度由模式标称时长决定
- [x] **M5** 频谱图 + 时间选区：Worker 内用 wasm 的 rustfft 算 STFT，主线程双层 Canvas
  渲染，支持缩放/平移（滚轮 + **横向滚动条**）与选区创建/移动/两端缩放
- [x] **M6** 结果渲染 + 逐张 PNG 下载 + 状态/进度；**默认自动识模**（一次可解出多张，
  结果画廊展示）；音频播放（整段 / 选区）、播放头同步、点击频谱图定位
- [x] **M7** V8 性能收尾：浏览器实测各模式耗时（V8 ≈ Cranelift）；`panic=abort`+`strip`
  + binaryen `wasm-opt -Oz` 把 wasm 从 1055 KB 压到 **683 KB（-35%）**；`PIXEL_FFT_STRIDE`
  无需调整
- [x] **M8** 实时接收（麦克风）：`getUserMedia` → `AudioWorklet`（已关闭回声消除/降噪/
  自动增益）→ Worker 内**常驻解码器**（自动识模）+ **流式 STFT 瀑布图** + **逐行实时图像**；
  采集/接收的 PCM 全部在本机内存中，不上传
- [x] **M9** 渐进（实时）解码：内联 slowrx 核心并新增渐进模式——检测到 VIS 后约 1–2 行
  内锁定同步，之后**每收够一行就解一行**（`line` 事件）逐行绘制；整图末尾用**完整 sync
  轨道重解一次**，保证最终图与离线批处理**逐像素一致**。首图延迟≈1–2 行，且解码 CPU
  摊到整段接收时间上
- [x] **M10** 实时质量工具：**输入诊断面板**（实际生效的采样率/声道/回声消除/降噪/AGC +
  实时峰值/RMS/削波/动态范围）、**实时录音**（停止后一键用**离线文件路径**重解，作同源
  权威结果）、**停止收尾精修**（中途停止时用完整 sync 重解已收行，产出标记「未完整」的图）

## 设计要点

- 离线/文件解码仍是“两遍式”（缓冲约一整张图后爆发式计算），故选区解码的进度为估算；
  实时接收改用渐进模式逐行解码（见下），不在此列。解码必须放 Web Worker（方案 4.1）。
- 强制模式必须携带解码窗口；WS 自动识模与强制模式两条路径在 UI 上可切换。
- wasm 目标依赖 `rustfft/wasm_simd` + `-C target-feature=+simd128`，实测有效。
- 音频用 `postMessage` + transferable 传递，不引入 SharedArrayBuffer。
- 播放：Worker 在载入完成后回传一份 PCM 副本，主线程用 Web Audio API
  (`AudioBufferSourceNode`) 播放；播放头由 `AudioContext.currentTime` 驱动并绘制在
  频谱图上，移出视口时自动跟随。
- 会话模型：Worker 持有当前 PCM，频谱图与选区解码都引用它，避免重复搬运大数组。
- 两条解码路径（方案 6.5）：
  - **自动识模（默认）**：只喂入选区切片 + 末尾静音，按选区内的 VIS 头依次解码，
    可能一次得到多张图；结果以画廊展示、逐张下载；
  - **强制模式**：**直接用播放头位置当锚点**（点击/拖拽频谱图或用播放定位），由
    「起点 / 终点」决定语义（`starting_at` / `ending_at`），不显示范围选区；从锚点喂到
    文件末尾 + 末尾静音，长度由模式标称时长决定。
- 频谱图为 8 位强度矩阵（列优先），绝对 dBFS 参考归一化 + 伽马校正，传输/内存开销小。
- 体积：`panic=abort` + `strip` + binaryen `wasm-opt -Oz` 后生产版约 **669 KB**
  （gzip 193 KB），dev-synth 版约 683 KB；`build-wasm.ps1 -NoOpt` 可跳过优化。
- 性能：浏览器 V8 与 wasmtime(Cranelift) 基本持平；PD120≈5.9 s、PD180≈7.6 s、
  PD240≈9.1 s（SIMD128）。详见上面的基准表。
- 实时接收（渐进解码）：内联的 slowrx 核心新增**渐进模式**（`SstvDecoder::set_progressive`，
  WASM 侧 `WasmDecoder.setProgressive`）。检测到 VIS 后约 1–2 行即可从 sync 下降沿锁定
  `skip` 并开始逐行解码；`rate` 的斜率修正等约 8 行、Hough 可靠后再采用，之后每 8 行用
  更多 sync 重估一次（只影响未解码的行）。整图收完后**用整段 sync 重跑 `find_sync` 并整图
  重解**，因此 `ImageComplete` 与离线批处理逐像素一致（原生单测 `progressive_streams_lines_and_matches_batch`
  覆盖）。逐行解码把 CPU 摊到整段接收时间上，不再有整图突发卡顿。
- 实时瀑布图由 wasm 侧新增的**流式 STFT**（`StreamingSpectrogram`，复用 rustfft）
  增量输出新列，与离线频谱图使用同一套加窗/归一化（有单元测试保证逐字节一致）；
  主线程用离屏画布自滚动渲染，1 列 = 1 像素。逐行实时图像用 `putImageData` 脏矩形
  只刷新一行；整图末尾的重解发来同样的行，尺寸不变时不重画，避免闪烁。
- 实时质量诊断：`getUserMedia` 的约束可能被浏览器/驱动覆盖（尤其 AGC），因此面板显示
  `track.getSettings()` 的**实际生效值**，并统计实时峰值/RMS/削波/动态范围——用于区分
  「拾音链路问题」与「解码问题」。
- 实时录音 + 离线重解：接收的同时把 PCM 录在本地（上限 5 分钟），停止后可一键载入文件
  解码路径重解，得到同源的权威结果（也便于用同一段音频对比实时与离线）。
- 停止收尾：`SstvDecoder::finalize()` 用已收集的完整 sync 重解已到齐的行，产出一张
  `partial: true` 的图（UI 标记「未完整」），避免中途停止只剩未经精修的预览。
