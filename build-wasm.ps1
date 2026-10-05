# 构建 slowrx-wasm 并生成前端可直接 import 的 wasm-bindgen 绑定。
#
# 用法（在仓库根目录执行）：
#   .\build-wasm.ps1             # 生产构建：写入 web/src/wasm（target=web）
#   .\build-wasm.ps1 -Dev        # 开发构建：额外启用 dev-synth（合成测试音频）
#   .\build-wasm.ps1 -Node       # 生成 Node.js 绑定到 pkg-node（用于 smoke 测试）
#   .\build-wasm.ps1 -NoOpt      # 跳过 wasm-opt 体积优化
param(
    [switch]$Dev,
    [switch]$Node,
    [switch]$NoOpt
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
Push-Location $root
try {
    $featureArgs = @()
    if ($Dev) { $featureArgs = @('--features', 'dev-synth') }

    Write-Host '==> 编译 wasm（release / wasm32-unknown-unknown）' -ForegroundColor Cyan
    cargo build -p slowrx-wasm --release --target wasm32-unknown-unknown @featureArgs
    if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }

    $wasm = Join-Path $root 'target\wasm32-unknown-unknown\release\slowrx_wasm.wasm'
    if (-not (Test-Path $wasm)) { throw "未找到 wasm 产物：$wasm" }

    if ($Node) {
        $out = Join-Path $root 'pkg-node'
        Write-Host '==> wasm-bindgen（target=nodejs）' -ForegroundColor Cyan
        wasm-bindgen $wasm --out-dir $out --target nodejs --out-name slowrx_wasm
    }
    else {
        $out = Join-Path $root 'web\src\wasm'
        New-Item -ItemType Directory -Force -Path $out | Out-Null
        Write-Host '==> wasm-bindgen（target=web）' -ForegroundColor Cyan
        wasm-bindgen $wasm --out-dir $out --target web --out-name slowrx_wasm
    }
    if ($LASTEXITCODE -ne 0) { throw 'wasm-bindgen 失败' }

    if (-not $NoOpt) {
        $bg = Join-Path $out 'slowrx_wasm_bg.wasm'
        if (Test-Path $bg) {
            # 优先用项目内置 Node，避免依赖 PATH。
            $nodeExe = Join-Path $root '.node-v24.19.0-win-x64\node.exe'
            if (-not (Test-Path $nodeExe)) { $nodeExe = 'node' }
            Write-Host '==> 体积优化（wasm-opt -Oz，binaryen）' -ForegroundColor Cyan
            & $nodeExe (Join-Path $root 'web\scripts\optimize-wasm.mjs') $bg $bg
            if ($LASTEXITCODE -ne 0) { throw 'wasm 体积优化失败' }
        }
    }

    Write-Host "==> 完成：$out" -ForegroundColor Green
}
finally {
    Pop-Location
}
