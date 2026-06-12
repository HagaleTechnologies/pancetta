# Batch 81 — decode_origin (hb-247) vs wall-clock lateness as the v3 feature

## hard_200

5830 decodes, 5830 origin-stamped (391 with origin > 0).

| Feature | mean held-out ΔAUC vs v2 | fold weights | solo AUC (inv) |
|---|---:|---|---:|
| decode_origin/6 | +0.0401 | [-2.0, -2.0] | 0.6167 |
| decode_time slot-frac | +0.0288 | [-1.0, -0.5] | — |

## raw_530 subset-200

4204 decodes, 4204 origin-stamped (171 with origin > 0).

| Feature | mean held-out ΔAUC vs v2 | fold weights | solo AUC (inv) |
|---|---:|---|---:|
| decode_origin/6 | +0.0470 | [-2.0, -2.0] | 0.5837 |
| decode_time slot-frac | +0.0199 | [-0.5, -0.5] | — |

## Operating points (CQ population, origin-v3 = v2 - origin/6, cross-corpus τ* = 0.965)

τ=0.35 keeps origin-0 decodes byte-identical to the v1 gate and only
tightens recovery-pass decodes; τ=0.90 / τ* raise the bar globally.

| Corpus | v1 @ 0.35 | origin-v3 @ 0.35 | origin-v3 @ 0.90 | origin-v3 @ τ* |
|---|---|---|---|---|
| hard_200 | 1.0000 / 0.1131 | 1.0000 / 0.1131 | 1.0000 / 0.1519 | 1.0000 / 0.1590 |
| raw_530 subset-200 | 1.0000 / 0.0350 | 1.0000 / 0.0490 | 1.0000 / 0.0979 | 1.0000 / 0.1189 |
