# What will bite you about shutdown PTT-off?

A successful MessageBus::send_message to Hamlib proves an enqueue only. The
channel table retains a receiver clone even after the command loop exits.
The signal handler sets the shutdown flag before shutdown() runs, so the loop
must drain unkeys after the flag. The coordinator stops the TX producer and
settles its fallback before telling the drain to stop. New key-downs are dropped.

RigctldClient::connect verifies the connection with f under command_timeout_ms.
An outer timeout alone cannot help a slow rig. Shutdown uses 3000 ms per reply
and a 6 s overall direct budget concurrently with the drain.

See [the PAN-277 decision](../../docs/DECISIONS/tx-scheduling.md#shutdown-unkeys-through-the-hamlib-loop-pan-277-2026-11-10).
