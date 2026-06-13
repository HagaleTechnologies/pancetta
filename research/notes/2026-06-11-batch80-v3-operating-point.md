# Batch 80 — hb-103 v3 operating point for the autonomous CQ gate

## hard_200 — CQ-only (gate population)

1107 TPs / 305 FPs

| Gate | τ | TP recall | FP rejection |
|---|---:|---:|---:|
| v1 (production) | 0.352 | 1.0000 | 0.1148 |
| v3 @ 100%-recall τ | 0.666 | 1.0000 | 0.1738 |

## hard_200 — all decodes

4468 TPs / 1514 FPs

| Gate | τ | TP recall | FP rejection |
|---|---:|---:|---:|
| v1 (production) | 0.352 | 1.0000 | 0.1863 |
| v3 @ 100%-recall τ | 0.666 | 1.0000 | 0.2602 |

## raw_530 subset-200 — CQ-only (gate population)

808 TPs / 155 FPs

| Gate | τ | TP recall | FP rejection |
|---|---:|---:|---:|
| v1 (production) | 0.352 | 1.0000 | 0.0387 |
| v3 @ 100%-recall τ | 0.182 | 1.0000 | 0.0645 |

## raw_530 subset-200 — all decodes

3427 TPs / 834 FPs

| Gate | τ | TP recall | FP rejection |
|---|---:|---:|---:|
| v1 (production) | 0.352 | 1.0000 | 0.0168 |
| v3 @ 100%-recall τ | 0.182 | 1.0000 | 0.0348 |

## Cross-corpus-safe τ* = 0.182 (CQ population)

         | Corpus | v1 FP rejection | v3 @ τ* recall | v3 @ τ* FP rejection |
|---|---:|---:|---:|
| hard_200 | 0.1148 | 1.0000 | 0.1180 |
| raw_530 subset-200 | 0.0387 | 1.0000 | 0.0645 |

