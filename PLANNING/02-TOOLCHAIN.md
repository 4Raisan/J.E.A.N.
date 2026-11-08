# 02 — Toolchain

> **What tools we use.** Everything required to build, run, test and ship J.E.A.N. on Windows 11.
> All versions verified 2026-11-04 against crates.io, npm, PyPI, GitHub release APIs and official docs.

---

## 1. Build toolchain

| Tool | Version | Source | Notes |
|---|---|---|---|
| **Rust (stable)** | **1.99.1** (`58cf8f9c3 2026-10-13`) | `rustup` / static.rust-lang.org | Observed from `channel-rust-stable.toml` |
| **Cargo** | 0.100.0 | ships with Rust | — |
| **Project MSRV** | **1.90** | our choice | Floor set by `rusqlite` 1.89, `rmcp`/`agent-client-protocol` 1.88, `tray-icon`/`muda`/`tauri` 1.90. Track stable in CI. |
| **rustup toolchain** | `stable-x86_64-pc-windows-msvc` | rustup | MSVC, not GNU — required for `windows-rs` COM/UIA and ConPTY |
| **Visual Studio Build Tools 2022** | current | Microsoft | C++ workload. Supplies `link.exe` and the Windows SDK. rustup docs: the MSVC toolchain "requires an installation of Visual Studio". |
| **Windows SDK** | bundled with VS Build Tools 2022 | Microsoft | UIA, ConPTY and Job Objects are Win10 1809+ OS APIs and `windows-rs` ships its own metadata, so the bundled SDK suffices. *Exact minimum SDK build number: `UNVERIFIED`.* |
| **Git** | current | git-scm | Required by coding agents (worktree isolation) and by `cargo` git dependencies |
| **Node.js** | ≥ 22 | nodejs.org | **Only** for `playwright-mcp` and npm-distributed coding agents. Not a core dependency. |

### Install (Windows, PowerShell)

```powershell
# 1. Rust
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-msvc

# 2. MSVC + Windows SDK
winget install Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

# 3. Git
winget install Git.Git

# 4. Node (only if browser automation via playwright-mcp is enabled)
winget install OpenJS.NodeJS.LTS

# 5. Optional local model server
winget install Ollama.Ollama
```

### Verify

```powershell
rustc --version      # expect 1.99.1 or newer
cargo --version
rustup show          # expect stable-x86_64-pc-windows-msvc
link                 # MSVC linker must be on PATH inside a VS dev shell
```

> **Gotcha:** `link.exe` is only on `PATH` inside a *Developer PowerShell for VS 2022* (or after `vcvars64.bat`). Building `windows-rs` COM code from a plain shell fails at link time. Configure the IDE/terminal to use the dev shell.

**How this repository solves it** (`target/dev.cmd`, committed): every build, test and lint runs *through* it, so a plain Git-Bash shell gets a vcvars-initialised environment per invocation without anyone remembering to open a dev shell first.

```bash
cd "<repo>" && export MSYS2_ARG_CONV_EXCL='*' \
  && cmd.exe /c 'target\dev.cmd test --workspace --all-targets'
```

`MSYS2_ARG_CONV_EXCL='*'` stops Git Bash rewriting `/c`, `/d` and the other MSVC-style flags into Windows paths. The wrapper prints `[exit] N`; **read that line** — piping the output into `tail` hides a linker failure behind the last test name.

---

## 2. Development tooling (cargo subcommands)

| Tool | Version | Published | Purpose | Verdict |
|---|---|---|---|---|
| **cargo-nextest** | 0.9.151 | 2026-11-03 | Test runner. Parallel, better output, retries. | **ADOPT** — very active. **Not used in Phase 0 or 1**: the suite is run with `cargo test --workspace --all-targets` through `target\dev.cmd`, because several tests exercise process-global state (the console-control handler, the global `tracing` subscriber, the real console) and the suite is still fast enough that per-test process isolation buys nothing yet. Its `--no-capture` and per-test timing output are the reason to switch when Phase 4 starts driving UIA. |
| **cargo-deny** | 0.20.2 | 2026-07-09 | License + advisory + ban + source audit in CI. **This is how we enforce the "permissive licenses only" rule.** | **ADOPT** |
| **cargo-audit** | 0.22.2 | 2026-06-05 | RustSec advisory database check. | **ADOPT** (cargo-deny also covers advisories; keep one) |
| **cargo-bloat** | 0.12.1 | 2024-05-10 | Binary size analysis. Semi-stale but functional. | **ADOPT** — we have an explicit size budget |
| **cargo-tree** | built into cargo | — | Dependency graph inspection. Stable since 1.44. | **ADOPT** |
| **cargo-watch** | — | — | Rebuild on change | CANDIDATE — convenience only |

```powershell
cargo install cargo-nextest cargo-deny cargo-audit cargo-bloat --locked
```

### CI gates we commit to

| Gate | Command | Blocks merge when |
|---|---|---|
| Format | `cargo fmt --check` | any diff |
| Lint | `cargo clippy --all-targets -- -D warnings` | any warning |
| Test | `cargo nextest run --all-features` | any failure |
| License | `cargo deny check licenses bans sources` | any non-permissive license or banned crate |
| Advisory | `cargo deny check advisories` | any known vulnerability |
| **Size** | `cargo bloat --release --crates -n 20` | core binary regresses past budget |
| **Idle RSS** | harness measures `Get-Process` working set | core exceeds 50 MB idle |
| MSRV | `cargo +1.90 check` | MSRV floor broken |

> The size and RSS gates are not optional. "Lightweight" is a claim we measure, not a claim we make. Measure the core **and** its children — never report the parent alone.

---

## 3. Runtime components — what actually runs on the machine

### 3.1 The core (always running)

| Component | Implementation | Notes |
|---|---|---|
| Orchestrator | Rust binary `jean-core` | Task state machine, tool registry, policy engine, event routing |
| Persistence | SQLite via `rusqlite` **0.41.1** (`bundled` feature) | Vendored C — **no system SQLite dependency** |
| Local IPC | Phase 0: raw `windows` `Win32_System_Pipes` + `Win32_Security_Authorization`. Phase 2+: `tokio::net::windows::named_pipe` | Phase 0 needs a creator-owner-only DACL on the pipe; tokio's `ServerOptions` cannot set a `SECURITY_ATTRIBUTES`. No network listener at any phase |
| Windows APIs | `windows` crate **0.62.2** with minimal feature flags | See `03-DEPENDENCIES.md` for the exact feature list |
| Diagnostics | `tracing` + `tracing-appender` rolling file logs | Never `println!` in the core |
| Desktop shell | `tray-icon` **0.26.1** + `muda` **0.21.2** | **No window framework in v1.** ~2 MB binary, ~5–10 MB RSS |
| CLI | `clap` **4.6.7** | `jean-cli` for diagnostics and early interface |

### 3.2 Optional workers (loaded only when enabled)

| Worker | Process | Implementation | Loaded when |
|---|---|---|---|
| **Voice** | `jean-voice` | **Rust**, using the official `sherpa-onnx` crate **2.0.1** | `[voice] enabled = true` |
| **Vision / OCR** | `jean-vision` | Python sidecar (RapidOCR) *or* Rust `ocrs` / `ort` | `[vision] enabled = true` **and** a task requests visual context |
| **Local model server** | external | **Ollama v0.40.14** (primary) or `llama-server` from llama.cpp | `[model] provider = "ollama"` |
| **Elevated helper** | `jean-elevated` | Rust, narrowly scoped, launched via UAC | An operation requires elevation |

### 3.3 External tools the core supervises (never embedded)

| Category | Tool | How the core reaches it |
|---|---|---|
| Shells | PowerShell (`pwsh`), CMD | `std::process`-style anonymous pipes for one-shot; **ConPTY** via raw `windows` (`CreatePseudoConsole` + `STARTUPINFOEXW`, D-024) for interactive |
| Coding agents | Codex, Claude Code, OpenCode, Gemini CLI, Qwen Code, Goose | **ACP** over stdio (`agent-client-protocol` 5.0.0) |
| Browser | Chromium/Edge via Playwright | **`@playwright/mcp` 0.0.84** spawned as a stdio MCP child |
| Version control | Git | direct CLI; worktrees for parallel agents |
| Containers | Docker | direct CLI |
| Editors | VS Code | CLI (`code`) or UIA |
| Any MCP server | — | `rmcp` 3.8.0 client, `transport-child-process` |

---

## 4. Voice worker — concrete toolchain

**The biggest correction to the original guide:** `sherpa-onnx` has an **official Rust crate** (`sherpa-onnx` 2.0.1, Apache-2.0, sources at `sherpa-onnx/rust/`, examples at `rust-api-examples/`). The voice worker can be **Rust** — no Python or C++ sidecar required.

### Engine

| Item | Value |
|---|---|
| Crate | `sherpa-onnx = "2.0.1"` (+ `sherpa-onnx-sys`) |
| License (code) | **Apache-2.0** |
| Capabilities confirmed | streaming ASR, non-streaming ASR, TTS, VAD, keyword spotting |
| Linking | static (per `rust-api-examples/Cargo.toml`: `default-features = false` + static feature) |
| Prebuilt binaries also available | `sherpa-onnx-streaming-asr-x64-v2.0.1.exe`, `sherpa-onnx-non-streaming-tts-x64-v2.0.1.exe`, plus ~200 `win-x64`/`win-x86`/`win-arm64` lib tarballs and CUDA 12.x/13.x variants in release **v2.0.1** |

### Models (all from `k2-fsa/sherpa-onnx` GitHub releases)

| Purpose | Asset | Size | License |
|---|---|---|---|
| Streaming ASR (lean) | `sherpa-onnx-streaming-zipformer-en-20M-2023-02-17.tar.bz2` | **127.9 MB** | `UNVERIFIED` — not stated on release page; trained on LibriSpeech (CC BY 4.0 data) |
| Streaming ASR (accurate) | `sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2` | **310.4 MB** | `UNVERIFIED` — same caveat |
| VAD | `silero_vad_v5.onnx` | **2.3 MB** | Upstream `snakers4/silero-vad` repo is **MIT**. HF copy is gated — redistribute the sherpa release asset, and re-check terms before bundling. |
| TTS (lean) | `vits-piper-en_US-lessac-medium-int8.tar.bz2` | **21.0 MB** | Per-voice `MODEL_CARD` — `UNVERIFIED` (HF gated) |
| TTS (full) | `vits-piper-en_US-lessac-medium.tar.bz2` | **67.2 MB** | same |
| TTS (alt) | `matcha-icefall-en_US-ljspeech.tar.bz2` | **76.8 MB** | `UNVERIFIED` |
| Keyword spotting | `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2` | **17.6 MB** | `UNVERIFIED` |

**Total download: ~169 MB (lean profile) / ~415 MB (full-quality profile).**
Runtime RAM: **`UNVERIFIED`** — not benchmarked. Budget a few hundred MB for the streaming zipformer worker and measure before committing to a number.

> **License gate before shipping:** every model's `MODEL_CARD` must be read individually. The sherpa-onnx *code* is Apache-2.0; the *weights* are not uniformly licensed. Do not bundle a model whose card you have not read.
>
> **Avoid `TEN-VAD`** — non-SPDX custom license. Use Silero (MIT).

### Fallbacks

| If | Then |
|---|---|
| A sherpa ASR model license turns restrictive | `whisper.cpp` via `whisper-rs` 0.16.0 (MIT) — **batch only, no true streaming**, and it drags a C++/cmake build into Rust CI |
| Piper voices are unusable | `kokoro-onnx` 0.6.1 (PyPI, MIT code) as a Python TTS sidecar; `hexgrad/kokoro` weights license is `UNVERIFIED` (HF gated) |
| The Rust binding proves insufficient | Fall back to the prebuilt `sherpa-onnx-*-x64-v2.0.1.exe` binaries driven over stdio |

---

## 5. Local model serving — concrete toolchain

| Item | **Ollama (primary)** | **llama.cpp (alternative)** |
|---|---|---|
| Version | **v0.40.14** (2026-11-03) | rolling build **b12078** (2026-11-03); version tag v0.8.0 has no binaries |
| License | MIT | MIT |
| Windows distribution | `ollama-windows-amd64.zip`, `-arm64.zip`, `-rocm.zip`, `-mlx.zip` | `llama-b12078-bin-win-{cpu-x64, cpu-arm64, vulkan-x64, vulkan-arm64, cuda-12.4-x64, cuda-13.4-x64, rocm-10.0-x64, openvino-2026.4.1-x64, sycl-x64, opencl-adreno-arm64}.zip` |
| API the core calls | `POST http://127.0.0.1:11434/api/chat` with `{"model":…,"messages":[…],"stream":true}` → **NDJSON** lines | `POST /v1/chat/completions` (OpenAI-compatible) via `llama-server` |
| Other endpoints | `/api/generate`, `/api/embed`, `/api/tags`, `/api/ps`, `/api/pull` | OpenAI chat/completions/responses/embeddings + Anthropic Messages route + experimental MCP server |
| Requires | nothing but the binary | nothing but the binary |
| Docker / WSL | **No** | **No** |

**Decision:** Ollama is the shipped default (signed Windows zips, model manager, no build step). Keep an `openai_compatible` provider profile pointing at `llama-server` as the alternative. Both are plain HTTP — trivial from `reqwest`.

> Neither is embedded. Both are external processes the core talks to. If neither is running, J.E.A.N. still works via the deterministic fallback.

---

## 6. Coding-agent toolchain

All verified 2026-11-04. **ACP is the primary bus.**

| Agent | Version observed | License | Windows distribution | Mechanism |
|---|---|---|---|---|
| **Codex** | `rust-v0.166.0` (2026-10-29) | **Apache-2.0** | Native MSVC x64 + arm64; separate `codex-app-server-x86_64-pc-windows-msvc.exe` | ACP adapter, or `codex exec --json` (JSONL), or `codex app-server` (JSON-RPC stdio) |
| **Claude Code** | npm `@anthropic-ai/claude-code` **2.1.317** | **PROPRIETARY** — "© Anthropic PBC. All rights reserved" | npm | `claude -p --output-format stream-json`; ACP via `@agentclientprotocol/claude-agent-acp` **0.100.0** (Apache-2.0). Requires an Anthropic account. |
| **OpenCode** | **v1.19.5** (2026-10-28) | **MIT** | `opencode-windows-x64.zip`, `-x64-baseline`, `-arm64`, desktop `.exe` | Native `opencode acp`; or `opencode serve` (HTTP + OpenAPI 3.1) |
| **Gemini CLI** | npm **0.66.0** | Apache-2.0 | Node | Native `gemini --acp`; `--output-format text\|json\|stream-json`. Requires a Google account. |
| **Qwen Code** | npm **0.26.6** | Apache-2.0 | Node | `qwen serve` (HTTP daemon, `127.0.0.1:4170`, SSE + `Last-Event-ID`); ACP flag is **`--experimental-acp`** — *not* `--acp` |
| **Goose** | **v1.57.0** (2026-10-28) | Apache-2.0 | `goose-x86_64-pc-windows-msvc.zip`, `goose-acp-x86_64-pc-windows-msvc.zip` | Native `goose acp` |
| **Aider** | last push 2026-05-22 | Apache-2.0 | pip | **REJECTED** — no `--json`, no `--output-format`, no MCP, no ACP. PTY-scraping only. |

### ACP protocol facts (verified from the schema source)

| Fact | Value |
|---|---|
| Spec repo | `agentclientprotocol/agent-client-protocol` — Apache-2.0, Rust, pushed 2026-11-03 |
| **Stable protocol version** | **V1** (`ProtocolVersion::LATEST = V1`). V2 is a draft behind `unstable_protocol_v2`. |
| Rust crate | **`agent-client-protocol` 5.0.0** (2026-11-02, Apache-2.0, 5.87M downloads) |
| Roles | Client, Agent, **Proxy**, Conductor. J.E.A.N. speaks ACP as **Client** (possibly Proxy). |
| Methods | `initialize`, `authenticate`, `auth/status`, `logout`, `session/new\|load\|fork\|resume\|close\|status\|list\|delete\|archive\|set_title\|set_mode\|prompt\|cancel\|update`, `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/create\|output\|release\|kill\|wait_for_exit`, `mcp/message`, `notifications/progress`, `$/cancel_request` |
| **`session/stop`** | **DOES NOT EXIST.** Interruption is `session/cancel`. Teardown is a **process kill**. `session/close` is in the v1 schema but agent-side support varies. |
| Companion crates | `agent-client-protocol-http` (HTTP/SSE + WS), **`agent-client-protocol-rmcp`** (ACP↔MCP bridge — directly useful to us), `agent-client-protocol-conductor` |

> **Plan around this:** every agent connector needs a process-handle kill path. Do not design a "stop the agent" feature that assumes a protocol method.

---

## 7. Browser automation toolchain

| Option | Version | License | Requires | Verdict |
|---|---|---|---|---|
| **`@playwright/mcp`** | **0.0.84** | Apache-2.0 | Node.js; `npx @playwright/mcp@latest` | **ADOPT** — spawned as a stdio MCP child. Uses Playwright's accessibility tree, not pixels. |
| Playwright (npm) | 1.64.1 | Apache-2.0 | Node | Pulled in by the MCP server |
| `thirtyfour` (Rust, WebDriver) | 0.38.1 (2026-10-21) | `UNVERIFIED` | chromedriver binary | CANDIDATE — most actively maintained pure-Rust option |
| `headless_chrome` (Rust, CDP) | 1.0.22 (2026-06-11) | `UNVERIFIED` | Chrome/Edge (Edge ships with Win11) | CANDIDATE — pure Rust, no Node |
| `chromiumoxide` (Rust, CDP) | 0.9.1 (2026-02-25) | `UNVERIFIED` | Chrome/Edge | CANDIDATE |
| `playwright` (Rust crate) | 0.0.20, **last updated 2022-08-20** | — | — | **REJECTED — dead. There is no maintained Rust Playwright client.** |

**Decision:** `@playwright/mcp` over stdio is the primary path (matches our MCP investment, uses the a11y tree, no Rust binding needed). `headless_chrome` is the no-Node fallback — Windows 11 ships Edge, so CDP is available without installing a browser.

---

## 8. Vision / OCR toolchain (optional, on demand)

| Component | License (code) | License (weights) | Path | Verdict |
|---|---|---|---|---|
| **RapidOCR** (`RapidAI/RapidOCR`, 8.4k★) | Apache-2.0 | Apache-2.0 per project; **per-file `UNVERIFIED`** | Python sidecar (`rapidocr-onnxruntime` 1.4.4, Apache-2.0). A C++ ONNX sibling `RapidAI/RapidOcrOnnx` exists (Apache-2.0). | **ADOPT as sidecar** |
| `ocrs` (Rust, pure) | MIT OR Apache-2.0 | — | 0.13.1 (2026-09-13), 1.9k★, repo pushed 2026-11-01 | CANDIDATE — accurate on clean English, **weak on UI screenshots and CJK** |
| `ort` (ONNX Runtime for Rust) | MIT OR Apache-2.0 | — | **2.0.0-rc.15** — pre-release, no stable on crates.io, 24.7M downloads | CANDIDATE — real work, rides an RC |
| `leptess` (Tesseract) | MIT | — | 0.14.0, **2023-02-21**, needs Tesseract C lib | **REJECTED — stale** |
| **OmniParser** (`microsoft/OmniParser`) | **repo CC-BY-4.0** | **v2.0 weights MIT**; **v1 icon detector has Ultralytics YOLO → AGPL-3.0 lineage** | Python sidecar | **v2 weights only.** Never the v1 detector. |
| **UI-TARS-1.5-7B** | — | **Apache-2.0** (HF confirmed) | GGUF: `mradermacher/UI-TARS-1.5-7B-GGUF` (20.6k downloads), `Lucy-in-the-Sky/…-{Q4_K_M,Q6_K,Q8_0}-GGUF` | **Best-licensed grounding option.** 7B — strictly on demand. |
| **ShowUI-2B** | — | **MIT** (HF confirmed) | GGUF: `localattention/ShowUI-2B-Q4_K_M-GGUF` (MIT tag) | **ADOPT as the light grounding model** — runs in llama.cpp on CPU/Vulkan |
| **Jedi-3B-1080p** (`xlang-ai/OSWorld-G`) | — | Upstream HF **gated (401) → `UNVERIFIED`**. Quantizers tag their GGUFs `apache-2.0`, which is **not proof of the upstream license**. | `mradermacher/Jedi-3B-1080p-GGUF`, `bartowski/xlangai_Jedi-7B-1080p-GGUF` | **BLOCKED pending license due diligence.** Do not ship. |

**Order of operations (rule R1):** try UIA text patterns → then OCR → then a grounding VLM. Never start at the VLM.

---

## 9. Repository and workspace layout

Per `../IMPLEMENTATION_GUIDE.md` §13, with the planning folder added:

```text
jean/
├── README.md                     # product vision (root, technical)
├── IMPLEMENTATION_GUIDE.md       # engineering direction (root, technical)
├── RESEARCH.md                   # ecosystem survey (root, technical)
├── PLANNING/                     # ← THIS FOLDER. Planning only, no code, no specs.
│   ├── README.md
│   ├── 01-SCOPE.md
│   ├── 02-TOOLCHAIN.md
│   ├── 03-DEPENDENCIES.md
│   ├── 04-SETTINGS.md
│   ├── 05-ROADMAP.md
│   ├── 06-RISKS.md
│   └── 07-DECISIONS.md
├── Cargo.toml                    # workspace root
├── deny.toml                     # cargo-deny: license + ban policy
├── rust-toolchain.toml           # pins the MSRV floor
├── crates/
│   ├── jean-core/                # orchestration + task state machine
│   ├── jean-protocol/            # typed IPC / events / tool contracts
│   ├── jean-tools/               # tool registry + capability manifests
│   ├── jean-process/             # child process + Job Object supervision + ConPTY
│   ├── jean-policy/              # permission policy, approvals, UAC broker, audit
│   ├── jean-store/               # SQLite persistence + migrations
│   ├── jean-windows/             # windows-rs FFI: UIA, windows, processes, input
│   ├── jean-mcp/                 # rmcp client/server integration
│   └── jean-acp/                 # agent-client-protocol integration
├── apps/
│   ├── jean-cli/                 # early interface + diagnostics
│   └── jean-tray/                # tray-icon + muda shell (later)
├── workers/
│   ├── voice/                    # jean-voice (Rust + sherpa-onnx)
│   └── vision/                   # jean-vision (Python RapidOCR sidecar)
├── config/
│   ├── defaults.toml             # see 04-SETTINGS.md
│   ├── tools.example.toml
│   ├── mcp.example.toml
│   └── agents.example.toml
├── tests/
│   ├── integration/
│   └── fixtures/                 # includes a known Win32 test app for UIA tests
└── docs/                         # technical specs, ADR-adjacent detail
    ├── architecture.md
    ├── security.md
    └── decisions/                # long-form; short-form log lives in PLANNING/07
```

**Do not create every crate on day one.** Start with `jean-core`, `jean-protocol`, `jean-process`, `jean-store`. Split the rest when the boundaries become real. *(See `05-ROADMAP.md` Phase 0.)*

---

## 10. What we deliberately do NOT use

| Not using | Why |
|---|---|
| **Electron** | A Rust engine does not need a bundled Chromium. Tray-first. |
| **Tauri (in the core)** | WebView2's process tree puts RSS at 80–200 MB — **blows the 50 MB idle budget**. Revisit only as a separate optional UI binary. |
| **Docker / WSL** | Nothing in this stack requires either. A desktop companion that needs Docker is not a desktop companion. |
| **`winapi` crate** | Frozen since 2020-06-26. `windows` (windows-rs) supersedes it. |
| **Python in the core** | Python appears only in the optional vision sidecar. |
| **A general-purpose agent framework in the Rust core** | An explicit state machine is easier to profile, test and reason about. |
| **A vector database** | One SQLite file. Semantic search only when a measured retrieval problem appears. |
