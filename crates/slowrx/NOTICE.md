# Third-Party Notices

`crates/slowrx` is derived through the following chain:

1. [windytan/slowrx](https://github.com/windytan/slowrx) — the original C
   implementation by Oona Räisänen (OH2EIQ), under the **ISC** License;
2. [jasonherald/slowrx.rs](https://github.com/jasonherald/slowrx.rs) — a pure-Rust
   port built on 1, under the **MIT** License;
3. [wty2019wty/slowrx.rs](https://github.com/wty2019wty/slowrx.rs/tree/sstv-web)
   (the `sstv-web` branch) — a fork of 2, under the **MIT** License;
4. [Web_SSTV_by_slowrx.rs](https://github.com/wty2019wty/Web_SSTV_by_slowrx.rs) —
   this project, which inlines the decoder core from 3 as `crates/slowrx`, adds
   progressive/real-time decoding, and wraps it in WebAssembly plus a web front
   end. The project as a whole is under **AGPL-3.0-or-later**; the
   `crates/slowrx` portion itself remains under MIT.

VIS detection, mode-specification tables, frequency-to-pixel mappings, and sync
correlation in this crate are translated from slowrx's C sources; per-file
headers in `src/` identify the corresponding slowrx file. The ISC copyright and
permission notice below is reproduced verbatim from upstream slowrx, as required
by its distribution terms.

## slowrx — ISC License

```text
Copyright (c) 2007-2013, Oona Räisänen (OH2EIQ [at] sral.fi)

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

The ISC License is functionally equivalent to the 2-clause BSD and MIT
licenses. The upstream Rust port is released under MIT, which preserves
slowrx's permission terms while adding the standard MIT warranty disclaimer.
`crates/slowrx` itself continues to be distributed under **MIT** (see
[`LICENSE`](./LICENSE)), while the rest of this project is under
**AGPL-3.0-or-later** (see the repository root `LICENSE`). MIT/ISC and AGPL are
compatible and may be combined and distributed together.

## Project links

- Original C project: https://github.com/windytan/slowrx
- Rust port: https://github.com/jasonherald/slowrx.rs
- Inlined core source (`sstv-web` branch): https://github.com/wty2019wty/slowrx.rs/tree/sstv-web
- This project: https://github.com/wty2019wty/Web_SSTV_by_slowrx.rs
- slowrx project page: https://windytan.github.io/slowrx/
- Author: Oona Räisänen (OH2EIQ), https://windytan.github.io/
