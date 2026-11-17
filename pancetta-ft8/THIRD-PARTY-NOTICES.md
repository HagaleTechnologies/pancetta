# Third-Party Notices for pancetta-ft8

This package contains vendored third-party C code: the ft8_lib sources in
`vendor/ft8_lib/` and the kissfft FFT inside them. Their licenses are
reproduced below. Anyone who distributes a binary built from this package
must keep these notices with it.

The crate's own code is licensed `MIT OR Apache-2.0` (see `LICENSE-MIT` and
`LICENSE-APACHE`). Cargo dependencies carry their own licenses and are not
listed here.

---

## ft8_lib

- **Project:** [`kgoba/ft8_lib`](https://github.com/kgoba/ft8_lib)
- **Author:** Kārlis Goba (YL3JG)
- **Bundled at:** `vendor/ft8_lib/`: the `ft8/`, `fft/` and `common/`
  directories and the project's `LICENSE`. `build.rs` compiles every C file
  there except `common/audio.c` and `common/wave.c`, which ship with their
  headers but are not compiled.
- **Used as:** the FT8/FT4 reference decoder, compiled by `build.rs` and
  called through FFI in `src/ft8_lib_ffi.rs`. Also a source of algorithms
  ported into the native Rust decoder and encoder, each marked by an
  attribution comment (search for `ft8_lib` in `src/`).

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

---

## kissfft (bundled inside ft8_lib)

- **Project:** [`mborgerding/kissfft`](https://github.com/mborgerding/kissfft)
- **Author:** Mark Borgerding
- **Bundled at:** `vendor/ft8_lib/fft/`
- **Used as:** the FFT inside ft8_lib's C decoder. Its source files carry
  only `SPDX-License-Identifier: BSD-3-Clause` and a pointer to a `COPYING`
  file that ft8_lib does not ship, so the BSD-3-Clause text is reproduced
  here with the copyright line from `kiss_fft.c`.

```
Copyright (c) 2003-2010, Mark Borgerding. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

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
