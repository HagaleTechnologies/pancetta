#!/usr/bin/env python3
"""Independent WSJT-X type 0.1 (DXpedition) bit packer for PAN-259 fixtures.

Re-implements, from WSJT-X lib/77bit/packjt77.f90 (`pack77_01`, `pack28`,
`ihashcall`), the 77-bit payload of a Fox multi-addressee frame such as

    K1ABC RR73; W9XYZ <KH1/KH7Z> -08

Layout (format 2b28.28,b10.10,b5.5,2b3.3):

    n28a(28) n28b(28) n10(10) n5(5) n3(3)=1 i3(3)=0

n28a is the Hound that gets RR73, n28b the Hound that gets the report, n10
the Fox's 10-bit call hash, and n5 = (report + 30) / 2 (Fortran integer
division, truncating toward zero) clamped to 0..31; the receiver renders
2*n5 - 30. This script deliberately shares no code with pancetta: run it
with `python3 -I gen_dxpedition_type01.py` and compare against the pinned
values in dxpedition_type01.md.
"""

import sys

NTOKENS = 2063592
MAX22 = 4194304
A1 = " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
A2 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
A3 = "0123456789"
A4 = " ABCDEFGHIJKLMNOPQRSTUVWXYZ"
HASH_CHARS = " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/"


def pack28_standard(call):
    """pack28 for a standard callsign (no /P, /R, tokens or hashes)."""
    call = call.upper()
    # WSJT-X: right-align so the call-area digit lands in position 3.
    if len(call) >= 3 and call[2].isdigit():
        c6 = call
    elif len(call) >= 2 and call[1].isdigit():
        c6 = " " + call
    else:
        raise ValueError("not a standard call: " + call)
    c6 = c6.ljust(6)
    if len(c6) != 6:
        raise ValueError("not a standard call: " + call)
    i1 = A1.index(c6[0])
    i2 = A2.index(c6[1])
    i3 = A3.index(c6[2])
    i4 = A4.index(c6[3])
    i5 = A4.index(c6[4])
    i6 = A4.index(c6[5])
    n = 36 * 10 * 27 * 27 * 27 * i1 + 10 * 27 * 27 * 27 * i2 + 27 * 27 * 27 * i3
    n += 27 * 27 * i4 + 27 * i5 + i6
    return NTOKENS + MAX22 + n


def ihashcall(call, m):
    c = call.upper().ljust(11)[:11]
    n8 = 0
    for ch in c:
        n8 = 38 * n8 + HASH_CHARS.index(ch)
    return ((47055833459 * n8) % (1 << 64)) >> (64 - m)


def fortran_div(a, b):
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def n5_for(report):
    n5 = fortran_div(report + 30, 2)
    return max(0, min(31, n5))


def pack77_01(rr73_to, report_to, fox, report):
    n28a = pack28_standard(rr73_to)
    n28b = pack28_standard(report_to)
    n10 = ihashcall(fox, 10)
    n5 = n5_for(report)
    bits = (
        format(n28a, "028b")
        + format(n28b, "028b")
        + format(n10, "010b")
        + format(n5, "05b")
        + format(1, "03b")
        + format(0, "03b")
    )
    assert len(bits) == 77
    return n28a, n28b, n10, n5, bits


def payload_bytes(bits):
    padded = bits + "000"
    return [int(padded[i : i + 8], 2) for i in range(0, 80, 8)]


def main():
    args = sys.argv[1:] or ["K1ABC", "W9XYZ", "KH1/KH7Z", "-8"]
    rr73_to, report_to, fox, report = args[0], args[1], args[2], int(args[3])
    n28a, n28b, n10, n5, bits = pack77_01(rr73_to, report_to, fox, report)
    print("n28a=%d n28b=%d n10=%d n5=%d" % (n28a, n28b, n10, n5))
    print("bits=" + bits)
    print("payload=" + ", ".join("0x%02X" % b for b in payload_bytes(bits)))
    for r in (-40, -31, -30, -9, -8, -7, 0, 1, 31, 32, 33, 50):
        print("report %d -> n5=%d -> rendered %+03d" % (r, n5_for(r), 2 * n5_for(r) - 30))


if __name__ == "__main__":
    main()
