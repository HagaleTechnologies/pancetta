# Third-Party Notices for pancetta-core

The crate's own code is licensed `MIT OR Apache-2.0` (see `LICENSE-MIT` and
`LICENSE-APACHE`). Its DXCC tables are generated from third-party data,
credited below. Cargo dependencies carry their own licenses and are not
listed here.

---

## AD1C country files (cty.dat)

- **Project:** BigCTY country files, maintained by Jim Reisert, AD1C
  ([country-files.com](https://www.country-files.com/))
- **Used in:** `src/dxcc_table.rs`, `src/dxcc_exact.rs`, `src/dxcc_zones.rs`
  and `src/dxcc_registry.rs`
- **Used as:** the source of those generated DXCC prefix, exact-call, zone
  and entity tables, produced by the repository's
  `scripts/gen_dxcc_table.py` from input `VER20261107` (recorded in
  `src/dxcc_registry.rs`).
- **Terms:** the repository records them only as "free for amateur use".
  Confirmation of the redistribution terms is pending and tracked as a
  follow-up to PAN-384; it gates any crates.io publish of pancetta-core.

ADIF DXCC entity codes come from the ADIF specification
(<https://adif.org/315/ADIF_315_annotated.htm>).
