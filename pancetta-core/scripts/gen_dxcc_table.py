#!/usr/bin/env python3
"""
Generate a Rust prefix→DXCC-entity-name table from an AD1C BigCTY cty.dat file.

Usage:
    python3 pancetta-core/scripts/gen_dxcc_table.py /path/to/cty.dat \
        > pancetta-core/src/dxcc_table.rs

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
Leading `*` on the PRIMARY prefix means "non-DXCC contest subdivision" — keep the prefix but
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

    The primary_prefix may start with '*' (non-DXCC contest subdivision) or contain a '/'
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
        # Strip leading '*' (non-DXCC contest subdivision marker).
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

    # Sort by prefix length DESC then alphabetically so a regeneration
    # diffs cleanly. The consumer (`pancetta_core::dxcc`) indexes the rows
    # by prefix, so its lookup does not depend on this order.
    sorted_entries = sorted(clean.items(), key=lambda kv: (-len(kv[0]), kv[0]))
    return sorted_entries


def escape_rust_str(s: str) -> str:
    return s.replace('\\', '\\\\').replace('"', '\\"')


def parse_records(path):
    """Read every entity, including exact-only and non-DXCC subdivisions."""
    from pathlib import Path
    for record in Path(path).read_text(encoding='latin1').split(';'):
        lines = record.strip().splitlines()
        if not lines:
            continue
        fields = lines[0].split(':')
        if len(fields) < 8:
            raise ValueError('Malformed cty.dat header')
        aliases, exact = [], []
        for token in ' '.join(lines[1:]).split(','):
            token = strip_modifiers(token).upper()
            if token:
                (exact if token.startswith('=') else aliases).append(token.lstrip('='))
        yield fields[0].strip(), fields[7].strip().upper(), aliases, exact


def rule_entity(call):
    head = call.split('/')[0]
    if head.startswith('KG4'):
        suffix = head[3:]
        return ('Guantanamo Bay' if not suffix or
                (len(suffix) == 2 and suffix.isascii() and suffix.isalpha()) else 'United States')
    return None


def supplemental_tables(records, mapping, valid_ids, base):
    registry, exact = {}, {}
    names = {}
    for raw_name, raw_primary, _, calls in records:
        primary = raw_primary.lstrip('*')
        if primary not in mapping:
            raise ValueError(f'Missing ADIF mapping: {primary}')
        row = mapping[primary]
        if str(row['id']) not in valid_ids or row['id'] <= 0:
            raise ValueError(f'Invalid ADIF identity: {primary}')
        if raw_name != row['name'] and raw_name not in row.get('aliases', []):
            raise ValueError(f'Unmapped entity spelling: {raw_name}')
        name = row['name']
        canonical = row.get('canonical', name)
        value = (name, row['id'], canonical)
        if primary in registry and registry[primary] != value:
            raise ValueError(f'Conflicting primary: {primary}')
        registry[primary] = value
        for n in [name, *row.get('aliases', [])]:
            if n in names and names[n] != row['id']:
                raise ValueError(f'Conflicting name: {n}')
            names[n] = row['id']
        for call in calls:
            if call in exact:
                previous = exact[call]
                if names[previous] != row['id']:
                    raise ValueError(f'Conflicting exact call: {call}')
                exact[call] = min(previous, name)
            else:
                exact[call] = name
    for name, ident, canonical in registry.values():
        if names.get(canonical) != ident:
            raise ValueError(f'Invalid parent: {name} -> {canonical}')
    for _, name in base:
        if name not in names:
            raise ValueError(f'Unmapped shipped entity: {name}')
    prefix = dict(base)
    max_len = max(map(len, prefix), default=0)
    def baseline(call):
        rule = rule_entity(call)
        if rule:
            return rule
        for length in range(min(len(call), max_len), 0, -1):
            if call[:length] in prefix:
                return prefix[call[:length]]
        if call.startswith(('K', 'W', 'N')) or (len(call) > 1 and call[0] == 'A' and 'A' <= call[1] <= 'L'):
            return 'United States'
        return None
    retained = sorted((call, name) for call, name in exact.items() if baseline(call) != name)
    return sorted((primary, *value) for primary, value in registry.items()), retained


def zone_tables(path, mapping):
    """Build defaults, winning prefixes and unpruned exact-call zone rows.

    Entity-only outputs deliberately discard same-entity exact calls. Zones
    cannot use that pruning: such calls can still override either zone.
    """
    from pathlib import Path
    defaults, primaries, aliases, exact = {}, {}, {}, {}
    token_pairs = {}

    def checked_zone(value, maximum):
        text = str(value).strip()
        if not re.fullmatch(r'[0-9]+', text) or not 1 <= int(text) <= maximum:
            raise ValueError(f'Invalid zone: {value}')
        return int(text)

    def insert_unique(table, key, value):
        if key in table and table[key] != value:
            raise ValueError(f'Conflicting zones: {key}')
        table[key] = value

    for record in Path(path).read_text(encoding='latin1').split(';'):
        lines = record.strip().splitlines()
        if not lines:
            continue
        fields = lines[0].split(':')
        if len(fields) < 8:
            raise ValueError('Malformed cty.dat header')
        primary = fields[7].strip().upper().lstrip('*')
        row = mapping.get(primary)
        if row is None or (fields[0].strip() != row['name'] and
                           fields[0].strip() not in row.get('aliases', [])):
            raise ValueError(f'Unmapped zone entity: {primary}')
        name = row['name']
        default = (checked_zone(fields[1], 40), checked_zone(fields[2], 90))
        insert_unique(defaults, name, default)
        explicit = {}
        for raw_token in ' '.join(lines[1:]).split(','):
            token = raw_token.strip().upper()
            if not token:
                continue
            cq_matches = re.findall(r'\(([^)]*)\)', token)
            itu_matches = re.findall(r'\[([^]]*)\]', token)
            bare = strip_modifiers(token)
            if any(c in bare for c in '()[]') or len(cq_matches) > 1 or len(itu_matches) > 1:
                raise ValueError(f'Malformed zone modifiers: {token}')
            pair = (checked_zone(cq_matches[0], 40) if cq_matches else default[0],
                    checked_zone(itu_matches[0], 90) if itu_matches else default[1])
            is_exact = bare.startswith('=')
            bare = bare.lstrip('=')
            if not bare:
                raise ValueError(f'Empty zone token: {token}')
            insert_unique(token_pairs, (name, is_exact, bare), pair)
            if is_exact:
                insert_unique(exact, (bare, name), pair)
            elif '/' not in primary and '/' not in bare:
                explicit[bare] = pair
        # A virtual slash primary must never become a general prefix. Its
        # exact-call rows and entity default are still retained above.
        if '/' not in primary:
            primaries.setdefault(primary, (name, *explicit.get(primary, default)))
            for token, pair in explicit.items():
                aliases.setdefault(token, (name, *pair))
    prefixes = {**aliases, **primaries}
    return (sorted((name, *pair) for name, pair in defaults.items()),
            sorted((token, *value) for token, value in prefixes.items()),
            sorted((token, name, *pair) for (token, name), pair in exact.items()))


def generate_supplemental(core):
    import hashlib
    import json
    from pathlib import Path
    core = Path(core)
    source = core / 'data/cty.dat'
    raw = source.read_bytes()
    mapping = json.loads((core / 'data/dxcc_adif_map.json').read_text())['entities']
    ids = json.loads((core / 'data/adif_dxcc_ids.json').read_text())['entities']
    base = re.findall(r'\("([^"]+)", "([^"]+)"\)', (core / 'src/dxcc_table.rs').read_text())
    registry, exact = supplemental_tables(list(parse_records(source)), mapping, ids, base)
    version = re.search(rb'=VER[0-9]+', raw).group().decode().lstrip('=')
    header = ('//! Generated by `python3 pancetta-core/scripts/gen_dxcc_table.py --supplemental`.\n'
              '//! Source: https://www.country-files.com/bigcty/cty.dat\n'
              f'//! Input: {version}; SHA-256 {hashlib.sha256(raw).hexdigest()}\n'
              '//! ADIF identities: https://adif.org/315/ADIF_315_annotated.htm#DXCC_Entity_Code_Enumeration\n\n')
    def lit(s):
        return '"' + escape_rust_str(s) + '"'
    reg = header + '#[rustfmt::skip]\npub(crate) static REGISTRY: &[(&str, &str, u16, &str)] = &[\n'
    for primary, name, ident, canonical in registry:
        reg += f'    ({lit(primary)}, {lit(name)}, {ident}, {lit(canonical)}),\n'
    reg += '];\n\n#[rustfmt::skip]\npub(crate) static NAME_ALIASES: &[(&str, &str)] = &[\n'
    aliases = sorted((alias, row['name']) for row in mapping.values() for alias in row.get('aliases', []))
    reg += ''.join(f'    ({lit(alias)}, {lit(name)}),\n' for alias, name in aliases) + '];\n'
    ex = header + '#[rustfmt::skip]\npub(crate) static EXACT_TABLE: &[(&str, &str)] = &[\n'
    ex += ''.join(f'    ({lit(call)}, {lit(name)}),\n' for call, name in exact) + '];\n'
    defaults, prefixes, zone_exact = zone_tables(source, mapping)
    zones = header
    for symbol, rows, shape in [('ZONE_DEFAULTS', defaults, '(&str, u8, u8)'),
                                ('ZONE_PREFIXES', prefixes, '(&str, &str, u8, u8)'),
                                ('ZONE_EXACT', zone_exact, '(&str, &str, u8, u8)')]:
        zones += f'#[rustfmt::skip]\npub(crate) static {symbol}: &[{shape}] = &[\n'
        for row in rows:
            zones += '    (' + ', '.join(lit(value) if isinstance(value, str) else str(value)
                                         for value in row) + '),\n'
        zones += '];\n'
    return {'dxcc_registry.rs': reg, 'dxcc_exact.rs': ex, 'dxcc_zones.rs': zones}


def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} /path/to/cty.dat", file=sys.stderr)
        sys.exit(1)

    if sys.argv[1] == '--supplemental':
        from pathlib import Path
        core = Path(__file__).resolve().parents[1]
        for name, text in generate_supplemental(core).items():
            (core / 'src' / name).write_text(text)
        return

    path = sys.argv[1]
    entries = build_table(path)

    print('//! AUTO-GENERATED from cty.dat (AD1C BigCTY, <https://www.country-files.com/>, free for amateur use).')
    print('//! Do not edit by hand — regenerate with:')
    print('//!   python3 pancetta-core/scripts/gen_dxcc_table.py /path/to/cty.dat \\')
    print('//!       > pancetta-core/src/dxcc_table.rs')
    print('//!')
    print('//! Maps callsign prefix -> DXCC entity name.')
    print('//! Entries are sorted by prefix length DESC then alphabetically so a')
    print('//! regeneration diffs cleanly; `crate::dxcc` indexes them by prefix.')
    print()
    print('pub static PREFIX_TABLE: &[(&str, &str)] = &[')
    for pfx, name in entries:
        print(f'    ("{escape_rust_str(pfx)}", "{escape_rust_str(name)}"),')
    print('];')

    print(f'\n// Total: {len(entries)} entries', file=sys.stderr)


if __name__ == '__main__':
    main()
