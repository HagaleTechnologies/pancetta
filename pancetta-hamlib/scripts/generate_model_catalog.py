#!/usr/bin/env python3
"""Generate pancetta-hamlib/src/model_catalog_generated.rs from the vendored
`rigctl -l` snapshot in pancetta-hamlib/data/.

Parses the snapshot with the header's column boundaries (model names contain
spaces, so whitespace token splitting cannot work) and rejects malformed
records, zero/out-of-range IDs and duplicate IDs instead of silently
skipping lines. Reads nothing but the snapshot: no Hamlib installation and
no network. Test/dev tool only — a normal Cargo build uses the committed
generated file.

Usage:
    generate_model_catalog.py           # write src/model_catalog_generated.rs
    generate_model_catalog.py --check   # exit 1 if the file is missing/stale
"""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CRATE_DIR = SCRIPT_DIR.parent
SNAPSHOT = CRATE_DIR / "data" / "rigctl-4.5.5.txt"
OUTPUT = CRATE_DIR / "src" / "model_catalog_generated.rs"

HAMLIB_VERSION = "4.5.5"
MAX_ID = 2147483647

# (field, header label); slice end for each field is the next field's start
FIELD_SPECS: list[tuple[str, str]] = [
    ("id", "Rig #"),
    ("manufacturer", "Mfg"),
    ("model", "Model"),
    ("version", "Version"),
    ("status", "Status"),
    ("macro", "Macro"),
]


class ParseError(Exception):
    pass


def parse(text: str) -> list[dict]:
    lines = text.splitlines()
    if not lines:
        raise ParseError("snapshot is empty")
    header = lines[0]
    starts = {}
    for name, label in FIELD_SPECS:
        try:
            starts[name] = header.index(label)
        except ValueError:
            raise ParseError(f"header column {label!r} not found") from None

    rows: list[dict] = []
    seen_ids: dict[int, int] = {}
    for lineno, line in enumerate(lines[1:], start=2):
        if not line.strip():
            continue

        def field(name: str) -> str:
            index = [i for i, (n, _) in enumerate(FIELD_SPECS) if n == name][0]
            start = starts[name]
            end = (
                starts[FIELD_SPECS[index + 1][0]]
                if index + 1 < len(FIELD_SPECS)
                else len(line)
            )
            return line[start:end].strip()

        try:
            rig_id = int(field("id"))
        except ValueError:
            raise ParseError(f"line {lineno}: Rig # is not an integer: {line!r}") from None
        row = {
            "id": rig_id,
            "manufacturer": field("manufacturer"),
            "model": field("model"),
            "version": field("version"),
            "status": field("status"),
            "macro": field("macro"),
        }
        if not 1 <= rig_id <= MAX_ID:
            raise ParseError(f"line {lineno}: Rig # {rig_id} out of range 1..{MAX_ID}")
        for col in ("manufacturer", "model", "version", "status", "macro"):
            if not row[col]:
                raise ParseError(f"line {lineno}: empty {col} column: {line!r}")
        # Upstream prints `(null)` for at least one backend (TM-D710(G),
        # 2034) whose riglist.h define is missing, so only require the macro
        # column to be present; curated tables pin macros that exist.
        if rig_id in seen_ids:
            raise ParseError(f"line {lineno}: duplicate Rig # {rig_id} (first seen on line {seen_ids[rig_id]})")
        seen_ids[rig_id] = lineno
        rows.append(row)
    if not rows:
        raise ParseError("snapshot has no model rows")
    return rows


def rust_str(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def render(rows: list[dict], checksum: str) -> str:
    out = []
    out.append("//! GENERATED FILE - do not edit by hand.")
    out.append("//! Source: pancetta-hamlib/data/rigctl-4.5.5.txt (Hamlib 4.5.5 `rigctl -l`).")
    out.append("//! Regenerate: python3 pancetta-hamlib/scripts/generate_model_catalog.py")
    out.append(f"//! Snapshot SHA-256: {checksum}")
    out.append("//!")
    out.append("//! The catalog is a pinned offline baseline, not a probe of the installed")
    out.append("//! Hamlib; models newer than it stay usable by numeric `rig.model` ID.")
    out.append("")
    out.append("/// One row of the vendored Hamlib 4.5.5 model list.")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
    out.append("pub struct CatalogEntry {")
    out.append("    /// Hamlib model number, the value `rigctld -m` takes.")
    out.append("    pub id: u32,")
    out.append("    /// Manufacturer as `rigctl -l` prints it, for example `\"Icom\"`.")
    out.append("    pub manufacturer: &'static str,")
    out.append("    /// Model name as `rigctl -l` prints it, for example `\"IC-7300\"`.")
    out.append("    pub model: &'static str,")
    out.append("    /// Backend version as `rigctl -l` prints it.")
    out.append("    pub version: &'static str,")
    out.append("    /// Backend status (`Stable`, `Beta`, ...).")
    out.append("    pub status: &'static str,")
    out.append("    /// Upstream `RIG_MODEL_*` macro name, the stable key other tables pin.")
    out.append("    /// One upstream row (TM-D710(G), 2034) prints `(null)` — retained verbatim.")
    out.append("    pub macro_name: &'static str,")
    out.append("}")
    out.append("")
    out.append(f"/// Every model row of the vendored Hamlib {HAMLIB_VERSION} catalog, in snapshot order.")
    out.append("#[rustfmt::skip]  // one row per line; regenerated, not hand-formatted")
    out.append("pub const HAMLIB_MODEL_CATALOG: &[CatalogEntry] = &[")
    for row in rows:
        out.append(
            "    CatalogEntry {{ id: {id}, manufacturer: {mfg}, model: {model}, version: {ver},"
            " status: {status}, macro_name: {macro} }},".format(
                id=row["id"],
                mfg=rust_str(row["manufacturer"]),
                model=rust_str(row["model"]),
                ver=rust_str(row["version"]),
                status=rust_str(row["status"]),
                macro=rust_str(row["macro"]),
            )
        )
    out.append("];")
    out.append("")
    return "\n".join(out)


def main() -> int:
    check = "--check" in sys.argv[1:]
    if [a for a in sys.argv[1:] if a != "--check"]:
        print(f"usage: {sys.argv[0]} [--check]", file=sys.stderr)
        return 2
    if not SNAPSHOT.exists():
        print(f"snapshot not found: {SNAPSHOT}", file=sys.stderr)
        return 1
    text = SNAPSHOT.read_text()
    try:
        rows = parse(text)
    except ParseError as exc:
        print(f"{SNAPSHOT}: {exc}", file=sys.stderr)
        return 1
    checksum = hashlib.sha256(text.encode()).hexdigest()
    expected = render(rows, checksum)
    if check:
        actual = OUTPUT.read_text() if OUTPUT.exists() else None
        if actual != expected:
            print(
                f"{OUTPUT} is stale or missing. Regenerate with:\n"
                f"  python3 {SCRIPT_DIR.name}/generate_model_catalog.py",
                file=sys.stderr,
            )
            return 1
        print(f"ok: {OUTPUT} matches {SNAPSHOT.name} ({len(rows)} rows)")
        return 0
    OUTPUT.write_text(expected)
    print(f"wrote {OUTPUT} ({len(rows)} rows)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
