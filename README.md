# J.E.A.N. — The AI Companion

> **Give J.E.A.N. the goal. J.E.A.N. remembers, plans, executes, and verifies the result.**

**J.E.A.N.** is a Windows-native, local-first AI desktop companion and task orchestrator inspired by **J.A.R.V.I.S.** — combining continuous voice interaction, self-updating local memory with offline & deep dreaming consolidation, procedural playbooks for known tasks, and an autonomous strategy solver for unfamiliar tasks — built on a strict Rust supervision, policy, and verification core.

---

## Key Capabilities

### 1. Hands-Free Voice & J.A.R.V.I.S. Desktop Overlay
- **Reactor Overlay (`Open JEAN Desktop.cmd` / `tools/jean-desktop/jean-desktop.py`)**: Always-on-top PyQt6 audio-reactive orb + compact command card with **Smart / Task / Chat** modes, activity log, audio device selectors (built-in, Bluetooth, USB), and Windows notification tray (`jean-tray`).
- **Continuous Voice Loop**:
  - Local wake-word listening (**"Jean"**, **"Hey Jean"**) that keeps the conversation active across follow-up turns, retries once on a missed utterance, and returns cleanly to standby on natural sign-offs (**"Thank you"**, **"That's all"**, **"Stand by"**, **"Stop Jean"**).
  - Global push-to-talk hotkey (`Shift + Caps Lock`) and spoken task controls (**"task status"**, **"cancel task"**, **"task approved"**, **"deny task"**, **"Jean terminate"**).
  - Pluggable listening backends: **Gemini Live** (streaming PCM over WebSocket), **Faster-Whisper**, **Zipformer + Parakeet hybrid**, and **Windows Speech** / **Sherpa ONNX** (`jean-voice` worker).
  - Natural spoken replies (`jean speak --task-result`) that strip CLI boilerplate and read back memory recalls, dream reports, workflow summaries, and verified task outcomes concisely.
- **Rolling Conversation Context & Smart Escalation**:
  - `jean chat` automatically receives rolling conversation history (`--context`) and relevant local memories.
  - In **Smart** mode, if a conversational request actually requires desktop action, J.E.A.N. automatically escalates it once into the policy-gated task pipeline instead of asking you to switch modes manually.

### 2. Memory, Dreaming & Self-Updating Memory (SQLite WAL + FTS5)
All memory lives locally in `%LOCALAPPDATA%\JEAN\jean.db` (or `JEAN_DATA_DIR`), protected by a credential-rejection floor and `jean_redact` FTS5 triggers. **Retrieved memory is always reference data, never execution authority.**

- **Explicit Operator Facts (`saved_facts`)**:
  - Save, search, list, update, or forget explicit facts by voice (*"Remember that my project uses Rust"*, *"What do you remember about Rust?"*, *"List memories"*) or CLI (`jean memory add | search | list | update | forget`).
- **Episodic Memory (`memory_episodes`)**:
  - Automatically captures bounded, redacted observations from conversations, task successes, task failures, and operator corrections.
- **Self-Updating Semantic Memory (`memory_notes`)**:
  - Automatically extracts durable facts, preferences, and corrections (e.g., *"My editor is VS Code"* → *"Actually, my editor is Cursor"*), reinforcing repeated facts and **superseding** contradictory notes on the same `topic_key` while keeping full lineage.
  - Automatically injected into `jean chat` and `jean plan` / `jean do` prompts as bounded, untrusted reference context (`jean memory recall`).
- **Dreaming / Memory Consolidation (`dream_cycles`)**:
  - **Offline deterministic dreaming** (`jean memory dream` or say **"Dream now"** / **"Consolidate memories"**): harvests recent settled tasks and unconsolidated episodes, updates/supersedes preferences, synthesizes lessons from recurring failures, gently decays and archives untouched stale notes, promotes repeated verified task patterns into reusable Playbooks, and verifies FTS5 index consistency.
  - **Model-assisted deep dreaming** (`jean memory dream --deep` or say **"Deep dream"**): performs a read-only JSON synthesis pass over redacted episodes and active notes with the configured main model while still enforcing redaction and credential rejection on every output.
  - **Automatic idle dreaming**: the desktop overlay runs a lightweight offline dream cycle in the background when completely idle after accumulating new activity.

### 3. Known Tasks (Playbooks) vs. New Tasks (Autonomous Solver)
- **Known Tasks — Procedural Playbooks (`playbooks` + `playbook_runs`)**:
  - Teach reusable workflows by voice/text (*"Remember workflow morning as check my mail; what time is it"*, *"Remember workflow clips as search {query} on YouTube"*) or CLI (`jean playbook teach`).
  - J.E.A.N. matches incoming requests against saved and learned playbooks by **exact name**, **exact trigger**, **parameterized trigger templates** (`{query}`, `{url}`), and **high-confidence FTS5 recall** over previously verified skills.
  - Replaying a playbook **re-resolves targets live** and passes every step through JEAN's real policy engine, audit log, and independent verification — recording `verified_success`, `failed`, `uncertain`, or `cancelled` separately, with instant version rollback (`jean playbook undo <name>`).
- **New Tasks — Autonomous Problem-Solving (`crates/jean-core/src/solver.rs`)**:
  - When asked to perform a task it has **never done before**, J.E.A.N. inspects available tools, workspace context, desktop/browser settings, coding workers, and recalled memory lessons to pick a grounded execution strategy:
    1. **Direct verified probes & registered tools** (top CPU/RAM processes, disk space, Git status, Windows version, file search) — works even offline.
    2. **Bounded Windows UI Automation (`desktop-goal`)** for operating open or launched desktop apps (supports `Invoke`, `SetValue`, `Toggle`, `Select`, `Expand`, `Collapse`, `SetFocus`, redacted witness verification, and password length-only readback).
    3. **Bounded Workspace Tasks (`workspace-goal`)** for inspecting, summarizing, or creating top-level workspace files with mandatory read-back verification and rollback on failed writes.
    4. **Controlled Browser Automation (`jean-browser`)** using an isolated profile, origin allowlist revalidation after navigation/clicks, and IPv6-safe URL parsing.
    5. **Coding-Agent Delegation (`jean delegate`)** via ACP / CLI workers (e.g., Codex, OpenCode) with git worktree isolation, baseline + post-run independent verification, and review/apply/discard patch gating.
    6. **Multi-step Structured Planning (`jean plan` / `jean do`)** enriched with recalled memory lessons.
    7. **Focused Clarification** when a critical target is genuinely missing.
  - **Automatic Skill Capture**: when J.E.A.N. solves a new task and independently verifies success, it automatically captures the verified recipe into `playbooks` so next time it runs as a **Known Task**.

### 4. Security, Policy & Verification Boundaries
- **Local-first, zero network listeners**: task cancellation uses Windows named pipes with a per-user SID protected DACL (`D:P(A;;FA;;;<user SID>)`).
- **Model output never grants authority**: policy evaluation (`jean-policy`) has no dependency on `jean-model`; explicit approval is preserved for workspace and desktop observations even when YOLO mode is toggled.
- **Process containment**: supervised children run inside Windows Job Objects with `KILL_ON_JOB_CLOSE`, allowlisted environments (model API keys are isolated and stripped from unrelated subprocesses), bounded stdout/stderr capture, and crash reconciliation.
- **Three-valued honest verification**: every act concludes as `Proven`, `NotProven`, or `Unverifiable` (exit codes `0 / 1 / 3 / 4 / 5 / 130`).
- **Recoverable Workspace Trash**: explicit file/folder deletes route to a safe workspace trash store with cross-drive copy verification, read-only purge handling, and non-overwriting restore.

---

## Workspace Architecture

| Member | Role |
|---|---|
| `apps/jean-cli` | `jean` CLI, desktop request bridge, `memory`, `playbook`, `chat`, `plan`, `do`, `desktop`, `workspace-goal`, `delegate`, `speak` |
| `apps/jean-tray` | Windows notification-area tray icon with Win32 event loop and persistent status console |
| `apps/jean-elevated` | Reserved elevation refusal stub (runtime elevation is unsupported by design) |
| `workers/voice` | `jean-voice` out-of-process speech worker (Fixture, Windows SAPI, optional Sherpa ONNX) |
| `crates/jean-core` | Orchestrator core: config, intents, planner, plan runner, memory/dreaming (`dream.rs`), playbooks (`playbooks.rs`), new-task solver (`solver.rs`), delegation, MCP, browser, desktop |
| `crates/jean-store` | SQLite WAL persistence, append-only migrations (v1–v6), FTS5 search, episodic/semantic/dream/playbook storage |
| `crates/jean-policy` | Tiers, capability rules, run grants, audit JSONL writer |
| `crates/jean-process` | Windows Job Objects, ConPTY, bounded pipe capture, cancellation |
| `crates/jean-windows` | Windows UI Automation, window discovery, capture, witness readback, `jean-desktop-fixture` |
| `crates/jean-browser` | Chrome/Edge CDP controller with isolated profile & origin allowlist enforcement |
| `crates/jean-acp` / `crates/jean-mcp` | Coding-agent ACP/CLI connectors and bidirectional MCP stdio support |
| `crates/jean-model` | Provider-neutral Ollama & OpenAI-compatible HTTP/TLS client |
| `crates/jean-protocol` / `crates/jean-tools` | Shared schemas, redactor, 9-state machine, app discovery, process & file probes |
| `tools/jean-desktop` | PyQt6 J.A.R.V.I.S. overlay, audio metering/selectors, Gemini Live streaming, workspace trash, regression suite |

---

## Quickstart & CLI Examples

### Launch the Desktop Companion
Double-click **`Open JEAN Desktop.cmd`** (or run from PowerShell):
```powershell
py -3 tools/jean-desktop/jean-desktop.py
```

### Useful CLI Commands
```powershell
# Inspect commands & model status
.\target\debug\jean.exe --help
.\target\debug\jean.exe model status

# Memory, self-updating & dreaming
.\target\debug\jean.exe memory add "My primary language is Rust"
.\target\debug\jean.exe memory observe "Actually, my editor is Cursor"
.\target\debug\jean.exe memory recall "editor" --spoken
.\target\debug\jean.exe memory notes
.\target\debug\jean.exe memory dream
.\target\debug\jean.exe memory dream --deep
.\target\debug\jean.exe memory dream-log

# Known tasks (playbooks)
.\target\debug\jean.exe playbook list
.\target\debug\jean.exe playbook show <name>
.\target\debug\jean.exe playbook match "check git status"
.\target\debug\jean.exe playbook run <name> --param query="Iron Man"
.\target\debug\jean.exe playbook undo <name>

# Conversation & planning
.\target\debug\jean.exe chat "What editor do I prefer?"
.\target\debug\jean.exe plan "Check git status and list top memory processes"
.\target\debug\jean.exe do "Check git status"

# History & audit
.\target\debug\jean.exe tasks
.\target\debug\jean.exe history search "git"
.\target\debug\jean.exe history rebuild
```

---

## Build & Test on Windows

Requires **Rust 1.90+**, **Visual Studio 2022 C++ Build Tools + Windows 10/11 SDK**, and **Python 3.12+** (with `PyQt6` for the desktop overlay and `uv` for isolated microphone capture).

Run from an x64 Visual Studio developer shell (or via `target\dev.cmd` which initializes `vcvars64.bat`):

```powershell
# Build core binaries
.\target\dev.cmd build --bin jean --bin jean-tray --bin jean-voice

# Run the entire Rust workspace test suite (unit + integration + live UIA fixture)
.\target\dev.cmd test --workspace --locked

# Check optional Sherpa voice feature
$env:DOCS_RS="1"; .\target\dev.cmd check -p jean-voice --features sherpa

# Run the Python desktop overlay & audio/trash/JARVIS regression suite
py -3 -m unittest -v tools/jean-desktop/test_desktop_regressions.py
```

---

## Further Documentation

- [Implementation Status](IMPLEMENTATION_STATUS.md) — current baseline and historical verification records
- [Architecture Overview](docs/architecture.md) — subsystem boundaries, storage schema, and budgets
- [Desktop Learning & Playbooks](docs/desktop-learning-plan.md) — known tasks, new task learning, and evaluation design
- [Security Model](docs/security.md) — local-first, policy, redaction, UIPI, and browser invariants
- [Decision Model](docs/decision-model.md) — shadow/active replacement-worker selection
- [Goal & Vision](Goal%20-%20README.md) · [Implementation Guide](IMPLEMENTATION_GUIDE.md) · [Planning Index](PLANNING/README.md) · [Decision Benchmarks](tools/decision-bench/)
