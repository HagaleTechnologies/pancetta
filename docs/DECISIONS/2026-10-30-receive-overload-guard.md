# Receive overload attenuator guard

PAN-246 adds opt-in attenuation for live FT8 receive audio. The default remains
disabled because this changes a rig's gain. Peaks come from the captured 12 kHz
decode samples before software gain. Three eligible windows at or above -1 dBFS
request one supported ATT increase. Three windows strictly below -6 dBFS cancel
pending action. Every successful change requires three new hot windows before
another step. The normal 0.25 to 0.28 full-scale reference does not trigger.

Hamlib supplies the discrete integer-dB steps. Unsupported discovery disables
the feature for that connection instead of substituting a model table. The guard
reads the current setting at execution time and never decreases attenuation.
The operator or the per-band preset owns resetting it.

The receive capture boundary precedes the source slot by its pre-roll. Gain
changes reserve the radio against PTT-on and require hardware-confirmed PTT-off.
Queued unkey is not confirmation. A failed or uncertain gain write keeps TX
inhibited until reconnect and stable readback. PTT-off remains available.
Socket closure cannot cancel a command already received by hardware.

Diagnostics and service logs share the `rig.rx_overload` notices. Success means
acknowledgement and matching ATT readback. Pending work adds no status-line
writes. Typed readiness exposes the same gain-transition reason as the TX gate.

Radio qualification is unverified, including FTdx10 ATT discovery, physical
attenuation, boundary headroom and settling time. Mock and TCP tests establish
software behavior only. See `docs/CONFIG.md` for configuration and the station
qualification checks.
