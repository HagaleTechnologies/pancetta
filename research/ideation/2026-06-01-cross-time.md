---
slug: ideation-cross-time
mode: ft8
state: ideation
category: cross-time
created: 2026-06-01T00:00:00Z
branch: iter/2026-06-01-ideation-cross-time
parent_hypothesis: meta — temporal-scope bank-refill ideation
generated_count: 14
wild_card_count: 4
disposition: ideation only — aggregator will triage into hypothesis_bank.md
related_closed: hb-027 (joint-multi-slot via QSO context — shelved via hb-051), hb-085 (cross-cycle on residual — structurally shelved), hb-074 (complex-spec coherent cross-cycle — shelved)
related_scoped_or_active: hb-048 a7 (adjacent-slot template correlation, scoped), hb-057 (per-callsign DT history, scoped), hb-089 (multi-cycle coherent residual accumulation, active), hb-091 (a8 early-decode latency, active), hb-100 (synthetic interferer-pair corpus, active)
---

## Frame

Pancetta's decoder is a slot-local function today: `decode_window(samples) -> Vec<Decoded>`.
Recently-graduated mechanisms have started to break that purity in narrow,
defensible ways:

- **hb-079/080 multipass** is *intra*-slot iteration (coherent subtract within a slot).
- **hb-086 V1 joint-pair-retry** is *intra*-slot (retry on the same residual).
- **hb-052/062 FP filter** carries a *rolling cross-slot* callsign window — but only
  to gate plausibility on the way out, not to inform decode in.
- **hb-048 a7** (scoped, not built) is the only structurally cross-slot decode-input.
- **hb-057** (scoped) is the only per-callsign across-slot prior.
- **`recently_responded_to`** (autonomous, 60 s) is the only TX-side cross-slot memory.

Almost every other temporal scope — **within-QSO**, **within-session**,
**across-session**, **per-band-time-of-day**, **per-operator-profile**,
**propagation-regime**, **sun-cycle**, **periodic** — is *unused*. That's a
large, mostly untouched search space.

This document enumerates 14 cross-time mechanisms that challenge the
slot-local assumption. Each carries a defensible analog (WSJT-X, JT65, Q65,
FT4, JS8, JTDX, JTAlert, WSPR, VHF EME logs, contest software, or a
wild-card stretch). Aggregator decides which earn hypothesis IDs.

## T1 — Within-QSO context graph (decode-time)

**Mechanism.** A QSO is a 4–7-slot structured exchange (`CQ → GRID → 73`,
`CQ → R-RPT → RR73`). Once pancetta has decoded the FIRST exchange of a QSO
(say slot N: `K1ABC W1XYZ EM10`), the structure constrains slot N+1, N+2,
N+3 message space hugely: W1XYZ's *response* will almost certainly be one of
8 specific message shapes (`K1ABC W1XYZ R-12`, `... RR73`, `... 73`,
`... -05`, ...). A `WithinQsoContext` table — keyed by `(callsign_pair,
frequency_hz±15Hz)`, evicted on `73`/`RR73` or 6-slot timeout — feeds
*pair-conditional* AP templates into the next slot. Different from hb-048
a7: a7 templates are rooted at one callsign; this is the FULL EXPECTED PAIR
of an in-flight QSO, with the structural state of where in the exchange
they are.

State flows in BOTH directions: a confirmed `RR73` on slot N validates
the slot N-1 `R-rpt` decode retroactively (boosts confidence), and could
even *revisit* a slot-N-1 LDPC-fail candidate at the expected message
template (this is a "decode in reverse" — bold).

**Defensible prior.** JTAlert and N1MM both track in-flight QSOs for
LOGGING; no FT8 decoder uses QSO state for *decode improvement*. JT65 had
"Auto-Sequence" but no AP feedback. WSJT-X's a7 is the closest analog and
deliberately stays single-callsign.

**Assumption challenged.** Each slot is decoded independently of which
QSOs are mid-flight on the band right now.

**Kill-switch sketch.** On hard-1000 (which is shuffled), this is largely
untestable for cross-slot — but the WITHIN-WAV variant works: extract all
multi-slot WAVs in `research/corpus/` and measure how many missed truths
are second-or-third turns of a QSO whose first turn pancetta decoded. Bar:
≥8% of misses are downstream-QSO-turns where the upstream turn DID decode.
Measurable today with the existing corpus and the existing QSO-state-machine
in pancetta-qso.

**Effort estimate.** Spec-sized 2 sessions; implementation 1–2 sessions
(table + dual-callsign template generator + decoder hook + eval). New state
plumbing nontrivial because the eval harness shuffles WAVs (same trap as
a7 §Risk 2).

**Headline risk.** State staleness: a stale entry from a 30-min-old QSO
that resumed on a different band would inject wrong templates. Mitigation:
short eviction window (90 s), band-keyed table.

## T2 — Within-session DT-and-frequency drift model per active station

**Mechanism.** Per-callsign, build a *running linear model* across the
current session of `(slot_timestamp → dt_offset)` and `(slot_timestamp →
audio_freq)`. The first is sender clock drift relative to UTC; the second
is the operator's TX-freq stepping behavior (some operators move 50 Hz
between QSOs to avoid pile-ups, some sit still). After 3+ sightings of a
callsign in a session, sync for *that callsign* uses the *predicted* DT and
predicted audio-freq, not the global ±2.0 s × ±50 Hz search. This is the
**continuous-time extension of hb-057** (which is median + IQR, not a
drift model).

The drift model also feeds a *propagation phase coherence* prior:
constant DT + slow freq drift = stable propagation = expect more decodes
from this station; rapidly changing DT/freq = QSB / multipath fading =
boost LDPC iterations + drop the gate.

**Defensible prior.** JTDX maintains per-callsign DT smoothing (the
inspiration for hb-057). WSPR plots `(dt, snr, dfreq)` vs time per
callsign-pair on `wsprnet.org`. No FT8 decoder uses the drift model for
sync prediction; that's the wedge.

**Assumption challenged.** Sync windows are static per-config (`±2.0 s`),
not per-callsign-state.

**Kill-switch sketch.** Extend the hb-057 diagnostic
(`hb057_dt_history_potential.rs`): after fitting a *linear* DT model per
callsign on top-20 hard-200, what fraction of missed truths fall within
the ±0.2 s predicted window vs the median window? If linear adds <3% over
median (i.e., the drift is not linear or the corpus has too few sightings
per callsign), this dies as "scope-creep on hb-057". Run today.

**Effort estimate.** 1 session if it grafts onto hb-057's storage; 2
sessions if standalone.

**Headline risk.** Overfit on 3 sightings — a 3-point linear fit is
nearly-singular if two sightings are seconds apart.

## T3 — Cross-session ADIF-driven AP pool (multi-day depth)

**Mechanism.** The operator's ADIF (`~/.pancetta/qsos.adi`) is a
historical record of every callsign worked, going back months/years. The
AP pool today (`recent_calls` for AP2) is session-local. Extend AP2's
caller-injection pool to include the **last K=200 distinct callsigns from
the operator's ADIF**, weighted by recency (exponential decay with
half-life = 7 days) and band-match (same-band callsigns weighted 4×).

Different from hb-052's CallsignContinuityFilter (which is plausibility
on the way OUT): this is *AP injection on the way IN*. The filter rejects
implausible decodes; this *creates* decodes that LDPC otherwise couldn't
converge.

**Defensible prior.** WSJT-X-Improved's "WANTED" list (operator-curated)
is conceptually similar. JTAlert maintains a "previously worked" highlight
that feeds nothing decode-side. cqdx.io has per-operator history that
could feed this. No prior FT8 decoder mines the operator's OWN log for
AP injection — that's the wedge.

**Assumption challenged.** The set of plausible decodable callsigns is
defined by the *current* RF environment, not the operator's history.

**Kill-switch sketch.** Take the operator's ADIF (or a 6-month synthetic
proxy from cqdx.io's spot history for K5ARH's grid), enumerate top-K=200
callsigns by recency-weighted score, and measure the AP-recovery ceiling
*restricted to that pool* on hard-200. Bar: ≥3 callsigns / 200 hard-200
WAVs have their truth callsign in the operator's ADIF AND fail decode
without AP. Measurable today.

**Effort estimate.** 2 sessions (ADIF reader is already in
`pancetta-qso::callsign_continuity`; plumbing into `par_try_ap_decode`'s
caller pool is the work).

**Headline risk.** AP-FP inflation. The hb-051 ceiling diagnostic showed
AP-blast has a hard recall ceiling; pool inflation could blow up novels
(this is exactly what hb-087's shelve teaches). Bound: cap AP pool
expansion to ≤2× current `recent_calls` size, threshold-sweep aggressively.

## T4 — Per-band-time-of-day propagation expectation prior

**Mechanism.** Build (offline, from ADIF + cqdx.io spot history) a 3D
table `P(callsign decodable | band, UTC hour, day_of_year)`. Use this as a
*prior* feeding AP candidate selection and the FP filter trust threshold:
at 23:00 UTC on 20m, station JA1XYZ has historical P=0.4 (high JA opening);
at 03:00 UTC on 20m, P=0.02 (band dead). High-P callsigns get AP injection
priority; low-P decodes face a higher FP-filter trust threshold (because
"JA1XYZ at 03Z on 20m" is a priori unlikely, so be more skeptical).

**Defensible prior.** Contest software (N1MM+, Win-Test) all use band-by-
hour propagation prediction (VOACAP) for run-rate optimization. PSKReporter
visualizes per-band openings. No DECODER uses propagation priors —
universally consumed only by humans / TX strategy.

**Assumption challenged.** The decoder ignores propagation physics; only
the autonomous operator considers it (and only for hunt prioritization).

**Kill-switch sketch.** Pull 30 days of cqdx.io / PSKReporter spots for
K5ARH's grid, partition by `(band, UTC_hour)`, compute the conditional
recovery rate for hard-200's truths bucketed by `(band, sender_continent)`.
Bar: ≥15% of missed truths come from `(band, hour, continent)` combos
where the conditional prior would have triggered AP-pool injection.
Measurable in ~1 day (requires cqdx.io history pull or PSKReporter scrape).

**Effort estimate.** 3 sessions (data pipeline + table + decoder hook).
This is the kind of mechanism where the data engineering exceeds the
algorithmic work.

**Headline risk.** Propagation priors are noisy and operator-grid-
specific; a model fit to K5ARH won't transfer. Mitigation: ship the prior
generator, not the prior table; recompute per-install on first run from
the operator's own ADIF + a cqdx.io history pull.

## T5 — Sunspot-cycle aware AP weighting (long-cycle)

**Mechanism.** Solar cycle 25 is descending. As SFI drops, 10m/12m close
during the day and 80m/40m open more reliably. Maintain a slowly-updated
table `solar_state → band_activity_weight` driving the *autonomous
operator's* hunt-mode band-hopping priority AND the decoder's AP-pool
band-restriction. As 10m dies through 2027, AP pool for 10m down-weights
older callsigns (they're not on this band anymore); 40m AP pool up-weights
historical 40m operators (they're back).

Pulls from `https://services.swpc.noaa.gov/json/solar_probabilities.json`
(NOAA solar data, free, no auth). 60 min refresh.

**Defensible prior.** All long-distance contest planning uses SFI
forecasts (HamCAP, VOACAP). Q65 is designed for HF-degraded conditions
specifically. wild_card: false — but the *decoder* link is novel.

**Assumption challenged.** Band-level AP pools are time-invariant.

**Kill-switch sketch.** Long-cycle (months) — cannot measure on existing
corpus. Proxy: backtest on cqdx.io's 18-month spot history, group spots
by `(band, SFI_band)`, measure whether per-band callsign distributions
shift detectably with SFI. Bar: KL-divergence between SFI-quartile
distributions ≥ 0.5 nats. Day-of-data work.

**Effort estimate.** 2 sessions to first ship; the *evaluation* is the
long pole — needs months of operational data to verify.

**Headline risk.** Solar cycle changes glacially; the mechanism's effect
is small per-session, large per-year. Hard to evaluate offline. May only
show value at 6-month review.

## T6 — Contest weekend / periodic event detection

**Mechanism.** Time-aware activity prior: ARRL contest calendar
(SS, RTTY-RU, NAQP-CW, etc.) plus regional events (POTA Activator
Week, FIELD DAY) drive AP-pool composition. During CQ-WW-RTTY, 20m
fills with contest-format messages (`599 005`); during ordinary days,
those decode as gibberish. Pre-load AP pool with contest-format templates
during contest windows; relax certain FP-filter rules (e.g., allow
`/M`/`/P` portable suffixes); during POTA weeks, boost POTA-spotter
callsign AP weights.

Calendar source: a YAML file shipped with pancetta (manually maintained,
~30 events/year). Updated quarterly.

**Defensible prior.** N1MM has contest-mode dropdowns; JTAlert highlights
contest-format messages. No FT8 decoder shifts decode strategy by
calendar. Field-Day FP filter (hb-058) was a graduated *negative* version
of this — pancetta rejects FD format. T6 makes it *bidirectional*: reject
in non-FD windows, ACCEPT (and AP-boost) during FD windows.

**Assumption challenged.** The decoder is calendar-blind.

**Kill-switch sketch.** Pull cqdx.io spot density by `(weekend, band)`
over 6 months; identify the 8–10 spike events (top 5% of weekend×band
density). Verify that decode RATES on those weekends are above-mean by
≥2σ. If contest weekends look like ordinary weekends in spot density,
the mechanism has no signal. ~3 hours of analysis.

**Effort estimate.** 2 sessions for calendar + decoder hooks + FP filter
mode-switch wiring; ongoing maintenance is the calendar file.

**Headline risk.** Stale calendar = wrong mode = silent regressions.
Mitigation: visible status indicator ("CONTEST MODE: ARRL DX SSB"), strict
date matching, ship calendar updates with each pancetta release.

## T7 — Per-operator (sender) personality fingerprint

**Mechanism.** Build per-callsign behavioral profiles from cross-session
observation: typical TX message rate (calls/min), typical DT, typical audio
freq, typical SNR distribution, typical CQ-vs-respond ratio, typical "73
when done" vs "abrupt cutoff" pattern, typical "answer immediately" vs
"answer after delay" timing. After K=20 observed slots from a callsign,
the profile sharpens. Use it to:

1. **Bias sync.** Operator W1XYZ TXs always at DT = 0.45 s → sync this
   callsign there first.
2. **Bias message-type prior.** Operator K1ABC almost always sends grid
   in their reply (no SNR) → upweight grid-shaped AP templates.
3. **Detect anomaly.** Operator JA1XYZ usually responds within 2 slots;
   if they go silent for 6 slots mid-QSO, escalate AP-template generation
   on the assumption fading is the issue, not abandonment.

**Defensible prior.** wild_card: TRUE for the *full* fingerprint
mechanism (operator personality modeling is not done by any digital-mode
decoder). The per-callsign DT slice (T2) is supported by JTDX prior art.
The full personality model is bold.

**Assumption challenged.** All operators behave identically; differences
are noise.

**Kill-switch sketch.** Diagnostic on cqdx.io's spot history: cluster
top-100 most-spotted callsigns by `(median DT, DT IQR, audio_freq
stability, message-shape distribution)`. Bar: at least 3 distinct
clusters visible (i.e., operators DO differ measurably). Statistical
test: silhouette score ≥ 0.3 on K=3 clustering. ~1 day of analysis.

**Effort estimate.** 3–4 sessions (it's a small ML model with cross-
session storage). Spec-sized for sure.

**Headline risk.** Operator changes hardware / location / strategy →
profile is wrong. Privacy implications (we're profiling other hams).
Mitigation: opt-out, treat profiles as local-only (no upload to
cqdx.io).

## T8 — Propagation-regime classifier (TEP / Es / Aurora)

**Mechanism.** Real-time propagation regime detection from the
decoder's own output: TEP (trans-equatorial) opens at sunset locally,
characterized by `(my_grid_latitude × DX_grid_latitude < 0)` decodes
clustering in time. Sporadic-E (Es) on 6m / 10m is short-duration
intense openings characterized by sudden in-region decode bursts.
Auroral is characterized by *warbly* tones (high frequency variance
on Costas symbols).

Classify the current 15-min window into `{normal, TEP, Es,
aurora, geomagnetic-storm}`. Each regime tunes the decoder:

- **TEP**: bias AP pool to other-hemisphere callsigns.
- **Es**: relax FP filter for in-region calls (the band is open to
  nearby ops that aren't in the operator's ADIF yet).
- **Aurora**: increase LDPC iterations, widen Costas tolerance on
  frequency variance, accept "doppler-smeared" sync candidates that
  the regular gate rejects.

**Defensible prior.** All ionosphere-aware HF software (DX Atlas, DX
Heat, PSK Reporter band condition view) does regime classification.
WSJT-X has no regime-aware decoder behavior. Q65's design *is* a
fixed regime adaptation (chosen by the operator's mode switch); T8 is
*automatic* regime adaptation.

**Assumption challenged.** The decoder uses the same parameters
regardless of ionospheric conditions.

**Kill-switch sketch.** From operator's ADIF history, extract WAVs
captured during known TEP / Es / quiet periods (NOAA Kp index ≥ 5 for
aurora; SDO X-ray flux for flares). Measure recall delta per regime.
If quiet vs Aurora recall difference is <5%, regime adaptation has
little room. ~2 days of capture + analysis. Long cycle to fully
validate (needs aurora events to occur during testing).

**Effort estimate.** 4 sessions (classifier + 5 mode parameters +
eval per regime).

**Headline risk.** Regime classifier mistakes (TEP misclassified as
Es) → wrong tuning → silent regression. Mitigation: classifier outputs
calibrated probabilities, decoder blends parameter sets weighted by
regime probability.

## T9 — Cross-session band-noise floor learning

**Mechanism.** The operator's antenna + RX-chain noise floor varies
hourly (electrical noise from neighbors, sunset/sunrise terrestrial
noise, geomagnetic events). Track the noise floor per
`(band, UTC_hour_of_week)` from RX'd waterfall data, smoothed over
weeks. When the current noise floor is significantly above the
historical median, the decoder *expects* fewer / weaker decodes and:

- Drops `min_sync_score` (admit weaker candidates since strong ones
  won't exist).
- Raises FP-filter trust threshold (more candidates → more risk).
- Reports "noise above median, decode rate may be reduced" to the
  TUI.

Conversely, on unusually quiet nights, tighten the sync gate (no need
to fish in the weeds; the easy decodes are there).

**Defensible prior.** WSPR's noise-floor reporting and CW Skimmer's
adaptive thresholds. JTDX has a "DX call only" mode the operator
toggles based on band conditions. No FT8 decoder *auto*-tunes its
gate to historical noise floor.

**Assumption challenged.** Decoder gates are configured statically;
noise environment is not a parameter.

**Kill-switch sketch.** Capture 7 days of waterfall noise floor at
the operator's QTH per band per hour. Variance must exceed ≥ 3 dB
between best and worst hours to justify the mechanism (otherwise
fixed gates are fine). 1 week of capture, 1 day of analysis.

**Effort estimate.** 2 sessions if waterfall capture is already
plumbed; 3–4 if not (the noise-floor probe is `pancetta-dsp` work).

**Headline risk.** Auto-tuned gates can oscillate (low noise →
tighten → miss → low decode count → loosen → false decodes → tighten
again). Mitigation: hysteresis on regime transitions, weekly
recompute not per-slot.

## T10 — Time-aware decode-confidence calibration

**Mechanism.** Calibrate `confidence(decode) → P(decode is correct)`
*as a function of `(elapsed time since decode, decoder pass that
produced it)`*. A decode that emerges from pass-3 multipass and gets
*confirmed* by a follow-up RR73 6 slots later is hindsight-proven
correct; track that base rate. Build a cross-session calibration
curve: `decoder_internal_score → P(eventually confirmed by downstream
QSO turn or matching PSK Reporter spot ≤ 30s later)`.

Feed *backwards*: when a low-confidence decode at T=0 is confirmed at
T+30s by another evidence source, retroactively boost the *current*
threshold for decodes with similar features. This is a
*conditioning-on-the-future* loop.

The mechanism doubles as a real-time FP-filter precision audit: on
days where calibration breaks (confirmations don't happen at the
expected rate), something is wrong with the decoder OR the trust
sources are degraded.

**Defensible prior.** wild_card: TRUE. No digital-mode decoder does
this. Closest analog is reCAPTCHA's calibration loop, which uses
delayed-confirmation signals to retrain a scorer.

**Assumption challenged.** Decoder confidence scores are well-
calibrated at decode time and don't need backward correction.

**Kill-switch sketch.** On operator's session history, measure
fraction of pass-3 multipass decodes that get downstream confirmation
(QSO turn / PSK Reporter spot / cqdx.io spot) within 60 s. Bar:
≥10% of pass-3 decodes get external confirmation (enough signal to
calibrate). Measurable in ~2 days of operator data.

**Effort estimate.** 3 sessions (calibration table + retroactive
loop + eval). Heavy state plumbing — this is a feedback control
problem.

**Headline risk.** Feedback loop instability. If calibration boosts
threshold based on confirmation, and confirmation drops because of
unrelated propagation, threshold drifts wrongly. Strict damping
required.

## T11 — Wild-card: cross-operator (pancetta-network) priors

**Mechanism.** Multiple pancetta instances (each operator's install,
across the user base) share **anonymized** "just-decoded" callsign +
band + UTC timestamp tuples to cqdx.io. cqdx.io aggregates and pushes
a stream: "at 03:42 UTC on 20m, 14 pancetta instances heard JA1XYZ".
The local decoder uses this as an instantaneous-AP-pool injector: if
14 other pancettas just heard JA1XYZ, my next slot AP-pool gets
JA1XYZ heavily weighted, even if I haven't heard them myself.

This is **federated** propagation awareness — the network learns the
band state in real time and feeds individual decoders.

**Defensible prior.** PSK Reporter and Reverse Beacon Network do
something analogous for spotting; they don't feed back into decoders.
JT-Alert pulls cluster data for highlighting only. The
network-feeding-decoder loop is wild_card.

**Assumption challenged.** Each pancetta is decoding in isolation;
other operators' decodes are useless to mine.

**Kill-switch sketch.** Simulate: take 1 day of cqdx.io's spot
firehose, restrict to spots within ±60 s of a given operator's RX
slots, measure how many of THAT operator's missed truths appear in
the firehose at the same time. Bar: ≥10% of missed truths on hard-
1000 are spotted by *someone* on the network within ±60 s. If others
also miss them, the network signal is useless. Day of analysis on
cqdx.io history.

**Effort estimate.** 5+ sessions; spec-sized (federated infra). cqdx.io
endpoint, opt-in flow, anonymization audit, decoder hook,
rate-limit/abuse protection. Substantial.

**Headline risk.** Privacy / federation poisoning. A malicious
pancetta instance could spam fake spots to bias others' AP pools.
Mitigation: cqdx.io reputation system, signed reports, slow-trust
build-up per source.

## T12 — Wild-card: time-reversed multipass (decode latest first, propagate backwards)

**Mechanism.** Multipass today goes forward: pass 1 → subtract →
pass 2 → subtract → pass 3. But the strongest decodes typically
emerge in pass 1 (clean signals), and the weakest emerge in passes
2–3 (mutually-masked). What if we ran an extra **time-reversed**
multipass: take the residual from pass 3, run sync_search again,
collect remaining candidate positions, but score them using LLRs
built from the **time-reversed complex baseband** (mathematically:
swap I/Q convention then re-FFT)?

Time-reversal of an FT8 signal is itself a *valid* FT8 signal up to
a known transformation (since 8-GFSK is symmetric under time
reversal of the tone sequence). A position that LDPC-fails in
forward direction might LDPC-succeed in reverse if the interference
pattern is asymmetric (e.g., a click at the start vs an HF burst at
the end). Then accept decodes that succeed in *either* direction.

**Defensible prior.** wild_card: TRUE. Forward-backward smoothing is
standard in HMMs (Baum-Welch), but applied to symbol-level decoding
of a structured waveform is novel for FT8. The math is sound for
the symmetric tone sequence; structural impact unknown.

**Assumption challenged.** Decoder direction is irrelevant; only the
forward direction needs evaluation.

**Kill-switch sketch.** Synthesize 10 WAVs with asymmetric
interference (lightning click at t=0, RFI burst at t=14s). Run a
forward decoder + a reversed-baseband decoder, count distinct
decodes. Bar: reversed decoder recovers ≥2 distinct decodes the
forward decoder misses. If 0, the asymmetry hypothesis is wrong.
~½ day to synthesize + test. Same-day test.

**Effort estimate.** 1–2 sessions (the bulk is `time_reverse_complex`
+ a parallel multipass entry point; could share most of `decoder.rs`).

**Headline risk.** Time-reversal symmetry assumption may be subtly
wrong for 8-GFSK in the presence of frequency drift (Doppler). Easy
to falsify on synthetic.

## T13 — Wild-card: meta-decode QSO-state inference (decode the OPERATOR, not the slot)

**Mechanism.** Treat the operator-and-station combination as a
**hidden Markov model**: state = `{listening, calling-CQ, in-QSO-
turn-1, ..., logging}`, observation = `decoded messages`. With a
trained transition model (from operator's ADIF history), the
decoder maintains a **probability distribution over the operator's
own state**. This distribution informs AP heavily:

- If P(in-QSO-turn-3-with-W1XYZ) = 0.7, AP pool is sharply
  conditioned on W1XYZ's expected next message.
- If P(listening) = 0.9 (no recent decode-near-our-call), AP pool
  is uniformly CQ-shaped from random callsigns.
- Sudden P-mass shift (e.g., turn-3 → turn-4 transition) updates AP
  templates mid-slot if early decodes match.

**Defensible prior.** wild_card: TRUE. HMMs are textbook ML, but
applied to FT8 operator-state-as-decoder-conditioning is novel.

**Assumption challenged.** The operator's intent is irrelevant to
decode; the decoder is a pure signal-processing function.

**Kill-switch sketch.** From operator's ADIF, extract complete
QSOs, label each WAV/slot with the OPERATOR's HMM state, then
measure mutual information between state and which AP template
would have helped. Bar: I(state; helpful_AP_template) ≥ 0.3 bits.
1–2 days of analysis.

**Effort estimate.** 4+ sessions (HMM training + integration +
eval). Spec-sized.

**Headline risk.** HMM state-space explosion (many possible QSO
states × many active QSOs), inference cost in real-time.

## T14 — Periodic / diurnal CQ pile-up prior

**Mechanism.** Some operators run *scheduled* activations: K1WX
fires up 20m every weekday at 22:00 UTC ±10 min; Roman activates
SOTA summits Saturday mornings ±3 hours. Mine cqdx.io spot history
to identify per-callsign **periodicity signatures**: build a
spectral analysis (literally: FFT over the inter-spot gap times,
per callsign). Strong daily / weekly periodicity → high prior at
the predicted next window.

Use as: at T-15min relative to predicted activation, *pre-load* AP
pool with that callsign + relevant grid + likely message types.
During the predicted ±30min window, AP-pool weight is 5×;
outside, default.

**Defensible prior.** WSPRnet shows beacon-like periodic signals
clearly. POTA activation alerts are explicitly scheduled. SOTA-
watch broadcasts scheduled activations. wild_card: false on the
data (cqdx.io has this); false on the use case (POTA spotter does
similar); novel on the *decoder-AP-pool* feedback path.

**Assumption challenged.** All callsigns are equally likely at all
times; no per-callsign temporal pattern is mined.

**Kill-switch sketch.** Per top-100 most-spotted callsigns in
cqdx.io, FFT their inter-spot gaps. Bar: at least 5 callsigns
show a periodicity spike with power ≥ 3× background (i.e., real
periodic behavior). 1 day of analysis on cqdx.io history.

**Effort estimate.** 2–3 sessions (periodicity miner + per-callsign
schedule table + AP-pool hook).

**Headline risk.** Self-reinforcing prediction: if pancetta
*always* expects K1WX at 22:00 UTC, it might falsely decode K1WX
on noise during that window. Mitigation: prior boosts AP weight but
does NOT lower LDPC threshold; the decoded codeword must still
pass CRC + plausibility independently. Prior is "where to look",
not "what to accept".

## Cross-cutting observations

**State plumbing is the binding cost.** T1, T7, T10, T11, T13 all
require persistent cross-session state that didn't exist before
hb-057 was scoped. Once hb-057's `dt_history` storage lands, much of
the per-callsign cross-time machinery can share its lifetime.
Recommend: aggregator considers a *shared cross-time-state crate*
(or coordinator-level `Arc<RwLock<CrossTimeState>>`) before approving
the second of these — avoid 5 parallel per-callsign tables.

**Eval harness is the binding measurement gap.** The harness shuffles
WAVs (a7's Risk 2). Most cross-slot mechanisms can only measure their
*within-WAV* effect in offline eval; their *cross-slot* effect needs
production telemetry. Recommend: aggregator considers a "chronological
eval" tier — the harness can replay a real session's WAVs in true
slot order, on a hold-out captured by the operator's existing
audio-capture path. This is *infrastructure*, not a hypothesis;
several T-entries depend on it.

**Shortest-cycle idea (testable today):** T12 (time-reversed
multipass) — synthesize asymmetric WAVs, run a reversed-baseband
multipass, count decodes. ~½ day.

**Longest-cycle idea (months of capture):** T5 (sunspot-cycle aware
AP weighting) — needs 6+ months of operator decode telemetry across
declining solar cycle to demonstrate value. Ship-without-validation
is the only path; consider as a "deferred wager".

**Top 3 by potential disruption:**
1. **T1 (within-QSO context graph)** — large addressable surface
   (every QSO turn after the first is template-constrained), composes
   with hb-048 a7 cleanly, defensible measurement path today.
2. **T13 (operator-state HMM)** — restructures the decoder around a
   probabilistic model of the operator's mental state; if it works, every
   AP mechanism downstream gets sharper conditioning for free.
3. **T11 (federated cross-operator priors)** — turns pancetta from a
   solo decoder into a network member; the value compounds with
   adoption.

**Distinct from closed hypotheses.** None of T1–T14 overlap with
hb-027 (joint-multi-slot-via-QSO — that was *joint slot decode*, this
is *template injection conditioned on QSO state*), hb-085 (cross-cycle
on residual — that was averaging the residual; here we use cross-
*session* state to *inform* decode), hb-074 (raw spectrogram coherent
averaging — wrong axis). Distinct from active/scoped:

- T2 extends hb-057 (linear drift vs. median) — flag as
  "scope-creep candidate" not standalone.
- T1 is orthogonal to hb-048 a7 (a7 = single-callsign template
  correlation; T1 = full-pair QSO-state-conditioned templates).
- T11 is orthogonal to hb-089 (multi-cycle residual accumulation = own-
  cycle data; T11 = others' data).

## Hand-off to aggregator

Aggregator should:

1. Triage T1, T7, T11, T13 into the hypothesis bank as potential
   hb-101 .. hb-104 (priorities 0.40, 0.35, 0.25, 0.30 — wild cards
   lower).
2. Note T2 as hb-057 follow-up rather than standalone.
3. Flag the **chronological eval tier** as an infrastructure
   prerequisite for T1, T8, T10.
4. Consider the **shared CrossTimeState** plumbing recommendation
   before approving the second per-callsign state mechanism.
