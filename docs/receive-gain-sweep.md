# Measured receive-gain sweep

The opt-in FT8 gain experiment alternates the rig's supported PREAMP and ATT
settings and saves an advisory per-band JSON report. It does not apply a preset.
The implementation has synthetic audio and CAT tests. No FTDX10 timing or live
radio qualification has been performed for this feature.

Stop any other Pancetta process controlling the rig before starting:

```sh
pancetta --headless --gain-sweep-seconds 600 --gain-sweep-report gain-20m.json
```

Both flags and `--headless` are required. Supplying `--gain-sweep-seconds`
without a value requests 600 seconds. Accepted durations are 1 through 3600
seconds. The destination must not exist. Replay, WAV input, disabled audio and
test transmission are incompatible with a sweep. The normal station exits after
experiment cleanup. Diagnostics go to stderr; sweep progress and results go to
stdout. Keep the dial, mode, RF gain and AGC fixed during the experiment.

## Windows and transmission

The experiment discovers supported integer PREAMP/ATT steps from `rigctld` and
reads both original controls before any write. Candidates include each positive
attenuator step with the preamp off, bypass, and each preamp step with attenuation
off. They are ordered by nominal net gain. Unsupported or malformed capabilities
make the sweep unavailable.

A candidate occupies one eligible receive window. Cycles include every candidate
once, with a rotated starting point to balance source parity. With exactly two
settings, an extra unmeasured receive slot holds the last setting after every
cycle. This quiet slot changes parity so stations transmitting on either parity
can be compared. TX skips do not advance the candidate. The duration is elapsed
time, so skipped slots reduce the amount of evidence collected.

Gain changes start at the capture boundary, normally 500 ms before the UTC slot
start, or 1 second with extended capture. Wakeups more than 50 ms late are skipped.
Readback must finish at least 250 ms before the signal begins, followed by settling.
Slow or uncertain CAT transactions invalidate the window and end the sweep.

PTT and gain transitions share an admission gate. Active or pending TX prevents
an experiment change. Configured fixed TX-parity slots are skipped even when no frame
is queued. With automatic parity, idle slots remain eligible, but pending TX
intent reserves the station until that request completes or is cancelled.
Frequent TX can leave too little shared-parity evidence for a result.
A PTT-on request cannot key during a gain transition. PTT-off remains available.

## Measurements and recommendations

Noise is raw one-sided spectral power in dBFS/Hz. The calculation uses 2048-point
periodic Hann FFTs at 12 kHz, 50% overlap, normalized by sample rate and summed
squared window coefficients. A full-scale sine has integrated power 0.5, or
-3.0103 dBFS. The median uses time-frequency cells at 100-300 Hz and 2500-2900 Hz,
excluding bins within 50 Hz of decoded signals. At least 20 unmasked bins and
positive finite powers are required. All-zero, non-finite and clipped audio are
invalid. Clipping means at least 0.1% of samples reach absolute amplitude 0.999.
Negative-time capture lead-in is excluded from measurement.

The report counts valid zero-decode windows. Distinct resolved transmitting
callsigns count once per setting. Matched SNR is the median of per-station median
decoder SNR, restricted to stations heard under multiple settings. An S-meter
rise is not evidence of improved decoded SNR.

Paired comparisons match each station to its nearest observation under another
setting on the same source-slot parity, within 120 seconds. Ties prefer the
earlier slot. An observation is used once per station in each comparison.
Differences are summarized first per station, then across stations. Every lower
candidate is compared with every higher candidate.

The lowest candidate qualifies when all higher gains improve paired SNR by at
most 1 dB. Each comparison needs at least three resolved stations and six distinct matched
window pairs, and every candidate needs at least six valid windows. Missing evidence,
continued SNR improvement and a sweep with only the highest candidate remaining
produce no recommendation. Unique-station counts and rising noise explain the
result but do not replace paired evidence. The 1 dB rule is a reproducible
heuristic, not a statistical confidence claim.

## Cleanup and saved evidence

Completion, graceful interruption, dial or mode change and CAT failure all attempt
to restore both original controls with readback. Cleanup has a 30-second bound
and retains TX inhibition while the hardware state is uncertain. SIGKILL,
process or power loss and an unreachable rig cannot guarantee restoration.
Check the rig manually after a restoration failure. Interruption and operational
failures never retain a recommendation. Saving fails rather than overwriting an
existing report, including one created while the experiment runs.

The version 1 JSON contains source slots, generation and gain epochs, observations,
skipped reasons, gain pairs, summary values, paired comparisons, algorithm
parameters, restoration status and a nullable recommendation. Unavailable numeric
values appear as JSON null and as `n/a` in terminal summaries. This file is the
local handoff for future per-band presets. It creates no remote API or automatic
configuration changes.
