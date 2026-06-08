# Algorithm spec: Soft-Output GRAND family (SOGRAND, soft-output ORBGRAND, ORDEPT)

## Source attribution
- Primary academic sources (papers — facts, not expression):
  - Solomon, Duffy, Médard, "Soft Maximum Likelihood Decoding using
    GRAND" (https://granddecoder.mit.edu/papers).
  - Galligan, Solomon, Riaz, Médard, Duffy, "Iterative Soft-Input
    Soft-Output Decoding with Ordered Reliability Bits GRAND",
    arXiv:2207.06691.
  - Yuan, Duffy, Médard, "Soft-output (SO) GRAND and Iterative Decoding
    to Outperform LDPCs", arXiv:2310.10737.
  - Condo, Bioglio, "Ordered Reliability Direct Error Pattern Testing
    Decoding Algorithm" (ORDEPT), arXiv:2310.12039 / TechRxiv preprint.
  - Choi, Park, Duffy, et al., "Leveraging Code Structure to Improve
    Soft Output for GRAND, GCD, OSD, and SCL", arXiv:2503.16677.
- Open-source reference implementations (surveyed for the spec; NOT used
  for verbatim porting):
  - `kenrduffy/SOGRAND-C` — C/MEX bindings for MATLAB.
    **License: "GRAND Codebase Non-Commercial Academic Research Use
    License 021722"** (custom, non-commercial only). Files include
    `SOGRAND_mex.c` (core C), `SOGRAND_bitSO.m`, `SOGRAND_blkSO.m`
    (MATLAB drivers for bit- and block-level soft output),
    `sim_product.m`, `sim_blkSO_acc.m`, `sim_BLER_UER.m` (simulation
    drivers).
  - No public ORDEPT reference implementation was found at the time of
    this spec (the paper is the primary source).
- License posture for pancetta: **NONE of the reference code is
  permissively licensed**. Algorithm details below are taken from the
  papers (facts, not copyrightable expression). The implementer thread
  MUST write Rust from this spec without consulting the SOGRAND-C
  source.
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

A hard-decision GRAND variant (ORBGRAND, basic GRAND) returns a single
codeword and a binary "decoded / failed" flag. For **iterative**
decoding — particularly in concatenated code constructions like the
product codes for which SOGRAND was developed, OR for FT8's "decoder
chain where multiple candidate decodings could be ranked by
confidence" — a **soft output** is needed: a per-bit posterior LLR
that quantifies how confident the decoder is about each output bit.

Soft-output GRAND adds soft outputs to ORBGRAND/GRAND in two flavours:

1. **List-of-candidates approach (SOGRAND, ORDEPT)**: keep the first
   `L` codewords found instead of stopping at the first. Use the list
   to compute per-bit APP via a Σ over candidates weighted by their
   noise-pattern likelihood.
2. **Rank-based approach (early SO-ORBGRAND papers)**: the rank `q` at
   which the FIRST codeword was found is an estimator of the
   noise-pattern likelihood, which in turn estimates the codeword
   APP. Per-bit APP is derived from a single codeword + its rank.

For pancetta, the relevant uses are:
- A confidence score that **subsumes** the current ad-hoc
  `confidence_score` in `pancetta-ft8/src/decoder.rs`, giving a
  principled posterior probability per decoded message instead of
  the current heuristic.
- A soft-output decoder that could feed a future **iterative outer
  scheme** if pancetta ever adopts cross-message structure (e.g., QSO
  continuity priors as a soft outer code).

## Algorithm description (PROSE ONLY — no code)

### Variant A: List-of-candidates SOGRAND

#### Inputs
- LLR vector `L` (length n).
- Parity-check matrix `H`.
- Maximum logistic weight `W_max` and/or candidate-list cap `L_max`
  (e.g., `L_max = 4`).
- Optional **APP normalisation**: a noise-statistic estimator
  `Pr(e | channel)` for each candidate error pattern.

#### Outputs
- A list of up to `L_max` codewords `{c_1, c_2, ..., c_L}`, each
  paired with its noise pattern `{e_1, e_2, ..., e_L}` and an
  unnormalised likelihood `λ_i = Pr(received | c_i transmitted)`.
- Per-bit soft output `LLR_out[j]` for each output bit `j`.

#### Steps
1. Run ORBGRAND as in `spec-grand-orbgrand.md` (logistic-weight
   enumeration of test error patterns), but instead of stopping at the
   first zero syndrome, **continue until either L_max codewords have
   been found OR W_max is reached**.
2. For each codeword found, compute its **likelihood weight** under the
   channel. Under AWGN with known noise variance:
   `λ_i = Π_j (1 - p_j)^{1 - e_i[j]} · p_j^{e_i[j]}`
   where `p_j = 1 / (1 + exp(|L[j]|))` is the per-bit error probability
   derived from the LLR. In log-domain:
   `log λ_i = Σ_{j : e_i[j] = 1} log(p_j / (1 - p_j))
            = - Σ_{j : e_i[j] = 1} |L[j]|`
   (up to an additive constant that cancels in normalisation).
3. Normalise across the candidate list to APPs:
   `α_i = λ_i / Σ_k λ_k`. Note that this is APPROXIMATE — the true
   APP would marginalise over ALL codewords; the list is an
   approximation.
4. Compute per-bit soft output. For bit position `j`:
   `Pr(bit j = 0 | y) ≈ Σ_{i : c_i[j] = 0} α_i`
   `Pr(bit j = 1 | y) ≈ Σ_{i : c_i[j] = 1} α_i`
   `LLR_out[j] = log( Pr(j=0|y) / Pr(j=1|y) )`
5. The "extrinsic" LLR (for iterative decoding) is `LLR_out[j] - L[j]`
   if `L[j]` was the prior; this prevents double-counting in the
   outer-decoder loop.

#### Numerical parameters
- `L_max`: SOGRAND papers report diminishing returns past `L = 4`–`8`.
- `W_max`: as in basic ORBGRAND, code-rate dependent. For FT8
  (174, 91), expect `W_max = 100`–`200` is enough to fill the list at
  reasonable SNRs.
- Floor on `α_i`: avoid log(0); clamp `LLR_out` to `±50` or similar.

#### Edge cases
- Only one codeword found in the budget: `α_1 = 1`, `LLR_out[j]` is
  ±∞ for every bit (clip to ±50). The soft output is effectively a
  hard decision.
- Zero codewords found: decoder failure; no soft output.
- All candidates agree on bit `j`: `LLR_out[j]` saturates (correct
  behaviour — the soft output reflects high confidence).

### Variant B: Rank-based soft-output ORBGRAND (per Galligan 2022)

#### Inputs
- Same as ORBGRAND.

#### Outputs
- The decoded codeword `c`.
- A **per-codeword posterior probability estimate** `P(c | y)`.
- Per-bit soft output derived from `P(c | y)` and the noise pattern.

#### Steps
1. Run ORBGRAND. On first zero syndrome, record:
   - The decoded codeword `c`.
   - The noise pattern `e`.
   - The **rank** `q` at which the codeword was found (i.e., the
     number of test patterns attempted, including the successful one).
2. Estimate `P(c | y)` using a closed-form expression that depends on
   `q`, the channel noise model, and the code rate. The derivation
   in the paper assumes that all codewords (except the decoded one)
   are equally likely a priori, and uses the rank `q` as a sufficient
   statistic for the posterior. Concretely (paraphrased from the
   paper): higher `q` (more queries needed) → lower posterior
   probability that the found codeword is correct.
3. Per-bit soft output: for bits that match between `c` and the
   hard-decision of `r` (no flip), the output LLR carries the sign of
   `L[j]` with magnitude inflated proportional to `P(c | y)`. For
   bits that were flipped (`e[j] = 1`), the output LLR has sign
   opposite to `L[j]` with magnitude proportional to `P(c | y)`.

This variant is **less accurate** than the list-of-candidates approach
(it uses only the top-1 codeword and its rank) but is **much cheaper**
(no need to keep enumerating after the first match).

### Variant C: ORDEPT (Ordered Reliability Direct Error Pattern Testing)

Per Condo & Bioglio (arXiv:2310.12039 / TechRxiv): ORDEPT modifies
ORBGRAND's pattern-generation step. Instead of iterating over
logistic-weight classes and enumerating ALL integer partitions of
each (many of which yield syndrome-inconsistent patterns), ORDEPT
generates only patterns whose syndrome can match the observed
syndrome of `r`, using the parity structure of `H` to prune. This
substantially reduces the average number of queries.

The soft-output computation is the same as SOGRAND Variant A above:
a list of codewords is collected, each weighted by its noise
likelihood, per-bit APPs are computed by summation over the list.

Key prose-level distinguishing features of ORDEPT (without quoting
source code):

- Pattern generation is **syndrome-driven**: each candidate is
  constructed to be consistent with the observed syndrome from the
  start, rather than tested for consistency after generation.
- The ordering of candidate patterns is still by logistic weight, so
  near-ML behaviour is preserved.
- Per-bit and per-block soft outputs are produced in the same step
  (no separate forward / backward pass).
- The paper reports improvements in both BER and average query count
  vs. ORBGRAND for individual decoding and iterative product-code
  decoding.

Numerical parameters (per paper): list size `L_max = 2`–`4` is
reported as sufficient; `W_max` similar to ORBGRAND.

### Numerical constants (facts across the SO-GRAND family)
- Default list size for soft output: `L_max ∈ {2, 4, 8}`.
- Default `W_max` for short codes: paper recommendations `1–2 · n`,
  i.e., 174 to 348 for FT8.
- LLR clip on output: `±50` is standard to prevent f64 overflow on
  downstream operations.
- Noise prior for AWGN soft input: `p_j = 1 / (1 + exp(|L[j]|))`.

### Edge cases (across all variants)
- **Equally-likely candidate ties**: tie-break deterministically on
  insertion order to keep decoder behaviour reproducible.
- **No codeword found** within `(W_max, L_max)`: emit a decode failure
  with confidence 0; do NOT pass forward a low-confidence guess.
- **All candidates identical on a bit**: output LLR saturates; clip.

## Conflict with pancetta's existing mechanisms

Pancetta's existing decoder (`pancetta-ft8/src/decoder.rs`) emits a
`confidence_score` per decode that is a heuristic blend of sync
strength, LDPC pass count, and OSD success. SO-GRAND offers a
**principled posterior probability** as a drop-in replacement (or
augmentation):

- If ORBGRAND is wired in as a fallback (per `spec-grand-orbgrand.md`),
  upgrading it to SOGRAND (Variant A) at the cost of running until the
  list is full rather than stopping at the first match gives a
  posterior probability for free.
- The current `is_plausible()` / `has_high_risk_fp_pattern()` filter
  stack (per pancetta-qso) gates emissions on heuristic content
  patterns. A SOGRAND posterior offers a **principled** filter
  threshold: `if P(c|y) < τ then drop`. This could simplify the
  hb-103 content-score line.
- Combined with hb-103 (content score) and hb-062 (continuity filter):
  the SO-GRAND posterior is an **orthogonal** signal. Fusing the
  three should give better operating curves than any one alone.

The risk: SO-GRAND requires enumerating the list, so it's strictly
more expensive than first-match ORBGRAND. For tight TX-slot budgets,
the implementer might run ORBGRAND first, fall back to SOGRAND only
on candidates flagged as marginal.

## Estimated Rust port effort

- **SOGRAND Variant A** (list-of-candidates): +200 LOC on top of the
  Basic ORBGRAND scaffold. The likelihood weight `log λ_i = -Σ |L[j]|`
  is trivial. The per-bit APP loop is O(L · n).
- **Rank-based Variant B**: +50 LOC on top of Basic ORBGRAND — just
  record the rank `q`, run a closed-form posterior estimate.
- **ORDEPT (Variant C)**: substantial — the syndrome-driven candidate
  generation needs careful derivation; estimated +400 LOC and 2-3
  sessions. Recommend deferring until after Variant A is measured.

## Implementation notes for the implementer thread

- Wire in the same module as Basic ORBGRAND
  (`pancetta-ft8/src/orbgrand.rs`). SOGRAND Variant A is best
  expressed as an option flag on the same enumerator: "collect up to
  L_max instead of returning on first match".
- The output of the decoder needs a new struct, e.g.:
  ```text
  // shape sketch — implementer writes their own
  pub struct SoftOutputDecoded {
      payload: [u8; 91],
      candidates: Vec<(Codeword, NoisePattern, f64)>, // (c, e, log_λ)
      posterior: f64, // top-1 P(c|y) ∈ [0, 1]
      per_bit_llr: [f64; 174],
  }
  ```
- The likelihood-weight computation uses `|L[j]|` from the same LLR
  buffer that BP consumed; do NOT re-derive.
- Per-bit APP loop is a tight inner loop: vectorise where it pays.
  L_max ≤ 8 means scalar f64 is probably fine.
- For pancetta-research integration: emit `posterior` alongside the
  decode in the manifest; eval can then sweep thresholds and report
  precision-recall curves vs. the heuristic `confidence_score`.
- DO NOT trust the posterior outside the operating range it was
  validated for. On corner cases (zero candidates, all-saturated
  LLRs), set the posterior to a documented sentinel (e.g., NaN or
  `-1.0`) and have downstream code reject those decodes outright.
- Tests: round-trip on Hamming (7,4) with known AWGN noise; verify
  that as SNR → ∞, posterior → 1; as SNR → 0, posterior → 1/2^k
  uniformly across codewords.
