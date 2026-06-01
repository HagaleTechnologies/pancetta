---
slug: mr-009-deep-ideation
mode: ft8
state: meta-research (ideation summary + bank-aggregator journal)
created: 2026-06-01T18:00:00Z
last_updated: 2026-06-01T18:00:00Z
branch: iter/2026-06-01-ideation-aggregator
parent_thread: post-batch-14 (hb-086 V1 graduated 2026-05-31; V2/V3/hb-088/hb-087 S2 shelf cluster)
sources_surveyed: 8 parallel ideation sub-passes (architectural / diversity / metric / corpus / human-loop / cross-time / foundation-models / extras)
candidates_admitted: 115 (hb-101..hb-215)
bank_state_before: 100 entries (hb-001..hb-100)
bank_state_after: 215 entries (hb-001..hb-215)
journal_purpose: cross-pass summary + cross-category synthesis + top-10 ranking + recommended next-session picks
---

## Framing

After batch 14 (hb-086 V1 graduated — joint-pair-retry on residual,
+12 hard-200 rec) and the V2/V3/hb-088/hb-087 S2 shelf cluster, the
project sits on a recall plateau against a curated corpus
(hard-200 at 123.7% of jt9; +0.013993 composite over the
5-day-10-graduation cumulative). The bank was 100 entries deep with
strong recent additions from mr-008 (hb-089..hb-100), but the
plateau itself motivated **a deeper assumption-challenge pass**:
not "what's the next mechanism within the existing pipeline?" but
"what assumption underlying the existing pipeline is most likely
to be wrong?"

This pass spawned 8 parallel sub-agents, each owning a different
assumption-axis:

1. **Architectural** — what if pancetta's pipeline shouldn't be
   sequential hard-decision stages?
2. **Diversity** — what if the single-stream ceiling is real, and
   the next 1.0× lives in a second independent measurement?
3. **Metric** — what if per-slot-recall-on-hard-200 is the wrong
   axis to optimize?
4. **Corpus** — what if the curated truth set is silently shaping
   the decoder toward overfit?
5. **Human-loop** — what if pure autonomy is leaving operator-
   provided signal on the table?
6. **Cross-time** — what if slot-locality is too narrow a
   temporal scope?
7. **Foundation-models** — what if classical-DSP + 20K-param-CNN
   is the wrong ML stack for 2026?
8. **Extras** — wild-card / cross-category novel ideas that didn't
   fit elsewhere.

The aggregator (this journal) merges all 8 into the bank with
sequential IDs hb-101..hb-215.

## Per-category counts + top picks

| Category | Range | Count | Wild | Top picks |
|---|---|---|---|---|
| Architectural | hb-101..hb-114 | 14 | A6, A10, A14 (3) | A4 joint multi-candidate (hb-104); A8 sync uncertainty distribution (hb-108); A1 soft-output decoder (hb-101) |
| Diversity | hb-115..hb-128 | 14 | D4, D5, D9, D12, D13 (5) | D1 dual-Kiwi MRC (hb-115); D2 jt9⊕jtdx⊕panc ensemble (hb-116); D8 multi-sync-window LLR avg (hb-122) |
| Metric | hb-129..hb-143 | 15 | M10, M11, M12, M14, M15 (5) | M5 saturation-aware composite (hb-133) — "definitely worth shipping"; M9 adversarial corpus (hb-137); M1 TTFD (hb-129) |
| Corpus | hb-144..hb-157 | 14 | none flagged | C3 adversarial mutual-masking (hb-146); C1 cross-decoder consensus (hb-144); C7 high-jt9-novel-density (hb-150) — "cheapest to bootstrap" |
| Human-loop | hb-158..hb-172 | 15 | H6 only (1) | H4 STOP key (hb-161) — safety-critical for Phase 5; H3 `*` priority boost (hb-160); H8 real-time alarms (hb-165) |
| Cross-time | hb-173..hb-186 | 14 | T7, T10, T11, T12, T13 (5) | T1 within-QSO context graph (hb-173); T13 operator-state HMM (hb-185); T11 federated cross-operator priors (hb-183) |
| Foundation-models | hb-187..hb-201 | 15 | F8, F11, F14, F15 (4) | F4 end-to-end Transformer (hb-190); F6 SSL pretraining on operator firehose (hb-192); F1 Wav2Vec2 frozen-encoder (hb-187) |
| Extras | hb-202..hb-215 | 14 | none flagged | X5 WSJT-X plugin adapter (hb-206) — community lever; X12 SIMD-BP (hb-213) — elapsed budget enabler; X8 log-as-ground-truth (hb-209) |
| **Totals** | hb-101..hb-215 | **115** | **23 wild (20%)** | exactly on wild-card target |

## Cross-category synthesis (high-conviction directions)

Several ideas appear in multiple categories under different framings.
These convergences are stronger signal than any single-category
recommendation.

### Direction A — "Operator's own log as decoder input"
- hb-150 (C7: high-jt9-novel-density tier from operator archive)
- hb-175 (T3: cross-session ADIF-driven AP pool)
- hb-209 (X8: log-as-ground-truth retroactive labeler)
- hb-192 (F6: SSL pretraining on operator firehose)

All four exploit the operator's already-captured data, just at
different points in the pipeline (truth set, AP pool, ground-truth
labels, pretraining corpus). **Strategic implication**: a single
"operator-archive-as-asset" infra investment unlocks 4 hypotheses.

### Direction B — "Multi-decoder fusion"
- hb-105 (A5: jt9 + pancetta LLR-sum at input)
- hb-116 (D2: jt9 ⊕ jtdx ⊕ pancetta codeword-vote at output)
- hb-141 (M13: cross-decoder-disagreement recall metric)
- hb-144 (C1: cross-decoder consensus truth tier)

Four different uses of the same underlying primitive: **getting
jtdx baseline and jt9 LLR-extraction stood up**. Cross-cutting
infra prerequisite (see "Infra needs" below).

### Direction C — "Distributional output replaces hard gates"
- hb-101 (A1: soft-output decoder, codeword-posterior list)
- hb-102 (A2: probability-of-existence map replaces Costas gate)
- hb-103 (A3: continuous trust-score FP filter)
- hb-107 (A7: PartialDecode first-class object)
- hb-108 (A8: time-frequency uncertainty distribution at sync)

Five architectural ideas converge on "stop collapsing distributions
to point estimates inside the decoder." If any one of these wins,
the pipeline shifts in a way that makes the others cheaper to add.

### Direction D — "Operator-state-aware decoding"
- hb-173 (T1: within-QSO context graph drives pair-conditional AP)
- hb-185 (T13: operator-state HMM)
- hb-191 (F5: GPT-style cross-slot QSO language model)
- hb-188 (F2: Whisper-tiny in-slot transcriber with grammar prior)

QSO grammar and operator state encoded as priors that feed the
decoder. Different mechanisms (table, HMM, LM, transcriber) but
shared thesis: QSO context carries decode-relevant information
the slot-local decoder ignores.

### Direction E — "Operator-network as constellation"
- hb-120 (D6: friend-network decode-hash via cqdx)
- hb-127 (D13: public-KiwiSDR scavenging fleet)
- hb-183 (T11: federated cross-operator priors via cqdx)
- hb-206 (X5: WSJT-X plugin adapter — community lever)

Pancetta as a node in a network rather than a solo decoder. X5 is
the deployment vector that enables the others to bootstrap.

### Direction F — "Elapsed budget enabler"
- hb-213 (X12: SIMD/AVX-512 BP) — 3× speedup buys 3× headroom
- hb-205 (X4: warm-up via serialized hot state) — cold-start fix
- hb-129 (M1: TTFD metric) — surfaces latency as first-class
- hb-135 (M7: CPU-cost-adjusted recall) — Pareto-aware composite

The composite is silently ignoring elapsed; X12 fixes the budget
itself, the others make the budget visible.

## Cross-cutting infra prerequisites

Several ideas share infrastructure dependencies. These are
**infra-shaped**, not single-hypothesis-shaped, and warrant explicit
flagging:

1. **Chronological eval tier** (the harness shuffles WAVs today).
   Required by hb-173 (T1 within-QSO), hb-180 (T8 propagation
   regime), hb-182 (T10 time-aware calibration), and several
   cross-time entries. Already flagged in cross-time ideation's
   cross-cutting notes. **One-time infra investment unlocks ~5
   hypotheses.**

2. **Shared CrossTimeState plumbing** (per-callsign state
   storage). Required by hb-173, hb-179, hb-182, hb-183, hb-185.
   Recommended before approving the SECOND per-callsign state
   mechanism, to avoid 5 parallel per-callsign tables. hb-057
   already scoped most of the storage; extending its lifetime
   covers the rest.

3. **jtdx baseline integration** (build + cache). Required by
   hb-105, hb-116, hb-141, hb-144. Single ~4-8 hour infra
   investment unlocks 4 hypotheses.

4. **Saturation-aware composite (hb-133, M5)**. This is itself a
   hypothesis but is also infra: it unblocks the corpus refresh
   the 2026-05-30 survey already recommended, AND lets the
   cumulative graduation log survive future refreshes. **Highest
   work-to-impact ratio of all 115 candidates** per the metric
   ideation summary.

5. **Operator-archive-as-asset infra**. The "operator's own log
   as decoder input" Direction A above requires (a) cqdx capture
   firehose access (already exists per cqdx docs), (b) ADIF
   reader (exists in pancetta-qso::callsign_continuity), (c)
   labeled-corpus extraction (new, ~hb-209 scope). The infra
   itself unlocks Direction A's 4 hypotheses.

## TOP-10 overall priority ranking

Ranking integrates per-category priority scores, cross-category
convergence (Directions A-F membership), infra-unblock value, and
defensible_prior strength. One-line rationale per entry.

| # | hb-NNN | Title | Rationale |
|---|---|---|---|
| 1 | hb-133 | Saturation-aware composite (M5) | Highest work-to-impact ratio; 1 session of work, no decoder change, unblocks corpus refresh + preserves cumulative graduation log — INFRA-shaped force multiplier. |
| 2 | hb-150 | High-jt9-novel-density tier (C7) | Cheapest corpus to bootstrap; directly targets the 30% real recall headroom; Direction A member. |
| 3 | hb-173 | Within-QSO context graph (T1) | Top-3 cross-time disruption; Direction D anchor; defensible measurement path today; complementary to hb-048 a7 (already scoped). |
| 4 | hb-115 | Dual-KiwiSDR space-diversity MRC (D1) | Biggest single-mechanism composite lever (+3 dB on independent noise); hb-075 already proved MRC works; capture infra half-built. |
| 5 | hb-161 | `Q` STOP key (H4) | Required for Phase 5 by basic safety; 1 session; every press is gold training data; Direction F (operator-archive) input. |
| 6 | hb-187 | Wav2Vec2 frozen-encoder front-end (F1) | Quickest of top foundation-model picks (3-4 sessions, $10 train); ICASSP 2025 cite specifically mentions FT4/FT8; bounded OOD risk via frozen encoder. |
| 7 | hb-194 | Bayesian deep ensembles over OSD CNN (F8) | Cheapest deploy of any FM idea (2 sessions, $10); directly addresses hb-064 S2's overconfident-wrong failure; useful diagnostic even if doesn't win. |
| 8 | hb-129 | TTFD per-slot metric (M1) | 1 session; re-ranks hb-091 up (a8 early-decode) and hb-079 down (multipass); surfaces real operational axis the composite ignores. |
| 9 | hb-146 | Synthetic adversarial pair corpus (C3) | Could unshelve V2/V3 by giving them the regime they were designed for; 1-day generator extension on existing infra. |
| 10 | hb-206 | WSJT-X plug-in adapter (X5) | Community lever; deployment vector for Direction E; secondary benefit = log-as-truth feedback loop (hb-209) bootstraps via WSJT-X UI. |

Honorable mentions just below the cutoff: hb-104 (A4 joint
multi-candidate — most aligned with hb-088 structural finding,
but plan-sized), hb-160 (H3 `*` key boost — 0.5 session, trivial
implementation), hb-209 (X8 log-as-ground-truth — Direction A
anchor), hb-213 (X12 SIMD-BP — elapsed budget enabler).

## Bank statistics

**Before mr-009**: 100 active+shelved+graduated entries (hb-001..hb-100).
Counts by status (pre-pass, approximate):
- Active/pending/deferred: ~30
- Shelved: ~50
- Graduated: ~20
- Open territories (per mr-008 framing): 5

**After mr-009**: 215 entries (hb-001..hb-215). Counts by status:
- Active/pending/deferred: ~145 (added 115 pending entries)
- Shelved: ~50 (unchanged)
- Graduated: ~20 (unchanged)
- Wild-card ratio in new entries: 23/115 = 20% (exactly on target)

**Bank shape diversification post-pass**:
- 8 new structural attack axes (previously: 5 territories)
- Multiple infra-shaped entries (hb-133, hb-209) for the first time
- Direction-clustered convergences (A-F) provide cross-category
  prioritization signal that single-pass ideation lacks

## Recommendation for the next session (5-8 picks)

Based on the TOP-10 ranking + the "one decoding session" budget,
recommend tackling these in this order:

1. **hb-133 (saturation-aware composite)** — 1 session, no
   decoder change, unblocks corpus refresh. Single highest-leverage
   move; do this first.
2. **hb-150 (high-jt9-novel-density tier)** — 1 session, zero new
   audio, directly targets the 30% real recall headroom that
   hard-200 hides.
3. **hb-129 (TTFD metric)** — 1 session, re-ranks the current
   bank's top hypotheses; surfaces real operational axis.
4. **hb-161 (Q STOP key)** — 1 session, Phase-5-safety-critical,
   no risk of regression.
5. **hb-194 (Bayesian deep ensembles over OSD CNN)** — 2 sessions,
   cheapest FM bet, directly addresses hb-064 S2 failure mode.

Stretch picks (if time):
6. **hb-146 (synthetic adversarial pair corpus)** — could
   resurrect shelved V2/V3 if they win on the pair tier.
7. **hb-209 (log-as-ground-truth)** — 1-2 sessions, Direction A
   anchor, enables labeled-corpus growth for everything downstream.

What NOT to tackle first session:
- hb-104, hb-190 (plan-sized, multi-month research)
- hb-115, hb-127 (operator-physical / hardware-gated)
- hb-192 (6-month wall-clock data collection)

These remain in the bank for plan-sized commitments later.

## Hard constraints honored

- No push, force-push, --no-verify, reset --hard, revert, rebase
  performed by the aggregator.
- Cherry-pick used only to bring in source ideation files (commits
  0a7d755, f71131d, 2c1d17c, 7681a92, 09523f7, 0a3996a, f9541ef;
  the bundled 0a3996a covered cross-time + extras; 8952ec2 was a
  duplicate of extras and skipped).
- No modification to source ideation files.
- No production-code changes in pancetta-ft8 or pancetta-research.
- Aggregator branch: iter/2026-06-01-ideation-aggregator.
- Selective git add (no -A).

## Files in this batch

- research/ideation/2026-06-01-architectural.md (existing; cherry-picked)
- research/ideation/2026-06-01-diversity.md (existing; cherry-picked)
- research/ideation/2026-06-01-metric.md (existing; cherry-picked)
- research/ideation/2026-06-01-corpus.md (existing; cherry-picked)
- research/ideation/2026-06-01-human-loop.md (existing; cherry-picked)
- research/ideation/2026-06-01-cross-time.md (existing; cherry-picked)
- research/ideation/2026-06-01-foundation-models.md (existing; cherry-picked)
- research/ideation/2026-06-01-extras.md (existing; cherry-picked)
- research/hypothesis_bank.md (modified: header updated + 115 new
  entries inserted before Meta-research section)
- research/experiments/2026-06-01-mr-009-deep-ideation.md (this file)
