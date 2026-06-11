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

## Reference scores (2026-06-11, CJK-calibrated chunker, mixed 62-doc index, k=5)

Short set (216 cases) / Long set (40 cases), hit@1 unless noted:

| config | short hit@1 | long hit@1 | long hit@5 |
|---|---|---|---|
| bge-small + bge-reranker-v2-m3 | **96.8%** | 67.5% | 67.5% |
| Qwen3-0.6B + instruction + reranker | 94.9% | **70.0%** | **87.5%** |
| bge-small (no rerank) | 87.0% | 30.0% | 30.0% |
| Qwen3-0.6B + instruction (no rerank) | 82.9% | 40.0% | 40.0% |

Production recommendation: bge-small local embeddings + remote
bge-reranker-v2-m3 (kb.rerank). The reranker is the dominant lever
(+10-12 short, 2x+ long); the embedder choice is secondary once the
chunker is CJK-calibrated. Long-set absolute numbers are pessimistic —
the synthetic long docs share identical filler paragraphs, which
confuses every embedder; relative comparisons remain valid.

History (pre-CJK-chunker, 54-doc index): bge-small 87.0 / bge-base 81.9 /
qwen3+inst 82.9 / qwen3 79.6 on the short set. The pre-fix mixed index
collapsed to 0.5% (giant-chunk hub vectors + RRF double-counting) —
see git history for the full forensics.

## Long-doc set (`corpus-long/` + `golden-long.json`)

8 docs at ~1620 CJK chars (production-sized single chunks under the current
chars/4 token estimate) with facts planted at front (<400 chars), mid, and
tail (>900 chars) positions; 40 paraphrase-leaning queries. Ingest TOGETHER
with `corpus/` (62 docs total) so short docs act as distractors.

Scores (2026-06-11, hybrid, k=5, run with the 54 short docs as distractors):

| embedder | hit@1 | hit@5 | MRR |
|---|---|---|---|
| bge-small-zh | 20.0% | 20.0% | 0.200 |
| Qwen3-0.6B + instruction | 5.0% | 65.0% | 0.251 |

Two findings this set exposed:
1. bge-small embeds only the first 512 real tokens (~512 CJK chars) of a
   chunk — on production-sized Chinese chunks it is blind to 3/4 of the
   text and COLLAPSES (the short-doc set never triggers this).
2. The chunker token estimate (chars/4) is English-calibrated: for Chinese
   it yields ~2048-char chunks, 4x the intended 512 tokens. Recalibrating
   CJK chunk sizing is the upstream fix; a cross-encoder reranker is the
   query-time mitigation for Qwen3's rank-1 weakness (65% hit@5 vs 5% hit@1).

Sanity checks this benchmark has caught: a remote endpoint returning
IDENTICAL vectors for different inputs (slot corruption — two different
queries scoring bit-identical against every doc), mean-pooling deployments
(instruction prefix actively hurts), and partial-index evaluation.

If you change the corpus or golden set, regenerate scores for every row —
numbers are only comparable within one corpus+golden revision.
