# Real TX worker fixture

Run both consumers explicitly:

```sh
cargo test -p pancetta --features test-harness --test coord_sim --test tx_ptt_integration
```

The default build does not compile the internal testing module or these targets.
The full check script and CI run them separately.

Each CoordSim owns a temporary station home, a QSO manager and the production
transmitter worker. The manager and worker share the coordinator's atomics,
active sets, offset maps, relocation epochs and PTT synchronization gate.
Requests carry real FT8 text, origin, client identity and attempt identity.
The adapter records latest intent only after bus acceptance.

The mock rig consumes actual worker commands and records every ordered PTT
edge. The audio consumer verifies nonempty waveforms and acknowledges playback
start and flush. Durable outcomes drive the production QSO settlement handlers.
A refusal is recorded as WorkerRefused: the wire outcome does not carry a
reason, so the fixture does not reconstruct one by repeating worker gates.

Slot labels in the timeline are scenario labels. Scheduling and playback use
real UTC and FT8 durations. A frame has a 45-second settlement deadline.
Tests wait for observed audio or settlement rather than using a sleep to infer
worker progress. The arm clock is injected separately and does not change UTC.

Every scenario calls async shutdown. Shutdown joins the worker, settles its
fallback PTT-off, drains and joins consumers, then asserts the rig is off.
Drop requests shutdown as a failure-path backstop.

To check supersede sensitivity, first run
`supersede_during_playback_unkeys_before_rekey` alone. Back up
`pancetta/src/coordinator/tx.rs` outside the checkout. Within
`supersede_and_rekey_or_bundle` only, replace
`ptt_guard.release("Supersede").await;` with `ptt_guard.disarm();`.
Run that same test and require failure at the PTT sequence assertion, showing
the missing middle off edge. Restore the source and require the test to pass
again, then repeat the restored test three times. Never retain the mutation or
re-bless the abort golden.
