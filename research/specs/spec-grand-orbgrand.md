# Algorithm spec: GRAND family (basic GRAND, ORBGRAND, 1-line ORBGRAND, Segmented ORBGRAND)

## Source attribution
- Primary academic sources (papers — facts, not expression):
  - Duffy, Li, Médard, "Capacity-achieving Guessing Random Additive Noise
    Decoding (GRAND)", arXiv:1802.07010 / IEEE Trans. Inf. Theory.
  - Duffy, An, Médard, "Ordered Reliability Bits Guessing Random
    Additive Noise Decoding" (ORBGRAND), arXiv:2001.00546.
  - Solomon, Duffy, Médard, "Soft Maximum Likelihood Decoding using
    GRAND", and the 1-line / basic ORBGRAND extensions on
    https://granddecoder.mit.edu/papers.
  - Rowshan, "Segmented GRAND: Complexity Reduction through Sub-Pattern
    Combination", arXiv:2305.14892.
- Open-source reference implementations (surveyed for the spec; NOT used
  for verbatim porting):
  - `kenrduffy/GRAND-MATLAB` — MATLAB; **license = "GRAND Codebase
    Non-Commercial Academic Research Use License 021722"** (custom MIT
    affiliation license, restricted to non-commercial academic research).
    Implements basic GRAND, basic ORBGRAND, 1-line ORBGRAND. Non-
    parallelised, instructive.
  - `kenrduffy/SOGRAND-C` — C/MEX; **same custom non-commercial license**.
    See sister spec `spec-grand-soft-output.md`.
  - `mohammad-rowshan/Segmented-GRAND` — Python; **license = GPL-3.0**.
    Targets eBCH (128,106).
- License posture for pancetta: the reference code is **NOT permissively
  licensed** (the kenrduffy repositories are restricted to non-commercial
  academic research; the Rowshan repository is GPL-3.0). Treat all of
  the above as **idea sources only**. Algorithm description, parameter
  values, and step sequencing are taken from the published papers
  (which are facts, not copyrightable expression). The implementer
  thread MUST write Rust from this spec without reading any of those
  repositories.
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

GRAND inverts the classical decoding problem. Instead of asking "which
codeword is closest to the received word", it asks "which noise pattern
was added to a codeword to produce the received word". The receiver
enumerates noise patterns in order of decreasing likelihood. For each
candidate noise, it strips the candidate from the received word and
checks codebook membership via the parity-check syndrome. The first
candidate that yields a valid codeword is the maximum-likelihood (ML)
decode under the assumption that the channel is memoryless and the
noise enumeration order matches descending likelihood.

GRAND is **code-agnostic**: the decoder only needs the parity-check
matrix and a noise-ordering rule. It does not need a code-specific
decoder graph (unlike BP), trellis (unlike Viterbi), or generator-style
list (unlike OSD). This makes it attractive for **short, high-rate**
codes — exactly the regime where conventional ML approximations (BP,
SCL, OSD) leave performance on the table.

For pancetta, the FT8 (174, 91) code is **short and low-rate (r ≈ 0.52)**.
GRAND is most efficient at high rates because the abandonment threshold
scales with `2^(n-k)`; at rate 1/2 with `n-k = 83`, an unbounded GRAND
would be astronomical. ORBGRAND and its descendants use soft channel
information (LLRs) to drastically reduce the search by ordering
candidate noise patterns from "most reliable bits unflipped" outward.
**Even with ordering, the FT8 (174,91) regime is at the upper edge of
the GRAND family's practical envelope**; expectation is that GRAND
candidates compete with BP+OSD only at higher SNRs where the true noise
pattern has very low Hamming weight (≤ 4-5 flips), and the abandonment
threshold can be kept modest. This spec captures the algorithm cleanly
so the implementer can prototype and measure.

## Algorithm description (PROSE ONLY — no code)

### Common notation
- `n` = codeword length (174 for FT8).
- `k` = message length (91 for FT8).
- `H` = (n-k) × n binary parity-check matrix.
- `r` = received hard-decision bit vector, length n.
- `L` = vector of per-bit log-likelihood ratios, length n. `L[i] > 0`
  means bit i is more likely 0; `L[i] < 0` means more likely 1
  (or the opposite sign convention — implementer must be consistent
  with pancetta's existing LLR convention in `decoder.rs`).
- `|L[i]|` = magnitude of the LLR, interpreted as bit reliability.
- Convention: a **test error pattern** is a binary vector `e` of length
  `n`. The decoder tests whether `r XOR e` is a codeword by checking
  `H · (r XOR e)^T == 0`.

### Variant A: Basic (hard-decision) GRAND

#### Inputs
- Hard-decision word `r` (length n).
- Parity-check matrix `H`.
- Optional abandonment threshold `B` (max number of patterns to try
  before declaring decoding failure).

#### Outputs
- Either a decoded codeword (`r XOR e_match`) and the noise pattern
  that produced the match, OR a failure flag.

#### Steps
1. Initialise an enumerator that produces binary vectors in order of
   **non-decreasing Hamming weight**. Enumeration begins with the
   all-zero vector (Hamming weight 0).
2. Within a fixed Hamming weight `w`, the enumerator yields all `C(n, w)`
   vectors. Order within a Hamming-weight class can be arbitrary
   (e.g., colexicographic on the support set).
3. For each candidate `e`:
   a. Compute the test codeword `c = r XOR e`.
   b. Compute the syndrome `s = H · c^T` over GF(2). For efficiency,
      maintain a running syndrome and update it by XORing the columns
      of `H` for the bits that flipped between successive `e` values.
   c. If `s == 0`, return `c` as the decode.
4. If `B` patterns have been tested without a match, return failure.

#### Numerical parameters
- Abandonment threshold `B`: paper reports `B = 2^(n-k)` as the
  asymptotic bound; for FT8 (n-k=83) this is infeasible. Practical
  hard-decision GRAND on FT8 would set `B` to something like `2^20`
  (~10^6) and accept that it only succeeds when the true error has
  Hamming weight ≤ ~3.

#### Edge cases
- All-zero pattern `e = 0` is the first tested — it succeeds when the
  received word is already a codeword (lucky / very high SNR).
- Hamming weight w > some threshold is so improbable that abandoning
  the search is the right move; this is the **GRANDAB** variant in the
  literature.

### Variant B: Basic ORBGRAND (soft-input)

#### Inputs
- LLR vector `L` (length n) — soft channel information.
- Parity-check matrix `H`.
- Maximum logistic weight `W_max` (search depth parameter).

#### Outputs
- Either a decoded codeword + noise pattern, or failure.

#### Step 1: Reliability sort
Compute `|L[i]|` for all bits. Sort indices in **ascending** order of
`|L[i]|` to produce a permutation `π` such that `π[1]` is the
LEAST reliable bit (most likely to be in error) and `π[n]` is the
MOST reliable bit (least likely to be in error). Tie-breaking can be
deterministic on bit index.

The hard-decision bit vector `r` is derived from the sign of `L`:
`r[i] = 0` if `L[i] > 0`, else `r[i] = 1` (subject to LLR sign
convention).

#### Step 2: Logistic weight enumeration

Define **logistic weight** of an error pattern `e` as the sum of
**reliability ranks** of the bits flipped by `e`:
`logistic_weight(e) = Σ rank_of_flipped_bit`
where the rank of the bit at position `π[j]` is `j` (so a flip at the
least-reliable bit costs 1, a flip at the most-reliable bit costs n).

ORBGRAND enumerates error patterns in **non-decreasing logistic
weight**. For each target logistic weight `w_log = 1, 2, 3, ...,
W_max`, it generates **all integer partitions of `w_log` into distinct
parts each in [1, n]**. Each partition `(p_1, p_2, ..., p_h)` with
`p_1 < p_2 < ... < p_h` corresponds to a single error pattern whose
support is `{π[p_1], π[p_2], ..., π[p_h]}`. The Hamming weight of the
pattern equals the number of parts `h`.

Example: `w_log = 6` partitions into `{6}`, `{1,5}`, `{2,4}`, `{1,2,3}`.
Each gives one error pattern flipping the bits at the corresponding
permuted positions.

#### Step 3: Syndrome check per candidate

For each generated pattern `e`:
1. Compute `H · (r XOR e)^T` over GF(2). Use **incremental syndrome
   update**: maintain the previous syndrome and XOR in only the columns
   of `H` corresponding to bits that differ between the previous and
   current pattern.
2. If the syndrome is zero, accept `r XOR e` as the decode.

#### Step 4: Termination

- On first zero syndrome: return the decoded codeword and the noise
  pattern.
- If `w_log` reaches `W_max` without success: declare failure
  (abandonment).

#### Numerical parameters
- `W_max`: paper recommendations are `n` to `2n` for high-rate codes;
  for FT8 (n=174) at low rate, a useful pancetta experiment range is
  `W_max ∈ {50, 100, 200, 400}` with measured BER vs CPU cost.
- Reference paper notes ORBGRAND uses no more than ⌈log₂(n)⌉ bits of
  quantised reliability information; the implementer may either use
  full-precision `|L|` or quantise to e.g. 4 bits.

#### Edge cases
- Logistic weight 0 corresponds to the all-zero error pattern (test the
  received word directly); always test first.
- Multiple partitions of the same `w_log` are tested in arbitrary
  order — the order does not affect correctness, only the FIRST-match
  time.
- Reliability sort can have ties (especially with quantised `|L|`);
  break ties deterministically.

### Variant C: 1-line ORBGRAND

Identical to Basic ORBGRAND except for **how partitions are
enumerated**. The "1-line" name refers to a closed-form sequential
recurrence that produces the **next partition** from the **previous
partition** with constant work, avoiding the recursive partition-
generation overhead. The recurrence (described prose-only in the
academic source):

- Maintain the current partition as a sorted list of distinct parts.
- The "next partition" rule increments the smallest part, propagating
  carries upward when the smallest exceeds its neighbour, similar to
  an odometer with the constraint that parts must remain distinct and
  in [1, n].
- When a logistic-weight class is exhausted, advance to the next class
  by incrementing the total.

The decoder otherwise runs identically. Performance benefit is purely
in candidate-generation throughput, important when the syndrome check
is fast (e.g., hardware) but partition enumeration becomes the
bottleneck.

For a software prototype in pancetta, the 1-line variant is a
nice-to-have — Basic ORBGRAND with a straightforward recursive
partition generator is sufficient for first-pass evaluation.

### Variant D: Segmented ORBGRAND

For low code rates (FT8 is r ≈ 0.52, on the boundary), the
single-pool logistic-weight enumeration generates many patterns whose
syndromes are inconsistent and therefore wasted. Segmented ORBGRAND
(Rowshan 2023, arXiv:2305.14892) partitions the codeword into
**segments** and enumerates **per-segment sub-patterns** that are
**syndrome-consistent within their segment**, then combines segment
sub-patterns in near-ML order using a **two-level integer partition**.

This is more complex to implement and is described as reducing average
queries to **one third** at all SNR regimes for eBCH (128, 106).
Pancetta evaluation should defer to Segmented ORBGRAND only after
basic ORBGRAND has been characterised against the existing BP+OSD
chain.

## Conflict with pancetta's existing mechanisms

Pancetta currently runs **BP (tanh-domain, currently f32 with a v2 spec
in flight to lift to f64) followed by OSD-2** for the (174, 91) LDPC
inner code (`pancetta-ft8/src/decoder.rs`). GRAND-family decoders are
a structurally different approach. Potential roles:

1. **Replacement** at high SNR: ORBGRAND at low `W_max` is extremely
   fast when the true noise has low logistic weight (~strong SNR).
   Could short-circuit the BP-OSD chain when ORBGRAND finds a
   syndrome-clean codeword in the first few queries.
2. **Augmentation** for OSD's known weakness: OSD-2 enumerates
   error patterns by Hamming weight ≤ 2 over the **most reliable
   positions**. ORBGRAND enumerates by logistic weight over the
   **least reliable**, which is the dual viewpoint. Marginal decodes
   that OSD-2 misses because they require flipping 3-4 unreliable bits
   could be reached by ORBGRAND with `W_max ≈ 50–100`.
3. **Independent decoder for noise-floor harvesting**: if the cost is
   low enough, run ORBGRAND in parallel with BP+OSD and union the
   results, then dedupe on `(message_payload, time, frequency)`.

The dominant risk: at low SNR (the regime pancetta cares most about),
ORBGRAND's logistic weight requirement may explode past any reasonable
`W_max`, making it useless precisely where help is needed. Empirical
characterisation on pancetta's hard-200 corpus is mandatory before any
production wire-up.

## Estimated Rust port effort

- **Basic ORBGRAND** (Variant B): ~400 LOC for a clean implementation
  with incremental syndrome update, plus ~100 LOC of tests against a
  toy code first (Hamming (7,4) is the standard pedagogical check),
  then integration into pancetta-ft8 behind a feature gate.
  Estimated 1–2 sessions for the standalone implementation; +1 session
  for harness integration (eval-style benchmarking on hard-200).
- **1-line ORBGRAND**: +100 LOC for the partition recurrence; +0.5
  session.
- **Segmented ORBGRAND**: substantially more (estimated 800-1200 LOC
  including segment selection logic, two-level partition enumeration,
  cross-segment combination); only worth pursuing if Basic ORBGRAND
  shows promise.
- **Hybrid (BP+OSD+ORBGRAND parallel)**: just orchestration in
  `decoder.rs`; ~100 LOC after the standalone decoder lands.

## Implementation notes for the implementer thread

- Insert as a new module `pancetta-ft8/src/orbgrand.rs` (sibling to
  `osd.rs`).
- The (174,91) parity-check matrix is already constructed in
  `pancetta-ft8/src/ldpc.rs` (or wherever the H matrix lives — the
  implementer can find this from `decoder.rs` imports). Reuse it
  directly; do NOT re-derive.
- Incremental syndrome update is critical for performance.
  Pre-compute and cache the n columns of H as `u128`s (n-k = 83 ≤ 128,
  so each column fits in a `u128`). The syndrome is then a single
  `u128` and "XOR in the column of bit j" is one XOR.
- Reliability sort: use `|L[i]|` from the same LLR vector that BP and
  OSD consume. Be careful about the sign convention — pancetta's LLR
  convention is documented in `decoder.rs` near where the BP loop is
  set up.
- Partition generator: a recursive generator over (n, target_weight)
  is clean and fits in ~60 LOC. Yield error patterns as bitsets
  (`u256` or `[u64; 3]`).
- Stopping condition: parametrise `W_max` from `Ft8Config`. Suggested
  experiment range: `W_max ∈ {50, 100, 200, 400, 800}` measured on the
  hard-200 corpus via `pancetta-research`.
- For the first experiment, **run ORBGRAND only on the decoder's
  failure cases** (i.e., when BP+OSD returns nothing). This isolates
  whether ORBGRAND finds anything novel without bothering the existing
  fast path. Use `pancetta-research` to compare:
  - baseline (current BP+OSD): TPs, FPs
  - +ORBGRAND-fallback at `W_max = X`: novel TPs, novel FPs, wall-clock
- Tests: start with Hamming (7,4) round-trip (encode → flip k bits →
  ORBGRAND → check decode = original) as a sanity check before
  pointing it at FT8. Pancetta's existing test patterns in
  `decoder.rs` for BP can be adapted.
