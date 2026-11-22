# Tier alerts and operator watchlist

PAN-261 adds optional tier admission for autonomous pouncing. Existing numeric
score thresholds still apply when `pounce_min_tier` is unset. When it is set,
tier admission replaces both numeric thresholds. Rarer tiers rank first; the
weighted score breaks ties within a tier. Other pounce safety gates remain.
Tier names and ranking come from the shared priority scorer. Config names use
`atno`, `per_band_dxcc_new`, `special_station`, `per_band_grid_new`, `standard`
and `suspect`, in rarity order. This avoids a competing tier-number system.

Decode alerts use that same scorer and cached station lookup. Each channel has
its own threshold. Tier triggers require a CQ; operator pins trigger on any
decode. A decode from a different captured band cannot alert after a band
switch. Unknown cqdx need lists do not provide evidence for fallback-created
DXCC tier alerts. Locally established needs and pins remain usable.

All channels default off. Bell runs only in the TUI; headless operators can use
ntfy, webhook and INFO logs. HTTP delivery makes one attempt with a five-second
timeout and no redirects. Repeated decodes deduplicate per callsign, band and
channel for 300 seconds by default. Three consecutive delivery failures pause
a target for five minutes. Notifications are transient, so they do not use the
durable QSO-upload outbox. The ntfy JSON request goes to the server root and
names its topic in the body. The generic webhook receives a versioned
`decode_alert` payload with call, entity, band, audio offset, SNR, tier and
trigger. No live notification service has been qualified.

PAN-361 hardens the webhook only; ntfy keeps its URL rule and has no rate
limit, because self-hosted ntfy on a LAN address is common. An enabled webhook
must be an https URL on a public host. One opt-in,
`alerts.webhook.allow_private_targets`, accepts plain http, loopback, private,
link-local, CGNAT, unspecified and broadcast literals (IPv4 inside IPv6 is
judged as IPv4) and the special-use names `localhost`, `.local`, `.home.arpa`,
`.internal` and single-label names. The check reads the parsed URL only and
never resolves a name, so a public name that resolves to a private address, or
DNS rebinding, is not caught. The alert dispatcher repeats the check when it
starts and turns only the webhook channel off, with a URL-free warning, rather
than fail the Autonomous task. Webhook POSTs are capped at
`alerts.webhook.max_per_minute` (default 10) admissions in any rolling 60 s,
summaries included, counted after dedupe and the failure pause. An alert over
the cap, or arriving while others are already held, is held. Held state is a
running count plus the rarest 20 alerts, so an ATNO is never pushed out by
routine pins. The Autonomous 250 ms timer sends the held alerts once the
window has room and the target is not paused: one `decode_alert_summary`, or
a plain `decode_alert` when only one was held. A pause delays the summary and
never drops it; the summary is a single attempt that counts toward the
failure pause.

`Shift+W` opens a callsign input prefilled from the focused DX Hunter row.
Enter toggles that pin; Esc cancels. Pins render as `◆`, while automatic
near-miss entries retain `◇`. Pins bypass automatic expiry and persist in
`~/.pancetta/pancetta.toml` until removed or worked. The autonomous task owns
the list; operator edits and completed-QSO removal use the same bus command.
The relay persists changed pin snapshots through the comment-preserving
config writer. Automatic entries retain their default 150-second expiry.
Watchlist membership never boosts scoring or initiates TX.

Alert, watchlist and tier admission settings require restart, like other
non-consent config settings. Existing cluster-alert and sound-file config
models stay unchanged; their cleanup and Pushover integration are outside
this ticket. See `docs/CONFIG.md` for exact keys and defaults.
