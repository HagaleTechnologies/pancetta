# Third-Party Notices

Pancetta bundles or directly ports code from the following third-party
projects. Their licenses are reproduced verbatim below. Compiled
binaries that include this code must preserve these notices.

Cargo dependencies pulled at build time are not listed here — their
licenses are surfaced by `cargo about generate` / `cargo deny check`
and apply per their own crate-level `LICENSE` files. The list below
covers code that ships *in this repository* (vendored sources) or that
was directly copied/ported into Pancetta's own source tree.

---

## ft8_lib

- **Project:** [`kgoba/ft8_lib`](https://github.com/kgoba/ft8_lib)
- **Author:** Kārlis Goba (YL3JG)
- **Bundled at:** `pancetta-ft8/vendor/ft8_lib/`
- **Used as:** Primary FT8 reference decoder, compiled via `cc` in
  `pancetta-ft8/build.rs` and called through FFI in
  `pancetta-ft8/src/ft8_lib_ffi.rs`. Also a source of ported
  algorithms in Pancetta's native Rust decoder/encoder
  (`pancetta-ft8/src/{decoder,encoder,ldpc,message,osd}.rs`), each of
  which carries an attribution comment at the relevant call site
  (search for `ft8_lib` in those files).

```
MIT License

Copyright (c) 2018 Kārlis Goba

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

`ft8_lib` itself bundles `kissfft` (FFT primitives) under a 3-clause
BSD-style license; see `pancetta-ft8/vendor/ft8_lib/fft/` for that
notice.

---

## FT8 Protocol Specification

The FT8 protocol — including the Costas sync arrays, the LDPC(174,91)
generator matrix, the CRC-14 polynomial, the Gray code mapping, and
the message-payload schema — was designed by **Joe Taylor (K1JT)** and
**Steve Franke (K9AN)** and is published in
[*The FT4 and FT8 Communication Protocols*](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf)
(QEX, July/August 2020). These constants are inherent to any
conformant FT8 implementation. Pancetta sources them via `ft8_lib`'s
reproduction of the protocol values.

---

## WSJT-X (NOT linked)

[WSJT-X](https://wsjt.sourceforge.io/) is the de-facto reference FT8
application, published under the GPL. Pancetta does **not** link,
vendor, or copy any WSJT-X source — it only interoperates with WSJT-X
via the protocol on the air. The phrase "bit-exact with WSJT-X / ft8_lib"
in this codebase refers to verified output equivalence at the protocol
level (same payload bits, same LDPC codeword, same audio symbols),
achieved by implementing the published spec and validating against
`ft8_lib`'s reference output — not by code derivation from WSJT-X.
