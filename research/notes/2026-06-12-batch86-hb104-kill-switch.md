# Batch 86 — hb-104 kill-switch: one-step LS subtract + re-decode

Spec: `docs/superpowers/specs/2026-06-12-hb104-joint-decode-scoping.md`.
Work list: `research/corpus/curated/ft8/hb104_kill_switch.json` (20 slots, 70 pairs).
Truth: ft8_lib, tol=0 exact text. Pairs are independent trials from the
original audio (never cumulative). Miss coordinates unused in decode.

Two fit rounds per pair: **global** (one complex amplitude over the
12.64 s synth, nominal coordinates) and **refined** (10 per-block
amplitudes x grid search over delta-f in +/-1.6 Hz, delta-t in +/-2400
samples, best residual energy). The refined round is the decision
round per the spec's "< 2% after refinement" SHELVE wording.

## Work-list integrity finding

54/70 pairs are **display aliases**: ft8_lib renders an unresolved hashed
callsign as `<...>` while pancetta renders `<...NNNN>`, so the "miss" is
the same transmission as the decoded neighbor (identical text modulo the
hash token; coordinates agree to grid granularity, up to 3.125 Hz /
0.16 s). Subtracting the neighbor removes the missed signal itself —
recovery is structurally impossible for those pairs. They were run anyway
(they exercise the precision subtract and provide the mechanism
evidence). The remaining 16 pairs have cross-text neighbors that did not
reproduce in the greedy decode — all 16 neighbor texts look like FP
decodes from the original scan (e.g. `2W9XHD JU4YID/P R JN15`), so the
work list contains zero valid co-channel targets. See the premise audit
below for what this means for the Batch 85 numbers.

## Per-slot results

| Slot | pairs | neighbor-not-decoded | encode/fit failed | tried (alias) | rec global | rec refined (alias) | serendip TPs (refined) | new FPs (refined) | mean res-ratio g/r | mean band-ratio g/r |
|---|---:|---:|---:|---|---:|---|---:|---:|---|---|
| ft8_20260530_154228 | 5 | 1 | 0 | 4 (4) | 0 | 0 (0) | 0 | 0 | 0.9999 / 0.8144 | 0.999 / 0.103 |
| ft8_20260530_170958 | 5 | 1 | 0 | 4 (4) | 0 | 0 (0) | 0 | 2 | 1.0000 / 0.9803 | 0.999 / 0.169 |
| ft8_20260530_171858 | 5 | 1 | 0 | 4 (4) | 0 | 0 (0) | 0 | 0 | 0.9999 / 0.9726 | 0.999 / 0.288 |
| ft8_20260530_154358 | 4 | 1 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 0.9996 / 0.7994 | 0.999 / 0.257 |
| ft8_20260530_163143 | 4 | 1 | 0 | 3 (3) | 0 | 0 (0) | 2 | 0 | 1.0000 / 0.9983 | 0.999 / 0.524 |
| ft8_20260530_171828 | 4 | 1 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 0.9999 / 0.9786 | 0.997 / 0.126 |
| ft8_20260530_191028 | 4 | 2 | 0 | 2 (2) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9944 | 0.999 / 0.170 |
| ft8_20260530_152443 | 3 | 1 | 0 | 2 (2) | 0 | 0 (0) | 0 | 1 | 0.9993 / 0.8296 | 0.997 / 0.150 |
| ft8_20260530_152828 | 3 | 0 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9773 | 0.999 / 0.222 |
| ft8_20260530_152928 | 3 | 1 | 0 | 2 (2) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9466 | 0.999 / 0.381 |
| ft8_20260530_153258 | 3 | 1 | 0 | 2 (2) | 0 | 0 (0) | 0 | 0 | 0.9987 / 0.6574 | 0.997 / 0.436 |
| ft8_20260530_154258 | 3 | 0 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9890 | 0.998 / 0.185 |
| ft8_20260530_154328 | 3 | 0 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 0.9997 / 0.7881 | 0.999 / 0.200 |
| ft8_20260530_160958 | 3 | 0 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9551 | 0.998 / 0.305 |
| ft8_20260530_161058 | 3 | 1 | 0 | 2 (2) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9935 | 1.000 / 0.358 |
| ft8_20260530_161928 | 3 | 2 | 0 | 1 (1) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9767 | 0.999 / 0.329 |
| ft8_20260530_163513 | 3 | 0 | 0 | 3 (3) | 0 | 0 (0) | 0 | 1 | 1.0000 / 0.8411 | 1.000 / 0.268 |
| ft8_20260530_171958 | 3 | 1 | 0 | 2 (2) | 0 | 0 (0) | 0 | 1 | 1.0000 / 0.9547 | 1.000 / 0.280 |
| ft8_20260530_174028 | 3 | 1 | 0 | 2 (2) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9993 | 0.998 / 0.407 |
| ft8_20260530_185458 | 3 | 0 | 0 | 3 (3) | 0 | 0 (0) | 0 | 0 | 1.0000 / 0.9927 | 0.999 / 0.328 |

## Per-pair detail

| Slot | Neighbor (subtracted) | f (Hz) | Targeted miss | class | rec g | rec r | ser r | FP r | res g | res r | band g | band r | best df | best dt | mean \|a,b\| |
|---|---|---:|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ft8_20260530_154228 | <...3631> WZ8DX EM79 | 1321.9 | <...> WZ8DX EM79 | alias | no | no | 0 | 0 | 1.0000 | 0.9971 | 0.999 | 0.045 | -1.2 | -1920 | 0.00733 |
| ft8_20260530_154228 | <...3631> WB8TLI EN70 | 925.0 | <...> WB8TLI EN70 | alias | no | no | 0 | 0 | 0.9997 | 0.2648 | 1.000 | 0.044 | -1.4 | -1680 | 0.12057 |
| ft8_20260530_154228 | <...476> N0CVP DM41 | 1931.2 | <...> N0CVP DM41 | alias | no | no | 0 | 0 | 1.0000 | 0.9972 | 1.000 | 0.081 | -0.9 | -2280 | 0.00649 |
| ft8_20260530_154228 | <...476> K2BAG DM33 | 831.2 | <...> K2BAG DM33 | alias | no | no | 0 | 0 | 1.0000 | 0.9987 | 0.996 | 0.240 | -1.4 | -1560 | 0.00491 |
| ft8_20260530_170958 | <...3631> KF0RRJ R-07 | 2196.9 | <...> KF0RRJ R-07 | alias | no | no | 0 | 0 | 1.0000 | 0.9498 | 1.000 | 0.231 | +1.0 | -1920 | 0.02472 |
| ft8_20260530_170958 | <...3631> KB9DED EN54 | 2387.5 | <...> KB9DED EN54 | alias | no | no | 0 | 0 | 1.0000 | 0.9932 | 0.999 | 0.081 | +1.2 | -2040 | 0.00965 |
| ft8_20260530_170958 | <...3631> KR4FK EM85 | 1984.4 | <...> KR4FK EM85 | alias | no | no | 0 | 1 | 1.0000 | 0.9953 | 0.999 | 0.188 | -0.8 | -2040 | 0.00733 |
| ft8_20260530_170958 | <...673> K7CTV DM42 | 2456.2 | <...> K7CTV DM42 | alias | no | no | 0 | 1 | 1.0000 | 0.9829 | 0.999 | 0.175 | +1.6 | -2280 | 0.01594 |
| ft8_20260530_171858 | <...3462> KF9UG EN71 | 2562.5 | <...> KF9UG EN71 | alias | no | no | 0 | 0 | 0.9998 | 0.9281 | 0.998 | 0.122 | +0.6 | -1920 | 0.02910 |
| ft8_20260530_171858 | <...3631> KR4FK EM85 | 684.4 | <...> KR4FK EM85 | alias | no | no | 0 | 0 | 1.0000 | 0.9847 | 0.999 | 0.225 | -1.0 | -1920 | 0.01374 |
| ft8_20260530_171858 | <...3631> KA4UPC EL88 | 996.9 | <...> KA4UPC EL88 | alias | no | no | 0 | 0 | 1.0000 | 0.9826 | 0.999 | 0.270 | +0.6 | -1800 | 0.01524 |
| ft8_20260530_171858 | <...3631> WI0K DM79 | 940.6 | <...> WI0K DM79 | alias | no | no | 0 | 0 | 1.0000 | 0.9951 | 1.000 | 0.534 | +0.8 | -960 | 0.00812 |
| ft8_20260530_154358 | <...476> K2BAG DM33 | 828.1 | <...> K2BAG DM33 | alias | no | no | 0 | 0 | 1.0000 | 0.9985 | 1.000 | 0.206 | +1.1 | -2040 | 0.00492 |
| ft8_20260530_154358 | <...3559> AC6F R-14 | 1850.0 | <...> AC6F R-14 | alias | no | no | 0 | 0 | 1.0000 | 0.9998 | 1.000 | 0.529 | -0.9 | -1800 | 0.00164 |
| ft8_20260530_154358 | <...3631> WB8TLI EN70 | 1271.9 | <...> WB8TLI EN70 | alias | no | no | 0 | 0 | 0.9987 | 0.3998 | 0.998 | 0.036 | +0.5 | -1440 | 0.10021 |
| ft8_20260530_163143 | <...2829> KD9UJM EM78 | 1546.9 | <...> KD9UJM EM78 | alias | no | no | 0 | 0 | 1.0000 | 0.9968 | 0.999 | 0.483 | -0.3 | -1920 | 0.00593 |
| ft8_20260530_163143 | <...3303> K5VJZ R-11 | 2815.6 | <...> K5VJZ R-11 | alias | no | no | 1 | 0 | 1.0000 | 0.9993 | 1.000 | 0.130 | +1.0 | -2160 | 0.00278 |
| ft8_20260530_163143 | <...2829> AE1AA FN43 | 2546.9 | <...> AE1AA FN43 | alias | no | no | 1 | 0 | 1.0000 | 0.9989 | 1.000 | 0.960 | -0.1 | -1200 | 0.00351 |
| ft8_20260530_171828 | <...3631> KR4FK EM85 | 684.4 | <...> KR4FK EM85 | alias | no | no | 0 | 0 | 1.0000 | 0.9849 | 0.999 | 0.155 | -1.1 | -1920 | 0.01301 |
| ft8_20260530_171828 | <...3462> KF9UG EN71 | 2562.5 | <...> KF9UG EN71 | alias | no | no | 0 | 0 | 0.9999 | 0.9778 | 0.996 | 0.124 | +0.6 | -2040 | 0.01555 |
| ft8_20260530_171828 | <...3631> KA4UPC EL88 | 996.9 | <...> KA4UPC EL88 | alias | no | no | 0 | 0 | 0.9999 | 0.9729 | 0.997 | 0.099 | +0.7 | -1800 | 0.01854 |
| ft8_20260530_191028 | <...3346> AD9P R+09 | 2078.1 | <...> AD9P R+09 | alias | no | no | 0 | 0 | 1.0000 | 0.9897 | 0.999 | 0.219 | +0.6 | -1680 | 0.01089 |
| ft8_20260530_191028 | <...673> K7CTV DM42 | 2459.4 | <...> K7CTV DM42 | alias | no | no | 0 | 0 | 1.0000 | 0.9991 | 0.999 | 0.121 | -1.5 | -1800 | 0.00338 |
| ft8_20260530_152443 | KE8IFR <...3631> -10 | 1046.9 | KE8IFR <...> -10 | alias | no | no | 0 | 0 | 0.9987 | 0.7828 | 0.995 | 0.142 | +0.0 | -1680 | 0.05350 |
| ft8_20260530_152443 | <...2993> K4SSL EM60 | 621.9 | <...> K4SSL EM60 | alias | no | no | 0 | 1 | 0.9999 | 0.8763 | 1.000 | 0.158 | -1.0 | -1560 | 0.04105 |
| ft8_20260530_152828 | <...3631> KB9SCT EN43 | 1046.9 | <...> KB9SCT EN43 | alias | no | no | 0 | 0 | 1.0000 | 0.9964 | 0.999 | 0.199 | -0.3 | -1560 | 0.00609 |
| ft8_20260530_152828 | <...3631> WB8TLI EN70 | 1415.6 | <...> WB8TLI EN70 | alias | no | no | 0 | 0 | 1.0000 | 0.9358 | 1.000 | 0.156 | +1.6 | -1800 | 0.02721 |
| ft8_20260530_152828 | <...3631> KO4KYM EM85 | 2859.4 | <...> KO4KYM EM85 | alias | no | no | 0 | 0 | 1.0000 | 0.9998 | 0.999 | 0.312 | -1.0 | -1920 | 0.00151 |
| ft8_20260530_152928 | <...3631> WB8TLI EN70 | 1415.6 | <...> WB8TLI EN70 | alias | no | no | 0 | 0 | 1.0000 | 0.8932 | 1.000 | 0.191 | +0.0 | -1680 | 0.03629 |
| ft8_20260530_152928 | <...3631> KO4KYM EM85 | 2859.4 | <...> KO4KYM EM85 | alias | no | no | 0 | 0 | 1.0000 | 1.0000 | 0.999 | 0.571 | -1.2 | -1800 | 0.00054 |
| ft8_20260530_153258 | <...3631> KN6NW DM33 | 850.0 | <...> KN6NW DM33 | alias | no | no | 0 | 0 | 1.0000 | 0.9999 | 0.998 | 0.772 | +1.6 | -960 | 0.00113 |
| ft8_20260530_153258 | <...3631> WB8TLI EN70 | 1409.4 | <...> WB8TLI EN70 | alias | no | no | 0 | 0 | 0.9974 | 0.3149 | 0.997 | 0.100 | +1.2 | -1800 | 0.10805 |
| ft8_20260530_154258 | <...3631> WZ8DX R+11 | 1321.9 | <...> WZ8DX R+11 | alias | no | no | 0 | 0 | 1.0000 | 0.9718 | 0.999 | 0.168 | -1.1 | -2040 | 0.02118 |
| ft8_20260530_154258 | <...3559> AC6F DM13 | 1850.0 | <...> AC6F DM13 | alias | no | no | 0 | 0 | 1.0000 | 0.9980 | 0.999 | 0.237 | +0.1 | -1920 | 0.00559 |
| ft8_20260530_154258 | <...476> K2BAG DM33 | 828.1 | <...> K2BAG DM33 | alias | no | no | 0 | 0 | 1.0000 | 0.9972 | 0.998 | 0.150 | +1.5 | -2400 | 0.00674 |
| ft8_20260530_154328 | <...3559> AC6F DM13 | 1850.0 | <...> AC6F DM13 | alias | no | no | 0 | 0 | 1.0000 | 0.9996 | 0.998 | 0.300 | +0.1 | -1920 | 0.00251 |
| ft8_20260530_154328 | <...476> K2BAG DM33 | 828.1 | <...> K2BAG DM33 | alias | no | no | 0 | 0 | 1.0000 | 0.9990 | 1.000 | 0.202 | +1.3 | -2280 | 0.00392 |
| ft8_20260530_154328 | <...3631> WB8TLI EN70 | 1271.9 | <...> WB8TLI EN70 | alias | no | no | 0 | 0 | 0.9992 | 0.3657 | 0.999 | 0.098 | +0.9 | -1560 | 0.09814 |
| ft8_20260530_160958 | <...3631> WF4RC EM65 | 1512.5 | <...> WF4RC EM65 | alias | no | no | 0 | 0 | 1.0000 | 0.9958 | 0.998 | 0.239 | -0.6 | -2040 | 0.00616 |
| ft8_20260530_160958 | <...3631> K0VM EN42 | 2687.5 | <...> K0VM EN42 | alias | no | no | 0 | 0 | 0.9999 | 0.8702 | 0.999 | 0.078 | +0.0 | -1680 | 0.03700 |
| ft8_20260530_160958 | <...3631> W2HAC FM18 | 790.6 | <...> W2HAC FM18 | alias | no | no | 0 | 0 | 1.0000 | 0.9993 | 0.998 | 0.597 | -0.3 | -2160 | 0.00232 |
| ft8_20260530_161058 | <...3631> K0VM R+04 | 2687.5 | <...> K0VM R+04 | alias | no | no | 0 | 0 | 1.0000 | 0.9874 | 1.000 | 0.157 | -0.1 | -1680 | 0.01109 |
| ft8_20260530_161058 | <...673> AC7WY DN61 | 2246.9 | <...> AC7WY DN61 | alias | no | no | 0 | 0 | 1.0000 | 0.9996 | 1.000 | 0.559 | +0.9 | -2400 | 0.00186 |
| ft8_20260530_161928 | <...442> KJ5DZV -15 | 1340.6 | <...> KJ5DZV -15 | alias | no | no | 0 | 0 | 1.0000 | 0.9767 | 0.999 | 0.329 | -1.2 | -1800 | 0.01792 |
| ft8_20260530_163513 | <...2829> KD9UJM EM78 | 1546.9 | <...> KD9UJM EM78 | alias | no | no | 0 | 0 | 1.0000 | 0.9986 | 1.000 | 0.345 | -0.6 | -2040 | 0.00319 |
| ft8_20260530_163513 | AJ4W <...3631> -14 | 2493.8 | AJ4W <...> -14 | alias | no | no | 0 | 0 | 0.9999 | 0.5279 | 1.000 | 0.050 | +1.0 | -1560 | 0.07100 |
| ft8_20260530_163513 | <...2829> K7AC R-10 | 865.6 | <...> K7AC R-10 | alias | no | no | 0 | 1 | 1.0000 | 0.9968 | 1.000 | 0.408 | +1.3 | -2160 | 0.00553 |
| ft8_20260530_171958 | <...3631> KA4UPC R+12 | 996.9 | <...> KA4UPC R+12 | alias | no | no | 0 | 1 | 1.0000 | 0.9966 | 1.000 | 0.326 | +0.9 | -1920 | 0.00702 |
| ft8_20260530_171958 | <...3631> K2BLA EL99 | 2053.1 | <...> K2BLA EL99 | alias | no | no | 0 | 0 | 1.0000 | 0.9128 | 1.000 | 0.235 | +0.1 | -1560 | 0.03644 |
| ft8_20260530_174028 | <...3462> KF9UG EN71 | 2565.6 | <...> KF9UG EN71 | alias | no | no | 0 | 0 | 1.0000 | 0.9987 | 0.999 | 0.333 | -0.4 | -1320 | 0.00411 |
| ft8_20260530_174028 | <...673> K6AGA DM04 | 1275.0 | <...> K6AGA DM04 | alias | no | no | 0 | 0 | 1.0000 | 0.9998 | 0.997 | 0.481 | -1.3 | -1800 | 0.00166 |
| ft8_20260530_185458 | <...3631> KI4YDQ EM95 | 1784.4 | <...> KI4YDQ EM95 | alias | no | no | 0 | 0 | 1.0000 | 0.9927 | 1.000 | 0.271 | +0.8 | -2160 | 0.01029 |
| ft8_20260530_185458 | <...3631> KK4VKM EM76 | 2800.0 | <...> KK4VKM EM76 | alias | no | no | 0 | 0 | 1.0000 | 0.9996 | 0.999 | 0.207 | -0.8 | -1560 | 0.00246 |
| ft8_20260530_185458 | <...3631> KC4OBY EM74 | 1021.9 | <...> KC4OBY EM74 | alias | no | no | 0 | 0 | 1.0000 | 0.9858 | 0.999 | 0.507 | -0.3 | -1440 | 0.01417 |

## Aggregate

- Pairs in work list: 70; neighbor not in greedy decode: 16; miss already in greedy decode: 0; encode/fit failed: 0.
- **Tried: 54** (0 genuine co-channel, 54 alias).
- Recovered, global round: 0. Serendip/FP global: 1/0 (0 of the FPs are hash-display aliases of truth messages).
- **Recovered, refined round: 0** (0 genuine, 0 alias). Serendip/FP refined: 2/5 (0 of the FPs are hash-display aliases of truth messages).
- Recovery rate (refined), all tried: 0.000 (0/54).
- **Recovery rate (refined), genuine pairs: 0.000 (0/0)** — the kill-switch number.
- Mean residual-energy ratio (full window): global 0.9999, refined 0.9214 (1.0 = subtract removed nothing; lower = better fit).
- Mean victim-band energy ratio after/before (neighbor band +/- 10 Hz over the fit window): global 0.999, refined 0.262 — the spec's mechanism evidence. ~1.0 means the precision subtract never removed the neighbor's energy at the victim's bins.

## Premise audit (full 5/30 scan x ft8_lib truth, hash-normalized)

- 2066 slots, 39668 truth decodes.
- Misses at tol=0 exact text: 1766 (the Batch 85 population).
- Misses after hash normalization: 1000 — **766 of 1766 (43.4%) of nominal misses are display aliases of pancetta's own decodes** (the Batch 85 "within one tone spacing of a decoded signal" premise was dominated by misses at delta-f = 0 from themselves).
- Genuine misses within 6.25 Hz / 2 s of a decoded signal: 55 (5.5%); of a TRUTH-CONFIRMED decoded signal: 0 (0.0%).
- Genuine misses within 25 Hz / 2 s of a decoded signal: 90 (9.0%); of a TRUTH-CONFIRMED decoded signal: 6 (0.6%).

## Decision (pre-registered criteria)

PROCEED >= 5% genuine recovery with FPs <= 1 per recovered TP; WEAK-PROCEED 2-5%; SHELVE < 2% after refinement or FP cost > 1/TP.

**SHELVE — zero valid co-channel targets: every tried pair's "miss" is a hash-display alias of the subtracted neighbor itself, and the premise audit shows the Batch 85 co-channel population was a truth-matching artifact (0 genuine misses within 6.25 Hz of a truth-confirmed decode on the full 5/30 corpus). The mechanism hb-104 targets does not exist at measurable frequency in this data.**

## Side findings (mechanism evidence for the journal)

1. **The precision LS subtract itself works**: refined fits remove most of the neighbor's band energy (mean victim-band ratio 0.262; best pairs reach <0.05) once the time search is wide enough. The one-step ALS machinery is sound — it is the target population that is empty.
2. **Pancetta's reported time_offset is coarse**: the `--dt-scan` diagnostic and the refined-fit dt distribution show decodes reporting dt up to ~0.2 s from the sample-accurate signal position (LDPC tolerates it; coherent processing does not). Any future coherent mechanism (hb-090 matched filter, subtract refinement) must re-search time locally. The fixed dt cluster near -0.13..-0.19 s on these slots also suggests a systematic component worth checking in the sync chain.
3. **Hash-display truth-matching artifact**: tol=0 exact-text scoring against ft8_lib truth double-penalizes every pancetta decode of an unresolved hashed callsign (1 phantom miss + 1 phantom FP). This contaminated Batch 85's premise and likely deflates TP counts in every ft8_lib-truth eval that includes hashed messages. Eval tooling should normalize `<...>`/`<...NNNN>` tokens before set intersection.
