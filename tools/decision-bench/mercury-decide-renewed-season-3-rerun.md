# Mercury Decide · Renewed Season 3 rerun

Two sequential passes of the existing 25-case suite via Inception's Decisions API, requesting `mercury-decide`. All 50 calls returned valid choices. The existing adapter does not capture the response model ID. Median response measures client API round trip. GUI decisions use simulated states.

| Run | Routing | GUI | Overall accuracy | Median response | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| Pass 1 | 12/13 | 11/12 | 23/25 · 92% | 0.59 s | 3,789 + 25 |
| Pass 2 | 12/13 | 11/12 | 23/25 · 92% | 0.54 s | 3,776 + 25 |
| Combined | 24/26 | 22/24 | 46/50 · 92% | 0.55 s | 7,565 + 50 |

- Pass 1, bounded polish. Selected Claude Code. Expected Codex Medium.
- Pass 1, browser tab identity. Selected Abstain. Expected Specified PR > Checks.
- Pass 2, bounded polish. Selected Claude Code. Expected Codex Medium.
- Pass 2, browser tab identity. Selected Abstain. Expected Specified PR > Checks.
