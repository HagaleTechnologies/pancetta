# Provenance & Clean-Room Methodology

[← Back to the README](../README.md)

Pancetta is MIT/Apache-2.0. Its FT8 engine is built from three clearly
separated sources, and we are careful about the boundary so the codebase
stays free of copyleft contamination:

1. **MIT code we use directly.** [`kgoba/ft8_lib`](https://github.com/kgoba/ft8_lib)
   (MIT, © Kārlis Goba) is vendored and called via FFI, and re-implemented
   in places in native Rust. ft8_lib's MIT license permits this; every
   ft8_lib-derived algorithm or constant is attributed at its call site
   (search `ft8_lib` in `pancetta-ft8/src/`).

2. **The published FT8 protocol.** The Costas arrays, LDPC(174,91)
   generator/parity matrices, CRC-14 polynomial, Gray code, and message
   schema are defined by Joe Taylor (K1JT) and Steve Franke (K9AN) in the
   [QEX paper](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf). These values are
   **identical in every conformant decoder** (WSJT-X, ft8_lib, JTDX, MSHV, …)
   because the protocol requires them — matching them is interoperability,
   not derivation.

3. **GPL peer decoders — algorithm *ideas* only, never code.** Where Pancetta
   adopts a *technique* from a GPL-licensed project (WSJT-X, JTDX,
   JS8Call-Improved, ft8mon, MSHV), it follows a strict **clean-room
   firewall**: one contributor reads the peer and writes a *prose-only*
   algorithm spec under `research/specs/` that explicitly does not quote
   source; a separate implementer writes the Rust from that spec alone. No
   GPL source is read, ported, copied, or paraphrased into Pancetta's code,
   and the modules written this way carry a `clean-room` affirmation in their
   header comments. Pancetta does **not** link, vendor, or copy any GPL
   source, and does **not** shell out to any GPL binary at runtime.

So: yes, the encoder/decoder *will* resemble the MIT `ft8_lib` (by design and
by license), and the protocol constants *will* match every other FT8 decoder
(by necessity) — but no GPL-licensed source has been incorporated. If you spot
anything that looks like a copyleft-source copy, please open an issue; we treat
that as a bug.
