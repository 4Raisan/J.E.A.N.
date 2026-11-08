# JEAN delivery plan for Luna

Reviewed 2026-11-08. This is the current execution order. The roadmap retains phase history and the implementation status retains evidence from earlier batches.

## Current baseline

| Area | Evidence | Remaining proof |
|---|---|---|
| Core, model plans, agents and browser | Earlier handoffs report working task supervision, structured planning and verified fixture workflows. | Establish a baseline on the current source. Real vendor sessions and browser teardown remain open unless newer evidence closes them. |
| Speech output | Kokoro generated an ordinary sentence to a non-silent WAV. The owner likes JEAN's speaking voice. | Preserve the working TTS path. Keep physical interruption and acoustic barge-in results separate. |
| Speech input | The tester captured the owner's speech. On the reported comparison JEAN produced “HYGIENE CAN YOU OPEN THIS BODY” and Wispr inserted “Hi G, can you open Spotify?” | Use Wispr or typed input for the owner workflow. Native ASR needs a controlled comparison on the owner's speech before it becomes the default. |
| Memory | Phase 5 shipped searchable history, explicit facts and preferences. | Prove scoped learned preferences affect planning and quality checks. |
| Owner workflow | `crates/jean-core/src/owner.rs` contains checkpoint helpers. No callers were found in crates or apps. The current filename sanitizer can collapse distinct IDs and writes overwrite the file directly. | Wire a durable checkpoint into one real handoff. Prove restart, identity and no replay. |
| Recent external edits | Source includes an immediate SQLite migration transaction, RapidOCR sidecar, tray/elevation workspace members, and installer and benchmark sources. | Inspect each normal entry point and its evidence before calling these complete. |
| Build environment | About 3.05 GiB free on C:. Full offline locked Cargo metadata exited 101 while attempting to fetch uncached `atk 0.18.2`. | Resolve the selected Windows target and inspect the dependency chain. The all-platform metadata result alone does not prove a Windows build failure. |

The workspace has no Git baseline at review time. File presence and timestamps do not establish authorship or successful tests. Start from the current tree and preserve external edits.

## Architecture and decision models

Keep the Rust core responsible for authority, state, budgets, cancellation, and verification. Use deterministic routing for quota, deadlines, disk and known failure types. A planner can propose an approach and a worker can implement it. The core checks evidence before advancing. Luna in Codex can coordinate OpenCode during development. JEAN should eventually run the loop through its own structured adapters, with durable state in JEAN and working context in the agent session.

| Candidate | Disposition and test |
|---|---|
| Rust rules | Adopt as baseline. Test quota switches, context restart, disk pause and eligibility without any model call. |
| TypeSafe Jev / jev-ultrafast | Evaluate after the owner loop works. The repository demonstrates selecting an operation and an indexed target from observed browser controls. Its documented setup uses hosted inference. Reuse that pattern with the current browser/UIA executor. Treat Jev itself as an optional remote candidate. [Upstream design](https://github.com/browser-use/jev-ultrafast/blob/main/docs/design.md) · [Setup](https://github.com/browser-use/jev-ultrafast#try-it) |
| Chif | Unidentified. Require the exact repository or model card before judging license, local availability or performance. |
| Small local decision model | Candidate for ambiguous action selection and failure triage. Run on demand through a worker. Compare against rules and the current planner before adopting. |

Give experimental selectors a bounded list of legal action IDs and an abstain option. Revalidate action, target, authority, and freshness in Rust. A selector cannot issue commands or grant permissions. Compare identical observations against the baseline using correct action rate, wrong target rate, abstentions, p50/p95 end-to-end latency, cold start, peak RAM, disk and cost. Include stale and hostile page content and no-valid-action examples. Record an acceptance budget before testing. Keep the rules if a candidate fails to improve the measured workflow within budget.

## Open-source adoption gate

Reuse existing project interfaces first. For each candidate record the upstream URL, pinned revision, release and maintenance evidence, relevant open defects, code license, weight and asset licenses, and transitive dependencies. Verify the exact artifact and hashes where published. Read the actual API and build scripts. Check for implicit downloads, extra runtimes, telemetry, credentials and changes to Windows privileges.

Run the smallest Windows prototype with real inputs. Record correctness, failure handling, cancellation, teardown, cold/warm latency, peak private bytes and working set, CPU under normal workload, downloaded/extracted/build size, and offline behavior. Set budgets for the intended role before measuring. Compare to the existing path and keep a fallback. Use the verdicts candidate, trial, adopted or rejected. A README, benchmark on someone else's machine, successful compilation, or model card alone is insufficient for adoption.

## Execution order

1. **Reconcile the current tree.** Read sections 63–65 of the handoff and inspect the external changes. Locate the VS developer wrapper and Rust binaries. Measure free space and check the Windows dependency tree. Run a targeted locked check for affected crates if feasible. Separate observed build results from earlier reports. Stop disk-heavy work before the volume becomes critical. Preserve artifacts and user edits.
2. **Ship one owner workflow.** Accept a short typed or Wispr-dictated goal and an optional starting agent. Reuse the current plan, authorization, delegation and verification paths. Use a scratch workspace. Prove a single Codex-first or OpenCode-first handoff with a structured worker, independent verification and visible status. Use a fixture when live credentials or quota are unavailable. Keep one active writer.
3. **Wire checkpoint and fallback.** Validate IDs without lossy mapping. Prefer an existing SQLite transaction for bounded, redacted checkpoints or use atomic replacement with recovery tests. Save resolved scope, worker, changed-state evidence, verified results and next step. Restore in a fresh process. Simulate quota exhaustion after partial work and resume using an eligible worker. Reconcile uncertain effects before retrying. Test context exhaustion, missing sign-in, network loss, and disk pause. Keep paid workers opt-in and emit only actionable alerts.
4. **Make personalization observable.** Start with two explicit profiles and one identical frontend goal. Show different acceptance checks for a prototype versus polished UI, and prove current instructions override learned defaults. Then introduce inspectable, scoped preference candidates from repeated owner feedback. Keep raw history, secrets and model instructions out of learned authority. Provide correction, forget and learning controls.
5. **Improve native speech input independently.** Keep the current TTS and tester. Wispr remains a usable input path. Confirm selected mic and capture boundaries with a consented recording, including clipped starts and ends. Compare Zipformer with Sherpa's Parakeet TDT v2 INT8 offline path on identical recordings of the owner. Score exact app names and developer terms, accuracy, release-to-text latency and resources. Use a hosted ASR comparison only if approved. Keep transcript review for uncertain targets. Promote a model only after it meets the recorded quality and resource budget. [Sherpa Rust example](https://github.com/k2-fsa/sherpa-onnx/blob/master/rust-api-examples/run-nemo-parakeet-en.sh)
6. **Trial decision models.** Apply the adoption gate and bounded selector benchmark above. Keep trials out of the critical path.
7. **Close and package.** Verify the concurrent database startup fix, browser teardown, installer/tray/elevation behavior, and dependencies and model redistribution. Run one final workspace validation on the final source with sufficient space, `CARGO_INCREMENTAL=0` and `-j 2`. Record skips and machine requirements. Packaging readiness requires an install and uninstall rehearsal on a clean Windows test environment.

## First prompt for Luna

Use this as the task message in a Codex chat running Luna.

```text
Implement one JEAN owner workflow in this workspace. Read
PLANNING/08-OWNER-WORKFLOW.md and steps 1–2 of PLANNING/09-LUNA-EXECUTION-PLAN.md
for context. Preserve existing edits. Work directly in Codex.

Inspect only the normal CLI entry point and existing worker and policy path.
Check free disk space before compiling. If an offline check fails on atk,
inspect whether it belongs to the Windows target before treating it as a blocker.

Wire a short goal and an optional starting agent through the normal JEAN CLI.
Demonstrate one OpenCode-first handoff in a scratch workspace using a structured
fixture that makes a small file change. Have JEAN independently verify the
expected file contents and expose the result through its normal status output.
Keep one active writer. A fixture proves the JEAN handoff only. A live OpenCode
run is optional and requires existing authorization for any quota use.

Run one focused check of the affected Windows target if disk and cached
dependencies permit. Report the exact command and outcome, whether the handoff
used a fixture or live worker, and any specific blocker. Stop after this slice.
Leave checkpoints, fallback, voice, OCR, and packaging for later batches.
```
