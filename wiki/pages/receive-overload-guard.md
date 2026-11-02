# What will bite you about receive overload attenuation?

The opt-in FT8 guard measures raw 12 kHz capture samples, not normalized waterfall
bins or decoder SNR. A source slot includes pre-roll. A late TX can contaminate
that pre-roll even when requested PTT is false at emission. Only confirmed
hardware unkey ends the exclusion interval. While receiving, the Hamlib poll
loop also reads hardware PTT every 500 ms: a keyed reading (front-panel MOX,
another CAT client) makes the window known TX and discards earlier pending
work, and more than 2 s without an Off reading makes it invalid. A key shorter
than about one poll interval can go unseen.

Three eligible hot receive windows create one pending step. Missing or invalid
receive windows break the streak. Known TX windows do not count. Retunes, mode
changes, audio discontinuities, reconnects and gain changes invalidate old work.
A successful step needs three new hot windows before another request.

The Hamlib owner discovers supported integer-dB ATT steps and executes changes
at safe capture boundaries. No positive capability means no automatic write.
A gain reservation and the PTT admission path share state so either ATT or TX
wins, and PTT-off remains available. Failed or unconfirmed changes inhibit TX
until confirmed recovery. Closing a socket does not cancel physical rig work.

The guard does not restore attenuation. The operator or a per-band preset does.
Hardware qualification remains pending. Code and `docs/CONFIG.md` define the
current behavior; this page records the integration constraints.
