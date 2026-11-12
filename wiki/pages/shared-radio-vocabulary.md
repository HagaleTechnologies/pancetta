# What will bite you about the shared radio vocabulary?

PAN-330 moved the station's shared radio-domain vocabulary into `pancetta-core`,
so several old private copies are gone. If you are adding code that classifies a
token or a frequency, use the core owner instead of writing another table.

## Grid shape versus grid validity

Two different jobs, two different checks, and mixing them up is the classic bug:

- **Shape** — `pancetta_core::callsign::is_grid_shape` answers "is this token
  shaped like a 4-character Maidenhead square?" It deliberately accepts `ZZ99`,
  because false-positive detection needs to *recognize* a grid-shaped token
  before rejecting it as semantically impossible.
- **Validity** — `pancetta_core::gridsquare::validate_grid` answers "is this a
  real Maidenhead locator?" Field letters are `A`–`R`, subsquares `A`–`X`, case
  is normalized, even lengths 2–10 are supported. `ZZ99` and `FN42YY` are
  invalid; `RR73` is *geographically* valid (field `RR`, square `73`).

Callers pin their own precision and case on top of the core validator as small
adapters: exchange accepts uppercase 4/6 characters, contest matching uppercase
4, autonomous extraction 4/6 case-insensitive, statistics 4/6/8, station
configuration 4/6/8 with conventional uppercase-field/lowercase-subsquare
presentation. FT8 close handling reserves `RR73` explicitly — a semantic
validator alone cannot tell an acknowledgement from a locator.

## CQ modifiers: decoded versus packable

`pancetta_core::ft8_tokens` owns two named policies:

- `is_packable_cq_modifier` — what the FT8 *encoder* can pack: 1–4 ASCII
  uppercase letters, or exactly three digits. A packing constraint, unchanged
  from the encoder's old private rule.
- `is_decoded_cq_modifier` — what a *decoded* free-text CQ line may carry: a
  named whitelist (`DX`, `NA`, `QRP`, `POTA`, …) plus 1–3 letters or digits,
  ASCII case normalized. Empty is not a token; modifier absence is the caller's
  decision.

They deliberately diverge: `12` is a decoded CQ-zone modifier but is not
directly packable, while `ABCD` packs but is not a decoded whitelist token.
Tests pin both directions.

## One band table, one ADIF case adapter

`Band::from_frequency` with `Band::frequency_range`'s inclusive Hz edges is the
only membership table; `Band::name()` is the canonical lowercase label that
Display, parsing and serde all use. DSP recording labels, cqdx and hamlib
delegate to it. Known consequences at the 60 m edge (5,330,000–5,405,000 Hz):

- 5,410,000 Hz is no band everywhere (DSP's old final-kHz bucket and cqdx's old
  5–6 MHz bucket both said `60m`).
- ADIF keeps uppercase labels (`60M`) as an explicit format adapter over the
  core table, and keeps its rounded-MHz fallback (`5MHZ`) outside it — so
  5,405,000 Hz now writes `60M` instead of `5MHZ`, and 5,300,000 Hz writes
  `5MHZ` instead of `60M`.

## Callsign identity and tokens

`callsigns_match` is the identity predicate (compound forms, resolved hash
renders); `decode_directed_at_us` is the relay-facing wrapper both coordinator
paths use — `EA8/K5ARH DL1ABC -10` is directed at `K5ARH`. `is_callsign_token`
is the shared "is this decoded token callsign-shaped" check (3–11 chars, ASCII
alphanumeric plus `/`, a digit and a letter, no empty slash components, not a
bare grid); scoring keeps the stricter `is_plausible_callsign`. The busy
detectors classify payloads through `ft8_tokens::is_exchange_payload` and party
identity through `callsigns_match`, so our own portable-form traffic is not
marked a third-party exchange.
