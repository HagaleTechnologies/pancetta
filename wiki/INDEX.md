# pancetta wiki index

- [pancetta — what is this and where do things live?](pages/overview.md) — pancetta is an autonomous FT8/FT4 ham-radio station in Rust: it decodes,
- [How do operating modes (FT8/FT4/FT2, Hound) work?](pages/modes.md) — pancetta runs a single station-wide operating mode — FT8, FT4, or FT2 — chosen
- [How does the QSO engine work and why is it shaped this way?](pages/qso-engine.md) — The QSO engine is the state machine in `pancetta-qso` that advances a contact
- [How does remote operation work and why is it shaped this way?](pages/remote-operation.md) — Remote operation is two separate, default-OFF subsystems: a **read-only
- [How does TX scheduling work and why is it shaped this way?](pages/tx-scheduling.md) — TX scheduling is WSJT-X-style and driven by *slot parity*: every decoded frame
- [Why the config-merge guardrail, hardware tiers, and decoder budgets?](pages/config-and-platform.md) — This covers three platform decisions: hand-written `ConfigSection::merge_with`
- [Why is pancetta written in Rust?](pages/language-rust.md) — pancetta is written in Rust (ADR-001, Accepted). It was chosen for memory safety
- [Why is QSO logging ADIF-first with opt-in per-QSO uploads?](pages/logging-uploads.md) — QSO logging is an ADIF-first hybrid: `~/.pancetta/qsos.adi` is the durable,
- [Why is the TUI shaped the way it is?](pages/tui.md) — The TUI was redesigned (2026-07-03) into four task-focused activity views
- [What will bite you about the additive-only remote gateway?](pages/additive-only-gateway.md) — The remote gateway is wired **additive-only**: every feed to it is a *new* bus
- [What will bite you about audio channel framing into the DSP stage?](pages/audio-channel-framing.md) — every buffer on `audio_to_dsp_tx` is interleaved frames, not mono; DSP de-interleaves against `[audio] input_channels` and a mono producer silently halves the effective sample rate.
- [What will bite you about the armed-TX gate?](pages/fail-closed-arm-gate.md) — The remote-TX arm gate **fails CLOSED**: on a poisoned lock (or any verify
- [What will bite you about the half-duplex parity rule?](pages/parity-rule.md) — FT8 is deaf while transmitting, so **every concurrent active QSO must transmit
- [What will bite you about measuring impulsiveness on real recordings?](pages/impulsiveness-metric.md) — a slot-global median is fooled by level drift, per-bin metrics by dense bands, and edge-truncated windows by next-slot onsets; PAN-162's instrument uses a 25-block in-slot local median at 3×.
- [What will bite you about retrying a failed decode candidate?](pages/candidate-retry.md) — a retry must bypass the soft combiner (no double-counted audio, no evicted bucket), check the budget before it starts, and is not cheap: retrying every failed candidate doubled PAN-155's unlimited eval time.
- [What will bite you about the Serena project config?](pages/serena-project-config.md) — `.serena/project.yml` gives catalyst-dev agents real symbol search; activation rewrites the file in place if it sees a legacy schema key, so the committed config already uses the current key names.
- [What's the PR review convergence policy?](../docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md) — Four-tier round schedule per reviewer: rounds 1-5 fix everything, 6-15 ticket new P2-or-lower, 16-25 ticket P1 too unless critical/blocking, round 25 is a hard stop — so PRs converge instead of oscillating.

- [What can invalidate a timing-history experiment?](pages/timing-history.md) — fresh slot clocks, native timing coordinates, and missing baseline truth.
