---
date: 2026-06-01
category: human-in-the-loop
assumption_challenged: "Pancetta is best built as a fully autonomous FT8 station; operator involvement during a session is a regression."
counter_hypothesis: "K5ARH + Pancetta as a *cooperative pair* — with feedback channels operator → decoder and operator → decision engine — produces both higher decode rates and higher-quality QSO targeting than autonomy alone, *with no loss of unattended capability when the operator is absent.*"
operator: K5ARH
hardware_target: FTdx10 (MiniPC) + Mac dev
session_budget_min: 45
count_target: 12-15
distinct_from: |
  - hb-087 (callsign priors) — bank-driven, not operator-real-time
  - hb-016/068 (decoder threshold tuning) — pure algorithm
  - hb-062 (continuity filter) — already uses operator ADIF passively
  - autonomous.rs hunt/CQ/hybrid modes — no operator feedback at runtime
related_files:
  - pancetta-qso/src/autonomous.rs
  - pancetta-qso/src/priority.rs
  - pancetta-tui/src/app.rs
  - pancetta/src/coordinator/pipeline.rs
notes: |
  The autonomous architecture is good and should remain the default. These
  ideas all assume "operator-absent mode = current behavior" — the operator
  HITL paths are *additive*, gated, and silently no-op when the operator
  isn't keying. Several deliberately exist as cheap exhaust-stroke wins for
  Phase 5 dry-run (operator is sitting in front of the radio anyway).
---

## H1 — One-key "confirm decode" relaxes thresholds for current callsign

### Mechanism
TUI surfaces low-confidence decodes (LDPC-iters > some threshold, OSD-rank > 2,
near-CRC-fail) with a dim style and a single-letter prompt. Operator
presses `y` to confirm the callsign is real (they recognize it, heard it on
PSKReporter, etc.) or `n` to discard. On `y`, pancetta:

1. Adds the callsign to a session-scoped "operator-confirmed" set
2. Lowers the LDPC-iter / OSD-rank acceptance bar for *that callsign only*
   for the next N decode windows (e.g., 20)
3. Promotes future decodes of that callsign past continuity-filter rejection

Pancetta runs the full decoder unchanged in operator-absent mode; the
confirmed-set defaults empty and the threshold drop only fires on a hit.

### Why operator-in-the-loop helps here
The decoder's hard-200 corpus is exactly the regime where callsign priors
(hb-087) showed 23.6% of missed truths *are* in some operator-derived set
already. Real-time confirmation is a strictly stronger prior than any
offline bank: it incorporates the operator's PSKReporter glance, their
ear, the cluster they're watching. Low friction (one key) and immediate
payoff (next slot's decode is more likely to land).

### Defensible prior
Active-learning classifiers (Tong & Koller 2001; modern label-efficient
fine-tuning) regularly outperform fully-supervised models with ~10x less
label budget by querying the most-uncertain example. Same logic applies
to a decoder's decision boundary.

### Assumption challenged
"Decoder thresholds must be a global, offline-tuned constant."

### Kill-switch
Engagement floor: if operator confirms <3 decodes/session over 5 sessions,
the prompt is too aggressive (or operator is absent). Auto-suppress prompt
and revert to global threshold. Friction kill: if `y` requires >2 keystrokes
or pulls focus from band-activity panel, kill.

### Effort
1.5-2 sessions. TUI prompt + per-callsign threshold override map +
threading through decoder. Most-risky bit: per-callsign threshold in the
hot decode loop (need to keep lookup O(1)).

### Headline risk
Operator fatigue. Prompt-saturation in a busy band could blow past
working-memory budget; need an "auto-yes for callsigns already in seed file
or recent ADIF" pre-filter to keep volume to <5/min.

---

## H2 — Pointing finger: operator clicks/hovers a waterfall slice → decoder focuses

### Mechanism
TUI waterfall (planned; not yet built but on the roadmap) becomes
clickable. Operator clicks a faint trace they suspect is a signal but
pancetta isn't decoding. Pancetta:

1. Re-runs decode on the next 15s slot with a *narrow* candidate-search
   window (±25 Hz around clicked frequency)
2. Drops sync-score thresholds for that window only
3. Allows OSD rank 4 (vs default 2) within the window

Operator's spatial attention (a thing humans are still excellent at vs
silicon) feeds a search-space prior; pancetta amortizes the expensive
deep decode only where it's been pointed.

### Why operator-in-the-loop helps here
Decoder spends compute uniformly across 200-3000 Hz. Operator can identify
"there's a candidate at ~1450 that's been pulsing for 3 cycles" — visual
pattern recognition pancetta doesn't currently have. Narrow window + relaxed
thresholds = cheap deep-dive.

### Defensible prior
Speech-recognition systems that allow user re-record / point-to-correct
(Whisper.cpp's selection-aware refinement, Google Docs voice-typing
re-listen). Visual saliency as a prior is well-established in image
search.

### Assumption challenged
"Decoder search should be uniform across the band."

### Kill-switch
Requires the waterfall click target first — depends on TUI roadmap. If
clicks/session stays at 0 for first 5 real sessions, kill.

### Effort
3-4 sessions. Depends on click-able waterfall existing. Decoder side is
already capable of narrow-window decode; the wiring is the cost.

### Headline risk
Operator clicks noise; pancetta wastes deep-decode budget on garbage. Need
a fast pre-filter (sync-score min) inside the narrowed window so a noise
click doesn't synthesize a fake.

---

## H3 — `STAR` key: bump priority of next-cycle CQ from this callsign

### Mechanism
Operator browsing band-activity list highlights a CQ and presses `*`.
Pancetta:

1. Adds callsign to session-scoped `manual_priority_boost` map with score
   +0.50
2. Persists boost across slots until QSO completes or operator unmarks
3. If callsign goes silent for 5 slots, boost decays to 0

When the boosted callsign re-CQs, `PriorityScorer` adds the boost,
typically guaranteeing response selection.

### Why operator-in-the-loop helps here
Operator may know "P5/W0PR is on, that's North Korea, I want it" before
cqdx.io rarity score catches up. Or operator wants to chase a friend.
Single-key telegraphs intent without needing a config edit or hunt-list
rebuild.

### Defensible prior
Trading desks (price-alert + click-to-route), DAW favorites (Ableton's
hot-cue model), browser bookmarks. "I want THIS one, now" is a universal
UX pattern.

### Assumption challenged
"Priority weights are static config; intra-session retargeting is too
much state."

### Kill-switch
If operator presses `*` <1x per session over 5 sessions, the feature is
unused — kill. If pressed >20x/session, becomes a hunt-list workaround;
prompt operator to add to seed file.

### Effort
0.5-1 session. Trivial map + score-add in priority.rs. TUI key handler
addition.

### Headline risk
Boosted call already-worked or duplicate; need to keep duplicate-penalty
intact so `*` doesn't override "you already did this QSO."

---

## H4 — `STOP` mid-QSO when operator sees pancetta is wrong

### Mechanism
Big red key: `Q`. Pressed during an in-flight QSO, pancetta:

1. Immediately stops further TX for *this* QSO
2. Logs current state to `~/.pancetta/qsos-aborted.jsonl` with
   diagnostic snapshot (last 3 RX msgs, decoder confidence, audio RMS)
3. Marks the contact as "operator-aborted" (NOT a failure for recent-failure
   penalty; flagged distinctly to retraining set)
4. Releases the frequency

### Why operator-in-the-loop helps here
Phase 5 specifically: first real on-air outings will have edge cases
pancetta hasn't seen — collision, double-call, wrong-direction RR73,
contest-FP slipping past CRC. Operator is the supervisor; they need an
e-stop. Also: every Q-press is gold-standard training data ("here's a
QSO an autonomous system shouldn't have continued").

### Defensible prior
Autonomous-vehicle safety driver disengagements. Industrial robot
e-stops. All certified-safe autonomy has a kill-switch by regulation;
ham radio should too (and FCC arguably implies it via control-operator
rules).

### Assumption challenged
"Autonomous means hands-off, period."

### Kill-switch
N/A — this *is* a kill-switch. Always wanted.

### Effort
1 session. Wire a TUI key → coordinator-bus shutdown-this-qso message.
QSO state machine already has terminal states.

### Headline risk
Operator panics and Q-stops valid QSOs; reduces logged QSO rate during
training period. Mitigate: aborted QSOs counted separately in the
"sessions until trust" metric.

---

## H5 — Post-session review: operator labels each decode "real / FP / unsure"

### Mechanism
After session end (or on-demand), pancetta writes a CSV/JSONL:

```
ts, freq, snr, decoded_msg, decoder_confidence, network_corroborated
```

Operator opens it (CSV in Numbers, or a future TUI review-pane), marks
each row `real / fp / unsure`. On next session start, pancetta:

1. Mines patterns from FPs (same callsign repeating, certain
   message-format malformations, audio-feature signatures)
2. Auto-suppresses callsigns that scored `fp` on >3 distinct sessions
3. Boosts continuity-filter weight for callsigns scored `real` repeatedly

### Why operator-in-the-loop helps here
The FP corpus is hand-labeled by the actual operator on the actual
station — not a generic offline test set. K5ARH knows what FPs look like
on his band slice better than any global heuristic. Tightens the
continuity filter with operator-personalized priors.

### Defensible prior
Email spam-filter design (Bayesian + user-flagged ham/spam). Every
modern classifier system with active user feedback. ML-ops in production.

### Assumption challenged
"FP detection should be derived from offline corpora only."

### Kill-switch
Operator engagement: if review-CSV opens < once/week over 4 weeks, kill
the feature (pancetta will be just-autonomous). Friction-reduction:
default to "real if it produced a logged QSO" pre-filling.

### Effort
2 sessions. CSV export trivial; the import-back + pattern-mining is the
work. Don't over-engineer — start with "auto-block callsigns marked FP 3x".

### Headline risk
Operator marks something `fp` that's actually a rare DX trying again;
auto-suppression eats real contacts. Mitigate: 3-strike requirement +
network-corroboration override (if cqdx.io spotted it, override the FP
flag).

---

## H6 — Voice-shortcut "answer that one" via hotkey-triggered TTS-to-STT or push-to-talk macro

### Mechanism
Operator hands-on-keys (FTdx10 mic in hand for ragchew or contesting on
SSB) wants to tell pancetta "answer the next CQ from W1ABC." Two paths:

1. **Bluetooth foot-pedal** wired to a key macro → boosts next-decode
   callsign in operator's verbal/typed selection
2. **STT** (Whisper.cpp local, on-Mac): operator says "Pancetta: answer
   W1 alpha bravo charlie" → STT → callsign extract → enqueue auto-respond

Mechanism is decoupled: input layer pumps a `ManualTarget(callsign)` event
into the bus, autonomous responder evaluates it as if operator clicked
`*` in TUI (H3).

### Why operator-in-the-loop helps here
At-the-rig ergonomics. Operator's hands may not be on the keyboard during
multi-mode operation; voice or pedal frees them. Especially valuable
during Phase 5 dry-run where operator is supervising AND likely
fidgeting with rig.

### Defensible prior
Pro-audio control surfaces (Stream Deck), accessibility software
(Dragon NaturallySpeaking), DJ-deck footswitches. Voice on radio:
contest loggers (N1MM voice-keyer, but in reverse direction).

### Assumption challenged
"Operator interaction with pancetta must go through TUI keyboard."

### Kill-switch
STT accuracy: if callsign extraction WER > 20% on phonetic alphabet, kill
the voice path. Pedal path much cheaper to validate — bind to space if
unused.

### Effort
Pedal: 1 session. STT: 3 sessions + Whisper.cpp tuning + phonetic-alphabet
post-processor. Wild_card.
wild_card: true

### Headline risk
STT mishears "kilo five alpha romeo hotel" as own callsign, pancetta
responds to itself. Need confidence threshold + double-tap confirmation.

---

## H7 — Operator-confidence-tier overlay on FP filter

### Mechanism
Operator maintains a small ranked list (in `~/.pancetta/operator-trust.toml`):

```toml
trust_tier_a = ["W1AW", "K3LR", "VE3KI"]  # known good operators
trust_tier_b = ["*/POTA"]                  # POTA-suffix generic trust
distrust = ["TT5GIBBERISH", "FAKECALL"]    # known FP signatures
```

Trusted callsigns get continuity-filter bypass + +0.10 priority bonus +
auto-confirm (H1 path). Distrusted callsigns get hard-rejected even on
strong CRC. Tier-b regex wildcards.

### Why operator-in-the-loop helps here
The operator's social knowledge of the ham community ("W1AW always
sends clean, never FPs") is not in any database. Encoding it lets the
decoder lean harder on signal evidence for trusted calls and lean harder
on rejection for known garbage.

### Defensible prior
Web-of-trust (PGP), SSH known_hosts, browser allow/blocklists. Reputation
systems are the default for distributed identity.

### Assumption challenged
"All callsigns deserve equal evidence weight."

### Kill-switch
If operator's `operator-trust.toml` has <5 entries after 3 months, fold
the feature back into the seed file (which already has prior list role).

### Effort
1 session. TOML loader + lookup in priority.rs + decoder continuity-filter
exception path.

### Headline risk
Distrust list grows stale; operator forgets and a real callsign sits on
it. Add `last_added_at` and prompt to review entries >6 months old.

---

## H8 — Real-time "alarm tier": pancetta TUI rings/highlights when target appears

### Mechanism
Operator defines targets in TOML or via `+W1AW`-style TUI command:

```toml
[alarms]
needed_dxcc = true       # any new DXCC entity → terminal-bell + flash
specific_calls = ["P5/", "VK0", "ZL9"]   # auto-flag exotic prefixes
distance_km_over = 12000
snr_above = -5            # signal is *really* strong
```

When any alarm condition matches, pancetta plays a terminal-bell, flashes
the band-activity row, and (optionally) sends a system notification on
Mac/Windows.

Operator now knows to look at the screen. Pancetta still autonomously
queues a response if hunt-mode is on; the alarm just surfaces context.

### Why operator-in-the-loop helps here
Best of both: autonomy continues to chase, BUT operator gets pulled in
for the exceptional case. Closes the "you missed P5 because you were
making coffee" failure mode of pure autonomy.

### Defensible prior
SkimTalk / RBN sound alerts. Logger4OM / N1MM contest-call alerts.
Trading-desk price alerts. Universally beloved feature.

### Assumption challenged
"Autonomous means you don't have to look."

### Kill-switch
If operator silences the alarm >5 times/session, threshold too low —
auto-tighten. If alarms fire <1x/week, threshold too tight —
auto-loosen.

### Effort
1-1.5 sessions. TUI bell + flash + Mac-osascript notification.
Conditions live in priority.rs alongside scoring.

### Headline risk
Alarm fatigue. Highly likely if thresholds wrong on day 1. Auto-tuning
mitigates; hard cap at 1 alarm / 30s baseline.

---

## H9 — Reverse-handover: pancetta yields the QSO to operator at uncertainty cliff

### Mechanism
During in-progress QSO, if pancetta's decoder confidence on DX's last
message drops below threshold (or it can't decode at all 2 slots in a row,
or QSO state machine sees ambiguous transition), pancetta:

1. Beeps + flashes "HANDOVER — your turn?" in TUI
2. Pauses auto-TX for next 2 slots
3. Pre-fills the next likely message in a TUI "send queue" widget for
   operator to confirm/edit before sending

Operator presses Enter to send pre-filled, Tab to edit, `Q` to abort.

### Why operator-in-the-loop helps here
QSO failures often happen at decode-cliff moments (QSB knocking SNR
down 6 dB). Pancetta can detect "I don't have evidence" but can't
generate evidence. Operator with headphones might pull out the call by
ear or remember the in-flight context.

### Defensible prior
Tesla Autopilot lane-change confirmation. Bash autocomplete (cycle
suggestions, Enter accepts). Composer "send draft" workflows.

### Assumption challenged
"QSO state machine is either fully auto or fully manual."

### Kill-switch
If operator never takes handovers (lets them auto-abort), kill the
feature — operator wants pure autonomy. If operator always overrides,
operator distrust of pancetta → revisit autonomy thresholds first.

### Effort
2-3 sessions. QSO state machine needs a "pending operator confirm"
state. TUI send-queue widget. Coordinator yields TX briefly.

### Headline risk
Operator sleeps through handover, QSO times out. Mitigate: default
to "auto-continue if no operator response in 8s" with telemetry to
distinguish auto-continue from operator-OK.

---

## H10 — Skill-transfer: log operator's manual QSOs, mine priority preferences

### Mechanism
For 30 days, every QSO operator makes manually (with N1MM, WSJT-X,
JTDX, or pancetta) is exported. Pancetta analyzes:

- DXCC distribution (which entities did operator chase?)
- Time-of-day patterns (operator works 0200Z mostly = LP path interest?)
- Band preferences (heavy 20m operator → boost 20m hunting)
- Specific-call frequencies (operator answered W1ABC 4x → trusted)

Computes a per-operator priority-weight delta and offers operator: "I
learned you prefer (a) needed_dxcc weight 0.45 (vs default 0.35),
(b) +0.20 boost for grids in EU, (c) avoid weekends on 40m. Apply?"

### Why operator-in-the-loop helps here
Defaults are inevitably wrong for any specific operator. Auto-learning
the operator's revealed preferences from their own log is strictly
better than asking them to tune 7 weights.

### Defensible prior
Spotify's Discover Weekly (mines listening to recommend), GitHub Copilot
personalization, ad-targeting (well, the *technique* — apply to ham
without dystopia).

### Assumption challenged
"Operator must hand-tune priority weights via config editing."

### Kill-switch
If 30-day ADIF has <50 QSOs, not enough signal — keep defaults.
If learned weights produce *worse* outcomes (lower priority-score-mean
on subsequent week), revert.

### Effort
3 sessions. ADIF analyzer + weight-fitter (small ridge regression) +
TUI/CLI confirmation flow. Validation harness needed.

### Headline risk
Overfit to operator's recent quirk (one bad week chasing CN2 makes
pancetta over-weight Africa forever). Use rolling 90-day window with
recency decay.

---

## H11 — Co-pilot mode: pancetta SUGGESTS, operator AUTHORIZES every TX

### Mechanism
Hard-mode HITL: autonomous is *off*, but pancetta queues *every* TX
candidate in TUI:

```
[CQ] W1ABC FN42 -12 dB  AT 1402Hz  →  send 'W1ABC K5ARH EM10'? (y/space=yes, n=skip)
```

Operator presses Space to send-as-suggested, edits-and-sends, or skips.
Every interaction is logged with operator action + outcome for later
retraining (H10).

### Why operator-in-the-loop helps here
First weeks of Phase 5: training-wheel mode. Operator builds trust by
watching pancetta's suggestions; pancetta learns the operator's
acceptance/rejection patterns. After N sessions of >95% acceptance,
pancetta proposes graduating to autonomous mode.

### Defensible prior
Copilot/Cursor autocomplete suggest-then-tab pattern. Autonomous-driving
testing protocols (always supervised initially). Aviation: autopilot
engages-on-command, not by default at takeoff.

### Assumption challenged
"Autonomous-from-day-one is the only valid mode."

### Kill-switch
This IS the kill-switch alternative to full autonomy; doesn't need its
own. If operator finds 1-key-per-TX too tedious, that signals confidence
in autonomy is high → suggest mode-switch.

### Effort
2 sessions. TUI prompt + bus-level "pending TX" gate. Coordinator
already has TX queue.

### Headline risk
Defeats the autonomous goal. Worth shipping as opt-in mode for new
users / new bands / first-time exotic-DX chases — not as default.

---

## H12 — "Suspicious decode" reverse-active-learning: pancetta asks operator about uncertain ones it would have ACCEPTED

### Mechanism
Mirror of H1. Pancetta tags decodes that *barely* passed CRC + continuity
filter (e.g., LDPC iters > 90% of max, OSD-rank = 3, callsign not in
seed/recent/cqdx) with a `[?]` marker in band-activity panel. Doesn't
suppress them — they're displayed and counted — but operator can press
`r` (reject) or `a` (accept) to label them.

Pancetta:

- Builds a corpus of `(decoder_features, operator_label)` pairs over
  weeks
- Trains a small classifier (logistic regression on decoder features) to
  predict operator's accept/reject label
- After 200+ labels, suggests "I can predict your accept/reject decision
  with 92% accuracy — apply auto-filter?"

### Why operator-in-the-loop helps here
Decoder confidence scores are not calibrated to operator-perceived
quality. This learns the calibration mapping with cheap labels — and
the cost is bounded because pancetta only asks about marginal cases
(maybe 5/session, not 50).

### Defensible prior
Active-learning literature (uncertainty sampling). Spam filter
"is this spam?" feedback loops. Recommendation systems' implicit-feedback
calibration.

### Assumption challenged
"Decoder confidence and operator-acceptance are the same signal."

### Kill-switch
If label-rate <1/session after 4 sessions, kill (operator won't engage).
If classifier never reaches 80% acc after 500 labels, the features
chosen are bad — revisit feature engineering or kill.

### Effort
2-3 sessions. TUI label keys (cheap) + label persistence + sklearn-style
LR via `linfa` Rust crate (or just a hand-rolled LR). Real cost is the
weeks of label-gathering before the classifier becomes useful.

### Headline risk
Pancetta becomes operator-biased and rejects what other operators
*would* call valid decodes (e.g., contest-format messages K5ARH doesn't
recognize). Mitigate: keep filter opt-in per band/mode-context, never
discard, just down-rank.

---

## H13 — "Confidence whisper": pancetta narrates its decisions to operator in real time

### Mechanism
A TUI panel ("Decision Log") shows the autonomous operator's reasoning
turn-by-turn in plain English:

```
14:02:00  Heard W1ABC FN42 -12. Score=0.42 (needed_dxcc=0). Holding for better.
14:02:15  Heard P29NO QH00 -8.  Score=0.91 (needed_dxcc=+0.35, rarity=+0.31, snr=+0.05).
          Allocating 1450Hz, parity=odd. Calling P29NO K5ARH EM10.
14:02:30  No response from P29NO; he answered VK6ZW. Will re-call next P29NO CQ.
```

Operator reads it. After 10 sessions of "I'd have made the same call",
trust accrues. Where they disagree, operator can `*` the missed
opportunity (H3) and pancetta learns the disagreement.

This is HITL *by transparency* — no direct feedback, but visible
reasoning lets operator catch and correct.

### Why operator-in-the-loop helps here
Black-box autonomy is hard to trust. Once the operator can SEE the
score breakdown, they can:
1. Diagnose mis-tuning ("why is rarity=0.5 for that obviously-rare call?")
2. Course-correct via H3/H7 with intention
3. Eventually approve full autonomy because they've watched it think

### Defensible prior
Anthropic's own "thinking" mode in Claude. Cursor's "explain this
suggestion" panel. Aviation flight-director displays.

### Assumption challenged
"Operators don't need to see why the autonomy made each call."

### Kill-switch
If decision-log panel is collapsed/hidden for >90% of session-time
across 5 sessions, kill. Operator doesn't care; ship pure-autonomous.

### Effort
1 session. Just thread `ScoreBreakdown` through to TUI; renderer is
straightforward. Most of the work is making it READABLE not data-dumpy.

### Headline risk
Information overload — band-activity already crowds the panel real
estate. Mitigate: collapsed by default, expand on `D` keypress.

---

## H14 — Operator-hot-paths: rapid `+`/`-` reinforcement during session

### Mechanism
Each decoded message rendered in band-activity panel is implicitly
selectable. Operator can press `+` or `-` on any visible row to give
feedback:

- `+` = "I'd want this" — promotes callsign to manual-priority-boost
  (H3) PLUS records a `+1` reinforcement signal
- `-` = "I don't care" — demotes callsign with manual-priority-suppress
  PLUS records a `-1` reinforcement signal

Over time, pancetta builds a feature-weighted reinforcement model
(prefix, country, grid distance, mode, band, time-of-day) similar to
H12 but on *priority* not decoder. After 500 +/- signals, suggest
applying learned weight deltas.

### Why operator-in-the-loop helps here
H3 alone is one-shot; H14 is the cumulative-learning version. Cheap
input (two keys), high-throughput feedback channel.

### Defensible prior
Reddit upvote/downvote → ranking. Tinder swipes → preference learning.
TikTok dwell-time → recommendation. The "thumbs up" pattern.

### Assumption challenged
"Operator preferences are static config, not learned from interaction."

### Kill-switch
If `+/-` press-rate < 5/session after 3 sessions, operator isn't
engaging — kill. If learned weights destabilize across days
(thrashing), use larger smoothing window or kill.

### Effort
2 sessions. Key handler + persistent feedback log + simple
linear-weight fitter.

### Headline risk
Operator emotional momentum (`-` everyone after a bad QSO) corrupts the
training set. Mitigate: per-session ratio normalization, anomaly
detection ("you `-`'d 40 in 5 minutes, ignoring this session").

---

## H15 — Mixed-initiative QSO authoring: operator dictates message text mid-QSO

### Mechanism
Within QSO state machine, operator can press `m` to interrupt the
auto-generated next message and type a custom one:

```
[QSO with W1ABC, expected next: 'W1ABC K5ARH R-12']
Operator types: 'W1ABC TU FB QSO 73'
[Override accepted, will send next TX slot. QSO state → COMPLETING]
```

Free-form text (validated against FT8 message-format library) replaces
the scripted next message. Useful for: "thanks, fine biz" closings,
QRZ requests, dupe-clearing ("W1ABC DUPE TU"), 2x exchange when grid
is missing.

### Why operator-in-the-loop helps here
Real ham radio has texture pancetta's QSO state machine doesn't model
(yet). Operator can compose situationally-appropriate messages without
exiting autonomous-everything-else mode.

### Defensible prior
WSJT-X's "Tx5" custom message slot. JTDX free-text. Every chat client
that lets you type in the middle of a typing-indicator from the bot.

### Assumption challenged
"All QSO messages must come from the state machine's scripted templates."

### Kill-switch
If `m` is pressed <1x/week, the templates are good enough — kill or
hide the feature. If `m` is pressed in 50%+ of QSOs, the templates are
too restrictive — fix templates first.

### Effort
2 sessions. Inline text-edit widget + ft8-message validation + queue
to TX path. Need to gate against breaking QSO state (don't send
"73 TU" mid-report).

### Headline risk
Operator types invalid FT8 message or mistypes callsign; sends garbage.
Mitigate: strict validation pre-send + 2s edit-buffer with preview.

---

## Summary statistics

- **Count**: 15 ideas (target 12-15, achieved upper bound).
- **Wild cards**: H6 (voice/foot-pedal).
- **Top 3 by value-per-friction**:
  1. **H4 (operator STOP key)** — 1 session effort, infinite value
     (it's an e-stop; required for Phase 5 by basic safety).
  2. **H3 (`*` priority boost)** — 0.5-1 session, immediate operator
     leverage on top of working autonomy.
  3. **H8 (real-time alarms)** — 1-1.5 sessions, covers the
     "made coffee, missed P5" failure mode of pure autonomy.
- **Counter to autonomous goal but worth considering**:
  **H11 (co-pilot mode)** — explicitly suggests turning autonomy *off*
  by default. Worth it because: (a) it builds operator trust for
  graduating to autonomy, (b) every co-pilot interaction is training
  data (H10/H12/H14), (c) some bands or first-of-DXCC chases will
  reasonably want manual confirmation forever, (d) shipping it makes
  pancetta usable for ops who would otherwise not trust full autonomy
  — *expanding* the autonomous-goal user base.
- **Distinct from prior closed hypotheses**: All 15 introduce a
  runtime feedback channel from operator to pancetta. Prior work
  (hb-087, hb-062) used offline operator data passively; H1-H15 are
  online and interactive.
