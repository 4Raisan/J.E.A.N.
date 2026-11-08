# J.E.A.N. — Implementation Suggestions

The [owner workflow milestone](PLANNING/08-OWNER-WORKFLOW.md) specifies how the existing planner, task history, and supervised workers will support personalized quality defaults and checkpointed agent handoffs after voice validation.

> Engineering direction for a lightweight, Rust-first desktop orchestrator.

This guide translates the J.E.A.N. vision into a practical build strategy. It is intentionally opinionated: build the orchestration core first, keep specialist capabilities outside it, and add integrations only when they solve a real use case.

## 1. The most important architectural decision

**Do not build J.E.A.N. as one giant autonomous agent process.**

Build a small Rust supervisor/orchestrator that coordinates separate tools and optional workers.

The core should own:

- Task lifecycle and state.
- Tool registry and tool selection metadata.
- Permission and approval policy.
- Process lifecycle and cancellation.
- Context references and compact task history.
- Event routing and result verification.
- User-facing status and notifications.

The core should not own:

- Large language model weights or inference runtimes.
- Always-on screen recognition.
- Speech model inference.
- Browser engines.
- Coding-agent implementations.
- Heavy GUI automation frameworks unless a specific capability requires one.

A task should be able to use external processes without blocking the core or the voice interface.

## 2. Recommended architecture

```text
                 J.E.A.N. UI / Voice / Text
                            |
                    Local IPC Gateway
                            |
                  RUST ORCHESTRATOR CORE
        +-------------------+-------------------+
        |                   |                   |
    Task Engine         Tool Registry       Policy Engine
        |                   |                   |
    Task Store          Adapters            Approval Rules
    Event Log           Capability Map      Audit Log
    Scheduler           Health Checks       UAC Broker
        |                   |
        +-------------------+-------------------+
                            |
                     Execution Manager
                            |
          +-----------------+------------------+
          |                 |                  |
     Windows Bridge     Agent Bridge      Optional Workers
          |                 |                  |
      Windows API          ACP              Voice service
      UI Automation        MCP              Vision service
      Files/processes      CLI              Local model server
      Shell/ConPTY         Browser API      OCR service
```

### Design rule

Every subsystem should expose a small contract. The orchestrator should not need to understand the internal implementation of a voice engine, browser automation tool, or coding agent.

## 3. Language and runtime

### Primary: Rust

Use Rust for the main Windows process, process supervisor, IPC, policy enforcement, task state machine, and native operating-system integration.

Rust is a good fit because Windows APIs can be accessed through Microsoft's Rust projections. Prefer a narrow set of Windows API features rather than enabling a huge surface by default. The current `windows-rs` guidance specifically recommends focused crates when they fit and using feature flags or generated bindings to reduce dependency weight.

References:

- [Rust for Windows — Microsoft Learn](https://learn.microsoft.com/en-us/windows/dev-environment/rust/rust-for-windows)
- [microsoft/windows-rs](https://github.com/microsoft/windows-rs)

### Runtime recommendations

- Use Tokio only where async concurrency is useful; do not create an unbounded task for every event.
- Use `serde` for typed configuration and IPC messages.
- Use `tracing` for structured diagnostics, with configurable log levels and rotation.
- Use SQLite for durable task state, history, and settings. Keep large screen captures and audio out of the database; store them as optional files with retention limits.
- Keep dependencies feature-gated and periodically inspect the dependency tree and binary size.

Avoid introducing a general-purpose agent framework into the Rust core during the MVP. A simple explicit state machine is easier to profile, test, and reason about.

## 4. Use protocols for integrations, not for everything

### MCP — tool connectivity

Use MCP when an external tool or service already exposes an MCP interface, or when you need J.E.A.N. to expose its own capabilities to compatible clients.

Candidate Rust SDK: [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk). It is the official Rust SDK and uses Tokio. Add only the client/server and transport features that J.E.A.N. needs.

### ACP — coding-agent connectivity

Use ACP as the preferred standardized path for coding agents that support it. It is designed for communication between clients and coding agents. Candidate Rust SDK: [agentclientprotocol/rust-sdk](https://github.com/agentclientprotocol/rust-sdk).

Still provide direct process/CLI adapters where an agent lacks a stable ACP interface. Do not assume every CLI supports the same lifecycle, interrupt, permissions, or session semantics.

### Local IPC

For J.E.A.N.-owned local components, prefer Windows named pipes or a similarly constrained local IPC mechanism. For spawned command-line tools, stdio is often simpler. Avoid exposing a local HTTP server to the network unless a real use case requires it; bind locally, authenticate, and validate requests if HTTP is used.

## 5. Windows integration: semantic first, pixels second

Use a layered control strategy:

1. **Application APIs first** — use a supported application API, CLI, or browser DOM/CDP when available.
2. **Windows UI Automation (UIA)** — inspect named controls, control types, enabled state, and supported patterns.
3. **Keyboard and mouse input** — use for controls that expose no useful API or accessibility interface.
4. **Visual recognition and OCR** — use as a fallback for canvas content, inaccessible controls, remote desktops, or other pixel-only interfaces.

This hierarchy is important. A button's UIA name and enabled state are usually more reliable than guessing which pixels represent the button.

Candidate projects to study or prototype against:

- [trycua/cua](https://github.com/trycua/cua) — Rust computer-use substrate with Windows support identified in the attached research.
- [microsoft/UFO](https://github.com/microsoft/UFO) — Windows-first desktop-agent architecture; useful reference for choosing between native application APIs and GUI actions.
- [CursorTouch/Windows-MCP](https://github.com/CursorTouch/Windows-MCP) — broad Windows MCP tool surface; study its permission and control concepts, but do not automatically make a Python runtime a core dependency.
- [microsoft/windows-rs](https://github.com/microsoft/windows-rs) — native Windows API bindings.
- [enigo-rs/enigo](https://github.com/enigo-rs/enigo) — candidate Rust input library; input injection alone does not provide UIA semantics or screen understanding.

### Recommended first prototype

Build a small Rust proof of concept that can:

- Enumerate top-level windows.
- Identify the foreground window and its process.
- Read a UIA tree for one supported desktop application.
- Find a control by accessible name and invoke it.
- Capture a screenshot only when requested.
- Report whether the action succeeded or why it could not be verified.

Do this before integrating a vision model.

## 6. Process and terminal orchestration

J.E.A.N. should treat every external command or agent as a supervised child process or managed connection.

Each execution record should include:

- Tool/agent identifier.
- Executable and arguments (with secrets redacted).
- Working directory.
- Environment overrides (allowlisted, not arbitrary inherited secrets).
- Start time and current state.
- Stdout/stderr stream references.
- Exit code or protocol completion status.
- Cancellation and timeout status.
- Permission level and approval record.

### Important Windows details

- Use standard process APIs for non-interactive commands.
- Use ConPTY when an interactive terminal session is required.
- Use Windows Job Objects to track and terminate child process trees reliably.
- Do not assume killing the parent process automatically stops all descendants.
- Do not assume a command returning exit code 0 proves the user-requested outcome. Run a verification step.

A task needs explicit states such as `queued`, `planning`, `awaiting_approval`, `running`, `verifying`, `succeeded`, `failed`, `cancelled`, and `needs_attention`.

## 7. Permissions and administrative operations

Do not run the entire J.E.A.N. process as Administrator by default.

Recommended model:

- Run the main orchestrator at normal user integrity.
- Classify tools/actions by risk and required permissions.
- Allow normal user-level actions under the user's configured policy.
- For an operation requiring elevation, launch a narrowly scoped helper through the Windows UAC consent flow.
- Show the exact operation, target, and reason for elevation.
- Record who approved it, what ran, and the result.
- Never store an administrator password or attempt to bypass UAC.
- Detect integrity-level restrictions (including UIPI) and report a clear blocker instead of silently retrying forever.

Keep destructive actions—permanent deletion, registry edits, service changes, credential access, bulk file changes—behind explicit confirmation or stricter policy.

## 8. Voice: an independent service

Voice should not be a dependency of the main task engine. A GUI automation task can take a long time or hang; the user must still be able to interrupt, ask a question, or hear status updates.

Recommended shape:

- A small voice process handles microphone capture, VAD, wake-word detection, STT, and TTS.
- It sends compact events to the core over local IPC.
- The core can reply with text/audio events without sharing the GUI worker's execution thread.
- Speech and task execution have separate cancellation scopes.
- Expensive models are loaded only when voice is enabled or used.

Candidate to evaluate: [k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx). The project reports local STT, TTS, VAD, keyword spotting, and Windows support with multiple language APIs including Rust. Confirm the exact model, license, binary distribution, and memory use before shipping.

Do not start with always-on speech recognition. First deliver push-to-talk or a hotkey-triggered voice interaction.

## 9. Screen understanding and vision

Do not run a vision model at a fixed high frame rate. Use an event-driven approach:

1. A task requests visual context.
2. Capture only the relevant window or region.
3. Compare with the previous capture or use a UI event to avoid redundant processing.
4. Try UIA and OCR first when sufficient.
5. Call a vision-language model only when semantic interpretation or coordinate grounding is necessary.
6. Discard or retain the capture according to the task's privacy and retention settings.

Candidate components from the research:

- [windows-capture](https://github.com/NiiightmareXD/windows-capture) or [DXcam](https://github.com/ra1nty/DXcam) for Windows capture.
- [RapidOCR](https://github.com/RapidAI/RapidOCR) for OCR.
- [OmniParser](https://github.com/microsoft/OmniParser) for screenshot parsing; carefully review the repository and weight licenses before redistribution.
- [UI-TARS Desktop](https://github.com/bytedance/UI-TARS-desktop) and [Agent-S](https://github.com/simular-ai/Agent-S) as computer-use references, not mandatory runtime dependencies.

Keep the image pipeline optional and independently restartable.

## 10. The AI model is a replaceable dependency

The core should speak to a provider-neutral model interface. The provider adapter should accept a request containing context, available tools, policy constraints, and output schema, and return a structured plan or tool request.

Provide at least:

- One hosted-model adapter.
- One optional local-model adapter.
- A deterministic fallback for basic commands that do not require an LLM.

Candidates for local inference include [llama.cpp](https://github.com/ggml-org/llama.cpp) and [Ollama](https://github.com/ollama/ollama). Do not embed either inference runtime in the always-running core. Communicate with it as a separate service.

Use deterministic code for permission checks, argument validation, path handling, retries, process supervision, task state, and verification. Do not delegate security decisions to the LLM.

## 11. Memory and task state

Start with one local SQLite database. Do not start with a vector database or knowledge graph.

Suggested tables:

- `tasks` — task identity, goal, state, timestamps.
- `task_steps` — ordered plan and step results.
- `tool_runs` — process/agent invocation details.
- `events` — append-only execution events.
- `approvals` — user approvals and denials.
- `tools` — discovered tool metadata and capability manifests.
- `preferences` — user-configurable behavior.

Keep the initial memory model straightforward: saved preferences, recent tasks, user-approved facts, and explicit project context. Add semantic search only when a measurable retrieval problem appears.

Potential future project to study: [Gentleman-Programming/engram](https://github.com/Gentleman-Programming/engram), a Go/SQLite/FTS5 memory tool. It is not necessary for the first version.

## 12. Build order: the recommended MVP

### Phase 0 — Architecture spike

**Goal:** prove that Rust can launch and supervise the local tools J.E.A.N. needs.

Deliver:

- A Rust executable that starts, logs, and exits cleanly.
- Tool registry loaded from a local config file.
- Launch a process with a working directory and explicit arguments.
- Stream stdout/stderr.
- Capture exit code.
- Cancel a process tree.
- Persist a task record in SQLite.

Exit criteria: the core can launch PowerShell, run a harmless command, collect its output, cancel a long-running child, and recover its task state after restart.

### Phase 1 — Useful local orchestrator

**Goal:** complete useful tasks without AI autonomy first.

Deliver:

- Text command input.
- A small set of deterministic intents (launch application, run command, inspect process, locate file).
- Tool registration and health checks.
- Approval policy and audit log.
- A compact task status view.

Exit criteria: each action has a clear start, output, status, and verifiable result.

### Phase 2 — Model-driven planning

**Goal:** use an LLM for intent understanding and tool selection, not for low-level execution policy.

Deliver:

- Provider-neutral LLM interface.
- Structured plan schema.
- Tool argument validation.
- Plan preview for sensitive or multi-step tasks.
- Tool result summarization.
- Failure recovery with bounded retries.

Exit criteria: the model can plan a workflow but cannot call tools outside the registry or override permission rules.

### Phase 3 — Coding-agent and browser coordination

**Goal:** show J.E.A.N.'s core value—coordination.

Deliver:

- One ACP coding-agent connector where supported.
- One direct CLI adapter for a tool that does not expose ACP.
- One browser integration using Playwright or a browser protocol.
- Progress events and cancellation.
- Test/result verification.

Exit criteria: ask J.E.A.N. to delegate a task, monitor the worker, inspect the result, run a test, and report what actually happened.

### Phase 4 — Windows UI and screen awareness

**Goal:** understand and operate the desktop reliably.

Deliver:

- Window enumeration and focus.
- UIA inspection and semantic control actions.
- On-demand screenshot and OCR.
- Pixel grounding fallback.
- Action verification and recovery.

Exit criteria: complete a controlled set of UI tasks in supported applications without relying only on fixed coordinates.

### Phase 5 — Voice and richer memory

**Goal:** make the orchestrator feel like a companion without making the core heavier.

Deliver:

- Push-to-talk.
- Independent voice sidecar.
- Speech interruption and task cancellation.
- TTS progress updates.
- Optional wake word.
- Long-term preferences and searchable task history.

Exit criteria: a user can speak a task, interrupt it, ask for an update, and resume or cancel it while a separate worker continues safely.

## 13. Suggested repository layout

```text
jean/
├── README.md
├── IMPLEMENTATION_GUIDE.md
├── Cargo.toml
├── crates/
│   ├── jean-core/             # orchestration and task state machine
│   ├── jean-protocol/         # typed IPC/events/tool contracts
│   ├── jean-tools/            # tool registry and capability manifests
│   ├── jean-process/          # child process and Job Object supervision
│   ├── jean-policy/           # permission policy and approvals
│   ├── jean-store/            # SQLite persistence
│   ├── jean-windows/          # Windows API, UIA, windows/processes
│   ├── jean-mcp/              # MCP client/server integration
│   └── jean-acp/              # coding-agent ACP integration
├── apps/
│   ├── jean-cli/              # early interface / diagnostics
│   └── jean-tray/             # lightweight desktop shell (later)
├── workers/
│   ├── voice/                 # independent voice process (later)
│   └── vision/                # optional screen/vision worker (later)
├── config/
│   ├── defaults.toml
│   └── tools.example.toml
├── tests/
│   ├── integration/
│   └── fixtures/
└── docs/
    ├── architecture.md
    ├── security.md
    └── decisions/
```

Do not create every crate immediately. Start with a small workspace and split modules when boundaries become real and useful.

## 14. Performance targets and measurement

Treat these as initial engineering goals, not guaranteed outcomes:

| Metric | Initial target |
|---|---:|
| Idle core memory | Aim for under 50 MB before optional workers |
| Idle CPU | Near zero when no events/tasks are active |
| Startup to command-ready | Under 2 seconds on the development machine |
| Core inference dependency | None required |
| Screen capture | On demand by default |
| Voice/vision models | Not loaded when disabled |
| Task status latency | Immediate local event updates; measure under load |

Measure with Windows Performance Monitor, Process Explorer, or `Get-Process`. Record the core process separately from its workers; do not claim J.E.A.N. is lightweight by measuring only its parent while ignoring child processes.

Track:

- Idle and peak working set.
- Private bytes.
- CPU use while idle and during a task.
- Startup time.
- Number of child processes.
- Model load time and peak RAM/VRAM.
- Event and IPC latency.

## 15. Testing strategy

Build tests around the orchestrator's contracts before adding autonomy:

- Unit tests for task state transitions.
- Unit tests for tool schema and argument validation.
- Policy tests for allow/deny/approval decisions.
- Process tests for timeout, cancellation, child cleanup, and output capture.
- Restart tests proving that task state is recoverable.
- Integration tests using harmless test executables and a mock tool.
- Windows UI tests against a small known test application.
- End-to-end tests for one tool chain at a time.

For every automated action, define how the expected result is verified. “The click API returned success” is not the same as “the intended operation completed.”

## 16. Decisions to avoid early

- Do not start by integrating ten coding agents. Start with one ACP-capable worker and one CLI fallback.
- Do not start with always-on voice. Start with text/hotkey input, then push-to-talk.
- Do not start with continuous screenshot streaming or a permanent VLM.
- Do not start with a vector database, graph database, Docker stack, or workflow server unless a demonstrated requirement needs one.
- Do not build a large Electron UI around a Rust engine. Start with CLI/tray and add a UI only when the interaction model is stable.
- Do not run the entire application elevated.
- Do not allow the model to construct and execute arbitrary shell commands without policy checks, argument validation, and user-visible audit events.
- Do not bind the product to OpenClaw. Study its orchestration and integration design, and make it an optional external capability if useful.

## 17. Recommended initial stack

| Layer | Recommendation |
|---|---|
| Core | Rust |
| Async runtime | Tokio, used selectively |
| Windows APIs | `windows-rs` with minimal required features |
| Persistence | SQLite |
| Serialization | Serde |
| Diagnostics | `tracing` |
| Tool protocol | Official Rust MCP SDK (`rmcp`) |
| Coding-agent protocol | Official Rust ACP SDK |
| Process execution | Rust `std::process` plus Windows Job Objects; ConPTY when interactive |
| GUI semantics | Native Windows UI Automation |
| Input fallback | Evaluate `enigo` or direct Win32 input APIs |
| Screen capture | Windows Graphics Capture / DXGI adapter |
| Voice worker | `sherpa-onnx` as a candidate, separate process |
| Local model server | Optional `llama.cpp` or Ollama process |
| Browser | Playwright / CDP adapter |

Official/repository references:

- [windows-rs](https://github.com/microsoft/windows-rs)
- [MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)
- [ACP Rust SDK](https://github.com/agentclientprotocol/rust-sdk)
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)
- [trycua/cua](https://github.com/trycua/cua)
- [OpenClaw](https://github.com/openclaw/openclaw)

The reference projects are candidates to inspect and selectively reuse. Check each repository's current license, release status, platform support, and dependency footprint before making it a shipped dependency.

---

## Final recommendation

Build **J.E.A.N. Core first**: a Rust process supervisor, task state machine, tool registry, permission layer, and verification system. Make it useful before adding voice or vision.

Then prove the differentiator with one complete workflow:

> User request → J.E.A.N. plans → launches a coding agent → monitors it → inspects output → runs tests → verifies the result → reports back.

Once that workflow is dependable and lightweight, add Windows UI Automation, screen understanding, and voice as optional capabilities.

**Protect the core from feature creep. J.E.A.N.'s superpower is orchestration.**
 
