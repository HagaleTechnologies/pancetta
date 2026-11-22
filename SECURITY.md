# Security Policy

## Reporting a Vulnerability

If you believe you have found a security vulnerability in Pancetta, please report
it privately rather than opening a public GitHub issue.

**Email:** tony@hagale.net

Please include:

- A description of the issue and the affected component(s)
- Steps to reproduce, ideally with a minimal proof of concept
- The version / commit hash you reproduced against
- Any logs, screenshots, or stack traces that help

You should expect an initial acknowledgement within **3 business days** and a
status update within **14 days**. For confirmed issues, we will coordinate a
fix and a disclosure timeline with you.

## Supported Versions

Pancetta is pre-1.0. Only `main` receives security fixes; they ship in the
next tagged release. Earlier tags (v0.9.5, v0.9.6) are not patched.

## Known Security Considerations

These are intentional trade-offs in the current codebase, documented so users
can make an informed deployment choice:

### Plaintext Credentials in Config Files

`~/.pancetta/pancetta.toml` can hold these secrets, and **all of them are
stored as plaintext on disk** (the single-operator station trust model — the
same posture as WSJT-X's `.ini`):

- the ClubLog password and API key;
- the QRZ password and the QRZ Logbook API key;
- the eQSL password;
- the cqdx.io token;
- `[network.remote_gateway].auth_token`;
- the HTTP proxy password and the TLS key password.

LoTW stores no credential here: TQSL holds your certificate, and Pancetta only
records the `tqsl` path and station location. The unused `[network.web_api]`
section still has fields named `key_encrypted` and `secret_encrypted`; nothing
encrypts them. No running component reads `[network.web_api]` or
`[network.proxy]`.

Mitigations:

- **Pancetta now writes this file owner-only (`0600`) and its `~/.pancetta`
  directory `0700` automatically, atomically (temp + rename).** This holds even
  when the file is rewritten (e.g. choosing an audio device in the TUI), which
  previously reverted a hand-applied `chmod 600`. You no longer have to harden
  it yourself on Unix; on Windows the per-user profile ACL applies.
- Never commit the config file to a shared repo (it lives in `~/.pancetta`,
  outside the source tree).
- A future release may add OS-keyring lookup; until then, treat the config
  file as you would `~/.netrc`.

### `rigctld` Network Surface

`pancetta-hamlib` connects to `rigctld` over TCP, defaulting to
`127.0.0.1:4532`. If you bind `rigctld` to a non-loopback interface, anyone
who can reach the port can drive your transceiver. Keep `rigctld` on
loopback unless you have a specific reason otherwise.

### Audio Device Access

Pancetta opens audio input and output streams via cpal. On macOS this
triggers a Microphone permission prompt; on Linux it requires read access
to the relevant ALSA / PulseAudio device. Raw audio never leaves the
machine. When remote operation is enabled, decoded messages and waterfall
magnitudes are sent to authorized clients.

### Remote Operation

Both remote channels are off by default. The local gateway
(`[network.remote_gateway]`) requires `auth_token`. It accepts browser pages
only from origins in `allowed_origins`, which defaults to an empty list, and
binds a non-loopback address only with `allow_network_exposure = true`.
Both sections are described in
[docs/CONFIG.md](docs/CONFIG.md).

The station agent (`[network.station_agent]`) has two trust roots.

- **Transmit is rooted on the station.** A remote client can arm TX only
  after you store its device key with `pancetta trust-client`
  (`<key_dir>/clients/<keyId>.pub`), and only while `remote_tx_enabled` is
  on. The arm must also carry the client's signature and a TX-enabled
  cqdx.io token. Cloud data alone cannot arm TX.
- **Connection and non-TX control are rooted in cqdx.io when the cqdx
  integration is on.** Turning on `[network.cqdx]` with a token, the same
  setting that turns on spots and logbook upload, makes cqdx.io's
  authorization list the station agent's admission list. Any key cqdx.io
  lists for your station can connect with no step on the station, and
  cqdx.io signs the token that grants its `status` or `qsy` scope. Without
  the cqdx integration only keys in `tx_allow_list` can connect.

The project accepts the residual risk (decision of 2026-11-07). Anyone who
can add an authorization for your station on cqdx.io, including an attacker
who compromises cqdx.io, can connect a key they hold, read your decodes and
waterfall, retune or re-split the rig while it is not transmitting and no
station-run QSO is active, stop a CQ, and take control from your own remote
client, which disarms it. While `remote_tx_enabled` is on, that key can also
queue QSO requests, which cannot transmit without an arm. A cqdx.io
compromise alone cannot make the station transmit, because that also needs a
device key you trusted on the station.

With the cqdx integration on, revoke a client's admission by removing its
authorization on cqdx.io. Removing it from `tx_allow_list` evicts it at
once, but the next successful poll re-admits it while cqdx.io still lists
it. Without the integration, remove the client from `tx_allow_list`.
`pancetta untrust-client` and `remote_tx_enabled = false` remove TX only.
Setting `[network.station_agent] enabled = false` cuts off all station-agent
remote access; the local gateway is a separate channel. Details:
[remote-operation decisions](docs/DECISIONS/remote-operation.md).

### `unsafe` Code

Production Rust unsafe blocks occur in these six files:

- `pancetta-ft8/src/ft8_lib_ffi.rs`: bindings to the bundled C decoder,
  compiled when `not(ft8lib_stub)`. C layouts have compile-time size asserts;
  text reads are bounded. A private `MonitorGuard` uniquely owns each initialized
  monitor and calls `monitor_free` on normal return, early return and Rust unwind.
  It cannot recover from C allocation failure, abort or process termination.
- `pancetta/src/runtime.rs`: Linux `setpriority`.
- `pancetta-agent/src/arm.rs`: Linux, Android and Apple `clock_gettime` for the suspend-aware clock.
- `pancetta/src/responsibility.rs`: macOS responsibility lookup and process relaunch.
- `pancetta/src/mic_access.rs`: macOS Objective-C microphone authorization calls.
- `pancetta/src/doctor.rs`: macOS Objective-C microphone authorization lookup.

The FT8 and main library and binary roots warn on
`clippy::undocumented_unsafe_blocks` and deny it when compiled by Clippy.
The Clippy-only deny prevents the crate warning from overriding CI's deny flag.
CI checks all targets with
`-D clippy::undocumented_unsafe_blocks`. Each unsafe block has a `SAFETY:`
explanation. macOS-only sites also require native macOS checking; a Linux
Clippy run does not compile them. Unsafe callback types and the explicit
`unsafe extern "C"` declaration in the binding are declarations, not blocks.

Test-only C encoder calls occur in `pancetta-ft8/src/encoder.rs`,
`pancetta-ft8/tests/dxpedition_type01.rs` and
`pancetta/src/coordinator/qso.rs`. Process-control tests also use unsafe
in `pancetta/src/responsibility.rs` and `pancetta/tests/shutdown_signals.rs`.
The FFI module also tests valid owned
waterfall reads. The accuracy helper uses an atomic LCG without unsafe.
`pancetta-research/src/tier_slots.rs` uses documented Unix `flock` calls;
that local research harness is excluded from CI.

`pancetta-tui`, `pancetta-config`, `pancetta-dx`, `pancetta-cqdx`,
`pancetta-dsp`, `pancetta-hamlib` and `pancetta-core` forbid `unsafe_code`,
as does `pancetta-protocol`. `pancetta-qso` denies it. These policies apply
to those libraries, not the bundled C code or all transitive dependencies.
Source inventory searches classify executable code separately from comments,
strings, callback types and extern declarations.

## Coordinated Disclosure

We follow standard coordinated disclosure: report privately, allow time to
fix, then publish the advisory together with the patched release. Credit
will be given to reporters who request it.
