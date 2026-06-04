# hb-092 — Codeword-based NMS dedup (post-decode) — SHELVED

**Date**: 2026-06-04
**Branch**: iter/2026-06-04-hb-092-codeword-dedup-diag
**Status**: **SHELVED — definitive, mechanism-level**
**Effort**: ~30 minutes (diagnostic + run + journal)

## Question

The bank entry's premise: "Two decodes of the SAME codeword can have
different displayed `(freq, dt)` due to fractional-bin sync refinements;
the text-level dedup keyed on `unique_decoded: HashSet<String>` misses
these duplicates." If true, then codeword-binary dedup
(`HashSet<[u8; 174]>` or `HashSet<payload_bits>`) would catch a
precision-improvement headroom that text dedup leaves on the table.

Kill-switch: top-20 (later top-200) hard-200, group `DecodedMessage`s
by `message.payload_bits` (the canonical 91-bit FT8 payload). PROCEED
if ≥ 5% of novel decodes are codeword-duplicates with TF separation
(Δfreq ≥ 5 Hz OR Δdt ≥ 50 ms) from another decode in the same WAV.

## Result

**Zero codeword-duplicates on the entire hard-200 corpus.**

| Scope        | Total decodes | Distinct payloads | Codeword-dups (TF-distinct) | Novel-dups / novel |
|--------------|--------------:|------------------:|----------------------------:|-------------------:|
| top-20       |           723 |               723 |                           0 |              0.00% |
| top-200      |          6912 |              6912 |                           0 |              0.00% |

Every single `unique_decoded` output across both scopes has a 91-bit
payload distinct from every other output in the same WAV.

## Mechanism (why the premise is wrong)

`DecodedMessage.text` is produced by `Ft8Message::to_string()`, which
is a deterministic function of `Ft8Message.payload_bits`. Same payload
→ same text. The fractional-bin sync refinement that the bank entry
posited as a source of text-distinct-but-payload-identical decodes
perturbs the **display metadata** (`frequency_offset`, `time_offset`),
not the payload bits. Two decodes that produce the same 91 LDPC
information bits will always produce the same canonical text, and
will therefore always collide on the text key.

So pancetta's `unique_decoded: HashSet<String>` is functionally
equivalent to `HashSet<payload_bits>` for the purpose of
codeword-binary dedup, on this corpus.

## What this closes

The hb-036 SHELVE journal recommended codeword-binary dedup as "the
brute-force answer to duplicate-vs-distinct discrimination". This
diagnostic shows pancetta is **already doing** that comparison
implicitly via the text key. The "brute-force answer" gives nothing
new.

The hb-092 territory is closed.

## Edge cases not exercised here

1. **AP injection producing text variation** with same payload — would
   show up if the AP-injected branch fixes some bits but the resulting
   91-bit payload+CRC verifies as identical. This corpus has AP
   off-by-default (eval `Ft8Config::default()`). If a future iter ever
   enables AP in eval AND a payload-identical-but-text-variant case
   appears, hb-092 may want a quick re-run.
2. **Multiple slot-alignments** that happen to land on the same
   payload but with structurally different (freq, dt) bins. The
   text-from-payload map would still collapse them. No headroom here.
3. **`coherent_subtract_and_repass` step 4** producing residual
   re-decodes of an already-decoded signal. The text-level dedup
   collapses them; no headroom.

## What didn't move

- Production: no code change (research-only example added).
- Composite: not touched (research-only).
- Bank state: hb-092 flips pending → SHELVED. Counters: +1 shelf.

## Lessons

1. **The "brute-force answer" was already in production.** When a
   future hypothesis cites "just dedup by codeword" as a fix, check
   whether the text-level dedup already does this implicitly via a
   deterministic text-from-payload mapping. In pancetta it does.
2. **Kill-switches at hard threshold 5% are valuable for clear shelves.**
   0/6912 is a binary result — no fragility, no need to retest at
   wider scope. The diagnostic ran in ~3 min on M4 for top-200.

## Artifacts

- `pancetta-research/examples/hb092_codeword_dedup_diagnostic.rs` — the
  diagnostic (kept for any future re-test; cheap to run).
- `research/hypothesis_bank.md` — hb-092 marked SHELVED.
- This journal.

## Production impact

None. The hypothesis was about a precision-improvement headroom that
turns out to not exist on this corpus.
