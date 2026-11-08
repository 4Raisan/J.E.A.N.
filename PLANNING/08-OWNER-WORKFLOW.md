# Owner workflow and personalization

**Status: started in source.** Checkpoint helpers exist and have no execution caller as of the baseline review. Build the first workflow with typed or Wispr-dictated goals while native speech quality is evaluated. Follow the [Luna execution plan](09-LUNA-EXECUTION-PLAN.md) before Phase 6 release readiness.

## Give JEAN the goal

A short request is the normal entry point. For example, “Start with Codex. Improve the dashboard for mobile and keep the current visual style. Use free models for follow-up.” The owner connects agents and sets model eligibility and spending preferences once. JEAN discovers project context, chooses a plan, writes worker prompts, delegates, and verifies the results. “Start with Codex” selects the first substantive worker. Later routing can use another eligible worker when appropriate.

## Learn what quality means to the owner

Build on Phase 5's saved facts, preferences, and history. Keep explicit facts, learned preference candidates, project conventions, and observed workflow outcomes distinguishable. Record the source and scope of each learned preference. Repeated feedback can promote a candidate into an editable default. A one-off instruction stays with its task. The owner can inspect, correct, forget, or turn off learning. Keep profiles separate across owners and workspaces. Retrieve only context relevant to the current goal.

Current instructions take precedence over learned defaults. Project requirements still apply. Model output and observed content can provide evidence, but cannot authorize actions, change spending rules, or become owner preferences by themselves. Improve future choices through feedback and recorded outcomes. Changes to JEAN's code or policy remain ordinary explicitly scoped tasks.

## Delegate and recover

Start with one supervised reviewer and implementer loop in an isolated workspace. Use supported structured agent interfaces. JEAN drafts prompts and returns actual diffs, test results, and unresolved findings for the next decision. Check the workspace and persist the goal, resolved scope, results, and next action before switching sessions. Reconcile uncertain effects before retrying.

Handle context exhaustion with a compact checkpoint and a new session. On a known quota failure, try the next eligible model within the standing budget. Keep premium models reserved according to owner policy. Use deterministic checks for disk, connectivity, deadlines, and eligible model selection. A fast local or hosted model may help with triage and prompt drafting. No decision model is required for the first milestone.

Record routine recovery in activity history. When storage is critical, pause disk-heavy work and notify the owner with measured free space and the blocked operation. When sign-in or network access is missing, continue useful local work or try an eligible fallback. Save progress and surface a blocker when owner action is necessary. Bound retries and stop when verification succeeds or recovery options are exhausted.

## Demonstration

1. Give JEAN a short goal and a starting agent. Verify that it writes and delivers the prompt, inspects changes, and runs the configured checks without manual relay.
2. Simulate quota and context exhaustion after partial work. Verify that an eligible second model continues from the checkpoint without losing changes, repeating uncertain actions, or spending beyond policy.
3. Run the same frontend goal with two profiles. Show how a quick prototype and a polished frontend differ in planned work and verification. Show which preferences led to those choices, and verify a new explicit instruction overrides them.
4. Simulate critical disk, unavailable sign-in, and network loss. Verify continued local work where possible, and a clear resumable blocker where necessary.

**Keywords:** goal-driven orchestration, owner profile, project quality standards, agent handoff, automatic model fallback, checkpointed recovery.
