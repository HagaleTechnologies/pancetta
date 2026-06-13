# Catalog: Neural LDPC decoder reference implementations

Survey of public open-source implementations of neural / learned
decoders that could plausibly be adapted to FT8's (174, 91) LDPC code.
Compiled 2026-06-08 by reader thread for the clean-room extraction
process. None of the implementations below were read for code-level
detail; this catalog records what exists, license posture, and
whether each is **usable as a direct dependency** vs. **needs clean-
room re-implementation** vs. **paper-only follow-on** (no code
available).

## Summary table

| Project | URL | Paper | License | Framework | Code dims supported | Pretrained weights? | Posture for pancetta |
|---|---|---|---|---|---|---|---|
| CrossMPT | github.com/iil-postech/crossmpt | arXiv:2405.01033 / 2507.01038 (ICLR 2025) | non-commercial research only | PyTorch | configurable, demo on (121,60) LDPC + (31,16) BCH | none distributed | NOT a dependency; clean-room only |
| ECCT | github.com/yoniLc/ECCT | Choukroun & Wolf 2022 (NeurIPS) | **MIT** | PyTorch | configurable, demo on POLAR(64,32) | none | **Permissive — direct dependency possible** if rewritten in Rust or Python sidecar; alternative is clean-room with code-structure reference allowed |
| Mamba-Transformer | (no public repo found) | arXiv:2505.17834 | n/a | n/a | paper reports BCH, Polar, LDPC | n/a | paper-only — follow-on agent could try author GitHub profiles |
| Neural Min-Sum (Lugosch) | github.com/lorenlugosch/neural-min-sum-decoding | Nachmani et al. 2018 | CRAPL ("research-only", non-commercial) | TensorFlow | BCH(63,36), BCH(63,45), BCH(127,106) | none | NOT a dependency; clean-room only; no LDPC support out of box |
| Model-Driven NMS-LDPC | github.com/tjuxiaofeng/A-Model-Driven-Deep-Learning-Method-for-Normalized-Min-Sum-LDPC-Decoding | (tjuxiaofeng) | GPL-3.0 | Python (unspecified DL framework) | LDPC (576, 432) — IEEE 802.11 alist + gmat | none | NOT a dependency; clean-room only; (576,432) ≠ (174,91) so retargeting required |
| thadikari/ldpc_decoders | github.com/thadikari/ldpc_decoders | self | unspecified — check repo | NumPy/SciPy | configurable via alist | n/a (no neural component) | NOT neural; survey artefact |

## Per-project notes

### CrossMPT (iil-postech)

Cross-attention message-passing transformer. The architecture
maintains two distinct representations (magnitude and syndrome) and
iterates masked cross-attention between them, emulating the
variable-node / check-node message passing of BP but with learned
attention weights instead of hand-coded sum-product updates.

Argument structure exposed via CLI: `--code_n`, `--code_k`,
`--N_dec` (decoder layer count), `--d_model` (embedding dimension).
Demo runs use N=6, d=128 for (121,60) LDPC. The (174,91) FT8 code
would require training a new model with `--code_n=174 --code_k=91`
plus the appropriate alist for FT8's parity-check matrix.

**License caveat**: README explicitly states "Codes are available
only for non-commercial research purposes." This is NOT a recognised
OSI-approved license. Treat as paper-and-prose only for pancetta
(MIT/Apache-2.0). The architecture description and hyperparameters
from the published paper ARE usable; the reference code is NOT.

**Posture for pancetta**: Strong candidate for a clean-room
implementation IF and only if the FT8 decoder accuracy gap (currently
5–10% of WSJT-X, the dominant headroom) is worth the operational cost
of running a transformer inference per decode slot. PyTorch sidecar
would be a 15-second-cadence runtime; embedded transformer inference
in Rust would need `candle` or `burn` and aggressive quantisation.
**Recommend deferring** until simpler hb-* lines (BP precision lift,
ORBGRAND fallback, content-score fusion) are exhausted.

### ECCT (yoniLc) — only MIT-licensed entry

Error Correction Code Transformer. First transformer-based
model-free decoder for ECCs. Uses code-structure masking to inject
the parity-check topology into the attention pattern.

**License**: **MIT**. This is the only neural-decoder reference
implementation surveyed with a fully permissive license. The
implementer thread is permitted to read this code more directly
(per the clean-room feedback: paraphrasing is preferred even with
permissive licenses, but verbatim copy with attribution would also
be acceptable).

Demo code uses POLAR(64,32). The codebase includes a `Codes_DB`
folder suggesting other codes can be added by dropping alist /
gmat files. Retargeting to FT8 (174,91) requires:
1. Providing the (174, 91) parity-check matrix in the expected
   alist format.
2. Re-training from scratch (no pretrained weights are distributed
   anyway).
3. Possibly tuning `N_dec` and `d_model` for the larger code; the
   paper reports N=6 d=32 for shorter codes.

**Posture for pancetta**: The most viable neural-decoder candidate
because the license is clean and the architecture is well-documented.
Same operational caveat as CrossMPT (transformer inference cost).
**Recommend follow-on agent if neural decoding becomes the priority.**

### Mamba-Transformer (arXiv:2505.17834)

Paper-only at time of survey. Authors: Shy-el Cohen, Yoni Choukroun,
Eliya Nachmani (Tel Aviv U / Technion). Note that Choukroun and
Nachmani also authored ECCT, so a code release may follow the
typical ~6-month lag after publication.

**Posture for pancetta**: Wait. A follow-on agent could try the
authors' GitHub profiles in 3–6 months. Note also the SECOND Mamba
paper, "Scalable Mamba-Based Message-Passing Neural Decoder",
arXiv:2605.10681 — also paper-only.

### Neural Min-Sum (Lugosch)

TensorFlow implementation of Nachmani et al. 2018's neural belief
propagation. Trains per-edge weights for the message-passing graph,
unrolling iterations into a feedforward network.

**License**: CRAPL (Community Research and Academic Programming
License) — explicitly non-commercial / research-only. NOT
OSI-approved. Treat as paper-and-prose only.

Architecturally smaller than the transformer approaches:
hyperparameters are a learned weight per edge × iteration.
For (174, 91) LDPC with the FT8 parity matrix's roughly 600 edges,
this gives a few thousand trainable parameters total — orders of
magnitude smaller than a transformer.

**Posture for pancetta**: The simplest, cheapest neural approach.
A clean-room Rust implementation in `pancetta-ft8` is plausible
(weights are a small fixed buffer; inference is just a weighted
min-sum BP). Training would need to be done offline (e.g., PyTorch
or Sionna), then weights baked in as a `[[f32; n_iter]; n_edges]`
constant. **Recommend as the second-choice neural follow-on if ECCT
is judged too expensive at runtime.**

### Model-Driven NMS-LDPC (tjuxiaofeng)

Unfolds normalised min-sum (NMS) iterations into a feedforward
neural net (NNMS) and a shared-weight variant (SNNMS). Targets
(576, 432) LDPC. Distinct from full neural BP — only the
normalisation factors are learned, not per-edge weights.

**License**: GPL-3.0. NOT directly usable for pancetta MIT/Apache.
Clean-room only.

**Posture for pancetta**: Architecturally appealing — fewer
learned parameters than full neural BP, mathematically grounded
in the existing min-sum decoder. A Rust port would just add a
small lookup `[f32; n_iter]` of learned normalisers to the
existing min-sum loop in `decoder.rs`. **Consider as a "next
step after BP f64 lift" experiment** — small mechanism, easy to
characterise.

### thadikari/ldpc_decoders

Pure-NumPy implementation of multiple LDPC decoders (min-sum,
sum-product, OSD, BF). Not neural — included here as a survey
artefact and as a possible reference for the clean-room
implementer to sanity-check Rust ports of classical decoders (it
is, per other public mentions, MIT-licensed though the surveyor
did not verify).

**Posture for pancetta**: Reference / sanity-check tool. Not a
deployment dependency.

## Top-level recommendation

For near-term FT8 decoder improvements:
1. **Do NOT** wire in any neural decoder yet. Operational cost
   (PyTorch sidecar or embedded inference) outweighs the proven
   benefit for the headroom remaining in classical decoders
   (BP-f64 lift, ORBGRAND fallback, soft-output fusion).
2. If a neural decoder becomes a priority:
   - **First choice**: ECCT clean-room (MIT, smallest license risk).
   - **Second choice**: Neural Min-Sum clean-room (smallest model,
     simplest training, fits cleanly into existing min-sum loop).
   - **Defer**: CrossMPT, Mamba-Transformer until license / code
     release situation is clearer.
3. Periodic re-survey (6 months) for new releases, particularly
   from the Choukroun / Nachmani / Médard groups, who tend to
   release code post-publication.
