# Algorithm spec: WSJT-X mainline osd174_91 — ordered-statistics decoder

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- File (traceability only; NOT quoted): `lib/ft8/osd174_91.f90`
- Companion: `lib/ft8/decode174_91.f90` (the BP+OSD coordinator that
  calls OSD), `lib/ft8/bpdecode174_91.f90` (BP-only variant)
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

OSD ("Ordered Statistics Decoding") is the fallback decoder used when
belief propagation fails to find a valid codeword. The high-level idea
is: take the BP output (or channel LLRs), find the most-reliable 91
bits, treat them as the "message", encode to get a candidate codeword,
then perturb low-weight subsets of those 91 bits to generate a list of
nearby codewords, and pick the one closest (in soft-distance terms) to
the received word.

The `osd174_91` variant is the FT8 version, decoding the (174, 91) LDPC
code with a 14-bit CRC cascaded into the message bits.

Three different FT8 implementations have written OSD independently:
WSJT-X mainline (this file), wsjtr (`crates/jt9r/src/osd.rs`), and
ft8mon. All three are slightly different — different test-pattern
schedules, different early-termination conditions, different
"preprocessing" rules. This spec catches details from mainline that
may not be in pancetta's existing notes.

## Inputs

- `llr(174)` — soft log-likelihood ratios for the 174 codeword bits.
- `k` — effective message length, in `[77, 91]`. Mainline FT8 always
  passes `Keff = 91` (use all 14 CRC bits cascaded with LDPC).
  - The semantics: `p2 = 91 - k` CRC bits go into the LDPC as
    extra message bits. The remaining `p1 = k - 77 = 14 - p2` CRC
    bits are used for error detection at the end.
  - With `k = 91`: all 14 CRC bits cascaded; **none** used for error
    detection. Detection happens via the final `get_crc14` call on
    the recovered 77 message bits.
- `apmask(174)` — bit mask: 1 if a bit is "locked" via AP, else 0.
  Locked bits are not perturbed by OSD test patterns.
- `ndeep` — OSD depth, 0..6. Maps to `(nord, npre1, npre2, nt,
  ntheta, ntau)` quintuples. See "ndeep parameter tables" below.

## Outputs

- `message91(91)` — recovered 77 payload bits + 14 CRC bits.
- `cw(174)` — full codeword (174 bits).
- `nhardmin` — number of hard-decision flips relative to the input
  hard-decisions. **Negated** to indicate failure: if the final CRC
  check fails, `nhardmin = -nhardmin` (caller checks sign).
- `dmin` — soft-weighted Hamming distance.

## ndeep parameter tables (facts)

The original directly enumerates these:

```
ndeep   nord   npre1   npre2   nt   ntheta   ntau
  0     (return immediately with order-0 codeword)
  1      1      0       0      40    12     unused
  2      1      1       0      40    10     unused
  3      1      1       1      40    12       14
  4      2      1       1      40    12       17
  5      3      1       1      40    12       15
  6      4      1       1      95    12       15
```

`ndeep > 6` is clamped to 6.

Mainline `ft8b` typically calls `osd174_91` with `norder` from the caller
(usually 2), so most calls land at `ndeep = 2` → 1st-order test patterns
with preprocessing rule 1.

For pancetta context: the harder settings (ndeep ≥ 4) blow up the test
pattern count exponentially. `nord = 4` means generating all
`C(91, 4) ≈ 2.8M` 4-bit test patterns; `nt = 95` widens the early-
termination gate, so it's the deepest production-feasible setting.

## Numerical constants

- `N = 174`, `K = 91`, `M = N - K = 83`.
- Gaussian elimination column search range: `[id, id+20]`. The 20-column
  search-radius is "ad hoc" per the in-source comment.
- `boxit91` hash table sizes: `indexes(5000, 2)`, `fp(0:525000)`,
  `np(5000)`. The fp size (~525K) is `2^19` — used for `ntau`-bit
  pattern indexing (max `ntau` is 17 from ndeep=4, so 2^17 = 131K
  would suffice; 2^19 is over-provisioned).

## Algorithm description (prose only)

### Step 1: build (or reuse) the generator matrix

On first call, build a `91 × 174` generator matrix that encodes both:
- The (174, 91) LDPC code.
- The "partial CRC cascade": for each message bit position `i` in
  `[1, 77]`, the corresponding generator row includes the CRC-14 bits
  that would result from a unit-vector message of just that bit.

The construction: for `i = 1..77`, set message bit `i` to 1, run the
CRC-14 on the 91-bit padded message (`m96(1:91)`), put the 14 CRC bits
into positions 78..91 of the message, then `encode174_91_nocrc` to
produce the 174-bit codeword. For `i = 78..91`, just set the bit
directly and encode (these correspond to direct CRC bits cascaded into
the LDPC message).

The matrix is built once and cached (`save` attribute) for the lifetime
of the process.

### Step 2: reorder generator columns by LLR magnitude

Take the absolute values of the input LLRs as a reliability measure.
Sort indices in descending reliability order. Reorder both the
generator matrix columns and an `indices` permutation array so that
the first 91 columns of the *reordered* matrix correspond to the 91
most-reliable received bit positions.

### Step 3: Gaussian elimination on the first 91 columns

Sweep down the diagonal (rows 1..91). For each diagonal `id`:
- Find the first column in `[id, id+20]` with a 1 in row `id`.
  - If `id + 20 > 91 + small`, the search may extend slightly past
    column 91 into the "parity" columns. The 20-column slack handles
    cases where the first 91 columns aren't linearly independent (this
    happens when the most reliable 91 bits include a redundant set).
- If found at column `icol != id`, swap columns `id` and `icol`.
- XOR row `id` into all other rows that have a 1 in column `id`,
  zeroing out column `id` for non-diagonal rows.

After this, the first 91 columns of the reordered generator are the
identity matrix and the remaining 83 columns hold the parity coefficients.

### Step 4: form the "order-0" message and codeword

Permute the hard decisions of the LLR (`hdec`) by `indices`, take the
first 91 → `m0`. This is the "order-0" message (the most-reliable bits
treated as the message).

Encode `m0` against the row-echelon-form generator → `c0` (the order-0
codeword).

Initial `cw = c0`, `nhardmin = sum(c0 XOR hdec)`, `dmin = sum((c0 XOR
hdec) * absrx)`.

### Step 5: test-pattern enumeration (perturbations)

For `iorder = 1` to `nord`:

Initialize `misub` as a length-91 vector with `1`s in positions
`91 - iorder + 1` through `91` (i.e., the last `iorder` bits are set).
This is the "lexicographically smallest weight-iorder pattern".

Loop: walk through all weight-`iorder` patterns of `misub` (via
`nextpat91`, which generates the next-lexicographic-larger pattern;
returns `iflag < 0` when exhausted).

For each pattern, also loop `n1` from `iflag` down to `iend`:
- Set `mi(n1) = 1` (i.e., add the bit at position `n1` to the test
  pattern).
- If any of those new bits collide with `apmaskr` (AP-locked
  positions), skip (`cycle`).
- `me = m0 XOR mi` (perturbed message).

**Subtle optimization (the "delta encoding"):** rather than re-encoding
`me` from scratch every iteration, mainline does the following:
- The first iteration of the inner loop (`n1 == iflag`) computes
  `ce = encode(me)`, `e2sub = (ce XOR hdec)[k+1..N]` (parity-bit
  error pattern), `e2 = e2sub`, `d1 = sum((me XOR hdec)[1..k] * absrx)`.
- Subsequent iterations only need the *change* in `e2`: flipping
  message bit `n1` from `0` to `1` XORs the parity column for `n1`
  (which lives in `g2(k+1:N, n1)`) into `e2sub`. So
  `e2 = e2sub XOR g2(k+1:N, n1)`. The full encode is only re-done if
  the early-termination gate is *not* tripped.

**Early-termination gate:**
- Compute `nd1kpt = sum(e2[1..nt]) + offset` (where `offset` is 1 or 2
  depending on whether this is the first or subsequent iteration). This
  is an estimate of how many parity-bit errors the perturbation would
  produce, *truncated* to the first `nt` parity bits.
- If `nd1kpt > ntheta`, the perturbation almost certainly won't
  improve `dmin` — skip the full encode and continue.
- If `nd1kpt <= ntheta`, do the full encode and full distance
  computation; if the new distance `dd < dmin`, update `dmin`, `cw`,
  and `nhardmin`.

This is the heart of OSD's tractability — without the `nt`/`ntheta`
gate, the test-pattern enumeration is `O(C(91, nord) * 174)`
operations, which is infeasible at `nord ≥ 3`. The gate prunes the
overwhelming majority of patterns before doing the full work.

### Step 6: preprocessing rule 2 (npre2 = 1 path)

When `npre2 == 1`, after the regular test-pattern loops, run an
*additional* pattern enumeration:

**Box-build phase (`boxit91`):**
- For each pair `(i1, i2)` with `i1 > i2` in `1..k`, compute the XOR of
  the `ntau` leading parity columns: `mi(1:ntau) = g2(k+1:k+ntau, i1)
  XOR g2(k+1:k+ntau, i2)`.
- Hash `mi(1:ntau)` to an integer `ipat` (`ntau` bits → integer in
  `[0, 2^ntau)`).
- Store `(i1, i2)` indexed by `ipat` in a hash chain (`indexes`, `fp`,
  `np`).

**Fetch phase:**
- Loop through weight-`nord` test patterns of `m0` (same enumeration
  as the outer loop).
- For each test pattern, compute `e2sub` (parity errors at the
  perturbed message).
- For each `i2` in `0..ntau`:
  - Construct `ui` with a single 1 at position `i2` (or all zeros for
    `i2 == 0`).
  - Compute `r2pat = e2sub XOR ui`.
  - Use `fetchit91` to look up all `(in1, in2)` pairs whose hashed
    parity-XOR matches `r2pat`.
  - For each match: form a *new* test pattern by setting both `in1`
    and `in2` in `mi` (in addition to the existing `misub` bits). If
    the total weight is below threshold OR overlaps with the AP mask,
    skip. Otherwise, encode and check distance.

**What this is doing:** it's a structured search for "complementary"
bit pairs — pairs of bit positions whose parity contributions cancel
out a particular parity-error pattern. The boxes/hash-table machinery
makes the search O(`k^2`) instead of O(`k^4`).

This rule is the most distinctive piece of WSJT-X's OSD relative to
textbook OSD. wsjtr's OSD has a different (simpler) preprocessing rule.

### Step 7: final codeword reordering and CRC check

After all test patterns are exhausted, the winning `cw` is in the
*reordered* column order. Un-permute via `cw(indices) = cw` to get the
original-order codeword.

Extract the first 91 bits as `message91`. Pad to 96 bits with the LDPC
parity in the right slots (positions 1..77 + 83..96 — the CRC bits
land at 83..96). Run `get_crc14` to verify. If bad, **negate
`nhardmin`** to signal failure.

## decode174_91 — the BP+OSD coordinator (referenced)

`decode174_91` orchestrates the BP-then-OSD flow:

- `maxosd < 0`: BP only (30 iterations, early-stop if 5 consecutive
  iterations show no improvement AND iter ≥ 10 AND ncheck > 15).
- `maxosd == 0`: BP, then ONE OSD call with the channel LLRs.
- `maxosd > 0`: BP, then up to `maxosd` (capped at 3) OSD calls with
  *saved BP message-passing sums* (`zsave`) from successive BP
  iterations.

The `zsave` trick: during BP, store `zsum = sum of zn over all
iterations 1..maxosd`. Each OSD call uses a different iteration's
`zsum` as the LLR input. The idea: BP at iteration 1 has different
"soft information" than iteration 3, so feeding multiple iterations'
worth of beliefs to OSD gives multiple shots at recovery.

The BP loop itself:
- Standard log-domain belief propagation.
- `Tmn = product(tanh(-toc/2))` for each variable node.
- `tov = 2 * platanh(-Tmn)` (`platanh` is a piecewise-linear-approx
  of `atanh` used to avoid overflow at `Tmn = ±1`).
- AP-masked bits (`apmask = 1`) skip the `tov` update — their LLR is
  fixed at the channel value.
- Codeword check at the *start* of each iteration: if parity is
  satisfied AND CRC is valid → return success.
- Early stop: 5 consecutive iterations with no decrease in
  unsatisfied-parity count AND iter ≥ 10 AND ncheck > 15 → abort
  (avoid wasting time on hopeless decodes).

## What differs from wsjtr's / ft8mon's OSD

1. **Mainline caches the generator matrix as a Fortran `save`d
   global.** wsjtr rebuilds per call (cheap but redundant). Pancetta
   should cache (lazy_static or once_cell).
2. **Mainline's preprocessing rule 2** (`npre2`-path) uses a hash-table
   structure (boxit91/fetchit91) to find complementary bit pairs.
   wsjtr's OSD does NOT implement this. This is the single biggest
   algorithmic difference and the source of mainline's slight edge at
   `ndeep ≥ 3`.
3. **Mainline's early-termination gate** uses *only* the first `nt`
   parity bits to estimate cost. wsjtr uses all 83 parity bits for the
   same check. The `nt`-truncated version is faster but loses some
   patterns; the wider check is slower but more accurate. Different
   correct trade-offs.
4. **Mainline's Gaussian elimination column-search slack is 20.**
   `ndepth = 6` push it to 95. Wsjtr uses 17 in some paths (per its
   docs). Different reliability cliff edge.
5. **Mainline returns `-nhardmin` on CRC failure** (sentinel). Callers
   must check sign. Pancetta should match this convention or use an
   `Option<DecodeResult>` (cleaner Rust idiom; documented as such).
6. **The `nord` parameter values are different by `ndeep`** than other
   implementations. Specifically: ndeep=2 → nord=1 (not nord=2), which
   is faster than ft8mon's analog setting for the same nominal depth.
7. **`maxosd = 0` semantics:** mainline calls OSD *once* with the
   channel LLRs (not BP-iterated LLRs). wsjtr's analog uses BP output
   even at maxosd=0. This is a small but real divergence.
8. **The `apmaskr` overlap check** (`any(iand(apmaskr(1:k), mi) == 1)`):
   if the test pattern would flip an AP-locked bit, the pattern is
   skipped. This is correct — AP-locked bits are by definition certain.
   Make sure pancetta's OSD honors this; an AP bit flip would corrupt
   the message even if the resulting codeword has low soft distance.

## What wsjtr's / ft8mon's docs paraphrase or miss

1. **The "20-column slack" in Gaussian elimination is `ad hoc`** per
   the in-source comment. Don't try to derive it; treat it as a tuned
   constant.
2. **The `nt`-truncated parity-error count is `nt = 40` for `ndeep <=
   5`, then `nt = 95` for `ndeep == 6`.** Wsjtr's docs may have
   different numbers. Mainline's are the originals.
3. **The CRC check at end fills `m96(1:77) = cw(1:77)`,
   `m96(83:96) = cw(78:91)`.** Note the gap at 78..82 — those are
   padding bits, NOT message bits. The CRC computation operates on
   the 96-bit padded message. Easy to mis-port.
4. **`platanh` (piecewise-linear atanh) is used instead of `atanh`**
   in the BP loop to avoid overflow/NaN when `Tmn → ±1`. Pancetta's
   BP must use the same (or a numerically equivalent) approximation.
5. **Early-stop in BP is *not* "no decrease" — it's "no decrease for
   5 consecutive iterations AND iter ≥ 10 AND ncheck > 15".** Three
   conjunctive conditions, not one.

## Conflict with pancetta's existing mechanisms

- Pancetta's OSD likely doesn't implement `npre2 = 1` (the
  hash-table-based complementary-bit-pair search). Adding it is the
  most distinctive headroom item from this read. Estimated +1-2%
  recall at low SNR — small but real.
- Pancetta's BP early-stop should match the 3-condition gate. If
  pancetta uses a simpler "no decrease for N iter" gate, marginal
  decodes are being abandoned slightly earlier or later than mainline.
- The `zsave` trick (saving multiple BP iteration `zsum` snapshots
  for OSD) requires BP to expose intermediate state. Pancetta may not
  do this currently; if so, only single-OSD-call is available, which
  matches mainline's `maxosd = 0` behavior, not `maxosd = 2`.

## Estimated Rust port effort

- Generator matrix construction + caching: ~80 LOC.
- Column reordering + Gaussian elimination: ~150 LOC.
- Test pattern enumeration (orders 1-4): ~200 LOC.
- Preprocessing rule 2 (boxit91/fetchit91): ~150 LOC.
- BP loop with zsave + early stop: ~200 LOC.
- Total: ~800 LOC, plus ~200 LOC of tests.
- Sessions: 2-3, if pancetta already has BP. If full BP+OSD is a
  fresh build: 4-5.

## Implementation notes for the implementer thread

- The generator matrix is `91 × 174` (or `Keff × 174` if Keff < 91).
  Use `BitVec` or `[u8; 174]` per row. ~16 KB for the full matrix —
  fits in L1.
- The reordered indices array (`indices`) is the key state — keep it
  as a `[u8; 174]` (or `[u16; 174]` if you want headroom). All
  per-pattern enumeration works in *reordered* space; the un-permute
  at the end is cheap.
- `nextpat91` is a standard "next lexicographic combination" — there
  are well-known Rust crates for this (`combinations` etc.) but
  rolling your own ~30 LOC version matches mainline exactly.
- The `boxit91` hash table is sparse — `fp` is sized at 525000 but
  most slots are empty. Use a `HashMap<u32, Vec<(u8, u8)>>` instead;
  cleaner and probably faster than the Fortran array.
- Test against known WAVs that trigger OSD (BP fails, OSD succeeds).
  Wsjtr's test corpus likely has some.
- `Keff = 91` is what `ft8b` always passes. Implement other Keff
  values lazily.
- The CRC computation must match `get_crc14` exactly. The polynomial
  and bit order are FT8-standard but easy to mis-encode.
