# What will bite you about silent audio-output recovery?

`OutputWatchdog` announces `WentDead` once per episode and keeps that latch
across `rebaseline`. A reopen that rebuilds a stream which never calls back
produces no second edge, so PAN-295's `OutputReopenPolicy` keeps its own
stalled flag and retries 10 s after each rebuild finishes. Never clear the
watchdog latch to get another edge: it is the TX hard mute's safety input.

A reopen result is not proof of recovery. Every rebuild publishes
`audio_output_alive = false`, the first counter read afterwards is only a
baseline, and only later callback progress unmutes TX.

The budget is three output-watchdog requests per rolling ten minutes. An
attempt stops counting at exactly 600 s. Only callback progress ends the
episode and clears the limit label; it keeps the history. An operator pick
with `d`, an input-stale reopen and a processing-error reopen neither charge
nor refill the budget; they only postpone the next automatic request, never
the limit label. An output still silent after a processing-error rebuild
counts as a stall through the watchdog's dead latch. The request goes through the audio thread's own reopen
FIFO, and the reply is polled with `try_recv` because the same thread drains it.

`audio_output_reopen_exhausted` is display-only. It changes the
`TxAudioDead` blocker's label and never gates TX. Stub, replay and
`--no-audio` never start the policy. Physical-device qualification is pending.
