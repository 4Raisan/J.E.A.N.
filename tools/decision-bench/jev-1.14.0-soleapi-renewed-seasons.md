# Jev 1.14.0 via SoleAPI · Renewed seasons

All 78 benchmark requests succeeded through SoleAPI's `/v1/systemone` using native Windows curl, and every response reported `jev-1.14.0`. Seasons 1 and 2 each use 14 cases. Season 3 combines two sequential runs of the same 25 cases. Median times include the local curl process and API round trip and exclude pacing. GUI cases used saved or simulated states.

| Renewed season | Routing | GUI | Overall accuracy | Median response | Input + output tokens |
|---|---:|---:|---:|---:|---:|
| 1 | 7/7 | 6/7 | 13/14 · 93% | 0.61 s | 6,592 + 971 |
| 2 | 7/7 | 6/7 | 13/14 · 93% | 0.58 s | 6,592 + 971 |
| 3 · Both rounds | 26/26 | 22/24 | 48/50 · 96% | 0.61 s | 19,672 + 2,826 |

- season-1, Notepad. Selected File menu. Expected Settings button.
- season-2, Notepad. Selected File menu. Expected Settings button.
- season-3-round-1, wrong process identity. Selected App B > Settings. Expected Abstain.
- season-3-round-2, wrong process identity. Selected App B > Settings. Expected Abstain.
