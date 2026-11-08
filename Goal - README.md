# J.E.A.N. — The AI Companion

> **Give JEAN the goal. JEAN handles the workflow.**

J.E.A.N. is a lightweight, local-first AI desktop orchestrator for Windows. It is designed to understand what the user wants, select the right tools, coordinate their execution, and return a verified result.

J.E.A.N. is **not intended to be another coding agent, browser, terminal, or all-in-one AI application**. It is the layer that connects and orchestrates the applications and agents already available on the computer.

The central product is the **orchestration engine**. Voice, screen understanding, GUI automation, coding agents, browsers, terminals, and other tools are capabilities that J.E.A.N. can call when needed—not workloads that must all run inside its core.

The planned owner experience is a short goal with optional direction such as “Start with Codex.” Configure available agents and model budgets once. JEAN writes the prompts, delegates work, checks the results, and recovers from routine failures. It adapts the depth and polish of its work using your preferences and each project's standards. See the [owner workflow and personalization milestone](PLANNING/08-OWNER-WORKFLOW.md).

---

## The idea

The [current delivery plan](PLANNING/09-LUNA-EXECUTION-PLAN.md) starts with a short goal, supervised delegation and independent verification. Typed and Wispr-dictated goals can exercise that workflow while native listening quality is evaluated on the owner's speech. JEAN keeps speech output and recognition independently replaceable.

A user should be able to tell J.E.A.N. what outcome they need, rather than manually deciding which application, agent, terminal, or automation tool to use.

For example:

- “Open this project in VS Code and find why the tests are failing.”
- “Ask Codex to investigate the error, monitor its progress, and show me the result.”
- “Open Brave, find the relevant documentation, and summarize the important parts.”
- “Check what is using my memory and explain what can safely be closed.”
- “Run the setup commands for this project and verify that the environment works.”
- “Look at the active window and help me complete this workflow.”

J.E.A.N. interprets the request, gathers the necessary context, plans the work, chooses the appropriate tools, supervises execution, handles failures, and verifies the result.

**J.E.A.N. does not have to do every job itself. It has to know how to use the right tool for each job.**

## What J.E.A.N. should look like

J.E.A.N. should feel like a small, always-available desktop companion—not a heavyweight AI workstation.

- A lightweight background process with a tray icon and a quick way to talk to it.
- A voice interface for natural, hands-free interaction when enabled.
- A compact activity view showing what J.E.A.N. is doing, which tools are running, and what needs approval.
- Screen awareness that can be requested when relevant, rather than an expensive vision model watching the desktop continuously.
- Connections to existing applications and agents, including VS Code, PowerShell, CMD, Brave, Codex, Claude Code, OpenCode, Git, Docker, and other compatible tools.
- A clear task history showing actions, outputs, errors, and verification results.

The interface should expose the orchestration process without becoming the main product. The engine and its integrations are the priority.

## Core principles

### 1. Orchestration over reinvention

Reuse existing applications and specialist tools. J.E.A.N. should launch, communicate with, monitor, and coordinate them instead of rebuilding their functionality.

### 2. Lightweight by design

The core should remain small and responsive while idle. Heavy tasks such as speech recognition, text-to-speech, vision inference, local LLM inference, browsers, and coding agents should run only when needed and preferably in separate processes.

### 3. Local-first, provider-independent

J.E.A.N.'s orchestration and computer integration should run locally. Reasoning can use a configured cloud model or an optional local model. No single AI provider should be essential to the architecture.

### 4. Modular and replaceable

Voice, vision, desktop control, coding-agent connectors, and memory should have clear interfaces. A subsystem should be replaceable without rewriting the orchestrator.

### 5. Observable and verifiable

J.E.A.N. should know which task is running, what tool is executing it, what happened, and whether the requested outcome was actually achieved. It must not report success just because a command was launched.

### 6. Powerful, but permission-aware

J.E.A.N. can use broad computer capabilities, but it should not silently bypass Windows security. Administrative operations, destructive actions, credential use, and sensitive data access must be governed by explicit policies and user authorization.

---

## Main capabilities

| Capability | J.E.A.N.'s responsibility |
|---|---|
| Conversational interface | Accept text and voice instructions; keep the user informed |
| Task orchestration | Break requests into steps, choose tools, coordinate work, recover, and verify |
| Tool discovery | Find available applications, commands, agents, MCP servers, and integrations |
| Windows integration | Launch and inspect applications, manage processes, files, windows, and system information |
| Terminal operations | Start and supervise PowerShell, CMD, and other command-line tools |
| GUI interaction | Read accessible UI structures and interact with controls; use visual fallback when necessary |
| Screen awareness | Capture and understand a screen or window when a task needs it |
| Browser integration | Prefer browser-native automation and APIs over fragile coordinate clicking |
| Coding-agent integration | Delegate programming tasks to Codex, Claude Code, OpenCode, and compatible agents |
| Memory and context | Retain relevant preferences, task state, and execution history locally |
| Permissions | Apply policy, request approval, handle UAC, and maintain an audit trail |

These are long-term capability areas. They are not a promise that every feature exists in the first release.

## Architecture at a glance

```text
                         USER
                           |
                   Text / Voice / UI
                           |
                 +-------------------+
                 |    J.E.A.N. CORE  |
                 |       Rust        |
                 +-------------------+
                           |
             Intent, context and task planning
                           |
                 +-------------------+
                 |  ORCHESTRATION    |
                 |  Task state       |
                 |  Tool registry    |
                 |  Event routing    |
                 |  Verification    |
                 +-------------------+
                           |
                  Policy / Permission
                           |
        +------------------+------------------+
        |                  |                  |
   WINDOWS TOOLS       AI AGENTS         OPTIONAL SERVICES
        |                  |                  |
   UI Automation       Codex              Voice engine
   Screen capture      Claude Code        Vision models
   Processes           OpenCode           Local LLM server
   Files / Shell       Other CLIs         Browser automation
   Browser control     MCP / ACP tools    Memory extensions
        |                  |                  |
        +------------------+------------------+
                           |
                  Result verification
                           |
                    USER RESPONSE
```

The core coordinates the work. Specialist engines do the work. Expensive inference does not need to live inside the core process.

## Technology direction

### Core language: Rust

Rust is the preferred implementation language for the always-running engine because it offers native execution, memory safety, efficient concurrency, and access to Windows APIs. It is a practical low-level choice without requiring the entire project to be written in C or C++.

### Initial technology candidates

| Area | Initial direction | Notes |
|---|---|---|
| Core runtime | Rust | Keep the core dependency-light and feature-oriented |
| Async work | Tokio or a carefully scoped async/event loop | Use bounded tasks and avoid uncontrolled background work |
| Windows APIs | `windows-rs` / focused Windows bindings | Enable only required API features |
| Persistence | SQLite | Store task state, settings, and event history locally |
| Tool integration | MCP | Connect external tools and local capability servers |
| Coding-agent integration | ACP and process/CLI adapters | Treat coding agents as external workers |
| Local IPC | Windows named pipes; stdio for child tools | Keep communication local and typed |
| Screen capture | Windows Graphics Capture / DXGI | Capture on demand, not continuously by default |
| GUI semantics | Windows UI Automation (UIA) | Prefer named controls and accessibility data over raw coordinates |
| Voice | Optional sidecar, initially `sherpa-onnx` candidate | Do not put audio/model runtime in the core |
| Local model serving | Optional `llama.cpp` or Ollama integration | External service; not required for core operation |
| Browser automation | Playwright or browser-specific adapters | Use DOM/CDP when available; visual control as fallback |

These are candidate choices, not fixed dependencies. The implementation guide explains how to validate each one before committing to it.

## Development roadmap

### Stage 1 — Minimal orchestration core

- Rust application lifecycle and configuration.
- Structured task model and event log.
- Tool registry and capability descriptions.
- Process launching and supervision.
- Cancellation, timeout, and result collection.
- Minimal CLI or tray interface.

### Stage 2 — Useful Windows companion

- Application and process discovery.
- File and directory operations.
- PowerShell and CMD execution.
- Window enumeration and basic window management.
- Permission and confirmation policies.

### Stage 3 — External tool orchestration

- MCP client/server support where useful.
- Coding-agent adapters, prioritizing ACP where supported.
- Browser automation integration.
- Task monitoring and result verification.

### Stage 4 — Desktop awareness and interaction

- Windows UI Automation integration.
- On-demand screen capture.
- OCR and visual grounding fallback.
- Reliable action confirmation and recovery.

### Stage 5 — Voice and advanced intelligence

- Separate voice process for wake word, VAD, STT, and TTS.
- Conversational task planning.
- Optional local model support.
- Persistent memory and multi-step workflows.

## Non-goals

J.E.A.N. is not initially intended to:

- Replace Codex, Claude Code, or other specialist coding agents.
- Train or host large AI models inside its core process.
- Reimplement a full browser, IDE, terminal, or file manager.
- Run an always-on vision model over every screen frame.
- Depend on one cloud provider or one model family.
- Automatically perform every privileged or destructive action without user awareness.

## Project status

**Status: Concept / architecture and research phase.**

The project vision and candidate architecture are defined. Implementation choices should be validated with small prototypes and measured benchmarks before the system grows. There is no claim that the planned capabilities are already implemented.

## Guiding idea

> J.E.A.N. does not need to be the best at every task. It needs to understand the task, choose the best available tool, coordinate the work, and make sure the result is correct.

**Give JEAN the goal. JEAN handles the workflow.**
