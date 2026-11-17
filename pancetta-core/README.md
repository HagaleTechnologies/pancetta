# pancetta-core

Shared amateur-radio types for [`pancetta-ft8`](https://crates.io/crates/pancetta-ft8)
and the Pancetta station: FT8 slot parity, callsign and Maidenhead grid
validation, FT8 message tokens, band names and an offline DXCC entity lookup.

`pancetta-core` is published so that `pancetta-ft8` can be. Its API serves the
Pancetta workspace. It follows semver. While below 1.0, a release that breaks
an item `pancetta-ft8` uses bumps the minor version.

The DXCC tables are generated from the AD1C BigCTY `cty.dat`; see
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option (`MIT OR Apache-2.0`).
