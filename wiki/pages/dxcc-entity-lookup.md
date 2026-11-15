---
id: dxcc-entity-lookup
title: What will bite you about the DXCC entity resolver?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta-core/src/dxcc.rs
  - pancetta-core/src/dxcc_table.rs
  - pancetta-core/scripts/gen_dxcc_table.py
  - pancetta-tui/src/app.rs
  - pancetta-tui/src/dx_need.rs
verified:
  commit: ebd09fa6
  date: 2026-11-08
links:
  - tui
---
The offline callsign to DXCC entity resolver is `pancetta_core::dxcc`
(PAN-284). Its prefix index answers match the old linear scan. Exact-call
and KG4 layers take precedence (PAN-346).

## Never resolve inside a frame

Each DX Hunter row (`DxStation`) carries `offline_entity`, resolved once when
the row is created. Read it through `DxStation::display_entity`, which puts
cqdx's `entity_name` first. `DxNeedLookup::lookup_need` receives the row's
entity as a parameter: a provider must use it and must not resolve the
callsign again. A lookup in the renderer or the need refresh runs for every
row in every frame.

## The US safety net is reachable

cty.dat lists `AH0` to `AH9` but no bare `AH`. So `AH`, `AHA` and `AH` plus
any non-digit fall through the table to the K/W/N and AA to AL safety net and
answer "United States". Do not delete the safety net as dead code.

## Regenerating the table

The generator lives in `pancetta-core/scripts/gen_dxcc_table.py`:
`python3 pancetta-core/scripts/gen_dxcc_table.py /path/to/cty.dat >
pancetta-core/src/dxcc_table.rs`, then run
`cargo test -p pancetta-core --lib -- dxcc::`. A duplicate prefix would be
silently collapsed by the `HashMap`, so `index_has_one_key_per_table_row`
fails on it. The index computes its longest key when built, so longer prefixes
need no code change.

## Keep normalization as it is

The lookup uppercases with `to_uppercase`, not `to_ascii_uppercase` (`ß`
becomes `SS`), resolves FT8 hash-renders (`<N7RLK>`) first, and slices with
`str::get` so a non-ASCII first character cannot panic.

## Identity and exact rules (PAN-346)

The registry maps primary prefixes and names to explicit ADIF IDs. European
and Asiatic Turkey share 390; African Italy, Sicily and Italy share 248;
Shetland and Scotland share 279; Bear Island and Svalbard share 259;
Vienna Intl Ctr and Austria share 206. Display names remain separate.

Full normalized exact calls win over the KG4 rule and prefix layer, including
slash exact calls. Bare KG4 and KG4 plus two ASCII letters are Guantanamo;
other nonempty KG4 suffixes are USA. The rule examines the leading component
before /P. No hypothetical KG4AB override is added to production data.

The pinned cty input, explicit primary-prefix ID map and ADIF enumeration
live under pancetta-core/data. Regenerate registry and exact output with
`python3 pancetta-core/scripts/gen_dxcc_table.py --supplemental`. This leaves
the base prefix table unchanged.
Generated headers record the input version and SHA-256.

## Zone data and ADIF enrichment (PAN-306)

`zones_for_callsign` uses the same normalization and selected entity as the
entity resolver. Full zone exact calls come first, then KG4 header defaults,
then the longest zone prefix. Mismatching entities fail closed for zones,
including a mismatching longest prefix; a broader prefix is never substituted.
Only the existing US safety net without a prefix uses US header defaults.

Regenerate `dxcc_zones.rs` with `python3 pancetta-core/scripts/gen_dxcc_table.py
--supplemental` and run `python3 -m unittest discover -s pancetta-core/scripts
-p 'test_gen_dxcc_table.py' -v`. That mode preserves the shipped base prefix
table. The old supplemental outputs must stay byte-identical for this change.
The new data retains same-entity exact calls that entity-only generation prunes:
N2NL/MM needs CQ 7 even though its entity remains United States. K0ABC uses
4/7 instead of the US header's 5/8. Do not merge contest subdivisions by ADIF ID.

ADIF inference runs in `qso_to_adif`, never the renderer or a TUI frame.
Imported tags suppress inference per field; historical files stay untouched
until an explicit export. Tests cover these paths without a rig or account.
