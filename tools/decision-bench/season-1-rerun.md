# Season 1 · Partial-result rerun

Fresh pass of the original seven routing and seven GUI cases, limited to the three models marked partial. All 42 requests returned HTTP 200 and valid choices. Median API response time excludes pacing. GUI cases use saved controls without executing actions.

| Model | Routing | GUI | Overall accuracy | Median response | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| Perplexity Decider v1.2 | 7/7 | 7/7 | 14/14 · 100% | 0.70 s | 3,114 + 14 |
| Perplexity Decider Flash v1.2 | 7/7 | 6/7 | 13/14 · 93% | 0.67 s | 3,114 + 14 |
| Liquid d1-Ultra | 7/7 | 7/7 | 14/14 · 100% | 0.89 s | 2,225 + 0 |

Perplexity Decider Flash selected the disabled Submit button where the expected action was to abstain and ask.
