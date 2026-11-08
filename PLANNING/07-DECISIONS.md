# 07 — Decisions

> The decision log. What we chose, what we rejected, and why — so the same argument does not happen twice.
> A decision is recorded **when it is made**, not reconstructed later. Reversing one requires a new entry that supersedes it.

**Status:** `ACCEPTED` · `PROPOSED` · `SUPERSEDED` · `REJECTED`

---

## Index

| ID | Decision | Status | Date |
|---|---|---|---|
| D-001 | Rust for the core, not Python or TypeScript | ACCEPTED | 2026-11-03 |
| **D-002** | **Do not bind to OpenClaw — build our own core** | **ACCEPTED** | **2026-11-04** |
| D-003 | Orchestration-first, not agent-first | ACCEPTED | 2026-11-03 |
| D-004 | Semantic-first control hierarchy (API → UIA → input → pixels) | ACCEPTED | 2026-11-03 |
| D-005 | No general-purpose agent framework in the Rust core | ACCEPTED | 2026-11-03 |
| D-006 | Voice is a separate process with its own cancellation scope | ACCEPTED | 2026-11-04 |
| D-007 | ACP for coding agents; MCP for tools; neither for everything | ACCEPTED | 2026-11-04 |
| D-008 | SQLite only — no vector DB, no graph DB, no server | ACCEPTED | 2026-11-03 |
| D-009 | Tray-first UI; no Electron, no Tauri in the core | ACCEPTED | 2026-11-04 |
| D-010 | Never run elevated by default; elevation is a scoped sidecar | ACCEPTED | 2026-11-03 |
| D-011 | The model may request an action but never grant one | ACCEPTED | 2026-11-03 |
| D-012 | Exit code 0 is not success — verification is mandatory | ACCEPTED | 2026-11-03 |
| D-013 | `sherpa-onnx` for voice, in Rust, via its official crate | ACCEPTED | 2026-11-04 |
| D-014 | Ollama as the default local model server | ACCEPTED | 2026-11-04 |
| D-015 | `rusqlite` with `bundled`, not `sqlx` or `diesel` | ACCEPTED | 2026-11-04 |
| D-016 | `portable-pty` for ConPTY, not the `conpty` crate | ACCEPTED, partly superseded by D-024 | 2026-11-04 |
| D-017 | `uiautomation` crate behind an internal trait, raw COM as fallback | ACCEPTED | 2026-11-04 |
| D-018 | Browser automation via `@playwright/mcp`, CDP-over-Edge as fallback | PROPOSED | 2026-11-04 |
| D-019 | Jedi-3B blocked on license; ShowUI-2B is the grounding model | ACCEPTED | 2026-11-04 |
| D-020 | Aider is not a supported orchestration target | ACCEPTED | 2026-11-04 |
| D-021 | Claude Code is drivable but never bundled | ACCEPTED | 2026-11-04 |
| D-022 | Permissive licenses only, enforced by `cargo-deny` in CI | ACCEPTED | 2026-11-04 |
| **D-023** | **Phase 1 launches detached and verifies with a window for the pid** | **ACCEPTED** | **2026-11-05** |
| **D-024** | **Raw ConPTY through `windows`, not `portable-pty` — supersedes part of D-016** | **ACCEPTED** | **2026-11-05** |
| D-025 | Two more `windows` features: `Win32_UI_WindowsAndMessaging`, `Data_Xml_Dom` | ACCEPTED | 2026-11-05 |
| D-026 | A tool health probe is a path lookup. It never runs the tool. | ACCEPTED | 2026-11-05 |
| D-027 | A remembered approval is bound to operation + resolved scope, not tool + tier | ACCEPTED | 2026-11-05 |
| D-028 | Re-armable signal handler, idempotent logging, and exit code 5 for a refusal | ACCEPTED | 2026-11-05 |
| **D-029** | **Model transport is a hand-rolled HTTP/1.1 client over `std::net`. Plain HTTP only, no TLS** | **SUPERSEDED by D-038** | **2026-11-05** |
| **D-030** | **One flat `[model]` table; `anthropic` and `openai` are refused at load** | **ACCEPTED, amended by D-038** (endpoint scheme rule) | **2026-11-05** |
| **D-031** | **The plan schema is a closed vocabulary of four operations, with no field for a path, a command, a cwd or an environment** | **ACCEPTED** | **2026-11-05** |
| **D-032** | **Plan validation is split: structure in `jean-protocol`, the world in `jean-core`** | **ACCEPTED** | **2026-11-05** |
| **D-033** | **Steps run in declaration order; a forward dependency or a cycle is refused rather than re-sorted, and a failure stops only its dependents** | **ACCEPTED** | **2026-11-05** |
| **D-034** | **A step executes as exactly one existing `Intent`; the stricter of tool-declared and step-declared verification wins** | **ACCEPTED** | **2026-11-05** |
| **D-035** | **The plan's parent is a task row that owns the plan; an interrupted plan becomes `needs_attention` and a step recorded `running` is never reopened** | **ACCEPTED** | **2026-11-05** |
| **D-036** | **Transport retries are bounded and live in the provider client; a tool run is never retried** | **ACCEPTED** | **2026-11-05** |
| **D-037** | **Summaries come from the execution record; model wording is opt-in; a credential is an env-var name, scrubbed everywhere it could echo** | **ACCEPTED** | **2026-11-05** |
| **D-038** | **The model transport is `ureq` with the Windows trust store: verified HTTPS, chunked bodies, bounded streaming, redirects not followed** | **ACCEPTED** | **2026-11-05** |
| **D-039** | **A person typing `jean do` authorises the run, bound to the resolved plan scope; it expires with the run and never enters the approval ledger** | **ACCEPTED** | **2026-11-05** |
| **D-040** | **Certificate verification is proven by an isolated anchor set in tests, never by a flag: `native-tls` adopted as a dev-dependency, no trust store touched** | **ACCEPTED (extends D-038)** | **2026-11-05** |
| **D-041** | **ACP and MCP are spoken by their maintained SDKs on a current-thread tokio runtime owned by two crates; J.E.A.N. still does every spawn, bound and decision** | ACCEPTED | 2026-11-06 |
| **D-042** | **One delegated worker at a time, stated in the tool's own output, until cancellation can route more than one active task** | ACCEPTED | 2026-11-06 |
| **D-043** | **A delegation is one authority: in-workspace file actions are covered without re-asking, an escape is refused before the engine, and an agent is only ever answered with an option it offered** | ACCEPTED | 2026-11-06 |
| **D-044** | **A child's working directory is the plain drive path, never `canonicalize`'s verbatim `\\?\` form** | ACCEPTED | 2026-11-06 |
| **D-045** | **MCP is stdio in both directions; the exposed list is also the callable list; and a server with no terminal refuses rather than assumes consent** | ACCEPTED | 2026-11-06 |
| **D-046** | **The browser is the installed Chrome, started and owned by J.E.A.N. over loopback CDP; Playwright and attach-to-the-user's-browser are rejected** | ACCEPTED — **built and demonstrated 2026-11-06 (IMPLEMENTATION_STATUS §45)**; the dependency closure is still un-audited (§33) | 2026-11-06 |
| **D-047** | **A live protocol peer in a test is a script J.E.A.N. writes and starts, not an installed agent it authenticates to** | ACCEPTED | 2026-11-06 |
| **D-048** | **One maintained UIA wrapper, four replaceable boundaries, and the raw-COM fallback left unwritten** | ACCEPTED — **built and measured 2026-11-06 (IMPLEMENTATION_STATUS §49, §50, §53)**; the wrapper swap itself remains unexercised (R-04) | 2026-11-06 |
| **D-049** | **Capture, input and OCR go dependency-free: `PrintWindow`/`BitBlt`, raw `SendInput`, and a contract instead of a model** | ACCEPTED — **built; the libraries the plan named are measured as `lock=0`; the input path that shipped is now measured by coordinate on real mixed-DPI hardware, and enigo's own DPI test is retired as an acceptance requirement (§52 rows 1 and 5, §55)** | 2026-11-06 |
| **D-050** | **A hung accessibility provider is abandoned, never cancelled — and the leak is capped and counted instead of hidden** | ACCEPTED — **its uncertainty rule is intact and its isolation mechanism is superseded by D-051** (2026-11-06 correction): a timed-out call now ends a process rather than abandoning a thread. Recorded here because the reasoning, not just the outcome, is the value | 2026-11-06 |
| **D-051** | **A hung desktop call costs a process, and the process is terminated: provider work moves to a supervised child** | ACCEPTED — **built and demonstrated** (`IMPLEMENTATION_STATUS.md` §55.1): typed bounded IPC, three deadlines, job-object process-tree kill, fresh worker per call, no in-process fallback, five hung rounds with `live_workers() == 0` and a healthy-window action performed afterwards. Packaging must ship `jean-desktop-worker.exe` beside `jean.exe` | 2026-11-06 |
| **D-054** | **Audio is WASAPI inside the voice worker, inference lives off the control loop, and sherpa is a feature the default build refuses to resolve** | ACCEPTED — **built, with the evidence tiers separated (§58)**: shared-mode endpoints, bounded capture, PTT state machine, device-loss mapping, per-thread engines, cancellation between decode steps. **Its "keep the dependency commented out" rule and its "MT into MD is a link error" claim are superseded by D-055** (both measured wrong); the rest survives, including no native link, inference or physical-audio evidence | 2026-11-07 |
| **D-055** | **The speech dependency is wired as optional, provisioning is declared before the feature is enabled, and the archive's own refusal paths are the guard** | ACCEPTED — **built and measured (§59)**: `dep:sherpa-onnx` behind `sherpa`, default tree carries no sherpa, `sherpa.rs` compiles against the published 2.0.1 crate (one real signature error found and fixed), both no-download branches executed with 0 bytes fetched, CRT compatibility probed (`LNK4098` into today's dynamic-CRT build; clean under `+crt-static`), ASR and synthesis provisioning split, Kokoro selected on an Apache-2.0 weights licence, and `jean voice transcribe` added for microphone-free tier-3 evidence. **Its last clause — link, inference and audio blocked on absent assets — is closed for link and inference by §60; physical audio and TTS remain open** | 2026-11-07 |
| **D-056** | **The native archive is linked into the default dynamic-CRT build, and the `LNK4098` it produces is carried rather than silenced** | ACCEPTED — **built and measured (§60)**: digest verified against the real bytes, 12 `.lib` files extracted, `--features sherpa` at `[exit] 0` with `LNK4098` as the only linker message, three published clips recognised through that binary, int8-vs-fp32 comparison attributing the 8 kHz tail loss to the linear resample. No `crt-static`, no `/NODEFAULTLIB`, no test added to flatter the run — **but §61.2 reversed its attribution** (an independently resampled 16 kHz copy truncates identically, so the cause was trailing context, fixed by a 0.5 s pad) **and §61.4 built the `crt-static` variant it refused, as an opt-in second executable, with the default unchanged as this decision argues** | 2026-11-07 |
| **D-053** | **History search is a derived, redacted, bounded read; long-term memory is written only by an explicit operator command** | ACCEPTED — **built and tested (§57)**: FTS5 verified in the shipped binary, schema-level redaction before truncation, triggers for insert/update/delete, `query_only` plus a 2 s interrupt watchdog, metadata-only audit in the same transaction, four preference keys with real readers. 995 workspace tests pass | 2026-11-07 |
| **D-058** | **Native speech is validated against a file instead of a device, the recognizer alone gates configuration, and the transducer is given a measured tail** | ACCEPTED — **built and measured (§61)**: `voice.offline_wav` pairs the real engine with a WAV and a counted sink, which is how native cancellation, ping responsiveness and an unrelated task surviving an interrupt were observed at all; a Kokoro load failure now refuses speech and never listening; `finish()` pads 0.5 s because 300 ms lost a word and 400 ms did not | 2026-11-07 |

---

## D-001 — Rust for the core

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** The always-running orchestrator is written in Rust. Optional workers may be other languages where Rust has no viable path.

**Context.** The core is always resident. It owns process supervision, policy enforcement, task state and native Windows integration. Python and TypeScript were both viable for the integrations but not for an always-on, low-footprint Windows process.

**Rationale.**
- Native execution with a realistic path to the <50 MB idle budget. A Python or Node core cannot credibly hit that.
- Memory safety without a GC pause in the supervision loop.
- First-class Windows API access through Microsoft's own Rust projections (`windows` 0.62.2).
- The two protocols that matter both have **official Rust SDKs**: `rmcp` 3.8.0 and `agent-client-protocol` 5.0.0.
- Job Objects, ConPTY, UIA COM and `SendInput` are all reachable from Rust without a runtime.

**Consequences.**
- (+) No runtime to ship, no interpreter to secure, no dependency on a system Python/Node.
- (+) `windows-rs` gives us the exact narrow feature surface we want.
- (−) Slower iteration than Python for the exploratory phases.
- (−) The Rust Windows-integration ecosystem is thinner than Python's. Several capabilities have **no crate** and need raw FFI: toast notifications, window management, elevation, integrity-level detection. See `06-RISKS.md` R-17, R-14.
- (−) `uiautomation` has a bus factor of 1 (R-04).

**Alternatives rejected.**
- *Python core* — cannot meet the idle budget; `pyautogui` (the obvious input library) is unmaintained for Win11 with open DPI bugs; `pynput` is LGPL-3.0.
- *TypeScript/Electron core* — bundles Chromium for a tray app. Rejected outright.
- *C#/.NET* — genuinely strong for Windows (FlaUI, WinUI) and remains the fallback for UIA if R-04 fires. Rejected for the core because it brings a runtime and because the protocol SDKs are Rust-native.

---

## D-002 — Do not bind to OpenClaw

**Status:** ACCEPTED · **Date:** 2026-11-04 · **Supersedes:** the recommendation in `../RESEARCH.md` Part 7.2

**Decision.** J.E.A.N. does **not** use OpenClaw as its substrate. We build our own Rust core. OpenClaw's orchestration and integration design is studied, and it may be exposed later as an *optional external capability* — never as a dependency.

**Context.** The ecosystem research (`../RESEARCH.md`) concluded that [`openclaw/openclaw`](https://github.com/openclaw/openclaw) — MIT, 393,843★, pushed 2026-11-03 — already ships ~80% of the original vision: a local-first gateway, native desktop apps including a Windows Hub (`openclaw-windows-node`, C#/WinUI, MIT) with local STT, TTS, screen capture, browser control and shell execution; MCP as both server and client; an ACP client (`extensions/acpx`) that drives Copilot CLI, OpenCode, Qwen Code, Kilo Code, Pi, Grok Build and Qoder CLI; a skills registry; a plugin SDK; and local memory. The research recommendation was to fork it and replace only its weak Windows computer-control plugin.

**That recommendation is reversed.**

**Rationale.**
1. **It is the wrong shape.** OpenClaw is a *chat assistant that meets you in your channels* — Discord, iMessage, Slack, Teams, Telegram, WhatsApp and 20+ more. J.E.A.N. is a *desktop orchestrator* whose product is the task state machine, the tool registry, the policy engine and verification. Adopting OpenClaw means inheriting a channel-routing product and working against its grain to build an orchestrator.
2. **The core is TypeScript and the Windows gateway runs in WSL.** Its own setup wizard offers "Install a local gateway (WSL)". A WSL-hosted gateway cannot enumerate Windows UI Automation trees without an interop bridge, and cannot inject native input cleanly. UIA-first control — our highest-leverage decision (D-004) — fights the substrate.
3. **The 50 MB idle budget is unreachable.** OpenClaw's gateway plus a Node runtime plus a WinUI companion is not a lightweight background process. "Lightweight by design" is a stated principle, not a preference.
4. **Its computer control is the part we would have to rewrite anyway.** Verified from its own docs: Windows computer use goes through the **experimental** `cua-computer` plugin, is **pixel-coordinate based**, supports **no held-input contracts** (no modifier-held clicks), and targets the **primary display only**. That is precisely the gap D-004 and R-14/R-15 exist to close. We would be forking 393k★ of TypeScript to replace the one subsystem that matters, while keeping everything we do not want.
5. **Dependency risk on someone else's product direction.** A Foundation-stewarded project with daily releases is a wonderful thing to *learn from* and a dangerous thing to *be*. Its roadmap is not our roadmap, and its breaking changes would become ours.
6. **We do not need it.** Both protocols it uses are available to us directly, in Rust, officially: `rmcp` 3.8.0 and `agent-client-protocol` 5.0.0. There is no capability OpenClaw provides that we cannot reach ourselves — and several it provides that we actively do not want.

**Consequences.**
- (+) Full control over the architecture, the footprint and the roadmap.
- (+) Native Windows throughout: UIA, Job Objects, ConPTY, integrity detection, elevation — all in-process, no WSL boundary.
- (+) The <50 MB idle budget is actually achievable.
- (−) **We build the gateway ourselves**: sessions, permissions, plugin host, MCP wiring, ACP wiring, audit. The research estimated this at months of work — that cost is now ours, and it is the reason Phase 0–3 exist.
- (−) We forgo a 393k★ community, a plugin marketplace (ClawHub) and 20+ pre-built chat channels.
- (−) We must re-derive integration patterns OpenClaw already solved. Mitigation: read `extensions/acpx` and `extensions/cua-computer` as **reference implementations**, and read `microsoft/UFO` for the GUI-vs-native-API arbitration pattern.

**What we still take from OpenClaw.** Design, not code:
- The three-tier permission gate (`CursorTouch/Windows-MCP`'s relaxed/balanced/strict is the same idea) → our `policy.toml` tiers.
- The capability-permission model where a node advertises only what the user enabled → our `tools.toml` `enabled` + tier fields.
- The ACP-client pattern for driving external coding harnesses while owning sessions, delivery and permissions → `jean-acp`.
- The MCP-server-and-client duality → `jean-mcp`.

**Reversal condition.** If a future OpenClaw release ships a native (non-WSL) Windows gateway with a UIA-based, multi-monitor computer-control plugin under MIT, and its footprint drops below our budget, this decision should be revisited. Record it as a new entry that supersedes this one.

---

## D-003 — Orchestration-first, not agent-first

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** The product is the orchestration engine. Voice, vision, GUI automation, coding agents, browsers and terminals are *capabilities it calls*, not workloads it hosts.

**Rationale.** Every "AI desktop agent" in the surveyed ecosystem competes on model quality and pixel grounding — a race with a 21.3% SOTA on OSWorld-V2 (R-21). Nobody competes on *knowing whether the outcome was achieved*. That is a durable, defensible, and genuinely useful differentiator, and it does not require winning the model race.

**Consequences.** The core owns task lifecycle, tool registry, policy, process supervision, event routing and verification. It does not own model weights, inference runtimes, always-on screen recognition, browser engines or coding-agent implementations.

---

## D-004 — Semantic-first control hierarchy

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** Control follows a strict order: **application API → UIA → synthetic input → OCR → VLM grounding.** Never start at the bottom.

**Rationale.** A button's UIA `Name`, `IsEnabled` and `AutomationId` are stable across themes, DPI, resolution and window position. A pixel coordinate is stable across none of them. The three control approaches were compared in `../RESEARCH.md` §3.3: screenshot+coordinate is the least reliable and the slowest per step; UIA is fast and semantically verifiable; DOM is the most reliable but web-only.

**Consequences.**
- (+) Actions become **verifiable** — we can re-read the tree and confirm the state changed, instead of hoping the click landed.
- (+) Immune to DPI and resolution changes for any app that exposes a tree.
- (−) Chromium/Electron expose nothing without `--force-renderer-accessibility` (R-16).
- (−) Canvas, WebGL, games, CAD and RDP frames expose nothing at all — the pixel fallback must exist.
- (−) UIA reads are blocked across integrity levels (R-14).

---

## D-005 — No agent framework in the Rust core

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** An explicit hand-written state machine, not LangGraph / AutoGen / CrewAI / Pydantic AI.

**Rationale.** Those frameworks are Python (or TS), and their value is durable-execution plumbing we get for free from SQLite plus a state machine. An explicit state machine is easier to profile, test and reason about, and it does not put a 44k★ dependency between us and a 50 MB budget. The research confirmed `microsoft/autogen` is **archived** and `openai/swarm` is **replaced** — framework churn in that space is high.

**Consequences.** We write `queued → planning → awaiting_approval → running → verifying → succeeded/failed/cancelled/needs_attention` ourselves, plus checkpoint/resume on SQLite. That is a few hundred lines, unit-testable, with no framework upgrade treadmill.

---

## D-006 — Voice is a separate process

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** Voice runs as `jean-voice`, an independent Rust process with its own cancellation scope. Audio never crosses into the core — only compact events do.

**Rationale.** Two verified failure modes in the mature voice frameworks:
- **Pipecat** has an *open* bug (2026-09-15): interrupting the bot as it calls a tool leaves the call stuck "in progress"; `has_function_calls_in_progress` stays true; *"Delegated backend work never finishes"* and the caller waits until timeout.
- **LiveKit Agents** shipped four separate fixes for the same class in October 2026 ("answer realtime tool calls interrupted past the timeout", "answer tool calls left open when an interrupted turn times out", "preserve adaptive interruption across tool calls").

A desktop operator runs 30-second to 30-minute GUI actions. If voice shares the agent's execution context, that bug class means the click survives and the voice reply never completes. Separation makes the failure structurally impossible rather than merely unlikely.

**Consequences.**
- (+) A hung GUI automation task cannot freeze the voice interface. The user can always interrupt, ask a question, or hear status.
- (+) Barge-in is handled in the voice plane where it belongs.
- (+) Speech and task execution cancel independently.
- (−) An extra process and an IPC contract. Acceptable — named pipes are already in the design.

---

## D-007 — ACP for agents, MCP for tools

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** Coding agents are supervised over **ACP** (`agent-client-protocol` 5.0.0, protocol **V1**). External tools connect over **MCP** (`rmcp` 3.8.0). Neither is used for everything.

**Rationale.** Verified across the ecosystem: **no first-party coding CLI exposes itself as an MCP server.** Codex = client only. Claude Code = client only, with no `claude mcp serve`. Gemini, Qwen, OpenCode, Crush = clients. Aider = nothing at all. The MCP-as-server path is third-party wrappers only, and the best-known one (`steipete/claude-code-mcp`, MIT, 1,315★) is **archived** (2026-05-15). Meanwhile ACP is Apache-2.0 at stable protocol V1, with native support in OpenCode, Gemini CLI, Qwen Code and Goose, and adapters for Claude Code and Codex.

**Consequences.**
- (+) One protocol for agents, one for tools, and a bridge crate (`agent-client-protocol-rmcp`) if we need to cross them.
- (−) **ACP has no `session/stop`.** Verified: interruption is `session/cancel`; teardown must be a **process kill**. Every connector needs a kill path. `session/close` is in the v1 schema but agent-side support varies.
- (−) Qwen's ACP flag is **`--experimental-acp`**, not `--acp`, and its `serve` daemon is documented as experimental. Do not treat it as stable.

---

## D-008 — SQLite only

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** One local SQLite file. No vector DB, no graph DB, no Postgres, no Redis, no Docker.

**Rationale.** A desktop companion that needs Docker is not a desktop companion. The research killed the alternatives on exactly this ground: Trigger.dev self-hosting needs Postgres ×2 + Redis + MinIO + ClickHouse; Hatchet and DBOS need Postgres; Graphiti needs Neo4j/FalkorDB/Neptune; Honcho is AGPL plus Postgres plus Redis. Semantic search over task history is an **FTS5** problem until a measured retrieval problem says otherwise.

**Consequences.** Large blobs (screen captures, audio) stay **out** of the database — stored as files with retention limits. `retain_events_days = 90` prunes the event log.

---

## D-009 — Tray-first, no Electron, no Tauri in the core

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** `tray-icon` 0.26.1 + `muda` 0.21.2. No window framework in v1. A rich UI, if ever, is a **separate optional process**.

**Rationale.** Both crates are maintained by the Tauri org and are what Tauri v2 itself uses for tray and menus on Windows, so they are battle-tested at scale. Cost: ~2 MB binary, ~5–10 MB RSS. **Tauri's WebView2 process tree is 80–200 MB RSS** — it blows the budget outright and pulls the heaviest dependency tree in the entire candidate list. `tauri` is banned in `deny.toml`.

**Consequences.** The UI is deliberately modest: tray icon, menu, task board, approval queue. The interface exposes the orchestration process without becoming the product. If a rich dashboard is later justified, it launches as its own binary and the core's budget is unaffected.

---

## D-010 — Never elevated by default

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** The orchestrator runs at normal user integrity. Elevation is a narrowly scoped helper (`jean-elevated.exe`) launched through the Windows UAC consent flow, for a single operation, with the operation, target and reason shown to the user.

**Rationale.** UIPI means a non-elevated process cannot touch elevated windows — and gets **no error** (R-14). The tempting fix is to run everything as Administrator, which hands an LLM-adjacent process full machine control. The correct fix is to detect the mismatch and escalate deliberately.

**Consequences.**
- (+) Blast radius is bounded. A compromised or confused agent is still a normal user.
- (−) Some operations against elevated targets will fail with a typed `ElevationRequired` blocker. That is correct behaviour, and it must be reported clearly rather than retried.
- (−) The secure desktop (the UAC prompt itself) is unreachable by design (R-19). We do not attempt it.
- Never store an administrator password. Never attempt to bypass UAC. `auto_deny` includes `modify_uac`.

---

## D-011 — The model may request, never grant

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** Permission checks, argument validation, path handling, process supervision, task-state transitions, verification and secret handling are **deterministic code**. The policy engine does not import the model module.

**Rationale.** Prompt injection through screen or web content is the standard attack on computer-use agents (R-23). Any capability decision the model can influence is a capability decision an attacker can influence.

**Consequences.** `never_delegate_to_model` is a config list *and* a module-boundary rule. With `provider = "none"`, the four deterministic intents still work — the product is useful without any model at all.

---

## D-012 — Exit code 0 is not success

**Status:** ACCEPTED · **Date:** 2026-11-03

**Decision.** Every automated action declares how its result is verified. A tool without a `verify` method is **rejected at config load**.

**Rationale.** "The click API returned success" is not "the intended operation completed." "The agent said it's done" is not "the tests pass." Given that the field's SOTA on OSWorld-V2 is 21.3% (R-21), an orchestrator that reports honestly is more valuable than one that reports optimistically.

**Consequences.** Verification methods include: window-with-expected-PID exists, exit code **and** an output predicate, UIA tree re-read showing the expected state change, re-capture with a perceptual-hash diff, running the project's test command after an agent finishes. `verify_after_agent = true` by default.

---

## D-013 — sherpa-onnx for voice, in Rust

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** The voice worker uses the **official `sherpa-onnx` Rust crate 2.0.1** (Apache-2.0), statically linked.

**Rationale.** The original guide listed sherpa-onnx as a candidate with "multiple language APIs including Rust" — **verified true**. Sources are in-repo at `sherpa-onnx/rust/` with examples at `rust-api-examples/`; the crate was published 2026-10-27. It is the only stack shipping streaming ASR + TTS + VAD + keyword spotting together with **prebuilt Windows x64/x86/arm64 binaries and CUDA 12.x/13.x variants** in release v2.0.1.

**This means the voice worker is Rust.** No Python or C++ sidecar for speech — which was the assumption in the original plan.

**Consequences.**
- (+) One language for the core and the voice worker; one build system; no interpreter to ship.
- (+) Fallback available if the binding proves insufficient: the prebuilt `sherpa-onnx-*-x64-v2.0.1.exe` binaries driven over stdio.
- (−) **Model weight licenses are unverified** (R-08) — a per-model audit is a Phase 6 gate.
- (−) Runtime RAM is not benchmarked; budget a few hundred MB for the streaming zipformer and measure.

**Alternatives rejected.** *Piper standalone* — GPL-3.0 (R-12); its MIT voices are consumed through sherpa-onnx instead. *whisper.cpp / whisper-rs 0.16.0* — MIT but **batch only**, no true streaming, and it drags a C++/cmake build into Rust CI. *Moonshine direct* — **no Rust binding exists** (python/swift/android/wasm only); use its models *through* sherpa-onnx. *TEN-VAD* — Agora non-compete clause (R-13); use Silero. *openWakeWord pretrained models* — CC BY-NC-SA 4.0, so a wake word requires training our own.

---

## D-014 — Ollama as the default local model server

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** Ollama v0.40.14 (MIT) is the shipped default. `llama-server` (llama.cpp build b12078, MIT) is kept as an `openai_compatible` alternative profile.

**Rationale.** Both are MIT, both ship signed native Windows binaries, and **neither requires Docker or WSL**. Ollama wins on friction: model manager, `ollama-windows-amd64.zip`/`-arm64.zip`, NDJSON streaming from `POST /api/chat`, no build step. llama.cpp wins on control and offers an OpenAI-compatible route plus an Anthropic Messages route.

**Critical constraint from the research:** **vLLM has no native Windows build** — its own install index lists CUDA/ROCm/XPU/Apple/CPU only. Any plan assuming vLLM on Windows assumes WSL2. We do not.

**Consequences.** Neither is embedded; both are external processes reached over HTTP via `reqwest`. With `provider = "none"`, J.E.A.N. still works. `start_if_missing = false` — we do not auto-launch Ollama without consent.

---

## D-015 — `rusqlite` with `bundled`

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** `rusqlite` 0.41.1 with the `bundled` feature. Migrations via a hand-rolled `user_version` pragma loop.

**Rationale.** Embedded, serverless, vendored SQLite C (no system dependency), 143M downloads, published 2026-10-28. Its synchronous API is fine inside `tokio::task::spawn_blocking`.

**Alternatives rejected.** *`sqlx` 0.9.0* — MSRV **1.94** (the highest in the whole stack), still 0.x, and its async SQLite driver has more moving parts than a single-user local store needs. Keep as a migration path if we ever want fully async DB access. *`diesel` 2.3.14* — mature with first-class migrations, but ORM + schema DSL + codegen is disproportionate to seven modest tables.

---

## D-016 — `portable-pty` for ConPTY

**Status:** ACCEPTED · **Date:** 2026-11-04 · *the adoption is superseded by [D-024](#d-024--raw-conpty-through-windows-not-portable-pty); the rejection of the `conpty` crate stands*

**Decision.** `portable-pty` 0.9.0 (MIT, from wezTerm) for interactive terminals.

**Rationale.** The `conpty` crate (zhiburt) 0.7.0 was published **2024-09-23** with the repo's last push the same day and 24★ — unmaintained for over two years. `portable-pty` has 26M downloads, uses ConPTY on Windows, offers `native_pty_system()`, `PtySize`, child kill/wait, and is battle-tested by wezTerm nightly even though its last publish was 2025-02-11.

**Alternative noted.** `pty-process` 0.5.4 is the freshest (2026-10-19) and supports tokio — but its repo lives on a **self-hosted Gitea** (`git.tozt.self`), a supply-chain and availability consideration (R-07).

**Consequence.** ConPTY requires Windows 10 1809+. Fine for a Win11-first product.

---

## D-017 — `uiautomation` behind an internal trait

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** Adopt `uiautomation` 0.25.1 (Apache-2.0) for UIA, behind a `UiAutomationBackend` trait in `jean-windows`. Keep a partially-implemented raw-COM path from day one.

**Rationale.** The honest picture is better than feared: a **real, alive wrapper exists**, built directly on `windows ^0.62.2` + `windows-core ^0.62.2` (verified from its manifest — not a stale fork), ~1.04M downloads, wrapping `IUIAutomation`, elements, patterns, `UITreeWalker`, conditions and caching in safe Rust. But it is pre-1.0 with **204★ and a single primary author** (R-04).

**Consequences.**
- (+) UIA — our highest-leverage choice (D-004) — is reachable in safe Rust today.
- (−) The trait boundary is mandatory, not optional. Raw COM via `windows` feat `Win32_UI_Accessibility` + `Win32_System_Com` + `Win32_System_Ole` is ~10× the code and involves `unsafe` COM with `VARIANT`/`SAFEARRAY`/`BSTR` marshalling — but it is mechanical, and having it partially written means a stall costs days, not a rewrite.
- Final fallback: **FlaUI** (C#, MIT, 3.2k★, pushed 2026-08-13) as an out-of-process sidecar, at the cost of a .NET runtime.

**Note.** The old `com` crate (com-rs 0.6.0, last publish 2021-10) is **not** a fallback. Crates that do not exist and must not be planned around: `windows-uiautomation`, `uiautomation-rs`.

**As built (2026-11-06) — see D-048 for the full accounting.** Two corrections, one of them only a name:
the trait shipped as `accessibility::AccessibilityBackend`, not `UiAutomationBackend`, and it answers five
questions rather than the two this entry implies. The wrapper shipped exactly as decided —
`=0.25.1`, `default-features = false`, `features = ["control", "pattern", "input"]`, isolated to
`uia.rs`. **The second half of the Decision did not happen:** the raw-COM path was *not* "partially
implemented from day one", is not implemented now, and `Win32_UI_Accessibility` is not in this workspace's
feature list. That was a choice made against observation, not a forgotten line: the two UIA gaps Phase 4
hit were in J.E.A.N.'s own call sites (`.ok()` swallowing pattern errors as absences; a state read that
needed a fifth boundary question), and 21 native tests now pass through the wrapper without the fallback
being needed. The honest consequence is that this entry's *"a stall costs days, not a rewrite"* is
**unproven** — the escape hatch is documented, and its cost has never been paid. R-04 stays open on that
basis.

---

## D-018 — Browser automation via `@playwright/mcp`

**Status:** PROPOSED · **Date:** 2026-11-04 · *needs a decision before Phase 3*

**Decision (proposed).** Spawn `@playwright/mcp` 0.0.84 (Apache-2.0) as a stdio MCP child via `rmcp`'s `transport-child-process`.

**The open question.** That path requires **Node.js** on the machine — which cuts against "lightweight, no runtime". The alternative is `headless_chrome` 1.0.22 (pure Rust CDP), and **Windows 11 ships Edge**, so CDP is available with no browser install and no Node.

**Options.**

| Option | Cost | Benefit |
|---|---|---|
| `@playwright/mcp` | Node.js dependency | Accessibility-tree driven, token-efficient, maintained by Microsoft, matches our MCP investment |
| `headless_chrome` (CDP) | Pure Rust, no Node | Needs Chrome/Edge (Edge ships with Win11); we write the CDP orchestration |
| `thirtyfour` (WebDriver) | chromedriver binary | Most actively maintained pure-Rust option (0.38.1, 2026-10-21) |

**Leaning:** CDP-over-Edge as the **default** (zero added runtime, consistent with D-001 and the no-Docker stance), with `@playwright/mcp` as an opt-in for users who already have Node. **This needs to be settled before Phase 3 starts.**

**Rejected.** The `playwright` Rust crate — 0.0.20, **last updated 2022-08-20**. Dead. There is no maintained Rust Playwright client (R-05).

---

## D-019 — ShowUI-2B is the grounding model; Jedi-3B is blocked

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** Pixel grounding, when needed at all, uses **ShowUI-2B** (weights confirmed **MIT** on HuggingFace; GGUF `localattention/ShowUI-2B-Q4_K_M-GGUF` carries an MIT tag), served by llama.cpp. **UI-TARS-1.5-7B** (weights confirmed **Apache-2.0**) is the heavier opt-in.

**Blocked.** **Jedi-3B-1080p** (`xlang-ai/OSWorld-G`). The repo is Apache-2.0 but the **HF weight page is gated (401)**, so the upstream weight license is `UNVERIFIED`. The available GGUFs are tagged `apache-2.0` **by the quantizer**, which is not evidence of the upstream license. **Do not ship it** (R-09).

**Rationale.** Grounding is the *last* rung of the D-004 hierarchy and should rarely fire. A 2B MIT-licensed model that runs on CPU/Vulkan is the right size for a fallback. A 7B model is opt-in only.

**Rejected.** *OmniParser v1 icon detector* — Ultralytics YOLO **AGPL-3.0 lineage** (R-10); v2 weights (MIT) only, and the repo itself is CC-BY-4.0. *UGround* — MIT repo but the HF weight card has **no license tag**. *SeeClick*, *Aguvis*, *Aria-UI*, *ShowUI-π* — no license file or untagged weights. *Holo1* — no public repo; the CLI is archived and points to a paid API.

---

## D-020 — Aider is not a supported target

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** Aider is not an orchestration target.

**Rationale.** Verified from its argument parser: it has `--message/-m`, `--message-file/-f`, `--yes-always`, `--stream` (a *text* streaming toggle), `--pretty`. It has **no `--json`, no `--output-format`, no MCP, no ACP, and no server**. A code search found zero `stream-json` or ACP hits. The only supervision path is PTY-scraping ANSI output, which cannot reliably distinguish progress from failure. Last push 2026-05-22.

**Consequence.** If a user has Aider installed, J.E.A.N. can *launch* it as a generic CLI tool at the user's request. It cannot *supervise* it, and we do not claim otherwise.

---

## D-021 — Claude Code is drivable but never bundled

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** `agents.toml` ships with `claude-code` **`enabled = false`** and `requires = "user-installed"`. We detect it; we never distribute it.

**Rationale.** `anthropics/claude-code` is **proprietary** — LICENSE.md reads *"© Anthropic PBC. All rights reserved. Use is subject to Anthropic's Commercial Terms of Service"*; GitHub's `license.spdx_id` is `NONE`; npm's license field is `"SEE LICENSE IN README.md"`. The ACP adapter `@agentclientprotocol/claude-agent-acp` 0.100.0 is Apache-2.0 and the Python `claude-agent-sdk` 0.2.177 is MIT — but both merely *drive* the proprietary CLI. It also requires an Anthropic account (cloud).

**Consequence.** Our first-class, shippable connectors are **Codex** (Apache-2.0, native Windows MSVC binaries) and **OpenCode** (MIT, native Windows zips). Those are the two we build in Phase 3.

---

## D-022 — Permissive licenses only, enforced mechanically

**Status:** ACCEPTED · **Date:** 2026-11-04

**Decision.** `cargo-deny` runs in CI and blocks any non-permissive license or banned crate. The allow-list is MIT, Apache-2.0, BSD-2/3-Clause, CC0-1.0, 0BSD, ISC, Unlicense, PSF-2.0.

**Rationale.** The adopted stack contains **no copyleft anywhere** — that is a property worth protecting with a gate rather than with discipline. Several tempting candidates are copyleft or source-available and would silently contaminate a proprietary product: Piper (GPL-3.0), Skyvern (AGPL-3.0), claude-squad (AGPL-3.0), koboldcpp (AGPL-3.0), Honcho (AGPL-3.0), Khoj (AGPL-3.0), `sqlite_orm` (AGPL-3.0), Inngest's server (SSPL-1.0), Restate (BSL 1.1), Dify (Modified Apache), n8n (Sustainable Use License), LobeHub (Community License), Open WebUI (branding clause).

**Consequences.**
- (+) The license question is answered by CI, not by memory.
- (+) Explicit bans on `winapi`, `tauri`, `conpty` and `device_query` encode decisions D-009, D-016 and R-02 as machine-checkable rules.
- (−) Model **weights** are outside cargo-deny's reach. R-08 and R-09 must be handled by a manual per-model audit — a Phase 6 exit gate.

---

## D-023 — Phase 1 launches detached and verifies with a window for the pid

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `jean launch` starts the application with `CREATE_NEW_CONSOLE`, outside any Job Object, with handle inheritance off; the verified outcome is *a top-level operator window carrying that pid appeared within a bounded wait*.

**Context.** Phase 0's supervision exists to capture output and to guarantee that nothing outlives the task. An application launch has the opposite requirements: the user wants the app to stay open after `jean` exits, and a GUI app that is attached to our console inherits a dead parent and a shared stdin.

**Rationale.**
- Joining a launched app to our Job Object would kill Word the moment `jean launch word` returned. So a detached launch takes **no** job — and the honest consequence is that JEAN cannot then clean it up. That asymmetry is written into the intent module's documentation rather than hidden.
- `CREATE_NEW_CONSOLE` with `bInheritHandles = false` and an unpopulated `STARTUPINFOW` is the only combination that gives the app its own console and no path back into our pipes.
- Exit code 0 from a launcher proves nothing (D-012). Chrome, Outlook and every single-instance app exit `0` *and* open nothing, because they hand the request to an already-running process.
- So verification is a window: `EnumWindows` → `GetWindowThreadProcessId` → visible, non-empty trimmed title, and not a `WS_EX_TOOLWINDOW` unless it has an owner. Bounded at 8 192 windows examined, 256 reported, 512 characters per title, polling 100 ms (clamped 25 ms…1 s) for `window_timeout` (default 5 s, floor 1 s).
- A pid that dies with no window is reported as `ExitedWithoutWindow`. A pid whose *name* now has a window while its own pid has none is reported as `HandedOff` with verdict **`Unverifiable`** — the honest answer, not a pass.
- `creation_time_ms` is read from the process handle *before* it is closed, so the record still says when the child started even though nothing is held onto it afterwards.

**Consequences.**
- (+) Launched applications survive `jean`, get their own console, and cannot starve or read our stdin.
- (+) "Window for pid" is a claim a GUI user can check by looking at the screen.
- (−) A detached launch is **not** cancellable and **not** reaped by the job. `jean` records the pid and stops there; killing it stays the user's call, and inspection deliberately never terminates anything.
- (−) A windowless but successful launch (a background agent, a tray-only app) is `NotProven`, not `Failed`. `--expect-window` exists to make the check optional, not to make it pass.
- (−) We never call `SetForegroundWindow`, `AttachThreadInput` or `ShowWindow`. Stealing the user's focus is a policy refusal, not a convenience.

**Alternatives rejected.** *Supervised launch inside a Job Object* — correct cleanup, useless product: the app dies with the CLI. *`ShellExecuteW`* — returns success for a launch the shell merely *accepted*, gives no pid to verify against, and silently falls back to the run dialog. *Verifying with "the process still exists"* — every single-instance launcher satisfies that while opening nothing.

---

## D-024 — Raw ConPTY through `windows`, not `portable-pty`

**Status:** ACCEPTED · **Date:** 2026-11-05 · *supersedes the adoption half of D-016; the rejection of the `conpty` crate stands*

**Decision.** Interactive commands use ConPTY directly — `CreatePseudoConsole`, `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, `STARTUPINFOEXW` — through the `windows` crate we already vendor. `portable-pty` is not a dependency.

**Context.** Phase 1 had to add an interactive terminal path. D-016 rejected the abandoned `conpty` crate in favour of wezTerm's `portable-pty` 0.9.0. Implementing against it then showed the two crates solve slightly different problems.

**Rationale.**
- `portable-pty` is a *terminal emulator's* abstraction: `PtyPair`, a `SlavePty` you hand a shell, its own `PtySize`, its own child-killer, its own event loop. We needed a *supervisor's* abstraction: the same Job Object, the same `CancelToken`, the same timeout arithmetic and the same `BoundedCollector` that Phase 0 already gives the pipes path.
- Our supervision loop keys everything off `OwnedHandle` and one output stream. With `portable-pty` every one of those — job assignment, `ResumeThread` ordering, the cancel pipe, the grace period — has to be re-derived from a `Box<dyn DynChild>` that does not expose the process handle we need for `AssignProcessToJobObject`.
- Raw ConPTY is six hundred lines including its own unit tests, inside a crate that already links `Win32_System_Console`, `Win32_System_Pipes` and `Win32_System_JobObjects`. `portable-pty` would have added `filedescriptor`, `shared_child`, `vsshell`, `downcast-rs`, `serial-term` and a WinCon layer we would immediately bypass.
- cargo-deny (D-022) is a gate we have to keep honest; the smallest dependency that does the job is the one that stays easy to audit.
- The fallback that keeps the promise intact: a tool declared `interactive` whose platform is not Windows is refused with an explicit degradation note, and every non-interactive tool keeps the plain-pipe path unchanged.

**Consequences.**
- (+) One supervision code path, two transports. The pty branch merges stdout and stderr into a single `console.log` artifact and sets `stderr_path = None` — which the API *refuses* to accept for a pty spec, so the two cannot drift.
- (+) No new third-party code in the interactive path, which is the path most likely to be asked about in a security review.
- (−) We now own ConPTY quirks directly: the resize message is best-effort, a keystroke relay is ours to write, and raw-mode input has to be guarded and restored (`RawInputGuard`).
- (−) `portable-pty` would have been cross-platform. This is not — which is fine for a Windows-first product and consistent with the whole `jean-process` crate.

**Alternatives rejected.** *`portable-pty` 0.9.0* — the right library for a terminal, the wrong one for a supervisor (above). *`conpty` 0.7.0* — still abandoned (last publish 2024-09-23); D-016's rejection stands. *`pty-process` 0.5.4* — freshest, tokio-aware, but hosted on a self-hosted Gitea (R-07). *No pty at all in Phase 1* — the roadmap asked for it, and `jean exec --interactive` is the one place where a pipe is provably not enough.

---

## D-025 — Two more `windows` features: `Win32_UI_WindowsAndMessaging`, `Data_Xml_Dom`

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** Add exactly two features to the workspace-level `windows` manifest: `Win32_UI_WindowsAndMessaging` (window enumeration) and `Data_Xml_Dom` (toast XML). No other additions.

**Context.** Phase 1 added window-based launch verification (D-023) and toast notifications. Both are Windows APIs `windows-rs` projects for free, but only if the feature is switched on.

**Rationale.**
- `EnumWindows`, `GetWindowThreadProcessId`, `IsWindowVisible`, `IsIconic`, `GetWindowLongPtrW` and `GetWindowTextW` are all in `Win32_UI_WindowsAndMessaging`. The alternative — a second `windows` crate version or a hand-written `extern` block — would fork the type system our code already speaks.
- `ToastNotificationManager`, `ToastNotificationContent` and the XML document the Windows toast runtime demands come from `Data_Xml_Dom`; the notification surface was already listed as raw-FFI territory in D-001's consequences.
- Features are declared once at the workspace root and inherited, so the surface stays a single, reviewable list — one comment per feature explaining what needs it. This is how R-02 (Windows API sprawl) stays a checklist instead of a surprise.

**Consequences.**
- (+) Compile-time, not run-time, guarantee that no other module can quietly reach a new Win32 surface.
- (−) A marginally longer build. Acceptable, and the same trade already made for the Phase-0 features.

**Alternatives rejected.** *`windows-sys`* — no `HWND`/`BOOL`/`PWSTR` wrappers, and it would mean re-implementing the projections we depend on for enumerating windows. *A separate `windows` version for `jean-core`* — two incompatible type sets across one call boundary; the worst possible outcome. *Skip toasts* — the roadmap asked for a notification path, and a tray-less CLI still needs one.

---

## D-026 — A tool health probe is a path lookup. It never runs the tool.

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `jean health` answers "is this tool runnable here?" by resolving the declared command against the tool's own `PATH` (plus its `fallback_command`) with a filesystem lookup. It never spawns the tool. A tool is marked unhealthy after `mark_unhealthy_after` consecutive failed probes (default 3), and a healthy result is trusted for the freshness interval instead of re-probed.

**Context.** The roadmap asked for tool health checks without a *definition* of "healthy". The obvious first implementation — run `tool --version` — is a program that executes arbitrary registered commands on a timer.

**Rationale.**
- Running a tool to learn about it is exactly the thing the policy tier exists to gate. A health pass that spawns children would either bypass approval or demand approval for a question the user did not ask.
- "Absent from `PATH`" is the failure mode JEAN can actually do something about — install it, or fix `tool.env`. A tool that is present but crashes is an *execution* failure, and the run pipeline already records that with an exit code, an artifact and a verdict.
- A pure lookup is safe on the discovery surface too: nothing executable is touched, which is the same rule the application-discovery walk follows (`.lnk` targets are read, never opened).
- Cost is near zero, so `jean health` can be run before every operation without changing the shape of the CLI, and a cached answer is free to consult.
- Disabled tools get `Disabled` rather than `Absent`: a different reason, a different fix, and a status the user should not have to infer.

**Consequences.**
- (+) Health is honest about what it measured — the `ProbeAnswer` enum names the resolution it actually performed, and the detail is redacted before it reaches the ledger.
- (+) `mark_unhealthy_after` prevents one transient filesystem hiccup from blacklisting a tool.
- (−) A tool that is on `PATH` but broken reports healthy. The reply is the record it produces when it actually runs, not a guess made beforehand.
- (−) A wrapper script that exists but is not executable, or a shim pointing at a deleted target, still reads as present.

**Alternatives rejected.** *Run `--version` and parse it* — executes unapproved commands, needs a per-tool version-flag dialect, and turns a health check into a task. *`where.exe`* — spawns a process to do what one loop over `PATH` does, and its exit code is the only useful signal it returns. *Probe lazily at run time only* — then the first user of a broken tool is the one who discovers it, with no history to point at.

---

## D-027 — A remembered approval is bound to operation + resolved scope, not tool + tier

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** The `approvals` ledger stores a digest over `(operation kind, tool id, resolved target, validated argument vector)` — the *resolved* target and the *validated* arguments — and a remembered approval only covers a later operation whose digest matches. The shipped key is `policy.approval_scope = "operation_and_arguments"`, and `tool_and_tier` is a hard error at load.

**Context.** The settings document sketched `scope = "tool_and_tier"` — "approve `git` at tier `restricted`, never see the prompt again". Phase 1 implemented it and rejected the shape.

**Rationale.**
- `tool_and_tier` means approving `git` covers `git push --force origin master` forever. A scope that cannot distinguish those two is not a scope; it is a switch.
- The target is digested **after** resolution: a canonical executable path, a canonical search root, a pid. A digest over text the user typed either never matches (`C:\Dir` vs `c:/dir`) or matches too much (`--force` appearing in both an unrelated argument and the dangerous one).
- Arguments are length-prefixed before hashing. Concatenating `["a", "bc"]` and `["ab", "c"]` yields the same string; a canonical, injective encoding makes that impossible.
- The digest is stored next to the human-readable, **redacted** summary, so `jean approvals` can show what was actually granted. An approval the user cannot read is not an approval.
- The digest function is `std`'s `DefaultHasher` over the canonical form, not a hash crate. Its keys are not promised stable across Rust releases, so a toolchain bump can make every stored approval stop matching — and the result of a non-matching digest is a **prompt**, not a grant. A digest that failed *open* would need SHA-2; one that fails closed does not, and `03-DEPENDENCIES.md` gains no entry for it.
- Tiers still drive whether a prompt happens at all — `auto_deny` and elevation refusal are checked *before* the ledger is consulted, so a remembered approval can never out-rank a rule.

**Consequences.**
- (+) "Remember" means *this command, these arguments*, and the next different invocation prompts again. That is what a user believes they clicked.
- (+) The audit line records the `approval_id`, so any run can be traced back to the grant that covered it.
- (−) More prompts than `tool_and_tier` would produce. Deliberate: the friction is the feature.
- (−) A renamed path or a rebuilt argument list silently invalidates a remembered approval — the safe direction for a wrong answer. So does a Rust release that changes `DefaultHasher`'s keys, which costs one prompt per approval and no more.

**Alternatives rejected.** *`tool_and_tier`* — the original plan; blanket, unreadable, and indistinguishable from disabling approval. *`tool_and_arguments` without resolution* — unstable across quoting and path spelling, so approvals expire at random. *`always_allow` per capability* — a capability is a *class* of targets ("may read the filesystem"), which is the right unit for a tier, not for a grant.

---

## D-028 — Re-armable signal handler, idempotent logging, and exit code 5 for a refusal

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** Three Phase-1 infrastructure changes, decided together because one test exposed all of them: the console-control handler is installed once but reads a **re-armable** cancel token; `logging::init` is idempotent; and a policy refusal returns **exit code 5**, distinct from every other terminal state.

**Context.** Phase 0 assumed one process = one task, so `arm()` used a `OnceLock` and `logging::init()` installed a global subscriber unconditionally. Phase 1 runs several intents inside one `jean` invocation, and its tests need to build a `Context` repeatedly in a single test binary.

**Rationale.**
- *Handler.* With a `OnceLock`, arming the second intent failed, so Ctrl-C during the second intent of a run could start it but never stop it — the worst case a cancellation feature can have. The token now lives in `OnceLock<Mutex<Option<CancelToken>>>`, the install is guarded by an `AtomicU32`, and `arm`/`disarm` are called per intent. The handler takes the poisoned lock rather than skipping, because a panic while a child is running is precisely when cancellation still has to work.
- *Logging.* `tracing` has one global default subscriber; a second `init()` panics. `init` now returns early once installed (first configuration wins) and rolls the flag back if setting the default fails, so a genuinely broken log directory still reports an error instead of silently running unlogged.
- *Exit code.* A refusal is not a failure: nothing ran, nothing needs attention, and no output was produced. Folding it into `1` tells a script "the tool is broken" when the truth is "policy said no", and a user who pipes `jean` into a loop must be able to tell those apart. The ladder is now `0` proven · `3` ran but not proven · `4` needs attention · `5` refused by policy · `130` cancelled · `1` failed.
- `--yes` is accepted, **reported to the operator as reserved for Phase 2, and does not answer a prompt**. A grant comes from a person or from the configuration (D-011); an auto-yes inside the CLI would be neither. `auto_deny` and the elevation refusal deny with `5` regardless.
  *Amended by **D-039** (2026-11-05): the claim "an auto-yes inside the CLI would be neither a person nor the configuration" still stands, and `--yes` is still a no-op. What changed is that a person's own `jean do` now carries the run, so the flag has nothing left to bypass; `--prompt-each` is the option that alters how a person is asked.*

**Consequences.**
- (+) Multi-intent invocations are cancellable at every step, and one test binary can build many contexts.
- (+) A refusal is self-describing to both a human and a script, and the audit trail already carries the reason.
- (−) `5` is a new code every consumer must learn; it is in the CLI's help and in `IMPLEMENTATION_STATUS.md`.
- (−) First-config-wins logging means a second `init` in one process cannot change the level. Acceptable — that is a test-only convenience, and it is stated in the doc comment.

**Alternatives rejected.** *One handler per intent, installed and removed repeatedly* — a race where a signal arriving between `SetConsoleCtrlHandler(remove)` and the next `add` is lost outright. *`try_init()` and swallow the error* — hides a real failure to create the log directory behind a silent success. *Reuse `1` for denials* — cheapest option, and it makes the pipeline look broken when it is working as designed. *`--yes` bypasses everything* — D-011 says the model may request and never grant; a flag that grants on request would be the same mistake with a nicer name.

---

## D-029 — Model transport is a hand-rolled HTTP/1.1 client over `std::net`. Plain HTTP only, no TLS

**Status:** SUPERSEDED by **D-038** · **Date:** 2026-11-05 · **Superseded:** 2026-11-05

> **Why this was reversed.** The operator required a maintained client with verified HTTPS and chunked responses rather than a
> hand-written transport, and required it not to be treated as a limitation to document. Reading the rule back
> after a TLS client existed also exposed a real defect (C-1 in `IMPLEMENTATION_STATUS.md` §28): refusing `https`
> while accepting a remote `http` endpoint meant the *only* reachable remote shape was one that would have sent
> the bearer token in the clear. The reasoning below is kept because the trade-offs it records — ownership of the
> deadline, the byte ceiling and the cancel flag — are still the reasons `jean-model` configures its client
> narrowly instead of taking a default agent.

**Decision.** `jean-model` speaks HTTP/1.1 itself — `TcpStream`, one request per connection, `Content-Length` bodies, and a reader for streamed answers — instead of taking an HTTP crate. `https` is refused at configuration load with a stated reason, and a redirect is refused at request time.

**Context.** Phase 2 needs one POST to a loopback Ollama or a local `llama-server` and nothing else: no cookies, no connection pool, no auth scheme beyond one bearer header. The candidate dependencies (`reqwest` with its hyper/rustls/tokio closure, or `ureq` with a rustls closure) each add tens of crates to a workspace whose inventory `03-DEPENDENCIES.md` curates by hand and whose license gate is `deny.toml` (D-022). The batch brief also forbade installing software, so a TLS provider could not have been exercised on this machine anyway.

**Rationale.**
- The surface actually used is four things: connect, write a bounded request, read a bounded response, stop early on a deadline or a cancel. That is smaller than the API of any client crate, and each of the four is where JEAN's own rules live — byte ceilings, timeouts, cancellation.
- Plain HTTP is enough for the two supported providers as scoped: both are expected on loopback. A deployment that needs TLS terminates it locally (a proxy at `http://127.0.0.1:x`), which reaches the same providers without JEAN holding a trust store.
- Refusing `https` rather than accepting it and sending plaintext to a remote host is the honest version of the same limitation. A silent downgrade is a security bug; a load error is a configuration question.
- Timeouts and cancellation are polled in 50 ms slices on a non-blocking read, which is what lets one deadline cover headers *and* body, and lets Ctrl-C stop a model request mid-flight — something a blocking client library does not give you cheaply.

**Consequences.**
- (+) Zero new third-party crates for Phase 2; the dependency tree is what Phase 1 had.
- (+) Byte ceilings, the request deadline and cancellation are enforced by the code that uses them, and are testable against an in-process stub server with no network.
- (+) Chunked transfer, redirects and keep-alive surprises are refused loudly instead of being half-supported.
- (−) JEAN cannot talk to an `https` endpoint directly. Stated in `jean model status`, in the config refusal, and in `IMPLEMENTATION_STATUS.md`; needs a loopback proxy.
- (−) We own HTTP parsing. Mitigated by refusing everything outside the small subset, and by stub tests over status lines, header shapes, oversized bodies and stream reassembly.

**Alternatives rejected.** *`reqwest`* — the widest-used client, and a dependency closure several times the size of this workspace, for one POST per command. *`ureq`* — smaller, still a TLS provider in the tree that this build cannot use or test. *`WinHTTP`/`WinINet` through `windows`* — no new crates and OS TLS, but it makes the crate Windows-only, and transport is the part worth keeping portable for a later Linux runner. *Accepting plain HTTP to a remote host* — would send a bearer token in the clear; refused instead.

---

## D-030 — One flat `[model]` table; `anthropic` and `openai` are refused at load

**Status:** ACCEPTED, amended by **D-038** · **Date:** 2026-11-05

> **Amendment.** One consequence of this decision assumed D-029 held: the endpoint checks refused `https` because
> no client existed and allowed any `http` host. With a verified TLS client (D-038) the asymmetry became a hole,
> so the rule is inverted — `https` to any host is accepted and verified, plain `http` is accepted only for a
> loopback endpoint, and a non-loopback `http` `base_url` is a load error. The flat table, the provider list and
> the refused cloud providers are unchanged.

**Decision.** `04-SETTINGS.md` planned `[model.ollama]`, `[model.openai_compatible]`, `[model.anthropic]` and `[model.openai]` sub-tables. Shipped is one flat `[model]` table (`provider`, `base_url`, `model`, `api_key_env`, and the bounds), and a `provider` other than `none | ollama | openai_compatible` is a load error.

**Context.** A configuration selects exactly one provider per run, so per-provider tables hold three copies of the same three questions and let a reader ask the wrong one. The two named cloud providers are out of reach in this batch by instruction: no paid API without explicit authorization.

**Rationale.**
- Unknown keys in a config layer are a warning, mistyped values are a hard error. A stale `[model.anthropic]` block would have been the first case — silently ignored, with the operator believing they had configured a provider.
- Refusing `anthropic`/`openai` at load turns an instruction from this batch into a property of the product. A future phase that authorizes them adds an adapter and a name to one list; no configuration file can reach a paid endpoint by accident today.
- `require_provider_for` and `deterministic_fallback` were dropped as keys: `provider = "none"` *is* the deterministic fallback, and a second setting that can only say the same thing is a way for a file to disagree with itself.

**Consequences.**
- (+) One table, one provider, one set of bounds; `jean model status` prints exactly what would be sent.
- (+) The credential variable's *name* is configuration; its value stays in the environment (D-037).
- (−) Switching providers means editing four lines rather than uncommenting a block. Acceptable — a machine has one provider, and `jean model status` shows which.
- (−) Per-provider overrides are not expressible. Deliberate: the bounds are JEAN's limits, not the provider's dialect.

**Alternatives rejected.** *Per-provider sub-tables as planned* — three copies of the same fields, a silent ignore for the two providers this build lacks, and a loader deciding which table wins. *Accept the names and return "not implemented" at request time* — the error then arrives after a person typed a goal, and after a config file that looks valid has been written.

---

## D-031 — The plan schema is a closed vocabulary of four operations

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** A plan is `{"schema_version":1,"goal":…,"steps":[…]}` where each step is one of four operations — `run_command` (a registered tool id plus a typed argument vector), `launch_app` (an application *name*), `inspect_process`, `locate_file` — each with a mandatory `verify` declaration. Every struct is `deny_unknown_fields`. There is no field for an executable path, a command string, a shell, a working directory, an environment variable, a policy tier, or a deadline longer than the tool's own.

**Context.** D-011 says the model may request and never grants. The Phase-1 intents already refuse those fields when typed; Phase 2 must not make the model a better-connected caller. The question this decision answers is how a *schema* enforces that, rather than how a *check* does.

**Rationale.**
- A check can be bypassed by a code path that forgets to call it. A schema cannot: there is no place to put the request. `{"kind":"run_command","command":"notepad.exe"}` does not parse, and the refusal names the field the model invented.
- `run_command` takes a **registered tool id**, so the executable comes from `tools.toml` — the registry an operator edits — and arguments are a typed vector (`text`/`integer`/`boolean`/`path`/`pid`), never a string a shell would re-parse. A registered shell tool can still be run; that is not the same as a model being able to write a shell script, and the wording here is deliberate.
- Internally-tagged `operation.kind` with nested variants rather than `flatten`: serde cannot combine `flatten` with `deny_unknown_fields`, and that combination is the only way an invented key becomes an error instead of being dropped.
- `verify` is mandatory, with no default meaning "unchecked" (D-012), and each operation lists the proofs it can honestly produce — a launch is proven by a window for its pid, not by an exit code.

**Consequences.**
- (+) "Cannot call a tool outside the registry" is a parse failure, tested by `a_field_the_schema_does_not_have_is_refused_not_ignored`.
- (+) The catalog in the prompt is generated from the live registry, so the model is never told about a tool this machine does not have.
- (−) A new operation means a schema change and a release — by design; a model cannot add a verb.
- (−) `depends_on` is a stop-premise, not a data flow: no value passes between steps. Stated in the schema doc and in the preview, so nobody reads it as a pipeline.

**Alternatives rejected.** *JSON-Schema-driven dynamic tools (function calling)* — every provider spells the same thing differently, and it lets the model propose a call shape the code must invent handling for at runtime. *A `command` field validated against a blocklist* — a blocklist is a race between an attacker's spelling and a reviewer's imagination. *An allowlist of executables inside the plan* — a second, weaker registry that would drift from `tools.toml`. *A free-form script step* — the exact capability this phase must not add.

---

## D-032 — Plan validation is split: structure in `jean-protocol`, the world in `jean-core`

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `Plan::validate_structure` answers every question the plan text can answer alone (schema version, bounds, ids, argument types, the dependency graph, whether the declared verification is one the operation can produce). `jean_core::planner::validate` answers the questions that need this installation (is the tool registered, enabled, not elevated, not auto-denied; do its arguments survive translation; is this root inside `workspace.root`; are the plan's ceilings inside configuration). Both run before the first side effect.

**Context.** `jean-protocol` must not depend on the store, the policy engine or the process layer (D-011 keeps the model side of the dependency graph thin), yet the shape checks are the ones that most need to be unit-testable and to run without a database.

**Rationale.**
- Structure checks are pure functions on text, so a cycle or an oversized needle is caught in a test that opens nothing — and the same function is what a later `jean plan --from-file` could run with no context at all.
- World checks need live facts, and their answer changes with the machine: the same plan is valid here and refused where the tool is disabled. Putting them in `jean-core` keeps the authority where the configuration is.
- The split also fixes the order of reporting: structure errors first (the model can fix them), then world errors (the operator can fix them). One list, one report.
- Diagnostic codes are a stable operator-facing vocabulary — `[tool_not_registered]`, `[path_outside_workspace]`, `[forward_dependency]` — printed in the refusal and sent back to the model for its one bounded retry, so a replanning attempt has something to act on.

**Consequences.**
- (+) A refusal test can assert row counts instead of log text: nothing was written because validation ran before the write.
- (+) Every refusal is a list, so one retry can fix three problems.
- (−) Two places define bounds for one plan. Mitigated: `jean-core` calls `validate_structure` itself, and the ceilings live in `jean-protocol` as `MAX_*` constants the world stage imports.
- (−) A code can appear twice if both stages check the same invariant. Accepted for needles and empty verification, where the world stage also runs standalone in `jean plan` of a stored plan; duplication in a diagnostic is cheaper than a missing one.

**Alternatives rejected.** *One validator in `jean-core`* — the simplest structure, and it would make shape bugs depend on a database. *One validator in `jean-protocol` with injected facts* — a trait for "the world" that is really `jean-core` wearing a coat, still needing the registry. *Fail fast on the first error* — cheaper to implement, and it throws away the one thing that makes a bounded retry worth spending.

---

## D-033 — Steps run in declaration order; a forward dependency or a cycle is refused rather than re-sorted, and a failure stops only its dependents

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** Execution order is declaration order. A step that depends on one listed at or after itself is a plan error (`forward_dependency`), and a cycle is an error naming the path (`dependency_cycle`). At run time a failed step skips the steps that depend on it (`unmet_dependency` → `skipped`) and leaves steps the plan did not connect to it running; only a cancellation or an uncertain step stops the whole plan.

**Context.** A topological sort is forty lines and would make every plan runnable however the model listed it. D-028 leaves one active task per process, so there is no scheduler to feed. The preview the operator sees is built by `render_plan(plan, order)` from the same rows that execute.

**Rationale.**
- A sort that silently reorders means the operator previewed one sequence and a different one ran. Refusing the forward reference keeps "what you saw is what ran" true without a scheduler, and the diagnostic contains the fix: *put the dependency first*.
- Sequential order is also what makes the crash record answerable: one step row moves `pending → running → settled`, so `running` after a restart has exactly one meaning.
- Stopping only dependents is what "step failure with dependents stopped" asks for, and it keeps the parent's state honest. The first cut here cancelled everything after any failure, which made a plan whose *first* step failed report `cancelled` (exit 130) for what was a failure (exit 1); the Phase-2 tests caught it.
- A failed step still decides the parent's state, so stopping early can never look like success.

**Consequences.**
- (+) No scheduler, no thread pool, no concurrency the cancellation design cannot supervise (D-028).
- (+) The plan the operator previewed and the plan that ran come from the same function over the same rows.
- (−) A model that lists steps in the wrong order has its plan refused. Accepted: the refusal says what to reorder, and the bounded retry fixes it.
- (−) Independent steps do not run in parallel, so four probes take four times as long. Correct for this build, where the cost is seconds and the alternative is a second runner.

**Alternatives rejected.** *Topological sort with a cycle check* — tolerates sloppy plans and breaks the preview guarantee. *A parallel DAG scheduler* — needs a per-step cancel scope the single-active-task handler does not have, and the brief explicitly does not want a workflow framework. *Stop the whole plan at the first failure* — simplest, and it renames a failure as a cancellation.

---

## D-034 — A step executes as exactly one existing `Intent`; the stricter of the two verifications wins

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `build_intent` maps each plan operation onto one `jean_core::intent::Intent`, which goes through `intent::execute` — the same policy decision, audit writer, Job Object supervisor, ConPTY path and verifier Phase 1 was tested with. A step's verdict is the **stricter** of the tool's declared verification and the step's own declaration. A plan may tighten a deadline; the registry's is the one that applies, and the report says so.

**Context.** The obvious Phase-2 shape is a plan executor that spawns children itself. That would be a second path around the engine — the exact thing D-011 and the batch brief forbid.

**Rationale.**
- One act path means one audit path. A plan step and the same command typed at the console produce the same decision, the same ledger row and the same artifacts; the only difference is a plan row above it saying which proposal it came from.
- Taking the *worse* of two verdicts closes the direction that matters: a plan cannot make an unproven run look proven by declaring a weaker check (a tool that requires a needle still requires it), and cannot weaken the registry. The converse is enforced too — a step that promises more is checked for the extra promise.
- `PlanStepState` is deliberately its own eight-state enum rather than borrowing `TaskState`: a step is asked "did it run, and was it proven", a task is asked "what did the operator's request become", and one shared vocabulary would let a reader conflate `skipped` with `cancelled`.
- Read-only probes are recorded as tool runs with no child pid, so `jean task` shows the link and shows nothing was spawned — the difference between an audited read and an unaudited one.

**Consequences.**
- (+) No second process runner, no second policy call, no new way to spend the approval budget.
- (+) A plan cannot out-verify the tool it calls, and a test proves it.
- (−) Each of the four operations must map onto an intent that already exists; a step needing something Phase 1 has no intent for is refused at validation rather than executed by a new code path.
- (−) Two verification layers mean two verdicts to reconcile. `worse_of` is twelve lines, ordered by what an operator can do about each (`not_proven` outranks `unverifiable`).

**Alternatives rejected.** *A plan executor that spawns children directly* — faster to write, and it puts a second, less-audited door in the house. *Let the step's declaration override the tool's* — the plan then weakens the registry. *Accept either verdict (union)* — the same hole from the other side. *Reuse `TaskState` for steps* — saves an enum and loses the distinction between "the plan stopped" and "this step never started".

---

## D-035 — The plan's parent is a task row that owns the plan; an interrupted plan becomes `needs_attention`, and a step recorded `running` is never reopened

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `jean do` opens one parent task — its `tool_id` is the literal `plan`, its goal is the operator's own words, redacted — and records the plan plus one `pending` row per step *before* the first act. The parent's runner is recorded with the same helper the supervised runner uses, so reconciliation leaves a live parent alone and adopts a dead one. On restart, a plan with a step still in `running` moves to `needs_attention`; the step row keeps saying `running`, and `start_plan_step` refuses to reopen it.

**Context.** The lifecycle requirement is that the parent request, the validated plan, step states, tool runs and verification outcomes are persisted with redaction, and that uncertain side effects are never replayed. Phase 0 already had reconciliation for orphaned tasks — but `runner::run` reconciles at the start of every intent, which meant the *first step* of a plan would mark the plan's own live parent as an orphan.

**Rationale.**
- Writing the step row before the act is what makes a crash answerable at all: `pending` certainly did not happen, `running` may have, `succeeded` did. A row written afterwards cannot distinguish the first two.
- Ownership on the parent is the smallest change that makes Phase-0 reconciliation work *for* a plan instead of against it, and it gives the interrupted case its correct state for free: a dead parent already becomes `needs_attention`, and its `running` step is then precisely the thing the operator is being asked about.
- Refusing to reopen a step that is not `pending` puts the no-replay rule in the store rather than in the planner's memory, where a future code path could forget it. The `UNCERTAIN` line in `jean task` exists because that refusal is the honest answer: *its effect may already have happened, and JEAN will not run it again to find out*.
- `plans.superseded_at_ms` keeps a replacement plan beside the one it replaced instead of overwriting it, so "the model changed the plan" stays a fact in the record.
- `steps_json` stores the plan as validated and redacted — what was approved — and each step's `arguments_json` stores the vector that was actually going to be passed, not the provider's raw text.

**Consequences.**
- (+) `jean tasks` shows one line per operator request; `jean task <id>` shows the plan, the step states, the verdicts, the evidence, and which step is uncertain.
- (+) Schema v3 is additive: two new tables, no earlier column touched, and a Phase-1 database migrates in place.
- (−) A parent row exists even when planning fails, so a refused plan is visible in `jean tasks` with its refusal in the trail rather than vanishing. Deliberate: an unanswered request is a fact.
- (−) `tool_id = "plan"` overloads a column that otherwise names a registry tool. Documented at the write, and the status table already folds a name that repeats the intent.

**Alternatives rejected.** *No parent row; steps are ordinary tasks* — cheapest, and it loses the operator's request, the plan text and the step ordering, which is most of what the lifecycle asks to persist. *A `tasks.parent_id` column* — touches Phase-0 tables for a relationship the plan row already carries. *Re-run `running` steps on recovery* — the one thing that must not happen: a lost response is not proof that nothing happened. *Rewrite the step to `failed` during reconciliation* — turns an unknown into a claim.

---

## D-036 — Transport retries are bounded and live in the provider client; a tool run is never retried

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `model.retry_backoff_ms` (default `[500, 2000, 8000]`, at most four entries) drives retries **inside** the provider client, and only for errors that are safe to repeat: connect failure, timeout, write failure, HTTP 429 and 5xx. Other 4xx responses, an unreadable body, an oversized response and a cancel are never retried. Planning has its own separate bound — at most `MAX_PLANNING_ATTEMPTS = 3` *proposals*, each re-validated from scratch. Nothing in this build retries a tool.

**Context.** The brief asks for bounded retries for transient model transport errors and, in the same section, for never retrying a side-effecting tool merely because its response was lost. Those are two different failure classes, and conflating them is how an orchestrator ends up launching an application twice.

**Rationale.**
- A model request is idempotent from this build's point of view — the worst outcome is a second wasted answer — and the retry belongs beside the transport that knows which errors are transient. `ModelError::retryable()` is one match on the variant, and the tests assert the classification rather than trusting a comment.
- A lost tool response is not a failed tool: the child may have run, posted, written or deleted. `jean-core` cannot know, so the record says uncertain and reconciliation says `needs_attention` (D-035). Retrying would buy a second side effect with a guess.
- The two bounds are independent and each is stated where it applies: three provider *proposals*, each after transport retries, so the worst case is 3 × 4 requests — bounded, and visible in the plan row's `attempts` and the `model_responded` event.
- Re-asking with the previous refusal attached (`user_prompt(goal, rejected)`) makes a retry a *conversation* rather than a re-roll: the diagnostic codes it got are the instructions it receives.

**Consequences.**
- (+) A restarting Ollama costs one slow command, not a lost request.
- (+) `jean do` against an endpoint nothing listens on retried four times in eighteen seconds and then recorded a single failed parent — observed on the real binary, not only in a test.
- (−) A provider that fails forever costs three proposals before giving up, capped by configuration.
- (−) A tool whose response was lost is reported as uncertain rather than retried, and a person resolves it. That is the point, and it is in the report line.

**Alternatives rejected.** *Retry in the planner* — it would have to guess which errors were transport-shaped, and that guess is exactly jean-model's classification. *Retry a tool once on timeout* — forbidden by the brief and unsupported by the record. *Re-ask the model without saying why* — wastes the diagnosis and usually reproduces the same fault. *An unbounded planning loop* — the difference between a slow command and a hung one.

---

## D-037 — Summaries come from the execution record; model wording is opt-in; a credential is an env-var name, scrubbed everywhere it could echo

**Status:** ACCEPTED · **Date:** 2026-11-05

**Decision.** `PlanReport.summary` is built from the step rows (`"succeeded: 2 of 3 step(s) proven, 1 failed"`) and the exit status is derived from those same states. `model.summary_wording = "model"` asks the provider to *phrase* those recorded facts, and any failure or refusal of that call falls back to the deterministic sentence. `model.api_key_env` names an environment variable; the value is read at request time, never stored, never printed, and scrubbed from provider error text and from every redacted field on its way into the database.

**Context.** The reporting requirement is that summaries come from actual records and that model wording can never override a recorded failure; the interface requirement is that credentials come from a documented secret source and are never logged. The validation requirement is a test for redaction of credentials and sensitive request fields.

**Rationale.**
- A model that writes the summary can write *"all steps completed successfully"* about a plan whose second step failed. Keeping the facts in code and offering only the prose to the model makes that impossible to express: `wording()` decorates a `PlanReport` that has already computed its own `exit_status()`, and it is opt-in because a plan is a decision, not a composition.
- The wording request goes through the same client, the same bounds, and the same *no authority* instruction ("You have no authority over this task"). Its answer is stored as text either way.
- Three leak paths had to be closed rather than one: the request (the prompt never holds the key; the bearer header is built per attempt and never echoed), the response (provider error bodies are scrubbed before becoming `ModelError`, because some misconfigured servers quote the `Authorization` header back), and our own records (every stored field passes the `Redactor` — goal, titles, argument vectors, evidence, and, the one this batch found, the `plan_proposed` audit line, which quoted the model's restatement of the operator's goal verbatim and put a `password=…` into the events table).
- Redaction happens before shortening. Truncating first can cut the middle out of a pattern and leave a fragment that matches nothing.

**Consequences.**
- (+) `jean model status` prints "credential environment variable X is set / is not set" and no value; `jean do` prints the same line before asking.
- (+) The redaction test asserts the parent goal, `plans.steps_json`, `plan_steps.arguments_json`, `tool_runs.args_json` and every event detail on the parent, so the guarantee is not one place that happens to be covered.
- (−) `summary_wording = "model"` costs a second request per run and returns prose an operator must read as prose; it is labelled in the code path that prints it.
- (−) Scrubbing is pattern-based (`password=`, `token=`, `api_key=`, `sk-…`), so a secret in an unexpected shape can still reach a record. The patterns are configuration (`core.redact_patterns`) and the record keeps the key name so a person can see that something was removed.

**Alternatives rejected.** *Let the model write the summary by default* — reads better, and makes a recorded failure deniable. *A second model with more authority over the wording* — the same problem with an extra hop. *Hash or drop secrets instead of `[REDACTED]`* — a hash of a short secret is guessable, and dropping the key name leaves a record nobody can diagnose. *The OS credential store* — `04-SETTINGS.md` mentions it; it needs a credential API and a migration story this batch does not have, so the environment variable is the documented secret source and only its name appears in configuration.

---


---

## D-038 — The model transport is `ureq` with the Windows trust store: verified HTTPS, chunked bodies, bounded streaming, redirects not followed

**Status:** ACCEPTED (supersedes **D-029**) · **Date:** 2026-11-05

**Decision.** `jean-model` stops speaking HTTP itself. The transport is `ureq` 3.5.1 with
`default-features = false, features = ["native-tls"]` — a blocking client, so no async runtime enters
the workspace — configured in one function so its security-relevant settings are asserted rather than
assumed: TLS through Schannel against the platform's roots, certificate and hostname verification with
no way to disable it, `max_redirects(0)`, a global deadline equal to `model.request_timeout_secs` plus
separate connect and resolve bounds, a header-size ceiling, and the response body read through
`Read::take(model.max_response_bytes + 1)`. A non-loopback `http` endpoint becomes a configuration
error; `https` to any host becomes supported.

**Context.** D-029 chose a hand-written client and documented no-TLS and no-chunked as limitations.
The operator rejected that: a maintained transport was required, along with the six properties named
above, and a decision was explicitly not a reason to leave a requirement unimplemented. Re-reading the
endpoint rules once a real TLS client existed found a live defect: the build refused `https` while
accepting `http://remote`, so the only reachable remote shape was one that would have carried the
prompt and the bearer token in the clear (C-1).

**Rationale.**

1. **The parser is not the policy.** What JEAN must own is the ceiling, the deadline, the cancel flag
   and the refusal to follow a redirect; what it should not own is HTTP/1.1 framing, chunked decoding
   and TLS record handling. Those moved to `ureq`; the four policy items stayed here and each has a
   test that would fail if someone moved them.
2. **Blocking, because the supervisor is blocking.** DV-02 keeps the run synchronous and
   `jean-process` owns the threads. A client that needs a runtime would put tokio, hyper and h2 in the
   tree for one POST per planning attempt. Verified in the resolved lockfile: none of `tokio`, `hyper`,
   `h2`, `rustls` or `ring` is present; 156 packages total.
3. **`native-tls` (Schannel) rather than rustls.** It uses the operating system's trust store and its
   verifier, adds no C-built crypto to compile, and on this platform is the path that makes "verified
   against the machine the operator actually runs on" literally true — including an enterprise CA the
   machine already trusts. `RootCerts::Specific` (pinning) was not chosen: a pinned set would silently
   break on a rotated certificate, and pinning is a feature this build has no use case for.
4. **Cancellation through the pool, not by polling a socket.** `Agent` is cheap to clone and
   `Agent::shutdown()` closes every connection the agent holds, including one parked in a read on
   another thread. So the exchange runs on a worker and the watching thread polls the operator's flag;
   when it fires, shutdown makes the blocked read return immediately instead of at the deadline.
   `a_stalled_answer_is_cancelled_mid_flight` measures this against a provider that accepts the request
   and never answers.
5. **The honest ceiling on that**: shutdown does not interrupt name resolution or an in-progress TCP
   connect. Those are bounded by `timeout_resolve`/`timeout_connect`, and a connection that completes
   after shutdown fails as a request on a shut pool rather than producing a late answer. Documented in
   `jean-model`'s own module comment, and asserted in the config test.
6. **Redirects are refused twice over.** `max_redirects(0)` means the client never asks, and a 3xx
   response is still turned into `ModelError::Redirect` with the address it declined to visit.
   `ureq`'s own default is 10, so this is a setting, not an accident — and its separate default of never
   re-attaching `Authorization` across a redirect is what keeps a future opt-in from becoming a
   credential leak.
7. **Streaming became real.** Both adapters have always reassembled SSE and NDJSON, and both formats
   arrive chunked, so `model.stream = true` was unreachable code until this decision. The shipped
   default stays `false`: one bounded body is the easier shape to prove.

**Consequences.**

- (+) Verified HTTPS, chunked responses, and a maintained parser for the framing an adversary controls.
- (+) The C-1 hole is closed, and the closure is a load error with a sentence explaining it.
- (+) The security settings are pinned by `certificate_verification_is_never_switched_off_and_redirects_stay_unfollowed`,
  which asserts `disable_verification() == false`, the provider, the root source, `max_redirects == 0`
  and the timeout fields. A future edit reaching for an accept-any verifier fails there first.
- (−) One third-party crate with a C-free TLS path is now in the audit surface, and `cargo deny` has
  still never been run on this machine (D-022 stays open). The supply-chain argument that justified
  D-029 is real and is not dismissed here; it was outweighed, on an explicit instruction.
- (−) Roughly 2.3 GB of additional build artifacts on a volume that had 2.8 GB free. This is now the
  binding operational constraint on the machine; recorded in `IMPLEMENTATION_STATUS.md` §29.
- (−) We no longer read every byte of the protocol state machine ourselves. Mitigated by the config
  assertions, by a stub server that speaks chunked/redirect/stall shapes, and by the fact that the
  bounds are applied *after* the client returns, on our own read.
- (−) `jean-model` still has no proxy or HTTP/2 support, and plain `http` remains loopback-only.

**Alternatives rejected.** *`reqwest`* — the most-used client, and it drags an async runtime into a
synchronous supervisor for one request per attempt. *`hyper` directly* — a runtime question again, and
more assembly than the job needs. *`WinHTTP` through the `windows` crate* — no new crate lineage, but
it would make `jean-model` Windows-only and hand HTTP framing back to us in a different dialect.
*Keep the hand-written client and add rustls* — a maintained crypto library behind an unmaintained
parser; the operator asked for the parser to stop being ours. *`native-tls` with `vendored`* — builds
OpenSSL from source, which this machine cannot afford in time or space. *`ring`-backed rustls* —
compiles C and was unnecessary once Schannel was available; it also would have made the trust store a
vendored list rather than the machine's own.

---

## D-039 — A person typing `jean do` authorises the run, bound to the resolved plan scope; it expires with the run and never enters the approval ledger

**Status:** ACCEPTED (amends the per-step reading of **D-028** and **D-033**) · **Date:** 2026-11-05

**Decision.** An operator-initiated run carries a **grant**: the set of steps in the plan that was
validated from that operator's own goal, each bound to its `plan_id`, its step key and its *resolved*
scope digest. A step in that set is decided without asking. `auto_deny` and the elevation boundary are
consulted **before** the grant exists; the remembered-approval ledger and the prompt come **after** it
fails to match. The grant is a value owned by the run: it is dropped when the run returns, it is never
serialised, and no code path writes it to `approvals`. `jean do --prompt-each` mints nothing, which
keeps Phase 1's per-step gate reachable as an operator choice rather than as the default.

**Context.** The Phase-2 batch connected each plan step to the existing intent pipeline and stopped
there, so a three-step plan whose steps each touched a capability on `require_approval` produced three
prompts about the one thing the operator had asked for once. That was recorded as a limitation with the
reason attached. The brief had said the operator's original instruction authorises routine in-scope
steps and that repeated prompts for them should be avoided, and the limitation was rejected as an
unimplemented requirement.

**Rationale.**

1. **The source of the authority is a person, not a string in the plan.** A model's proposal, a tool's
   output and a remembered approval are all *inputs*; none of them is a grant (D-011 stands). The only
   door is `RunAuthority::OperatorRun`, which `jean do` passes because a human typed the goal into it.
   `--yes` remains a no-op — a flag is neither a person nor a configuration decision.
2. **Bound to the resolved scope, computed by the executing code.** Entries are minted from
   `intent::operation_for(...)`, which is `prepare()` — resolution included — the same call the step
   itself makes. A grant minted from what the model *declared* would authorise a different operation
   than the one that runs: `launch_app` resolves a name to a particular executable, `locate_file`
   resolves a root, and the digest covers kind, tool id, target and argument vector.
3. **A mismatch is not a weaker yes; it is no answer at all.** Different plan, different step key, or a
   resolved digest that differs from the minted one, and the step falls through to the ledger and then
   to a person. That is the fail-closed direction, and it is what makes a mid-run replan or a drifting
   machine state prompt rather than proceed.
4. **Expiry is a lifetime, not a timer.** No clock, no TTL to reason about, no row to sweep: the run
   owns the value, so the next invocation — same process or not — has nothing to reuse. `approvals`
   stays empty in every granted scenario, asserted rather than intended.
5. **Only steps that would have asked are entered.** A covered step is by construction a step whose
   capability is on `require_approval` and that is neither auto-denied nor elevated, so the number the
   operator is shown means "these are the prompts you will not see" instead of counting steps that never
   needed one.
6. **Every decision says which authority answered it.** `Authority::RunScope` writes `run_scope` into
   the audit trail beside `owner_command`, `approved_this_time` and `remembered_approval`. After
   something goes wrong, "was this run because a person asked, because they said yes to that step, or
   because a year-old approval is still standing?" has an answer, and "the model asked" is never one of
   them.
7. **The boundary that this does not move.** Deterministic validation can prove a plan is inside the
   vocabulary this build can execute; it cannot prove that any sentence a person typed authorises any
   command a model generated. Only registered, enabled, policy-passing operations execute, and a
   materially ambiguous target or action is still refused or asked about.

**Consequences.**

- (+) A three-gated-step plan prompts zero times and still records three decisions, three tool runs and
  three verifications; the same plan under `--prompt-each` prompts three times. Both are tested, with a
  *denying* prompter in the granted case so that a leak of authority surfaces as a refusal.
- (+) Explicit deny rules and the privilege boundary are untouched: a capability on both lists is still
  refused with no prompt, and an elevated step is still `elevation_required`, grant in hand.
- (+) Nothing persists, so the reuse vector that remembered approvals carry (D-027) does not exist for
  a run grant, by construction rather than by cleanup.
- (−) A plan the operator skimmed is covered by their own instruction. Mitigated — not eliminated — by
  the plan being displayed before anything runs, by validation refusing anything outside the closed
  vocabulary, and by `--prompt-each` for anyone who wants every step in front of them.
- (−) `RunAuthority` is a parameter, so a future caller could pass `OperatorRun` when no person typed
  anything. The mitigation is that the CLI's typed entry is the only such caller today, that a delegated
  path has a reason to say so, and that the enum's meaning is written where it is constructed.
- (−) Cancellation mid-run leaves the grant dropped, which is intended, and means a resumed run is a
  new run needing a new operator.

**Alternatives rejected.** *Per-step prompts as the default* — what shipped, and rejected: asking about
each step of the thing a person just asked for is friction, and friction trains people to answer yes
without reading, which is the reason D-011 exists. *Widen `require_approval` remembered approvals to
cover the run* — a remembered approval outlives the run by design (D-027), which is exactly the reuse
this decision forbids. *`--yes` as the grant* — a flag is not a person, and it would make the authority
dependent on a token in a shell history line. *Approving the plan as a whole, from the model's own
summary* — model output cannot grant (D-011), and the operator would be approving a description rather
than a resolved scope. *A `policy.run_grant = true` configuration key* — a file would then answer for a
person, and the question "did anyone consent to this run" would be answered by a setting instead of by
the code path that read a human's words. *Signing or hashing the plan and approving the hash* — adds a
second concept of scope to keep consistent with the digest that already binds operation, tool, target
and arguments.

---

## D-040 — Certificate verification is proven by an isolated anchor set, never by a flag: `native-tls` is adopted test-only

**Status:** ACCEPTED (extends **D-038**) · **Date:** 2026-11-05

**Decision.** The claim "`https` means verified TLS" is demonstrated by handshake, not by inspection.
`crates/jean-model/src/tls.rs` stands up a real TLS server on loopback and points the production client
at it with one parameter that replaces the root set with a single throwaway CA — so a trusted chain with
the right name is **accepted**, and both a chain that does not reach that CA and a chain that does but
names a different host are **rejected**. `native-tls` (already in the tree through `ureq`, same version,
Schannel path) becomes a **dev-dependency** of `jean-model` and nothing else; the shipped dependency set
is unchanged. No trust store on the machine is read, written or mutated.

**Context.** D-038 shipped a maintained client with verification on, and the suite proved it only
*negatively*: an `https` endpoint was attempted, a plaintext listener was refused by the TLS layer, and
`disable_verification` was `false` in the configuration being built. IMPLEMENTATION_STATUS.md §29 said
exactly that, in those words. The preparation brief ruled that shape out in advance — a test that checks
configuration flags, or that refuses a plaintext server, does not demonstrate certificate validation.
What was missing was a positive result and two named negatives, from a chain nobody on this machine
trusts.

**Rationale.**

1. **A flag being off is not the same as a check working.** The settings under test are reachable
   through several paths inside `ureq` and `native-tls`; only a handshake says which one ran.
2. **An isolated root set is narrower than the platform's, not weaker.** `RootCerts::Specific` makes the
   pinned CA the *only* authority the connection may build to, with verification still on. The
   substitution therefore cannot grant anything a shipped build does not already have — it can only take
   trust away, which is exactly what a rejection test needs.
3. **The accepted rows are what make the rejected rows evidence.** While the harness was broken by two
   separate defects (see Consequences, and IMPLEMENTATION_STATUS.md §32.3: `Identity::from_pkcs8`
   produces a credential Schannel refuses, and a stub that leaves the request body unread closes the
   socket with an RST that reaches the client as a connection failure *after* a successful handshake)
   the rejection tests passed. They were proving nothing. A handshake that succeeds under the same
   harness with one input changed is the control that makes a rejection mean "the certificate".
4. **Asserting Schannel's own sentence costs nothing and forgives nothing.** The rejection tests require
   `"not trusted"` and `"does not match"` in the error text. Both are Windows' words for the two distinct
   checks, and neither appears in a socket reset — so the class of failure that hid the first two defects
   cannot hide a third.
5. **Zero new runtime surface.** The crate is already compiled for the client side; adding it as a
   dev-dependency changes one edge in `Cargo.lock` and pulls nothing new. A test that needed a *second*
   TLS stack would have been the wrong trade.

**Consequences.**

- (+) The Phase-2 exit criterion "verified HTTPS" is demonstrated in both directions on this machine,
  deterministically, offline, in about eight seconds.
- (+) The certificates and the reason each exists are generated by one committed script that warns they
  are throwaway; a fresh tree runs it and gets the same five results.
- (+) `describe()` now attaches the "this is a verified TLS connection" sentence to an `https` failure on
  loopback as well, so an operator who points `https` at a plain-http Ollama gets the explanation rather
  than a bare socket error.
- (−) One `Option<&[u8]>` parameter exists in the transport whose only caller with `Some` is
  `#[cfg(test)]`. It is compiled out of a shipped build, and the test asserting that both branches leave
  `disable_verification` off is the guard against anyone adding a second caller.
- (−) The fixtures must be generated before `cargo test -p jean-model` on a fresh tree; a missing file
  panics with the exact command rather than failing as a compile error that would take the rest of the
  crate's tests with it.
- (−) The fixture directory contains private keys — of certificates no store trusts, in a directory
  whose header says they must never be pointed at by a real configuration. That is still keys on disk.
- (−) `generate.sh` emits PKCS#12 bundles that exist only because `from_pkcs8` is broken on Windows. The
  workaround is commented where it lives, so nobody has to rediscover it.

**Alternatives rejected.** *Assert the configuration flags and stop* — what D-038 shipped, and the thing
the brief named insufficient; it cannot distinguish "verification on" from "verification on and never
reached". *Validate against a live `https` provider* — needs an authorised endpoint, a network, and a
certificate nobody controls, and it produces neither an untrusted chain nor a wrong name without
attacking someone's DNS. *Import the test CA into the Windows user or root store* — mutates machine
state, needs elevation for the root store, and leaves a trusted third party behind after the test ends,
which is the opposite of isolated. *Add `rustls` as a dev-dependency because custom roots are easier
there* — a second TLS stack and a C-built crypto provider in the tree, contradicting the reason D-038
chose `native-tls`, in order to make a test easier to write. *A test-only "skip verification" hook* —
the one thing these tests exist to prove unavailable; a build that can be told not to verify cannot be
shown to verify. *Hand-roll a TLS server over `std::net`* — the argument for adopting `ureq` was that a
transport is a parser an adversary writes; putting one in a test would contradict the crate's own doc
comment.

---


---

## D-041 — ACP and MCP are spoken by their maintained SDKs, on a runtime that stays inside two crates

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** Adopt `agent-client-protocol = "=5.0.0"` and `rmcp = "=3.8.0"` as the only code that
implements either protocol, and let each of `jean-acp` and `jean-mcp` own a **current-thread tokio
runtime** that exists for the length of one session. J.E.A.N. still spawns every process, bounds every
frame, decides every request and kills every child.

**Context.** Phases 0–2 are synchronous, and that was a preference, not a requirement. The Phase-3
brief settled the question in advance: *"If an SDK requires an async runtime, adopt the necessary
runtime deliberately. Earlier preference for synchronous code is not a reason to hand-write a complex
protocol or abandon supported SDKs."* Both SDKs need an async context; ACP's entry point takes an
`AsyncFnOnce` and has no synchronous form.

**Rationale.**

1. A protocol with a conformance suite and a maintainer is not ours to reimplement. Hand-written ACP
   framing would have been a second, worse parser written by the party with the least information about
   the spec.
2. The crate boundary is where the runtime stops: `tokio` is a dependency of `jean-acp` and `jean-mcp`
   and of **no other crate**, so `jean-core`, `jean-policy`, `jean-process` and the CLI gain no futures,
   no `async` in their signatures, and no scheduler to reason about.
3. Current-thread, `block_on`, one session per call — the protocol task lives and dies inside one
   function. There is no thread pool, and nothing outlives the command.
4. `blocking::Unblock` keeps a parked pipe read off the runtime's single worker, so a slow peer cannot
   idle the timer that is supposed to bound it.

**Consequences.**

- (+) `Cargo.lock` goes from 68 to **276 packages** — counted from the lock, correcting the 119 first
  written here — and the first `tokio` enters the tree (locked `1.54.1`), deliberately and in two named
  places: `jean-acp` and `jean-mcp`, and no other crate declares it.
- (+) `rmcp`'s `transport-child-process` feature is **excluded**, so the SDK cannot spawn anything: JEAN
  hands it the pipes `jean-process` made. The same rule holds for ACP's `AcpAgent` spawner type, which
  this code never constructs.
- (−) Two crates must now be reasoned about as async, and §38 shows the cost is real: a session that
  does not settle is harder to see in an async loop than in a blocking read.
- (−) The dependency audit (§33) that has never run now covers a much larger tree.

**Alternatives rejected.** *Hand-write ACP/MCP framing* — rejected by the brief and by the fact that a
malformed-frame case we cannot enumerate is exactly what the SDKs already handle. *Adopt the SDKs but
let them spawn the peer* — puts process creation outside the job object, outside the environment
allowlist and outside the policy decision. *A multi-thread runtime* — a pool of workers for one
conversation, and a much harder story about what outlives the command. *Stay synchronous and drop the
phase* — the phase is the product (M2).

---

## D-042 — One delegated worker at a time, and the tool says so

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** `[delegation] max_parallel` is validated to `1`, and `jean agent` prints `parallel 1 —
this build runs one worker at a time, because cancellation has one active task per process`. No
command, doc or report claims concurrent delegation.

**Context.** The brief made this an explicit fork: *"Before concurrent workers, resolve the current
single-active-task cancellation limitation. Otherwise ship sequential workers explicitly; do not claim
parallel support."* The limitation is structural — `jean-core::ipc` binds one cancellation pipe per
process and D-028 installs one signal handler, so a second in-flight task has nowhere to be cancelled
from.

**Rationale.**

1. Two workers with one cancel channel is not parallelism, it is a task that cannot be stopped.
2. A refusal costs the user nothing; a false capability costs them the run they thought they had.
3. The router that would fix this (per-task pipe, or one pipe multiplexing task ids) is a design of its
   own and belongs to a later phase, not to the middle of this one.

**Consequences.**

- (+) `jean delegate` is honest in its own output, and the config cannot be set to a value this build
  cannot honour.
- (−) A batch of independent tasks is serialised. Acceptable: the objective workflow is one goal, one
  agent, one verification.

**Alternatives rejected.** *Accept `max_parallel = 3` and run three* — the roadmap number, and the one
thing the brief forbade without the cancellation work. *Accept it and queue silently* — a config value
that changes nothing. *Refuse the option entirely* — hides the intent; validating to `1` keeps the knob
and names the reason.

---

## D-043 — A delegation is one authority, and an agent cannot widen it

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** The operator's approval of a delegation grants a **bounded** authority: a file action
inside the workspace the worker was pointed at, under the capability the worker was configured with, is
answered without asking again (`Authority::DelegationScope`). Everything else reaches the ordinary
engine. Two things are refused **before** the engine, and therefore cannot be approved by any prompt: an
ask whose target resolves outside that workspace (`DenyReason::OutsideScope`), and a write from a
worker whose configured `sandbox` is `read-only`. An agent is only ever answered with an option from its
own `options` list, preferring `allow_once`, and an answer naming an option it did not offer becomes a
rejection.

**Context.** ACP delivers `session/request_permission` mid-turn, with the agent's own offer list. The
brief required both halves: *"Covered actions must not produce repeated prompts. An agent cannot expand
its own authority."* Answering every edit in a 40-file task would train the operator to press `y`
without reading — which is how an approval engine becomes a rubber stamp.

**Rationale.**

1. The workspace and the capability are the two things the operator actually chose, and both are already
   in the approval they just gave.
2. A path outside the workspace does not depend on the operator's mood, so asking would be dishonest: a
   prompt implies that a `yes` exists.
3. The sandbox is configuration. If a mid-session ask could unlock a write, `read-only` would be a
   suggestion, and D-011's whole point is that the requesting side cannot negotiate the rules.
4. Ordering matters: scope and sandbox are checked **before** `Policy::decide`, so no remembered
   approval and no prompt can reach them.
5. Taking the option id from the agent's own list is what keeps the answer protocol-valid; the
   alternative — inventing an id — is a session error the agent may treat as approval.

**Consequences.**

- (+) A covered session asks zero questions; the delegation row records `covered_asks` and
  `prompted_asks` in separate columns, so "no prompts happened" stays distinguishable from "no prompts
  were needed".
- (+) Both escapes are auditable by name: `delegation_scope` and `outside_scope` appear in the audit
  trail, and the ask is a task event either way.
- (−) `Authority::DelegationScope` is a fourth way an operation can be allowed, and it has to be
  reasoned about wherever `jean approvals` is discussed.
- (−) A `read-only` worker cannot do anything that needs a new file, which is a narrow use.

**Alternatives rejected.** *Prompt for every file action* — the brief rules it out, and a human cannot
read 40 identical prompts. *Trust the agent's own `allow_always`* — that is the agent choosing its
future authority. *Make the boundary configurable* — an escape hatch for "the workspace" is not a
boundary; `OutsideScope` is deliberately reachable by no configuration at all.

---

## D-044 — A child process is started in a plain path, not in `canonicalize`'s verbatim one

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** Before a workspace path becomes a `LaunchSpec` working directory, the `\\?\` verbatim
prefix that `std::fs::canonicalize` returns on Windows is removed for drive paths; a real UNC root keeps
its `\\host\share` spelling. Containment checks still canonicalize both sides internally.

**Context.** The first live delegation (IMPLEMENTATION_STATUS §37) left this in its own artifact file:

```text
'\\?\C:\Users\…\work'
CMD.EXE was started with the above path as the current directory.
UNC paths are not supported.  Defaulting to Windows directory.
```

The baseline verification — the check that gates every "proven" — had run in `C:\Windows`.

**Rationale.**

1. Exactness is not the same property as usability: a verbatim path is precise and unusable-by-others at
   the same time.
2. The failure is silent. Nothing crashed, the exit code came from the wrong tree, and no scripted test
   could observe it, because no scripted test starts a child there.
3. `git` given the same prefix takes minutes where it should take milliseconds, which is how a
   90-second budget starts to look like a hang.

**Consequences.**

- (+) Worker and checker start where the operator said; the delegation test now asserts the plain form
  and that no `\\?\` survives.
- (−) Two spellings of one path exist in the code, and the conversion is a one-line function that has to
  stay at the boundary rather than leak into comparisons.

**Alternatives rejected.** *Stop canonicalizing and pass the raw path* — loses the symlink resolution
that makes the containment test mean something. *Add a `cd` to the checker's argv* — fixes the observed
symptom for one program and leaves the next one broken. *Assert in tests that the checker's stderr was
empty* — a detector, not a fix, and the scripted suites never had a stderr file to read.

---

## D-045 — MCP is stdio in both directions, the exposed list is also the callable list, and a server with no terminal refuses

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** J.E.A.N. speaks MCP only over standard input and output it already owns — as a client to a
server it starts (`jean mcp tools|call`), and as a server to the client already attached to its own
pipes (`jean mcp serve`). No listener, no port, no session that outlives the command. In both roles the
operator's list is a boundary enforced by refusing rather than by omitting: `[[mcp.servers]]
allow_tools` gates what J.E.A.N. will call, `[mcp] expose` gates what it will publish, and the router
rejects a name that is not on the published list before the host is reached. In the server role, a call
that needs a person is **refused** — its question is written to stderr and its refusal names what would
have made it allowed — because stdin and stdout are the transport and a prompt cannot be answered there.

**Context.** The brief: *"Exposed JEAN tools must pass through the existing policy pipeline. Use local
stdio initially; do not open a network listener."* The pipeline part was satisfied by routing every
inbound call through `jean_core::intent::execute` — the same function `jean run` uses — rather than a
second executor. The prompt part is where the two halves collide: the approval engine's third step is a
human, and a server process has none.

**Rationale.**

1. stdio has no attack surface to authenticate against; a listener on a shared machine is exactly the
   thing an orchestrator should not open by default.
2. Publishing a tool is a promise about what can run, so the list is built from the registry and
   refusal to publish an unknown or disabled name happens **before** the transport exists — a client can
   never be offered a tool that resolves to nothing.
3. Silence is not consent. A server that cannot ask must not guess, and `Answer::Denied(Unanswerable)`
   is the engine's existing vocabulary for "nobody said yes".
4. The transport thread is bridged to the main thread by a channel instead of sharing state, because
   `Context` owns one SQLite connection and `rmcp`'s handler must be `Send + Sync`. That turns the
   constraint into a property: a protocol peer has no code path to the database, the policy engine or a
   process except the one the operator's main thread runs.
5. Diagnostics go to stderr in both roles, so a log line can never be mistaken for a frame — and the
   e2e test asserts stdout carried nothing but frames.

**Consequences.**

- (+) One pipeline, two doors: an MCP call leaves the same task row, run row, audit lines and
  verification as the typed command that leaves the same result.
- (+) `[mcp] max_message_bytes` is applied to the wire (the SDK's 16 MiB default would have made the
  setting decorative), and a 9 KiB frame against a 2 KiB ceiling is refused rather than buffered.
- (−) `jean mcp serve` cannot approve anything interactively, so its usable set is "capabilities not on
  `require_approval`" plus "exactly what `jean approvals` remembers". Documented in the refusal text.
- (−) Argument values enter the approval digest, so a remembered approval never covers "the same tool
  with different arguments" — deliberate, and it means an agent asking twice for two paths is asked
  twice.

**Alternatives rejected.** *A localhost TCP listener, "just for now"* — an unauthenticated local socket
is still a door, and stdio needs none of it. *Refuse to serve at all until there is a tray UI* — the
roadmap asks for the server role in this phase, and refusing-by-prompt is a working, honest answer.
*Let the transport thread own a second `Store`* — two connections to one database inside one process for
the sake of avoiding a channel. *Feed the operator's terminal from the transport thread* — reads stdin
for a `y` that is actually a peer's frame.

---

## D-046 — The browser is the Chrome already installed, started and owned by J.E.A.N. over loopback CDP

**Status:** ACCEPTED — **implemented, and demonstrated against the installed Chrome on 2026-11-06**
(`IMPLEMENTATION_STATUS.md` §45; the checks behind it in §46) ·
**Date:** 2026-11-06

**Decision.** The Phase-3 browser step drives
`C:\Program Files\Google\Chrome\Application\chrome.exe` (156.0.8078.81, present on this machine) as a
**child J.E.A.N. starts, owns and closes**, with its own temporary profile and a loopback-only CDP
endpoint, using `headless_chrome` — already declared at `1.0.22` in `Cargo.toml`, 100 % synchronous, so
the browser path adds no third runtime. `[browser] allowed_origins` is validated to `file:` plus
loopback `http(s)` before any navigation, and the websocket URL is checked for a loopback host before
it is connected to.

**Context.** The brief requires the choice to be settled *before* implementation, weighing an
already-installed browser, external runtime requirements, reliability and cost. Chrome, Edge and Brave
are all installed and were all inspected — none launched, none touched a profile, nothing installed.
The workflow the phase owes is one loop: navigate → inspect content → interact → verify, against a local
fixture J.E.A.N. writes into the task's artifact directory.

**Rationale.**

1. Installed beats ideal: the operator already runs this binary, so "will it start" is answered by the
   machine rather than by a README.
2. `headless_chrome` is a synchronous CDP client, which keeps the browser off the async runtime D-041
   confined to two crates, and it is already in the dependency plan (D-022).
3. Owning the process is what makes the safety properties real: a browser J.E.A.N. started has a
   disposable profile, a job object around its renderer children, and a remote-debugging endpoint nobody
   else enabled.
4. The crate recovers that endpoint by scraping the child's stderr and performs **no loopback
   validation** — so the check has to live in J.E.A.N., at the boundary, before `Browser::connect`.

**Consequences.**

- (+) A decision recorded with its alternatives, which is what the brief asked for before code — and then
  the code, on the same day: `crates/jean-browser`, `crates/jean-core/src/browser.rs`, `jean browser
  status|run`, four demonstrated paths, and 0 JEAN-owned browser processes left afterwards against the
  operator's own still-running Chrome.
- (−) ~~**Nothing is usable yet.** `[browser]` parses and validates, `BrowserAction` exists as an event
  kind, and no module reads either.~~ **Corrected 2026-11-06:** both are now read — `[browser]` gates the
  run (enabled, origin, executable, timeouts, cap) and `BrowserAction` is emitted for every step. What the
  sentence got right and time did not erase: `headless_chrome` arrived by `cargo fetch` only when the volume
  had room for it, and its 61 new lock entries have still never been licence-audited or sized (§33 —
  `cargo deny check` remains unrunnable, which is a debt this decision inherits rather than one it creates).
- (−) CDP-over-Chrome is a Chrome-shaped answer; a different engine later means revisiting this. Detection
  offers Edge and Brave paths, and only the Chrome one has been run.

**Alternatives rejected.** *Playwright, or `@playwright/mcp`* — both require installing npm packages and
browser builds, which this batch may not do; an MCP browser server would also put a second process
between J.E.A.N.'s policy engine and the act, weakening D-011. *`chrome --dump-dom`* — one-shot and
non-interactive, so "perform an interaction" cannot be expressed at all. *Attach to the browser the
operator is already running* — that profile holds their sessions, and enabling remote debugging on it is
a machine-visible change to somebody's everyday tool; refused in `BrowserConfig`'s own documentation.
*Skip the browser until the disk is fixed* — rejected as a *decision*: the choice still had to be made
and recorded, only the code could not be written.

---

## D-047 — A live protocol peer in a test is a script J.E.A.N. writes and starts, not an agent it authenticates to

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** Where a Phase-3 test needs the other end of a real pipe pair, the fixture is a small
Node script written into the test's own temporary data directory and started by J.E.A.N. as the child it
would start in production (`node` is installed; no npm package is fetched). Determinism comes from the
script; the transport, the spawning, the bounds, the policy decision and the records are all production
code. Every such test reports out loud when the machine cannot run it.

**Context.** §5 of the brief asks for deterministic fake ACP/MCP/CLI workers *and* for real checks,
while forbidding installs, authentication and paid-service activity. A vendor agent (opencode, codex)
cannot be driven in CI without the operator's account and quota, and mocking the protocol on both ends
proves only that two mocks agree with each other.

**Rationale.**

1. It is the only shape available that exercises the actual wire: the `rmcp` handshake, newline
   framing, line ceilings, result caps, job teardown — 6 MCP scenarios pass against a live peer this
   way, and the ACP fixture reached a performed write before the connector's settlement defect (§38)
   stopped it.
2. Machine dependence is reported, not hidden: these tests `eprintln!` that `node` was missing and
   return, instead of pretending the round trip happened.
3. The fixture is ~30 lines in the test file, under version control with the assertions it satisfies, so
   a protocol mistake shows up as a failing test rather than as a silent gap.
4. It keeps the *decision* on J.E.A.N.'s side: the script asks, J.E.A.N. answers, and an answer nobody
   gave is not in the tree.

**Consequences.**

- (+) Mock-versus-live evidence is now separable in IMPLEMENTATION_STATUS: scripted peers for the
  accounting, live peers for the protocol, and the vendor agents recorded as *unverified*.
- (+) The first live run found a real defect (D-044) that 21 scripted tests could not have seen.
- (−) The suite is partly dependent on `node`/`git` being present, so coverage differs by machine — and
  says so.
- (−) A hand-written agent is only as faithful as the protocol subset it implements; it will never
  discover a vendor's odd `session/update` shape.

**Alternatives rejected.** *Run a real coding agent* — needs an authenticated account and spends the
operator's quota, both ruled out by the brief. *Assert on the connector's parsing only* — what the
scripted suites do, and §38 shows it can pass while the session never settles. *Skip live protocol
tests entirely* — leaves "the ACP connector works" unsupported either way, and hides the finding.

---

## D-048 — One maintained UIA wrapper, four replaceable boundaries, and the raw-COM fallback left unwritten

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** Adopt `uiautomation` as the *only* UIA provider, confined to one module behind a
five-question trait, and add three more replaceable boundaries beside it — capture, input, and text
extraction — so that every part of Phase 4 that could be wrong is wrong in one place. Do **not** write the
raw-COM path D-017 asked to keep "partially implemented from day one", and record that as an unexercised
escape hatch rather than as safety.

**Context.** Phase 4 had to deliver native desktop operation against real applications on a machine whose
providers are not under J.E.A.N.'s supervision, with the brief's explicit instruction to *"use maintained
Windows/UIA libraries after checking their actual APIs and licenses"* and *"do not write custom COM
infrastructure merely to avoid a suitable dependency"*. D-017 had already chosen the wrapper in the
abstract; what changed in this batch is that the choice met a compiler.

**Rationale.**

1. **The wrapper was checked, not assumed.** `=0.25.1`, licence read from the registry manifest for both
   halves (`uiautomation` **and** `uiautomation_derive 0.7.9` → `Apache-2.0`), built on `windows ^0.62.2` —
   *exactly* the version this workspace already pins, which is why adopting it adds no second copy of
   windows-rs (the `0.61.3` in the lock belongs to `win32job`, measured with `cargo tree --invert`).
2. **One feature had to be enabled for a reason that is not ours.** `default-features = false` with
   `["control", "pattern", "input"]`: 0.25.1's `core.rs:38` imports `crate::inputs::MouseButton`
   unconditionally while `lib.rs:13` gates that module behind `feature = "input"`, so the crate **does not
   compile** without it. This is a measurement, not an inference — the verbatim compiler output
   ("unresolved import `crate::inputs`", "found an item that was configured out") is retained in
   `target/p4-fixture-build.log`. J.E.A.N. calls none of the crate's input API; the cost is one `windows`
   feature this workspace already requests for `SendInput`.
3. **A trait with four questions was not enough, and the fifth is the phase's most useful artefact.** The
   boundary is `accessibility::AccessibilityBackend` — `read_tree`, `resolve`, `probe`, **`read_state`**,
   `act`. `read_state` exists because an injected keystroke has no pattern call that can carry a witness:
   the input path proves itself by reading what the target now contains, and before that method existed the
   `--expect` on a typed value could not be *proven*, only asserted (§51 defects 5 and 6, and the test
   `a_state_read_reports_the_controls_own_patterns_without_acting`).
4. **Isolation is measured, not claimed.** `grep -rl uiautomation crates apps --include=*.rs` returns
   `uia.rs` (789 lines) plus two doc comments in `accessibility.rs`. Nothing above that module can tell a
   wrapper is underneath, which is what makes R-04 a swap rather than a rewrite — *in principle*, see the
   consequences.
5. **The escape hatch was not written, and the phase says so.** Two UIA difficulties actually appeared:
   `.ok()` on pattern retrievals (turning an **error** into a *different* absence — four sites), and the
   missing state read. Both were in J.E.A.N.'s call sites. `Win32_UI_Accessibility` was therefore never
   added to our feature list; the wrapper's own manifest requests it, cargo unifies features across the same
   version, and a second name for the same binding would have bought nothing.

**Consequences.**

- (+) One dependency, +3 lock entries (337 → 340; `uiautomation` and `uiautomation_derive` are named, the
  third is **not attributable from the retained evidence** — resolving it needs the pre-Phase-4 `Cargo.lock`,
  and this tree is not a git repo), 21 native tests passing against real providers.
- (+) Four replaceable boundaries means the parts that depend on somebody else's behaviour are enumerable:
  `AccessibilityBackend` (one impl), `CaptureBackend` (`PrintWindowCapture`, `ScreenCapture`),
  `InputBackend`, `ocr::TextExtractor` (one impl, and it refuses).
- (−) **The swap has never been performed.** D-017's *"a stall costs days, not a rewrite"* is now a
  hypothesis with an unexercised remedy attached; the honest claim is "one file imports it", not "we can
  replace it quickly".
- (−) The bus factor of the wrapper (204★, one primary author, pre-1.0, a feature-gating bug of the kind
  above) is **still the project's exposure**, and the newly discovered input-feature failure is evidence for
  R-04 rather than against it.

**Alternatives rejected.** *Raw COM via `windows` for the whole UIA surface* — ~10× the code, and the brief
forbids writing custom COM infrastructure to avoid a suitable dependency that exists. *FlaUI as a sidecar* —
a .NET runtime the brief forbids installing, to replace a dependency that answered 21 native tests.
*Adopting `uiautomation`'s `screenshot`/`process`/`clipboard`/`event`/`dialog` features too* — `screenshot`
pulls `png`, and the rest are either unused or duplicated by code that already had to be written with the
bounds this phase needs.

---

## D-049 — Capture, input and OCR go dependency-free: `PrintWindow`/`BitBlt`, raw `SendInput`, and a contract instead of a model

**Status:** ACCEPTED · **Date:** 2026-11-06

**Decision.** Take no new dependency for the three mechanism surfaces that Phase 4's dependency table
assigned to `xcap`, `windows-capture`, `enigo` and RapidOCR/`ocrs`. Implement per-window capture directly on
`PrintWindow(PW_RENDERFULLCONTENT)` with a virtual-screen `BitBlt` alternate and a local BMP writer;
implement input directly on `SendInput`; deliver OCR and grounding as a **typed contract that refuses**, and
report the runtime as unverified.

**Context.** The four candidates were measured before this was decided, and each measurement is in the
documents: `grep -c '^name = "…"' Cargo.lock` → **0** for `xcap`, `windows-capture`, `enigo` and `ocrs`
(their **sources** are in the local registry cache, which is a different fact and is stated separately), and
`C:\Python314` imports `onnxruntime 1.27.0`, `numpy 2.4.6`, `PIL 12.3.0` but **not** `rapidocr` or `cv2`. The
brief forbade installing interpreters, downloading weights, launching model servers or calling paid APIs, and
warned that the build volume was itself a risk (R-26: this batch ran out of disk mid-build, §53.2).

**Rationale.**

1. **Capture is one still, not a pipeline.** What the desktop phase asks for is "show me this window,
   now, bounded", and `Windows.Graphics.Capture` is a frame-subscription API that wants a D3D surface and a
   live pipeline. That is a shape mismatch, decided against the requirement — **not** against a measured
   WGC failure, because none was measured, and `03-DEPENDENCIES.md` §3 says so in those words.
2. **The capture that shipped answers the case that mattered.** Measured against the fixture: an excluded
   window **refuses capture before either API runs** (T10), while the same window answers the tree (T05),
   names its own exclusion (T08) and is operated to `proven` (T21); a dark window is *measured* (`luma=`,
   `uniform=false`, T11) rather than called protected. A black frame disables nothing, which is the brief's
   requirement, and it needed no dependency and no new lock entry.
3. **`SendInput` is where the guarantees live, and they are not in an injector library.** The two things
   Phase 4 must do are *before* the send — re-verify the real foreground, re-resolve the target rectangle —
   and *after* it, which is D-048's `read_state`. Enigo would have owned the part in the middle and left
   both ends to us. Two measured details justify that division: `SendInput` counts **events, not
   characters**, and `KEYEVENTF_UNICODE` requires `wVk = 0` with the UTF-16 unit in **`wScan`** — writing the
   character into `wVk` produced `9.*5354 5` on the screen (§51 defect 6), and the fix is `unicode_event()`
   plus `typed_text_carries_its_characters_in_the_scan_field_and_names_no_key`.
4. **A contract that refuses is worth more than a mock that answers.** `ocr::TextExtractor` ships with
   `NoOcr`, which returns `InvalidSetting("vision.ocr_backend is \"none\"; no text extraction is
   configured")` and reports `BackendStatus::Unavailable`; `SidecarProtocol` translates a request to one
   line and a reply line to a result, with the spawn, supervision and timeout deliberately left in
   `jean_core::desktop`, which is the only place that knows whether the operator granted a worker a process.
   `BackendStatus::Verified` exists as an arm **nothing in this build can return** — which is what keeps a
   future report from confusing "wired up" with "proven", and why the brief's *"do not label a mock OCR/VLM
   response as working inference"* is enforced by a type rather than by discipline.
5. **Disk is a design input.** `target\debug` measured 26.87 GB of 340 lock entries at the time of writing,
   with 278 MB free on the volume, and the aggregate workspace test run did not complete (§53.2). Four
   libraries that each pull an image pipeline, a D3D stack, or a tokeniser would have made the next batch's
   verification harder, not easier.

**Consequences.**

- (+) Phase 4 adds exactly one external crate (D-048's), and the three surfaces that could have been
  dependency sprawl are `crates/jean-windows/src/{capture,input,ocr}.rs` behind traits.
- (+) The refusal path is tested, so "OCR is not configured" is a sentence the build can produce honestly
  instead of a silence.
- (−) **The plan's input test was written against enigo and is retired, not done** (R-03, §52 row 5): "test
  its per-monitor-DPI behaviour explicitly" was a condition of depending on it, and this decision is the
  reason it was never depended on. Retiring it is not a free pass — the requirement's real subject, that a
  *fixed coordinate* lands on the right control at mixed scaling, had to be proven some other way, and the
  Phase-4 correction does that on the shipped path: normalisation edge cases as unit tests, and a coordinate
  click on the 100 % secondary driven from a 125 % primary, witnessed by the target's own counter. What the
  machine still cannot show is a negative virtual origin, 150 % and 200 %, and a single-monitor-only session.
- (−) No OCR, no grounding, no pixel-based action has ever produced an answer on this machine. The
  visual-fallback half of D-004's hierarchy is **a boundary, not a capability**, and §52 says so.
- (−) WGC, `xcap` and `ocrs` are unmeasured alternatives; a future batch may find one of them better, and it
  will be choosing without a failure of the shipped path to point at.

**Alternatives rejected.** *Adopt the four libraries as planned* — the brief's no-install constraint
covered the model side, and the two capture libraries plus enigo would have bought a shape mismatch, a
bus-factor question, and a compile cost that this machine could not pay at 278 MB free. *Ship a mock
extractor that returns fixture text* — it would pass the contract tests while teaching every caller that a
`TextExtractor` produces text, which is the exact over-trust the phase was told to prevent. *Defer capture
and input too, and ship only UIA* — UIA without a verified input fallback cannot operate an application
that exposes no pattern, which is the case the brief asked to be handled rather than ignored.

---

## D-050 — A hung accessibility provider is abandoned, never cancelled — and the leak is capped and counted instead of hidden

**Status:** ACCEPTED — **partially SUPERSEDED by D-051 on 2026-11-06** · **Date:** 2026-11-06

> *What still stands:* an interrupted desktop call is **uncertain**, reported as such, given exit **4**, and
> never replayed; and the worker limit refuses rather than queues.
> *What did not:* the mechanism. Point 2's "isolation has to come from the thread's shape" was the wrong
> lever — a thread can be given its own apartment and still be unretrievable, and D-051 moved the
> unretrievable work into a unit that *is* retrievable by being destroyed. The `IN_FLIGHT` count in point 3
> is now an input-path count; the provider-path count is `worker::live_workers()`.

**Decision.** Run every UIA, capture and input call on a **throwaway thread with its own COM apartment**,
wait on it with a deadline and a cancellation check, and when it does not answer, **give up on it without
claiming to have stopped it**. Count the outstanding calls for the process, cap them at
`[desktop] max_concurrent_workers` (default **4**), and refuse accessibility work outright beyond the cap.
Report the abandoned case as `uncertain` — exit **4** — and never replay it.

**Context.** The brief's requirement was specific: *"keep UIA/capture operations outside the core's execution
thread. A hung accessibility provider must be bounded by a worker timeout; a timed-out thread alone is not
adequate isolation if it remains stuck indefinitely."* That sentence is the whole difficulty: a
`SendMessage` into a provider that is not pumping messages cannot be cancelled by anything in the calling
process, and a naive bounded-worker design turns one stubborn application into an unbounded pile of parked
threads.

**Rationale.**

1. **The honest verb is *abandon*.** `bounded.rs`'s own module header says the thing that would otherwise be
   buried: *"a timed-out call is abandoned, not cancelled, and pretending otherwise would be the kind of
   claim this project exists to refuse"*. Only the receiving window processing the message, or being
   destroyed, ends it.
2. **So the isolation has to come from the *thread's shape*, not its countability.** One thread per call,
   never reused, each with its own apartment — which is why one stuck provider cannot starve an unrelated
   window's inspection, and why T23 (unrelated windows still answering while a worker is blocked) is the
   test that matters.
3. **The leak is made visible instead of optimistic.** `IN_FLIGHT` is incremented *before* the spawn and
   decremented by the worker itself, so an answered call cannot leave the count high and an abandoned thread
   is **still counted until it actually returns** — the tests are
   `a_call_that_never_answers_is_abandoned_and_still_counted` and
   `the_budget_refuses_before_spawning_another_leak`.
4. **Cancellation stays responsive while the call is not.** The wait is sliced at 25 ms and checked against
   the run's cancel flag as well as the deadline — the same shape `jean_model::http` and
   `jean_acp::watchdog` use — so `cancellation_ends_the_wait_even_though_the_call_continues` is true of the
   *caller*, which is the only party J.E.A.N. can actually control.
5. **An uncertain side effect is not retried, and the exit code says so.** The verdict vocabulary carries a
   dedicated value: exit **4**, "the input could not be accounted for … it will not be repeated" (T22). The
   brief's *"never replay an uncertain click or text entry automatically"* is a different outcome from
   failure, and a process that could distinguish them needed a fourth code.
6. **A panicking worker is a distinct answer too** (`a_panicking_worker_is_reported_as_a_panicked_worker`),
   because a panic swallowed as a timeout would misreport the mechanism's own health as somebody else's hang.

**Consequences.**

- (+) `jean desktop` cannot be turned into a whole-application hang by one unresponsive provider; T22 and
  T23 measure exactly that.
- (+) The cap is *configuration*, so an operator who has measured their own desktop can raise or lower it,
  and validation refuses `0`.
- (−) A provider that never returns and is never destroyed holds a thread, its stack and its COM apartment
  for the life of the process, and **no reaper exists** — R-29 records this as bounded rather than fixed.
- (−) Refusing accessibility outright at the cap is, by design, a worse user experience than quietly
  queueing the call, and a queue would have been the dishonest option.
- (−) Four exit codes (0/1/3/4, plus 5 for a policy refusal) is more than a CLI usually carries; every
  document that reports a verdict now has to say which of the four it means, which is why §49 defines the
  vocabulary before any result does.

**Alternatives rejected.** *One long-lived worker thread with an apartment and a queue* — it is the design
the brief warned about: the queue is the hang, and the whole desktop loses accessibility to one application.
*`Join` the timed-out thread and call it cancelled* — the `Drop`-joins-a-parked-thread failure mode of
Phase 3 (R-27, §44) is the measured precedent for what that costs. *Mark the abandoned call `failed` and let
the caller retry* — a retry of a click that may have landed is the one thing this phase may not do;
`uncertain` with exit 4 exists precisely so the operator, not the mechanism, decides what happens next.

---

## D-051 — A hung desktop call costs a **process**, and the process is terminated: provider work moves to a supervised child

**Status:** ACCEPTED — **supersedes D-050's isolation mechanism, and keeps D-050's uncertainty rule** ·
**Date:** 2026-11-06 (Phase-4 correction)

**Decision.** Run UIA and capture calls in a separate `jean-desktop-worker.exe`, one per call, spawned
through the supervision `jean-process` already provides — `CREATE_SUSPENDED`, a Job Object with
`KILL_ON_JOB_CLOSE`, pipe halves owned by the parent — and talk to it over typed JSON-lines IPC with a
**startup bound, a request bound and a shutdown bound**, closing the job object at any of them. Never answer
a provider call in-process, and never fall back to in-process work when the binary is missing: that is a
refusal. Keep D-050's rule intact across the boundary — an interrupted call with side effects is `uncertain`,
exit 4, and is not replayed.

**Context.** D-050 recorded the honest verb — a timed-out cross-process send is *abandoned*, not cancelled —
and then bounded the damage with a thread count. That was still not the requirement: *"a timed-out thread
alone is not adequate isolation if it remains stuck indefinitely."* An abandoned thread keeps its stack, its
COM apartment and the provider's attention for the life of the process, and no API in this crate's reach
retrieves any of it. Raising `max_concurrent_workers` would have made the leak bigger rather than smaller,
and documenting leakage as solved would have been the other half of the same mistake. The thing that can
actually be ended is a **process**.

**Rationale.**

1. **Kill what can be killed.** The parent closes the worker's job object; the OS ends the process and every
   child it started. Nothing is left parked, counted, or carried — the accumulation D-050 could only cap is
   now structurally impossible.
2. **The boundary has to be typed and bounded, not a general-purpose RPC.** `Body`/`ReplyBody` are the whole
   desktop vocabulary (tree, resolve, state, act, capture, probe) at 1 MB in and 8 MB out, with
   `PROTOCOL_VERSION` checked on a `Hello`/`Pong` handshake so a stale binary names itself instead of
   presenting as a hung provider. A protocol that could express anything would have to be validated like an
   API at a network boundary; this one is a crate talking to its own child.
3. **Three deadlines, because there are three ways to not arrive.** Startup (the process may never paint),
   request (the provider may never answer), shutdown (a healthy worker that never saw its EOF). The last one
   was not hypothetical: it was measured as a **2.03 s** cost on every healthy call, from the parent's own
   duplicate stdin handle keeping the pipe open, and it is now asserted against
   (`supervised < worker::SHUTDOWN_BOUND`) rather than noticed by a human.
4. **COM stays where COM belongs.** The apartment, the `UIAutomation` instance and every element are created
   in one call inside the child and die with it, so nothing can be cached across the boundary and an
   interrupted call cannot corrupt the next one (`uia.rs`'s first rule).
5. **Frames cross as a file plus metadata, and the parent reads the bytes before the session ends** —
   the frame directory is the parent's own temp path and is deleted with the session, so a reply that
   outran its evidence is impossible by construction, and a short frame is reported incomplete rather than
   displayed.
6. **The count changed meaning rather than being raised.** `[desktop] max_concurrent_workers` is still 4 and
   still refuses rather than queues, but now counts **live worker processes** (`worker::live_workers()`,
   gated before a task row exists), which is the number an operator can check against `tasklist`.
7. **Input stayed on the thread path, on purpose.** `SendInput` posts to the system queue and its
   verification reads the window manager's own records, so no foreign provider can hold it open — and killing
   a process *between* the press and the release of a click would make the no-replay rule harder to honour,
   not easier. A mechanism moved for consistency with a sibling would be a mechanism moved for the tidy
   diagram.

**Consequences.**

- (+) Five rounds of deliberately hung providers end with `live_workers() == 0`, no stray worker in
  `tasklist`, and a successful `Invoke` on a healthy window immediately afterwards
  (`a_storm_of_hung_providers_leaves_no_worker_behind_and_the_desktop_still_works`) — a claim the thread
  design could not have made, because its leak was real.
- (+) Reuse of existing supervision rather than a second `CreateProcessW`: the cost is three lockfile lines
  (`jean-process`, `serde`, `serde_json`) and 340 packages unchanged.
- (+) The isolation is measured as a number and reported as one: ~3 ms supervision, ~56 ms handshake,
  ~150 ms for an 18-control walk, ~88 ms for a 640x360 capture.
- (−) Desktop work now needs a second binary shipped beside `jean.exe`, and an absent one means no desktop
  functionality at all. That is the intended shape of the refusal, but it is a packaging obligation Phase 6
  cannot skip.
- (−) A hung call's *effect* is still unknown; termination is not a rewind. D-050's exit 4 survives exactly
  for this reason.
- (−) Every desktop call pays a process start, and the honest framing is that the phase traded wall-clock
  time in the core for the elimination of an unbounded leak.
- (−) A worker that is killed while holding a UIA connection leaves *that* conversation's state wherever the
  provider keeps it. The provider is somebody else's process; nothing in this build can promise what it did
  with the half-finished call, which is why the call is reported uncertain rather than clean.

**Alternatives rejected.** *Raise `max_concurrent_workers` and keep the thread model* — explicitly ruled out
by the instruction, and correct: it enlarges the cap on a leak instead of removing the leak. *`CancelSynchronousIo`
on the parked thread* — it does not reach a `SendMessage` already inside another application's window
procedure, and a thread that wakes into a half-completed UIA call is a worse liability than one that never
wakes. *A long-lived worker with a queue inside it* — D-050 rejected the in-process queue for the same
reason, and moving the queue into the child only relocates the hang: one blocked provider would then
starve every unrelated window the same worker would have served next. *In-process fallback when the worker
binary is missing* — a fallback that silently restores the behaviour this decision exists to remove is a
way of unmaking the decision under conditions nobody tests on purpose. *Also moving input into the worker* —
see rationale 7; consistency was not a reason.

---

## D-052 — Retention expiry is authorised by an ownership manifest, not by a filename pattern, and a frame that cannot be claimed is not captured

**Status:** ACCEPTED · **Date:** 2026-11-06 (Phase-4 correction) · **Supersedes the "no cleaner exists"
position held by D-049's retention clause and by `config.rs`'s `d_retention` comment**

**Decision.** Make `[desktop] retention = "keep_24h"` true by writing an append-only **ownership manifest**
(`jean-captures.jsonl`) inside a validated capture root: a frame is claimed *before* its bytes are written,
the claim is the only thing that authorises a deletion, expiry runs at 24 h from the claim, the kept set is
held under a 512 MiB ceiling oldest-first, and every path is refused if it is a reparse point or resolves
outside the root. If the claim cannot be written, the capture is refused.

**Context.** R-30 recorded that two of the three retention values wrote unredacted pixels and the build
removed nothing, and §49.1's config default said plainly that a cleaner did not exist because *"a mechanism
module that deletes its own evidence is a mechanism that loses it"*. That reasoning was sound about a
**glob** and wrong about a **manifest**: what must not be deleted is evidence somebody else owns, and the
honest fix was to make ownership explicit rather than to keep promising a lifetime nobody enforced. Leaving
it for packaging would have shipped a configuration file that lies.

**Rationale.**

1. **A filename pattern cannot tell JEAN's frames from the operator's.** The capture names are
   `jean-<hwnd>-<epoch-ms>.bmp`, and a sweep by prefix and mtime would remove a file the operator copied,
   renamed or placed there — the failure this decision exists to prevent, and the reason a *claim* rather
   than a *convention* is the authority.
2. **Claim-before-write inverts the only order that accumulates.** Writing first and claiming after leaves a
   window in which a crash produces an unremovable frame; claiming first means the worst outcome is a
   refused capture with a reason attached.
3. **Reparse points are refused at the root and per file**, because a junction is how "the capture
   directory" becomes "some other directory" without any path in the configuration changing. The attribute
   value is recorded as **measured**: a real `mklink /J` entry reports `0x410` and `symlink_metadata()` says
   it is not a directory, and the first version of this module used `0x1000` (`FILE_ATTRIBUTE_OFFLINE`),
   which produced a refusal naming the wrong cause — a worse outcome than no refusal, because it sends the
   operator to look for a broken directory instead of the link that is there.
4. **Unreadable claims are refused in the same place valid ones are**, not skipped: a manifest line that
   cannot be parsed arrives as an entry whose name cannot be safe, so "I could not read this" and "this
   authorises a deletion" cannot live in two code paths that disagree.
5. **`keep_until_manual` is excluded from the byte ceiling.** It made a promise to the operator, and paying
   for `keep_24h`'s bound out of the manual keeps would silently change a different setting's meaning.
6. **The clock is a parameter** (`reconcile(dir, now_ms, horizon_ms, budget_bytes)`) because a 24-hour
   behaviour that is only testable by waiting a day is not a tested behaviour; and the deletion tests run a
   real `mklink /J` rather than asserting on a simulated attribute.

**Consequences.**

- (+) `keep_24h` is a true statement, and `jean desktop status` reports the owned total, what is due, and the
  count of files it will not touch — so the boundary is visible rather than assumed.
- (+) The one permitted deletion in this batch's instruction — capture expiry on its own artifacts — is now
  the only deletion the mechanism can perform.
- (−) A capture can now fail for a store reason (an unwritable or non-safe root), which is a new failure the
  operator can meet; it comes with the reason in the error.
- (−) The manifest is append-only and grows without bound while the setting is in use; the sweep prunes
  claims whose files are gone, but this is a log that gets longer in normal use, not a fixed-size table.
- (−) Pixels are still unredacted. This decision bounds *how long* they exist, not *what they show*, and the
  note says so in four places rather than pretending otherwise.

**Alternatives rejected.** *Glob and mtime over `capture_dir`* — the shape that deletes the operator's
files; rejected outright. *A `jean desktop captures clean` command only* — leaves the setting false for
everyone who does not know to run it, which is the same promise broken politely. *Reject `keep_24h` at
configuration load* — what the instruction allowed if safe expiry could not be delivered; it was deliverable,
and refusing a feature the plan asked for to avoid writing ~1,200 lines is not a trade someone else should
have to make. *Delete the frame after the task reads it* — that is `discard_after_task` with a file in the
middle, and it destroys the evidence a kept capture exists to be. *A background timer* — R-20's
"nothing continuous" rule, and a cleaner that runs unseen is the least auditable way to delete something.

---

## D-053 — History search is a derived, redacted, bounded read; long-term memory is written only by an explicit operator command

**Status:** ACCEPTED · **Date:** 2026-11-07 (Phase-5 memory) · **Extends D-008 (SQLite only) and D-011 (the model may request, never grant); narrows the runtime-settings list in `04-SETTINGS.md`**

**Decision.** Index `tasks.goal`/`outcome_detail` and safe `events.detail` in FTS5 tables kept in step by triggers, redact through SQLite scalars **before** truncating, bound every search by query length, result count, snippet size, `PRAGMA query_only` and a 2 s interrupt watchdog, and store long-term memory only in `saved_facts`/`preferences` rows written by an explicit CLI command with a metadata-only audit row in the same transaction.

**Context.** §56 left searchable history and user-approved facts open. The danger is not retrieval quality but authority: a search that returns another project's notes, an index that copies a legacy clear-text token, or a "remember this" path that promotes a transcript would each turn stored text into either a leak or an instruction. `keep_24h` had already shown what a setting with no reader costs.

**Rationale.**

1. **The index is derived, never authoritative.** Hits join back to `tasks`/`events`, so state and outcome are the row's current values; a drifted index can show a stale snippet but cannot report a stale fact about what happened.
2. **Redaction belongs in the schema.** Triggers call `jean_redact`, so a row written by an older build, or by any JEAN connection, is indexed redacted. Redaction precedes truncation because truncating first can cut the middle out of a pattern (D-037).
3. **A per-connection scalar fails closed.** An external writer cannot satisfy the trigger and its write is refused, rather than being able to put unredacted text into the index.
4. **Bounds must cover work, not only output.** `LIMIT` caps results; the watchdog caps time. There is no `LIKE` fallback anywhere, so an unavailable FTS5 is a reported incompatibility, not a silent unbounded scan.
5. **Provenance is a column with one allowed value.** `operator_command` is the only label `saved_facts.source` and `memory_changes.provenance` accept. The CHECK constrains the value, not the caller — the call graph is the guarantee and the schema is the backstop.
6. **A deletion's record must not retain the deleted content.** `memory_changes` holds action, id, provenance and time only; no text, scope, value or hash.
7. **A preference with no reader is a lie in a table.** Four keys are supported because four have consumers; the rest are refused. Values are validated against a candidate configuration before the row exists, and the environment is applied last so `JEAN_NO_VOICE` cannot be overridden by a row that outlives the session.

**Consequences.**

- (+) Search and memory work with `provider = none` and no voice worker; 43 new tests plus the existing suites pass (995 total).
- (+) Scope isolation is enforced in the query, not by a caller remembering to filter: no scope means global only.
- (−) Facts are a separate table from `preferences`, deviating from the roadmap's wording; identity and lifetime differ enough that key/value rows would need synthetic keys.
- (−) Pattern-based credential detection is a floor, not a guarantee: an unknown shape can still be saved, and the limit is documented at the refusal.
- (−) CJK runs are single tokens under `unicode61`; substring search within a run does not match. Pinned by a test rather than left to be discovered.
- (−) Hand-editing history now requires a JEAN connection, because the triggers depend on registered scalars.

**Alternatives rejected.** *`LIKE '%…%'` fallback when FTS5 is missing* — answers correctly and costs without bound; the incompatibility must be reported. *External-content FTS tables* — one content table per index cannot span `tasks` and `events` without a third id space that drifts. *Storing a `DefaultHasher` digest as fact identity* — only stable while the standard library's hash is; a fact outliving a Rust upgrade would duplicate itself. *Recording fact text in the audit trail* — a second copy of content the operator asked to remove, in the file kept longest. *Supporting the full settings-document key list* — keys with no reader are the `keep_24h` failure again. *Injecting `MemoryView` into every model request* — pays for all facts including other projects', and would need a framing rule for facts versus live instructions that this phase does not decide.

---

## D-054 — Audio is WASAPI inside the voice worker, inference lives off the control loop, and sherpa is a feature the default build refuses to resolve

**Status:** ACCEPTED · **Date:** 2026-11-07 (Phase-5 native speech) · **Implements D-006 (voice is a
separate process) and D-013 (sherpa-onnx in Rust), and adds the rule that a dependency's build script
may not fetch binaries; narrows `04-SETTINGS.md` `[voice]` to the keys this build reads**

**Decision.** Capture and playback go through Windows WASAPI in **shared mode**, opened only inside
`jean-voice`; the recognizer and the synthesizer each own a thread and own their audio endpoint on that
thread; the control loop sends requests and drains events and therefore never runs inference; the
sherpa-onnx crate is **not in `Cargo.toml` or `Cargo.lock` at all** — the native adapter is
`#[cfg(feature = "sherpa")]` code whose dependency line ships as a comment, and provisioning it is an
explicit operator act recorded in `docs/voice-assets.md`.

**Context.** §57 shipped the voice worker as fixture-only: `synthesize()` and `end_capture()` were
called directly inside the 10 ms control loop, so a real model would have made `Interrupt`, `Ping` and
`Shutdown` wait for inference — exactly the Pipecat-class stall D-006 exists to prevent, reintroduced
one layer down. Two constraints collided: the accepted stack (D-013) is a crate whose build script
downloads a 122 MB archive unless `SHERPA_ONNX_LIB_DIR` is set, and this batch may not download, install,
open a microphone or play audio.

**Rationale.**

1. **The engine objects are `!Send` because they wrap raw C pointers.** `OnlineRecognizer`,
   `OfflineTts` and the WASAPI interfaces are created, used and dropped on one thread. That is a fact
   about the APIs, so it became the architecture rather than something to argue with: recognition owns
   the microphone, synthesis owns the speaker, and nothing is shared between them. Only the factory —
   paths, not handles — is `Send + Sync`.
2. **Cancellation has to be noticed mid-operation or it is not cancellation.** Every long call takes a
   `CancelFlag`: `feed`/`finish` check it between decode steps, and sherpa's TTS callback returns `false`
   to abort generation. A stop request that only lands after the whole utterance is synthesized would
   reproduce the bug class D-006 rejects.
3. **Deferred replies, not blocked ones.** `BeginCapture` is answered with `Capturing` when the endpoint
   actually opened, and `EndCapture` with the transcript when the flush finishes. The press the owner
   asked about is the press that happened, so an operator who releases within one scheduling quantum gets
   a coherent activation instead of a refusal — which is what the first draft got wrong, and the
   state-machine test now pins.
4. **Shared mode is the only defensible choice for a companion.** Exclusive mode would take the
   operator's speaker away from every other application. The price is a mix format Windows chose, so
   `audio.rs` resamples to `voice.sample_rate` instead of hoping the device agrees.
5. **`Opening` exists because "the device is open" is a fact only the device can confirm.** Reporting
   `Capturing` before `activate` succeeds would let the worker claim it is recording when it is not.
6. **An absent dependency beats an optional one.** An optional dependency is still resolved and still
   written into `Cargo.lock`, so `cargo build --locked --offline` for the *default* feature set would
   either need the crate cached or fail — and a plain `cargo build` without `--offline` would run the
   build script that fetches 122 MB. Shipping the line as a comment keeps the default build honest, and
   the `sherpa` feature with no dependency makes the missing-crate error self-explanatory next to the
   instruction that fixes it. `assets::native_unavailable` then distinguishes "you did not enable the
   feature" from "you enabled it and the assets are wrong", which is the diagnostic an operator needs.
7. **Device policy is derived, not configured.** `backend = 'fixture'` forces the simulated endpoint;
   there is no key that pairs a placeholder engine with a real microphone, because that test would
   record the operator while reporting itself as a test.
8. **Licences were checked per artifact, and one of them failed.** The ASR bundle's model card is
   Apache-2.0 and Silero VAD is MIT (both read from source); the `amy` voice card says its dataset
   licence is "See URL" and that it is a fine-tune of another voice — which is not a redistribution
   grant for those weights. So TTS ships as implemented-but-blocked-on-licence rather than defaulted,
   and `docs/voice-assets.md` §4 states what would have to be true to unblock it.

**Consequences.**

- (+) `cargo check --workspace` passes with zero warnings and the WASAPI module **compiles**, which is
  real evidence for the audio path even though no device was opened.
- (+) The four cancellation scopes survive the rewrite and are measured: `Interrupt` and `Ping` answer
  while inference is in flight, and a cancelled task still leaves voice `ready`.
- (+) Searchable history, memory and every earlier phase keep passing: 1034 tests across 60 suites.
- (−) **No native compile/link, inference or physical-audio evidence exists**, and the batch cannot
  produce any without the assets. The tiers in `docs/voice-assets.md` §7 stay separated for that reason.
- (−) `sherpa.rs` is written against API signatures read from the published v2.0.1 sources rather than
  from a compiler. A mismatch will surface at provisioning, in tier 2 — which is why tier 2 is a named
  step and not a footnote.
- (−) `configure_timeout_ms` had to become its own deadline. Model loading cannot finish inside the 2 s
  control timeout, and a worker that is loading models is not a hung worker.
- (−) Announcements and spoken controls ship **off by default**. An announcement takes the single
  utterance slot; a spoken control intercepts a transcript that would otherwise become a task. Both
  change what the operator experiences, so neither arrives as a surprise.
- (−) Linear resampling only; no streaming TTS, so a long answer is fully generated before it is heard.
- (−) **No acoustic barge-in.** `Interrupt` is a control message. The roadmap box for barge-in stays
  unchecked with that distinction written out, because the two are routinely conflated.

**Alternatives rejected.** *Optional `sherpa-onnx` dependency with a `sherpa` feature* — still resolved
into `Cargo.lock`, so it either breaks `--locked --offline` for the default build or reintroduces the
silent download; measured, not theorised: adding it made `check --locked` fail. *Raw `extern "C"` FFI to
the sherpa C API to dodge the crate entirely* — would contradict D-013, need the whole header surface
re-derived, and move a large unsafe boundary into the tree for no gain in honesty. *Loading models on
the control loop and accepting a stall* — reintroduces the exact failure D-006 was written to make
structurally impossible. *`unload_when_idle_secs` / unloading models after each press* — trades a
multi-second reload for memory, and the settings document's version of it had no reader. *Reporting
`Capturing` before the endpoint opened* — a claim about hardware that has not been confirmed. *A real
`audio_backend` config key* — permits the fixture-engine-with-microphone combination that records the
operator under a test label. *Auto-enabling `spoken_controls` because the phrases are safe* — the risk
is not the phrase, it is a goal the operator typed and never got submitted.

---

## D-055 — The speech dependency is wired as optional, provisioning is declared before the feature is enabled, and the archive's own refusal paths are the guard

**Status:** ACCEPTED — built and measured (`IMPLEMENTATION_STATUS.md` §59). Partially supersedes **D-054**, whose rejected-alternative row ruled out exactly this wiring on a claim that has now been measured and found wrong.
**Date:** 2026-11-07

**Decision:** `sherpa-onnx` = 2.0.1 becomes a real `optional = true` dependency behind `sherpa = ["dep:sherpa-onnx"]`; the "no silent download" guarantee is enforced by the crate's own declared-provisioning branches rather than by keeping the dependency commented out; the API surface is checked by a compiler instead of by reading upstream source; and the CRT question is settled by measurement with a local probe rather than by the archive's filename.

**Context.** §58 left `sherpa.rs` never compiled, the link never attempted, and the reason being that an optional dependency "would still be resolved and locked, so `--locked --offline` for the default build would break". The batch that followed was asked to wire the dependency, compile against the actual SDK, verify no silent download, and test linker/CRT compatibility rather than assume it. All four required the commented-out arrangement to go.

**Rationale.**
1. **The premise was tested and it was wrong.** With the dependency wired, `cargo tree -p jean-voice -e normal` lists **no** sherpa package in the default feature set, and `cargo check --workspace --all-targets --offline --locked` passes after one online resolve wrote the lock (341 → 380 packages, +39, none built). A comment in a manifest is not a dependency mechanism; an optional dependency is, and it does the same job honestly.
2. **Compilation against the real crate is worth more than compilation against my reading of it.** The first `cargo check --features sherpa` found one genuine error — `OnlineModelConfig` has no `joiner`; the transducer trio is at `model_config.transducer.*`. Had the line stayed commented, that error would have surfaced on the operator's machine next to a 122 MB download.
3. **The skip used for compiling is the crate's own documented one, not a stub.** `DOCS_RS` makes `-sys`'s build script return before downloading or emitting link directives. There is no locally written stand-in for the SDK anywhere in this tree: the types, traits and function signatures checked are the published crate's. The evidence is labelled compile-only for exactly that reason — `cargo check` never links, so it proves signatures, not symbols.
4. **"Cannot silently download" was executed rather than argued.** With `SHERPA_ONNX_ARCHIVE_DIR` naming a directory without the archive, the build stops with the full expected filename; with `SHERPA_ONNX_LIB_DIR` naming a non-directory, it stops with that path. `target/sherpa-onnx-prebuilt/` stayed 0 bytes. The fetch branch is reachable only when *neither* variable is declared — which is why §6 of the asset document always pairs the variable with `--features sherpa`.
5. **The archive name does not answer the CRT question, and a probe does.** `jean-voice.exe` imports `VCRUNTIME140.dll` plus `api-ms-win-crt-*.dll` (`/defaultlib:msvcrt`); a `/MT` library linked into it produces `LNK4098` and **still links and runs**; the same library under `-C target-feature=+crt-static` has no conflict, and `jean-voice` builds that way. So §58's "mixing is a link error, not a warning" was false in both documents that repeated it, and the real risk — two CRTs, one process, separate heaps — is a configuration decision, not a filename.
6. **A 32-byte cross-CRT `free` was measured and deliberately not used as evidence.** It returns cleanly and proves nothing about load. Recording that refusal is part of the decision: the probe documents what it does not establish.
7. **Recognition and synthesis are separate prerequisites, so they must be separate failures.** `assets::required()` had merged them, which meant a fully provisioned ASR bundle was refused because TTS was missing. Splitting the lists is what lets Phase 5 land incrementally on an operator's machine.
8. **A file-based transcribe is the only tier-3 evidence obtainable without a person.** `jean voice transcribe clip.wav` runs the real `Recognizer` boundary over 100 ms packets, labels its engine, and refuses to pretend when that engine is a fixture. It exists because the batch rule leaves microphone and speaker use to the operator.

**Consequences.**
- (+) `sherpa.rs` compiles and lints clean under `--features sherpa -- -D warnings`; the one API error is fixed and the header records which upstream file each signature came from.
- (+) The default shipping build is unchanged and still asset-free, now proven by dependency tree rather than by assertion.
- (+) §58's two unverified claims — the optional-dependency rule and the MT/MD link error — are corrected where they were written, with the measurements that replaced them.
- (+) TTS has a selected bundle with an explicit weights licence: `kokoro-en-v0_19`, 319,625,534 bytes, published `sha256:91280485…`, from `hexgrad/Kokoro-82M` at revision `f3ff3571…`, Apache-2.0 on the model card. Piper stays as the documented rejected option; `rhasspy/piper-voices` answers HTTP 401 anonymously, which is itself a reason not to build a default on it.
- (−) **39 packages now sit in `Cargo.lock`**, including C-building ones (`zstd-sys`, `bzip2-sys`) and a second `ureq` (2.12.1 beside the workspace's 3.5.1). Resolved-not-built is still resolved: `cargo deny`-style audits see them, and the audit remains open from Phase 0.
- (−) Enabling the feature requires one online resolve before `--offline --locked` works again — measured: `cargo check --offline` fails with `no matching package named sherpa-onnx` until the lock exists.
- (−) The native link, model load, transcript, synthesis and physical audio remain **undelivered**: the archive and the ASR bundle are absent from this machine and were not fetched.
- (−) Adding an optional dependency invalidated `--locked` for the whole workspace until the lock was rewritten once. Any CI that assumes a frozen lock across feature changes must resolve first.
- (−) `cargo test -p jean-cli` does not rebuild `jean-voice.exe`; two tests failed against a stale worker until a full build ran. Kept in the handoff because it will be rediscovered otherwise.
- (−) TTS code now supports two engines, selected by the presence of `tts_voices`. That is more surface, and neither branch has ever synthesised anything.

**Alternatives rejected.** *Keeping the dependency commented out* — the reason recorded in D-054 is measured false, and it left the adapter permanently uncompiled. *A locally written stub crate mimicking the sherpa API to "compile" the adapter* — would produce exactly the false confidence this decision exists to avoid: the stub's signature errors would be my own, and the real crate's would still arrive at provisioning. *Building everything with `crt-static` right now* — measured to work for `jean-voice`, but it changes the CRT of every binary in the workspace, including the task worker and the desktop fixture, to solve a problem that has not yet been demonstrated; it is written as the operator's option, not applied as a default. *Downloading the archive to "verify the checksum" myself* — the verification the brief asks for is the operator's, and fetching 122 MB under a rule that refuses silent fetches is not made honest by doing it loudly. *Selecting a Piper voice because it is smaller* — its licence is unreadable anonymously and its model card points at a dataset, which is the exact pattern D-054 §7 refused.

## D-056 — The native archive is linked into the default dynamic-CRT build, the `LNK4098` it emits is carried rather than silenced, and int8 recognition weights are the shipped set

**Status:** ACCEPTED — built and measured (`IMPLEMENTATION_STATUS.md` §60). Closes the last undelivered clause of D-055 for link and inference; changes nothing about TTS or physical audio.
**Date:** 2026-11-07

**Decision:** take the link the probe predicted instead of pre-empting it — build `--features sherpa` against the extracted `/MT` libraries with the workspace's ordinary dynamic-CRT configuration, accept `LNK4098` as the only linker message, and keep `-C target-feature=+crt-static` as a documented operator switch rather than a default. Provision recognition from the int8 encoder and joiner with the fp32 decoder, verify every extracted byte-length against the archive's own entry table before trusting any of it, and attribute the one imperfect transcript by re-running with a different weight set rather than by explaining it away.

**Context.** D-055 left the tree compiling and the archive absent, with §59.5 writing "the archive still has to be checksum-verified against `sha256:23e7b986…` by the operator — nothing here has seen its bytes". The next batch was given both archives locally and asked to verify, extract, configure in isolation, link, and transcribe a known phrase — while applying "the smallest build-scoped fix if needed". Two fixes were available before the first attempt even ran: force `crt-static` because the archive says `MT`, or pass `/NODEFAULTLIB:LIBCMT` to make the warning disappear. Both were refused on principle before the linker had spoken, and the linker then agreed with the probe.

**Rationale.**
1. **The digest is what D-055 said it was, and now that is checked.** 122,863,596 bytes hashing to `23e7b986…f0e66` — the published value, matched against the real file. That matters because the alternative was to keep reasoning about an artifact nobody had opened.
2. **The ASR archive publishes no digest, and that difference is kept visible.** Its size matched the published 506,956,414; its hash, `455f40e5…efe0a4da`, is *ours* — recorded so a future copy can be compared, not as a verification. Nothing in this tree pretends the second is the first.
3. **Extraction was verified rather than assumed.** Every one of the 13 model files and 12 libraries came out at exactly the length its archive entry declared (`target/p5o-verify-extract.py`). A silent truncation would have produced a model-load failure that reads like a bad bundle.
4. **Two CRTs, one process, no heap fault observed — reported as that and nothing stronger.** `jean-voice.exe` still imports `VCRUNTIME140.dll`; the libraries carry `LIBCMT`; three recognitions and four model loads exited cleanly. Forcing `crt-static` would change the CRT of *every* binary in the workspace to address a failure that has not occurred, and it silently invalidates the incremental cache. This is the same reasoning D-055 used to reject the pre-emptive switch, now applied with the real link in hand.
5. **The one bad transcript was attributed by measurement.** The 8 kHz clip loses its final word. Same result with fp32 weights → not quantisation; `decode_ready` is an uncapped `while is_ready` loop with a bounded silence pad → not our flush budget. What is left is the linear resample, which `docs/voice-assets.md` §8 had already named as unverified. Recording the discriminator is more useful than recording the symptom, and the fix (sherpa's own resampler) stays an operator-visible option rather than a quiet swap.
6. **Isolation was a requirement, not a nicety.** `%LOCALAPPDATA%\JEAN\jean.toml` did not exist and still does not; the four absolute paths live in a scratch directory reached through `JEAN_DATA_DIR`, which also captures anything the run writes. The operator's settings plane is untouched.
7. **The shipped recognition set is int8 encoder + int8 joiner + fp32 decoder** — 190,180,941 bytes, the combination sherpa's own example uses, and the reason the planning estimate of "483 MB for the ASR bundle" was wrong: the *directory* is 546 MB because it also holds the fp32 duplicates and the test clips, and the *build* carries 1,057,860,400 bytes of libraries that no model-size estimate ever counted.

**Consequences.**
- (+) Phase 5's speech path is now real at three tiers: it compiles, it links, and it transcribes known audio. Nothing about the cancellation architecture, the memory path, or the default asset-free build changed to get there — 1043 tests, same as §59.10.
- (+) The probe's prediction is confirmed by the artifact it was built to simulate, so §59.5's careful "measured, but not on the real archive" note now has a successor result rather than an orphan.
- (+) `jean voice doctor` prints its provisioning path as readable text, found by using it.
- (−) The distributed worker binary still requires the Visual Studio CRT at runtime (`VCRUNTIME140.dll`), and it contains `LIBCMT` statically. That is a supported-configuration statement, not a bug report.
- (−) `LNK4098` will appear on every native build from now on. Anyone who adds `-D warnings` to a *build* (not clippy) step in CI will see it as a warning, and the honest answer is the one in §60.4, not a suppression.
- (−) Cancellation during live native inference is still unexecuted, and the reason is structural: `audio_backend()` derives the device from the speech backend, so `sherpa_onnx` always means `System` audio. Reaching it from a scripted device would be a new configuration surface, which is not this batch's to add.
- (−) A workspace build silently reverts `target\debug\jean-voice.exe` from the 24.7 MB native binary to the 1.67 MB default one. Documented in §60.9 and in the operator pre-flight, because it will otherwise be mistaken for a broken provisioning step.
- (−) TTS is untouched: no Kokoro bundle was supplied, so the synthesis branch remains PENDING after two batches of compiling it.

**Alternatives rejected.** *`RUSTFLAGS=-C target-feature=+crt-static` as the default native build* — measured to work, but it re-CRTs the whole workspace to prevent a fault that has not happened, and D-055 already refused it for the same reason. */NODEFAULTLIB to silence `LNK4098`* — that is removing the only signal the two-CRT arrangement emits, which is the opposite of "test linker/CRT compatibility rather than assuming". *fp32 encoder for every bundle* — 353,707,185 bytes against 187,823,992 for the same three words, and the int8/fp32 comparison this batch ran showed them agreeing on every clip including the imperfect one. *Recording a fresh WAV for the known-phrase test* — the bundle ships three clips with published transcripts; a synthetic recording of my own would have been a weaker ground truth than the one the model's authors distribute. *Calling the 8 kHz loss "model quality"* — that would have closed the finding without attributing it, and the re-run with fp32 weights was four seconds.

---

## D-057 — Owner goals drive execution and feedback shapes future defaults

**Status:** ACCEPTED product direction, implementation planned · **Date:** 2026-11-07

The owner supplies a short goal and optional starting agent. JEAN owns the prompt, delegation, verification, and recovery loop within the configured workspace and budget. Implement this in [Milestone 5.5](08-OWNER-WORKFLOW.md).

Keep D-053's explicit saved facts intact. Add a separate, inspectable layer for learned preferences with source, scope, and owner controls. A current instruction overrides a learned default. Use durable checkpoints and measured outcomes to improve routing over time.

Rejected alternatives: requiring a workflow recipe for each task adds setup to every run. Storing every observed instruction as a preference makes one-off requests and untrusted content persist as defaults.

## D-058 — Native speech is validated against a file instead of a device, the recognizer alone gates configuration, and the transducer is given a measured tail

**Status:** ACCEPTED — built and measured (`IMPLEMENTATION_STATUS.md` §61). Refines D-056's carried-warning clause with a built alternative and D-054's cancellation clause with native evidence; supersedes nothing.
**Date:** 2026-11-07

**Decision.** A real speech engine is exercised through configuration, not through hardware: `voice.offline_wav` substitutes a WAV for the microphone and a counted sink for the speaker; `Configured` depends on the recognizer alone, so a synthesis bundle can fail without switching listening off; and `finish()` pads a *measured* 0.5 s of silence before declaring the stream finished.

**Context.** D-054 moved inference off the control loop and D-056 linked the engine, but §60.7 recorded a structural consequence nobody had wanted: `VoiceConfig::audio_backend()` derives the audio source from the speech backend, so choosing `sherpa_onnx` also chooses `System` audio. Every cancellation assertion in the repository therefore ran against the fixture, and the honest line in the docs was "native cancellation is a tier-4 observation" — i.e. something only the operator could ever see. Meanwhile the 8 kHz clip was losing its last word, and §60.6 had closed that as a resampling limitation, which the next batch was told to reproduce rather than accept.

**Rationale.**
1. **The seam had to be configuration, not a test binary.** A `#[cfg(test)]` harness can prove a function; it cannot prove the supervised worker, because the worker is a separate process reached over pipes. So the substitution lives where the operator's own choices live — one key, empty by default, documented as a validation seam, outranking the backend derivation because a file is the microphone and the speaker is a counter.
2. **A file must be replayed at 1× or it is not a microphone.** The first three supervised runs refused with an empty transcript, and the fault was the harness: replaying a 6.6 s clip as fast as it reads means the release arrives 40 ms in, and a recognizer with 40 ms of audio returning nothing is *correct behaviour*. Pacing the packet pump at real time is what makes the seam answer the question that was asked.
3. **One engine's failure must not veto the other's success.** Kokoro without its lexicon cannot speak, and until this batch `maybe_configured` required both loads, so a working zipformer reported itself unusable. That is the inverse of the rule the project already keeps — no placeholder reported as working speech — and here it was *real* speech being reported as absent.
4. **A threshold is measured, not argued.** Appending 100 ms of silence to the clip recovered the word; the internal pad was already 300 ms. So the requirement is ~400 ms of trailing audio, 0.5 s was chosen for margin under a bounded cost, and the two 16 kHz clips that end in their own recorded silence were re-run to prove the pad is not paid for by a regression elsewhere.
5. **An earlier fix of mine was deleted by its own measurement.** Reordering the pad so it decoded before `input_finished()` was implemented, built, and run — `P0000` truncated identically before and after. It was reverted rather than kept, because "plausible and harmless" is not a reason to carry code.

**Consequences.**
+ Cancellation, ping latency and release→transcript now have **native** numbers (30–76 ms to open, 28 pings answered mid-decode with the slowest at 0–2 ms, 58–116 ms to transcript across runs, expired press refused with no transcript) and a supervised test file that reproduces them on demand and skips with a stated reason when the bundle or the executable is absent.
+ An unrelated long-running task was shown to survive a voice interruption, with the mechanism named rather than inferred.
+ The device-free path is the *pre-flight* an operator can run before trusting a microphone, which is the cheapest separation between "engine problem" and "audio-device problem" this project can offer.
+ A partially provisioned Kokoro bundle is reported as partially provisioned, both by `doctor` and by the refusal text — and the phoneme seam that reaches the acoustic model drops only the requirement it genuinely does not need.
− One more configuration key whose empty and non-empty meanings differ completely, and whose non-empty meaning ("this press is a file") must be caught by `doctor` reporting `offline_wav`, because a silent file-backed session would be a very convincing fake.
− The 0.5 s pad is a floor for the last token, not a general tail guarantee; it bounds release latency on quiet releases by ~500 ms of decoding.
− The seam proves the *worker's* control and cancellation paths, not WASAPI behaviour, endpoint loss, or anything a speaker can do. Physical audio stays the operator's measurement (§9).
− The phoneme route exercises model, voices, tokens, inference and encoding, and deliberately not grapheme-to-phoneme — so a green synthesis result here must never be reported as text-to-speech, which is why the output JSON carries `input: "phonemes"`.

**Alternatives rejected.** *A dedicated `--test-audio` build or a second binary* — it doubles the artifact matrix to avoid one key, and the thing being proven is the shipped worker, so the shipped binary has to be the one that carries the seam. *Mocking the device layer in the test process* — that tests `capture_once`, not the supervised process, the wire protocol, or the deadlines; §58's whole design rests on the worker being a separate process. *Fixing the 8 kHz clip by switching to sherpa's `LinearResampler`* — the independent-resample experiment had already exonerated the existing resampler, so this would have been a change made to a cause that measurement had ruled out. *Raising the pad to 2 s to be safe* — the release path waits for that decode, and an unbounded "safety" margin on a hotkey-triggered capture is exactly the latency the budgets exist to prevent. *Reporting Kokoro as ready because the model loaded* — the engine refuses at tokenization; the user-visible truth is that it cannot speak, so `doctor` and the required-asset list had to be corrected to agree. *Downloading a lexicon to close the gap* — out of batch scope, and the rule that assets are the operator's explicit act is the reason TTS was ever blocked rather than quietly fixed.

## How to add a decision

1. Take the next `D-nnn`.
2. Fill in: **Status**, **Date**, **Decision** (one sentence), **Context**, **Rationale** (numbered), **Consequences** (with `+`/`−`), **Alternatives rejected** (with the reason each was rejected).
3. If it reverses an earlier decision, mark the old one `SUPERSEDED` and link the new ID. Never delete an entry — the reasoning is the value.
4. Update the index table at the top.
5. If the decision changes a dependency, update `03-DEPENDENCIES.md`. If it changes a config key, update `04-SETTINGS.md`. If it changes the order of work, update `05-ROADMAP.md`. If it introduces a hazard, update `06-RISKS.md`.

**A decision without a recorded rejected alternative is not a decision — it is a preference.**
