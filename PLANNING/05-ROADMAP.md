# 05 — Roadmap

> **What we implement, in what order.** Phases, deliverables, exit criteria, dependencies.
> Every exit criterion is testable. A phase is not done because the code merged — it is done because the criterion was demonstrated.

---

## Overview

| Phase | Name | Builds | Depends on | Est. |
|---|---|---|---|---|
| **0** | Architecture spike | Process supervision + SQLite + config | — | 2 wks |
| **1** | Useful local orchestrator | Deterministic intents, tool registry, policy, audit | 0 | 3 wks |
| **2** | Model-driven planning | Provider-neutral LLM interface, plan schema, validation | 1 | 3 wks |
| **3** | Agent & browser coordination | ACP connectors, MCP client, Playwright, verification | 1 (2 optional) | 4 wks |
| **4** | Windows UI & screen awareness | UIA, capture, OCR, grounding fallback | 3 | 5 wks |
| **5** | Voice & richer memory | Voice worker, barge-in, searchable history | 1 (independent of 4) | 4 wks |
| **6** | Hardening & packaging | Installer, elevation helper, benchmarks, docs | 3–5 | 3 wks |

**Critical path:** `0 → 1 → 3 → 4`. Phase 2 and Phase 5 are **off the critical path** and can run in parallel from week 1 of their prerequisite.

```text
week:  1  2  3  4  5  6  7  8  9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24
P0    ██ ██
P1          ██ ██ ██
P2                      ██ ██ ██
P3                            ██ ██ ██ ██
P4                                        ██ ██ ██ ██ ██
P5          (starts after P1)             ██ ██ ██ ██
P6                                                              ██ ██ ██
```

---

## Phase 0 — Architecture spike (2 weeks)

**Goal:** prove Rust can launch and supervise the local tools J.E.A.N. needs, and that state survives a restart.

### Crates created

`jean-core`, `jean-protocol`, `jean-process`, `jean-store`. **Nothing else yet.**

### Deliverables

- [x] Rust workspace builds clean on `stable-x86_64-pc-windows-msvc` with MSRV 1.90
- [~] `deny.toml` + `rust-toolchain.toml` committed; **`cargo deny check` has never been green because it has never been run** — `cargo-deny` is not installed on this machine and installing it is not this project's call (IMPLEMENTATION_STATUS.md §33, §41). `deny.toml` is real and version-pinned, and it has never been enforced by the tool it was written for; the known `sysinfo → ntapi → winapi` policy conflict is still open. Do not read this box as compliance.
- [x] Executable starts, initialises `tracing` with rolling file logs, and exits cleanly on Ctrl-C and on window close
- [x] Config loader: layered TOML → typed `serde` structs; unknown key = warning, wrong type = hard error; `config_version` enforced
- [x] Tool registry loaded from `tools.toml` with capability manifests
- [x] Launch a child process with an explicit working directory and argument vector
- [x] **Every child is assigned to a Job Object with `KILL_ON_JOB_CLOSE`** (`win32job` 2.0.3)
- [x] Stream stdout/stderr with a 10 MB per-stream cap and truncation marker
- [x] Capture exit code
- [x] **Cancel a whole process tree** and prove the descendants died (not just the parent)
- [x] Environment allowlist enforced — no blanket inheritance
- [x] Secret redaction applied to logged arguments
- [x] Persist a task record in SQLite (`rusqlite` 0.41.1, `bundled`), WAL mode
- [x] Migration runner (`user_version` pragma loop)
- [x] Task state machine with all nine states, unit-tested for every legal and illegal transition
- [x] `jean-cli`: `jean run <tool> [args…]`, `jean tasks`, `jean cancel <id>`

*Phase 0 was completed and demonstrated before this document was last reconciled; the boxes were ticked retroactively from `IMPLEMENTATION_STATUS.md` §"Phase 0 demonstrations" (D1–D10) rather than from memory.*

### Exit criteria

> The core launches PowerShell, runs a harmless command, collects its output, **cancels a long-running child tree**, and **recovers its task state after a hard process kill and restart**.

Demonstrated by: kill `jean-core` with Task Manager mid-task → restart → the task is present in its last state and resumable or cleanly marked `needs_attention`.

### Do not build in Phase 0

No UIA. No capture. No MCP. No ACP. No LLM. No tray icon. No voice. **The temptation to add a tray icon here is the first sign of scope creep.**

---

## Phase 1 — Useful local orchestrator (3 weeks)

**Goal:** complete genuinely useful tasks with **zero AI**. Deterministic intents only.

### Crates created

`jean-tools`, `jean-policy`.

### Deliverables

- [x] Explicit subcommand input via `jean-cli` — **not** unrestricted natural language. `jean apps|launch|exec|ps|find|health|approvals|status|notify` take typed flags; nothing in Phase 1 parses a sentence, and the CLI says so.
- [x] **Four deterministic intents**, no model involved (engine: `crates/jean-core/src/intent.rs`; behaviour: `crates/jean-core/tests/intents.rs`):
  - launch application (by name, resolved via PATH + Start Menu + registry)
  - run command (shell tool, ConPTY when interactive)
  - inspect process (`sysinfo` 0.40.1 — CPU, memory, children)
  - locate file (`fd`/`ripgrep` if present, else a bounded recursive walk)
- [x] Tool health checks; mark unhealthy after `tools.mark_unhealthy_after` (default 3) failed probes. **A probe is a path lookup and never runs the tool** (D-026); a healthy result is trusted for the freshness interval instead of re-probed.
- [x] **Policy engine**: three tiers (`read_only` / `normal` / `elevated`), `require_approval` and `auto_deny` lists, approval timeout → **deny**. `auto_deny` and the elevation refusal are checked before the approval ledger is consulted, so a remembered grant can never out-rank a rule.
- [x] Approval prompt flow (CLI first; tray later) — `jean_policy::Prompter` is a parameter, not a stdin read, so a later tray UI answers the same question.
- [x] **Audit log** as append-only JSONL: actor, tool id, arguments (redacted), cwd, tier, decision, timestamps, outcome. Every attempt is recorded, including denials that never reached a task row.
- [x] Remembered approvals — **scope deviated to `operation + resolved target + validated arguments`**, not `tool_and_tier` (D-027). `tool_and_tier` would have let one click cover `--force` forever.
- [x] **Verification step per intent** — each declares how its result is proven, and the answer is three-valued (`Proven` / `NotProven` / `Unverifiable`):
  - launch app → window with the expected process id exists within N seconds. A single-instance handoff is reported **`Unverifiable`**, not passed (D-023).
  - run command → exit code **and** an output predicate
  - inspect process → the process id is alive and the fields are non-null (`thread_count` is a documented platform absence on Windows, not a null field)
  - locate file → the path exists and is readable — re-opened after the walk, not trusted from the listing
- [x] Compact task status view (CLI table) — `jean status` with one row per task and `jean status <id>` for the record behind it; demonstrated in `IMPLEMENTATION_STATUS.md` §11 (P1-21, P1-22).
- [x] ConPTY backend for interactive shells; plain pipes for one-shot. **Raw ConPTY through `windows`, not `portable-pty`** (D-024, superseding D-016's adoption): one supervision loop, two transports.
- [x] Toast notifications via raw WinRT (`UI_Notifications`), with the **registration path** for the AUMID under `HKCU\Software\Classes\AppUserModelId\JEAN.Orchestrator` (display name only; no Start Menu shortcut is written). The registry write is opt-in — `jean notify register` — and was deliberately **not** performed during validation, because completing a command does not entitle the tool to change the operator's `HKCU`. `ui.notifications` therefore ships as `none`.

### Exit criteria

> Every action has a clear start, output, status, and a **verified** result. `auto_deny` actions are refused without a prompt. Killing a task leaves no orphaned descendants.

Demonstrated by: a test that runs `format` → refused with no prompt; a test that spawns a shell which spawns three children → cancel → all four gone.

**How Phase 1 met each clause** (`IMPLEMENTATION_STATUS.md` §11):

| Clause | Evidence |
|---|---|
| a clear start, output, status and **verified** result | P1-7/P1-8/P1-9 print the decision, the task id, the artifact paths with byte counts, the termination and the verdict; P1-6 and P1-18 show the same shape when the outcome is *not* proven, and return exit 3 instead of pretending |
| `auto_deny` refused **without a prompt** | P1-11 (`format_volume`) and P1-10 (elevation) both exit 5 with "child never started" and no question printed; `jean-cli/tests/cli_phase1.rs` asserts the absence of a prompt and the absence of a task row |
| killing a task leaves **no orphaned descendants** | P1-23 timed out a PowerShell child and the process table before/after counts were identical; `jean-process/tests/supervision.rs` (20 tests) covers the tree cases through the same `TerminateJobObject` path. The one clause not exercised here is a *manual* Ctrl-C, because the validation harness has no console — §13 says so and gives the steps |

### Known Windows trap to solve here

`SetForegroundWindow` is rate-limited by the foreground-lock timeout. Use the `AttachThreadInput` / `AllowSetForegroundWindow` dance, or accept failure and report it. **Do not silently retry.**

**How Phase 1 resolved it: the third option the note did not offer — do not attempt it at all.** Nothing in `jean-tools::desktop` calls `SetForegroundWindow`, `AttachThreadInput` or `ShowWindow`; the window surface is enumeration and queries only. The foreground-lock dance steals the user's focus, and an orchestrator that grabs the keyboard is a hazard, not a feature (`06-RISKS.md`). Verification therefore asks "does a window the operator can see now belong to the pid we started", which needs no focus change — see D-023.

---

## Phase 2 — Model-driven planning (3 weeks) · *off the critical path*

**Goal:** use an LLM for intent understanding and tool selection — **never** for execution policy.

### Deliverables

**Status: implemented 2026-11-05** (see `IMPLEMENTATION_STATUS.md` §16-24). Each row says what shipped
where the wording below differed from what turned out to be the right shape.

- [x] Provider-neutral `ModelClient` trait: input = context + available tools + policy constraints + output schema; output = a structured plan. *A plan only, not a bare tool request — a single tool call is `jean exec`, which Phase 1 already runs through the intent layer. The trait is `generate`/`info`/`provider_name`, three methods, synchronous like the rest of the supervisor (D-029).*
- [x] `ollama` adapter — `POST /api/chat`, NDJSON streaming (Ollama v0.40.14). *`stream = false` by default: one bounded JSON body is easier to verify than a reassembled stream.*
- [x] `openai_compatible` adapter — `POST {base_url}/chat/completions` (llama.cpp `llama-server`). *The supported subset is documented rather than assumed: no tool-calling API, no function schemas, no redirects, no chunked transfer encoding (D-029, D-030).*
- [x] Structured plan schema (`serde`), validated on receipt; a malformed plan is a failure, not a retry-forever. *Schema v1 with a closed vocabulary of four operations (D-031); a bounded three proposals, each re-validated (D-036).*
- [x] **Tool argument validation** against each tool's manifest schema — deterministic, before execution. *Two stages: types and bounds in `jean-protocol`, registry/policy/workspace in `jean-core`, both before any row is written (D-032).*
- [x] Operator-authorised runs without repeated prompts — a person typing `jean do` covers the validated
  plan's gated steps for that run only (D-039), bound to each step's resolved scope, expiring with the
  run and never entering the approval ledger; `--prompt-each` keeps the per-step gate. *The refusals do
  not move: `auto_deny` and the elevation boundary are decided before a grant is consulted.*
- [x] Plan preview for any multi-step or sensitive task. *`jean plan` previews and validates while writing nothing; `jean do` prints the plan before running it. A display is not a second consent gate — the operator's goal already authorised the run, and each step still passes the Phase-1 policy gate.*
- [x] Tool result summarisation. *Built from the execution records, not from the model: `model.summary_wording` offers wording only (D-037).*
- [x] Failure recovery with bounded retries (`retry_backoff_ms = [500, 2000, 8000]`). *Transport retries only; a tool run is never retried, and an interrupted plan becomes `needs_attention` with its uncertain step named (D-035, D-036).*
- [x] `never_delegate_to_model` list enforced in code, not in a comment — the policy engine does not import the model module at all. *`jean-model` depends only on `jean-protocol`, and `jean-policy` depends on neither, so the boundary is the dependency graph; removing an entry from the list is a load error.*
- [x] Deterministic fallback intact: with `provider = "none"`, all four Phase-1 intents still work. *Verified with the real binary and in `provider_none_keeps_every_deterministic_command_exactly_as_phase_one_left_it`.*

### Exit criteria

> The model can plan a workflow but **cannot call a tool outside the registry** and **cannot override a permission rule**.

Demonstrated by: a prompt-injection test where the model's plan requests an `auto_deny` action → refused; a test where the plan names an unregistered tool → rejected at validation.

Both are in `crates/jean-core/tests/plans.rs`, together with 28 more Phase-2 scenarios driven by a
deterministic fake provider (no live model, no network), 6 grant tests in `jean-policy`, 3 real-binary
checks in `jean-cli/tests/cli_phase2.rs` and the transport scenarios in `jean-model`: the exit
criterion is met at the layer this build can test. **Still open:** a smoke test against a real provider. Nothing was installed, started
or configured with credentials for this batch, so live-provider compatibility is recorded as
unverified (`IMPLEMENTATION_STATUS.md` §20).

The transport's verified-TLS claim is now demonstrated rather than asserted: `crates/jean-model/src/tls.rs`
completes a handshake against a certificate chained to a pinned throwaway authority, and refuses the
same connection when the chain reaches no trusted root or the name does not match, with two accepted rows
as controls so a broken harness cannot pass as a rejection (`IMPLEMENTATION_STATUS.md` §32, D-040). No
trust store on the machine is touched; `cargo deny check` remains the one exit-adjacent check that has
never run (§33).

### Phase 3 preparation, done in this batch without starting Phase 3

- **Disk.** Measured and classified: `target/` is ~13 GB, of which 4.5 GB is incremental compile cache
  and 7.7 GB is `deps`. The proposal is written, **nothing was deleted**, and `cargo clean` is ruled out
  because `target/` also holds the build wrapper, the check transcripts and the demonstration database
  (`IMPLEMENTATION_STATUS.md` §31, `06-RISKS.md` R-26).
- **Dependency check.** The exact user-local install and check commands, their storage behaviour and the
  one ban (`sysinfo` → `ntapi` → `winapi`) that the first run is expected to report are recorded in
  §33. cargo-deny was not installed.
- **No Phase-3 code.** No `jean-acp`, no `jean-mcp`, no capture, UIA, voice, Telegram or tray surface was
  added, and no Phase-3 schema or command exists.

---

## Phase 3 — Coding-agent and browser coordination (4 weeks) · *the differentiator*

**Goal:** prove J.E.A.N.'s core value — coordination.

### Crates created

`jean-acp`, `jean-mcp`, `jean-browser`.

### Deliverables

Marked as actually built, not as planned — the original wording stays visible inside each line, so a change of plan is readable rather than silent. `IMPLEMENTATION_STATUS.md` §36–§47 is the evidence. The three states are `[x]` = delivered and tested here, `[~]` = delivered with something material missing or an open defect, `[ ]` = not delivered in this batch, with the reason — never *forgotten*.

- [x] **ACP client** via `agent-client-protocol` 5.0.0 (pinned `=5.0.0`), speaking protocol **V1** — `crates/jean-acp/src/acp.rs`. `initialize` → `session/new` → `session/prompt` → updates, over byte pipes this crate owns. **Not** proven against a vendor agent; the peer in the tests is a Node script JEAN writes and starts (D-047).
- [~] **Two ACP connectors:**
  - `codex` — **CLI fallback only.** The installed `codex-cli 0.160.1` has no `acp` subcommand (verified from its `--help`), so the `codex-cli` kind reads structured `codex exec --json` JSONL. The `@agentclientprotocol/codex-acp` adapter is a third-party runtime this build does not install, so an ACP-shaped Codex worker is **not** available here. `codex app-server` is a different protocol and is not implemented.
  - `opencode` — native `opencode acp` (verified present in `opencode 1.18.34 --help`) is what the `acp` kind is written for. **Live compatibility unverified**: no OpenCode session has been run.
- [x] **One direct CLI adapter** for an agent with no stable ACP surface — the `codex-cli` kind above *is* that adapter, so the abstraction is not ACP-only; both kinds sit behind the same `WorkerRunner` contract in `crates/jean-core/src/delegation.rs`.
- [x] **`session/update` event stream** routed into the task board — agent messages, tool calls and plan updates become `tool_run` rows and lifecycle events with a bounded queue; unrecognised update kinds are passed over rather than ending the session.
- [x] **`session/request_permission`** wired into our policy engine — the agent asks *us*, we ask the *user*, through the same `Policy::decide` + audit + approval-row path as a typed command. Covered actions do not re-prompt; an ask outside the run scope is refused; a `read-only` worker is never offered a write path at all (D-043).
- [x] Cancellation via `session/cancel`; **teardown via process kill** — the notification is sent first, then the Job Object closes the tree. **The two settlement defects this line carried are closed (2026-11-06, §44/R-27):** the cause was the transport's own `Drop` joining a thread parked in `ReadFile` on a live agent's stdout — inside `block_on`, which is also why none of the three timers could fire. The deadline now bounds the whole session including callbacks, a cancel stays responsive while a handler is in flight, and every request settles with a result or a typed error. `delegation_live.rs` runs in the default suite (**2 passed, 3.98 s** where it had hung ≈221 s and been killed), and `crates/jean-acp/tests/real_pipes.rs` shows the kill taking a **grandchild** with it over real processes. Cancelled/deadline runs keep the uncertainty rather than inventing an answer: `stop_reason = timeout`, **no claim**, and in the real-process test the witness file a grandchild was scheduled to write four seconds in never appears — the tree that would have written it is gone.
- [~] Git worktree isolation — **delivered for a single run** (`isolation = worktree`, baseline preserved, nothing committed/pushed/deleted by JEAN). **`max_parallel = 3` is NOT delivered**: the process supervisor has one active task per process for cancellation, so `jean agent status` reports the configured number *and* states that this build runs one worker at a time (D-042). Parallel workers require the cancellation limitation resolved first; the roadmap does not get to claim them before that.
- [ ] **Codex hooks** (`mcp_tool` handler for `PreToolUse`/`PostToolUse`/`PermissionRequest` via `commandWindows`) — **not built.** The `commandWindows` field is real and this is still the highest-value surface in the landscape (see `03-DEPENDENCIES.md` §5), but it is a Codex-side configuration this machine's agent would have to accept; no evidence yet that 0.160.1 honours it.
- [x] **MCP client** via `rmcp` 3.8.0 — **without `transport-child-process`** (D-041): JEAN spawns and supervises the server through the existing Job Object + audit path and hands the SDK the pipe halves over `transport-async-rw`. `@playwright/mcp` is **not** the server it spawns (D-046).
- [x] **MCP server** role: `jean mcp serve` exposes JEAN's own tools over **local stdio only** — no listener, no port. The exposed list must also pass the same policy engine, and a call needing a question JEAN cannot ask is refused rather than answered silently (D-045). The bridge crate `agent-client-protocol-rmcp` was **not** adopted: the two roles are implemented independently against their own SDKs, so the bridge's premise (one shared session object) does not match this design.
- [x] **Browser integration** — **decided (D-046), built, and demonstrated (2026-11-06, §45).** DOM-first via Playwright stays **superseded**: `@playwright/mcp` needs an npm install and its own browser downloads, and attaching to the user's running browser or profile stays rejected outright. What is here: `crates/jean-browser` drives the **installed Chrome** as a child spawned through `jean_process::piped` — so it is born inside the same kill-on-close Job Object as every other worker — attached over a `ws://127.0.0.1:<free port>` endpoint taken from that child's own stderr and **checked for a loopback host before it is dialled**, on a profile directory J.E.A.N. created and removes; `OwnedBrowser::start` **refuses a profile that already contains something**, which is the "never the operator's browser" rule as a checked fact rather than an intention. `jean browser run` performs navigate → inspect → interact → **independently verify** (a fresh read of an element named before the interaction, with the rendered PNG as a second witness; the click's own return value is not believed) through the existing task/policy/audit path, with `browser.drive` and a new `OperationKind::DriveBrowser` so an approval binds to the **origin**. Demonstrated on four paths — proven (0), page-declined → `not_proven` (1), no expectation → `unverifiable` (3), origin refused before any task row (1) — and `crates/jean-core/tests/browser_live.rs` runs four of them in the default suite. **Not claimed:** any real or authenticated site, `[browser] headless = false`, the Edge/Brave detection paths, or the `fetch` feature that would download a Chromium (`IMPLEMENTATION_STATUS.md` §47).
- [x] **Post-agent verification**: after an agent reports "done", JEAN runs the `[delegation] verify_command` **it resolved from configuration — never a command the agent suggested** — and reports its own observation. The agent's claim is a separate field (`claim`) from the verdict (`verification`), in the lines *and* in `--json`; an agent claiming success on a failing check reports `failed` (exit 1) with the verification line "did not pass", and a check that could not run reports `unverified` (exit 3) instead of a pass. Baseline verification before the run is recorded too, so "it was already broken" is a distinguishable answer.

### Exit criteria

> Ask J.E.A.N. to delegate a task, monitor the worker, inspect the result, run the tests, and report **what actually happened** — including the case where the agent claimed success and the tests failed.

Demonstrated by: an agent that reports success on a broken build → J.E.A.N. reports `failed` with the test output as evidence.

**Met by scripted evidence, and by one real ACP session against a fixture this repository wrote — still not by a live vendor agent.** `jean delegate` runs the whole sequence — resolve, baseline, spawn, monitor, permission-gate, list changes, verify, report — against deterministic fake workers, and the "claimed success, verification failed" case is a test, not a slide. The two settlement defects that used to be named here as the barrier are closed (§44, R-27), so `delegation_live.rs` — the Node ACP peer over real pipes — runs in the default suite in **3.98 s**. What still separates this from the sentence above is a vendor: **the sequence has not been run against Codex or OpenCode**, and a script that answers the way JEAN expects is not evidence about somebody else's agent (D-047, R-28).

### The one workflow to nail

This is the acceptance scenario from `01-SCOPE.md`. Until it works, nothing else matters.

---

## Phase 4 — Windows UI and screen awareness (5 weeks) · *the hard one*

**Goal:** understand and operate the desktop reliably.

### Crates created

`jean-windows`.

### Deliverables

Marked as actually built, in the same three states Phase 3 uses — `[x]` delivered and tested here, `[~]`
delivered with something material missing, `[ ]` not delivered with the reason. The original wording stays
visible inside every line. `IMPLEMENTATION_STATUS.md` §49–§53 is the evidence; §50 is the scored 30-task
suite.

- [x] **DPI correctness first**: `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` is established
  before any coordinate work (`identity.rs`), `GetDpiForWindow` is read per target, and bounds are converted
  with the window's own scale rather than the process default. **The test matrix differs from the sentence
  above, and deliberately so**: this machine has a 1920×1080 @ **125%** primary and a 1366×768 @ **100%**
  secondary, and that mixed-DPI pair is real, not manufactured — the brief forbids changing the operator's
  display configuration to buy coverage. So 125% and a mixed-DPI dual-monitor setup are **measured**
  (T24: a control located and operated on the 100% secondary from a 125% primary; T30: the same, by
  keyboard), and **100%-only and 150% are not covered** (§52). No third monitor exists to test.
- [x] Window enumeration, foreground detection, process attribution (`EnumWindows`, `GetDpiForWindow`) —
  plus what the plan did not ask for and the diagnostics need: PID, process-image path, creation identity,
  integrity level, DPI, bounds, visible/foreground, affinity and the desktop the input will reach. Measured
  on the operator's live desktop: **25 top-level windows** reported, `Medium (S-1-16-8192)` for this
  process, `High` for Taskmgr, `Low` for a `RtkUWP` frame.
- [x] **UIA tree read** via `uiautomation` 0.25.1 — behind an internal trait so the wrapper is swappable.
  The trait is `accessibility::AccessibilityBackend` and it answers **five** questions, not the two the
  plan implies (`read_tree`, `resolve`, `probe`, `read_state`, `act`); the fifth was added because the
  fourth defect in §51 was that an input path could see a control but not its **value**. One
  implementation, `uia::UiaBackend`, in one 789-line file.
- [x] Find a control by accessible name / AutomationId / control type, and read `IsEnabled` /
  `BoundingRectangle` — `Locator` is an enum of `AutomationId`, `Name { name, control_type }`, `Index` and
  `Path` (the ancestry chain), and every variant is *a question asked of the tree at action time, never a
  cached element*, which is what makes revalidation the default rather than a discipline someone has to
  remember. `resolve` returns a `Vec<Control>` and the trait's own doc forbids an implementation from
  silently picking one; the callers turn several matches into `Ambiguous { count }`, so ambiguity is a
  reported number rather than a coin flip. `enabled=` is in the readback digest and bounds ride on every
  listing row. Missing pattern, ambiguity, stale element and unavailable provider each have their own
  answer in `WindowsError` — `PatternUnsupported`, `Ambiguous`, `StaleElement`,
  `AccessibilityUnavailable`, plus `FocusChanged`, `IntegrityBlocked`, `SessionUnavailable` and
  `ProviderTimeout`, which is what makes `not_proven` mean something different from `unverifiable` on a
  real window.
- [x] **Invoke UIA patterns** (`Invoke`, `Value`, `Selection`) *instead of* synthetic clicks wherever a
  pattern exists — and `Toggle`, `SelectionItem`, `ExpandCollapse`, `ScrollItem`, `Transform` are probed
  and reported too, because "relevant supported patterns" is a longer list than the plan's three. Verified
  natively: `the_toggle_pattern_flips_between_two_named_states`, `the_value_pattern_writes_and_reads_back_
  what_was_written`, and the other 19 of the suite's 21 native tests, run against six fixture windows on a
  live desktop (`target/test-windows-6.log`).
- [x] **Raw-input fallback** — **enigo is not in this build** (`lock=0`, re-checked: `grep -c '^name =
  "enigo"' Cargo.lock` → 0), and what shipped instead is `SendInput` with
  `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` normalised against `SM_XVIRTUALSCREEN`, preceded by a
  foreground re-verification and a target-rectangle re-resolution, and followed by a witness read (§49.1,
  §51 defects 4 and 6). **The line's own instruction — "this is `UNVERIFIED`, test it explicitly" — was
  about enigo, and is retired rather than executed**: testing a library this project does not depend on is
  a research task, not an acceptance requirement, and adopting a dependency to compare against it is the
  kind of change this batch was told not to make. The instruction's *subject* — that coordinate input must
  be proven at non-100% scaling, on the second monitor — is now measured on the mechanism that ships:
  `a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on` clicks a control by coordinate
  on the 100 % secondary driven from a 125 % primary and reads the fixture's own counter as the witness,
  and the normalisation's edge cases (negative virtual origin, far-edge `-1`, clamping, an empty desktop)
  are unit tests in `input.rs`. See R-03 and §52 row 5.
- [x] **Integrity-level detection**: `GetTokenInformation(TokenIntegrityLevel)`, compared against the
  target, and **reported as a blocker rather than retried** — `diagnose` prints the level and the affinity
  for each window, and an integrity mismatch is a named capability failure, not a retry loop. What is *not*
  claimed: acting on a High-integrity window from this Medium process was never attempted, because the
  token to do it was not given (§52 row 4).
- [~] On-demand capture — **`xcap` and `windows-capture` are both `lock=0`** and neither was run; the
  shipped per-window still is `PrintWindow(PW_RENDERFULLCONTENT)` with a virtual-screen `BitBlt` as the
  alternate, bounded by `max_pixels`, classified (`Usable`/`UniformBlack`/`NearlyBlack`/…), written as a
  local 24bpp BMP, and governed by `[desktop] retention` (`discard_after_task` is the default and writes
  no file at all). Bounded size and bounded retention are delivered; the two libraries the plan named are
  not, for the reasons measured in `03-DEPENDENCIES.md` §3.
- [x] **Change detection** before paying for OCR/VLM — an average hash over an 8×8 lattice of
  box-filtered means, gathered in the same pass as the uniformity check. The in-source doc is explicit
  about what it is *not*: *"it carries no perceptual guarantees: it is stable across a caret blink and
  unstable across a re-layout, which is the property staleness detection needs and the property a verifier
  must not be trusted to need."** That is a weaker promise than "perceptual hash" in the line above, and it
  is stated as weaker because the alternative is a number that looks like a claim. The downstream it was
  meant to protect — OCR/VLM cost — never runs in this build, so the saving is unmeasured.
- [ ] **OCR: RapidOCR Python sidecar over stdio JSON; `ocrs` 0.13.1 evaluated as the pure-Rust
  alternative** — **not run, not installed, not downloaded** (brief). The contract ships:
  `ocr::TextExtractor` with exactly one implementation, `NoOcr`, which refuses with a stated reason;
  `OcrRequest`/`OcrResult` bounds; `SidecarProtocol`'s one-line request and one-line reply with the spawn
  and timeout belonging to `jean_core::desktop`; and a `BackendStatus::Verified` arm **nothing in this build
  can return**. `SidecarProtocol`'s tests test the **translation**, and are not reported as inference.
  Measured on the way: `C:\Python314` has `onnxruntime 1.27.0`, `numpy 2.4.6` and `PIL 12.3.0` importable
  and lacks `rapidocr` and `cv2`, so the next batch needs one pip install plus weights, not a toolchain.
- [ ] **Pixel grounding fallback: ShowUI-2B GGUF via llama.cpp, on demand only** — **not run**: it is a
  model download and a model server, both outside this batch. What ships is the request/response types
  (`GroundingRequest`, `GroundedBox`) and the rule that a grounding answer is a **hypothesis about pixels**
  which the input path must re-verify — the re-verification is `input::Target::verify`, and it is tested.
- [x] **Action verification**: after every GUI action, re-read the control's own state and confirm the
  intended change; *"the click API returned success" is not a result* is enforced structurally — `act` and
  `input` return a `Verdict`, and the only path to `Proven` runs through a readback digest or a whole-token
  `--expect` that holds. Measured end to end against the fixture (T21, T30) and disproven where it should
  be: a refused input reports `not_proven`/exit 1 and an input that cannot be accounted for reports
  `unverifiable`/exit 3 **and is never replayed** (§51 defects 4 and 5).
- [x] Multi-monitor coordinate mapping (virtual desktop → physical pixels) — on the machine's real
  mixed-scale arrangement (1920×1080 @125% primary, 1366×768 @100% secondary at a **positive** offset),
  proven both ways: by locator on the secondary (T24, T30) **and now by a fixed coordinate**, which is the
  half this row used to disclaim — `a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on`
  clicks the fixture's button at (2408,410) → normalised (48039,25041) on the 100 % monitor and reads the
  fixture's counter as the witness (`target/input-demo-coordinate.log`). A negative virtual origin
  (`SM_XVIRTUALSCREEN < 0`) is **this machine's absence, not its arrangement**: it is covered as arithmetic
  in `input.rs`'s unit tests and remains unproven on real hardware, as are a three-monitor or all-150%
  topology, none of which can be bought without changing the operator's displays.

### Exit criteria

> Complete a controlled set of UI tasks in supported applications **without relying only on fixed coordinates**, at 125% DPI, on a two-monitor setup.

**Met in form, and qualified in the only part that matters.** The 30-task suite is defined and published
(`IMPLEMENTATION_STATUS.md` §50: **30 passed / 0 failed / 0 skipped**, five runs, the environment header
re-measured by the script itself), every task addresses its target **by locator, not by fixed coordinate**,
and the DPI and two-monitor conditions are this machine's real ones — 125% primary, 100% secondary, no
display setting changed to buy coverage. The qualification is the noun: *"supported applications."* The
tasks are performed against **J.E.A.N.'s own fixture application** (`jean-windows/src/bin/
jean_desktop_fixture.rs`, a synthetic Win32 window with six instances, an intentionally stalled provider, a
capture-excluded variant and a secondary-monitor variant), and against the operator's real windows **as
reads and as refusals** — the suite's own focus-theft case (T25) is fixture-against-fixture, and the
separate scripted demonstration went further: `target/p4demo/input-demo2.log` records the refusal naming
`found hwnd=41684688 pid=21384 brave.exe` as the window that was actually in front, with the keystroke
never sent (the title in that line is the operator's page content and stays out of these documents).
**No third-party application was acted upon in this batch.** A suite the project wrote for itself, passed
against an app the project wrote for itself, is a mechanism check, and it is reported as one.

The benchmark sentence is therefore **not met in the way it was written**: nothing is tracked against
`microsoft/WindowsAgentArena` or `xlang-ai/OSWorld`, and the 30/30 must not be read next to "Agent-S's SOTA
on OSWorld-V2 is 21.3%". Those numbers measure different things — a public task set against real
applications on a fresh machine, versus a self-defined narrow set against a purpose-built fixture — and
§50 says so in its own header rather than letting the two look comparable. The realistic-target sentence
holds (≥80% on a self-defined, narrow suite; 80% on OSWorld is not realistic), but the comparison that
sentence invites has not been made here.

### The Chromium trap

Electron and Chromium apps expose **nothing** through UIA unless launched with `--force-renderer-accessibility`. Put that flag in the tool manifest (as `accessibility_flag` in `tools.toml`) and detect its absence rather than concluding the app has no controls.

**As observed (D-049), and this sentence does not survive contact with the desktop.** Two of its three
claims were tested and one is still standing.

- *"Nothing … unless launched with the flag"* — **not what this machine showed.** No flag was injected
  anywhere, `accessibility_flag` does not exist in the tool manifest or in any `#[derive]` in
  `crates/jean-protocol/src/tool.rs` (`grep -r accessibility_flag crates apps` → nothing), and the belief
  is recorded as disproved in §51.8. An empty tree is treated as an observation, not as a verdict about
  the application.
- *"detect its absence rather than concluding the app has no controls"* — **this half is delivered**, in
  the form of a distinction rather than a flag: `TreeExplanation::NoElements` versus
  `capability::Observation::ExplicitProtection`, so `jean desktop diagnose` can say "the tree came back
  empty and that is not evidence of a protection" separately from "this window declared
  `SetWindowDisplayAffinity` exclusion" (T08, T10) and separately from "the provider did not answer in
  time" (`ProviderTimeout`, whose scored case is T22 — bounded, reported as `uncertain`, exit **4**, and
  explicitly *"will not be repeated"*).
- *"[a dedicated Telegram connector is a candidate future integration]"* — the live diagnostic for
  Telegram on this machine is **pending, not solved**: what was measured is that the installed client is
  the packaged **Unigram 13.0.1.0** app (`…\38833FF26BA1D.UnigramPreview_13.0.1.0_x64__g9c9v27vpyspw\Telegram.exe`,
  pid 14640, with `Telegram.Stub.exe` pid 27628), that **neither process has a visible top-level window**
  on this desktop at measurement time, and that `jean desktop diagnose --pid 14640` therefore exits **1**.
  The black-screen case was not reproduced, so no claim is made about it; nothing was patched, no privacy
  setting was touched, no chat was opened, and the fixture-based capability classes are what shipped.

---

## Phase 5 — Voice and richer memory (4 weeks) · *off the critical path*

**Goal:** make the orchestrator feel like a companion **without making the core heavier**.

**2026-11-07 progress:** the concurrency foundation is implemented and verified with supervised,
device-free fixtures (`IMPLEMENTATION_STATUS.md` §56): independent task/voice processes, separate
cancellation scopes, explicit PTT submission and responsive status. Worker IPC uses private inherited
stdio pipes instead of the proposed named-pipe server. Native speech, physical controls/audio, richer
memory and the full spoken-task exit demonstration remain open; the combined deliverables below are
not claimed complete.

**2026-11-07 (second pass):** searchable history and explicit saved facts/preferences are delivered and
verified with `provider = none` and no voice worker (`IMPLEMENTATION_STATUS.md` §57, decision D-053).
Native speech, physical audio and the spoken-task exit demonstration are still open.

**2026-11-07 (third pass):** the native speech *implementation* is delivered — WASAPI capture and
playback in the voice worker, streaming ASR / Silero VAD / VITS TTS written against the verified 2.0.1
APIs behind an off-by-default `sherpa` feature, non-blocking inference with responsive cancellation,
endpoint enumeration and selection, a validated push-to-talk binding, and the asset manifest with
measured sizes and digests (`IMPLEMENTATION_STATUS.md` §58, decision D-054, `docs/voice-assets.md`).
What is **not** delivered is the evidence that needs assets or hardware: native compile/link, model
inference, physical microphone and speaker, and therefore the exit demonstration below. Those are the
provisioning steps in `docs/voice-assets.md`, not code to write. Silent download stayed refused, and in
that pass it was enforced by keeping the sherpa dependency line a comment — a rule the fourth pass
replaced with a real mechanism once the claim behind it was measured.

One cost of that pass is measured rather than promised: moving inference off the control loop means the
worker polls its channels, so the interrupt round trip went from §56's 252.6 µs to **22.19 ms** on the
same fixture (§58.2). Responsiveness during a 20-second recognition operation was the requirement; equal
latency as before was not, and closing that gap — one merged event channel, or waiting on the OS handles
instead of a 10 ms timeout — is listed as open work, not presented as finished.

**2026-11-07 (fourth pass):** the native speech path was **compiled rather than described**. `sherpa-onnx`
2.0.1 is now a real `optional = true` dependency behind `sherpa = ["dep:sherpa-onnx"]` (D-055, which
supersedes D-054's commented-out rule on a claim about `--locked --offline` that measurement disproved);
`sherpa.rs` compiles against the published crate and one genuine signature error of mine was found and
fixed; both of the build script's declared-provisioning refusal branches were **executed** with 0 bytes of
native archive fetched; the static/MT versus dynamic-CRT question was settled with a local probe instead
of read off the archive filename — which corrected a "link error, not a warning" claim repeated in two
documents; recognition and synthesis provisioning were separated, fixing a defect that refused the whole
engine when only TTS was missing; **Kokoro** (`kokoro-en-v0_19`, published sha256, Apache-2.0 on the
exported weights at revision `f3ff3571…`) is selected as the TTS bundle while Piper stays documented as
the rejected option; and `jean voice transcribe clip.wav` produces tier-3 recognition evidence with no
microphone, no speaker and no operator speech (`IMPLEMENTATION_STATUS.md` §59, `docs/voice-assets.md` §1,
§4a, §7, §9). Still not delivered, because the native library archive and the ASR bundle are both absent
and were not fetched: the **link**, a model load, a real transcript, any synthesis, and all physical
audio — so the exit criterion below stays open, and the Sonus question is unanswered because nothing hung
and nothing ran. *(Kept as written; the pass below closed link, model load and transcript.)*

**2026-11-07 (fifth pass):** the two assets the fourth pass named as missing were **supplied**, and the
first four of its five undelivered items closed. `sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2`
matched its published digest byte for byte — the first time anyone had checked — and
`sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2` matched its published size; both were inspected,
extracted into `%LOCALAPPDATA%\JEAN\native` and `\models` with every extracted file equal to its archive
entry, and originals left in `Downloads`. `--features sherpa` then **linked at `[exit] 0`** with the single
`LNK4098` the fourth pass's probe had predicted, so no CRT workaround was applied and none was needed, and
the engine **recognised real audio**: the bundle's own three public-domain clips, one exact, one differing
by a spelling variant, and the 8 kHz one losing its last word — attributed by re-running with fp32 weights
to the linear resample rather than to quantisation or to our flush. Recognition configuration was kept in a
scratch `jean.toml` reached through `JEAN_DATA_DIR`, so the operator's settings were never touched. Two
findings came from using the tools rather than writing them: `jean voice doctor` printed its provisioning
path as a code-unit array (fixed), and `VoiceConfig::audio_backend()` derives the audio source from the
speech backend, which means native inference is unreachable without a device — the reason cancellation
*during live native decode* is still a tier-4 observation. Still open: **synthesis** (no Kokoro bundle was
supplied; TTS remains PENDING, not delivered, not claimed), all physical audio, and acoustic barge-in.
`IMPLEMENTATION_STATUS.md` §60, `docs/voice-assets.md` §1, §2, §5, §6, §7, §8, §9.

**2026-11-07 (sixth pass):** the Kokoro archive the fifth pass could not use was **supplied, verified and
run**, and two claims from the fifth pass were disproved rather than refined. `kokoro-en-v0_19.tar.bz2`
matched its published SHA-256 exactly and extracted 360/360 files byte-consistently, its in-bundle
`LICENSE` is Apache-2.0, and the engine loaded model + voices + tokens — then refused *text* with
*"No lexicon available. Please provide --kokoro-lexicon"*, because 2.0.1 retired the espeak-ng path that
the `espeak-ng-data` directory in that same archive belongs to. So `docs/voice-assets.md` §4a's earlier
"no `dict_dir` for English" reading was wrong, `jean voice doctor` had been calling an unusable bundle
ready, and **TTS is now delivered for the acoustic engine and still pending for sentences**, separated by
a `--phonemes` seam that renders the bundle's own token alphabet to a WAV (24 kHz, 1.03 s, peak 0.332,
verified from the file by a second program). The 8 kHz truncation the fifth pass attributed to the linear
resample loses its final word **identically when scipy resamples the file**: the cause was trailing
context, the fix is a 0.5 s tail pad in `finish()`, and all three clips now transcribe. Native inference
is no longer device-bound either — `voice.offline_wav` pairs the real engine with a file and a counted
sink, and the supervised worker was measured through it: press opens in 30–76 ms, **28 pings answered
during a 7 s native decode with the slowest at 0–2 ms**, release→transcript 58–116 ms, an expired press
refused with no transcript, and an unrelated 20-second task that survived all of it. `LNK4098` was
*removed*, not accepted: `RUSTFLAGS=-C target-feature=+crt-static` links against the same libraries with
no linker message and no Visual C++ runtime import, and both executables are preserved and tested. Still
open: the Kokoro lexicon (one asset), physical audio, and acoustic barge-in.
`IMPLEMENTATION_STATUS.md` §61, `docs/voice-assets.md` §0, §1, §4, §5, §7, §8, §9.

### Crates created

`workers/voice` (`jean-voice`).

### Deliverables

- [x] **Independent Rust voice process** using `sherpa-onnx` 2.0.1 (Apache-2.0), statically linked — the process, the adapter and the static-link recipe exist, the crate is a real `optional = true` dependency behind `dep:sherpa-onnx`, it **compiles** against the published 2.0.1 API, and as of §60.4 it **links**: with `SHERPA_ONNX_LIB_DIR` at the extracted archive, `--features sherpa` builds at `[exit] 0` with the single predicted `LNK4098` and no other linker message. **That message has since been eliminated rather than tolerated** (§61.4): the same feature built with `RUSTFLAGS=-C target-feature=+crt-static` links at `[exit] 0` with **no linker output at all** and imports no `VCRUNTIME140*`/`api-ms-win-crt-*`, and both executables are preserved, named by `JEAN_VOICE_WORKER`/`JEAN_NATIVE_WORKER_EXE`, and pass the same native suite. The default build still compiles, links and runs with no native library anywhere.
- [ ] Models downloaded on first enable into `%LOCALAPPDATA%\JEAN\models` — **not implemented, and will not be.** `sherpa-onnx-sys`'s build script fetches a 122 MB archive unless provisioning is declared; a build that downloads binaries on its own is refused here. The guarantee is now executed rather than asserted: both declared-provisioning branches were run and refused by name, and `target/sherpa-onnx-prebuilt/` stayed 0 bytes (§59.4). Provisioning remains the operator's explicit act (`docs/voice-assets.md` §6), and on this machine it has been taken: both archives were supplied, verified — the native one against its published digest — and extracted by hand (§60.1, §60.2)
- [x] **Push-to-talk first.** Hotkey second (`jean voice ptt`, validated binding, one trigger not a mode). Wake word third — still open, below. Continuous never by default (§58)
- [~] Streaming ASR (zipformer) **delivered and executed on all three shipped clips**: one word-for-word, one with a single spelling variant, and the 8 kHz one now complete too — its lost final word was **not** the linear resample (a scipy-resampled copy truncated the same way) but a missing tail of trailing audio, fixed by a 0.5 s pad in `finish()` (§61.2; this corrects §60.5/§60.6's attribution, which measured fp32-vs-int8 and reached the right exclusion and the wrong conclusion). Silero VAD is implemented and type-checked but **still unexecuted**: no VAD weights were supplied, and it is skipped rather than mis-fed when `sample_rate != 16000`. TTS: the **Kokoro bundle is supplied, digest-verified, extracted, licensed (Apache-2.0 in-bundle) and running**, and it synthesizes — through `--phonemes`, which feeds the model the bundle's own phoneme alphabet and produced 1.03 s of measured, non-silent 24 kHz audio. **Arbitrary text is still PENDING on one asset**, `voice.models.tts_lexicon`, which 2.0.1 requires and this bundle does not ship (§61.3); VITS/Piper remains supported and still rejected on licence (§59.6). ASR-only provisioning no longer blocks on a missing voice bundle (§59.7), which is now enforced in code: a synthesis failure refuses speech, never listening.
- [x] IPC to the core: **private inherited stdio pipes**, not a named pipe — compact events only, and **audio still never crosses into the core process** (corrected in §56; unchanged by this pass)
- [x] **Separate cancellation scopes** (R2): four scopes — utterance (`Interrupt`), capture activation, voice worker (`Shutdown`), application. Proven again after the rewrite: `Ping`/`Interrupt` are answered while a long decode or synthesis is in flight, and a cancelled task leaves voice ready (§58). **Now proven with a real ONNX decode in the loop, not only a fixture** (§61.5): through `voice.offline_wav`, an expired press cancels mid-decode, is refused as a stale activation, delivers no transcript, leaves the worker answerable and recognising on the next press, and does not touch an unrelated 20-second task, which ran to `succeeded` with its verification needle intact. Cancellation during native *synthesis* is still a tier-1 fixture result, because the `Speak` path takes text and text needs the missing lexicon.
- [ ] **Barge-in**: user speech stops TTS playback — **not delivered, and the wording matters.** `Interrupt` stops playback through a control message. Nothing in this build listens over the speaker or measures whether you spoke during an utterance. Claiming acoustic barge-in here would be a false statement about a capability that does not exist (§58 "What this build does not do")
- [x] TTS progress announcements for long-running tasks — bounded queue of 4, deduplicated per task+state, stale entries dropped for a superseded task, and never spoken over an explicit `Speak` or an open capture. Opt-in (`announce_task_state`) because it takes the single utterance slot (§58)
- [~] Models **unloaded / process not started** while `[voice] enabled = false` — the process is never started and no model or device is opened, proven by the absence of a `voice_pid` and by OS-handle counts not growing. **Zero RSS contribution was never measured**; no assertion exists for it (§58)
- [x] Searchable task history: SQLite **FTS5** over the `events` and `tasks` tables. No vector DB. (§57: redacted at schema level, bounded queries, literal by default)
- [x] Long-term preferences and user-approved facts (§57). Facts use a dedicated `saved_facts` table rather than key/value rows in `preferences`; four preference keys with real readers are supported and the rest are refused rather than stored inert
- [ ] Optional wake word — **requires a self-trained KWS model** (see risk R-08)

### Exit criteria

> A user speaks a task, interrupts it, asks for an update, and resumes or cancels it **while a separate worker continues safely**.

Demonstrated by: start a 60-second GUI automation task by voice → interrupt the assistant mid-sentence → ask "what's it doing?" → get an answer → cancel → the worker's process tree is gone and the voice interface never stalled.

**Not demonstrated, and not claimable yet.** Every step of that sentence except the last clause needs
either a microphone or a provisioned speech model, and this batch was forbidden from both. What *is*
demonstrated is the same sequence with the transcript arriving through the same explicitly-submitted
PTT path — task started, utterance interrupted mid-playback, status asked, task cancelled, task tree
reaped, voice never stalled — with recognition and playback driven by fixtures instead of inference.
The remaining gap is `docs/voice-assets.md` §7 Tier 3 and Tier 4, not code.

### The bug we are designing around

Pipecat has an **open** bug where interrupting mid-tool-call leaves the call "stuck in progress", `has_function_calls_in_progress` stays true, and *"Delegated backend work never finishes"*. LiveKit shipped four separate fixes for the same class in October 2026. **This is why voice is a separate process with its own cancellation scope, and why barge-in lives in the voice plane, never in the agent's tool loop.**

---

## Milestone 5.5 — Owner workflow and personalization

Use the [current execution order](09-LUNA-EXECUTION-PLAN.md). The typed and Wispr-dictated owner workflow may proceed while the native ASR quality track remains open. Existing Phase 6 source is groundwork pending verification.

After Phase 5 voice validation, demonstrate a short-goal workflow with agent handoffs, owner-specific quality defaults, and automatic recovery across eligible models. The [requirements and acceptance tests](08-OWNER-WORKFLOW.md) define the work. This milestone is planned and belongs before Phase 6 release readiness.

## Phase 6 — Hardening and packaging (3 weeks)

### Deliverables

- [ ] `jean-tray`: `tray-icon` 0.26.1 + `muda` 0.21.2 — tray icon, menu, task board, approval queue. **No Tauri, no WebView2.**
- [ ] Elevation helper `jean-elevated.exe`: narrowly scoped, launched via `ShellExecuteW` verb `runas`, handles `ERROR_CANCELLED` (1223) as "user declined", communicates back over a named pipe with an SDDL restricted to the current user plus an integrity check
- [ ] Installer (MSIX or Inno Setup) that registers the AUMID and the pipe SDDL
- [ ] Benchmark harness recording: idle/peak working set, private bytes, CPU idle and under task, startup time, child-process count, model load time and peak RAM/VRAM, IPC latency
- [ ] **CI gates from `02-TOOLCHAIN.md` §2 all green**, including the size and idle-RSS gates
- [ ] Restart-recovery test in CI: kill mid-task, restart, assert state recovered
- [ ] `docs/architecture.md` and `docs/security.md` written from the shipped reality, not from intent
- [ ] Threat-model pass against `06-RISKS.md`

### Exit criteria

> A fresh Windows 11 machine goes from installer to a working Phase-3 acceptance workflow with no manual steps, and the performance budget is met and recorded.

---

## Dependency map

```text
Phase 0 ──┬──► Phase 1 ──┬──► Phase 2 (optional for P3)
          │              ├──► Phase 3 ──► Phase 4 ──► Phase 6
          │              └──► Phase 5 ────────────────┘
          └──► (nothing else; P0 unblocks everything)
```

| Can run in parallel | Cannot |
|---|---|
| Phase 2 ∥ Phase 3 (P3 does not require a model) | Phase 4 without Phase 3 (verification patterns come from P3) |
| Phase 5 ∥ Phase 3/4 (voice is an independent process) | Phase 6 without 3–5 |
| Phase 1 tool work ∥ Phase 0 store work | Phase 1 without Phase 0 (Job Objects + state recovery are prerequisites) |

---

## What we are explicitly NOT doing in any phase

| Temptation | Why we refuse |
|---|---|
| Integrating ten coding agents | Two ACP connectors + one CLI adapter proves the abstraction. The rest is config. |
| Always-on voice | Push-to-talk first. Continuous listening is a privacy and battery liability. |
| Continuous screenshot streaming | A permanent VLM is a non-goal. Capture is on demand. |
| A vector database | FTS5 over SQLite until a *measured* retrieval problem appears. |
| A large Electron/Tauri UI | Tray-first. A rich UI, if ever, is a separate optional process. |
| Running elevated by default | Normal integrity; elevation is a scoped, approved sidecar. |
| Letting the model build shell strings | Deterministic validation, always. |
| Binding to OpenClaw | Study it; optionally expose it as an external capability. See `07-DECISIONS.md` D-002. |
| Reporting success on exit code 0 | Verification is the product. |

---

## Milestones worth announcing

| Milestone | Meaning |
|---|---|
| **M0** — "it survives being killed" | Phase 0 exit. State recovery works. |
| **M1** — "it's useful without AI" | Phase 1 exit. Four intents, verified, audited. |
| **M2** — "it can prove the agent lied" | Phase 3 exit. **This is the product.** |
| **M3** — "it can operate the desktop" | Phase 4 exit. UIA-first, DPI-correct, multi-monitor. |
| **M4** — "you can talk to it while it works" | Phase 5 exit. Independent voice, barge-in, separate cancellation. |
| **M5** — "someone else can install it" | Phase 6 exit. Installer, budgets met, CI green. |
