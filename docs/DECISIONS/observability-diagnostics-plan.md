# Observability and diagnostics

## PAN-212: possible TX-induced desense

The detector uses capture intervals rather than decode-completion time.
A window overlapping our PTT interval is `tx` and cannot warn or feed the
baseline. Only the first receive window after PTT-off is `post_tx` and
warning-eligible. Later windows are `quiet`. The capture interval includes
1,000 ms of conservative pre-roll for extended decode windows. FT4 uses its
actual 7.5-second slot length.

Each band has up to 20 quiet samples. At least five samples, no older than
20 minutes, and a mean of at least 3.0 are required. The freshness bound
implements the ticket's same-band, same-hour comparison without fragmenting
history at clock-hour boundaries. A count at most 25% of the mean is a
collapse and never enters the baseline. The latest quiet same-band window
before PTT must have at least 50% of the baseline mean. Collapsed quiet
windows remain available for this health check even though excluded from
the baseline. A healthy receive window rearms the per-band episode latch.

`last_ptt_off_ms` records software unkey time beside every `ptt_active`
clear, including cancellation fallback and mid-window abort/re-key paths.
It represents enqueue-time intent, not hardware confirmation. Drop clears
the badge before its asynchronous delivery attempt, as before; that timestamp
is therefore a fallback estimate. Unknown or older-than-on off timestamps
assume a one-slot TX interval. An active PTT snapshot forces `tx` classification.
The decoder snapshots this evidence at window receipt, before decoding.

The warning template, with example values substituted, is:

```text
possible TX-induced desense on 40m: only 1 messages decoded in the first receive window after our transmission vs. a 10.6-message recent 40m baseline (band healthy before TX) — if this keeps happening, check TX/RX audio-path isolation (e.g. a shared USB audio interface)
```

The existing `ft8.desense` warning and TUI Diagnostics routing stay in place.
One debug line per window under `ft8.desense.window` records band, slot start,
count, class, baseline size and mean, including windows that do not warn.
Future logs can compare post-TX windows with quiet windows in the same band
and hour. The October 15/16 logs are unavailable here, so that incident has
not been retrospectively measured.

On-air qualification remains unverified. Confirming or excluding USB audio
coupling or AGC recovery requires station measurements. Confirmed coupling
needs a separate ticket and a linked sophon ticket. The heuristic reports
possible coupling; temporal evidence alone cannot prove causation.
