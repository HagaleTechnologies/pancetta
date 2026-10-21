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
Remote TX through the station agent (`[network.station_agent]`) needs local
consent (`remote_tx_enabled`) and a signed arm from an allow-listed client.
Both sections are described in
[docs/CONFIG.md](docs/CONFIG.md).

### `unsafe` Code

Production code contains `unsafe` in three files:

- `pancetta-ft8/src/ft8_lib_ffi.rs` — FFI to the bundled `ft8_lib` C
  decoder. The C struct layouts are pinned by compile-time size asserts, and
  decoded text is read only within a bounded buffer.
- `pancetta/src/runtime.rs` — the Linux `setpriority` call.
- `pancetta-agent/src/arm.rs` — `clock_gettime`.

Only the `arm.rs` site carries a `SAFETY:` comment. The FFI module has no
per-block justification comments; its safeguards are the layout asserts and
the bounded-buffer text reader described above. The `runtime.rs` call has no
justification comment either. `pancetta-protocol` forbids
`unsafe_code` and `pancetta-qso` denies it. Independent review of the FFI
boundary is welcome.

## Coordinated Disclosure

We follow standard coordinated disclosure: report privately, allow time to
fix, then publish the advisory together with the patched release. Credit
will be given to reporters who request it.
