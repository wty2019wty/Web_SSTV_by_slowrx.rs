// 用 binaryen（wasm-opt 的 JS 端口）对 wasm 做体积优化。
// 用法：node optimize-wasm.mjs <input.wasm> <output.wasm>
//
// 选用 -Oz 等价配置（optimize level 2 + shrink level 2，体积优先）。
// 本模块启用了 SIMD128，二进制里含 SIMD 指令，需由 binaryen 正常解析。
import fs from 'node:fs'
import binaryen from 'binaryen'

const [, , input, output] = process.argv
if (!input || !output) {
  console.error('用法: node optimize-wasm.mjs <input.wasm> <output.wasm>')
  process.exit(1)
}

const data = fs.readFileSync(input)
const module = binaryen.readBinary(data)
try {
  // 只启用标准特性：panic=abort 的 wasm 会用到 bulk memory / sign-ext 等，
  // 需要显式打开；但不能用 Features.All（含 Multibyte 等实验特性，
  // 会产出 V8 无法解析的模块）。
  const F = binaryen.Features
  module.setFeatures(
    F.MVP |
      F.MutableGlobals |
      F.NontrappingFPToInt |
      F.SIMD128 |
      F.BulkMemory |
      F.BulkMemoryOpt |
      F.SignExt |
      F.ReferenceTypes |
      F.Multivalue |
      F.ExtendedConst,
  )
  // 等价于 wasm-opt -Oz。
  binaryen.setOptimizeLevel(2)
  binaryen.setShrinkLevel(2)
  module.optimize()
  if (!module.validate()) {
    throw new Error('优化后的 wasm 未通过校验')
  }
  const optimized = module.emitBinary()
  fs.writeFileSync(output, optimized)
  const before = data.length
  const after = optimized.length
  const pct = (100 * (1 - after / before)).toFixed(1)
  console.log(
    `wasm-opt(-Oz)：${(before / 1024).toFixed(1)} KB → ${(after / 1024).toFixed(1)} KB（-${pct}%）`,
  )
} finally {
  module.dispose()
}
