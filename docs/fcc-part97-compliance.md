# FCC Part 97 Compliance Analysis — Pancetta (US amateur, autonomous FT8)

**Date:** 2026-06-23 · **Scope:** 47 CFR Part 97 as it applies to how pancetta
operates, with emphasis on unattended / automatic operation. **This is an
engineering compliance review, not legal advice.** The station licensee /
control operator is ultimately responsible (§97.103).

Authoritative text was read from the Cornell LII mirror of 47 CFR (§§97.109,
97.113, 97.119, 97.221). Frequencies below are the de-facto FT8 calling
frequencies pancetta and WSJT-X use.

---

## TL;DR

There is **one** real exposure, and it is specifically about **truly unattended
(automatic-control) operation that ORIGINATES calls**:

- **Running pancetta autonomously while you (the control operator) are present /
  monitoring is LOCAL or REMOTE control → fully compliant** on the normal FT8
  frequencies. §97.221 does not constrain you, because it only governs
  *automatic* control. You can call CQ, pounce, and complete QSOs.
- **Running pancetta UNATTENDED (no control operator at the control point =
  automatic control) and letting it ORIGINATE CQs on the standard FT8
  frequencies (14.074, 7.074, 10.136, 18.100, 21.074, 28.074, …) is contrary to
  §97.221** — those frequencies are **not** in the rule's automatic-control
  segments, and originating a CQ is not "responding to interrogation."
- Unattended **respond-only** operation (answering a human's CQ/call, ≤500 Hz
  bandwidth) on those frequencies is defensible under §97.221(c), but the
  cleanest posture is still to have a control operator.

Everything else pancetta does (identification, content, frequencies, etc.) is
compliant. Details below.

---

## 1. Station control — §97.109 (the core issue)

> (b) Local control … the control operator must be at the control point. Any
> station may be locally controlled.
> (c) Remote control … the control operator must be at the control point. Any
> station may be remotely controlled.
> (d) Automatic control … the control operator **need not** be at the control
> point. **Only stations specifically designated elsewhere in this part may be
> automatically controlled.** Automatic control must cease upon notification by
> a Regional Director … and must not resume without prior approval.

**How this maps to pancetta:**

| How you run it | Control type | Compliant to originate CQ on 14.074? |
|---|---|---|
| You're at the keyboard (or watching via Jump Desktop / SSH) and can intervene | **Local / Remote** | **Yes** — §97.221 doesn't apply |
| `--headless` under systemd/launchd, you're away | **Automatic** | **No** (see §97.221) |

The supervisor units we just shipped (`packaging/systemd`, `packaging/launchd`)
make *truly unattended* operation easy — which is exactly the mode that triggers
§97.221. That's the thing to gate/document.

## 2. Automatically controlled digital station — §97.221 (the binding rule)

> (b) A station may be automatically controlled while transmitting a RTTY or
> data emission on the 6 m or shorter wavelength bands, and on the
> **28.120–28.189, 24.925–24.930, 21.090–21.100, 18.105–18.110, 14.0950–14.0995,
> 14.1005–14.112, 10.140–10.150, 7.100–7.105, or 3.585–3.600 MHz** segments.
> (c) A station may be automatically controlled while transmitting a RTTY or
> data emission on any other frequency authorized for such emission types
> provided that: (1) the station is **responding to interrogation** by a station
> under local or remote control; and (2) **no transmission … exceeds 500 Hz**.

**The standard FT8 frequencies fall OUTSIDE the §97.221(b) segments:**

| Band | FT8 dial | In a (b) auto segment? |
|---|---|---|
| 80 m | 3.573 | No (segment is 3.585–3.600) |
| 40 m | 7.074 | No (7.100–7.105) |
| 30 m | 10.136 | No (10.140–10.150) |
| 20 m | 14.074 | No (14.0950–14.0995 / 14.1005–14.112) |
| 17 m | 18.100 | No (18.105–18.110) |
| 15 m | 21.074 | No (21.090–21.100) |
| 12 m | 24.915 | No (24.925–24.930) |
| 10 m | 28.074 | No (28.120–28.189) — but the **6 m and shorter bands are wholly permitted**, and 10 m is not 6 m |

So under **automatic control** on the normal FT8 frequencies, the only lawful
path is §97.221(c): **responding to interrogation, ≤500 Hz.** FT8 is ~50 Hz, so
the bandwidth condition (c)(2) is always met. The deciding condition is
(c)(1) — *responding* vs *originating*:

- **Pancetta "hunt/pounce" (answering a decoded CQ) and "answer a caller"** —
  this *is* responding to interrogation by another (human-controlled) station.
  Defensible under §97.221(c) even unattended.
- **Pancetta "CQ mode" (autonomous CQ origination, the `start_cq`/CQ-self path)**
  — this **originates**, so it is **not** covered by §97.221(c), and the
  frequency isn't in §97.221(b). **Unattended autonomous CQ on 14.074 is the
  contravention.**

## 3. Identification — §97.119 — COMPLIANT

> (a) … transmit its assigned call sign … at the end of each communication, and
> at least every 10 minutes during a communication … [no] unidentified
> communications.

Every pancetta transmission is a standard FT8 frame that contains our call sign
(`CQ K5ARH …`, `DX K5ARH …`, `K5ARH …`), so the 10-minute and end-of-QSO ID
requirements are met by construction — the same mechanism WSJT-X relies on. **No
issue.** (Caveat: never transmit a free-text/Tx5 frame that omits the call for
>10 min; pancetta's structured messages always carry it.)

## 4. Prohibited transmissions — §97.113 — COMPLIANT

Pancetta transmits only standard FT8 exchanges (CQ, grid, signal report, RR73,
73). It sends **no** business/compensated traffic, music, codes to obscure
meaning, obscene content, or **false signals/identification**, and it transmits
**only the operator's own authorized call sign**. The callsign-trust / FP
filters and sender-verification actually *strengthen* the §97.113(a)(4) "no
false/deceptive identification" posture (we won't log/answer a fabricated call).
**No issue.**

## 5. Licensee / control-operator responsibility — §97.103 / §97.105

The licensee is responsible for proper operation regardless of automation. The
operator must hold privileges for the frequency/mode in use. Pancetta uses the
operator's own call and operator-selected frequencies — responsibility rests
with the operator, as the rules intend.

---

## Recommendation (for the operator to decide)

The contravention is narrow and avoidable. Options, roughly in order of
least-surprise:

1. **Document the boundary (minimum).** State in the RUNBOOK/README that
   *unattended* autonomous operation is **automatic control** under §97.109(d),
   and that under automatic control on the normal FT8 frequencies pancetta must
   be **respond-only** per §97.221(c) — autonomous CQ origination requires a
   control operator present (local/remote) **or** one of the §97.221(b)
   automatic-control segments. Frame the supervisor (systemd/launchd) section
   accordingly.

2. **Add a software guard (recommended).** Introduce a config/runtime notion of
   "unattended" (e.g. `--headless` + autonomous + no recent operator input) that
   forces **RespondOnly** for *initiation* — i.e. the autonomous engine may
   pounce/answer but **not** auto-originate CQ — unless the dial is within a
   §97.221(b) segment. This maps cleanly onto the existing `TxPolicy`
   (`RespondOnly` already suppresses initiations) and the autonomous CQ-self
   path; it would make the safe behavior the default for headless operation.

3. **Frequency-aware automatic control.** Allow autonomous CQ origination under
   automatic control **only** when the current dial is inside a §97.221(b)
   segment; otherwise respond-only. (More complex; combine with #2.)

I have **not** changed any operating behavior — per your instruction, this is
the finding; you decide how to proceed. If you want, the cleanest first step is
option #2 wired onto the existing `TxPolicy::RespondOnly` machinery.

*Sources: 47 CFR §97.109, §97.113, §97.119, §97.221 (Cornell LII mirror,
read 2026-06-23).*
