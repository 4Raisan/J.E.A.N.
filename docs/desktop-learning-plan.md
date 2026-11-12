# J.E.A.N. desktop learning

J.E.A.N. learns reusable actions by storing structured local workflows. A workflow records an action and its arguments. A future workflow can also record how to resolve its target and how to verify the result. External coding agents can build and test this feature with synthetic data.

## Try the first stage

- Type `Remember workflow morning as open YouTube; check my mail; what time is it`.
- Say or type `Run workflow morning`.
- After a browser, Gmail, or clock action, say `Remember this as quick check`.
- Say `Open it again` to reopen the most recent app or website in this session.
- Use `List workflows` or `Forget workflow morning` to manage saved entries. Teaching the same name replaces its steps.
- Use `Show workflow morning` to inspect the saved steps and `Undo workflow morning` to restore the previous revision.
- Type `Remember workflow clips as search {query} on YouTube`, then say `Run workflow clips with query window automation`. A workflow can also use `open {url}` and accept `with url https://example.com`. Supply the input before any step runs.
- Say `Remember that my project uses Rust`, `What do you remember about my project?`, or `List memories` to use the local Rust fact store. Saved facts are retrieved on request.

Workflows persist across restarts on this Windows profile. Each contains up to five browser, Gmail, or clock actions and can reuse one input type. Up to eight revisions are retained. Saving runs zero actions. Replay stops when a handler reports failure. A browser or app handoff records that Windows accepted the launch request. Independent window readback is a later stage.

## Session 1 — Qoder, baseline and regression tests

> Work on J.E.A.N.'s Windows desktop learning feature. J.E.A.N. is an orchestrator that can call installed apps and delegate implementation to existing coding agents. Read this plan, the overlay, and the Rust memory and desktop entry points. Use disposable profiles and synthetic workflows only. Write regression tests that cover teach-without-execute, same-name replacement, recall after restart, case-insensitive recall, forgetting, malformed saved records, and a failed middle step preventing the remaining steps. Test that ordinary Chat, Wake, clock, Gmail, and browser routes still work. Report commands, exit codes, and actual versus simulated checks. Preserve existing changes.

## Session 2 — Codex, reusable workflows across contexts

Design a typed workflow schema in the existing Rust controller and a migration for desktop-profile workflows. Support explicit inputs, workspace scope, stable app identities, versioned corrections, and per-step verification. Add a teach-by-demonstration route that saves only the selected observed actions, with a preview. On replay, resolve targets again against the current desktop and verify each step before continuing. Pause on a missing or ambiguous target and preserve progress. Use the decision model to suggest bindings for ambiguous inputs, then validate its suggestions against the current task and available actions.

Acceptance requires teaching in one fixture and replaying in a second fixture with different window handles, screen position, and test input. A renamed or missing target must produce a recoverable stop. A crash between performing and verifying a step must preserve uncertainty without repeating the action.

## Session 3 — Qoder, holdout evaluation

> In a separate scratch profile, build an evaluation set independently of the implementation fixtures. Cover paraphrases, app aliases, follow-ups, changed input values, a second workspace, stale app identities, focus changes, corrections, cancellation, and restart. Measure retrieval success, verified completion, wrong-target or duplicate actions, p50/p95 latency, and sample counts. Compare exact retrieval first, then any proposed model-assisted approach on identical cases. Record failures with the shortest reproducible scenario. Use the requested Qwen model only after verifying its exact name in Qoder's model picker.

## Session 4 — Codex, learning from results

Record dispatch, verified success, failure, and uncertainty separately. A successful run can suggest an updated workflow. Promote the update only after a correction or independent verification and keep the previous version recoverable. Use the evaluation results to decide whether stronger retrieval or model assistance improves real completion.
