//! RGB-sequential mode decoder — Scottie 1/2/DX, Martin 1/2 and
//! Wraase SC2-180.
//!
//! All three families use [`crate::modespec::ChannelLayout::RgbSequential`]:
//! three RGB channels per radio line, written to the image in-place
//! via `image.put_pixel`. They differ in **where the sync pulse sits
//! within a radio line** and in **channel order**:
//!
//! - **Scottie** ([`crate::modespec::SyncPosition::Scottie`]): sync
//!   sits between the B and R channels (mid-line). `find_sync`
//!   applies a Scottie-specific correction so `skip_samples` lands
//!   at line 0's start.
//! - **Martin** ([`crate::modespec::SyncPosition::LineStart`]): sync
//!   at line start (standard SSTV convention); same as PD and Robot.
//! - **Wraase SC2-180** ([`crate::modespec::SyncPosition::LineStart`]):
//!   like Martin but without separator pulses (`septr_seconds` 0) and
//!   with the channels sent **R→G→B** instead of G→B→R.
//!
//! The wire order is per-mode ([`crate::modespec::RgbOrder`]) and is
//! applied via `spec.rgb_order.wire_rgb_indices()`, so the decoder
//! never assumes "channel 0 is Green".
//!
//! ```text
//! Scottie line layout (G→B→R):
//!   [septr][G pixels][septr][B pixels][SYNC][porch][R pixels]
//!     ^                                  ^
//!     |                                  |
//!     line start                         find_sync detects this
//!                                        (mid-line — Scottie branch
//!                                        in find_sync corrects skip)
//!
//! Martin line layout (G→B→R):
//!   [SYNC][porch][G pixels][septr][B pixels][septr][R pixels]
//!   ^
//!   |
//!   line start (sync at line start; standard PD/Robot path)
//!
//! Wraase SC2-180 line layout (R→G→B, no septr):
//!   [SYNC][porch][R pixels][G pixels][B pixels]
//! ```
//!
//! Translated from slowrx's `video.c:72-79` (Scottie `ChanStart`) and
//! the `video.c` "default" case (Martin/PD/Robot `ChanStart`). slowrx's
//! GBR storage convention (`video.c:440-444`) is bypassed — we write
//! RGB directly via [`crate::image::SstvImage::put_pixel`].
//!
//! See `NOTICE.md` for full slowrx attribution.
//!
//! Inline `// slowrx <file>.c:NNN` line refs are against the gitignored
//! local reference clone in `original/slowrx/` (see `clone-slowrx.sh`);
//! verified at audit #94 (2026-05-15).

// NOTE (audit #94 B15): the file is named `mode_scottie.rs` but its
// `decode_line` is shared by Martin 1 / Martin 2 / Wraase SC2-180 (all
// `ChannelLayout::RgbSequential` per ModeSpec). A rename to
// `mode_rgb_sequential.rs` is tracked but deferred — the existing
// name is grep-able and the per-mode behavior branches on
// `spec.sync_position` (mid-line for Scottie, line-start for the rest)
// and `spec.rgb_order` (G→B→R for Scottie/Martin, R→G→B for Wraase)
// rather than file boundaries.

use crate::modespec::ModeSpec;

/// Decode one RGB-sequential radio line (Scottie, Martin or Wraase
/// SC2-180) into `image`. Per-channel start times are
/// line-start-relative and branch on `spec.sync_position`:
///
/// - [`crate::modespec::SyncPosition::Scottie`] — channels are
///   `[septr, 2·septr+chan_len, 2·septr+2·chan_len+sync+porch]` from
///   line start. The mid-line sync sits at `2·septr + 2·chan_len`;
///   `find_sync` corrects `skip_samples` to land at line 0's start
///   (slowrx C `sync.c:123-125`), so all three offsets stay positive.
/// - [`crate::modespec::SyncPosition::LineStart`] — channels are
///   `[sync+porch, sync+porch+chan_len+septr, sync+porch+2·chan_len+2·septr]`
///   from line start, all post-sync. `find_sync` uses the unmodified
///   PD/Robot formula. Same shape as Robot 72 with RGB instead of `YCrCb`.
///
/// In both cases RGB is composed in place via `image.put_pixel`; no
/// chroma side-buffer is needed (cf. R36/R24's `chroma_planes`).
///
/// `line_index` is the 0-based image row this radio line emits;
/// `line_seconds_offset` is `f64::from(line_index) * spec.line_seconds`
/// (un-rounded — the per-pixel time computation does the single
/// `round()` to match slowrx `video.c:140-142`).
#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
pub(crate) fn decode_line(
    spec: ModeSpec,
    line_index: u32,
    audio: &[f32],
    skip_samples: i64,
    line_seconds_offset: f64,
    rate_hz: f64,
    image: &mut crate::image::SstvImage,
    demod: &mut crate::demod::ChannelDemod,
    snr_est: &mut crate::snr::SnrEstimator,
    hedr_shift_hz: f64,
) {
    let pixel_secs = spec.pixel_seconds;
    let sync_secs = spec.sync_seconds;
    let porch_secs = spec.porch_seconds;
    let septr_secs = spec.septr_seconds;
    let width = spec.line_pixels;
    let chan_len = f64::from(width) * pixel_secs;

    // Channel start times relative to *line start*. Scottie's mid-line
    // sync sits at `2·septr + 2·chan_len`; the LineStart branch puts
    // sync at offset 0 (channel 0 follows after sync + porch). find_sync
    // returns `skip_samples` already corrected for both — Scottie
    // applies `s = s − chan_len/2 + 2·porch` (slowrx C
    // `sync.c:123-125`), LineStart uses the unmodified PD/Robot
    // formula.
    //
    // Which colour each slot carries is `spec.rgb_order` (G→B→R for
    // Scottie/Martin, R→G→B for Wraase SC2-180); Wraase additionally
    // has `septr_seconds` 0, so its slots collapse to
    // `[sync+porch, +chan_len, +2·chan_len]` through the same formula.
    let chan_starts_sec: [f64; 3] = match spec.sync_position {
        crate::modespec::SyncPosition::Scottie => [
            septr_secs,                                                 // chan 0 (post-septr1)
            2.0 * septr_secs + chan_len,                                // chan 1 (post-septr2)
            2.0 * septr_secs + 2.0 * chan_len + sync_secs + porch_secs, // chan 2 (post-sync+porch)
        ],
        crate::modespec::SyncPosition::LineStart => {
            // Martin/Wraase layout — slowrx C video.c default case:
            //   ChanStart[0] = sync + porch
            //   ChanStart[1] = ChanStart[0] + chan_len + septr
            //   ChanStart[2] = ChanStart[1] + chan_len + septr
            [
                sync_secs + porch_secs,                                     // chan 0
                sync_secs + porch_secs + chan_len + septr_secs,             // chan 1
                sync_secs + porch_secs + 2.0 * chan_len + 2.0 * septr_secs, // chan 2
            ]
        }
    };

    let width_us = width as usize;

    // Decode each channel (in wire order) into its own buffer.
    let mut ch = [
        vec![0_u8; width_us],
        vec![0_u8; width_us],
        vec![0_u8; width_us],
    ];

    let ctx = crate::demod::ChannelDecodeCtx {
        audio,
        skip_samples,
        rate_hz,
        hedr_shift_hz,
        spec,
    };

    for (chan_idx, buf) in ch.iter_mut().enumerate() {
        crate::demod::decode_one_channel_into(
            buf,
            chan_starts_sec[chan_idx],
            line_seconds_offset,
            &ctx,
            &mut crate::demod::DemodState { demod, snr_est },
        );
    }

    // Compose RGB and write to the image. `wire[k]` is the RGB index
    // (0=R, 1=G, 2=B) of wire channel `k`, so `rgb[wire[k]] = ch[k]`
    // unwinds G→B→R (Scottie/Martin) and R→G→B (Wraase) alike. No
    // chroma conversion needed (cf. Robot's YCrCb→RGB).
    let wire = spec.rgb_order.wire_rgb_indices();
    for x in 0..width_us {
        let mut rgb = [0_u8; 3];
        for (chan_idx, buf) in ch.iter().enumerate() {
            rgb[wire[chan_idx]] = buf[x];
        }
        image.put_pixel(x as u32, line_index, rgb);
    }
}
