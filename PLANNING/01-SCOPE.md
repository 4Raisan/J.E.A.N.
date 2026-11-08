# 01 — Scope

The planned [owner workflow milestone](08-OWNER-WORKFLOW.md) extends the single delegated workflow into an unattended loop with checkpointed recovery and user-specific quality defaults.

> What J.E.A.N. is, what it is not, and the single workflow that proves it works.

---

## The product in one sentence

J.E.A.N. is a **lightweight Rust orchestrator** that sits between the user and the tools already on their PC — it interprets an outcome, picks the right tool, supervises execution, and verifies the result.

It is not an agent that does the work. It is the layer that decides *who* does the work and *whether it actually happened*.

## In scope

| Area | What J.E.A.N. owns |
|---|---|
| **Task lifecycle** | A state machine: `queued → planning → awaiting_approval → running → verifying → succeeded / failed / cancelled / needs_attention` |
| **Tool registry** | Discovery, capability manifests, health checks, selection metadata |
| **Process supervision** | Launch, stream, timeout, cancel, Job-Object tree kill, exit-code capture |
| **Policy & permissions** | Risk classification, approval prompts, UAC brokering, audit trail |
| **Result verification** | Proving the outcome, not just the launch. Exit code 0 is *not* success. |
| **Event routing** | Typed local IPC between core, UI, and workers |
| **Durable state** | SQLite: tasks, steps, tool runs, events, approvals, tools, preferences |
| **Integration** | MCP for tools, ACP for coding agents, CLI adapters where no protocol exists |

## Out of scope — permanently

These are **non-goals**, not deferred features. They stay out unless a decision in [`07-DECISIONS.md`](07-DECISIONS.md) says otherwise.

| Non-goal | Why |
|---|---|
| Replacing Codex / Claude Code / OpenCode | We supervise them; we do not reimplement them |
| Hosting model weights or inference in the core | Breaks the ~50 MB idle budget |
| Always-on screen recognition | Expensive, privacy-hostile, and unnecessary — capture is on demand |
| Reimplementing a browser, IDE, terminal, or file manager | Use the real thing |
| Binding to a single AI provider | The model is a replaceable dependency behind a provider-neutral interface |
| Binding to OpenClaw | Study it; optionally expose it as an external capability. Do not depend on it. *(See `07-DECISIONS.md` D-002.)* |
| Running the whole application elevated | Normal user integrity by default; elevation is a narrowly scoped, user-approved sidecar |
| Letting the model make security decisions | Permission checks, argument validation, path handling and verification are deterministic code |
| A vector database, graph database, Docker stack, or workflow server | Not until a measured retrieval or durability problem demands one |
| An Electron UI | Tray-first. A rich UI, if ever, is a separate optional process |

## Capability boundaries — who does the work

```text
┌────────────────────────────────────────────────────────────────┐
│  J.E.A.N. CORE (Rust, always running, ~50 MB idle target)      │
│  task state · tool registry · policy · process supervision     │
│  event routing · verification · SQLite                         │
│                                                                │
│  NOTHING HEAVY LIVES HERE. No inference. No audio. No pixels.  │
└───────┬──────────────────┬──────────────────┬──────────────────┘
        │                  │                  │
   local IPC           MCP / ACP          spawn + supervise
        │                  │                  │
┌───────▼───────┐  ┌───────▼────────┐  ┌──────▼──────────────┐
│ OPTIONAL      │  │ EXTERNAL TOOLS │  │ EXTERNAL AGENTS     │
│ WORKERS       │  │                │  │                     │
│               │  │ PowerShell/CMD │  │ Codex      (ACP)    │
│ voice         │  │ Git            │  │ Claude Code (ACP)   │
│ vision/OCR    │  │ Docker         │  │ OpenCode   (ACP)    │
│ local model   │  │ VS Code        │  │ Gemini CLI (ACP)    │
│ server        │  │ Brave/Chrome   │  │ Qwen Code  (ACP)    │
│               │  │ Playwright     │  │ Goose      (ACP)    │
│ Loaded only   │  │ MCP servers    │  │                     │
│ when enabled. │  │                │  │ Kill = process kill │
└───────────────┘  └────────────────┘  └─────────────────────┘
```

**The rule:** the core coordinates. Specialist processes do the work. A hanging GUI automation task must never block the core or the voice interface.

## The workflow that proves the product

Everything else is scaffolding until this works end to end:

> **User request →** J.E.A.N. plans → **launches a coding agent →** monitors its progress → **inspects its output →** runs the tests → **verifies the result →** reports what actually happened.

Concretely, the acceptance scenario:

1. User: *"Open this project and find out why the tests are failing."*
2. J.E.A.N. resolves the workspace, discovers the test command, selects an ACP-capable coding agent.
3. It requests approval (Tier: `normal` — write inside workspace).
4. It spawns the agent, streams `session/update` events into the task board.
5. The agent finishes. J.E.A.N. does **not** report success.
6. J.E.A.N. runs the test command itself, captures exit code *and* output.
7. It compares the observed result against the requested outcome.
8. It reports: what was changed, what the tests now do, and whether the goal was met.

If step 6–8 are missing, J.E.A.N. is a launcher with a UI. Verification is the product.

**How this scenario reads after Phase 3 (2026-11-05).** Steps 3–8 are implemented and tested; two
wordings above were written before the code existed and are corrected here rather than left as a
promise:

- **Step 2, "discovers the test command" — no.** JEAN resolves `[delegation] verify_command` from
  configuration *before* delegating, and **refuses to run at all when it is empty**. Nothing is
  inferred from the project, and a command the agent suggests is never executed: the check has to be
  the operator's, or the agent is grading its own homework (D-043). Same for the workspace:
  `--workspace` or `[workspace] root`, resolved and canonicalised up front.
- **Step 2, "selects an ACP-capable coding agent" — only if one is configured.** The worker kinds are
  `acp` and `codex-cli`; on this machine Codex 0.160.1 has no `acp` subcommand, so its connector is
  the structured CLI fallback. The abstraction being protocol-or-CLI is deliberate (step 4's event
  stream is the same task-board record either way).
- **Step 3** is not one approval but a gate: the delegation's own scope is granted once, and every
  mid-session request the agent makes is decided by the same policy engine — a covered action does
  not re-prompt, an action outside the scope is refused, and the agent cannot widen what it was given.
- **Steps 5–8** are the delivered shape: the agent's account is recorded as `claim`, JEAN's own run of
  its own check is `verification`, and the two are separate fields that never merge — claim of success
  plus a failing check reports `failed` (exit 1) with the verification line reading "did not pass",
  while a check that could not be run at all reports `unverified` (exit 3) rather than a pass. A
  baseline run of the same check happens *before* delegating, so "it was already broken" is
  distinguishable from "the agent broke it".
- **What is still not true:** the end-to-end run has been proved against deterministic workers and against
  a real ACP peer over real pipes — but that peer is a script this repository wrote, not somebody else's
  agent. **No vendor coding agent has completed a delegation on this machine** (`06-RISKS.md` R-28; the two
  ACP defects that used to be an additional reason are closed — §44, R-27). And the browser half, though now
  built and demonstrated end to end (D-046, §45), has only ever been pointed at **an offline local fixture
  it wrote itself**: no real site, no authenticated page, no visible window (`headless = false` is
  implemented and has never been run). What that leaves unproven is listed in `IMPLEMENTATION_STATUS.md` §47.

## Design rules that constrain every phase

| # | Rule | Consequence |
|---|---|---|
| R1 | **Semantic first, pixels second.** Application API → UIA → synthetic input → vision/OCR. | No coordinate clicking where a named control exists. |
| R2 | **Voice is an independent process with its own cancellation scope.** | A stalled GUI task cannot freeze the voice interface. |
| R3 | **Every child process is in a Job Object with kill-on-close.** | Killing the parent is not enough; the tree must die. |
| R4 | **The model may request an action; it may never grant one.** | All capability decisions flow through one deterministic policy engine. |
| R5 | **Exit code 0 does not prove the outcome.** | Every automated action declares how its result is verified. |
| R6 | **Nothing heavy is loaded while disabled.** | Voice/vision/model workers are opt-in and independently restartable. |
| R7 | **Local-first.** | Orchestration and computer integration run locally. Cloud is an opt-in reasoning backend. |
| R8 | **Audit everything.** | Tool calls, permission decisions and captures are logged with the acting identity. |

## Success criteria for the project (not for a phase)

- Idle core RSS **< 50 MB** with no optional workers loaded — measured including children, not just the parent.
- Idle CPU **≈ 0%** with no active task.
- Startup to command-ready **< 2 s**.
- Core runs with **no inference dependency** — a deterministic fallback handles basic commands.
- Every action in the audit log has: tool id, arguments (secrets redacted), working directory, start time, state, output reference, exit/protocol status, cancellation status, permission tier, approval record.
- Task state **survives a process restart** and resumes from the last checkpoint.

## What "done" is not

Done is not "it can click things." Done is not "it has 40 tools." Done is: **a user states an outcome, J.E.A.N. coordinates the right tools to produce it, and can prove — with evidence, not confidence — whether the outcome was achieved.**
