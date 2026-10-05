// 频谱/瀑布图共用的 magma 近似颜色映射。
//
// 抽取自原 SpectrogramView 内联实现；流式瀑布图（WaterfallView）复用同一份，
// 保证两处观感一致，也避免 LUT 重复构建。

export const COLOR_STOPS: ReadonlyArray<readonly [number, number, number]> = [
  [0, 0, 4],
  [28, 16, 68],
  [79, 18, 123],
  [129, 37, 129],
  [181, 54, 122],
  [229, 80, 100],
  [251, 135, 97],
  [254, 194, 135],
  [252, 253, 191],
]

/** 256 级强度 → RGB 的查找表，长度 256*3。 */
export const MAGMA_LUT: Uint8ClampedArray = (() => {
  const lut = new Uint8ClampedArray(256 * 3)
  for (let i = 0; i < 256; i++) {
    const t = (i / 255) * (COLOR_STOPS.length - 1)
    const i0 = Math.floor(t)
    const i1 = Math.min(i0 + 1, COLOR_STOPS.length - 1)
    const f = t - i0
    const a = COLOR_STOPS[i0]
    const b = COLOR_STOPS[i1]
    lut[i * 3] = a[0] + (b[0] - a[0]) * f
    lut[i * 3 + 1] = a[1] + (b[1] - a[1]) * f
    lut[i * 3 + 2] = a[2] + (b[2] - a[2]) * f
  }
  return lut
})()
