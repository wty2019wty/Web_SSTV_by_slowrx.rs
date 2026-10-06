//! SSTV mode specifications.
//!
//! Translated from slowrx's `modespec.c` (Oona Räisänen, ISC License).
//! See `NOTICE.md` for full attribution.
//!
//! Implemented modes: PD50, PD90, PD120, PD160, PD180, PD240, PD290,
//! Robot 24, Robot 36, Robot 72, Scottie 1, Scottie 2, Scottie DX,
//! Martin 1, Martin 2, Wraase SC2-180. All RGB-sequential modes
//! (Scottie + Martin + Wraase) share a single decode path; the per-line
//! offsets branch on [`SyncPosition`] and the channel order on
//! [`RgbOrder`].
//!
//! Timing data for the PD family (PD50/PD90/PD120/PD160/PD180/PD240/
//! PD290) and Wraase SC2-180 is taken from JL Barber (N7CXI), "Proposal
//! for SSTV Mode Specifications", Dayton SSTV forum, 20 May 2000 (the
//! "Dayton Paper"), which is sourced from the mode authors (Don Rotier /
//! Paul Turner for PD). Robot/Scottie/Martin timings are translated from
//! slowrx's `modespec.c` and match the Dayton Paper digit-for-digit.

/// SSTV operating mode. Implemented: [`SstvMode::Pd50`], [`SstvMode::Pd90`],
/// [`SstvMode::Pd120`], [`SstvMode::Pd160`], [`SstvMode::Pd180`],
/// [`SstvMode::Pd240`], [`SstvMode::Pd290`], [`SstvMode::Robot24`],
/// [`SstvMode::Robot36`], [`SstvMode::Robot72`], [`SstvMode::Scottie1`],
/// [`SstvMode::Scottie2`], [`SstvMode::ScottieDx`], [`SstvMode::Martin1`],
/// [`SstvMode::Martin2`], [`SstvMode::WraaseSc2_180`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SstvMode {
    /// PD-50. VIS `0x5D`. 320×256. See [`for_mode`] for full timing.
    Pd50,
    /// PD-90. VIS `0x63`. 320×256.
    Pd90,
    /// PD-120. VIS `0x5F`. 640×496.
    Pd120,
    /// PD-160. VIS `0x62`. 512×400.
    Pd160,
    /// PD-180. VIS `0x60`. 640×496.
    Pd180,
    /// PD-240. VIS `0x61`. 640×496.
    Pd240,
    /// PD-290. VIS `0x5E`. 800×616.
    Pd290,
    /// Robot 24 (conventional name — decode buffer is ~36 s). VIS `0x04`.
    Robot24,
    /// Robot 36. VIS `0x08`.
    Robot36,
    /// Robot 72. VIS `0x0C`.
    Robot72,
    /// Scottie 1. VIS `0x3C`.
    Scottie1,
    /// Scottie 2. VIS `0x38`.
    Scottie2,
    /// Scottie DX. VIS `0x4C`.
    ScottieDx,
    /// Martin 1. VIS `0x2C`.
    Martin1,
    /// Martin 2. VIS `0x28`.
    Martin2,
    /// Wraase SC2-180. VIS `0x37` (55d). 320×256, 逐行 RGB（R→G→B），
    /// 行内**没有**分隔脉冲（Dayton Paper「WRASSE SC2-180」）。
    WraaseSc2_180,
}

/// Mode timing + layout table entry.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct ModeSpec {
    /// The mode this entry describes.
    pub mode: SstvMode,
    /// CLI/filename slug. Stable across releases (filenames like
    /// `img-NNN-{short_name}.png` depend on this). lowercase, no
    /// separators: "pd120", "robot24", "scottiedx", "scottie1",
    /// "martin1", etc. (audit #91 B13)
    pub short_name: &'static str,
    /// Human-readable mode name. For log lines and any future
    /// user-facing display. "PD-120", "Robot 24", "Scottie DX", etc.
    /// (audit #91 B13)
    pub name: &'static str,
    /// 7-bit VIS code identifying this mode on the wire.
    pub vis_code: u8,
    /// Visible image width in pixels.
    pub line_pixels: u32,
    /// Total visible scan lines per image.
    pub image_lines: u32,
    /// Total per-line duration including sync + porches, seconds.
    pub line_seconds: f64,
    /// Sync pulse duration, seconds.
    pub sync_seconds: f64,
    /// Porch (post-sync settling) duration, seconds.
    pub porch_seconds: f64,
    /// Per-pixel duration within a colour channel, seconds.
    pub pixel_seconds: f64,
    /// Channel separator pulse duration, seconds. Translated from slowrx's
    /// `SeptrTime` field (`modespec.c`). Zero for all PD-family modes and
    /// Wraase SC2-180 (its scans run back-to-back); non-zero for Robot,
    /// Martin, and Scottie modes (V2). Stored here so the
    /// `chan_starts_sec` formula in `mode_pd::decode_pd_line_pair` matches
    /// slowrx's `video.c:88-92` term-for-term and won't silently break when
    /// non-PD modes are added.
    pub septr_seconds: f64,
    /// Channel layout used by per-mode decoders.
    pub channel_layout: ChannelLayout,
    /// Channel order within a radio line. Only meaningful for
    /// [`ChannelLayout::RgbSequential`] (see [`RgbOrder`]); PD/Robot
    /// layouts put Y/Cr/Cb on the wire and carry the `Gbr` placeholder.
    pub rgb_order: RgbOrder,
    /// Where the sync pulse sits within a radio line. See [`SyncPosition`]
    /// for the rationale (V2 carve-out forcing mid-line sync to be
    /// explicit when V2.3 Scottie lands).
    pub sync_position: SyncPosition,
}

/// Per-mode channel arrangement. PD-family modes use
/// [`ChannelLayout::PdYcbcr`]; Robot family uses [`ChannelLayout::RobotYuv`].
/// Future V2 mode families (Scottie, Martin) add their own values.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelLayout {
    /// PD-family: Y(odd) → Cr → Cb → Y(even). One radio line carries
    /// two image rows; chroma is shared between paired rows.
    PdYcbcr,
    /// Robot family: Y (single luma channel) plus chroma. R36/R24 carry
    /// alternating Cr/Cb per radio line with each chroma sample
    /// duplicated to the next image row; R72 carries Y/U/V sequentially
    /// per line. The shape difference is mode-internal — see
    /// `mode_robot::decode_line` for the per-mode dispatch.
    RobotYuv,
    /// Sequential single-line RGB layout — three channels per radio
    /// line. Used by Scottie (G→B→R, sync mid-line), Martin (G→B→R,
    /// sync at line start), and Wraase SC2-180 (R→G→B, sync at line
    /// start). The wire order is per-mode — see [`RgbOrder`].
    RgbSequential,
}

/// Where the sync pulse sits within a radio line.
///
/// PD/Robot/Martin all place sync at line start (the standard SSTV
/// convention). Scottie modes are the exception — sync sits between B
/// and R channels, not at line start. Stored here so future mode
/// decoders are forced to make their sync placement explicit at dispatch
/// time, surfacing the V1 line-clock-advance assumption that sync ==
/// line start.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyncPosition {
    /// Sync pulse at the start of each radio line. PD, Robot, Martin,
    /// Wraase SC2-180. Scottie family uses [`SyncPosition::Scottie`]
    /// instead.
    LineStart,
    /// Sync pulse between B and R within each radio line. Scottie family.
    Scottie,
}

/// Channel order within a radio line — only meaningful for
/// [`ChannelLayout::RgbSequential`].
///
/// Scottie and Martin transmit G→B→R; Wraase SC2-180 transmits R→G→B
/// (Dayton Paper: "SCAN SEQUENCE Red, Green, Blue"). Kept on
/// [`ModeSpec`] for the same reason as [`SyncPosition`]: it is a
/// wire-format fact that the RGB-sequential decoder and the synthetic
/// test encoder must both honour, so it is made explicit here instead
/// of hiding "channel 0 is Green" inside the decoder.
///
/// PD/Robot layouts put Y/Cr/Cb (not RGB) on the wire; their specs
/// carry the `Gbr` placeholder and nothing reads the field.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RgbOrder {
    /// G→B→R — Scottie 1/2/DX, Martin 1/2.
    Gbr,
    /// R→G→B — Wraase SC2-180.
    Rgb,
}

impl RgbOrder {
    /// The RGB component (0 = R, 1 = G, 2 = B) of each transmitted
    /// channel, in wire order: `Gbr` → `[1, 2, 0]` (G first, then B,
    /// then R), `Rgb` → `[0, 1, 2]`.
    ///
    /// Both directions use this single table so encode and decode stay
    /// symmetric: the decoder writes `rgb[wire[k]] = chan[k]`, the test
    /// encoder emits `rgb[wire[k]]` for channel `k`.
    #[must_use]
    pub(crate) fn wire_rgb_indices(self) -> [usize; 3] {
        match self {
            RgbOrder::Gbr => [1, 2, 0],
            RgbOrder::Rgb => [0, 1, 2],
        }
    }
}

impl ModeSpec {
    /// Offset (seconds) applied to the raw `xmax`-derived skip to
    /// land on line 0's content start. `LineStart` modes return 0;
    /// `Scottie` modes return `-chan_len/2 + 2 × porch_seconds` (the
    /// Scottie sync is mid-line, so the slip-wrapped `xmax` needs
    /// to be hoisted back left to align with line 0's content
    /// start). Audit #88 B4.
    #[must_use]
    pub(crate) fn skip_correction_seconds(&self) -> f64 {
        match self.sync_position {
            SyncPosition::LineStart => 0.0,
            SyncPosition::Scottie => {
                let chan_len = f64::from(self.line_pixels) * self.pixel_seconds;
                -chan_len / 2.0 + 2.0 * self.porch_seconds
            }
        }
    }
}

/// Look up the [`ModeSpec`] for a given 7-bit VIS code. Returns `None`
/// if the code is reserved, undefined, or maps to a mode not yet
/// implemented in this release. Derived from `ALL_SPECS`.
///
/// VIS codes are taken from Dave Jones (KB4YZ), 1998: "List of SSTV
/// Modes with VIS Codes".
///
/// **Parity-audit note (#27):** `0x00` is intentionally unmapped and
/// returns `None`. In slowrx (`vis.c:172-174`), an unknown VIS code causes
/// `GetVIS()` to return 0 and `Listen()` loops back to re-detect
/// (`do { ... } while (Mode == 0)`). Rust's equivalent is `None` from
/// this function: the caller in `SstvDecoder::process` emits
/// `SstvEvent::UnknownVis`, reseeds the VIS detector on the post-stop-bit
/// residue, and stays in `AwaitingVis` — the same "try again" effect as
/// slowrx's re-detect loop (slowrx's `printf("Unknown VIS")` becomes the
/// `UnknownVis` event). An unknown code is never an `Error`.
#[must_use]
pub fn lookup(vis_code: u8) -> Option<ModeSpec> {
    ALL_SPECS.iter().find(|s| s.vis_code == vis_code).copied()
}

/// Look up the [`ModeSpec`] for an [`SstvMode`].
///
/// Total over [`SstvMode`] — every implemented variant has a `const`
/// entry. Adding a new variant without adding its `const ModeSpec`
/// (and an arm here) is a compile error, by design. Pair with
/// [`lookup`] when starting from a VIS code on the wire.
#[must_use]
pub fn for_mode(mode: SstvMode) -> ModeSpec {
    match mode {
        SstvMode::Pd50 => PD50,
        SstvMode::Pd90 => PD90,
        SstvMode::Pd120 => PD120,
        SstvMode::Pd160 => PD160,
        SstvMode::Pd180 => PD180,
        SstvMode::Pd240 => PD240,
        SstvMode::Pd290 => PD290,
        SstvMode::Robot24 => ROBOT24,
        SstvMode::Robot36 => ROBOT36,
        SstvMode::Robot72 => ROBOT72,
        SstvMode::Scottie1 => SCOTTIE1,
        SstvMode::Scottie2 => SCOTTIE2,
        SstvMode::ScottieDx => SCOTTIE_DX,
        SstvMode::Martin1 => MARTIN1,
        SstvMode::Martin2 => MARTIN2,
        SstvMode::WraaseSc2_180 => WRAASE_SC2_180,
    }
}

/// The [`ModeSpec`] for every implemented mode, in table order.
///
/// Useful for tooling that needs to enumerate modes (e.g. a CLI
/// `--list-modes`).
#[must_use]
pub fn all_specs() -> &'static [ModeSpec] {
    &ALL_SPECS
}

/// Parse a mode from a user-supplied string.
///
/// Matches either the [`ModeSpec::short_name`] or the human-readable
/// [`ModeSpec::name`], case-insensitively and ignoring separators, so
/// `"pd120"`, `"PD-120"`, and `"Robot 36"` all resolve. Returns `None` when
/// nothing matches.
///
/// This is the inverse of [`ModeSpec::short_name`] only for strings that
/// spell a real mode; it is not a general grammar.
#[must_use]
pub fn parse_mode(input: &str) -> Option<SstvMode> {
    let normalized = |s: &str| -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let wanted = normalized(input);
    ALL_SPECS
        .iter()
        .find(|spec| normalized(spec.short_name) == wanted || normalized(spec.name) == wanted)
        .map(|spec| spec.mode)
}

// Mode timing constants — Robot/Scottie/Martin translated row-for-row
// from slowrx's modespec.c (PD120 lines 260-271, PD180 lines 286-297,
// PD240 lines 299-310, R72 lines 130-141, R36 lines 143-154, R24 lines
// 156-167). PD50/PD90/PD160/PD290 and Wraase SC2-180 are not in slowrx;
// their timings come from the Dayton Paper (N7CXI, 2000), whose PD
// numbers agree with slowrx's PD120/180/240 entries digit-for-digit.
//
// PD/Wraase line timing decomposes as
//   line_seconds = sync + porch + channels × width × pixel_seconds
// (4 channels for PD, 3 for Wraase; no separator pulses in either).

/// PD-50. Dayton Paper: VIS 93d, 320×256, color scan 91.520 ms,
/// transmission 49.7 s (128 line-pairs × 388.16 ms).
const PD50: ModeSpec = ModeSpec {
    mode: SstvMode::Pd50,
    short_name: "pd50",
    name: "PD-50",
    vis_code: 0x5D,
    line_pixels: 320,
    image_lines: 256,
    // 20 ms + 2.08 ms + 4 × 91.52 ms = 388.16 ms.
    line_seconds: 0.388_16,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_286, // 91.52 ms / 320 px
    septr_seconds: 0.0,
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

/// PD-90. Dayton Paper: VIS 99d, 320×256, color scan 170.240 ms,
/// transmission 90.0 s (128 line-pairs × 703.04 ms).
const PD90: ModeSpec = ModeSpec {
    mode: SstvMode::Pd90,
    short_name: "pd90",
    name: "PD-90",
    vis_code: 0x63,
    line_pixels: 320,
    image_lines: 256,
    // 20 ms + 2.08 ms + 4 × 170.24 ms = 703.04 ms.
    line_seconds: 0.703_04,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_532, // 170.24 ms / 320 px
    septr_seconds: 0.0,
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const PD120: ModeSpec = ModeSpec {
    mode: SstvMode::Pd120,
    short_name: "pd120",
    name: "PD-120",
    vis_code: 0x5F,
    line_pixels: 640,
    image_lines: 496,
    line_seconds: 0.508_48,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_19,
    septr_seconds: 0.0, // modespec.c: SeptrTime = 0e-3 for PD-family
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

/// PD-160. Dayton Paper: VIS 98d, 512×400, color scan 195.584 ms,
/// transmission 160.9 s (200 line-pairs × 804.416 ms).
const PD160: ModeSpec = ModeSpec {
    mode: SstvMode::Pd160,
    short_name: "pd160",
    name: "PD-160",
    vis_code: 0x62,
    line_pixels: 512,
    image_lines: 400,
    // 20 ms + 2.08 ms + 4 × 195.584 ms = 804.416 ms.
    line_seconds: 0.804_416,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_382, // 195.584 ms / 512 px
    septr_seconds: 0.0,
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const PD180: ModeSpec = ModeSpec {
    mode: SstvMode::Pd180,
    short_name: "pd180",
    name: "PD-180",
    vis_code: 0x60,
    line_pixels: 640,
    image_lines: 496,
    line_seconds: 0.754_24,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_286,
    septr_seconds: 0.0, // modespec.c: SeptrTime = 0e-3 for PD-family
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const PD240: ModeSpec = ModeSpec {
    mode: SstvMode::Pd240,
    short_name: "pd240",
    name: "PD-240",
    vis_code: 0x61,
    line_pixels: 640,
    image_lines: 496,
    // slowrx modespec.c:299-310 — PD240 LineTime = 1000e-3,
    // PixelTime = 0.382e-3, SyncTime = 20e-3, PorchTime = 2.08e-3.
    line_seconds: 1.000,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_382,
    septr_seconds: 0.0, // modespec.c: SeptrTime = 0e-3 for PD-family
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

/// PD-290. Dayton Paper: VIS 94d, 800×616, color scan 228.800 ms,
/// transmission 288.7 s (308 line-pairs × 937.28 ms).
const PD290: ModeSpec = ModeSpec {
    mode: SstvMode::Pd290,
    short_name: "pd290",
    name: "PD-290",
    vis_code: 0x5E,
    line_pixels: 800,
    image_lines: 616,
    // 20 ms + 2.08 ms + 4 × 228.8 ms = 937.28 ms.
    line_seconds: 0.937_28,
    sync_seconds: 0.020,
    porch_seconds: 0.002_08,
    pixel_seconds: 0.000_286, // 228.8 ms / 800 px
    septr_seconds: 0.0,
    channel_layout: ChannelLayout::PdYcbcr,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const ROBOT24: ModeSpec = ModeSpec {
    mode: SstvMode::Robot24,
    short_name: "robot24",
    name: "Robot 24",
    vis_code: 0x04,
    line_pixels: 320,
    image_lines: 240,
    // slowrx modespec.c:156-167 — R24 LineTime = 150e-3,
    // PixelTime = 0.1375e-3, SyncTime = 9e-3, PorchTime = 3e-3,
    // SeptrTime = 6e-3.
    line_seconds: 0.150,
    sync_seconds: 0.009,
    porch_seconds: 0.003,
    pixel_seconds: 0.000_137_5,
    septr_seconds: 0.006,
    channel_layout: ChannelLayout::RobotYuv,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const ROBOT36: ModeSpec = ModeSpec {
    mode: SstvMode::Robot36,
    short_name: "robot36",
    name: "Robot 36",
    vis_code: 0x08,
    line_pixels: 320,
    image_lines: 240,
    // slowrx modespec.c:143-154 — R36 LineTime = 150e-3,
    // PixelTime = 0.1375e-3, SyncTime = 9e-3, PorchTime = 3e-3,
    // SeptrTime = 6e-3.  Identical timing to R24.
    line_seconds: 0.150,
    sync_seconds: 0.009,
    porch_seconds: 0.003,
    pixel_seconds: 0.000_137_5,
    septr_seconds: 0.006,
    channel_layout: ChannelLayout::RobotYuv,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const ROBOT72: ModeSpec = ModeSpec {
    mode: SstvMode::Robot72,
    short_name: "robot72",
    name: "Robot 72",
    vis_code: 0x0C,
    line_pixels: 320,
    image_lines: 240,
    // slowrx modespec.c:130-141 — R72 LineTime = 300e-3,
    // PixelTime = 0.2875e-3, SyncTime = 9e-3, PorchTime = 3e-3,
    // SeptrTime = 4.7e-3.
    line_seconds: 0.300,
    sync_seconds: 0.009,
    porch_seconds: 0.003,
    pixel_seconds: 0.000_287_5,
    septr_seconds: 0.0047,
    channel_layout: ChannelLayout::RobotYuv,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

const SCOTTIE1: ModeSpec = ModeSpec {
    mode: SstvMode::Scottie1,
    short_name: "scottie1",
    name: "Scottie 1",
    vis_code: 0x3C,
    line_pixels: 320,
    image_lines: 256,
    // slowrx modespec.c:91-102 — S1 LineTime = 428.38e-3,
    // PixelTime = 0.4320e-3, SyncTime = 9e-3, PorchTime = 1.5e-3,
    // SeptrTime = 1.5e-3.
    line_seconds: 0.428_38,
    sync_seconds: 0.009,
    porch_seconds: 0.001_5,
    pixel_seconds: 0.000_432_0,
    septr_seconds: 0.001_5,
    channel_layout: ChannelLayout::RgbSequential,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::Scottie,
};

const SCOTTIE2: ModeSpec = ModeSpec {
    mode: SstvMode::Scottie2,
    short_name: "scottie2",
    name: "Scottie 2",
    vis_code: 0x38,
    line_pixels: 320,
    image_lines: 256,
    // slowrx modespec.c:104-115 — S2 LineTime = 277.692e-3,
    // PixelTime = 0.2752e-3, SyncTime = 9e-3, PorchTime = 1.5e-3,
    // SeptrTime = 1.5e-3.
    line_seconds: 0.277_692,
    sync_seconds: 0.009,
    porch_seconds: 0.001_5,
    pixel_seconds: 0.000_275_2,
    septr_seconds: 0.001_5,
    channel_layout: ChannelLayout::RgbSequential,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::Scottie,
};

const SCOTTIE_DX: ModeSpec = ModeSpec {
    mode: SstvMode::ScottieDx,
    short_name: "scottiedx",
    name: "Scottie DX",
    vis_code: 0x4C,
    line_pixels: 320,
    image_lines: 256,
    // slowrx modespec.c:117-128 — SDX LineTime = 1050.3e-3,
    // PixelTime = 1.08053e-3, SyncTime = 9e-3, PorchTime = 1.5e-3,
    // SeptrTime = 1.5e-3.
    line_seconds: 1.050_3,
    sync_seconds: 0.009,
    porch_seconds: 0.001_5,
    pixel_seconds: 0.001_080_53,
    septr_seconds: 0.001_5,
    channel_layout: ChannelLayout::RgbSequential,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::Scottie,
};

/// Martin 1. slowrx `modespec.c:39-50`.
const MARTIN1: ModeSpec = ModeSpec {
    mode: SstvMode::Martin1,
    short_name: "martin1",
    name: "Martin 1",
    vis_code: 0x2C,
    line_pixels: 320,
    image_lines: 256,
    // slowrx modespec.c:39-50 — M1 LineTime = 446.446e-3,
    // PixelTime = 0.4576e-3, SyncTime = 4.862e-3,
    // PorchTime = 0.572e-3, SeptrTime = 0.572e-3.
    line_seconds: 0.446_446,
    sync_seconds: 0.004_862,
    porch_seconds: 0.000_572,
    pixel_seconds: 0.000_457_6,
    septr_seconds: 0.000_572,
    channel_layout: ChannelLayout::RgbSequential,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

/// Martin 2. slowrx `modespec.c:52-63`.
const MARTIN2: ModeSpec = ModeSpec {
    mode: SstvMode::Martin2,
    short_name: "martin2",
    name: "Martin 2",
    vis_code: 0x28,
    line_pixels: 320,
    image_lines: 256,
    // slowrx modespec.c:52-63 — M2 LineTime = 226.7986e-3,
    // PixelTime = 0.2288e-3, SyncTime = 4.862e-3,
    // PorchTime = 0.572e-3, SeptrTime = 0.572e-3.
    line_seconds: 0.226_798_6,
    sync_seconds: 0.004_862,
    porch_seconds: 0.000_572,
    pixel_seconds: 0.000_228_8,
    septr_seconds: 0.000_572,
    channel_layout: ChannelLayout::RgbSequential,
    rgb_order: RgbOrder::Gbr,
    sync_position: SyncPosition::LineStart,
};

/// Wraase SC2-180. Not in slowrx — Dayton Paper「WRASSE SC2-180」:
/// VIS 55d, 320×256, color scan 235.000 ms (0.7344 ms/px), scan
/// sequence Red, Green, Blue, transmission 182 s (256 lines ×
/// 711.0225 ms). The simplest RGB mode: sync + porch, then the three
/// scans back-to-back with **no separator pulses** (`septr_seconds` 0).
const WRAASE_SC2_180: ModeSpec = ModeSpec {
    mode: SstvMode::WraaseSc2_180,
    short_name: "sc2180",
    name: "Wraase SC2-180",
    vis_code: 0x37,
    line_pixels: 320,
    image_lines: 256,
    // 5.5225 ms + 0.5 ms + 3 × 235 ms = 711.0225 ms.
    line_seconds: 0.711_022_5,
    sync_seconds: 0.005_522_5,
    porch_seconds: 0.000_5,
    pixel_seconds: 0.000_734_375, // 235 ms / 320 px
    septr_seconds: 0.0,
    channel_layout: ChannelLayout::RgbSequential,
    rgb_order: RgbOrder::Rgb,
    sync_position: SyncPosition::LineStart,
};

/// All implemented mode specs. Single source of truth — [`lookup`] is
/// derived from this; [`for_mode`] keeps its exhaustive match so
/// adding a `SstvMode` variant without a `const ModeSpec` (and a
/// matching arm in `for_mode`) is a compile error, by design.
///
/// The F8 round-trip test (`all_specs_roundtrip`) verifies every
/// entry's `(mode, vis_code, short_name, name)` quadruple is unique
/// and that `lookup` and `for_mode` agree with the table.
pub(crate) const ALL_SPECS: [ModeSpec; 16] = [
    PD50, PD90, PD120, PD160, PD180, PD240, PD290, ROBOT24, ROBOT36, ROBOT72, SCOTTIE1, SCOTTIE2,
    SCOTTIE_DX, MARTIN1, MARTIN2, WRAASE_SC2_180,
];

#[cfg(test)]
#[allow(clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn pd120_vis_code_resolves() {
        let spec = lookup(0x5F).expect("PD120 VIS resolves");
        assert_eq!(spec.mode, SstvMode::Pd120);
        assert_eq!(spec.vis_code, 0x5F);
        assert_eq!(spec.line_pixels, 640);
        assert_eq!(spec.image_lines, 496);
        assert_eq!(spec.channel_layout, ChannelLayout::PdYcbcr);
        assert_eq!(spec.line_seconds, 0.508_48);
        assert_eq!(spec.sync_seconds, 0.020);
        assert_eq!(spec.porch_seconds, 0.002_08);
        assert_eq!(spec.pixel_seconds, 0.000_19);
    }

    #[test]
    fn pd180_vis_code_resolves() {
        let spec = lookup(0x60).expect("PD180 VIS resolves");
        assert_eq!(spec.mode, SstvMode::Pd180);
        assert_eq!(spec.pixel_seconds, 0.000_286);
    }

    #[test]
    fn unknown_vis_codes_return_none() {
        assert!(lookup(0x00).is_none());
        assert!(lookup(0x42).is_none()); // reserved
        assert!(lookup(0xFF).is_none());
    }

    #[test]
    fn for_mode_returns_matching_spec() {
        assert_eq!(for_mode(SstvMode::Pd120).vis_code, 0x5F);
        assert_eq!(for_mode(SstvMode::Pd180).vis_code, 0x60);
    }

    #[test]
    fn all_specs_agrees_with_lookup_and_for_mode() {
        for spec in all_specs() {
            assert_eq!(for_mode(spec.mode), *spec);
            assert_eq!(lookup(spec.vis_code).map(|s| s.mode), Some(spec.mode));
        }
    }

    #[test]
    fn parse_mode_accepts_short_and_display_names() {
        for spec in all_specs() {
            assert_eq!(parse_mode(spec.short_name), Some(spec.mode));
            assert_eq!(parse_mode(spec.name), Some(spec.mode));
        }
        // Case- and separator-insensitive.
        assert_eq!(parse_mode("PD-120"), Some(SstvMode::Pd120));
        assert_eq!(parse_mode("pd 120"), Some(SstvMode::Pd120));
        assert_eq!(parse_mode("Robot 36"), Some(SstvMode::Robot36));
        assert_eq!(parse_mode("scottiedx"), Some(SstvMode::ScottieDx));
        assert_eq!(parse_mode("Scottie DX"), Some(SstvMode::ScottieDx));
        assert_eq!(parse_mode("nonsense"), None);
        assert_eq!(parse_mode(""), None);
    }

    #[test]
    fn pd_modes_have_zero_septr_seconds() {
        // PD-family: SeptrTime = 0e-3 (modespec.c). The field exists for
        // V2 parity (Robot/Scottie/Martin have non-zero SeptrTime); for PD
        // modes it must be zero so chan_starts_sec is numerically unchanged.
        for spec in all_specs()
            .iter()
            .filter(|s| s.channel_layout == ChannelLayout::PdYcbcr)
        {
            assert_eq!(spec.septr_seconds, 0.0, "{:?} septr", spec.mode);
        }
    }

    #[test]
    fn all_v2_modes_have_line_start_sync_position() {
        // V2 carve-out: ModeSpec.sync_position lets V2.3 Scottie declare
        // mid-line sync without retrofitting V1. PD/Robot/Martin/Wraase all
        // use line-start sync; Scottie is the V2.3 exception.
        for mode in [
            SstvMode::Pd50,
            SstvMode::Pd90,
            SstvMode::Pd120,
            SstvMode::Pd160,
            SstvMode::Pd180,
            SstvMode::Pd240,
            SstvMode::Pd290,
            SstvMode::Robot24,
            SstvMode::Robot36,
            SstvMode::Robot72,
            SstvMode::Martin1,
            SstvMode::Martin2,
            SstvMode::WraaseSc2_180,
        ] {
            let spec = for_mode(mode);
            assert_eq!(spec.sync_position, SyncPosition::LineStart);
        }
    }

    #[test]
    fn pd240_vis_code_resolves() {
        let spec = lookup(0x61).expect("PD240 VIS resolves");
        assert_eq!(spec.mode, SstvMode::Pd240);
        assert_eq!(spec.vis_code, 0x61);
        assert_eq!(spec.line_pixels, 640);
        assert_eq!(spec.image_lines, 496);
        assert_eq!(spec.channel_layout, ChannelLayout::PdYcbcr);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert_eq!(spec.line_seconds, 1.000);
        assert_eq!(spec.sync_seconds, 0.020);
        assert_eq!(spec.porch_seconds, 0.002_08);
        assert_eq!(spec.pixel_seconds, 0.000_382);
        assert_eq!(spec.septr_seconds, 0.0);
    }

    #[test]
    fn for_mode_returns_pd240_spec() {
        assert_eq!(for_mode(SstvMode::Pd240).vis_code, 0x61);
    }

    #[test]
    fn robot24_vis_code_resolves() {
        let spec = lookup(0x04).expect("R24 VIS resolves");
        assert_eq!(spec.mode, SstvMode::Robot24);
        assert_eq!(spec.vis_code, 0x04);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 240);
        assert_eq!(spec.channel_layout, ChannelLayout::RobotYuv);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert_eq!(spec.line_seconds, 0.150);
        assert_eq!(spec.sync_seconds, 0.009);
        assert_eq!(spec.porch_seconds, 0.003);
        assert_eq!(spec.septr_seconds, 0.006);
        assert_eq!(spec.pixel_seconds, 0.000_137_5);
    }

    #[test]
    fn robot36_vis_code_resolves() {
        let spec = lookup(0x08).expect("R36 VIS resolves");
        assert_eq!(spec.mode, SstvMode::Robot36);
        assert_eq!(spec.vis_code, 0x08);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 240);
        assert_eq!(spec.channel_layout, ChannelLayout::RobotYuv);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert_eq!(spec.line_seconds, 0.150);
        assert_eq!(spec.sync_seconds, 0.009);
        assert_eq!(spec.porch_seconds, 0.003);
        assert_eq!(spec.septr_seconds, 0.006);
        assert_eq!(spec.pixel_seconds, 0.000_137_5);
    }

    #[test]
    fn robot72_vis_code_resolves() {
        let spec = lookup(0x0C).expect("R72 VIS resolves");
        assert_eq!(spec.mode, SstvMode::Robot72);
        assert_eq!(spec.vis_code, 0x0C);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 240);
        assert_eq!(spec.channel_layout, ChannelLayout::RobotYuv);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert_eq!(spec.line_seconds, 0.300);
        assert_eq!(spec.sync_seconds, 0.009);
        assert_eq!(spec.porch_seconds, 0.003);
        assert_eq!(spec.septr_seconds, 0.0047);
        assert_eq!(spec.pixel_seconds, 0.000_287_5);
    }

    #[test]
    fn for_mode_returns_robot_specs() {
        assert_eq!(for_mode(SstvMode::Robot24).vis_code, 0x04);
        assert_eq!(for_mode(SstvMode::Robot36).vis_code, 0x08);
        assert_eq!(for_mode(SstvMode::Robot72).vis_code, 0x0C);
    }

    #[test]
    fn scottie1_modespec() {
        let spec = for_mode(SstvMode::Scottie1);
        assert_eq!(spec.mode, SstvMode::Scottie1);
        assert_eq!(spec.vis_code, 0x3C);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 256);
        assert_eq!(spec.channel_layout, ChannelLayout::RgbSequential);
        assert_eq!(spec.sync_position, SyncPosition::Scottie);
        assert!((spec.pixel_seconds - 0.4320e-3).abs() < 1e-9);
        assert!((spec.line_seconds - 428.38e-3).abs() < 1e-9);
    }

    #[test]
    fn scottie2_modespec() {
        let spec = for_mode(SstvMode::Scottie2);
        assert_eq!(spec.mode, SstvMode::Scottie2);
        assert_eq!(spec.vis_code, 0x38);
        assert!((spec.pixel_seconds - 0.2752e-3).abs() < 1e-9);
        assert!((spec.line_seconds - 277.692e-3).abs() < 1e-9);
        assert_eq!(spec.channel_layout, ChannelLayout::RgbSequential);
        assert_eq!(spec.sync_position, SyncPosition::Scottie);
    }

    #[test]
    fn scottie_dx_modespec() {
        let spec = for_mode(SstvMode::ScottieDx);
        assert_eq!(spec.mode, SstvMode::ScottieDx);
        assert_eq!(spec.vis_code, 0x4C);
        assert!((spec.pixel_seconds - 1.08053e-3).abs() < 1e-9);
        assert!((spec.line_seconds - 1050.3e-3).abs() < 1e-9);
        assert_eq!(spec.channel_layout, ChannelLayout::RgbSequential);
        assert_eq!(spec.sync_position, SyncPosition::Scottie);
    }

    #[test]
    fn scottie_vis_codes_resolve() {
        // Codebase uses `lookup` (returning `Option<ModeSpec>`) rather
        // than `for_vis_code`; mirrors the existing
        // `pd120_vis_code_resolves` style.
        assert_eq!(
            lookup(0x3C).expect("S1 VIS resolves").mode,
            SstvMode::Scottie1
        );
        assert_eq!(
            lookup(0x38).expect("S2 VIS resolves").mode,
            SstvMode::Scottie2
        );
        assert_eq!(
            lookup(0x4C).expect("SDX VIS resolves").mode,
            SstvMode::ScottieDx
        );
    }

    #[test]
    fn martin1_modespec() {
        let spec = for_mode(SstvMode::Martin1);
        assert_eq!(spec.mode, SstvMode::Martin1);
        assert_eq!(spec.vis_code, 0x2C);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 256);
        assert_eq!(spec.channel_layout, ChannelLayout::RgbSequential);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert!((spec.pixel_seconds - 0.000_457_6).abs() < 1e-9);
        assert!((spec.line_seconds - 0.446_446).abs() < 1e-9);
    }

    #[test]
    fn martin2_modespec() {
        let spec = for_mode(SstvMode::Martin2);
        assert_eq!(spec.mode, SstvMode::Martin2);
        assert_eq!(spec.vis_code, 0x28);
        assert!((spec.pixel_seconds - 0.000_228_8).abs() < 1e-9);
        assert!((spec.line_seconds - 0.226_798_6).abs() < 1e-9);
        assert_eq!(spec.channel_layout, ChannelLayout::RgbSequential);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
    }

    #[test]
    fn martin_vis_codes_resolve() {
        assert_eq!(lookup(0x2C).expect("M1").mode, SstvMode::Martin1);
        assert_eq!(lookup(0x28).expect("M2").mode, SstvMode::Martin2);
    }

    #[test]
    fn pd50_modespec() {
        let spec = for_mode(SstvMode::Pd50);
        assert_eq!(spec.mode, SstvMode::Pd50);
        assert_eq!(spec.vis_code, 0x5D);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 256);
        assert_eq!(spec.channel_layout, ChannelLayout::PdYcbcr);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert!((spec.pixel_seconds - 0.000_286).abs() < 1e-12);
        assert!((spec.line_seconds - 0.388_16).abs() < 1e-12);
    }

    #[test]
    fn pd90_modespec() {
        let spec = for_mode(SstvMode::Pd90);
        assert_eq!(spec.mode, SstvMode::Pd90);
        assert_eq!(spec.vis_code, 0x63);
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 256);
        assert_eq!(spec.channel_layout, ChannelLayout::PdYcbcr);
        assert!((spec.pixel_seconds - 0.000_532).abs() < 1e-12);
        assert!((spec.line_seconds - 0.703_04).abs() < 1e-12);
    }

    #[test]
    fn pd160_modespec() {
        let spec = for_mode(SstvMode::Pd160);
        assert_eq!(spec.mode, SstvMode::Pd160);
        assert_eq!(spec.vis_code, 0x62);
        assert_eq!(spec.line_pixels, 512);
        assert_eq!(spec.image_lines, 400);
        assert_eq!(spec.channel_layout, ChannelLayout::PdYcbcr);
        assert!((spec.pixel_seconds - 0.000_382).abs() < 1e-12);
        assert!((spec.line_seconds - 0.804_416).abs() < 1e-12);
    }

    #[test]
    fn pd290_modespec() {
        let spec = for_mode(SstvMode::Pd290);
        assert_eq!(spec.mode, SstvMode::Pd290);
        assert_eq!(spec.vis_code, 0x5E);
        assert_eq!(spec.line_pixels, 800);
        assert_eq!(spec.image_lines, 616);
        assert_eq!(spec.channel_layout, ChannelLayout::PdYcbcr);
        assert!((spec.pixel_seconds - 0.000_286).abs() < 1e-12);
        assert!((spec.line_seconds - 0.937_28).abs() < 1e-12);
    }

    #[test]
    fn wraase_sc2_180_modespec() {
        let spec = for_mode(SstvMode::WraaseSc2_180);
        assert_eq!(spec.mode, SstvMode::WraaseSc2_180);
        assert_eq!(spec.vis_code, 0x37);
        assert_eq!(spec.short_name, "sc2180");
        assert_eq!(spec.line_pixels, 320);
        assert_eq!(spec.image_lines, 256);
        assert_eq!(spec.channel_layout, ChannelLayout::RgbSequential);
        assert_eq!(spec.rgb_order, RgbOrder::Rgb);
        assert_eq!(spec.sync_position, SyncPosition::LineStart);
        assert!((spec.sync_seconds - 0.005_522_5).abs() < 1e-12);
        assert!((spec.porch_seconds - 0.000_5).abs() < 1e-12);
        assert!((spec.pixel_seconds - 0.000_734_375).abs() < 1e-12);
        assert!((spec.line_seconds - 0.711_022_5).abs() < 1e-12);
    }

    #[test]
    fn rgb_order_matches_family() {
        // Scottie/Martin 发 G→B→R；Wraase SC2-180 发 R→G→B。
        for mode in [
            SstvMode::Scottie1,
            SstvMode::Scottie2,
            SstvMode::ScottieDx,
            SstvMode::Martin1,
            SstvMode::Martin2,
        ] {
            assert_eq!(for_mode(mode).rgb_order, RgbOrder::Gbr, "{mode:?}");
        }
        assert_eq!(
            for_mode(SstvMode::WraaseSc2_180).rgb_order,
            RgbOrder::Rgb,
            "Wraase SC2-180 的扫描顺序是 R→G→B"
        );
    }

    #[test]
    fn rgb_order_wire_indices_are_a_permutation() {
        for order in [RgbOrder::Gbr, RgbOrder::Rgb] {
            let wire = order.wire_rgb_indices();
            let mut sorted = wire;
            sorted.sort_unstable();
            assert_eq!(sorted, [0, 1, 2], "{order:?} wire_rgb_indices 不是排列");
        }
        assert_eq!(RgbOrder::Gbr.wire_rgb_indices(), [1, 2, 0]);
        assert_eq!(RgbOrder::Rgb.wire_rgb_indices(), [0, 1, 2]);
    }

    #[test]
    fn skip_correction_seconds_zero_for_line_start_modes() {
        for mode in [
            SstvMode::Pd50,
            SstvMode::Pd90,
            SstvMode::Pd120,
            SstvMode::Pd160,
            SstvMode::Pd180,
            SstvMode::Pd240,
            SstvMode::Pd290,
            SstvMode::Robot24,
            SstvMode::Robot36,
            SstvMode::Robot72,
            SstvMode::Martin1,
            SstvMode::Martin2,
            SstvMode::WraaseSc2_180,
        ] {
            let spec = for_mode(mode);
            assert_eq!(
                spec.skip_correction_seconds(),
                0.0,
                "{mode:?} expected 0.0 skip correction"
            );
        }
    }

    #[test]
    fn skip_correction_seconds_scottie_formula() {
        for mode in [SstvMode::Scottie1, SstvMode::Scottie2, SstvMode::ScottieDx] {
            let spec = for_mode(mode);
            let expected =
                -f64::from(spec.line_pixels) * spec.pixel_seconds / 2.0 + 2.0 * spec.porch_seconds;
            assert!(
                (spec.skip_correction_seconds() - expected).abs() < 1e-12,
                "{mode:?} got {} expected {expected}",
                spec.skip_correction_seconds()
            );
            assert!(
                spec.skip_correction_seconds() < 0.0,
                "{mode:?} Scottie correction should be negative"
            );
        }
    }

    /// F8 (#91). Every entry in `ALL_SPECS` round-trips cleanly
    /// through `lookup` (VIS code → spec) and `for_mode` (mode →
    /// spec); the table has no duplicate modes, VIS codes or
    /// `short_names`, and every `name` and `short_name` is non-empty.
    ///
    /// Subsumes the per-mode `vis_code_resolves` tests as a
    /// structural invariant. The individual per-mode tests stay as
    /// fast-failing regression guards with descriptive names.
    #[test]
    fn all_specs_roundtrip() {
        use std::collections::HashSet;

        let modes: HashSet<_> = ALL_SPECS.iter().map(|s| s.mode).collect();
        assert_eq!(
            modes.len(),
            ALL_SPECS.len(),
            "ALL_SPECS has duplicate modes"
        );

        let vis: HashSet<_> = ALL_SPECS.iter().map(|s| s.vis_code).collect();
        assert_eq!(
            vis.len(),
            ALL_SPECS.len(),
            "ALL_SPECS has duplicate VIS codes"
        );

        let short_names: HashSet<_> = ALL_SPECS.iter().map(|s| s.short_name).collect();
        assert_eq!(
            short_names.len(),
            ALL_SPECS.len(),
            "ALL_SPECS has duplicate short_names"
        );

        let names: HashSet<_> = ALL_SPECS.iter().map(|s| s.name).collect();
        assert_eq!(
            names.len(),
            ALL_SPECS.len(),
            "ALL_SPECS has duplicate `name`s"
        );

        for spec in ALL_SPECS.iter().copied() {
            assert_eq!(
                lookup(spec.vis_code),
                Some(spec),
                "lookup({:#04x}) did not return ALL_SPECS entry for {:?}",
                spec.vis_code,
                spec.mode
            );
            assert_eq!(
                for_mode(spec.mode),
                spec,
                "for_mode({:?}) did not match ALL_SPECS entry",
                spec.mode
            );
            assert!(
                !spec.short_name.is_empty(),
                "{:?}: short_name empty",
                spec.mode
            );
            assert!(!spec.name.is_empty(), "{:?}: name empty", spec.mode);
        }
    }
}
