<!-- GENERATED FILE - do not edit by hand.
     Source: pancetta-hamlib/src/models.rs (SUPPORTED_RIGS).
     Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-hamlib supported_rigs_doc_is_current -->

# Supported rigs

Pancetta controls a rig through Hamlib's `rigctld`. It starts `rigctld` itself for the rigs below.
Set `rig.model` in `~/.pancetta/pancetta.toml` to a name from the `rig.model` column.
Case, hyphens and spaces are ignored.

The Yaesu FTdx10 is the only rig with a recorded hardware test.
Every other rig is mapped to its Hamlib model, but nobody has recorded using it with Pancetta.

| Rig | `rig.model` | Hamlib model | CAT | Audio | TX |
|---|---|---|---|---|---|
| Icom IC-7300 | `IC-7300` | 3073 | Untested | Untested | Untested |
| Icom IC-705 | `IC-705` | 3085 | Untested | Untested | Untested |
| Icom IC-7610 | `IC-7610` | 3078 | Untested | Untested | Untested |
| Icom IC-7851 | `IC-7851` | 3075 | Untested | Untested | Untested |
| Icom IC-9700 | `IC-9700` | 3081 | Untested | Untested | Untested |
| Kenwood TS-590S | `TS-590`, `TS-590S` | 2031 | Untested | Untested | Untested |
| Kenwood TS-590SG | `TS-590SG` | 2037 | Untested | Untested | Untested |
| Kenwood TS-890S | `TS-890`, `TS-890S` | 2041 | Untested | Untested | Untested |
| Yaesu FT-710 | `FT-710` | 1049 | Untested | Untested | Untested |
| Yaesu FT-817 | `FT-817`, `FT-817ND` | 1020 | Untested | Untested | Untested |
| Yaesu FT-857 | `FT-857`, `FT-857D` | 1022 | Untested | Untested | Untested |
| Yaesu FT-891 | `FT-891` | 1036 | Untested | Untested | Untested |
| Yaesu FT-991 | `FT-991`, `FT-991A` | 1035 | Untested | Untested | Untested |
| Yaesu FTdx10 | `FTdx10` | 1042 | Tested | Tested | Tested |
| Yaesu FTdx101D | `FTdx101D` | 1040 | Untested | Untested | Untested |
| Yaesu FTdx101MP | `FTdx101MP` | 1044 | Untested | Untested | Untested |

## What the columns mean

- CAT: Pancetta controls frequency, split and PTT. Optional `[rig] data_mode` sets the CAT mode on connection and after commanded retunes. Omit it for manual mode control. This option is hardware-unverified.
- Audio: Pancetta decodes from and transmits into the rig's USB sound device, which you pick as input and output in `pancetta setup`. Pancetta has no rig-specific audio code.
- TX: Pancetta keys the rig and sends FT8 on air.
- Tested: used on a real station with this rig, as recorded in the README [Capability status](../README.md#capability-status) table.
- Untested: this repository records no hardware test of this rig.
- Hamlib model: the number Pancetta passes to `rigctld -m`. `rigctl -l` lists the models your installed Hamlib knows; an older Hamlib may lack a newer rig.

## Test records

- Yaesu FTdx10: CAT and PTT radio-qualified; TX validated 2026-04-26 (clean ALC, PSKReporter spots across NA and EU); manual on-air QSOs since 2026-09.

## Remote operation

The station agent uses the same rig connection and the same audio devices as local operation, so a rig's row also applies to remote use.
No remote session on a real rig is recorded. Remote operation is integration-tested only.

## Rigs not in this list

If a `rigctld` is already listening on `RIGCTLD_HOST`:`RIGCTLD_PORT` (default `127.0.0.1:4532`), Pancetta connects to it and does not start its own.
To use another rig Hamlib supports, start `rigctld -m <model> -r <port>` yourself before Pancetta.
`pancetta doctor` still reports a `rig.model` that is not in this list as unknown.

SWR readings, attenuator steps and preamp steps differ between rigs. Pancetta reads them from `rigctld` when it connects, so this page does not list them.
