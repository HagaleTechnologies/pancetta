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
