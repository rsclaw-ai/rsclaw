# KB retrieval benchmark — fixed corpus + golden set

Reproducible retrieval-quality benchmark for the KB pipeline. Run it BEFORE
and AFTER any retrieval change (embedder swap, chunking, reranker, fusion
weights) — no ruler, no surgery.

## Contents

- `corpus/` — 54 synthetic Chinese company-report docs across 12 industry
  clusters (dairy, EV, baijiu, phones, banks, pharma, internet, appliances,
  F&B, logistics, semis, sportswear, airlines, energy). Clusters share
  vocabulary (营收/同比/净利润率/市占率) so intra-cluster confusion is real.
- `golden.json` — 216 queries, 4 per doc:
  1. direct (keyword query — BM25 should carry it)
  2. confusable (entity name removed; answerable only by a unique fact)
  3. paraphrase ×2 (reworded facts — this is where dense embedders
     differentiate; BM25 can't bridge "盈利能力" → "净利润率")

Each case asserts `expectTitleContains` (doc identity via filename) +
`expectTextContains` (exact substring of the target doc), both on the SAME
hit. Substrings are validated to exist in their target docs at generation
time.

## How to run

Use a throwaway profile so you never touch a real KB (the index must be
rebuilt per embedder — vectors are not comparable across models/dims):

```bash
# 1. point the profile's kb.embed at the embedder under test, then:
rsclaw --profile <test> gateway run &
rsclaw --profile <test> kb add tests/fixtures/kb-eval/corpus --recursive --ext md

# 2. WAIT until the index is complete — evaluating a partial index produces
#    garbage numbers (this exact mistake produced a bogus 78% during the
#    first bge-base run):
rsclaw --profile <test> kb stats   # chunkCount must equal 54

# 3. score:
rsclaw --profile <test> kb eval tests/fixtures/kb-eval/golden.json -k 5
```

## Reference scores (2026-06-11, hybrid BM25+dense, RRF+MMR, k=5)

| embedder | hit@1 | MRR | notes |
|---|---|---|---|
| bge-small-zh (512d, local) | 87.0% | 0.870 | reproduced twice; the default |
| Qwen3-0.6B + query instruction (1024d, remote) | 82.9% | 0.829 | healthy deployment |
| bge-base-zh (768d, local) | 81.9% | 0.819 | 3× params over small, loses anyway |
| Qwen3-0.6B, no instruction | 79.6% | 0.796 | instruction is worth +3.3 |

Caveat on the Qwen3 rows: this corpus is short docs (~1 chunk each), so
bge-small's 512-token input limit never bites. Real 研报 chunks at the
512-token chunk target + title prefix DO exceed it (tail silently truncated
at embed time) — that regime is where Qwen3 (32K input) should pull ahead
and is NOT yet covered by this golden set. Add long-doc cases before
treating bge-small as the final answer for production corpora.

Sanity checks this benchmark has caught: a remote endpoint returning
IDENTICAL vectors for different inputs (slot corruption — two different
queries scoring bit-identical against every doc), mean-pooling deployments
(instruction prefix actively hurts), and partial-index evaluation.

If you change the corpus or golden set, regenerate scores for every row —
numbers are only comparable within one corpus+golden revision.
