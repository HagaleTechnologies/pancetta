# Contest-mode support — design

**Status:** Draft, pending user review
**Origin:** PAN-49 (Kansas QSO Party "R"+grid decodes stalled two real manual QSOs the night of 2026-08-29/30). What started as an "invalid decode" report turned out to be a correct decode of a real, common contest exchange convention that pancetta's QSO engine had no way to recognize. This design generalizes that fix into full contest-mode support.
**Related tickets:** PAN-49 (this design + the immediate fix), PAN-50 (multiplier/scoring, deferred), PAN-51 (text-vs-struct classification architecture, deferred).

## Background

pancetta-ft8 correctly decodes `ir=1` + a valid grid as `StandardMessageType::ReplyWithR` (`pancetta-ft8/src/message.rs:1891-1900`, rendered `"<to> <from> R <grid>"`). This is real, common traffic: 285 such exchanges were heard the night of 2026-08-29/30 from dozens of unrelated callsign pairs, alongside many `CQ KSQP` CQs — the Kansas QSO Party's standard grid-for-report FT8 convention (participants exchange grid squares, acking with "R"+grid instead of a numeric report). Two of pancetta's own manual QSOs (K5TD, W0D) hit this and stalled, because `pancetta-qso`'s `MessageExchange::parse_message` (`pancetta-qso/src/exchange.rs:295`) re-derives its own classification from the *rendered text* via a fixed regex list (`QSO_PATTERNS`), which has no pattern for `"<call> <call> R <grid>"` — it falls through to `MessageType::NonStandard`, which the QSO state machine doesn't advance on.

Research into the broader space of FT8-worked contests (see PAN-49 for full citations) found:

| Contest | Exchange (FT8) | Wire mechanism | Confidence |
|---|---|---|---|
| US state QSO parties (KSQP, SCQP, etc.) | Grid both ways, "R"+grid ack | Standard i3=1/2, grid+ir | High |
| ARRL Field Day | Class + Section, R ack | Dedicated i3=0/n3=3-4 (decode already built) | High |
| EU VHF Contest | 6-char grid + report | Dedicated i3=0/n3=2 (decode already built) | High |
| NA VHF Contest | Same shape as EU VHF | Same wire type, distinct catalog entry | High |
| ARRL RTTY/FT Roundup | RST + state/province/DX | i3=3 (decode already built, generation already exists but only for RstSerial) | High |
| ARRL International Digital Contest | 4-digit grid | Standard i3=1/2, grid, same mechanism as state QSO parties | High |

WW Digi Contest, classic ARRL Sweepstakes, CQ WW/WPX, NAQP, and Winter Field Day were investigated and excluded from v1 (unverified ack-step shape, no representable FT8 exchange, no confirmed practice, or digital modes disallowed by contest rules, respectively).

## Scope

**In scope:** recognizing and generating the six catalog contest exchanges above for **manually-initiated QSOs only**; an operator-facing "enter this contest?" prompt; a general free-form fallback for any directed-to-us message pancetta can't classify (contest or not); passive pattern inference that proposes new candidate profiles from repeated unclassified traffic; ADIF logging of contest fields; Cabrillo export.

**Out of scope (deferred, ticketed separately):** multiplier/score tracking and contest-scoped dupe checking (PAN-50); replacing text-based classification with direct consumption of pancetta-ft8's typed `Ft8Message` struct (PAN-51); autonomous (unsupervised) contest operation — the autonomous engine's behavior is unchanged by this design; WW Digi Contest and any other unverified contest profile.

## 1. Contest profile data model & catalog

```rust
pub struct ContestProfile {
    pub id: String,                    // "us-state-qso-party", "arrl-field-day", ...
    pub display_name: String,
    pub cq_tag_patterns: Vec<String>,  // "KSQP", "SCQP", "FD", ... matched against the CQ modifier text
    pub exchange_shape: ExchangeShape,
    pub verified: bool,                // false reserved for future not-yet-field-confirmed entries; none in v1
    pub source_notes: String,          // provenance / research citations, for future maintenance
}

pub enum ExchangeShape {
    GridWithRAck,          // state QSO parties + ARRL Intl Digital: plain grid, then "R"+grid ack
    FieldDayClassSection,  // existing i3=0/n3=3-4 wire type
    VhfContestGridReport,  // EU/NA VHF Contest, i3=0/n3=2
    RstSerialOrState,      // ARRL RTTY/FT Roundup, i3=3 — generalizes today's dormant ContestConfig
}
```

The six locked-in contests seed this as static built-in data, with `source_notes` carrying the research provenance for future maintenance. Profiles live in **pancetta-config**, in a new `contest` section, alongside a `custom_profiles: Vec<ContestProfile>` the operator can hand-edit directly — reusing the existing hot-reload path rather than inventing a new config surface. This is also where the operator's own per-profile exchange data lives (Field Day class/section, RTTY/FT Roundup state/serial, etc. — see §3).

## 2. Recognition: profile matcher + general fallback

A new, shared, lightweight tokenizer extracts `(to_station, from_station, trailing_tokens)` from decoded text whenever the first two tokens look like callsigns, regardless of whether the trailing content matches anything known. Today's code has no equivalent — `MessageType::NonStandard` only carries raw text, no parsed calls. Both the contest matcher and the general fallback (below) need this.

Each `ExchangeShape` gets one matcher function (e.g. `GridWithRAck` recognizes `<to> <from> R <grid>` as an ack). These matchers only run for a contest profile **already engaged on that specific QSO** (§4) — `QSO_PATTERNS` and all non-contest classification stay completely untouched, preserving the "FT8 mode paths byte-identical" invariant for every QSO that isn't contest-engaged. `RstSerialOrState` continues to classify into the existing `MessageType::ContestExchange` variant (already wired into the state machine and `is_directed_response`) — no new classification needed there. `GridWithRAck`, `FieldDayClassSection`, and `VhfContestGridReport` had no prior classification at all; a match on these produces a new `MessageType::ContestReply { to_station, from_station, exchange: ContestExchangeData, is_ack: bool }`, which the state machine treats as exchange progress exactly the way `Report`/`ReportWithR` are treated today.

**General fallback (the free-form safety net):** when a message tokenizes with `to_station` matching us, but classifies as neither a known `QSO_PATTERNS` shape nor an engaged contest profile's shape, it's surfaced to the operator (§4) instead of silently landing in `NonStandard` as today. Scoped to `to_station == our_callsign` so background traffic we merely overheard never surfaces.

## 3. Generation

`pancetta-ft8`'s encoder already has `encode_field_day`, `encode_eu_vhf`, and `encode_rtty_roundup`. Generation for `FieldDayClassSection`, `VhfContestGridReport`, and `RstSerialOrState` is therefore mostly wiring: `MessageExchange::generate_message` (pancetta-qso) is extended to route to the right encoder call based on the QSO's engaged `ContestProfile`. Today's `ExchangeFormat` enum (`RstSerial`/`RstState`/`RstGrid`/`Custom`) declares variants that are never actually branched on — the `ContestExchange` generation arm always formats report+serial regardless. This design supersedes that dead enum with per-profile `ExchangeShape` routing rather than trying to resurrect its unused variants.

`GridWithRAck`'s first-step reply is an ordinary grid message — already fully supported, unchanged. Only the second-step **"R"+grid ack** needs new encoder support: `try_encode_standard`'s text-shape dispatch doesn't currently produce `ir=1` paired with a grid value. This is the mirror image of the already-existing decode-side `unpackgrid`/`parse_type1_standard` logic — small and contained to `pancetta-ft8/src/encoder.rs`.

**New config:** the operator's own exchange data per profile (Field Day class+section, RTTY/FT Roundup state/serial; VHF contest reuses the existing grid) lives in the `contest` config section from §1.

**Invariant preserved:** none of this fires for a normal QSO — generation only activates once a profile is engaged on that specific QSO (§4).

## 4. Operator UX

**"Enter this contest?" modal** — fires only on a manual call/response action (the autonomous engine never triggers or sees this), when the engaged station carries a recognized CQ tag or exchange shape from the catalog. Shows the contest name and detected exchange format, with Enter / Skip actions. Per-session memory is keyed by *profile id*, not station: accepting once engages that profile for every subsequent manual QSO with that contest, for the rest of the session, with no further prompts; declining re-prompts on the next engagement (a decline for one station isn't assumed to mean "skip this contest entirely").

**Free-form fallback prompt** — decoupled from contest engagement; this is §2's general safety net. Fires whenever a directed-to-us message can't be classified: shows the raw decoded text and a best-guess pre-filled reply built from the tokenizer's `(to, from, trailing)` split, editable, with Send / Ignore. "Send" reuses the existing `MessageType::NonStandard` TX path (`generate_message` already echoes arbitrary text verbatim) — no new TX plumbing — and logs like any manually-composed exchange.

## 5. Pattern inference (learn-a-new-profile)

No new supervised coordinator component. Every existing supervised task (Autonomous, DxCluster, Ft8Decoder, etc.) carries real restart-policy and health-wiring overhead (`ComponentCriticality`, restart budgets, `health.rs` plumbing); this feature is purely advisory, never touches TX/PTT, and can silently skip a cycle with zero consequence — not worth a new criticality tier.

Instead: a small in-memory tracker (rolling counts of unclassified-exchange-shape signatures, keyed by CQ tag / shape fingerprint, across distinct callsigns) fed inline from the same fallback path in §2 — every message landing in the general-fallback bucket is also examined here. This *is* the "idle time between windows," since decode-result handling already happens there; no separate scheduled tick is needed. Crossing a threshold (N distinct stations × M occurrences) surfaces a non-blocking toast (not a focus-stealing modal) offering to add a candidate profile, pre-filled from the inferred shape, using the same `ContestProfile` model from §1. Defensively wrapped so a bug here can never affect decode/QSO logic. State is in-memory and session-scoped, resetting on restart — consistent with the modal's accept/decline memory.

## 6. Logging — ADIF + Cabrillo

**ADIF:** contest QSOs flow through the existing logger unchanged, populating ADIF's already-standard contest fields (`CONTEST_ID`, `SRX_STRING`/`STX_STRING` or `SRX`/`STX` for numeric exchanges) rather than inventing new ones.

**Cabrillo:** new capability. Cabrillo's QSO-line format varies by contest sponsor, so each `ContestProfile` carries its own `cabrillo_qso_line_format` template, and export is a post-hoc pass over the ADIF-logged contest QSOs for a given profile/date-range — not real-time generation, and no attempt at a universal Cabrillo generator for arbitrary contests.

## 7. Testing strategy

- **pancetta-ft8:** round-trip encode/decode unit tests for the new `GridWithRAck` ack-packing addition (mirroring existing report/token round-trip tests), plus confirming `encode_field_day`/`encode_eu_vhf`/`encode_rtty_roundup` round-trip correctly once routed through the new profile-aware `generate_message`.
- **pancetta-qso:** unit tests per `ExchangeShape` matcher (including the shared tokenizer), state-machine tests confirming `ContestReply` advances a QSO the same way `Report`/`ReportWithR` do today, and a regression test replaying the actual PAN-49 log lines (`K5ARH K5TD R EM40`) end-to-end.
- **Integration:** extend `pancetta/tests/loopback_qso.rs` with a full contest QSO (encode → modulate → decode → classify → advance → log) for the `GridWithRAck` profile — the one with direct live evidence behind it.
- **TUI:** state-level tests for modal trigger/remember logic (accept-sticks/decline-doesn't) and fallback-prompt pre-fill, without requiring real terminal rendering, matching existing TUI test conventions.
- **Non-contest regression:** existing FT8/QSO test suites must stay green untouched — the hybrid approach's entire point is that ungaged QSOs take the exact same code path as today.

## Open risks / follow-ups

- PAN-50 (scoring/multipliers) and PAN-51 (text-vs-struct classification) are real gaps, deliberately deferred — this design does not attempt them.
- Cabrillo per-contest QSO-line templates will need real submission-format verification per contest sponsor before a log is actually submitted competitively; treat v1 Cabrillo output as "structurally correct, sponsor-format-unverified" until checked against a real submission.
- The pattern-inference thresholds (N stations × M occurrences) need tuning once live; no data yet on what avoids false positives during quiet band conditions vs. missing real contests during a busy weekend.
