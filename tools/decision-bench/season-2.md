# Season 2

18 models attempted. Same 14 cases, one pass per model. Model runs overlapped. Latencies are medians of successful API calls and exclude pacing. HTTP 403 indicates free-tier access refusal. HTTP 503 indicates the upstream provider could not serve the model.

| Model | Routing | GUI | Overall | Median (s) | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| Mammoth-1 | 7/7 | 7/7 | 14/14 · 100% | 0.71 | 3,292 + 14 |
| Mercury Decide | 7/7 | 7/7 | 14/14 · 100% | 0.70 | 2,842 + 14 |
| Mercury 3 | 7/7 | 7/7 | 14/14 · 100% | 1.02 | 1,921 + 2,134 |
| GPT-6 Luna Decisions Free | 7/7 | 7/7 | 14/14 · 100% | 1.24 | 3,469 + 0 |
| Drex Max v1.1 | 7/7 | 7/7 | 14/14 · 100% | 0.92 | 3,114 + 2,811 |
| SubQ Snap 1.1 | 7/7 | 7/7 | 14/14 · 100% | 0.93 | 3,114 + 0 |
| Elephant-1 | 7/7 | 7/7 | 14/14 · 100% | 0.71 | 3,292 + 14 |
| Jev Latest | 7/7 | 6/7 | 13/14 · 93% | 0.89 | 6,592 + 971 |
| Clef | 7/7 | 6/7 | 13/14 · 93% | 1.56 | 4,207 + 0 |
| Celeris-1 | 7/7 | 5/7 | 12/14 · 86% | 1.05 | 1,821 + 114 |
| Drex v2.0 | 7/7 | 5/7 | 12/14 · 86% | 0.85 | 2,178 + 1,467 |
| Laya · hosted | 6/7 | 1/7 | 7/14 · 50% | 1.50 | 2,302 + 0 |
| Perplexity Decider Flash v1.2 | 7/7 | 6/7 | 13/14 · 93% | 0.65 | 3,114 + 14 |
| Perplexity Decider v1.2 | 7/7 | 7/7 | 14/14 · 100% | 0.74 | 3,114 + 14 |
| Liquid d1-Ultra | 7/7 | 6/7 | 13/14 · 93% | 0.88 | 2,225 + 0 |
| Liquid d1 | — | — | HTTP 403 | — | — |
| Microsoft Decision-1 | — | — | HTTP 403 | — | — |
| Clef Flash | — | — | HTTP 503 | — | — |
