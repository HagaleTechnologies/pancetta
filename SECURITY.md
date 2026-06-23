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

Pancetta is pre-1.0. Only the `main` branch is currently maintained for
security updates. Tagged releases will be added to this table once they exist.

## Known Security Considerations

These are intentional trade-offs in the current codebase, documented so users
can make an informed deployment choice:

### Plaintext Credentials in Config Files

`pancetta-config` accepts passwords for QRZ, LoTW, eQSL, Clublog, and HTTP
proxies in `~/.pancetta/pancetta.toml`. **These values are stored as plaintext
on disk** (the single-operator station trust model — the same posture as
WSJT-X's `.ini`). The fields were previously misnamed `*_encrypted`; that name
implied encryption that does not exist.

Mitigations:

- **Pancetta now writes this file owner-only (`0600`) and its `~/.pancetta`
  directory `0700` automatically, atomically (temp + rename).** This holds even
  when the file is rewritten (e.g. choosing an audio device in the TUI), which
  previously reverted a hand-applied `chmod 600`. You no longer have to harden
  it yourself on Unix; on Windows the per-user profile ACL applies.
- Never commit the config file to a shared repo (it lives in `~/.pancetta`,
  outside the source tree).
- Prefer leaving these fields unset and supplying credentials at runtime via
  environment variables when the integration supports it.
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
to the relevant ALSA / PulseAudio device. Pancetta does not transmit audio
contents anywhere off the local machine — audio is consumed by the FT8
decoder and discarded.

### `unsafe` Code

The workspace contains `unsafe` blocks in:

- `pancetta-ft8` — SIMD intrinsics and FFI to the bundled `ft8_lib` C
  decoder, contained in dedicated modules.
- `pancetta-hamlib/src/bindings.rs` — currently dead-code FFI stubs for
  libhamlib; not invoked by the active rigctld path. Slated for removal.

Each `unsafe` block has a justification comment. Independent review of the
SIMD and FFI boundaries is welcome.

## Coordinated Disclosure

We follow standard coordinated disclosure: report privately, allow time to
fix, then publish the advisory together with the patched release. Credit
will be given to reporters who request it.
