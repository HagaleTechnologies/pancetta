# Station Agent P3.4d — reconcile to frozen `txArm`/`txEnabledUntil` contract

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps `- [ ]`.
> SECURITY-CRITICAL. TDD, fail-closed. `cargo fmt --all` + `cargo clippy` + tests in the FOREGROUND before committing. Never push (controller pushes). Never use `subagent_type: fork`.

**Goal:** Reconcile P3.4b's provisional "capabilityToken-embedded-in-grant" shortcut to the now-**frozen**
`e2e-auth.v1` shapes (dispensa `03bac8b`, Q-0015 resolved): the `txArm{capabilityToken, grant}` sibling
frame, the `txDisarm{armJti}` frame, the `txEnabledUntil` clock-2 enablement gate, the arm-time best-effort
deny-list, and the short-TTL backstop. Offline, behind the default-OFF agent.

**Frozen contract facts** (from `/Users/thagale/Code/dispensa/contracts/auth/e2e-auth.v1.schema.json`):
- `$defs.txArm` = `{type:"txArm", capabilityToken:<compact JWS>, grant:<$defs.txArmGrant>}` — **siblings**; `clientSig` signs ONLY the canonical `txArmGrant` (which references the token via `capabilityJti`).
- `$defs.txDisarm` = `{type:"txDisarm", armJti}` — `armJti` is a **sanity match** to the current arm, NOT a security gate; disarm-any is fail-safe TX-OFF.
- `capabilityToken.txEnabledUntil` (optional, **epoch SECONDS**): **absent ⇒ NOT tx-enabled → station MUST refuse to arm** even with `tx` scope (status/qsy unaffected); present ⇒ arm honored only if `txEnabledUntil > now`; the enabled token's `exp == txEnabledUntil` (≤ `MAX_ENABLEMENT` = 24h). Never client-asserted.
- **6-revocation:** non-enabled token ⇒ agent REJECTS `exp − iat > 900s` (short-TTL backstop); enabled token skips the 900s check (its longer life is intentional) but is deny-list-checked at arm time (best-effort). Station-local allow-list removal + Shift+Q remain the authoritative revoke.
- **Agent verification order** (bolded additions): verify capabilityToken (JWS vs pinned IdP; aud==agentKeyId; exp>now; scope⊇tx; **short-TTL if non-enabled**) → **txEnabledUntil present AND >now** → **jti not on best-effort deny-list** → verify grant.clientSig → grant.capabilityJti==token.jti AND grant.clientKeyId==token.clientKeyId → station-local allow-list → bounds (armedUntil≤10min, hb 5-15s) → grant.jti single-use → arm.

**Spec:** the Q-0015 concurrence in dispensa + the frozen `e2e-auth.v1`. Builds on P3.4c (main).

**Branch:** `feat/station-agent-p3d` off main (already checked out).

**Regression invariant:** default-OFF agent stays inert; no live-path change outside the agent crate + `station_agent`. Existing P3.4b/c tests updated to the new frame shapes, not weakened.

---

## Task 1 — `txArm`/`txDisarm` frame shapes (`control.rs`)
- [ ] Change `ControlAction::Arm { grant }` → `Arm { capability_token: String, grant: serde_json::Value }`. `map_client_frame` maps a `{type:"txArm", capabilityToken, grant}` inner frame → `Arm{capability_token, grant}` (token + grant as SIBLINGS; do NOT read the token from inside the grant). A `txArm` missing `capabilityToken` or `grant` → `Err` (malformed), NOT `Unsupported` (a partial arm must fail-closed). Keep the `setTransmitArmed{armed:true}`→`Unsupported` rule.
- [ ] Change `ControlAction::Disarm` → `Disarm { arm_jti: String }`; map `{type:"txDisarm", armJti}` → `Disarm{arm_jti}`. (Keep `setTransmitArmed{armed:false}` → `Disarm` too, if a client uses it — but that has no armJti; use `Disarm{arm_jti: String::new()}` or make arm_jti `Option` — your call, but `txDisarm` carries it.)
- [ ] Update the control tests for the new `txArm`/`txDisarm` shapes (a valid txArm → Arm{token,grant}; missing token → Err; txDisarm → Disarm{arm_jti}).
- [ ] Commit `feat(agent): txArm/txDisarm inner-frame shapes (capabilityToken + grant siblings; frozen e2e-auth.v1)`.

## Task 2 — `txEnabledUntil` + short-TTL + deny-list (`capability.rs`)
- [ ] `VerifiedCapability` gains `pub tx_enabled_until: Option<i64>` (epoch **seconds**, parsed from the token). `verify_capability_token` parses `txEnabledUntil` + `iat`.
- [ ] **Short-TTL backstop** in `verify_capability_token`: if `txEnabledUntil` is ABSENT and `(exp − iat) > 900` → `Err(TtlTooLong)` (non-enabled tokens must be short-lived). If `txEnabledUntil` is PRESENT: require `exp == txEnabledUntil` (→ `Err(EnablementMismatch)` if not) and `(txEnabledUntil − iat) ≤ 86_400` (24h `MAX_ENABLEMENT`, → `Err(EnablementTooLong)`). (Do NOT apply the 900s check to enabled tokens.)
- [ ] `verify_arm_grant` (or a small `check_tx_enabled` the dispatch calls right after token verify, matching the verification order): require `capability.tx_enabled_until.is_some_and(|t| t > now_secs)` → else `Err(NotTxEnabled)`. This is the clock-2 gate: no enablement ⇒ never arm (status/qsy unaffected because they don't call this).
- [ ] Add an arm-time **best-effort deny-list** param to `verify_arm_grant`: `revoked_jtis: &HashSet<String>` — if `capability.jti ∈ revoked_jtis` → `Err(Revoked)`. Empty set (offline / no known revocations) ⇒ inert (does NOT block) — the station-local allow-list is the authoritative revoke. Order it per the spec (after txEnabledUntil, before clientSig).
- [ ] Adversarial tests: token with NO `txEnabledUntil` → `NotTxEnabled` at arm (but base verify still OK for status/qsy); `txEnabledUntil` in the past → `NotTxEnabled`/expired; non-enabled token with `exp−iat = 901s` → `TtlTooLong`; enabled token with `exp != txEnabledUntil` → `EnablementMismatch`; enabled token 25h → `EnablementTooLong`; a jti in the deny-list → `Revoked`; empty deny-list → not blocked. Existing P3.2 adversarial tests updated (add `txEnabledUntil` to the valid fixtures so they still pass).
- [ ] Commit `feat(agent): txEnabledUntil clock-2 gate + short-TTL backstop + arm-time best-effort deny-list (fail-closed)`.

## Task 3 — dispatch sources token+grant separately; txDisarm armJti (`station_agent/mod.rs`)
- [ ] `dispatch_action` `Arm{capability_token, grant}`: `verify_capability_token(&capability_token, now)` → `verify_arm_grant(&grant, &capability, client_vk, now, &mut seen_jtis, &tx_allow_list, &revoked_jtis)` (thread the token as a SEPARATE input, NOT extracted from the grant). Seed `revoked_jtis` from an in-memory set (empty in v1 — a future cqdx-fed deny-list populates it; document the seam). On Ok → `arm(verified, now)`; any Err → audit `TxDenied{reason}`, never arm.
- [ ] `Disarm{arm_jti}`: `remote_tx_arm.lock().disarm(now)` (fail-safe TX-OFF); if `arm_jti` is non-empty and doesn't match the current arm's jti, still disarm but log a `warn!` "txDisarm armJti mismatch (disarming anyway)" — armJti is a sanity check, not a gate.
- [ ] Update the e2e/station_agent tests: the `Arm` path now feeds a real `txArm` frame (capabilityToken sibling + grant), with a `txEnabledUntil`-present enabled token → arms; a token WITHOUT txEnabledUntil → does NOT arm (new negative); txDisarm{armJti} disarms.
- [ ] `cargo test -p pancetta-agent -p pancetta`; full `cargo check --workspace --all-targets --features transmit` FOREGROUND. Commit `feat(coord): station-agent txArm(token+grant siblings) + txEnabledUntil at arm + txDisarm armJti sanity`.

## Task 4 — drift-guard the frozen $defs + docs + land
- [ ] A serde round-trip test that pancetta's `txArm`/`txDisarm` types match the frozen `$defs` field names (camelCase `capabilityToken`/`armJti`, `type` consts) — vendor the relevant `$defs` snippet or assert the exact JSON. (If a full schema-validate is heavy, a field-name round-trip is enough — the goal is to catch drift.)
- [ ] CLAUDE.md: update the station-agent bullet — the provisional-embed note is resolved to the `txArm` sibling frame; `txEnabledUntil` clock-2 gate + short-TTL + best-effort deny-list live; `txDisarm.armJti`. Remove the "PROVISIONAL — Q-0014" caveat.
- [ ] Final adversarial security-review subagent (focus: no-enablement ⇒ never arm; token+grant verified as separate inputs [not token-from-grant]; deny-list fail-closed on membership but inert when empty; short-TTL enforced for non-enabled; still no arm without client key + allow-list; default-OFF inert).
- [ ] fast gate → PR → **wait CI green** → merge; sync main. Update dispensa (Q-0015 pancetta implemented; drift-guarded).

---
## Self-review
- **Clock-2 enforced:** absent/expired `txEnabledUntil` ⇒ station refuses to arm (status/qsy unaffected).
- **Siblings, not embedded:** the capabilityToken is verified as a separate input; `clientSig` still signs only the canonical `txArmGrant`.
- **Deny-list best-effort; allow-list authoritative:** empty deny-list never blocks; local removal always wins.
- **Short-TTL backstop** for non-enabled tokens; enabled tokens bounded to 24h with `exp==txEnabledUntil`.
- **Default-OFF inert;** existing tests updated to the frozen shapes, not weakened.
