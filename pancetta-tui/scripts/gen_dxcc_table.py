#!/usr/bin/env python3
"""
Generate a Rust prefix→DXCC-entity-name table from an AD1C BigCTY cty.dat file.

Usage:
    python3 pancetta-tui/scripts/gen_dxcc_table.py /path/to/cty.dat \
        > pancetta-tui/src/dxcc_table.rs

Source: https://www.country-files.com/ (free for amateur use).

cty.dat record format
---------------------
Each record ends with `;`. The HEADER line has 8 colon-separated fields:
  field 0 – entity/country name  (e.g. "Angola")
  field 7 – primary callsign prefix (e.g. "D2")
One or more CONTINUATION lines follow, listing comma-separated alias prefixes.

Each token may carry modifiers that must be stripped before use:
  (NN)  – CQ-zone override
  [NN]  – ITU-zone override
  <lat/lon> – lat/lon override
  {CC}  – continent override
  ~tz~  – time-zone offset
Leading `=` means an EXACT-CALLSIGN entry (e.g. =9M2/PG5M) — skip these.
Leading `*` on the PRIMARY prefix means "deleted DXCC" — keep the prefix but
  strip the `*`.

Collision policy: if two entities claim the same bare prefix, the entity for
which it is a PRIMARY prefix wins; otherwise first-encountered wins.
"""

import re
import sys
from typing import Optional

# Strip all modifier groups from a token.
# Order matters: strip innermost or overlapping constructs consistently.
_MODIFIER_RE = re.compile(
    r'\([^)]*\)'       # (CQ-zone)
    r'|\[[^\]]*\]'     # [ITU-zone]
    r'|<[^>]*>'        # <lat/lon>
    r'|\{[^}]*\}'      # {continent}
    r'|~[^~]*~'        # ~timezone~
)

def strip_modifiers(token: str) -> str:
    """Remove all modifier groups from a cty.dat alias token."""
    return _MODIFIER_RE.sub('', token).strip()


def parse_cty_dat(path: str):
    """
    Yield (entity_name, primary_prefix, [alias_prefixes]) for every record.

    The primary_prefix may start with '*' (deleted DXCC) or contain a '/'
    (sub-entity like '3D2/c'); callers decide what to do with those.
    """
    with open(path, encoding='latin-1') as fh:
        raw = fh.read()

    # Split on record-terminator ';', keeping only non-empty chunks.
    records = [r.strip() for r in raw.split(';') if r.strip()]

    for record in records:
        lines = record.splitlines()
        if not lines:
            continue

        header = lines[0]
        # Header has exactly 8 colon-separated fields.
        parts = header.split(':')
        if len(parts) < 8:
            # Malformed – skip.
            continue

        entity_name = parts[0].strip()
        primary_raw = parts[7].strip()

        # Collect continuation lines (everything after the header).
        continuation = ' '.join(lines[1:])
        # Split on commas to get individual tokens.
        raw_tokens = [t.strip() for t in continuation.split(',') if t.strip()]

        # Parse alias prefixes: skip '=exactcall' entries; strip modifiers.
        aliases: list[str] = []
        for tok in raw_tokens:
            if tok.startswith('='):
                continue   # exact-callsign override – not a prefix
            bare = strip_modifiers(tok).upper()
            if bare:
                aliases.append(bare)

        yield entity_name, primary_raw, aliases


def build_table(path: str) -> list[tuple[str, str]]:
    """
    Build a sorted (prefix, entity_name) list from cty.dat.

    Primary-prefix entries have priority over alias entries on collision.
    Slash-sub-entities (3D2/c, 3D2/r, VK0H …) use only exact-call aliases
    (which we skip), so their primary_prefix contains '/' — we skip those as
    well since the prefix is not a dial-up-able prefix and would shadow the
    parent entity.
    """
    # Two passes: first primaries (higher priority), then aliases.
    primary_map: dict[str, str] = {}   # prefix → entity from PRIMARY field
    alias_map:   dict[str, str] = {}   # prefix → entity from alias list

    for entity_name, primary_raw, aliases in parse_cty_dat(path):
        # Strip leading '*' (deleted DXCC marker).
        primary_bare = primary_raw.lstrip('*').strip()

        # Skip sub-entity records whose "prefix" contains '/'.
        # These are virtual sub-divisions (Conway Reef = 3D2/c) with no
        # dial-prefixes; their alias lines are all exact-call (=...) entries.
        if '/' in primary_bare:
            continue

        primary_upper = primary_bare.upper()
        if primary_upper and primary_upper not in primary_map:
            primary_map[primary_upper] = entity_name

        # Aliases: skip if already claimed as a primary.
        for alias in aliases:
            if alias in primary_map:
                continue   # primary wins
            if alias not in alias_map:
                alias_map[alias] = entity_name

    # Merge: primary takes precedence, then alias.
    merged: dict[str, str] = {}
    merged.update(alias_map)
    merged.update(primary_map)   # primary overwrites alias on collision

    # Sanity filter: reject any entry that looks like a full callsign
    # (contains a digit AND is longer than 4 chars with non-prefix chars).
    # A legitimate prefix never contains '/' after stripping.
    clean: dict[str, str] = {}
    for pfx, name in merged.items():
        if '/' in pfx:
            # Still a sub-entity or exact-call that slipped through.
            continue
        clean[pfx] = name

    # Sort by prefix length DESC then alphabetically so the consumer's
    # longest-prefix-wins scan works without caring about order,
    # but we produce a deterministic, human-readable output.
    sorted_entries = sorted(clean.items(), key=lambda kv: (-len(kv[0]), kv[0]))
    return sorted_entries


def escape_rust_str(s: str) -> str:
    return s.replace('\\', '\\\\').replace('"', '\\"')


def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} /path/to/cty.dat", file=sys.stderr)
        sys.exit(1)

    path = sys.argv[1]
    entries = build_table(path)

    print('//! AUTO-GENERATED from cty.dat (AD1C BigCTY, https://www.country-files.com/, free for amateur use).')
    print('//! Do not edit by hand — regenerate with:')
    print('//!   python3 pancetta-tui/scripts/gen_dxcc_table.py /path/to/cty.dat \\')
    print('//!       > pancetta-tui/src/dxcc_table.rs')
    print('//!')
    print('//! Maps callsign prefix -> DXCC entity name.')
    print('//! Entries are sorted by prefix length DESC then alphabetically,')
    print('//! which enables a simple longest-first linear scan.')
    print()
    print('pub static PREFIX_TABLE: &[(&str, &str)] = &[')
    for pfx, name in entries:
        print(f'    ("{escape_rust_str(pfx)}", "{escape_rust_str(name)}"),')
    print('];')

    print(f'\n// Total: {len(entries)} entries', file=sys.stderr)


if __name__ == '__main__':
    main()
