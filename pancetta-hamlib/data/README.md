# Vendored Hamlib model-list snapshot

`rigctl-4.5.5.txt` is a byte-for-byte capture of `rigctl -l` from Hamlib
4.5.5, the catalog `pancetta-hamlib/src/model_catalog_generated.rs` is
generated from. It is a deliberate, pinned baseline — not a claim that
4.5.5 is the newest Hamlib. Models newer than this catalog stay usable by
numeric `rig.model` ID.

## Provenance

- Source packages (Ubuntu Noble, `amd64`), downloaded with `apt-get download`
  and extracted with `dpkg-deb -x` into a scratch directory, without
  installing any service:
  - `libhamlib-utils 4.5.5-3.2build2` (provides `usr/bin/rigctl`)
  - `libhamlib4t64 4.5.5-3.2build2`
  - `libusb-1.0-0 2:1.0.27-1`
  - `libindiclient1 1.9.9+dfsg-3build3`
  - `libnova-0.16-0t64 0.16-5.1build1`
- Capture command (run from the scratch directory holding `root/`):

  ```sh
  LC_ALL=C LD_LIBRARY_PATH=root/usr/lib/x86_64-linux-gnu \
      root/usr/bin/rigctl -l > rigctl-4.5.5.txt
  ```

- `rigctl --version` reported:
  `rigctl Hamlib 4.5.5 Apr 05 11:43:08Z 2023 SHA=6eecd3`
- Content checksum: SHA-256
  `afed5149497917ec4532a68ea6bfb10fc7665019c11e3f0fc9c63b63333bdf1f`
  (284 lines: one header plus 283 model records with 283 distinct IDs).

## Regenerating the Rust catalog

```sh
python3 pancetta-hamlib/scripts/generate_model_catalog.py          # write
python3 pancetta-hamlib/scripts/generate_model_catalog.py --check  # verify
```

The generator reads only this file — no Hamlib installation, no network —
and the `catalog_generation_is_current` test runs its `--check` mode in CI.
Refreshing the catalog is an explicit maintainer action: capture a new
snapshot into `data/`, update this README's provenance and checksum, rerun
the generator, and regenerate `docs/SUPPORTED_RIGS.md`.

## Licensing

The list is factual model metadata (ID, manufacturer, model, version,
status, macro) emitted by `rigctl -l`. Hamlib is LGPL-2.1; the table here
is data, not derived work source, and `THIRD-PARTY-NOTICES.md` carries the
attribution for the Hamlib project.
