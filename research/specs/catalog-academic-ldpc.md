# Catalog: Other public open-source LDPC decoder implementations

Brief survey of LDPC decoder implementations on GitHub / crates.io
that are NOT covered by the GRAND or neural-LDPC catalogs but might
be relevant to FT8's (174, 91) code. Compiled 2026-06-08 by reader
thread. The motivation: the FT8 LDPC matrix is short (n=174) and
medium rate (r ≈ 0.52); academic and standards-track LDPC
implementations exist for related dimensions and could be a
**source of reference / sanity-check** for pancetta's own decoder.

## Summary table

| Project | URL | License | Framework | Codes | Decoder algorithms | Relevance to (174, 91) |
|---|---|---|---|---|---|---|
| adamgreig/labrador-ldpc | github.com/adamgreig/labrador-ldpc | **MIT** | Rust (no_std) | CCSDS 231.1 (k=128/256/512, r=1/2); CCSDS 131.0 (k=1024/4096, r=1/2,2/3,4/5) | unspecified in README | **Very relevant** — Rust + no_std + MIT means architectural reference is permitted; codes don't match FT8 |
| daniestevez/ldpc-toolbox | github.com/daniestevez/ldpc-toolbox | (check) | Rust | LDPC code design utilities | n/a (design, not decode) | Reference for parity-check matrix construction; not a decoder |
| radfordneal/LDPC-codes | github.com/radfordneal/LDPC-codes | GPL | C | configurable via alist | sum-product BP, min-sum, decimation | Reference; GPL means prose-only |
| tavildar/LDPC | github.com/tavildar/LDPC | (check) | C/MATLAB | configurable | encode + BP decode | Reference |
| hichamjanati/pyldpc | github.com/hichamjanati/pyldpc | MIT | Python (NumPy) | configurable | BP | Reference for sanity-checking pancetta decodes |
| thadikari/ldpc_decoders | github.com/thadikari/ldpc_decoders | (check) | Python/NumPy | configurable via alist | min-sum, sum-product, OSD, BF | Reference for sanity-checking pancetta's OSD |
| robmaunder/ldpc-3gpp-matlab | (search hit) | (check) | MATLAB | 3GPP NR LDPC | encode/decode | Not relevant — 3GPP codes are long |
| blegal/Fast_LDPC_decoder_for_x86 | (search hit) | (check) | C++ (SIMD) | (varies) | optimised BP | Reference for SIMD acceleration ideas |
| shubhamchandak94/ProtographLDPC | (search hit) | (check) | Python | protograph LDPC | BP | Mainly construction; less relevant |
| xdsopl/LDPC | (search hit) | (check) | C++ | configurable | BP | Reference |
| vodafone-chair/5g-nr-ldpc | (search hit) | (check) | MATLAB | 5G NR | BP, layered | Not relevant — 5G codes long |
| PaulBryden/hdl_ldpc_decoder | github.com/PaulBryden/hdl_ldpc_decoder | (check) | nMigen / HDL | configurable; 1-bit error | hardware bit-flip | FPGA reference; not software |
| kunzjacq/ldpc_decoder | github.com/kunzjacq/ldpc_decoder | (check) | OpenCL | configurable | GPU BP | GPU acceleration reference |

## High-value entries

### adamgreig/labrador-ldpc (MIT, Rust, no_std)

The standout entry: a Rust, MIT-licensed, no_std LDPC decoder. The
codes supported (CCSDS standards at k=128/256/512 and k=1024/4096)
do NOT match FT8's (174, 91), but the **codebase architecture is
a permitted reference** for clean-room implementation of a Rust
LDPC decoder. Specifically:

- **Permissive licensing**: MIT means pancetta can paraphrase the
  code structure freely (per clean-room feedback: paraphrasing is
  preferred even with permissive licenses).
- **No-std**: this is a useful constraint to maintain if pancetta
  ever wants to deploy to embedded targets (hb-216 scoped-fast-path
  line). The labrador-ldpc decoder is proof that a no-allocator
  LDPC BP can fit on microcontrollers.
- **Decoder algorithm**: not explicitly documented in the README;
  follow-on agent could survey the source for which BP variant
  (sum-product vs. min-sum, log-domain vs. tanh-domain, etc.).

**Posture**: If pancetta ever wants to clean up its LDPC decoder
into a separate reusable crate, labrador-ldpc is the architectural
reference. The current decoder is wired tightly into
`pancetta-ft8/src/decoder.rs` and would need a refactor to extract;
that refactor is not urgent.

### hichamjanati/pyldpc (MIT, Python)

A clean MIT-licensed reference. Useful for:
- Sanity-checking pancetta's BP convergence behaviour on known
  test codes (since the algorithm is standard, results should
  match within numerical tolerance).
- Generating test vectors for the implementer thread without
  having to read pancetta's own decoder.

### radfordneal/LDPC-codes (GPL, C)

Radford Neal's reference C implementation. GPL, so clean-room
only. But this is one of the **most widely cited** academic LDPC
implementations and has a clean prose description of sum-product
and min-sum BP in its docs. The docs are a fact-source; the code
is not.

## Notes on the FT8 (174, 91) parity-check matrix

The FT8 LDPC code is **not a standard code** (CCSDS, 5G, WiFi).
It was hand-designed for the FT8 protocol by the WSJT-X team. The
parity-check matrix is defined in WSJT-X's source code (and
faithfully reproduced in ft8_lib and pancetta). It's a (174, 91)
rate-91/174 ≈ 0.52 code with relatively high (~8) average row
weight in the parity-check matrix.

None of the catalogued LDPC decoder implementations target this
specific matrix out of the box. Any port-and-retarget effort
needs:
1. Convert the FT8 H matrix to the implementation's expected
   format (alist, sparse coo, gmat, etc.).
2. Re-build any precomputed lookup tables (e.g., the column-to-
   bit indexing for incremental syndrome updates).

For pancetta's own decoder, the H matrix is already encoded in
`pancetta-ft8/src/ldpc.rs` (or wherever the matrix lives — find
via the BP iteration). Reuse it.

## Top-level recommendation

- **No immediate action.** The catalogued implementations are
  all reference / sanity-check candidates, not deployment
  candidates. Pancetta's own LDPC decoder (BP + OSD) is more
  mature than any of these for the specific (174, 91) FT8 code.
- **If a refactor of pancetta's LDPC decoder becomes a priority**
  (e.g., as part of a no_std embedded build line), use
  adamgreig/labrador-ldpc as the architectural template
  (MIT-licensed, Rust, no_std — clean fit).
- **For sanity-checking**: hichamjanati/pyldpc (MIT) or
  thadikari/ldpc_decoders (check license) can generate test
  vectors for the implementer thread without exposing pancetta
  to GPL contamination.
