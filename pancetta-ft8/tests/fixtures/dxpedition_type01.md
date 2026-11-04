# DXpedition type 0.1 reference payload (PAN-259)

Message: `K1ABC RR73; W9XYZ <KH1/KH7Z> -08`

Provenance: WSJT-X `lib/77bit/packjt77.f90` (master, read 2026-11-04),
subroutine `pack77_01` and the `i3=0 n3=1` branch of `unpack77`. The bits
below were produced by `gen_dxpedition_type01.py` in this directory, an
independent Python re-implementation of `pack28` (standard calls),
`ihashcall(c,10)` and the report field. It shares no code with pancetta:

    python3 -I gen_dxpedition_type01.py K1ABC W9XYZ KH1/KH7Z -8

Layout (`format 2b28.28,b10.10,b5.5,2b3.3`):

| Field | Bits   | Value    | Meaning                         |
|-------|--------|----------|---------------------------------|
| n28a  | 0..28  | 10214965 | K1ABC, receives RR73            |
| n28b  | 28..56 | 12751800 | W9XYZ, receives the report      |
| n10   | 56..66 | 201      | ihashcall("KH1/KH7Z", 10)       |
| n5    | 66..71 | 11       | report = 2*11 - 30 = -08        |
| n3    | 71..74 | 1        | DXpedition subtype              |
| i3    | 74..77 | 0        |                                 |

77-bit payload:

    00001001101111011110001101010000110000101001001110111000001100100101011001000

Payload bytes (77 bits, MSB first, 3 zero pad bits):

    0x09, 0xBD, 0xE3, 0x50, 0xC2, 0x93, 0xB8, 0x32, 0x56, 0x40

Report quantization (`n5 = (n + 30) / 2`, Fortran truncation, clamped to
0..31; rendered as `2*n5 - 30`):

| Sent | n5 | Rendered |
|------|----|----------|
| -40  | 0  | -30      |
| -31  | 0  | -30      |
| -9   | 10 | -10      |
| -8   | 11 | -08      |
| -7   | 11 | -08      |
| 0    | 15 | +00      |
| +1   | 15 | +00      |
| +32  | 31 | +32      |
| +33  | 31 | +32      |
