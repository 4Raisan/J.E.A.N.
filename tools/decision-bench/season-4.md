# Season 4 · JEAN decision benchmark

The nine Season 3 models each answered 25 cases per round, for 450 scored decisions. The cases cover 13 routing and recovery decisions and 12 Windows UI decisions using synthetic workflow states. Round 1 finished before Round 2 began. Five Vercel requests were initially rejected with HTTP 429 at the start of Round 2. Those cases resumed after cooldown and every model completed all 25 cases. No real UI actions were executed.

Median response is the API round trip for completed decisions, excluding pacing and rate-limit waits. The merged median pools all 50 completed response times per model. Token totals are provider-reported. The bounded CSS case expects Codex Medium, but its wording leaves Claude Code's effort unspecified, so misses on that case are sensitive to the answer key.

## Round 1

| Model | Routing | GUI | Overall accuracy | Median response | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| Perplexity Decider Flash v1.2 | 11/13 | 11/12 | 22/25 · 88% | 0.60 s | 3,888 + 25 |
| Mercury Decide | 12/13 | 11/12 | 23/25 · 92% | 0.54 s | 3,804 + 25 |
| Mammoth-1 | 11/13 | 12/12 | 23/25 · 92% | 0.63 s | 4,113 + 25 |
| Elephant-1 | 12/13 | 12/12 | 24/25 · 96% | 0.62 s | 4,113 + 25 |
| Perplexity Decider v1.2 | 12/13 | 12/12 | 24/25 · 96% | 0.67 s | 3,888 + 25 |
| Jev Latest | 13/13 | 11/12 | 24/25 · 96% | 1.01 s | 9,836 + 1,413 |
| Drex Max v1.1 | 13/13 | 12/12 | 25/25 · 100% | 0.92 s | 3,888 + 3,816 |
| SubQ Snap 1.1 | 12/13 | 12/12 | 24/25 · 96% | 0.85 s | 3,888 + 0 |
| GPT-6 Luna Decisions Free | 13/13 | 12/12 | 25/25 · 100% | 1.45 s | 4,612 + 0 |

## Round 2

| Model | Routing | GUI | Overall accuracy | Median response | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| Perplexity Decider Flash v1.2 | 11/13 | 11/12 | 22/25 · 88% | 0.59 s | 3,888 + 25 |
| Mercury Decide | 12/13 | 11/12 | 23/25 · 92% | 0.57 s | 3,781 + 25 |
| Mammoth-1 | 11/13 | 12/12 | 23/25 · 92% | 0.64 s | 4,113 + 25 |
| Elephant-1 | 12/13 | 12/12 | 24/25 · 96% | 0.63 s | 4,113 + 25 |
| Perplexity Decider v1.2 | 12/13 | 12/12 | 24/25 · 96% | 0.69 s | 3,888 + 25 |
| Jev Latest | 13/13 | 11/12 | 24/25 · 96% | 1.12 s | 9,836 + 1,413 |
| Drex Max v1.1 | 13/13 | 12/12 | 25/25 · 100% | 0.93 s | 3,888 + 3,816 |
| SubQ Snap 1.1 | 12/13 | 12/12 | 24/25 · 96% | 0.85 s | 3,888 + 0 |
| GPT-6 Luna Decisions Free | 13/13 | 12/12 | 25/25 · 100% | 1.24 s | 4,612 + 0 |

## Merged Season 4

| Model | Routing | GUI | Overall accuracy | Median response | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| Perplexity Decider Flash v1.2 | 22/26 | 22/24 | 44/50 · 88% | 0.60 s | 7,776 + 50 |
| Mercury Decide | 24/26 | 22/24 | 46/50 · 92% | 0.55 s | 7,585 + 50 |
| Mammoth-1 | 22/26 | 24/24 | 46/50 · 92% | 0.64 s | 8,226 + 50 |
| Elephant-1 | 24/26 | 24/24 | 48/50 · 96% | 0.63 s | 8,226 + 50 |
| Perplexity Decider v1.2 | 24/26 | 24/24 | 48/50 · 96% | 0.67 s | 7,776 + 50 |
| Jev Latest | 26/26 | 22/24 | 48/50 · 96% | 1.07 s | 19,672 + 2,826 |
| Drex Max v1.1 | 26/26 | 24/24 | 50/50 · 100% | 0.93 s | 7,776 + 7,632 |
| SubQ Snap 1.1 | 24/26 | 24/24 | 48/50 · 96% | 0.85 s | 7,776 + 0 |
| GPT-6 Luna Decisions Free | 26/26 | 24/24 | 50/50 · 100% | 1.35 s | 9,224 + 0 |

## Reproduce

Use native Windows Command Prompt from the JEAN root. Supply AI_GATEWAY_API_KEY, EXPERIENTIAL_API_KEY, SUBQ_API_KEY, and INCEPTION_API_KEY through the process environment.

```bat
py -3 -B tools\decision-bench\benchmark_season4.py "%TEMP%\jean-decision-season-4-new"
```

If Round 2 stops on HTTP 429, wait for the provider's limit to reset, then run the same command with `--resume-round-2-rate-limits`. Use a new output directory for a fresh run.
