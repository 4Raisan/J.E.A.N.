# Deep Research Report — Open-Source AI Desktop Operator & Autonomous Computer Assistant

**Research date:** 2026-11-04
**Verification:** every repository URL, star count, license and last-push date in this report was confirmed by a live `gh api repos/OWNER/REPO` call on the research date. Repos that 404, are archived, or that were re-homed are listed in [Appendix A](#appendix-a--killed-and-renamed-candidates) with their current canonical path. Where a compatibility claim could not be confirmed from source or docs, it is marked **UNVERIFIED** rather than asserted.

**Target:** Windows 11 first, without needlessly sacrificing cross-platform.

---

## Part 1 — Executive Overview

### The short version

Your vision is **realistically achievable today, and it is ~80% built already** — but not by the project the ecosystem's own README files point you at.

The decisive finding: **[`openclaw/openclaw`](https://github.com/openclaw/openclaw)** (MIT, **393,843★**, 82,781 forks, pushed 2026-11-03) is a single project that ships a local-first assistant gateway, native desktop apps for macOS/iOS/Android/**Windows**/Linux, **speech-to-text and text-to-speech on the Windows node**, screen capture, browser control, shell execution, an **MCP server *and* client**, an **ACP client that drives external coding agents** (Copilot CLI, OpenCode, Qwen Code, Kilo Code, Pi ACP, Grok Build — and Qoder CLI), a skills registry, a plugin SDK, and persistent local memory. No other project in the survey comes close.

But OpenClaw's **computer control on Windows is explicitly "experimental"** and is **pixel-coordinate based**: its own documentation states the Windows path "supports pixel coordinates but lacks held-input contracts" and is limited to the **primary display only**. That is the exact gap your vision needs closed, and it is where the engineering effort should go.

### The three structural findings that decide the architecture

**1. Windows already tells you what its buttons are. Almost nobody uses it.**
Every pixel-based grounding model (UI-TARS, ShowUI, Jedi, OmniParser, Qwen3-VL) answers *"what is drawn at (x,y)?"*. The Windows UI Automation tree answers *"what is the button labelled 'Submit', is it enabled, what is its AutomationId, and can I Invoke it?"* — a semantically stable answer. The UIA tree is available today via [`yinkaisheng/Python-UIAutomation-for-Windows`](https://github.com/yinkaisheng/Python-UIAutomation-for-Windows) (Apache-2.0) and [`pywinauto/pywinauto`](https://github.com/pywinauto/pywinauto) (BSD-3), both of which are the pinned dependencies of [`microsoft/UFO`](https://github.com/microsoft/UFO). UIA fails on canvas/WebGL/RDP-rendered content and on Chromium unless `--force-renderer-accessibility` is set — so it is a **primary path with a pixel fallback**, never the other way round. This single decision is worth more than any model choice, and it is the reason this report ranks accessibility-tree tooling above VLM grounding for Windows.

**2. Voice must be a separate operating-system process, not a callback inside the agent loop.**
This is not a style preference — it is forced by two independent, verified failure modes. LiveKit Agents had to ship four separate fixes for tool calls left open by interruptions ("answer realtime tool calls interrupted past the timeout", "answer tool calls left open when an interrupted turn times out" — both merged Oct 2026). Pipecat has an **open bug** where interrupting the bot mid-tool-call leaves the tool "stuck 'in progress'", the LLM is told it is still running, `has_function_calls_in_progress` stays true, and **"Delegated backend work never finishes"** — `BackendLLMWorker` waits for tool calls to settle until its timeout. A desktop operator runs 30-second to 30-minute GUI actions. If the voice loop and the action loop share a coroutine, that exact bug means the operator's mouse click survives but its voice reply never completes. Separating the planes (a local WebSocket ASR daemon + a separate wake daemon, with the brain as just another client) makes the failure structurally impossible.

**3. ACP is the bus for coding agents; MCP is the bus for tools; AG-UI is the bus for your own UI.**
[`agentclientprotocol/agent-client-protocol`](https://github.com/agentclientprotocol/agent-client-protocol) is Apache-2.0, at **stable protocol version 1**, with clients in Zed, neovim, VS Code, JetBrains, Emacs, Sublime, Unity, and agents in Cursor, Cline, Claude Code (via `@agentclientprotocol/claude-agent-acp`), Codex (via `@agentclientprotocol/codex-acp`), Gemini CLI and OpenCode. The one gap: **`session/stop` is implemented by zero agents** (verified across a 39-agent ACP registry probe) — so kill runs via the process handle, never via the protocol.

### Strategy verdict

| | Strategy A — adopt an all-in-one | Strategy B — assemble from parts |
|---|---|---|
| Coverage | OpenClaw covers ~80% of your vision out of the box | You assemble it yourself; nothing works until you build it |
| Windows depth | Shallow — computer control is experimental and pixel-based | Depends entirely on whether you choose UIA-first tooling |
| Coding-agent integration | Solved — ACP client already drives Claude Code / Codex / OpenCode / Qwen | 2–4 weeks of protocol work per agent |
| Latency & resources | Chat-channel oriented; voice is one node among many | Tunable per plane |
| Maintenance | 393k★ project, Foundation-stewarded, daily releases | You own every dependency |
| Extensibility | Plugin SDK + skills + ClawHub + MCP + ACP — all present | Whatever you build |

**Recommendation: Strategy B executed *on top of* OpenClaw as the substrate.** Treat OpenClaw as the gateway/session/policy/ACP/MCP/skill shell and the native Windows UI, then replace its computer-use plugin with a UIA-first Windows actuator you own. That gets you Strategy A's coverage and coding-agent integration for free while letting you fix the one part where Strategy A is weak. The alternative — building the ACP/MCP/skill/policy shell yourself — is months of work that buys you nothing, because OpenClaw's versions are MIT and already integrated.

### Two things to be honest about before you start

- **Computer-use reliability is still low.** Agent-S's own headline claim (SOTA on OSWorld-V2) is **21.3%** — roughly 4 tasks in 5 fail, on a strict binary pass/fail metric. `microsoft/WindowsAgentArena` and `xlang-ai/OSWorld` are the honest baselines. Anyone claiming a desktop agent "just works" is measuring on a narrow task distribution.
- **Licensing is a live constraint, not a formality.** Several obvious-looking picks are source-available rather than open source: `Dify` (Modified Apache with conditions), `n8n` (Sustainable Use License), `LobeHub` (Community License requiring a commercial license for derivative distribution), `Open WebUI` (branding clause), `OpenClaw`/`OpenAdapt`/`trycua` are clean MIT. `anthropics/claude-code` is **proprietary** (`spdx_id: NONE`) — you can drive it, you cannot ship it.

---

## Part 2 — Best All-in-One Projects

### 2.1 Tier ranking

| Rank | Project | Repo | License | ★ | Last push | Lang | Windows | One-line verdict |
|---|---|---|---|---|---|---|---|---|
| 1 | **OpenClaw** | [openclaw/openclaw](https://github.com/openclaw/openclaw) | **MIT** | 393,843 | 2026-11-03 | TypeScript | **Native app** | The reference all-in-one. Only project with voice + screen + shell + browser + MCP(both roles) + ACP + skills + plugins + local memory + a real Windows app. |
| 2 | **Microsoft UFO²/³** | [microsoft/UFO](https://github.com/microsoft/UFO) | MIT | 10,169 | 2026-10-30 | Python | **Windows-only** | The best **Windows-native GUI agent framework**. Hybrid GUI-vs-native-API arbitration. No voice. |
| 3 | **trycua / cua** | [trycua/cua](https://github.com/trycua/cua) | MIT | 31,669 | 2026-11-03 | Rust | Yes (UIA) | The best **cross-OS computer-use substrate**. A layer to build on, not a product. OpenClaw already wraps its driver. |
| 4 | **UI-TARS Desktop** | [bytedance/UI-TARS-desktop](https://github.com/bytedance/UI-TARS-desktop) | Apache-2.0 | 39,535 | 2026-10-09 | TypeScript | `publish:win32` | Solid Electron agent shell, but the "local operator" needs a **cloud** VLM endpoint — no bundled local weights. |
| 5 | **OpenAdapt** | [OpenAdaptAI/OpenAdapt](https://github.com/OpenAdaptAI/OpenAdapt) | MIT | 1,862 | 2026-11-03 | Python | Yes | The **deterministic-replay** differentiator: compile a demonstrated task into a local program with **no model calls at runtime**. Underrated. |
| 6 | **Agent-S (Simular)** | [simular-ai/Agent-S](https://github.com/simular-ai/Agent-S) | Apache-2.0 | 12,864 | 2026-10-30 | Python | Yes | Research framework with the best published Windows numbers (WindowsAgentArena). Requires hosted models for S3/S4. |
| 7 | **OpenHands** | [OpenHands/OpenHands](https://github.com/OpenHands/OpenHands) | MIT | 92,585 | 2026-11-03 | TypeScript | Yes | Best-maintained Windows-capable agent shell, but a *coding* agent — it would replace your coding agents, the opposite of your goal. |
| 8 | **Langflow** | [langflow-ai/langflow](https://github.com/langflow-ai/langflow) | MIT | 156,296 | 2026-11-03 | Python | Server-side | Clean MIT visual agent builder. Server-shaped, not desktop-shaped. |

> **Corrected names.** `anthropics/anthropic-quickstarts` → **`anthropics/claude-quickstarts`**. `sst/opencode` → **`anomalyco/opencode`**. `block/goose` → **`aaif-goose/goose`**. `All-Hands-AI/OpenHands` → **`OpenHands/OpenHands`**. `openadapt/OpenAdapt` → **`OpenAdaptAI/OpenAdapt`**. `cognee-ai/cognee` → **`topoteretes/cognee`**. `microsoft/Windows-MCP` **does not exist** — the real one is [`CursorTouch/Windows-MCP`](https://github.com/CursorTouch/Windows-MCP) (MIT, 11,012★).

### 2.2 OpenClaw — deep dive (the project that changes your plan)

**Verified capability map** (from `openclaw/openclaw` README + `openclaw/openclaw-windows-node` README + extension manifests, all read on 2026-11-04):

| Layer | What actually ships | Evidence |
|---|---|---|
| **Windows shell** | Native C#/.NET 10 WinUI companion app; x64 **and** ARM64 installers; tray, setup wizard, Command Center, node mode | `openclaw/openclaw-windows-node` (MIT, 2,188★) — "Choose which Windows capabilities they can use" |
| **Voice — STT** | "Transcribe microphone audio **locally**" | Windows node capability table |
| **Voice — TTS** | "Speak text aloud through this PC's speakers" | Windows node capability table |
| **Screen** | "Take screenshots and short screen recordings" | Windows node capability table |
| **Browser** | "Drive a compatible Chromium browser on this PC" | Windows node capability table |
| **Shell / OS** | "Run shell commands and scripts, subject to local exec approvals and sandbox policy" | Windows node capability table |
| **Other nodes** | Camera, Canvas, Location; notifications and device status | Windows node capability table |
| **Computer control** | `cua-computer` extension via **CUA Driver** — Windows + Linux supported, macOS via AX | `extensions/cua-computer/README.md`: *"Experimental computer control through CUA Driver"* |
| **Coding agents** | `extensions/acpx` — ACP runtime backend driving **Copilot CLI, Grok Build, Kilo Code, OpenCode, Pi ACP, Qoder CLI, Qwen Code** | `extensions/acpx/README.md`: *"ACP-backed agent runtime sessions… OpenClaw keeps the conversation transcript and asks for approval when the agent requests permission"* |
| **MCP** | Local MCP **server** mode for Claude Desktop / Claude Code / Cursor, **and** node-hosted MCP servers (stdio + streamable-http) | README + Windows node README |
| **Extension points** | Plugin SDK, skills registry, ClawHub marketplace, A2A extension, `browser` / `canvas` / `codex` / `copilot` extensions | `extensions/` directory listing (60+ entries) |
| **Policy** | Per-PC capability permission toggles, gateway allowlists, pairing + reapproval flows, sandbox selection per node | Windows node README |

**The three limitations that decide your roadmap:**

1. **Windows computer control is pixel-based.** Docs state: pixel coordinates are supported, **held-input contracts are not** (no modifier-held clicks), and it targets the **primary display only**. Multi-monitor and Ctrl+Shift+click behaviours are broken out of the box.
2. **It is experimental, explicitly.** The extension README opens with "Experimental computer control through CUA Driver."
3. **The gateway runs in WSL by default on Windows.** The Windows setup wizard offers "Install a local gateway (WSL)" or connecting to an existing gateway. A WSL gateway complicates Windows-native UIA (it cannot enumerate Windows UI Automation trees from inside WSL without an interop bridge) and complicates native input injection.

### 2.3 Microsoft UFO — the Windows GUI agent to study

- **Repo:** [`microsoft/UFO`](https://github.com/microsoft/UFO) (MIT, 10,169★, 2026-10-30)
- **Architecture:** HostAgent + per-application AppAgents. A "Puppeteer" chooses between **GUI actions** and **native API actions** — `ufo/automator/app_apis/{excel,powerpoint,word,web,shell}` ship app-specific API adapters that bypass the mouse entirely for supported apps.
- **Its `requirements.txt` is effectively Microsoft's own blessed Windows actuator stack:** `pywin32>=310`, `pywinauto==0.6.8`, `pyautogui==0.9.54`, `uiautomation==2.0.18`. Note it pins the **stale** pyautogui — even Microsoft pins the DPI-buggy injector.
- **Sandboxing:** "Automation runs in a sandboxed virtual desktop so you can keep using your main screen" — Windows' nearest analogue to a virtual display.
- **MCP integration is marked "Completed"** in its roadmap (`config/ufo/mcp.yaml`).
- **Limits:** Windows-only; no voice; UFO³ Galaxy (multi-device DAG) is the newest and least hardened piece.

---

## Part 3 — Best Projects by Capability

### 3.1 Voice and conversational AI

| Project | Repo | License | ★ | Last push | Local/Cloud | Role |
|---|---|---|---|---|---|---|
| **sherpa-onnx** | [k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) | Apache-2.0 | 15,710 | 2026-11-03 | Local | Streaming ASR + TTS + VAD + diarization. **Real Windows x64/arm64 binaries + CUDA-12/13 win-x64 builds** (verified in release `v2.0.1`). |
| **Pipecat** | [pipecat-ai/pipecat](https://github.com/pipecat-ai/pipecat) | BSD-2 | 17,032 | 2026-11-03 | Both | Realtime voice-agent framework. `LocalAudioTransport` = direct mic/speaker, **no WebRTC server, no Docker**. **Zero Windows CI** (verified: 0 code hits for `windows-latest`). |
| **LiveKit Agents** | [livekit/agents](https://github.com/livekit/agents) | Apache-2.0 | 15,076 | 2026-11-03 | Both | Best **background-tool** semantics (`lk_agents_cancel_task`, session-scoped tasks, adaptive interruption). **Has Windows CI**, but requires a LiveKit server (Cloud or Docker). |
| **Silero VAD** | [snakers4/silero-vad](https://github.com/snakers4/silero-vad) | MIT | 10,588 | 2026-10-22 | Local | <1 ms per 30 ms chunk. Use `silero-vad[onnx-cpu]` to avoid torch on Windows. |
| **Kokoro** | [hexgrad/kokoro](https://github.com/hexgrad/kokoro) | Apache-2.0 | 9,893 | 2025-08-06 | Local | 82M params, real-time on CPU. Needs **espeak-ng** on Windows (no official binary). |
| **faster-whisper** | [SYSTRAN/faster-whisper](https://github.com/SYSTRAN/faster-whisper) | MIT | 26,450 | 2026-11-02 | Local | Best quality-per-effort batch STT; chunkable for near-streaming. |
| **Moonshine** | [moonshine-ai/moonshine](https://github.com/moonshine-ai/moonshine) | NOASSERTION (README: MIT) | 11,397 | 2026-11-02 | Local | Only project shipping a *complete* on-device voice kit (STT+TTS+agent) on Windows/mac/Linux/WASM. `AgentFlow.barge_in()` is implemented; per-`say()` barge-in is documented as **"Reserved for future use"**. |
| **openWakeWord** | [dscripka/openWakeWord](https://github.com/dscripka/openWakeWord) | Apache-2.0 (code) | 2,909 | 2025-12-30 | Local | Trainable wake word. **Shipped models are CC BY-NC-SA 4.0** → you must train your own. Windows = onnxruntime only (no TFLite). |
| **Chatterbox** | [resemble-ai/chatterbox](https://github.com/resemble-ai/chatterbox) | MIT | 27,531 | 2026-07-21 | Local | Zero-shot voice cloning at CPU speed. Model license not stated on the HF card — verify before shipping. |

**Rejected (with reason):** `rhasspy/piper` **archived** (MIT → successor `OHF-Voice/piper1-gpl` is **GPL-3.0**, viral for a shipped desktop binary); `2noise/ChatTTS` **AGPL-3.0 + CC BY-NC 4.0 weights**; `fishaudio/fish-speech` Fish Audio Community License (non-commercial under $500k revenue, **hosted service needs a separate written license**); `SWivid/F5-TTS` weights **CC-BY-NC**; `coqui-ai/TTS` dead since 2024-08 and XTTS-v2 weights are **CPML**; `dscripka/openWakeWord` pretrained models CC BY-NC-SA; `TEN-framework/ten-vad` carries an **Agora non-compete clause**; `Picovoice/porcupine` requires an **AccessKey** (not offline, not shippable); `KoljaB/RealtimeVoiceChat` self-declared unmaintained, **no LICENSE file**; `wiseman/py-webrtcvad` stale since 2024; `myshell-ai/MeloTTS` and `yl4579/StyleTTS2` dead (2024); `moonshine` legacy non-English models are non-commercial.

**Design answer to "can voice stay active while the assistant operates other apps?" — Yes, but only if the listening plane is a separate process.** Pipecat supports `cancel_on_interruption=False` async tools that keep running through an interruption and push intermediate updates via `FunctionCallResultProperties(is_final=False)`; LiveKit supports session-scoped background tasks that survive agent handoff. Both still have the mid-tool-call interruption bug described in Part 1. Run ASR + wake + VAD as independent daemons (`sherpa-onnx` has a websocket server; the `wyoming-*` daemons expose `tcp://0.0.0.0:10400`) and the brain becomes just another client.

### 3.2 Screen recognition and visual intelligence

| Project | Repo | License | ★ | Last push | Model/VRAM | Role |
|---|---|---|---|---|---|---|
| **OmniParser** | [microsoft/OmniParser](https://github.com/microsoft/OmniParser) | **CC-BY-4.0** (repo) / MIT (v2 weights) | 25,604 | 2026-07-20 | <2 GB, CPU-capable | Screenshot → icon boxes + captions. **A parser, not a grounder** — self-reports 39.5% on ScreenSpot-Pro. Earlier Ultralytics-based detectors retain **AGPL**. |
| **RapidOCR** | [RapidAI/RapidOCR](https://github.com/RapidAI/RapidOCR) | Apache-2.0 | 8,431 | 2026-11-03 | CPU, ONNX | Best Windows-friendly OCR: zero GPU, zero paddle dependency. |
| **windows-capture** | [NiiightmareXD/windows-capture](https://github.com/NiiightmareXD/windows-capture) | MIT | 523 | 2026-10-05 | — | **Per-window** Windows Graphics Capture. `pip install windows-capture`. |
| **DXcam** | [ra1nty/DXcam](https://github.com/ra1nty/DXcam) | MIT | 806 | 2026-09-23 | — | Fastest full-monitor DXGI DDA grab; `device_idx`/`output_idx` for multi-monitor. |
| **Jedi (OSWorld-G)** | [xlang-ai/OSWorld-G](https://github.com/xlang-ai/OSWorld-G) | Apache-2.0 | 185 | 2026-06-18 | 3B ≈ 7 GB | **Grounding-only** model (Qwen2.5-VL prompt template) → clean click coordinate, ~8 GB VRAM. Smallest genuinely usable option. |
| **ShowUI** | [showlab/ShowUI](https://github.com/showlab/ShowUI) | Apache-2.0 | 1,928 | 2026-04-24 | 2B ≈ 5 GB | End-to-end grounding + action. Lighter alternative to Jedi. |
| **Qwen3-VL** | [QwenLM/Qwen3-VL](https://github.com/QwenLM/Qwen3-VL) | Apache-2.0 | 20,163 | 2026-01-30 | 8B ≈ 16 GB bf16 | Strongest general option; official `cookbooks/2d_grounding.ipynb` (boxes **and** points). |
| **UI-TARS** | [bytedance/UI-TARS](https://github.com/bytedance/UI-TARS) | Apache-2.0 | 11,667 | 2026-01-27 | 1.5-7B ≈ 6–16 GB | Reasoning + action in one call. GGUF quants exist → runs via llama.cpp on Windows. |
| **Molmo** | [allenai/molmo](https://github.com/allenai/molmo) | Apache-2.0 | 945 | 2024-12-12 | 7B ≈ 16 GB | Native **pointing** (`<point x y>`). Stale since 2024-12. |
| **UGround** | [OSU-NLP-Group/UGround](https://github.com/OSU-NLP-Group/UGround) | MIT | 321 | 2026-08-24 | 7B ≈ 16 GB | ICLR'25 Oral. **HF weight card has no license tag — UNVERIFIED for shipping.** |
| **GUI-Actor** | [microsoft/GUI-Actor](https://github.com/microsoft/GUI-Actor) | MIT | 424 | 2026-04-13 | 2B ≈ 5 GB | "Coordinate-free" grounding (predicts element refs, not xy). NeurIPS'25. |

#### The distinction the brief asked for — structure vs. pixels

| Capability | Concrete repo | What you actually get | Where it fails |
|---|---|---|---|
| **Real application structure** | [`yinkaisheng/Python-UIAutomation-for-Windows`](https://github.com/yinkaisheng/Python-UIAutomation-for-Windows) | `ControlType`, `Name`, `AutomationId`, `IsEnabled`, `BoundingRectangle`, `Invoke`/`Value`/`Selection` patterns | Chromium/Electron need `--force-renderer-accessibility`; custom-drawn canvas exposes nothing; **UIA reads are blocked across integrity levels**; must run as administrator |
| **Real application structure (dual backend)** | [`pywinauto/pywinauto`](https://github.com/pywinauto/pywinauto) | `backend="uia"` full tree + `backend="win32"` HWND/class/title fallback | `win32` backend is window-level only; UIA traversal on huge trees is slow |
| **Real application structure (.NET)** | [`FlaUI/FlaUI`](https://github.com/FlaUI/FlaUI) | UIA2/UIA3, first-class for WPF/WinUI | .NET only |
| **Live tree inspector** | [`ThePacielloGroup/aviewer`](https://github.com/ThePacielloGroup/aviewer) | `inspect.exe` clone (MSAA/IA2/UIA/ARIA/DOM) | Pascal/Win32, manual tool |
| **UIA exposed to an LLM** | [`locomorange/uiautomation-mcp`](https://github.com/locomorange/uiautomation-mcp) (MIT, 28★), [`dividduang/uiautomation_mcp`](https://github.com/dividduang/uiautomation_mcp) (Apache-2.0, 22★) | Structured control lists instead of pixels | Very young; token blowup on dense trees |
| **Shallow window structure** | `pywinauto` win32 backend / `EnumWindows` | Title, class, rect, focus | No widget semantics |
| **Pixels only** | OmniParser, ShowUI, Jedi, UI-TARS, Qwen3-VL, Molmo | Works on *anything* drawn — canvas, games, RDP frames, PDFs | Coordinates only; no names, no widget state, **cannot verify the click landed** |
| **Pixels + OCR text** | RapidOCR, PaddleOCR, EasyOCR | Text + word boxes → pseudo-element list | Text-only; misses unlabeled icon buttons |
| **Hybrid** | [`OpenAdaptAI/OpenAdapt`](https://github.com/OpenAdaptAI/OpenAdapt) | *"the compiler uses the most reliable layer the app offers: page structure first (DOM, Windows UI Automation, macOS Accessibility, Linux AT-SPI), a direct API call where one is bound, and pixels only where nothing else exists"* | Demo-compilation model, not a live agent |
| **Human-taught (no grounding needed)** | [`showlab/ShowUI-Aloha`](https://github.com/showlab/ShowUI-Aloha) | Records real screen+input demos, learns the workflow | Needs one demonstration per task family |

**Verdict:** UIA is the only route to *real application structure*. A Windows-native operator should use UIA as the semantic source of truth and pixels only as fallback for canvas/D3D/RDP content.

**Critical practical constraint:** **vLLM has no native Windows build** (its own install index lists CUDA/ROCm/XPU/Apple/CPU only). Every project that recommends vLLM (UI-TARS, OpenCUA, ShowUI, Jedi, Aguvis) therefore needs WSL2 for the server, or a **GGUF/llama.cpp** route on native Windows. GGUF quants exist for UI-TARS-1.5-7B/2B, ShowUI-2B and OpenCUA-7B. FlashAttention has no Windows wheels — use PyTorch SDPA.

**Rejected:** `xlang-ai/aguvis` (**no LICENSE file**, HF card untagged); `AriaUI/Aria-UI` (no LICENSE file); `showlab/showui-pi` (no LICENSE); UI-TARS-2 (**no weights released** — paper only); `apple/ferret-ui` (**no weights**); `Hcompany/Holo1` (404 — `hcompai/surfer-h-cli` **archived**, README points to a paid API); `zai-org/Open-AutoGLM` (**phone only**, Android/ADB); `hecomi/uDesktopDuplication` (dead 2021) / `uWindowCapture` (stale 2023); `IDEA-Research/GroundingDINO` (stale 2024, not GUI-specific); `likaixin2000` ScreenSpot-Pro is a dataset repo only; `MinerU`/`Surya`/`dots.ocr` are document parsers, not screen parsers.

### 3.3 Computer control and GUI automation

| Project | Repo | License | ★ | Last push | Mechanism | Windows notes |
|---|---|---|---|---|---|---|
| **pynput** | [moses-palmer/pynput](https://github.com/moses-palmer/pynput) | **LGPL-3.0** | 2,176 | 2026-05-12 | `SendInput` via ctypes | Only actively-maintained Python injector using real `SendInput`. **LGPL is a copyleft constraint** for a closed-source product. |
| **enigo** | [enigo-rs/enigo](https://github.com/enigo-rs/enigo) | MIT | 1,804 | 2026-11-02 | `SendInput`, Rust | **The only library that solves mixed-DPI correctly**: *"temporarily switches the calling thread to per-monitor DPI awareness"*. No UIA, no Python-native API. |
| **robotjs** | [octalmage/robotjs](https://github.com/octalmage/robotjs) | MIT | 12,783 | 2026-10-24 | native | **Revived**, not dead: v0.9.0 (2026-08-06), Node-API prebuilds for Win x64 + arm64. |
| **uiautomation** | [yinkaisheng/Python-UIAutomation-for-Windows](https://github.com/yinkaisheng/Python-UIAutomation-for-Windows) | Apache-2.0 | 3,595 | 2026-06-02 | UIA | Fastest, cleanest Python UIA. Ships a tree-dump CLI. **Documents the elevation requirement explicitly** — rare and valuable. |
| **pywinauto** | [pywinauto/pywinauto](https://github.com/pywinauto/pywinauto) | BSD-3 | 6,361 | 2026-10-28 | win32 + UIA | 1.0.0b1 (2026-10-15) supersedes the 0.6.9 line most tutorials use. |
| **Playwright** | [microsoft/playwright](https://github.com/microsoft/playwright) | Apache-2.0 | 99,200 | 2026-11-03 | DOM | Bundled Chromium — no dependence on the user's browser. |
| **playwright-mcp** | [microsoft/playwright-mcp](https://github.com/microsoft/playwright-mcp) | Apache-2.0 | 39,133 | 2026-11-03 | a11y tree | *"Uses Playwright's accessibility tree, not pixel-based input"* — token-expensive; the repo's own README suggests `playwright-cli` for coding agents. |
| **node-pty** | [microsoft/node-pty](https://github.com/microsoft/node-pty) | MIT | 2,058 | 2026-11-02 | ConPTY | Windows 10 1809+ required. The only way to drive `fzf`, `vim`, PowerShell prompts on Win11. |
| **pywinpty** | [andfoy/pywinpty](https://github.com/andfoy/pywinpty) | MIT | 164 | 2026-10-28 | ConPTY | Rust-backed Python binding. |
| **pyautogui** | [asweigart/pyautogui](https://github.com/asweigart/pyautogui) | BSD-3 | 12,754 | **2024-08-20** | `mouse_event`/`keybd_event` | **De facto unmaintained for Win11.** PyPI 0.9.54 = 2023-05; still legacy `mouse_event` with an unresolved "SendInput isn't working" TODO; system-DPI-only awareness; *"only works on the primary monitor"*. **Fallback only.** |
| **agent-takkub** | [takkub/agent-takkub](https://github.com/takkub/agent-takkub) | MIT | **4** | 2026-11-03 | pywinpty + pyte | The most on-target *tiny* find: Windows-native desktop cockpit, git-worktree isolation, evidence-gated QA, reads Claude Code's per-project session JSONL. |

#### The three control approaches compared

| | (a) Screenshot + coordinate | (b) Accessibility / UIA tree | (c) DOM / browser |
|---|---|---|---|
| **Reliability on Win11** | Lowest. Two independent failure modes: *where* to click (pixel→control mapping) **and** *whether* the click lands (UIPI). Breaks on resolution/DPI/theme changes. | High where a tree exists. Win32/WPF/WinForms/UWP good; Chromium/Electron need the a11y flag; canvas/Games/CAD/RDP frames expose nothing. | Highest. Deterministic selectors, live DOM. Fails only on DOM churn. |
| **Speed** | Slowest: capture (100–300 ms) → VLM (seconds) → act → verify. | Fastest to *act* (ms); tree traversal on huge trees is slow. | Fast (ms). |
| **Brittleness** | Very high — theme, DPI, scroll, animation, z-order, IME. | Medium — labels change; AutomationIds often absent in Win32 apps; Chrome/VS expose enormous trees. | Low-medium — CSS/XPath churn only. |
| **Non-web coverage** | Universal. | Good for native Windows; poor for canvas/RDP. | Zero. |
| **Elevation / UIPI** | Blocked. `SendInput` "is subject to UIPI… permitted to inject input only into applications that are at an equal or lesser integrity level", and "neither GetLastError nor the return value will indicate the failure was caused by UIPI blocking". | **Also blocked for reads** — `pywinauto` #890, uiautomation README ("run Python as administrator"), pywinauto PR #441 "handling Administrator privileges" **never merged**. | Unaffected — no UIPI concept. |
| **Setup cost** | Lowest. | Medium. | High (browser install, profile, CDP port). |

#### The three hard Windows 11 truths

1. **UIPI is the #1 silent killer.** A non-elevated agent physically cannot type into an elevated app, cannot read it via UIA, and **gets no error**. Fixes: run the agent elevated; launch the target at medium integrity; or use UIA *patterns* (`Invoke`/`Value`) instead of synthetic input.
2. **DPI is a live bug in the popular libs.** pyautogui calls only `SetProcessDPIAware()` (system DPI, not per-monitor-v2) → open issues #432 (mixed-DPI multi-monitor), #560 ("doubling the cursor distance movement"), #116 (screenshot scaling). With Win11 laptops defaulting to 125%/150% scaling, this is the #1 coordinate bug. **Set `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` before any capture or coordinate math.**
3. **No virtual display on Windows.** `PyVirtualDisplay` is Xvfb-only. Headless Win11 automation needs a real/virtual monitor (VM framebuffer, RDP session, dummy plug-in monitor) — or drive the target window offscreen.

**Rejected:** `microsoft/WinAppDriver` (dead since 2025-04, Developer Mode + admin, Win10 docs only); `appium/appium-windows-driver` (**deprecated upstream** — WinAppDriver speaks legacy JSONWP, not Appium 4 compatible); `appium/appium-wincore-driver` (10★, pushed hours before research — prototype to vendor, not a dependency); `oblitum/Interception` (needs WDK 7.1, admin driver install, **LGPL non-commercial / proprietary dual-license**, 353 issues about the driver breaking input); `nut-tree/nut.js` (npm packages unpublished, stale 2024-05); `pexpect`/`tmux`/`xdotool`/`ydotool`/`wtype` (Unix/Linux only); `cliclick`/`hammerspoon` (macOS only); `brookhong/KeyCastOW` (last push 2019 — a *visualizer*, not an input source).

### 3.4 Windows / OS integration

| Project | Repo | License | ★ | Last push | Unlocks |
|---|---|---|---|---|---|
| **psutil** | [giampaolo/psutil](https://github.com/giampaolo/psutil) | BSD-3 | 11,314 | 2026-11-01 | Process list/kill/children, CPU/mem/disk/net, `net_io_counters`, `win_service_iter` (service control needs elevation) |
| **pywin32** | [mhammond/pywin32](https://github.com/mhammond/pywin32) | PSF-2.0 | 5,634 | 2026-11-03 | `win32process`, `win32service`, `win32gui`, `win32clipboard`, `win32com` (Office COM) |
| **watchdog** | [gorakhargosh/watchdog](https://github.com/gorakhargosh/watchdog) | Apache-2.0 | 7,435 | 2026-11-03 | FS events via `ReadDirectoryChangesW` (verified in source) |
| **windows-rs** | [microsoft/windows-rs](https://github.com/microsoft/windows-rs) | Apache-2.0 | 12,894 | 2026-11-03 | The only clean route to **Job Objects** (`Win32_System_JobObjects`) — killing a job kills the whole child tree |
| **sqlite-utils** | [simonw/sqlite-utils](https://github.com/simonw/sqlite-utils) | Apache-2.0 | 2,184 | 2026-10-11 | SQLite state + FTS5 (better than `sqlite_orm`, which is **AGPL-3.0**) |
| **ripgrep / fd / fzf** | [BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep) (Unlicense), [sharkdp/fd](https://github.com/sharkdp/fd), [junegunn/fzf](https://github.com/junegunn/fzf) | MIT | 69,664 / 44,935 / 83,980 | 2026-11-03 | Fast content/filename search; `fzf` needs a PTY |
| **uv / mise** | [astral-sh/uv](https://github.com/astral-sh/uv), [jdx/mise](https://github.com/jdx/mise) | Apache-2.0 / MIT | 91,501 / 35,721 | 2026-11-03 | Modern Python/polyglot env management, native Windows |
| **Scoop** | [ScoopInstaller/Scoop](https://github.com/ScoopInstaller/Scoop) | Unlicense-or-MIT | 24,794 | 2026-11-03 | Per-user installs, **no UAC prompt** (winget requires elevation) |
| **win11toast** | [GitHub30/win11toast](https://github.com/GitHub30/win11toast) | MIT | 338 | 2026-01-17 | WinRT toast (needs a registered AppUserModelID) |
| **pyperclip** | [asweigart/pyperclip](https://github.com/asweigart/pyperclip) | BSD-3 | 1,841 | 2025-09-26 | Clipboard via pure ctypes, no pywin32 needed |
| **Windows-MCP** | [CursorTouch/Windows-MCP](https://github.com/CursorTouch/Windows-MCP) | MIT | 11,012 | 2026-11-03 | 16 MCP tools: app, clipboard, display, filesystem, input, notification, process, registry, scrape, shell, snapshot, + a three-tier permission gate |
| **mcp-windows** | [sbroenne/mcp-windows](https://github.com/sbroenne/mcp-windows) | MIT | 127 | 2026-11-03 | UIA-first MCP + `wincli`; has `ElevationDetector`, `SecureDesktopDetector`, `SnapshotDiffEngine` |
| **PowerToys** | [microsoft/PowerToys](https://github.com/microsoft/PowerToys) | MIT | 140,488 | 2026-11-03 | CmdPal (Command Palette), Workspaces, AdvancedPaste, FancyZones, Peek, Keyboard Manager |

**Windows-only projects worth reading for what to steal:** `microsoft/UFO` (GUI-vs-native-API arbitration pattern), `microsoft/WindowsAgentArena` (the `desktop_env` task-definition + evaluator format for regression testing your own agent), `CursorTouch/Windows-MCP` (relaxed/balanced/strict ownership gate + `control_ledger`/`control_recovery`), `sbroenne/mcp-windows` (elevation/secure-desktop/snapshot-diff hazards), `mario-andreschak/mcp-windows-desktop-automation` (its honest capability matrix: "services in session 0, headless Windows sessions, other users' sessions, and elevated windows beyond the server's privileges are not supported"), `microsoft/PowerToys` (CmdPal extension model), `microsoft/terminal` (`samples/ConPTY/EchoCon` — correct ConPTY lifecycle), `Flow-Launcher/Flow.Launcher` (plugin contract).

**Rejected:** `voidtools/Everything` — **404, no GitHub repo**; the Everything *app* is proprietary. Only the wrappers are OSS (`everything_sdk3`, `ES`, `http_server`). `speedtest-cli` archived; `al45tair/netifaces` archived; `OpenHardwareMonitor` stale since 2024-07; `asdf-vm/asdf` POSIX-only; `astral-sh/rye` archived; `microsoft/WinAppDriver` dead; `sudo-prompt` npm archived (use `elevate` / `windows-elevate`); `PyFilesystem/pyfilesystem2` adds little over `pathlib`.

### 3.5 Coding-agent integration

| Agent | Repo | License | Headless / structured output | MCP role | ACP |
|---|---|---|---|---|---|
| **OpenAI Codex** | [openai/codex](https://github.com/openai/codex) | **Apache-2.0** | `codex exec --json` (JSONL: `thread.started`, `item.started/updated/completed`, `turn.started/completed/failed`, `thread.error`) + **`codex app-server`** JSON-RPC over stdio/WS (thread/item/turn methods, `item/commandExecution/requestApproval`, `thread/memory/read`, `thread/list`, `thread/search`) | Client only | Adapter: [`agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp) |
| **Claude Code** | [anthropics/claude-code](https://github.com/anthropics/claude-code) | **Proprietary (`spdx_id: NONE`)** | `claude -p --output-format stream-json --verbose` (NDJSON). **Agent SDK**: `@anthropic-ai/claude-agent-sdk` (TS) / [`anthropics/claude-agent-sdk-python`](https://github.com/anthropics/claude-agent-sdk-python) (MIT, 8,420★) | Client only — **no `claude mcp serve`** | Adapter: `claude-agent-acp` |
| **OpenCode** | [anomalyco/opencode](https://github.com/anomalyco/opencode) | **MIT** | `opencode serve` → HTTP + **OpenAPI 3.1 at `/doc`**; SSE `GET /global/event`; the TUI itself is drivable via `/tui`; SDK `@opencode/sdk` | Client | **Native** (`opencode acp`) |
| **Qwen Code** | [QwenLM/qwen-code](https://github.com/QwenLM/qwen-code) | Apache-2.0 | `qwen serve` → HTTP daemon with SSE + `Last-Event-ID` replay, Web Shell UI, `/session/:id/prompt`, `/session/:id/approval-mode`, `/workspace/mcp/servers`; SDKs TS/Python/Java | Client (+ SDK-as-server) | **Native** (`qwen --acp`) |
| **Gemini CLI** | [google-gemini/gemini-cli](https://github.com/google-gemini/gemini-cli) | Apache-2.0 | `-p/--prompt` headless; `--output-format json` or streaming JSONL (`init, message, tool_use, tool_result, error, result`); exit codes 0/1/42/53 | Client | **Native** (`gemini --acp`) |
| **Goose** | [aaif-goose/goose](https://github.com/aaif-goose/goose) | Apache-2.0 | `goose acp`; `goose serve`; GDK (Rust, UniFFI → Python/Kotlin) with `ObservabilityHook` | Client **and ACP server** | **Native** (server) |
| **OpenHands** | [OpenHands/OpenHands](https://github.com/OpenHands/OpenHands) | MIT | **Agent Server** REST/WS event stream; Python + TS SDKs | Client | TS client (`acp-client.ts`) |
| **Crush** | [charmbracelet/crush](https://github.com/charmbracelet/crush) | NOASSERTION | `crush run [prompt]` + `crush server --host http://127.0.0.1:8080` → OpenAPI 3.1 at `/v1/docs/`; own hook system | Client | None |
| **Aider** | [Aider-AI/aider](https://github.com/Aider-AI/aider) | Apache-2.0 | `--message`/`--message-file`/`--yes-always` only. **No JSONL, no MCP, no ACP, no server.** Last push 2026-05-22 | **None** | **None** |

**Codex hooks — verified directly from `codex-rs/core/config.schema.json` (14 events):** `PreToolUse`, `PostToolUse`, `PermissionRequest`, `PreCompact`, `PostCompact`, `PreModelRequest`, `PostModelRequest`, `SessionStart`, `SessionEnd`, `Stop`, `Interrupt`, `SubagentStart`, `SubagentStop`, `UserPromptSubmit`. Handler `type` ∈ `command | mcp_tool | prompt | agent`, with a **Windows-native `commandWindows`** field. This is the cleanest correction-loop mechanism in the whole coding-agent landscape: your supervisor registers an MCP server, Codex calls it as a `mcp_tool` handler, and the supervisor can approve, block, or rewrite.

**How to drive each one — verified mechanism table:**

| Mechanism | Agents | Read progress | Send instruction | Detect error |
|---|---|---|---|---|
| **ACP over stdio** | Gemini CLI, Qwen Code, OpenCode, Goose (native); Codex, Claude Code (adapters) | `session/update` (tool_call, agent_message_chunk, plan) | `session/prompt` | `session/stop` **unimplemented by all 39 probed agents** → kill via process handle |
| **HTTP/OpenAPI** | OpenCode `serve`, Qwen `serve`, Crush `server` | SSE / NDJSON | POST session/prompt | HTTP codes; Qwen bearer-token 401/403 |
| **JSONL stdout** | Codex `exec --json`, Gemini `--output-format json` | Event stream | New invocation / `resumeThread()` | `turn.failed`, `thread.error`, exit code |
| **Agent-SDK** | Claude Code Agent SDK | `query()` async iterator | Same-session multi-turn | `result.is_error` |
| **Hooks (push)** | Codex (14), Claude Code (35) | Hook stdout JSON | — | Hook exit code, `decision:"block"` |
| **PTY (last resort)** | Any TTY-only agent | `node-pty` / `pywinpty` + `pyte` | Write bytes | Exit code; ANSI scraping is weak |
| **MCP as server** | **Only 3rd-party wrappers**: [`steipete/claude-code-mcp`](https://github.com/steipete/claude-code-mcp) (MIT, **archived** 2026-05-15), `disler/aider-mcp-server` (308★), `SDGLBL/mcp-claude-code` (305★), `ezyang/codemcp` (Apache-2.0) | `notifications/tools/list_changed` | `tools/call` | JSON-RPC error / `isError` |

**MCP-server-role reality check:** *no* first-party coding CLI exposes itself as an MCP server. Codex = client. Claude Code = client (no `serve`). Gemini = client. Qwen = client. Goose = client + ACP server. OpenCode = client. Crush = client. Aider = none. The MCP-as-server path is third-party only, and the best-known option is archived. **Use ACP, not MCP, to supervise coding agents.**

**Real orchestrators worth studying:**

| Project | Repo | License | ★ | Notes |
|---|---|---|---|---|
| **vibe-kanban** | [BloopAI/vibe-kanban](https://github.com/BloopAI/vibe-kanban) | Apache-2.0 | 28,579 | Best verified agent-switching + git-worktree model. UI switches among 10+ agents. Rust. Heavy as a library. |
| **claudexor** | [razzant/claudexor](https://github.com/razzant/claudexor) | MIT | 522 | Closest thing to a *supervision* plane (not a fan-out UI): quota-aware rotation, shared thread context, cross-model review across Claude Code/Codex/Cursor/OpenCode. |
| **agent-takkub** | [takkub/agent-takkub](https://github.com/takkub/agent-takkub) | MIT | **4** | Windows-native cockpit: spawns real `claude.exe` per teammate via pywinpty+pyte, isolates each in a git worktree, **evidence-gated QA**, reads Claude Code session JSONL for token metering/resume briefs. Explicitly Windows. |
| **conductor** | [conductor-oss/conductor](https://github.com/conductor-oss/conductor) | Apache-2.0 | 32,407 | Durable workflow engine, Java. Not a coding-agent driver. |
| **agent-of-empires** | [agent-of-empires/agent-of-empires](https://github.com/agent-of-empires/agent-of-empires) | MIT | 3,407 | Rust TUI + web supervisor of multiple Claude Code / OpenCode agents. |
| **claude-squad** | [smtg-ai/claude-squad](https://github.com/smtg-ai/claude-squad) | **AGPL-3.0** | 8,725 | PTY-scraping, not protocol-level. Stale 2026-08-20. |

**Rejected:** `steipete/claude-code-mcp` (archived, superseded by the Agent SDK + ACP adapter); `smtg-ai/claude-squad` (AGPL + terminal scraping); `chaitanya-archive/oh-my-codex` (archived); `stravu/crystal` (superseded by `nimbalyst/nimbalyst`); `lastmile-ai/mcp-agent` and `mcp-use/mcp-use` are MCP *clients*, not agent servers; `ruvnet/ruflo` is a harness that would *replace* your coding agents; **Aider as a supervised target** — no machine-readable output at all; `conductor-build/conductor` 404s (→ `conductor-oss/conductor`).

### 3.6 Agent orchestration, memory and context

| Project | Repo | License | ★ | Last push | Role | External deps |
|---|---|---|---|---|---|---|
| **LangGraph** | [langchain-ai/langgraph](https://github.com/langchain-ai/langgraph) | MIT | 44,406 | 2026-11-03 | Pregel graph runtime, `interrupt()`, `Command(resume=…)`, time-travel, SQLite checkpointer | **None** (SQLite in monorepo) |
| **Microsoft Agent Framework** | [microsoft/agent-framework](https://github.com/microsoft/agent-framework) | MIT | 14,519 | 2026-11-03 | Graph workflows, `FileCheckpointStorage`, formally-specified approval-resume boundary | **None** (file-based) |
| **Pydantic AI** | [pydantic/pydantic-ai](https://github.com/pydantic/pydantic-ai) | MIT | 21,014 | 2026-11-03 | Typed agent loop, `StepPersistence` snapshots, Harness (compaction/planning/guardrails) | SQLite default; 9 optional durable engines |
| **Mastra (TS)** | [mastra-ai/mastra](https://github.com/mastra-ai/mastra) | Apache-2.0 core, **proprietary `ee/`** | 29,312 | 2026-11-03 | Workflows `.then/.branch/.parallel`, documented suspend/resume | libSQL file-backed |
| **CrewAI** | [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | MIT | 60,455 | 2026-11-03 | Crews + Flows, SQLite flow persistence | SQLite default |
| **OpenAI Agents SDK** | [openai/openai-agents-python](https://github.com/openai/openai-agents-python) | MIT | 30,477 | 2026-11-03 | Handoffs, guardrails, sessions | SQLite session store |
| **mem0** | [mem0ai/mem0](https://github.com/mem0ai/mem0) | Apache-2.0 | 68,343 | 2026-10-30 | Fact extraction/recall | **Default LLM = OpenAI `gpt-5-mini`** — must repoint at Ollama for local-first |
| **Cognee** | [topoteretes/cognee](https://github.com/topoteretes/cognee) | Apache-2.0 | 33,029 | 2026-11-03 | Docs → knowledge graph, local GLiNER extraction | **Runs locally, no API key** |
| **Graphiti** | [getzep/graphiti](https://github.com/getzep/graphiti) | Apache-2.0 | 32,491 | 2026-11-03 | Bi-temporal KG memory | **Requires Neo4j / FalkorDB / Neptune** + an LLM per extraction |
| **Engram** | [Gentleman-Programming/engram](https://github.com/Gentleman-Programming/engram) | MIT | 7,758 | 2026-11-03 | **Single Go binary, SQLite + FTS5, MCP server + CLI + TUI.** Sets `TEMP`/`TMP` before opening SQLite on Windows — a real Win11 detail most projects miss. | **Zero** |
| **ai-memory** | [akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory) | MIT | 15,013 | 2026-11-03 | Git-backed **plain markdown** memory; *"the database is a derived index that can always be rebuilt from the files"* | Local binary |
| **sqlite-vector** | [sqliteai/sqlite-vector](https://github.com/sqliteai/sqlite-vector) | Apache-2.0 | 1,206 | 2026-11-01 | Faiss-based SQLite vector ext, ~30 MB RAM, quantization, prebuilt Windows x86 | **Local file** |
| **Temporal (dev server)** | [temporalio/temporal](https://github.com/temporalio/temporal) | MIT | 24,165 | 2026-11-03 | Durable workflows; **dev server defaults to SQLite** and ships a Windows amd64 binary | SQLite (dev) / Postgres (prod) |
| **APScheduler** | [agronholm/apscheduler](https://github.com/agronholm/apscheduler) | MIT | 7,679 | 2026-10-26 | Scheduler + job queue, SQLite jobstore | SQLite |
| **llama.cpp / Ollama** | [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp), [ollama/ollama](https://github.com/ollama/ollama) | MIT | 133,277 / 185,798 | 2026-11-03 | Local model serving | **vLLM has no native Windows build** |

#### The decisive framework comparison

| Framework | Interrupt / resume | Durable state | HITL | Long-task support | Local-first | Verdict for a desktop operator |
|---|---|---|---|---|---|---|
| **LangGraph** | ✅ `interrupt()` + `Command(resume=…)` | ✅ SQLite checkpointer + time-travel | ✅ first-class | ✅ built for it | ✅ MIT, SQLite-only | **Best overall.** Only MIT framework where durable execution, interrupt/resume and time-travel are all first-class with a single-file store. |
| **Microsoft Agent Framework** | ✅ approval-resume boundary, atomic restore | ✅ `FileCheckpointStorage` | ✅ | ✅ | ✅ MIT, file-based | **Best runner-up.** The file-based checkpoint storage is a genuine desktop fit. |
| **Pydantic AI** | ✅ deferred tools / approval | ✅ `StepPersistence` per settled step | ✅ | ✅ "run for days" | ✅ SQLite Harness | Best typed option; durability is delegated rather than owned. |
| **Mastra** | ✅ documented suspend/resume | ✅ libSQL | ✅ | ✅ | ✅ libSQL | Best TS option; **proprietary `ee/`** is a governance risk. |
| **CrewAI** | ⚠️ blocking console prompt | ✅ SQLite | ⚠️ | ⚠️ event-driven, not checkpoint-resume | ✅ | Good for batch delegation, **weak for interruptible long tasks**. |
| **Temporal** | ✅ signals | ✅ core strength | ✅ | ✅ built for it | ⚠️ dev server = SQLite | Best durability engine, **worst ergonomics** for a desktop app. |
| **AutoGen** | ❌ | ❌ | ❌ | ❌ | — | **DEAD** — Microsoft archived it; last release 2025-09-30; license **CC-BY-4.0**. |
| **OpenAI Swarm / Semantic Kernel / smolagents / Swarms** | ❌ | ❌ | ❌ | ❌ | — | Educational references or chat wrappers. Not a brain for an operator. |

**Rejected:** `microsoft/autogen` (archived, CC-BY-4.0); `microsoft/semantic-kernel` (superseded — its README now points to MAF); `openai/swarm` (replaced by the Agents SDK); `restatedev/restate` (**BSL 1.1**, source-available); `inngest/inngest` server (**SSPL-1.0**, not OSI — SDK-only is Apache-2.0); `triggerdotdev/trigger.dev` (self-host = Postgres ×2 + Redis + MinIO + ClickHouse — unacceptable for a desktop install); `hatchet-dev/hatchet` (Postgres + Docker; CLI Mac/Linux/WSL only); `dbos-inc/dbos-transact-py` (Postgres required); `plastic-labs/honcho` (**AGPL-3.0** + Postgres + Redis via Docker); `reorproject/reor` (archived, AGPL); `khoj-ai/khoj` (AGPL + Docker); `LostRuins/koboldcpp` (**AGPL-3.0**); `akitaonrails/ai-memory`… kept, but note the ecosystem default of cloud-LLM extraction in mem0 is the trap.

### 3.7 MCP and integration protocols

| Protocol | Spec repo | License | Status | Who implements it | Role for you |
|---|---|---|---|---|---|
| **MCP** | [modelcontextprotocol/modelcontextprotocol](https://github.com/modelcontextprotocol/modelcontextprotocol) | **NOASSERTION — MIT → Apache-2.0 transition** | Latest revision **2026-07-28**; transports: `stdio` + `streamable-http` only (**HTTP+SSE deprecated** since 2025-03-26, SEP-2596) | Every host | The tool bus |
| **MCP Apps** | [modelcontextprotocol/ext-apps](https://github.com/modelcontextprotocol/ext-apps) | NOASSERTION (same transition) | **SEP-1865**, revision 2026-01-26 | Claude, ChatGPT, VS Code, Goose, Postman | Render tool UIs in-chat. **This is the "MCP UI" spec.** |
| **MCP Skills** | [modelcontextprotocol/ext-skills](https://github.com/modelcontextprotocol/ext-skills) | Apache-2.0 | **SEP-2640, Status Final** (merged 2026-09-13), extension Beta | Per client matrix | Serve `SKILL.md`-style capability packs |
| **MCP Python SDK** | [modelcontextprotocol/python-sdk](https://github.com/modelcontextprotocol/python-sdk) | MIT | 24,747★ | — | Build your Windows MCP server |
| **FastMCP v3** | [PrefectHQ/fastmcp](https://github.com/PrefectHQ/fastmcp) (`jlowin/fastmcp` redirects here) | Apache-2.0 | v3 | — | *"some version of FastMCP powers 70% of MCP servers across all languages"* |
| **MCP Registry / MCPB / Inspector** | [registry](https://github.com/modelcontextprotocol/registry), [mcpb](https://github.com/modelcontextprotocol/mcpb), [inspector](https://github.com/modelcontextprotocol/inspector) | various | Active | — | Discovery, one-click desktop install, testing |
| **ACP** | [agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol) | **Apache-2.0** | **Stable version 1** (v1 + v2 schemas, negotiated) | Clients: Zed, neovim, VS Code, JetBrains, Emacs, Sublime, Unity. Agents: Cursor, Cline, Claude Code, Codex, Gemini CLI, OpenCode | **The coding-agent bus** |
| **A2A** | [a2aproject/A2A](https://github.com/a2aproject/A2A) | Apache-2.0 | v1.0.1, Linux Foundation | Broad | Delegate desktop sub-tasks to other agents |
| **AG-UI** | [ag-ui-protocol/ag-ui](https://github.com/ag-ui-protocol/ag-ui) | **MIT** | ~16 event types, transport-agnostic | LangGraph, CrewAI, Mastra, LlamaIndex, Claude Agent SDK, CopilotKit | **The UI bus** for your own desktop app |
| **AGENTS.md** | [agentsmd/agents.md](https://github.com/agentsmd/agents.md) | MIT | Active | Codex, Cursor, Claude Code | Per-project agent instructions |

**Practical MCP servers to build on for Windows:**

| Server | Repo | License | ★ | Note |
|---|---|---|---|---|
| **FlaUI-MCP** | [shanselman/FlaUI-MCP](https://github.com/shanselman/FlaUI-MCP) | MIT | 110 | Windows desktop automation MCP via FlaUI/UIA — best-known native-Windows option |
| **desktop-touch-mcp** | [Harusame64/desktop-touch-mcp](https://github.com/Harusame64/desktop-touch-mcp) | MIT | 25 | *Semantic discover-then-act* (entities + leases, not pixel coords), native Rust UIA engine, Chrome CDP |
| **Nova** | [bigduu/Nova](https://github.com/bigduu/Nova) | MIT | 22 | Cross-platform (macOS + **Windows**) AX/UIA computer-use MCP |
| **Windows-MCP** | [CursorTouch/Windows-MCP](https://github.com/CursorTouch/Windows-MCP) | MIT | 11,012 | Full OS-level MCP server (fs, apps, process, registry, clipboard, notifications, PowerShell, UI) + three-tier ownership gate |
| **mcp-windows** | [sbroenne/mcp-windows](https://github.com/sbroenne/mcp-windows) | MIT | 127 | UIA-first, elevation + secure-desktop detection |
| **mcp-ui** | [MCP-UI-Org/mcp-ui](https://github.com/MCP-UI-Org/mcp-ui) | Apache-2.0 | 5,300 | Community UI-over-MCP; now behind the official `ext-apps` but most deployed |

**Rejected:** `FlowiseAI/Flowise` (**archived** — README points to Future of Flowise); `mcp-router/mcp-router` (**EOL 2026-09-18**); `lastmile-ai/mcp-agent` (dormant since 2026-01-25); `lasso-security/mcp-gateway` (stale); `langchain-ai/langgraph-api` and `langgraph-cli` **404 — the LangGraph Platform/Agent Server is not open-source on GitHub** (LangGraph itself is MIT and usable standalone); `Cisco/AGNTCY` 404 (org is `agntcy`, low adoption); `agentprotocol/agentprotocol` — no canonical repo.

### 3.8 Benchmarks — use these to measure your own progress

| Benchmark | Repo | License | ★ | What it gives you |
|---|---|---|---|---|
| **WindowsAgentArena** | [microsoft/WindowsAgentArena](https://github.com/microsoft/WindowsAgentArena) | MIT | 915 | Reproducible **Windows 11** task benchmark; ships the "Navi" agent + OmniParser mode. Needs a Win11 Enterprise Eval ISO + 30 GB golden image VM. |
| **OSWorld** | [xlang-ai/OSWorld](https://github.com/xlang-ai/OSWorld) | Apache-2.0 | 3,250 | Real-computer task success rate. `desktop_env/` lives *inside* this repo. |
| **OSWorld-V2** | [xlang-ai/OSWorld-V2](https://github.com/xlang-ai/OSWorld-V2) | Apache-2.0 | 406 | Long-horizon tasks — the honest bar. |
| **ScreenSpot-Pro** | [likaixin2000/ScreenSpot-Pro-GUI-Grounding](https://github.com/likaixin2000/ScreenSpot-Pro-GUI-Grounding) | MIT | 398 | High-res professional GUI grounding. |
| **OSWorld-G / Jedi** | [xlang-ai/OSWorld-G](https://github.com/xlang-ai/OSWorld-G) | Apache-2.0 | 185 | Grounding-only subset. |

---

## Part 4 — Best Modular Combinations

### Combination A — **Minimal** (fewest components, good coverage)

**~6 repos, 2 languages, zero external services.**

| Component | Repo | Contributes |
|---|---|---|
| Shell / gateway | [`openclaw/openclaw`](https://github.com/openclaw/openclaw) | Sessions, permissions, channels, MCP client+server, ACP client, skills, plugins, local memory |
| Windows UI | [`openclaw/openclaw-windows-node`](https://github.com/openclaw/openclaw-windows-node) | Native WinUI shell, tray, node mode, capability permissions |
| Voice STT/TTS | [`k2-fsa/sherpa-onnx`](https://github.com/k2-fsa/sherpa-onnx) | Streaming ASR + TTS + VAD as Windows binaries |
| Screen capture | [`NiiightmareXD/windows-capture`](https://github.com/NiiightmareXD/windows-capture) | Per-window WGC capture |
| UIA semantics | [`yinkaisheng/Python-UIAutomation-for-Windows`](https://github.com/yinkaisheng/Python-UIAutomation-for-Windows) | The accessibility tree — the actual difference between "works" and "doesn't" |
| Computer-use driver | [`trycua/cua`](https://github.com/trycua/cua) | The driver OpenClaw's `cua-computer` plugin already wraps |

**Communication:** OpenClaw Gateway (WS) ⇄ Control UI / Windows Hub; ACP over stdio ⇄ coding agents; MCP over stdio ⇄ your Python actuator.
**Missing:** grounded visual fallback for canvas content; long-task orchestration semantics; durable memory beyond OpenClaw's.
**Custom work:** a Windows actuator MCP server exposing UIA-first verbs; a sherpa-onnx ↔ OpenClaw voice bridge.
**Difficulty:** **Low–medium.** You are replacing one plugin, not building a system.

### Combination B — **Balanced** (recommended)

**~12 repos. Adds the layers that actually determine reliability.**

| Layer | Repo(s) | Contributes |
|---|---|---|
| Shell / gateway / policy | `openclaw/openclaw` + `openclaw/openclaw-windows-node` | Everything in Combination A |
| Brain / orchestration | [`langchain-ai/langgraph`](https://github.com/langchain-ai/langgraph) (MIT) + `langgraph-checkpoint-sqlite` | Interruptible, resumable, durable task graphs with time-travel |
| Local model | `ggml-org/llama.cpp` or `ollama/ollama` | Local inference (no vLLM on Windows) |
| Voice | `sherpa-onnx` (STT/TTS/VAD) + `snakers4/silero-vad` + a `wyoming`-style daemon split | Always-on listening plane, separate process |
| Vision | `NiiightmareXD/windows-capture` + `RapidAI/RapidOCR` + `xlang-ai/OSWorld-G` (Jedi-3B) | Capture → OCR → grounding, all local |
| Actuator | `yinkaisheng/Python-UIAutomation-for-Windows` + `pywinauto` (win32 fallback) + `enigo-rs/enigo` or `pynput` for raw input | UIA-first actuation, pixel fallback |
| Browser | `microsoft/playwright` + `microsoft/playwright-mcp` | DOM-level browser control |
| Terminal | `microsoft/node-pty` (or `andfoy/pywinpty`) | ConPTY for interactive CLIs |
| OS | `giampaolo/psutil` + `mhammond/pywin32` + `gorakhargosh/watchdog` | Processes, services, windows, clipboard, FS events |
| Coding agents | ACP: `opencode acp`, `gemini --acp`, `qwen --acp`, `codex-acp`, `claude-agent-acp` | Supervision of external agents |
| Memory | `Gentleman-Programming/engram` (Go binary, SQLite+FTS5, MCP) or `topoteretes/cognee` | Local-first, no vector DB server |
| Security | `microsoft/windows-rs` Job Objects | Kill-the-tree containment for every spawned command |

**Communication:** OpenClaw Gateway ⇄ LangGraph service (HTTP + SQLite checkpointer) ⇄ subsystem MCP servers (stdio) ⇄ LangGraph → ACP client → coding agents.
**Missing:** nothing structural. Weakest link is computer-use reliability itself (Agent-S's OSWorld-V2 SOTA is 21.3%).
**Custom work:** the UIA-first actuator MCP server; the voice bridge; the LangGraph↔OpenClaw bridge; a permission broker.
**Difficulty:** **Medium.** The bulk of the work is integration glue, not new algorithms.

### Combination C — **Advanced** (most capable, most complexity)

**~18 repos. Adds deterministic replay, an AI-native layer, and production-grade orchestration.**

Combination B **plus**:

| Addition | Repo | Contributes |
|---|---|---|
| Deterministic replay | `OpenAdaptAI/OpenAdapt` (+ `openadapt-evals`, which scores on Windows Agent Arena) | Compile demonstrated tasks into no-model local programs with effect verification |
| Windows-native GUI agent | `microsoft/UFO` | GUI-vs-native-API arbitration; app-specific API adapters for Excel/Word/PowerPoint |
| Advanced capture | `ra1nty/DXcam` | Multi-monitor, fast DDA grabs (the multi-monitor gap in OpenClaw's `cua-computer`) |
| Deeper grounding | `QwenLM/Qwen3-VL` (points **and** boxes) or `bytedance/UI-TARS` GGUF via llama.cpp | Reasoning + action in one call, fully local |
| Durable execution | `temporalio/temporal` dev server (SQLite) | Long-horizon workflows that survive process restarts |
| MCP control plane | `samanhappy/mcphub` or `PrefectHQ/fastmcp` | Server registry, auth, one-command MCP distribution |
| UI bus | `ag-ui-protocol/ag-ui` + `assistant-ui` | Typed agent→UI event stream for a rich desktop UI |
| Voice (alt) | `moonshine-ai/moonshine` | Full on-device STT+TTS+agent in one MIT library |

**Communication:** same as B, plus OpenAdapt as a *sidecar* that intercepts repeatable task families and substitutes deterministic programs for VLM loops — the key cost optimisation and the key reliability win.
**Missing:** Windows secure-desktop (UAC prompt) interaction — genuinely hard, requires a signed helper.
**Custom work:** everything in B, plus the OpenAdapt↔actor bridge and a policy layer deciding "model vs. recorded program".
**Difficulty:** **High.** Worth it only after B is working.

---

## Part 5 — Recommended Architecture

### 5.1 Diagram

```
╔═══════════════════════════════════════════════════════════════════════════════════════╗
║  J.E.A.N.  —  Windows 11 Desktop Operator Shell                                          ║
║  WinUI 3 / Tray  ·  Voice Orb  ·  Live Task Board  ·  Approval Queue  ·  Screen Mirror   ║
║  github.com/openclaw/openclaw-windows-node  (MIT)  ← native shell, node-mode capabilities║
╚═══════════════════════════════╤═══════════════════════════════════════════════════════╝
                                │  AG-UI event stream (SSE) + HTTP commands
┌───────────────────────────────▼───────────────────────────────────────────────────────────┐
║  GATEWAY / POLICY CORE   —   openclaw/openclaw  (MIT, 393,843★)                           ║
║  sessions · channel routing · permission ledger · plugin host · skills · ClawHub            ║
║  MCP server ⇄ MCP client        ACP client (acpx)        A2A                              ║
└──┬──────────────┬────────────────────┬───────────────────────┬───────────────────────────┘
   │              │                    │                       │
   │ MCP stdio    │ MCP stdio         │ ACP stdio             │ in-process
   ▼              ▼                    ▼                       ▼
┌─────────────┐ ┌──────────────────┐ ┌────────────────────┐ ┌──────────────────────────────┐
│ WINDOWS     │ │ COGNITION        │ │ SUPERVISOR         │ │ MEMORY                       │
│ ACTUATOR    │ │ PLANE            │ │ PLANE              │ │                             │
│ (Python)    │ │ (Python)         │ │ (LangGraph, MIT)   │ │ Engram (Go, SQLite+FTS5,    │
│             │ │                  │ │                     │ │ MCP) or Cognee              │
│ • UIA tree  │ │ • LangGraph      │ │ • interrupt()/resume│ │                             │
│ • pywinauto │ │ • checkpoint     │ │ • SQLite checkpoint│ │ • no vector DB server       │
│   win32 fb  │ │ • local LLM      │ │ • task DAG          │ │ • zero Docker                │
│ • enigo /   │ │   (llama.cpp)    │ │ • error recovery    │ │ • Windows-native binary      │
│   pynput    │ │ • VLM for vision │ │ • self-verification │ │                             │
│ • Playwright│ │ • compaction     │ │ • HITL approval     │ │                             │
│ • node-pty  │ │ • guardrails     │ │ • human checkpoint  │ │                             │
│ • psutil    │ │                  │ │                     │ │                             │
│ • Job Objs  │ │                  │ │                     │ │                             │
└──────┬──────┘ └────────┬─────────┘ └──────────┬──────────┘ └─────────────────────────────┘
       │                 │                      │
       │           ┌─────▼──────────────────────▼─────┐
       │           │  EXTERNAL CODING AGENTS (ACP)      │
       │           │  Claude Code · Codex · OpenCode   │
       │           │  Gemini CLI · Qwen Code · Goose    │
       │           │  Copilot CLI · Qwen CLI · Qoder    │
       │           └───────────────────────────────────┘
       │
┌──────▼───────────────────────────────────────────────────────────────────────────┐
│  ALWAYS-ON LISTENING PLANE  —  separate OS processes, survives agent stalls        │
│                                                                                  │
│  ┌──────────────┐   ws://   ┌────────────────────┐   ┌────────────────────────┐   │
│  │ sherpa-onnx  │──────────▶│  VOICE BRIDGE      │   │ wake: wyoming-openwake-│   │
│  │ streaming ASR│◀──────────│  (MCP tool:        │   │ word (own trained      │   │
│  │ + TTS + VAD  │  audio    │   speak/stop/listen)│   │ model, Apache-2.0 code)│   │
│  │ win x64 bin  │           └────────────────────┘   └────────────────────────┘   │
│  └──────────────┘                                                                  │
│  Barge-in / interruption is handled HERE, never inside the agent's tool loop.       │
└──────────────────────────────────────────────────────────────────────────────────┘

╔═══════════════════════════════════════════════════════════════════════════════════════╗
║  VISION PLANE  —  capture → OCR → grounding → UIA cross-check → coordinate          ║
║  windows-capture (per-window WGC)  │  DXcam (multi-monitor DDA)  │  RapidOCR      ║
║  → Jedi-3B / Qwen3-VL (local GGUF)  →  uiautomation hit-test  →  pixel fallback    ║
╚═══════════════════════════════════════════════════════════════════════════════════════╝
```

### 5.2 Component responsibilities and interfaces

| Component | Responsibility | Interface | Runs as |
|---|---|---|---|
| **Windows Hub (OpenClaw)** | Native shell, capability negotiation, user-facing approval UX | WinUI ↔ Gateway WS | Win32 process |
| **Gateway (OpenClaw)** | Session state, policy enforcement, plugin/skill registry, channel routing | WS + HTTP | Node process |
| **Listening plane** | Continuous mic capture, VAD, wake detection, streaming ASR, TTS playback | WebSocket / Wyoming protocol | **2–3 independent OS processes** |
| **Voice bridge** | Bridges the listening plane into the tool surface as `speak()`, `listen()`, `stop_speaking()`; owns barge-in policy | MCP (stdio) | Python |
| **Cognition plane** | Intent parsing, planning, tool selection, vision-model invocation, compaction | HTTP (internal) | Python service |
| **Windows Actuator** | The only component permitted to touch the desktop. UIA-first, pixel-fallback. | MCP (stdio) | Python process |
| **Supervisor plane (LangGraph)** | Durable task graphs, interrupt/resume, error recovery, HITL | HTTP + SQLite checkpointer | Python service |
| **Memory** | Durable facts, session transcripts, task history | MCP (stdio) | Go binary (Engram) |
| **Coding-agent connectors** | Launch, observe, and correct external coding agents | ACP (stdio) | Spawned per agent |

### 5.3 Which components must be independent services

**Must be separate processes (architectural requirement):**
1. **Listening plane.** Reason: verified interruption bugs in both Pipecat and LiveKit that stall long-running tool calls. Separation makes the failure impossible.
2. **Windows Actuator.** Reason: it needs elevation decisions, and it must be killable independently of the cognition plane. A hung mouse action must not take down the voice.
3. **Supervisor plane.** Reason: it owns the SQLite checkpointer; a cognition crash must resume, not lose state.
4. **Memory.** Reason: single Go binary, zero deps, and it must survive a full-stack restart.

**Should be in-process:** cognition + grounding-VLM invocation (same process avoids a large image round-trip and lets you batch capture→ground).

**Must NOT be a separate process:** the UIA tree snapshot and the coordinate resolution that consumes it — they must be atomic, or the window moves between them and the click lands on the wrong control.

### 5.4 Local-first vs. cloud

| Function | Recommendation | Reason |
|---|---|---|
| Screen capture, UIA tree, OCR | **Always local** | Privacy; latency; no API key |
| Grounding VLM | **Local (GGUF via llama.cpp)** | vLLM has no Windows build; GGUF quants exist for UI-TARS-1.5-7B/2B, ShowUI-2B, OpenCUA-7B |
| Planner LLM | **Local by default, cloud as an opt-in fallback** | llama.cpp/Ollama are MIT and Windows-native; local keeps prompts off the wire |
| STT/TTS | **Local** | sherpa-onnx ships Windows binaries; Moonshine is MIT end-to-end |
| Coding agents | **Local processes** | Codex/Claude Code/OpenCode all run locally; you supervise processes, not APIs |
| Memory | **Local** | Engram = SQLite file; no vector DB server, no Docker |

**Hybrid rule:** the system must function **fully offline** with a local model, with cloud as an explicit per-task upgrade. Any architecture that breaks when the network drops is not a desktop assistant; it is a thin client.

---

## Part 6 — Build Versus Reuse Analysis

### 6.1 Reuse wholesale — do not rebuild

| Take | From | Why reuse beats building |
|---|---|---|
| Gateway, sessions, permissions, channels, skills, plugins | `openclaw/openclaw` (MIT) | Years of work: policy ledger, pairing/allowlists, plugin SDK, ClawHub, MCP server *and* client. |
| Native Windows shell | `openclaw/openclaw-windows-node` (MIT) | WinUI tray, Command Center, node-mode capability negotiation, ARM64 + x64 installers. |
| Windows UI Automation binding | `yinkaisheng/Python-UIAutomation-for-Windows` (Apache-2.0) | comtypes binding, pattern support, tree-dump CLI. Writing a UIA client is months. |
| Win32 fallback + typed input | `pywinauto/pywinauto` (BSD-3) | Dual backend, `type_keys`, handle-level control that crosses integrity levels where synthetic input cannot. |
| Correct DPI handling | `enigo-rs/enigo` (MIT) | Per-monitor-v2 awareness solved and documented. |
| Streaming speech | `k2-fsa/sherpa-onnx` (Apache-2.0) | Prebuilt **Windows x64/arm64 + CUDA** binaries, websocket server, streaming ASR/TTS/VAD in one dependency. |
| Browser control | `microsoft/playwright` (Apache-2.0) | Bundled Chromium, no dependency on the user's browser. |
| Console interaction | `microsoft/node-pty` (MIT) | Correct ConPTY lifecycle; `samples/ConPTY/EchoCon` in `microsoft/terminal` as reference. |
| Process/OS introspection | `giampaolo/psutil` (BSD-3) | Zero-dependency, cross-platform, Windows service enumeration included. |
| Coding-agent supervision | ACP clients in `openclaw` (`acpx`) | Already drives Claude Code, Codex, OpenCode, Qwen, Copilot, Kilo, Qoder. |
| Local model serving | `ggml-org/llama.cpp` (MIT) | Windows-native CPU/AVX2/AMX. |
| Durable orchestration | `langchain-ai/langgraph` + SQLite checkpointer (MIT) | Interrupt/resume/time-travel, single-file state. |
| Local memory | `Gentleman-Programming/engram` (MIT) | Single binary, SQLite+FTS5, MCP, correct Windows `TEMP`/`TMP` handling. |
| Screen capture | `NiiightmareXD/windows-capture` (MIT) + `ra1nty/DXcam` (MIT) | Per-window WGC + fast multi-monitor DDA. |

### 6.2 Build yourself — the genuine gaps

| Build | Why nothing reusable covers it |
|---|---|
| **UIA-first actuator with a pixel fallback and a verification step** | OpenClaw's `cua-computer` is pixel-only, primary-display-only, and explicitly experimental. UFO does UIA but is a research framework without voice, ACP or skills. `Harusame64/desktop-touch-mcp` is 25★ with a Rust UIA engine — a reference, not a dependency. **This is the highest-value original work in the whole project.** |
| **Elevation / integrity broker** | A non-elevated agent cannot touch elevated windows and gets *no error*. Windows-MCP has a permission gate; `sbroenne/mcp-windows` has `ElevationDetector`. Neither handles the secure desktop (UAC prompt) — that needs a signed helper and is genuinely hard. |
| **Voice bridge with correct barge-in** | Neither Pipecat nor LiveKit gets mid-tool-call interruption right. A desktop operator must. |
| **Policy broker (3-tier: read-only / normal / elevated-destructive)** | `CursorTouch/Windows-MCP` has the relaxed/balanced/strict pattern — **reuse the pattern, reimplement the logic** so it spans the actuator, shell, file ops and coding agents uniformly. |
| **Job-Object containment wrapper** | `microsoft/windows-rs` exposes `Win32_System_JobObjects`; no Python-ready equivalent exists. A ~150-line ctypes/pywin32 wrapper gives you kill-the-tree for every spawned command. |
| **Multi-monitor coordinate mapping** | OpenClaw's `cua-computer` is primary-display-only. Combining `windows-capture` (per-window) with `DXcam` (per-output) and a virtual-desktop→physical coordinate transform is original work. |
| **Deterministic-replay bridge** | `OpenAdaptAI/OpenAdapt` compiles demos into local programs but has no voice, no ACP, and no integration with an agent's tool loop. Wiring it in as a "cheaper path" for repeatable task families is original and high-value. |

### 6.3 Adopt, don't build (out of scope)

| Category | Decision | Why |
|---|---|---|
| **A UIA client** | Adopt `uiautomation` + `pywinauto` | Months of COM edge cases already solved |
| **An OCR engine** | Adopt `RapidOCR` | ONNX, CPU-only, Windows-clean |
| **A VLM** | Adopt Jedi-3B / Qwen3-VL GGUF | Training is out of scope |
| **A browser driver** | Adopt Playwright | Standards-compliant, battle-tested |
| **An MCP framework** | Adopt FastMCP v3 / the official Python SDK | Protocol churn handled upstream |
| **A protocol spec** | Adopt ACP / MCP / AG-UI | Interop is the entire point |
| **A coding agent** | Adopt Claude Code / Codex / OpenCode / Qwen | You want to *drive* them, not replace them |

---

## Part 7 — Final Recommendations

### 1. Best project to study first
**[`openclaw/openclaw`](https://github.com/openclaw/openclaw)** (MIT, 393,843★) — then its Windows node [`openclaw/openclaw-windows-node`](https://github.com/openclaw/openclaw-windows-node).
**Why:** it is the only project where voice, screen, browser, shell, MCP (both roles), ACP, skills, plugins, local memory and a native Windows app already coexist under one MIT license. Studying it answers every integration question before you write code. Study `extensions/acpx` and `extensions/cua-computer` first — the first is your coding-agent answer, the second is the gap you will fill.

### 2. Best project to potentially fork
**`openclaw/openclaw`.**
**Why:** MIT, so forking is legally trivial; the architecture already separates the policy core from capabilities via the plugin SDK, so a UIA-first Windows actuator slots in as a plugin rather than a rewrite; and the Windows Hub is a separate repo, so you can diverge on the shell without forking the engine. **Do not fork** `microsoft/UFO` (no voice/ACP/skills), `UI-TARS-desktop` (needs cloud VLM), `Dify`/`n8n`/`LobeHub` (source-available, not OSS).

### 3. Best voice subsystem
**[`k2-fsa/sherpa-onnx`](https://github.com/k2-fsa/sherpa-onnx)** (Apache-2.0) for STT + TTS + VAD, with [`snakers4/silero-vad`](https://github.com/snakers4/silero-vad) (MIT) for VAD and a **self-trained** `openWakeWord` model (Apache-2.0 code; the shipped models are CC BY-NC-SA).
**Why:** it is the only voice stack shipping **prebuilt Windows x64/arm64 binaries plus CUDA win-x64 builds**, with a websocket server — so the listening plane can be three independent daemons that survive any agent stall. Voice is not a library call inside your agent loop; it is a service the agent talks to.
*Swap:* [`moonshine-ai/moonshine`](https://github.com/moonshine-ai/moonshine) if you want one MIT library covering the whole voice stack — but its per-utterance barge-in is documented as "Reserved for future use".

### 4. Best screen recognition subsystem
**A three-stage pipeline you own, not a single model:** [`NiiightmareXD/windows-capture`](https://github.com/NiiightmareXD/windows-capture) (per-window capture) → [`RapidAI/RapidOCR`](https://github.com/RapidAI/RapidOCR) (text) → [`xlang-ai/OSWorld-G`](https://github.com/xlang-ai/OSWorld-G) Jedi-3B (grounding) → **UIA cross-check** via [`uiautomation`](https://github.com/yinkaisheng/Python-UIAutomation-for-Windows), falling back to raw pixels only when no UIA control matches.
**Why:** OmniParser is a *parser*, not a grounder (39.5% self-reported on ScreenSpot-Pro); UI-TARS/OpenCUA/Agent-S all need ~16 GB VRAM and a Linux serving stack (vLLM has no Windows build); Jedi-3B is ~7 GB and runs natively on Windows. The UIA cross-check is what makes clicks *verifiable* rather than hopeful.

### 5. Best GUI control subsystem
**UIA-first, three-layer:** `uiautomation` (tree) → `pywinauto` win32 backend (legacy HWND, and `SendMessage` which crosses integrity levels where synthetic input cannot) → `enigo`/`pynput` `SendInput` for raw input, with per-monitor-v2 DPI awareness set manually.
**Why:** the alternative — pixel coordinates only — breaks on mixed-DPI laptops, is limited to the primary display in OpenClaw's implementation, and cannot verify that a click landed. Use `pyautogui` only as a last-resort fallback; it is unmaintained for Win11 (PyPI 2023-05) and has open DPI bugs.
**Reference to read:** `microsoft/UFO` for the GUI-vs-native-API arbitration pattern, and `OpenAdaptAI/OpenAdapt` for the layer-priority rule it states explicitly.

### 6. Best coding-agent integration approach
**ACP, not MCP.** Use [`agentclientprotocol/agent-client-protocol`](https://github.com/agentclientprotocol/agent-client-protocol) (Apache-2.0, stable v1) over stdio, and reuse OpenClaw's existing `acpx` implementation.
**Why:** no first-party coding CLI exposes itself as an MCP server (Codex = client, Claude Code = client with no `serve`, Gemini/Qwen/OpenCode/Crush = clients, Aider = nothing at all); the MCP-as-server path is third-party only and the best-known option is archived. ACP has native support in OpenCode, Gemini CLI, Qwen Code and Goose, with adapters for Claude Code and Codex. Use **MCP for tools** and **AG-UI for your own UI**.
**Per-agent mechanism:** Codex → `codex app-server` JSON-RPC + 14 Codex hooks (verified in `config.schema.json`, including a Windows-native `commandWindows` field) for push-based correction; Claude Code → Agent SDK (`claude-agent-sdk` Python is MIT) + hooks; OpenCode → `opencode serve` HTTP/OpenAPI 3.1; Qwen Code → `qwen serve` SSE daemon with `Last-Event-ID` replay and per-session approval mode; Gemini CLI → `gemini --acp`.
**Caveat:** `session/stop` is unimplemented by every probed agent — kill via the process handle.

### 7. Best orchestration framework
**[`langchain-ai/langgraph`](https://github.com/langchain-ai/langgraph) (MIT) + `langgraph-checkpoint-sqlite`.**
**Why:** it is the only MIT framework where durable execution, `interrupt()`/`Command(resume=…)` and time-travel are all first-class *and* persistence is a single SQLite file with zero external services. A desktop operator needs exactly these three properties: long tasks, human-in-the-loop, and resume-after-crash. `microsoft/agent-framework` is the best runner-up (its `FileCheckpointStorage` is a genuine desktop fit) if you want C# parity. Avoid `AutoGen` (archived), `Trigger.dev` (Postgres ×2 + Redis + MinIO + ClickHouse), `Hatchet`/`DBOS` (Postgres required), and `Inngest`'s server (SSPL-1.0).

### 8. Best overall modular combination
**Combination B (Balanced).** OpenClaw + OpenClaw Windows Hub as the substrate, LangGraph + SQLite as the brain, sherpa-onnx + Silero as a separate listening plane, `windows-capture` + RapidOCR + Jedi-3B + UIA as vision, `uiautomation` + `pywinauto` + `enigo` as the actuator, Playwright + node-pty as browser/terminal, psutil + pywin32 + windows-rs Job Objects as the OS layer, ACP for coding agents, Engram for memory.
**Why:** it is the only combination where every component is permissive-licensed, runs natively on Windows, and requires no Docker, Postgres, Redis or vector DB server — while keeping the one genuinely valuable all-in-one project (OpenClaw) as the shell rather than trying to replace it.

### 9. Best long-term architecture
**OpenClaw's plugin-and-policy shell + a UIA-first Windows actuator you own + an always-on voice plane + LangGraph for durability + ACP for every external agent.**
**Why:** it is the only arrangement that satisfies all seven of your stated requirements simultaneously — voice, screen, mouse/keyboard, OS integration, coding-agent integration, orchestration, and extensibility — while keeping the operator local-first, offline-capable, and Windows-native. The moat you build is not the vision model (commodity, and the best ones are Apache-2.0 or weaker); it is the **Windows-specific actuator and policy layer**: UIA semantics, elevation handling, multi-monitor coordinates, secure-desktop workarounds, and the three-tier permission broker that spans every subsystem. That layer is exactly where no mature open-source project competes.

---

## Part 8 — Implementation Roadmap

### Phase 0 — Foundation (2 weeks, do not skip)

| Item | Detail |
|---|---|
| **Milestone** | A fork of OpenClaw runs on a Win11 box, connects to a local LLM, and answers a typed message. |
| Tasks** | Fork `openclaw/openclaw`; build the Windows Hub; install `llama.cpp` + a 7–8B instruct model; point the Gateway at it; register the ACP client; drive Claude Code through `acpx` end-to-end. |
| **Dependencies** | None — this unblocks everything else. |
| **Done when** | A Claude Code session runs inside your fork and you can read its transcript from the Control UI. |

### Phase 1 — Basic voice interaction (3 weeks)

| Item | Detail |
|---|---|
| **Milestone** | Push-to-talk and wake-word both work; the assistant speaks and listens while idle. |
| Tasks | Install `sherpa-onnx` (Windows x64 binaries) as a **separate daemon**; add Silero VAD; train a custom `openWakeWord` model (Apache-2.0 code, own model → no license issue); build the voice bridge as an MCP server (`speak`, `listen`, `stop_speaking`); wire barge-in **in the voice plane only**, never in the agent's tool loop. |
| **Pitfall to avoid** | Do not implement voice as a callback inside the agent loop. Pipecat's open bug — "Delegated backend work never finishes" — is exactly what that buys you. |
| **Done when** | You can talk to the assistant for 10 minutes while a long-running command executes, and interrupt it mid-sentence with no stuck state. |

### Phase 2 — Screen awareness (3 weeks)

| Item | Detail |
|---|---|
| **Milestone** | The assistant can answer "what's on my screen?" and "where is the Save button?" with a verifiable coordinate. |
| Tasks | Add `windows-capture` (per-window) and `DXcam` (multi-monitor); add RapidOCR; serve Jedi-3B locally via llama.cpp/GGUF; **build the UIA cross-check** (grounded bbox → hit-test the live UIA tree → prefer the control whose `Name`/`AutomationId` matches); add `imagehash` change-detection so you only pay for a VLM call when the screen actually changed. |
| **Decision** | Set per-monitor-v2 DPI awareness **before** any capture or coordinate math. |
| **Done when** | On a real Win11 desktop, "click the Save button in the Save As dialog" lands correctly at 125% scaling on a multi-monitor setup, and you can prove it via the UIA tree rather than by inspection. |

### Phase 3 — Reliable computer control (5 weeks) ← *the hard one*

| Item | Detail |
|---|---|
| **Milestone** | Reliable actuation across native apps, browsers, and dialogs, with a verification step after every action. |
| Tasks | Build the **UIA-first actuator MCP server**: `uiautomation` for the tree, `pywinauto` win32 backend for legacy HWNDs, `enigo`/`pynput` for raw input; add UIA *pattern* invocation (`Invoke`/`Value`/`Selection`) instead of synthetic clicks wherever a pattern exists — patterns are more reliable and can cross some integrity boundaries that `SendInput` cannot; add a **verify-after-act** step (re-read the UIA tree / compare screen hash) so a click is confirmed, not assumed; add the **elevation broker** (detect the target's integrity level, route through an elevated helper or refuse with a clear message instead of failing silently). |
| **Benchmark** | Track against [`microsoft/WindowsAgentArena`](https://github.com/microsoft/WindowsAgentArena) and [`xlang-ai/OSWorld`](https://github.com/xlang-ai/OSWorld) from day one. Publish your own numbers — Agent-S's OSWorld-V2 SOTA is **21.3%**. |
| **Done when** | A 30-task Windows suite you define passes ≥80% with no silent UIPI failures. |

### Phase 4 — PC and application integration (3 weeks)

| Item | Detail |
|---|---|
| **Milestone** | Real OS-level capability: files, processes, services, windows, notifications, clipboard — all permission-gated. |
| Tasks | Wrap `psutil`, `pywin32`, `watchdog`, `pyperclip`, `win11toast`, `ripgrep`/`fd`, `uv`/`mise` as MCP tools; implement the **three-tier permission broker** (read-only / normal / elevated-destructive) as a single choke point — reuse the relaxed/balanced/strict pattern from `CursorTouch/Windows-MCP` but implement the logic centrally; add Job-Object containment (`microsoft/windows-rs`) so every spawned command dies with its tree on timeout; add `send2trash` semantics so "delete" is recoverable. |
| **Done when** | Every OS tool goes through one audit log, and elevated operations require an explicit, visible user approval that cannot be triggered by a model without the user seeing it. |

### Phase 5 — Coding-agent integrations (3 weeks)

| Item | Detail |
|---|---|
| **Milestone** | You can start a coding agent, watch it work, interrupt it, and hand it a correction. |
| Tasks | Wire ACP for OpenCode, Gemini CLI, Qwen Code, Goose (all native); add the Claude Code and Codex adapters; add **Codex hooks** (`PreToolUse`/`PostToolUse`/`PermissionRequest` with `type:"mcp_tool"`) so your supervisor can approve, block, or rewrite Codex's actions in real time; add Codex `app-server` JSON-RPC for thread lifecycle + `item/commandExecution/requestApproval`; implement **kill via process handle** (ACP's `session/stop` is unimplemented everywhere); add worktree isolation via `git worktree` so parallel agents don't collide. |
| **Pitfall to avoid** | Do not build the MCP-server path — no first-party coding CLI supports it and the best wrapper is archived. |
| **Done when** | You can interrupt a runaway Claude Code run, read exactly what it did, and restart it with a correction — all without terminal scraping. |

### Phase 6 — Intelligent orchestration and memory (4 weeks)

| Item | Detail |
|---|---|
| **Milestone** | Multi-step tasks that survive interruptions, crashes, and your own mistakes. |
| Tasks | Move orchestration into **LangGraph** with the SQLite checkpointer; model every long task as an interruptible graph with explicit checkpoints; add a `human_checkpoint` node type for approvals; integrate **Engram** (Go binary, SQLite+FTS5, MCP) for durable memory; add context compaction; add self-verification guardrails (re-check the goal against the observed screen state, not the model's confidence). |
| **Pitfall to avoid** | Do not choose Trigger.dev, Hatchet, DBOS or Inngest's server — each requires Postgres (and Inngest's server is SSPL). |
| **Done when** | Kill the whole app mid-task, restart it, and it resumes from the last checkpoint with full context. |

### Phase 7 — Advanced autonomous workflows (ongoing)

| Item | Detail |
|---|---|
| **Milestone** | The operator handles recurring work with minimal intervention and degrades gracefully. |
| Tasks | Add **OpenAdapt-style deterministic replay** for repeatable task families — compile a demonstration into a no-model local program and route to it when the pattern matches, falling back to the VLM loop otherwise (the single biggest cost and reliability win); add multi-monitor and RDP-session support; add the secure-desktop (UAC) helper if you can sign it; add a task queue for scheduled work (`APScheduler` on SQLite, or Temporal's SQLite dev server); add the A2A bus so the operator can delegate sub-tasks to other agents. |
| **Done when** | A recurring multi-application task completes unattended with ≥95% success, and every failure mode has a typed, human-readable error instead of a silent no-op. |

### Dependency map

```
Phase 0 (fork + LLM + ACP)
   ├──► Phase 1 (voice daemon)          [independent of 2–4 — can run in parallel]
   ├──► Phase 2 (vision + UIA)           [needs Phase 0 only]
   │       └──► Phase 3 (actuator)       [needs 2 for verify-after-act]
   ├──► Phase 4 (OS layer + permissions) [needs Phase 0 only]
   │       └──► Phase 3 (elevation broker)
   └──► Phase 5 (coding agents)          [needs Phase 0 only]

Phase 3 + Phase 4 ──► Phase 6 (orchestration + memory)
Phase 6 ──► Phase 7 (deterministic replay, autonomy)
```

**Critical path:** Phase 0 → Phase 2 → Phase 3 → Phase 6 → Phase 7. Phase 1 (voice), Phase 4 (OS) and Phase 5 (coding agents) can run fully in parallel — start them in week 1.

---

## Part 9 — Security and Permission Architecture

Because this system holds full control of a machine, security is architecture, not a feature.

### The three tiers

| Tier | Examples | Mechanism |
|---|---|---|
| **Read-only** | Screen capture, OCR, UIA tree read, file read, process list, `git status` | No prompt. Fully logged. |
| **Normal interaction** | Mouse/keyboard input within an app, browser navigation, writing files inside a workspace, running `git`/`npm test` | Prompt on first use per scope; allowed thereafter. Job-Object contained. |
| **Elevated / destructive** | Service control, registry writes, `os.remove` outside the workspace, installing software, credential access, process termination of system processes | **Explicit per-action user approval**, always visible, never batch-approved, never auto-approved by the model. |

### Controls to implement

| Risk | Control | Source |
|---|---|---|
| **Elevation / UIPI** | Detect the target's integrity level before acting; route through an elevated helper or refuse with a clear message. Never fail silently. | `sbroenne/mcp-windows` `ElevationDetector`; MS docs: *"neither GetLastError nor the return value will indicate the failure was caused by UIPI blocking"* |
| **Runaway command** | Every spawned process goes into a Job Object with `KILL_ON_JOB_CLOSE`. | `microsoft/windows-rs` `Win32_System_JobObjects` |
| **Destructive file ops** | `send2trash` (Recycle Bin) instead of `os.remove`; workspace-scoped write sandbox. | `arsenetar/send2trash` |
| **Prompt injection from screen/web content** | Treat all screen/OCR/web text as **untrusted data**, never as instructions. Strip it before it reaches the planner's instruction channel; require explicit user phrasing for anything consequential. | `openclaw` docs: *"Treat inbound messages as untrusted input"*; `CursorTouch/Windows-MCP` `control_policy` |
| **Agent-to-agent escalation** | A coding agent's tool calls go through the same permission broker — never a second, laxer path. | Codex `PermissionRequest` hook; Claude Code `PermissionDenied` hook |
| **Credential exposure** | No secret is placed in the prompt; the broker injects scoped credentials only for the specific operation, and redacts them from logs. | — |
| **Screen privacy** | Per-capability toggles, default-off for camera/mic/screen; a visible indicator whenever capture is live; `imagehash`-style history retention is opt-in and bounded. | OpenClaw Windows node: *"Privacy-sensitive capabilities should stay off unless you intend to use them"* |
| **Audit trail** | Every tool call, permission decision, and capture is written to a local append-only log with the acting identity (model, user, or coding agent). | `CursorTouch/Windows-MCP` `control_ledger` / `control_recovery` |
| **Emergency stop** | A global hotkey that: cancels the task graph, releases the Job Object, stops all input queues, and mutes the voice — from a **separate, low-privilege process** that an elevated agent cannot kill. | — |
| **Secure desktop** | UAC prompts are on a separate desktop that UI Automation cannot reach. Either refuse elevated targets, or use a signed helper. Document the limit rather than pretend. | `mario-andreschak/mcp-windows-desktop-automation`: *"elevated windows beyond the server's privileges are not supported"* |

**Non-negotiable design rule:** the model may *request* an action; it may never *grant* it. Every capability decision flows through one broker, and the broker's decisions are visible to the user in real time.

---

## Appendix A — Killed & Renamed Candidates

Every one of these was checked on 2026-11-04. **404** = no such repository.

### Renamed / re-homed (use the right path)

| You may have seen | Canonical path |
|---|---|
| `sst/opencode` | [`anomalyco/opencode`](https://github.com/anomalyco/opencode) |
| `block/goose` | [`aaif-goose/goose`](https://github.com/aaif-goose/goose) |
| `All-Hands-AI/OpenHands` | [`OpenHands/OpenHands`](https://github.com/OpenHands/OpenHands) |
| `anthropics/anthropic-quickstarts` | [`anthropics/claude-quickstarts`](https://github.com/anthropics/claude-quickstarts) |
| `openadapt/OpenAdapt` | [`OpenAdaptAI/OpenAdapt`](https://github.com/OpenAdaptAI/OpenAdapt) |
| `cognee-ai/cognee` | [`topoteretes/cognee`](https://github.com/topoteretes/cognee) |
| `jlowin/fastmcp` | [`PrefectHQ/fastmcp`](https://github.com/PrefectHQ/fastmcp) (v3) |
| `lobehub/lobe-chat` | [`lobehub/lobehub`](https://github.com/lobehub/lobehub) |
| `conductor-build/conductor` | [`conductor-oss/conductor`](https://github.com/conductor-oss/conductor) |
| `coreybutler/nvm-windows` | [`nvm-windows/nvm`](https://github.com/nvm-windows/nvm) |
| `nutomic/nut.js` | [`nut-tree/nut.js`](https://github.com/nut-tree/nut.js) (stale 2024-05) |
| `robotjs/robotjs` | [`octalmage/robotjs`](https://github.com/octalmage/robotjs) |
| `microsoft/pywin32` | [`mhammond/pywin32`](https://github.com/mhammond/pywin32) |
| `microsoft/pywinpty` | [`andfoy/pywinpty`](https://github.com/andfoy/pywinpty) |
| `node-pty/node-pty` | [`microsoft/node-pty`](https://github.com/microsoft/node-pty) |
| `enthought/pyvirtualdisplay` | [`ponty/PyVirtualDisplay`](https://github.com/ponty/PyVirtualDisplay) (Linux/X11 only) |
| `SYSTRAN/whisperX` | [`m-bain/whisperX`](https://github.com/m-bain/whisperX) (batch only) |
| `coqui-ai/StyleTTS2` | [`yl4579/StyleTTS2`](https://github.com/yl4579/StyleTTS2) (dead 2024) |
| `rhasspy/piper` | [`OHF-Voice/piper1-gpl`](https://github.com/OHF-Voice/piper1-gpl) (**GPL-3.0**) |
| `OpenGVLab/OpenCUA` | [`xlang-ai/OpenCUA`](https://github.com/xlang-ai/OpenCUA) |
| `HelloAgAgent/Agent-S` | [`simular-ai/Agent-S`](https://github.com/simular-ai/Agent-S) |
| `zai-org/AutoGLM`, `THUDM/AutoGLM` | [`zai-org/Open-AutoGLM`](https://github.com/zai-org/Open-AutoGLM) (**phone only**) |
| `indykish/rye` | [`astral-sh/rye`](https://github.com/astral-sh/rye) (archived) |
| `nvidia/pynvml` | [`gpuopenanalytics/pynvml`](https://github.com/gpuopenanalytics/pynvml) |
| `iffyio/netifaces` | [`al45tair/netifaces`](https://github.com/al45tair/netifaces) (archived) |
| `simular-ai` vs `similar-ai` | **`simular-ai/Agent-S`** |
| `oi`/`openai-agents-python` | [`openai/openai-agents-python`](https://github.com/openai/openai-agents-python) |

### 404 — never existed or removed

`microsoft/Windows-MCP` (the real one is `CursorTouch/Windows-MCP`) · `voidtools/Everything` (the app is proprietary) · `microsoft/UFO2` (UFO² lives in `microsoft/UFO`) · `bytedance/ComputerAgent` · `appcypher/mcp-server` · `Cisco/AGNTCY` (org is `agntcy`) · `iDot-ai/InternetOfAgents` · `agentprotocol/agentprotocol` · `superiorailboats/jarvis` · `MODSetter/Open-Desktop-Assistant` · `jagilley/openauto` · `langchain-ai/langgraph-api` and `langgraph-cli` (the Agent Server is not open-sourced) · `mcp-router/mcp-router` (EOL) · `nori-nlp/RealtimeVoiceChat` · `ReSpeaker/RealtalTimeSTT` · `coqui-ai/TTS` (repo gone; PyPI remains) · `microsoft/agent-ui` · `xlang-ai/desktop-env` (inside `xlang-ai/OSWorld`) · `conductor-build/conductor` · `ruflo/ruflo` → `ruvnet/ruflo` · `timgolden/wmi` (PyPI only) · `microsoft/Windows-MCP` · `Hcompany/Holo1` · `apple/ferret-ui` (no weights) · `UNVERIFIED`/`similar-ai/Agent-S` · `n8n-io/n8n` was verified present (Sustainable Use License) · `agent-of-empires/agent-of-empires` verified present.

### Archived / dead

| Project | State | Note |
|---|---|---|
| `microsoft/autogen` | Archived by Microsoft | Last release 2025-09-30; license **CC-BY-4.0**; successor is `microsoft/agent-framework` |
| `FlowiseAI/Flowise` | Archived | 55,546★; superseded by Langflow (MIT) |
| `steipete/claude-code-mcp` | Archived | 1,315★; superseded by the Agent SDK + ACP adapter |
| `rhasspy/piper` | Archived | MIT → successor is GPL-3.0 |
| `jorangreef/sudo-prompt` (npm) | Archived | Use `elevate` (PyPI) or `windows-elevate` (npm) |
| `reorproject/reor` | Archived | AGPL-3.0 |
| `astral-sh/rye` | Archived | Use `uv` |
| `al45tair/netifaces` | Archived | Use `psutil.net_io_counters` |
| `sivel/speedtest-cli` | Archived | Use `psutil` + `iperf` |
| `hcompai/surfer-h-cli` | Archived | Holo1 open-weights path closed in favour of a paid API |
| `Kitt-AI/snowboy` | Dead | Last push 2021-10-13 |
| `coqui-ai/TTS` | Dead | Last push 2024-08-16; XTTS-v2 weights are CPML |
| `myshell-ai/MeloTTS`, `yl4579/StyleTTS2` | Dead | 2024 |
| `openinterpreter/openinterpreter` | **Superseded** | Now a Rust Codex-fork *coding* agent; the historic computer-control product is gone |
| `vocodedev/vocode-core` | Dead | Last push 2024-11-15 |
| `KoljaB/RealtimeVoiceChat` | Self-declared unmaintained | No LICENSE file |
| `microsoft/WinAppDriver` | Effectively dead | Last push 2025-04-14; 1,155 open issues; Developer Mode + admin; Win10 docs only |
| `appium/appium-windows-driver` | Deprecated upstream | Not Appium 4 compatible |
| `oblitum/Interception` | Dead | Needs WDK 7.1; admin driver install; LGPL non-commercial/proprietary dual-license |
| `openhandx`? | — | — |

### License traps (source-available, not OSI OSS)

`Dify` (Modified Apache + conditions) · `n8n` (Sustainable Use License) · `LobeHub` (Community License — commercial license required for derivative distribution) · `Open WebUI` (Open WebUI License with branding clause) · `anthropics/claude-code` (**proprietary**) · `Restate` (BSL 1.1) · `Inngest` server (SSPL-1.0) · `microsoft/autogen` (CC-BY-4.0) · `microsoft/OmniParser` repo (CC-BY-4.0 — weights are MIT) · `chatbr/ChatTTS` (AGPL-3.0 + CC BY-NC weights) · `fishaudio/fish-speech` (Community License) · `LostRuins/koboldcpp` (AGPL-3.0) · `skyvern` (AGPL-3.0) · `smtg-ai/claude-squad` (AGPL-3.0) · `mastra-ai/mastra` `ee/` (proprietary) · `plastic-labs/honcho` (AGPL-3.0) · `khoj-ai/khoj` (AGPL-3.0) · `sqlite_orm` (AGPL-3.0) · `AutoHotkey` (GPL-2.0) · `F5-TTS` weights (CC-BY-NC) · `Coqui XTTS-v2` weights (CPML) · `openWakeWord` pretrained models (CC BY-NC-SA) · `TEN-VAD` (Agora non-compete clause) · `modelcontextprotocol/modelcontextprotocol` + `ext-apps` (**NOASSERTION — MIT→Apache-2.0 transition in progress**) · `anthropics/skills` + `openai/skills` (no LICENSE file despite 187k★/28.5k★).

---

## Appendix B — Method & Confidence

- **Discovery:** 8 parallel research agents, each with a disjoint scope (voice; screen/vision; GUI control; Windows OS; coding agents; orchestration/memory; MCP/protocols; all-in-one landscape), each instructed to verify via `gh api` and to flag unverified claims.
- **Independent re-verification:** I re-ran `gh api repos/OWNER/REPO` myself on **85+ repositories** covering every repo named in this report's recommendation tables. All star counts, licenses, languages and last-push dates in this document are from those calls, not from the subagents.
- **Corrections applied to subagent output:** `simular-ai/Agent-S` (one agent wrote `similar-ai`); `modelcontextprotocol/ext-apps` license is **NOASSERTION** (licensing transition), not clean Apache-2.0; `modelcontextprotocol/modelcontextprotocol` likewise NOASSERTION; `FastMCP` is at `PrefectHQ/fastmcp` (v3) with `jlowin/fastmcp` redirecting; `pipecat-ai/pipecat` has **zero** `windows-latest` CI hits (independently re-verified by code search, not just workflow listing).
- **Source-inspected, not assumed:** Codex's 14 hook event names were read directly out of `codex-rs/core/config.schema.json`; `sherpa-onnx`'s Windows x64/arm64 + CUDA binaries were confirmed from the `v2.0.1` release assets; OpenClaw's Windows capability table and its `acpx`/`cua-computer` extension READMEs were read in full; OpenClaw's computer-use docs were fetched to confirm the Windows limitations.
- **Confidence markers:** **UNVERIFIED** = could not be confirmed from source or docs at research time (e.g. UGround's weight license, A2A cross-implementation interop, Crush's ACP support, Amp's CLI surface). **Self-reported** = the number comes from the project's own README (Agent-S's OSWorld figures, OmniParser's ScreenSpot-Pro figure).
- **Known limits of this research:** GitHub star counts are point-in-time (2026-11-04) and were confirmed live; OpenClaw's Windows computer-control path was evaluated from its docs and extension READMEs rather than by executing it on Windows; no project was run end-to-end, so all reliability judgements are from code, docs and issue evidence, not from benchmark execution.

---

*End of report.*
