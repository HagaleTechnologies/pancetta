# Local gateway authentication (PAN-99)

## Context

The default-off local gateway provides Panino's `?gateway=` transport with a read-only station view. This design adds admission control to that leg. Dispensa ADR-0009 defines the gateway profile separately from the relay-connected station agent.

## Admission sequence

Check the browser Origin before upgrading. Cap both inbound frames and messages at 64 KiB. Wait up to 5 seconds for the first text frame, which must be `hello` with a `gatewayToken` matching the configured secret. Compare token bytes with `subtle::ConstantTimeEq`. Ping and pong frames do not extend the deadline. A binary frame, close, or transport error ends admission. Only after authentication subscribe to live events, read the snapshot, and send `Welcome`.

## Browser Origin policy

A missing Origin permits native clients to proceed to token authentication. A present Origin must match an `allowed_origins` entry exactly, ignoring ASCII case. The empty default list allows no browser pages. `null` is never allowed. Entries are bare `http://` or `https://` origins with a host and optional port, no path, query, fragment, credentials, whitespace, or trailing slash.

## Bind policy

Parse `bind_addr` as `std::net::SocketAddr`, without DNS. Loopback means `ip.is_loopback()`, including 127.0.0.0/8 and ::1. Other addresses, including unspecified and IPv4-mapped IPv6 addresses, require `allow_network_exposure = true` and a token. Network exposure uses unencrypted `ws://`; TLS remains deferred.

The CLI reports these validation results:

```text
# enabled = true, no auth_token (or shorter than 32 chars, or containing whitespace)
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.auth_token must be set to a secret of at least 32 characters with no whitespace when the gateway is enabled (generate one with: openssl rand -hex 32)
exit 1

# enabled = true, auth_token set, bind_addr = "0.0.0.0:4080", allow_network_exposure = false
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.bind_addr 0.0.0.0:4080 is not a loopback address, so station and contact data would be served unencrypted to the network; bind to 127.0.0.1 or [::1], or set network.remote_gateway.allow_network_exposure = true to accept that
exit 1

# enabled = true, bind_addr = "localhost:4080"
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.bind_addr "localhost:4080" must be an IP address and port, e.g. "127.0.0.1:4080" or "[::1]:4080"
exit 1

# enabled = true, allowed_origins = ["https://cqdx.io/app"]
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.allowed_origins entry "https://cqdx.io/app" must be a bare browser origin: http:// or https:// followed by a host and optional port, with no path, query or trailing slash
exit 1

# enabled = true, auth_token set, bind_addr = "0.0.0.0:4080", allow_network_exposure = true
Configuration validation: PASS
exit 0
```

## Configuration

```toml
[network.remote_gateway]
enabled = true
bind_addr = "127.0.0.1:4080"          # IP:port only; loopback unless allow_network_exposure
auth_token = "9f2c…64 hex chars…e1"   # required when enabled; ≥ 32 chars; generate: openssl rand -hex 32
allowed_origins = []                  # browser pages allowed to connect, e.g. ["http://localhost:8081"]
allow_network_exposure = false        # true = permit a non-loopback bind_addr (plain ws://, unencrypted)
```

`enabled` defaults to false; disabled sections are not validated. `auth_token` has no default, must contain at least 32 characters, and cannot contain whitespace or control characters. Generate it with `openssl rand -hex 32` and protect the config with `chmod 600`. `allowed_origins` defaults to empty. `allow_network_exposure` defaults to false. All gateway configuration changes require a restart.

## Failure behaviour

Reject a browser Origin with HTTP 403 and body `origin not allowed`. Authentication failures close with 1008 and reason `authentication required`. Do not send station data before authentication. Log the local cause without logging tokens or inbound text. Validation failure at component start logs `remote_gateway not started: <validation error>` and returns without a listener.

A loopback listener logs INFO `remote_gateway listening on ws://<local>/ws (read-only, loopback only, clients must authenticate)`. A network listener logs WARN `remote_gateway listening on ws://<local>/ws (read-only, NETWORK-EXPOSED by allow_network_exposure, unencrypted, clients must authenticate)`. Use the actual local address, including an assigned port.

Origin refusal logs WARN `remote_gateway refused a browser connection from origin <origin> (not in allowed_origins)`. Admission refusal logs WARN `remote_gateway closed a connection that did not authenticate (<reason>)`. Reasons are `no hello in time`, `first frame was not a hello`, `hello carried no gatewayToken`, `gatewayToken did not match`, and `client closed before hello`.

## Alternatives considered

Browser WebSocket APIs cannot set an Authorization header. URL tokens can enter histories and HTTP logs. A custom subprotocol credential would require nonstandard echo handling. An operator-configured shared secret avoids portable token-file permission management; per-client scopes are unnecessary for this read-only view. A vetted constant-time comparator avoids handwritten cryptography. Implicit localhost Origin permission would admit arbitrary local pages; CORS does not govern WebSocket upgrades. Hostnames require DNS resolution, so bind addresses are IP literals. Disabled sections remain loadable. Standard Close 1008 avoids another wire frame variant. A 64 KiB limit suits the small hello payload. TLS certificate management is deferred to separate work.

## Rollout

Enforce these requirements on release. An enabled gateway requires `auth_token` at startup. Clients must send `hello.gatewayToken` within 5 seconds. Panino's `?gateway=` mode cannot connect until its WebSocketTransport does so. Production relay and station-agent authentication are unchanged. `PROTOCOL_VERSION` remains 1; token-less hellos retain their existing serialized shape.

The PR merges only after the following dispensa question is filed. The cloud phase has access only to this repository, so the proposal is recorded here before code changes. Filing is a merge prerequisite, not an implementation gate.

## Proposed dispensa question

Target: `dispensa/questions/Q-NNNN-local-gateway-hello-token.md` (assign number when filed).

Status: Open
From: pancetta
To: panino
Affects: contracts/rig/rig-api.v1.schema.json

PAN-99 proposes the following additive rig-api.v1 amendment for concurrence:

1. Add optional `hello.gatewayToken`, a string used only on the local-gateway leg and never sent over the relay. Leave the protocol version at 1 and omit the key when absent.
2. On the local-gateway leg, send no station data until the first hello carries a matching gatewayToken within 5 seconds. Otherwise close with code 1008, `authentication required`.
3. Have Panino's WebSocketTransport send that hello first, using a token supplied by the operator. Browser dev-server origins must also be explicitly allowed in station configuration.
