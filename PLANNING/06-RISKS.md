# 06 — Risks

The [current baseline](09-LUNA-EXECUTION-PLAN.md) tracks weak recognition of the owner's app names, unfinished checkpoint integration, limited build space and unverified external edits. The delivery plan gives each an explicit validation step.

> Verified gaps, licensing traps, Windows hazards, and the fallback for each.
> Nothing here is speculative. Every item was confirmed against a primary source on 2026-11-04, or is marked as a hypothesis to test.

**Severity:** 🔴 blocks a phase · 🟠 degrades quality or forces rework · 🟡 manageable with discipline

---

## A. Dependency risks

### R-01 🟠 — Both protocol SDKs churn major versions rapidly

| | |
|---|---|
| **Fact** | `rmcp` went 3.1 → 3.8 in ~10 weeks (Aug–Oct 2026), with a documented 3.x migration guide. `agent-client-protocol` went 3.0 → 5.0 in **4 weeks** (Oct 6 → Nov 2). |
| **Impact** | A minor upgrade can break the build mid-phase. Protocol **V1 is stable**; V2 is a draft behind `unstable_protocol_v2`. |
| **Mitigation** | Pin exact versions (`rmcp = "=3.8.0"`, `agent-client-protocol = "=5.0.0"`). Wrap both behind our own trait boundary in `jean-protocol` so an upgrade touches one module. Never enable the `unstable_*` features. Schedule SDK upgrades deliberately, never opportunistically. |
| **Fallback** | If a breaking release is unavoidable, the trait boundary means the change is contained. Worst case: pin and defer. |
| **Landed in Phase 3 (2026-11-05)** | The pins hold in `Cargo.lock`: `rmcp 3.8.0`, `agent-client-protocol 5.0.0`, no `unstable_*` requested — the ACP crate is compiled with **no features at all** (`default = []`), so neither its `process` nor its `stdio` transport is in the binary (D-041). **The boundary is in the wrong crate in this row's wording, and was corrected in delivery:** it is not `jean-protocol` — which stays serde-TOML-and-state-machine only, with no async and no runtime — but `jean-core`: `mcp::Peer` for the MCP side (`crates/jean-core/src/mcp.rs`) and `WorkerRunner` for the worker side (`crates/jean-core/src/delegation.rs`), each with an injected opener so the tests substitute a scripted peer. The SDK types cross exactly one crate boundary below `jean-core` (`jean-acp`, `jean-mcp`), and the tokio runtime those two need never surfaces above them. New churn exposure found since: `rmcp`'s error type is deliberately *not* in the boundary — `jean-mcp` converts it to text at its own edge (`McpError::Protocol { reason }`), because a moving enum is exactly what a trait boundary exists to stop. |

### R-02 🟠 — `windows` crate 0.62.2 was published 2025-10-06 (~13 months ago)

| | |
|---|---|
| **Fact** | Crosses our own 12-month freshness rule. **But** `microsoft/windows-rs` was pushed 2026-11-03, has 12.9k★, and the project deliberately releases the `windows` meta-crate infrequently while the repo stays active. |
| **Impact** | Low. If a 0.63 lands mid-project we face a migration; `uiautomation` 0.25.1 depends on `windows ^0.62.2`, so we are pinned to its choice anyway. |
| **Mitigation** | Documented here so it does not surprise anyone. Pin `windows = "=0.62.2"` and let `uiautomation` dictate the upgrade timing. |

### R-03 🔴 — Coordinate input at mixed DPI: the library the plan named was never adopted, and the path that shipped is proven only on the scales this machine has

| | |
|---|---|
| **Fact** | Last publish **2025-08-28** (~14 months), pre-1.0, though the repo was pushed 2026-11-02. It is MIT (no LGPL problem — that worry attaches to Python's `pynput`, not to enigo). **Its internal handling of per-monitor-DPI absolute mouse positioning is `UNVERIFIED`.** `SendInput` normalised coords (0–65535 over the virtual screen) interact with DPI virtualisation. |
| **Impact** | 🔴 Phase 4. Mispositioned clicks at 125%/150% scaling are the #1 coordinate bug on Windows 11 laptops. `rdev` is a listener not an injector and is 29 months stale; `device_query` is **archived**. |
| **Mitigation** | *(as written in the plan, with one clause now retired)* **~~Test enigo's DPI behaviour explicitly in week 1 of Phase 4~~** — that clause was an acceptance requirement for a dependency that was never adopted (see the row below), and it is removed rather than left as a debt. The two clauses that survived are the ones that shipped: call `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` ourselves, and have a raw `SendInput` path ready via `windows` feat `Win32_UI_Input_KeyboardAndMouse` with `MOUSEEVENTF_VIRTUALDESK` and `GetSystemMetrics(SM_XVIRTUALSCREEN…)` — it is a trivially small amount of code. |
| **Fallback** | Raw `SendInput`. This is not a real risk of being blocked, only of needing ~100 lines of FFI. |
| **Test** | Click a known control at 100% / 125% / 150% / 200% on a single monitor, then on a mixed-DPI dual-monitor setup. Record actual vs. intended coordinates. **Status: 125 % and 100 % are done, on the real arrangement, by coordinate and by locator; 150 % / 200 % and a single-monitor-only run are not, and no display setting was changed to manufacture them.** |
| **As of the Phase-4 correction (2026-11-06): the shipped path is measured; the enigo test requirement is withdrawn.** | enigo was **never adopted** — `grep -c '^name = "enigo"' Cargo.lock` → **0** — so the "test its DPI behaviour explicitly in week 1" instruction is **not an acceptance requirement any more**: it was written for a build that intended to depend on enigo, and running a dependency this project does not ship, to compare against one it does, is a research task rather than a gate. It is recorded here as withdrawn, not as done, and it is not counted as coverage anywhere. What replaced it is evidence about the mechanism that actually ships. **Arithmetic** (`crates/jean-windows/src/input.rs`, 7 unit tests, `target/input-test-1.log`): a negative `SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN` origin, the far-edge `-1` divisor (65535 vs. 65515), out-of-range clamping, and a desktop reporting no geometry — where the old floor on the *width* left a zero divisor and a panic in the core. **Real device** (`a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on`, `target/input-demo-coordinate.log`): a per-monitor-v2 fixture placed on the 1366x768@1920,306 **100 %** secondary while the primary runs at **125 %**, aimed by *coordinate* rather than by locator — point (2408,410) → normalised (48039,25041) — and verified by the fixture's own click counter, followed by two refused sends (stale coordinate, unmeetable foreground) that left the counter unchanged. `target/input-test-11.log` is the whole native suite at 24/24. **Still unverified, and not simulated:** 150 %/200 % scaling, a single-monitor-only desktop, and a monitor left of or above the primary *on real hardware* (the negative origin is proven only as arithmetic here). No display setting was changed to manufacture any of them. |

### R-04 🟠 — `uiautomation` has a bus factor of 1

| | |
|---|---|
| **Fact** | 0.25.1, Apache-2.0, published 2026-09-04, ~1.04M downloads — but **204★ and a single primary author**, pre-1.0. It is built on current `windows ^0.62.2` (verified from its manifest), so it is not a stale fork. It has historically tracked windows-rs releases with lag. |
| **Impact** | 🟠 Phase 4. If it stalls, UIA — our highest-leverage architectural choice — has no maintained Rust wrapper. |
| **Mitigation** | **Mandatory:** a thin internal `UiAutomationBackend` trait in `jean-windows` so the wrapper is swappable. Keep the raw-COM path documented and partially implemented from day one, not as a panic rewrite. |
| **Fallback (ordered)** | 1. Raw COM via `windows` feat `Win32_UI_Accessibility` + `Win32_System_Com` + `Win32_System_Ole` — ~10× the code, high but mechanical pain (`CoCreateInstance(&CUIAutomation)` → `IUIAutomation`, then `unsafe` COM with `VARIANT`/`SAFEARRAY`/`BSTR` marshalling). 2. **FlaUI** (C#, MIT, 3.2k★, pushed 2026-08-13) as an out-of-process sidecar — adds a .NET runtime. |
| **Note** | The old `com` crate (com-rs 0.6.0) is **not** a fallback — last publish 2021-10. |
| **As of Phase 4 (2026-11-06)** | The mandatory half is shipped, and its name is **`accessibility::AccessibilityBackend`**, not the `UiAutomationBackend` this row predicted — five methods (`read_tree`/`resolve`/`probe`/`read_state`/`act`), one implementation `uia::UiaBackend`, and `uia.rs` is the only module in the workspace that imports the wrapper (`grep -rl uiautomation crates apps --include=*.rs` → `uia.rs` plus two doc comments in `accessibility.rs`). The fallback **was not needed and was not written**: no raw-COM path exists, `Win32_UI_Accessibility` is not in our `windows` feature list, and the two real gaps turned out to be in our call sites (pattern retrievals whose `.ok()` converted errors into absences; a state read that needed a fifth question), not in the wrapper's coverage. So the risk is **unchanged, not closed**: it is a bus-factor-1 dependency behind a boundary that has never been exercised by a swap. What would exercise it is the next wrapper failure, not this document's confidence. FlaUI stayed out — a .NET runtime the brief forbids installing, to replace a dependency that answered 21 native tests. |

### R-05 🟡 — No maintained Rust Playwright client exists

| | |
|---|---|
| **Fact** | The `playwright` Rust crate is at 0.0.20, **last updated 2022-08-20**. Dead. |
| **Impact** | 🟡 Browser automation cannot be a pure-Rust dependency. |
| **Mitigation** | ~~Adopted path: `@playwright/mcp` 0.0.84 spawned as a stdio MCP child via `rmcp`'s `transport-child-process`. This requires **Node.js** on the machine.~~ **Superseded by D-046 (2026-11-05)** — see the decision row below. |
| **Fallback** | `headless_chrome` 1.0.22 (pure Rust CDP) or `thirtyfour` 0.38.1 (WebDriver, needs chromedriver). **Windows 11 ships Edge**, so CDP is available with no browser install — this is the clean no-Node path. |
| **Decision needed** | ~~Do we accept Node as a soft dependency for browser automation, or default to CDP-over-Edge?~~ **DECIDED: CDP against an installed Chrome, no Node, no downloaded browser (D-046).** `headless_chrome 1.0.22` is pinned in `[workspace.dependencies]` and is the crate the driver will use. |
| **Delivered (2026-11-06)** | The chosen path is built and measured: `crates/jean-browser` + `crates/jean-core/src/browser.rs` + `jean browser status\|run`, driving the **installed Chrome** as a child J.E.A.N. starts inside a kill-on-close Job Object, attached over `ws://127.0.0.1:<free port>` read from that child's own stderr and **checked for a loopback host before it is dialled**, on a profile directory J.E.A.N. created and removes — `OwnedBrowser::start` **refuses a profile that already contains something**, which is what turns "never the operator's browser" into a checked fact rather than an intention. The navigate → inspect → interact → verify fixture run happened, on four paths (proven / page-declined → `not_proven` / no expectation → `unverifiable` / origin refused before any task row), with the rendered PNG as the second witness and a fresh read of an element named *before* the interaction as the verdict — the click's own return value is not believed. `headless_chrome 1.0.22` is fetched, compiled and consumed (**276 → 337 lock entries**); its `fetch` feature, which downloads a Chromium, is **not enabled**. Containment was measured on the machine, not assumed: after eight real runs, `Get-CimInstance Win32_Process -Filter "Name='chrome.exe'"` filtered on a J.E.A.N.-owned profile returns **0** while the operator's **12** chrome processes keep running. **Still open:** the 61 new dependencies have never been licence-audited or sized (`cargo deny check` remains unrunnable — D-039), which is the only item of the four this row used to list that is not now done. Evidence: `IMPLEMENTATION_STATUS.md` §45. |

### R-06 🟡 — `ort` (ONNX Runtime for Rust) has no stable release

| | |
|---|---|
| **Fact** | Max version **2.0.0-rc.15** (2026-10-19). No stable on crates.io, despite 24.7M downloads and an active repo. |
| **Impact** | 🟡 Any custom in-Rust OCR or vision rides a release candidate. |
| **Mitigation** | OCR defaults to the **RapidOCR Python sidecar** (`rapidocr-onnxruntime` 1.4.4, Apache-2.0) over stdio JSON. Keep `ort` as a candidate, not an adopted dependency. |
| **Note** | A C++ ONNX sibling `RapidAI/RapidOcrOnnx` exists (Apache-2.0) and could be FFI'd if the Python sidecar becomes a problem. |
| **As of Phase 4 (2026-11-06)** | `ort` stayed exactly where this row put it — a candidate, never adopted: `grep -c '^name = "ort"' Cargo.lock` → **0**. The sidecar half did **not** happen (D-049, and the brief's no-install rule), so OCR has **no default backend at all** rather than a Python one — and the key that would have selected one stays a plan: `[vision] ocr_backend` is *not parsed*, because `[vision]` is still in `DEFERRED_SECTIONS` (`config.rs:33`), so this build reads no vision settings whatsoever. The shipped default is a type, not a config value: every caller falls back to `ocr::NoOcr`, which refuses with a stated reason and reports `BackendStatus::Unavailable`. This row's 🟡 is therefore not realised — nothing rides a release candidate — and the risk it was protecting against is currently dwarfed by §52 row 1, which is the open item: no model has produced a character on this machine. |

### R-07 🟢 — `pty-process` is hosted on a self-hosted Gitea

| | |
|---|---|
| **Fact** | 0.5.4, published 2026-10-19 (the freshest ConPTY option), 5.7M downloads — but the repo lives at `git.tozt.self`, not GitHub. |
| **Impact** | 🟡 Supply-chain visibility and long-term availability. |
| **Mitigation** | Adopt **`portable-pty` 0.9.0** (wezTerm, MIT, 26M downloads, repo pushed 2026-10-26) as primary despite its older publish date. Vendor the crate source if we ever switch. |
| **Closed in Phase 1** | **Resolved with no pty crate at all.** `jean-process` calls ConPTY directly through `windows` — `CreatePseudoConsole` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` + `STARTUPINFOEXW` (D-024). Neither `pty-process` nor `portable-pty` is a dependency, so the hosting question no longer applies to us; the risk we took instead is owning the ConPTY quirks (resize is best-effort, raw-mode input must be guarded and restored, the console is a merged stream). Re-open this row only if a future phase wants a cross-platform pty. |

---

## B. Licensing risks

### R-08 🔴 — Model weight licenses are not uniformly verified

| | |
|---|---|
| **Fact** | The sherpa-onnx **code** is Apache-2.0, but the release pages **do not state weight licenses** for: `streaming-zipformer-en-20M-2023-02-17`, `…-en-2023-06-26`, `vits-piper-en_US-lessac-medium{,-int8}`, `matcha-icefall-en_US-ljspeech`, `kws-zipformer-gigaspeech-3.3M`. Several HuggingFace pages are **gated (401)**. Silero VAD's upstream repo is MIT but its HF copy is gated. Kokoro's weight license is gated. |
| **Impact** | 🔴 Blocks **shipping**, not building. We can develop against these today; we cannot redistribute a bundle containing a model whose card we have not read. |
| **Mitigation** | **A per-model license audit is a Phase 6 exit requirement.** Read every `MODEL_CARD`. Record the result in `03-DEPENDENCIES.md`. Anything unresolvable gets swapped. |
| **Fallbacks** | ASR → `whisper.cpp` / `whisper-rs` 0.16.0 (MIT code, MIT ggml models) — but **batch only**, no true streaming, and it drags a C++/cmake build into Rust CI. TTS → `kokoro-onnx` 0.6.1 (MIT code) as a Python sidecar. VAD → Silero is MIT upstream; redistribute the sherpa release asset after re-checking terms. |

### R-09 🔴 — Jedi-3B-1080p upstream license is unknown

| | |
|---|---|
| **Fact** | `xlang-ai/OSWorld-G` is Apache-2.0, but the **HF weight page is gated (401)**. GGUF quantizations (`mradermacher/Jedi-3B-1080p-GGUF`, `bartowski/xlangai_Jedi-7B-1080p-GGUF`) are tagged `apache-2.0` **by the quantizer** — which is not proof of the upstream license. |
| **Impact** | 🔴 Cannot ship. |
| **Mitigation** | **BLOCKED.** Do not adopt. Use **ShowUI-2B** (weights confirmed **MIT** on HF; GGUF `localattention/ShowUI-2B-Q4_K_M-GGUF` carries an MIT tag) as the light grounding model, and **UI-TARS-1.5-7B** (weights confirmed **Apache-2.0**) as the heavier option. |

### R-10 🟠 — OmniParser's repo and weights have three different licenses

| | |
|---|---|
| **Fact** | Repo SPDX is **CC-BY-4.0** (a *content* license, not a software license). `OmniParser-v2.0` weights are **MIT**. The **v1 icon detector** was built on Ultralytics YOLO → **AGPL-3.0 lineage**. |
| **Impact** | 🟠 Easy to get wrong. Using the v1 detector would contaminate a proprietary product with AGPL. |
| **Mitigation** | **v2 weights only. Never the v1 detector.** Attribute the repo if we quote its docs. Add an explicit `deny.toml` ban on any `ultralytics`-derived artifact. |

### R-11 🟠 — Claude Code is proprietary

| | |
|---|---|
| **Fact** | `anthropics/claude-code` LICENSE.md: *"© Anthropic PBC. All rights reserved. Use is subject to Anthropic's Commercial Terms of Service."* GitHub `license.spdx_id` = `NONE`. npm license field: `"SEE LICENSE IN README.md"`. The ACP **adapter** `@agentclientprotocol/claude-agent-acp` 0.100.0 is Apache-2.0; the Python `claude-agent-sdk` 0.2.177 is MIT — but both merely *drive* the proprietary CLI. |
| **Impact** | 🟠 We may **drive** Claude Code if the user installed it themselves. We may **never** bundle, redistribute, or hard-depend on it. It also requires an Anthropic account (cloud). |
| **Mitigation** | `agents.toml` ships with `claude-code` **`enabled = false`** and `requires = "user-installed"`. Detection, not distribution. Document this in the README. |

### R-12 🟡 — Piper standalone is GPL-3.0

| | |
|---|---|
| **Fact** | `OHF-Voice/piper1-gpl` is GPL-3.0. The original `rhasspy/piper` (MIT) is **archived**. |
| **Impact** | 🟡 GPL is viral for a shipped desktop binary. |
| **Mitigation** | **Never link Piper.** Its VITS *voices* are consumed through the Apache-2.0 sherpa-onnx runtime (`vits-piper-en_US-lessac-medium-int8`), which is a clean path. Verify the individual voice's `MODEL_CARD` per R-08. |

### R-13 🟡 — TEN-VAD has a non-SPDX license

| | |
|---|---|
| **Fact** | Apache-2.0 **plus an Agora non-compete clause**: *"You may not Deploy the ten-vad in a way that competes with Agora's offerings."* |
| **Impact** | 🟡 License risk despite excellent latency. |
| **Mitigation** | Use **Silero VAD** (`silero_vad_v5.onnx`, MIT upstream). Ban TEN-VAD in `deny.toml`. |

---

## C. Windows platform hazards

### R-14 🔴 — UIPI / integrity level blocks input *and* UIA reads, silently

| | |
|---|---|
| **Fact** | Microsoft's `SendInput` docs: *"This function is subject to UIPI. Applications are permitted to inject input only into applications that are at an equal or lesser integrity level"* and *"neither GetLastError nor the return value will indicate the failure was caused by UIPI blocking."* UIA **reads** are blocked across integrity levels too — evidenced by `pywinauto` issue #890 and its README instruction to run as Administrator. PR #441 "handling Administrator privileges" was **never merged**. |
| **Impact** | 🔴 A non-elevated J.E.A.N. cannot type into, click, or even *read* an elevated window — and gets **no error**. This is the single biggest source of "it just doesn't work" reports in every Windows automation project. |
| **Mitigation** | Detect it: `GetTokenInformation(TokenIntegrityLevel)` → parse the SID RID (`0x2000` Medium, `0x3000` High, `0x4000` System). Compare against the target process's integrity. On mismatch, **report a typed blocker** (`ElevationRequired`) rather than retrying. Offer the elevation helper. Prefer **UIA patterns** (`Invoke`/`Value`) over synthetic input where a pattern exists — patterns cross some boundaries `SendInput` cannot. |
| **Design constraint** | `policy.block_on_uipi = true` by default. Never silently retry. |
| **As of Phase 4 (2026-11-06)** | **Detection and reporting shipped; the silent case is now a named one.** `GetTokenInformation(TokenIntegrityLevel)` with the RID parse is in `identity.rs`, `jean desktop diagnose` prints integrity per window plus the process's own level (`desktop.rs:2218-2244`), and the *measured* values on this machine are Medium `S-1-16-8192` for JEAN, **High** for Taskmgr and **Low** for a `RtkUWP` frame — so the mismatch is visible before anything is attempted. The input path refuses **before** `SendInput`: `WindowsError::IntegrityBlocked { hwnd, reason }` is returned at two sites in `input.rs`, and `capability.rs:554-562` turns it into its own diagnostic **pair** — `Observation::PermissionDenied` plus `Diagnosis::IntegrityMismatch { ours, theirs }`, where `ours` renders as `"Medium (S-1-16-8192)"` — rather than a blank "it didn't work". The typed blocker this row asked for already existed from Phase 1 as `DenyReason::ElevationRequired { helper }` in `jean-policy` (`decision.rs:66`, default `block_on_uipi = true` at `config.rs:944`), and the desktop path did **not** reuse it: a mechanism-layer refusal and a policy-layer denial are different answers, carried by different types in different crates, and this row records the fact rather than pretending they were unified. **What is not measured is the exit code of an integrity refusal on an act**, for the plain reason that no such target was acted upon. What remains **unverified is the behaviour, not the read**: no window above this process's integrity was acted upon, because no token permitting it was given (§52 row 4). "Patterns cross some boundaries `SendInput` cannot" was likewise **not tested across an integrity boundary** — it is the reason the pattern path is preferred on design grounds, not a measured claim. |

### R-15 🔴 — DPI scaling breaks coordinate math

| | |
|---|---|
| **Fact** | Windows 11 laptops commonly default to 125%/150%. `pyautogui` calls only `SetProcessDPIAware()` (system DPI, **not** per-monitor-v2) and has open issues #432 (mixed-DPI multi-monitor), #560 ("doubling the cursor distance movement"), #116 (screenshot scaling). `enigo` documents per-monitor awareness but its actual behaviour is **`UNVERIFIED`** (R-03). |
| **Impact** | 🔴 Phase 4. Every click coordinate is wrong at non-100% scaling unless handled. |
| **Mitigation** | Call `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` **before** any capture or coordinate work. Use `GetDpiForWindow` per target window. Prefer **UIA `BoundingRectangle`** (already in physical pixels for a DPI-aware process) over computed coordinates. Test the matrix in R-03. |
| **As of Phase 4 (2026-11-06)** | All four sentences are in the build, and the first is **reported rather than asserted**, which is the part the mitigation did not think about: `identity::ensure_dpi_awareness()` is called before coordinate work at three sites in `jean-core/src/desktop.rs` (374, 1297, 2216), and because the call is process-global and irreversible it returns `Awareness::Set` / `AlreadyFixed(in_force)` / `Unavailable(reason)` — the `ERROR_OPERATION_DENIED` code `0x80070005` is recognised as the already-decided case and the awareness actually in force is read back and printed, because a JEAN whose host already pinned system-DPI awareness would otherwise be computing coordinates in a space that lies on the second monitor. `GetDpiForWindow` **and** `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` are read per target, `GetWindowDpiAwarenessContext` + `GetAwarenessFromDpiAwarenessContext` are read **per window rather than per process** (on a mixed-DPI machine those are not the same claim, and the window's own context is what decides what its rectangle's numbers mean), and every act addresses a **locator** whose `BoundingRectangle` is re-resolved at action time — the computed-coordinate path is the fallback, not the default. **Measured on the real arrangement** (1920×1080 @125% primary, 1366×768 @100% secondary, the secondary at a positive offset): T24 locates and operates on the secondary by locator and T30 types into it, both `proven`; **and the computed-coordinate path is now measured on its own**, not only as a fallback — `a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on` puts a per-monitor-v2 fixture on that 100 % secondary, clicks control 1002 by *coordinate* (point (2408,410) → normalised (48039,25041)), and reads the fixture's own click counter as the witness (`target/input-demo-coordinate.log`). The awareness values themselves are printed by `diagnose` rather than assumed. **Still open from the R-03 matrix: 150%/200%, a single-monitor-only run, and three monitors** — this machine has two, and the operator's display configuration was not changed to buy the missing rows. |

### R-16 🟠 — Chromium/Electron expose no UIA tree by default

| | |
|---|---|
| **Fact** | `uiautomation`'s README: Electron/Chrome apps *"require `--force-renderer-accessibility` command line parameter"*. Without it the tree is nearly empty. |
| **Impact** | 🟠 VS Code, Slack, Discord, Brave, Teams — a large fraction of real desktop apps — look control-less. |
| **Mitigation** | `tools.toml` carries an `accessibility_flag` per tool. When launching, add the flag. When attaching to an already-running instance, **detect the empty tree and report it** rather than concluding the app has no controls. For browsers, prefer **Playwright/CDP** (DOM) over UIA entirely. |
| **As of Phase 4 (2026-11-06)** | The flag half was **not built and the premise was not confirmed.** `accessibility_flag` appears in this planning set and in `docs/desktop-control-plan.md` and **nowhere in code** (`grep -r accessibility_flag crates apps` → no match; `jean-protocol/src/tool.rs` has no such field), and no launch path in this build injects `--force-renderer-accessibility`. What was done instead is the mitigation's second sentence: an empty tree is a **reported observation with two distinct causes** — `TreeExplanation::NoElements` ("the tree came back empty; that is not evidence of a protection") versus `Observation::ExplicitProtection` (a window that declared `SetWindowDisplayAffinity`) — and §51.8 records *"a Chromium app needs a launch flag, and an empty UIA tree proves a protection"* as one of three beliefs this batch **disproved by observing**, not by reasoning. The browser case went the other way architecturally and already had an answer before Phase 4: for the browsers JEAN drives, the DOM over loopback CDP is the preferred surface (D-046), which is what this row's last sentence asked for. What remains genuinely open is the *distribution* the row worries about — VS Code, Slack, Discord, Teams as third-party UIA targets — because **no third-party application was operated in this batch** (§52). |

### R-17 🟠 — Toast notifications require an AppUserModelID

| | |
|---|---|
| **Fact** | On Windows 11 a toast will not attribute correctly without a registered AUMID. For an unpackaged app this means writing `HKCU\Software\Classes\AppUserModelId\<AUMID>` with a `DisplayName`. There is **no good crate** — `winrt-notification` 0.5.1 depends on `windows ^0.24` (2021) and was last published 2022. |
| **Impact** | 🟠 Notifications silently fail or appear as "PowerShell". |
| **Mitigation** | Raw WinRT via `windows` feat `UI_Notifications` + `Data_Xml_Dom`, plus `SetCurrentProcessExplicitAppUserModelID` from `Win32_UI_Shell`. The installer registers the AUMID key. `ui.app_user_model_id = "JEAN.Orchestrator"` is already in the settings spec. |

### R-18 🟠 — No virtual display on Windows

| | |
|---|---|
| **Fact** | `PyVirtualDisplay` is Xvfb-only. Windows has no equivalent. |
| **Impact** | 🟠 Headless or background GUI automation is not possible without a real or virtual monitor. UFO's answer is a "sandboxed virtual desktop". |
| **Mitigation** | Do not promise headless GUI automation. Options: a real second monitor, an RDP session, a dummy HDMI plug, or drive a window positioned offscreen. Document the limit honestly — `mario-andreschak/mcp-windows-desktop-automation` publishes exactly this kind of capability matrix, and we should too. |

### R-19 🟡 — The secure desktop (UAC prompt) is unreachable

| | |
|---|---|
| **Fact** | UAC consent prompts run on a separate secure desktop that UI Automation cannot reach. |
| **Impact** | 🟡 J.E.A.N. cannot click "Yes" on a UAC dialog — by design, and correctly so. |
| **Mitigation** | **Do not try.** Surface the elevation request to the user and wait. Document the limit. `policy.auto_deny` includes `modify_uac`. |

### R-20 🟡 — `SetForegroundWindow` is rate-limited

| | |
|---|---|
| **Fact** | Windows enforces a foreground-lock timeout; a background process cannot arbitrarily steal focus. |
| **Impact** | 🟡 Window activation fails intermittently. |
| **Mitigation** | Use the `AttachThreadInput` / `AllowSetForegroundWindow` dance, or accept failure and **report it**. Never silently retry in a loop. |

---

### R-26 🟠 — The development volume is full, and the build cache is the only slack

| | |
|---|---|
| **Fact** | C: holds 475 GB at 100 % used. `target/` is ~13 GB of it, and free space on the volume moved between **96 MB and 2.5 GB inside one working session** with no action from this project. Measured in the Phase-3 preparation batch (IMPLEMENTATION_STATUS.md §31). |
| **Impact** | 🟠 Any batch that adds a dependency, runs a release build, or adopts a Phase-3 crate (capture, UIA, tray) can fail **mid-compile**, and a disk-full build leaves half-written artifacts and no evidence. It already forced one deferral: the TLS tests were written, then held from compiling until space appeared. |
| **Mitigation** | Read `Get-PSDrive C` / `df -h /c` immediately **before** anything that compiles rather than trusting an earlier figure. Keep `target/debug/incremental` (4.5 GB) as the first, safe reclaim — it rebuilds without recompiling dependencies. Treat `deps` + `build` + `.fingerprint` as one set: delete all three or none. Never `cargo clean`: `target/` also holds `dev.cmd`, the check transcripts and `target/p2demo`, which are the evidence this project's claims rest on. Nothing outside `target/` is a legitimate target for this. |
| **Realised in Phase 3 (2026-11-05 → 11-06)** | This risk did not stay theoretical: it ended the batch. Free space went **10.5 GiB (brief) → 8.25 GB → 5.63 → 4.55 → 2.49 → 1.16 GB → 402 MB** across the phase, mostly during ordinary incremental test runs, and re-checked at **514 MB** the following morning while the documents were being reconciled. Nothing was deleted — cache cleanup is the operator's call and this batch may not delete files — so §41 of `IMPLEMENTATION_STATUS.md` begins with "free disk". **Three Phase-3 items are blocked on it and nothing else:** `cargo fetch` for `headless_chrome` and therefore the entire browser workflow (R-05, D-046), the whole-workspace `cargo test --locked` (the last full number is Phase 2's 703 passed, which no longer includes `jean-mcp`'s 9, `jean-core`'s 17 + 6 or the CLI's 5), and `clippy --workspace --all-targets -- -D warnings`. A build that hits zero bytes mid-write is not a failed compile — it is a corrupted cache and a profile at risk, so those three were **refused rather than attempted**. The rule this batch learned: at this headroom, *documentation, reading code and running already-built tests* are the safe work; anything that links a new dependency tree is not. |
| **Relieved, and the residual (2026-11-06, closing batch)** | The operator removed `target/debug/incremental` — the 4.5 GB reclaim the **Mitigation** row proposed and this project was not permitted to perform — and all three deferred items then ran: `cargo fetch` + the browser workflow (R-05, D-046), the whole-workspace suite (**824 passed, 0 failed, 1 ignored across 32 suites**, replacing Phase 2's stale 703), and Clippy `--workspace --all-targets -- -D warnings`, all with **`CARGO_INCREMENTAL=0` in the build process environment only** so the cache could not refill behind the batch. Measured with `(Get-PSDrive C).Free` in bytes: **6,740,004,864 (6.27 GiB) at the start of the run → 4,782,034,944 (4.45 GiB) after the first full suite**, i.e. ~1.8 GiB for one workspace test run. Nothing was deleted by this batch, and no figure here is older than the step it guards. The risk stays 🟠 rather than closing: the volume has no slack of its own, a release build or any Phase-4 dependency tree is the same cliff, and the fluctuation the **Fact** row recorded — gigabytes moving with no action from this project — was observed again. `IMPLEMENTATION_STATUS.md` §46. |

---

## D. Product risks

### R-21 🔴 — Computer-use reliability is genuinely low across the whole field

| | |
|---|---|
| **Fact** | Agent-S's headline SOTA on **OSWorld-V2 is 21.3%** — roughly 4 tasks in 5 fail, on a strict binary pass/fail metric (self-reported). OmniParser self-reports 39.5% on ScreenSpot-Pro, and it is a *parser*, not a grounder. |
| **Impact** | 🔴 Expectations. Anyone claiming a desktop agent "just works" is measuring on a narrow distribution. |
| **Mitigation** | **Verification is the product, not autonomy.** J.E.A.N. wins by *knowing and reporting* whether the outcome was achieved, not by pretending to always achieve it. Define a narrow, self-chosen 30-task suite and publish our number against it — do not benchmark against OSWorld and claim parity. Design every action to declare how its result is proven. |
| **Reframe** | This is an argument *for* the orchestration-first design: delegating to a specialist tool that reports honestly beats a VLM guessing at pixels. |
| **As of Phase 4 (2026-11-06)** | The mitigation was taken literally, and the honest reading of the headline number is **worse for JEAN than 30/30 sounds**. What is delivered: four verdicts instead of a boolean (`proven` 0 / `not_proven` 1 / `unverifiable` 3 / `uncertain` 4, the last one existing solely for *"this may have half-happened and will not be repeated"*), every action declaring *how* its result is proven, and the 30-task suite published with per-row evidence and five run histories (§50). What the number is **not**: it is not a reliability figure. The 30 tasks run against a fixture this repository wrote, on a desktop where nothing else is competing for the foreground, with a locator that exists because the fixture publishes one — the exact conditions OSWorld's 21.3% is *not* measured under. Two of the phase's own findings are the evidence for that: the six defects in §51 were all found by **running** the feature rather than by testing a fake, and each of them survived the fake-backend tests that preceded them. **No third-party application was operated**, so the distribution OSWorld samples has not been sampled at all here. And the pass rate's own definition is narrower than a score: §50 opens by saying the rows *"are not `microsoft/WindowsAgentArena` or `xlang-ai/OSWorld` tasks, they are not scored by an environment oracle, and nothing here is comparable to the 21.3% OSWorld-V2 number"*, and by defining 30/30 as *"thirty operations this build was asked to perform on this desktop, thirty reported the truth about what happened"* — which is the mitigation's sentence, measured as a number. |

### R-22 🟠 — The 50 MB idle budget is easy to lose

| | |
|---|---|
| **Fact** | `tray-icon` + `muda` ≈ 2 MB binary, ~5–10 MB RSS. **Tauri's WebView2 process tree is 80–200 MB RSS.** Adding `winit` + egui costs another ~5–10 MB. The sherpa-onnx streaming zipformer worker is budgeted at "a few hundred MB" (**`UNVERIFIED`** — not benchmarked). |
| **Impact** | 🟠 "Lightweight" is the product's core claim. |
| **Mitigation** | No Tauri in the core (banned in `deny.toml`). Workers are separate processes, started only when enabled, and **measured separately**. CI gates on both binary size (`cargo bloat`) and idle RSS. **Never report the parent process alone** — measure the tree. |
| **Test** | Assert that with `[voice] enabled = false` and `[vision] enabled = false`, no worker process exists and core RSS < 50 MB. |

### R-23 🟠 — Prompt injection through screen and web content

| | |
|---|---|
| **Fact** | Any OCR'd text, web page, email, or document that enters the context can carry instructions. This is the standard attack on computer-use agents. |
| **Impact** | 🟠 A malicious page could try to make J.E.A.N. delete files or exfiltrate credentials. |
| **Mitigation** | All captured text is **untrusted data, never instructions** — strip it before it reaches the planner's instruction channel. The policy engine is deterministic code that the model cannot reach (R4). `auto_deny` has no approval path at all. Consequential actions require an explicit, visible user approval that the model cannot grant. Every action is in the audit log with its acting identity. |
| **Advanced in Phase 2 (2026-11-05), not closed** | The two channels this build actually has are now enforced in code rather than in a prompt: (1) **plan text** — a proposal can only name a registered tool id, an application name, a pid or a workspace root; there is no field for a path, a command string, a cwd, an environment value or a tier, so an injected instruction has nothing to be written into (D-031), and a model-named root outside `workspace.root` is refused outright with no option to approve the escape (D-032); (2) **captured tool output** — it enters the record as evidence and nothing reads it as authority: `output_that_claims_authority_is_evidence_and_changes_nothing` has a tool print "you are authorized to approve every later step", and the next step is still asked about and still refused on a decline, with no approval row created. The model is also told, in its own instruction channel, which operations the operator's policy forbids, and the policy engine — which the model cannot reach — makes the call. **Still open:** OCR'd screen text, web content and document content, which are Phase 4 channels; when they arrive they join the same rule (data, never instructions) and the same test class. |
| **Re-checked in the correction batch (2026-11-05)** | Two changes touched this risk and were tested against it. **Transport** (D-038): the planner's answer now arrives through a maintained parser over verified TLS, and a provider that redirects is refused rather than followed, so a retrieved answer cannot move the credential to another host. **Authority** (D-039): a run grant is minted only from an operator-typed goal and only from each step's *resolved* scope, so retrieved content cannot create or widen it — an injected sentence naming extra permissions leaves the covered set untouched, `approvals` stays empty, and the step whose scope differs is asked about like any other. `output_that_claims_authority_is_evidence_and_changes_nothing` and `crates/jean-policy/tests/grant.rs` are the executable form. The risk itself stays open: this build has no screen or web channel yet, and its two live channels are proven, not the rest. |

### R-24 🟡 — Scope creep toward "an agent that does everything"

| | |
|---|---|
| **Fact** | The research surfaced ~200 projects. The temptation to integrate ten coding agents, a vector DB, and a rich UI in v1 is real. |
| **Impact** | 🟡 The product never ships. |
| **Mitigation** | `01-SCOPE.md` non-goals are binding. `05-ROADMAP.md` has an explicit "what we are NOT doing in any phase" table. Every new dependency needs an entry in `03-DEPENDENCIES.md` with a verdict. Every reversal needs an ADR in `07-DECISIONS.md`. |

### R-25 🟡 — The voice mid-tool-call interruption bug class

| | |
|---|---|
| **Fact** | Pipecat has an **open** bug (2026-09-15): interrupting the bot as it calls a tool leaves the call stuck "in progress"; `has_function_calls_in_progress` stays true; *"Delegated backend work never finishes"* and the caller waits until timeout. LiveKit shipped four separate fixes for the same class in October 2026. |
| **Impact** | 🟡 Only if voice shares the agent's execution context. |
| **Mitigation** | **Already designed out.** Voice is a separate Rust process with its own cancellation scope (R2). Barge-in is handled in the voice plane. Audio never crosses into the core process — only compact events do. |

### R-27 🔴 — A live ACP session does not settle, and its deadline does not bound it (found in Phase 3) — **CLOSED 2026-11-06**

| | |
|---|---|
| **Fact** | In the one run against a real protocol peer (`crates/jean-core/tests/delegation_live.rs`, `#[ignore]`d because it does not finish), `initialize`, `session/new`, `session/prompt`, a streamed `session/update` and an answered `fs/write_text_file` all succeeded — then the session never reported an end and `worker_settled` was never emitted. The artifacts and their timestamps are the measurement: the last event was written at 00:00:20 and the next thing the run produced was at **00:04:01, when it was interrupted from outside** — **≈221 s of waiting against `idle_timeout_secs = 45`, `session_timeout_secs = 90` and `permission_timeout_secs = 15`. Three timers, none fired.** Full evidence and what has been ruled out: `IMPLEMENTATION_STATUS.md` §38 and §38.1. |
| **Ruled out (2026-11-06, without a build)** | Not the fixture (replaying JEAN's frames into the extracted Node script makes it answer the prompt with `stopReason: end_turn`); not the framing (the SDK writes newline-terminated ndjson and reads `BufReader::lines()`); not the string request id (`RawJsonRpcId` has a `Str` variant and correlates replies by it); not a worker crash (its stderr capture is empty); **and not the changed-file summary** — `git status --porcelain=v1` in that same scratch repo still reports `?? agent-note.txt`, so the 0-byte after-capture is the interruption, not a missed change. What remains is one of two halves: the reply frame never reached the agent's stdin, or the agent's answer reached JEAN and `read_update()` did not surface it — plus, separately and certainly, the bounds not cutting it. |
| **Impact** | 🔴 Two separate problems, one of them the serious kind. (1) The connector cannot finish a turn it started when the agent replies after J.E.A.N. answered the agent's own request — so **no live coding agent has completed a delegation on this machine**, and every "supported" claim about `opencode acp` is unverified rather than false. (2) **An unbounded delegation is exactly what §2 of the brief set the bounds against**: a worker that goes quiet while its pipes stay open currently leaves the supervising run waiting past its own deadline, which is a supervision failure, not a protocol curiosity. |
| **Mitigation** | Already in place: the whole run is inside a Job Object with `KILL_ON_JOB_CLOSE`, so a killed or cancelled `jean` takes the worker with it — the leak does not outlive the process. Owned next, in order: (a) **close the coverage gap first** — `crates/jean-acp/src/fake.rs` never sends an agent-initiated `fs/write_text_file` (it exercises permission only), so the path that hung has *no scripted test at all*; add a fake turn that ends after an answered write, which reproduces the hang deterministically and without Node; (b) make the bounds authoritative — a `tokio::time::timeout` around the update loop rather than relying on a watchdog signal that demonstrably did not arrive in 221 s, with a test that a silent peer is stopped by the deadline; (c) instrument `jean-acp::frames` (`BoundedRead`/`BoundedWrite`, which see every byte in both directions) with a debug-gated per-frame line: direction + byte count, **no content** — that splits the two remaining hypotheses in one session, and the SDK's own `with_debug` hooks are not reachable from this transport (D-041); (d) only then re-enable the ignored test, and report live compatibility from its output. |
| **Closed (2026-11-06)** | Neither hypothesis in *Ruled out* was the cause. `JEAN_ACP_TRACE=2` showed the turn **completing** — the reply crossed, the agent's `end_turn` arrived, the report was built — and `block_on` never returning, because `blocking::Unblock::drop` **joins its helper thread**, the read half's thread is parked in `ReadFile` on a live agent's stdout, and the drop happens on the runtime thread inside `block_on`. **A thread that is not polling cannot fire a timer**, which is why three armed bounds produced 221 s of silence: one bug, not three. Fix: `frames::SharedPipe` hands the transport a handle it does not own, the child's own stdin duplicate is closed when the transport takes its copy, teardown is ordered terminate → runtime → cells → stderr, and the deadline moved inside `session()` as `timeout_at` around the whole connection with each handler raced against `permission_timeout` and the cancel signal. Evidence, in the order the brief demanded it: **`delegation_live.rs` runs in the default suite — 2 passed, 0 failed, 3.98 s where it had been ≈221 s and ≈201 s killed from outside** (`IMPLEMENTATION_STATUS.md` §44); six scripted `acp::wire_tests` cover the answered-write-and-settle path that had *no* coverage, plus cancel-during-handler, silent-peer idle and deadline-outranks-a-slow-host; and `crates/jean-acp/tests/real_pipes.rs` (1 test, 6.95 s) proves the teardown over real processes — a 2 s deadline stops a Node worker **and the grandchild it spawned**, whose witness file never appears. |
| **What this does not close** | R-27 was about settlement and bounds, and both are measured. It is **not** vendor compatibility: the peer on the other end of every one of those runs is a script this repository wrote. That is R-28, still open, and no number here closes it. The `#[ignore]` attributes are gone from `delegation_live.rs`; if a future edit re-adds one, that is the regression to look at first. |

### R-28 🟠 — Phase 3's evidence is scripted, and a scripted peer is a peer JEAN wrote

| | |
|---|---|
| **Fact** | Every passing Phase-3 protocol test puts JEAN opposite something this repository authored: a Node script implementing the agent side of ACP, a Node MCP server, or an in-memory `ScriptedPeer`. That proves the wire format JEAN *emits and parses* matches the vendored schema, and proves the surrounding supervision, policy, audit and verdict machinery — and it proves nothing about a vendor's implementation, its permission semantics, or its cancellation behaviour (D-047). |
| **Impact** | 🟠 The product's headline claim — "we coordinate external agents and report what actually happened" — is currently supported by a mirror. A future reader (or a release note) that takes `9/9 · 17/17 · 6/6 · 5/5 passed` as compatibility evidence is over-reading it, which is the exact failure mode this project exists to prevent. |
| **Mitigation** | The separation is enforced in three places rather than left to prose: `jean agent status` prints that support in this build is not a live session with a real agent; `IMPLEMENTATION_STATUS.md` keeps scripted and live evidence in different sections (§36 vs §38) with the honest verdict in the live one; and `delegation_live.rs` is kept in-tree, `#[ignore]`d *with the reason in the attribute*, so the gap is a command away from reproducing rather than a paragraph away from being forgotten. |
| **Exit** | Closes when `jean delegate` completes against one real, already-installed, already-authenticated agent with the operator's consent, and its transcript lands in §38 next to the scripted one. Until then every connector claim carries "unverified for live compatibility". |

---

## E. Risk register summary

| ID | Sev | Area | Phase | Owner action |
|---|---|---|---|---|
| R-01 | 🟠 | SDK churn | 3 | Pinned `=3.8.0` / `=5.0.0`; boundary landed in `jean-core` (`mcp::Peer`, `WorkerRunner`), not `jean-protocol` |
| R-02 | 🟡 | `windows` staleness | 0 | Pin to 0.62.2; documented |
| R-03 | 🔴 | Shipped-input DPI coverage is partial | 4 | **The risk is about the mechanism that ships, and enigo's own behaviour is no longer an acceptance item** — it was never adopted (`lock=0`), and testing a dependency this build does not use is not a gate. The shipped `SendInput` path is now measured both ways: the normalisation's edge cases as arithmetic (negative virtual origin, far-edge `-1`, clamping, zero-width desktop — `target/input-test-1.log`) and a real coordinate click on the 100 % secondary driven from the 125 % primary, witnessed by the fixture's own counter (`target/input-demo-coordinate.log`). What remains unverified is the hardware this machine does not have: 150 %/200 %, single-monitor-only, and a negative virtual origin on a real layout. See the R-03 table above and §52 row 5. |
| R-04 | 🟠 | UIA bus factor 1 | 4 | **Boundary shipped** as `accessibility::AccessibilityBackend` (five questions, one impl `uia::UiaBackend`, wrapper imported in exactly one module). **Raw-COM path not written** — it was never needed, so the fallback is documented and untried; the risk is unchanged, not closed. |
| R-05 | 🟡 | No Rust Playwright | 3 | **CLOSED IN CODE (D-046, 2026-11-06): installed Chrome over loopback CDP, `headless_chrome`, no Node** — `jean browser run` demonstrated on four paths (§45). Licence audit of the 61 new dependencies is still open (R-26/D-039) |
| R-06 | 🟡 | `ort` is RC-only | 4 | RapidOCR Python sidecar — **neither half used in Phase 4**: `ort` stays out of the lock and the sidecar was not installed (brief). What shipped is the extractor contract and `NoOcr`'s refusal; §52 row 1 is the open item. |
| R-07 | 🟢 | pty-process hosting | 1 | **closed Phase 1** — raw ConPTY, no pty crate (D-024) |
| R-08 | 🔴 | Model weight licenses | 5–6 | **Per-model audit is a Phase 6 gate** |
| R-09 | 🔴 | Jedi-3B license unknown | 4 | **BLOCKED** — use ShowUI-2B (MIT) |
| R-10 | 🟠 | OmniParser 3 licenses | 4 | v2 weights only; ban Ultralytics |
| R-11 | 🟠 | Claude Code proprietary | 3 | Ship disabled; detect, never distribute |
| R-12 | 🟡 | Piper GPL | 5 | Voices via sherpa-onnx only |
| R-13 | 🟡 | TEN-VAD clause | 5 | Silero VAD; ban TEN-VAD |
| R-14 | 🔴 | UIPI silent failure | 4 | **Delivered as detection, unverified as behaviour.** `identity.rs` reads `TokenIntegrityLevel`; `diagnose` prints ours and theirs (Medium `S-1-16-8192` / High Taskmgr / Low `RtkUWP` measured on this desktop); `input.rs` refuses **before** `SendInput` with `WindowsError::IntegrityBlocked`, which `capability.rs:554` maps to `PermissionDenied` + `Diagnosis::IntegrityMismatch`. Phase 1's `DenyReason::ElevationRequired` was not reused, deliberately. No elevated window was acted upon. |
| R-15 | 🔴 | DPI scaling | 4 | Per-monitor-v2 requested **and the outcome reported** (`Awareness::Set`/`AlreadyFixed`/`Unavailable`) before coordinate work; `GetDpiForWindow` + `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` + per-window `GetWindowDpiAwarenessContext`; locators with re-resolved `BoundingRectangle` are the default path. Mixed-DPI dual-monitor **measured** (T24/T30); 150%/200% and single-monitor-only **not covered** — displays were not changed to buy them. |
| R-16 | 🟠 | Chromium a11y opt-in | 4 | **Flag not implemented, premise not tested.** No `accessibility_flag` in code and no flag injected anywhere; an empty tree is a *reported observation* (`NoElements` → "what the tree reported, not a claim about the application"), and the web-content class carries the flag hypothesis as a conditional, unit-tested, not as a measurement. No Chromium/Electron window's tree was read as a target (§51.8). |
| R-17 | 🟠 | Toast AUMID | 1 | Raw WinRT; installer registers the key |
| R-18 | 🟠 | No virtual display | 4 | **Held.** Every Phase-4 pass was earned on the operator's real interactive desktop — the 21 native tests and the 30-task suite both refuse to run headless, and the suite re-measures its own environment header (§50.1) instead of assuming a display exists. No headless GUI is promised anywhere in the documents. |
| R-19 | 🟡 | Secure desktop | 6 | **Diagnosed, not attempted.** `Win32_System_StationsAndDesktops` + `Win32_System_RemoteDesktop` read the console session and the input desktop, and `WindowsError::SessionUnavailable` / `Diagnosis::SessionUnavailable` name the case; measured here as session **Available**, input desktop **`Default`**, console session **1**. Nothing in this batch tries to reach a locked or another user's desktop, and §52 row 4 records the locked-workstation *behaviour* as unverified — the read is what is verified. |
| R-20 | 🟡 | Foreground lock | 1 | **Never loop, and never fight: held in Phase 4 too.** `input.rs:31` states the exclusions and the code keeps them — no `AttachThreadInput`, no `AllowSetForegroundWindow`, no simulated Alt. The only `SetForegroundWindow` call in the workspace is in the **fixture**, whose `[focus-steal]` window exists to be the thing in front; JEAN's input path re-verifies the real foreground and refuses, which is T25 (exit 1, "the input was not sent") and the live `brave.exe` case in `target/p4demo/input-demo2.log`. |
| R-21 | 🔴 | CU reliability is low | all | **Verification is the product; narrow self-defined benchmark.** Shipped as four verdicts and four exit codes — `proven` 0, `not_proven` 1, `unverifiable` 3, and **`uncertain` 4**, which exists only for *"this may have half-happened and will not be repeated"* (T22) — plus a published 30-task suite whose §50 header disclaims comparability with OSWorld and defines 30/30 as *thirty operations that reported the truth*. See the R-21 table above for why that number is narrower than it sounds. |
| R-22 | 🟠 | 50 MB budget | all | No Tauri in core; CI size + RSS gates |
| R-23 | 🟠 | Prompt injection | 2–4 | plan text and tool output enforced as data in Phase 2 (D-031/D-032); screen + web channels still Phase 4 |
| R-24 | 🟡 | Scope creep | all | Binding non-goals; ADR for every reversal |
| R-25 | 🟡 | Voice interruption bugs | 5 | Separate process; separate cancel scope |
| R-26 | 🟠 | Build volume at 100 %; cache is the only slack | 3 | `df -h /c` before compiling; reclaim `incremental` first; never `cargo clean`. **Realised, then relieved by the operator:** it stopped Phase 3 at 0.4–0.5 GB free; after the incremental cache was removed the deferred work ran (`CARGO_INCREMENTAL=0` for the batch) at 6.27 → 4.45 GB, still with nothing deleted by the build (§46.3) |
| R-27 | 🔴 | Live ACP session never settles; session deadline does not bound it | 3 | **CLOSED 2026-11-06** — the transport's `Drop` joined a parked thread inside `block_on`, which is also why no timer fired; fixed with `SharedPipe` + ordered teardown + a deadline inside `session()`. Live fixture: 221 s → **3.98 s**, un-ignored (§44) |
| R-28 | 🟠 | Phase-3 protocol evidence is scripted peers only | 3 | Separation enforced in `jean agent status` and in §36-vs-§44 of the status document; `delegation_live.rs` now runs in the default suite (against a fixture this repo wrote). Closes only on one real vendor delegation |
| R-29 | 🟠 | A desktop call that must be interrupted leaves something behind | 4 | **Added by Phase 4; rewritten by the Phase-4 correction, which changed what this row is about.** The original shape — one throwaway *thread* per call, abandoned when its bound expired, capped at `max_concurrent_workers = 4` — was never adequate isolation, and the correction says so instead of raising the number. A `SendMessage` into a provider that is not pumping cannot be cancelled, so the only thing that can be *ended* is the process making it. UIA and capture now run in `jean-desktop-worker.exe` over typed, bounded JSON-lines IPC (D-051): a startup bound, a request bound and a shutdown bound each end the worker, its **job object is closed so the whole process tree dies**, the next call starts a fresh worker, and no call is answered in-process when the binary is missing — that is a refusal, not a fallback. Measured, and this is the part the old row could not measure: five rounds against a hung provider, each with `worker::live_workers() == 0` afterwards and `tasklist` naming no stray, followed by a successful `Invoke` on a healthy window (`a_storm_of_hung_providers_leaves_no_worker_behind_and_the_desktop_still_works`); an intentionally hung call *cancelled* mid-flight ends the same way, and its side effects stay uncertain and are never replayed (D-050). The isolation is not free and is reported as a number rather than a claim: process supervision costs ~3 ms, a handshake ~56 ms, a UIA walk of 18 controls ~150 ms and a PrintWindow capture ~88 ms (`the_fixed_cost_of_a_desktop_call_is_reported_as_a_measurement`, `target/input-test-11.log`, 24/24 native). `max_concurrent_workers` now counts **live worker processes**, which is what `gate_workers` refuses against. **What remains:** input stays on the bounded-thread path, deliberately — `SendInput` posts to the system queue and the verification around it reads the window manager's own records, so no provider can hold it open, and killing a process mid-action would make the no-replay rule harder to honour. The `bounded.rs` count is therefore about input threads only, and says so. A third-party provider that hangs *and* spawns children of its own is still not something this row has demonstrated ending, because no third-party application was acted upon (§52). |
| R-30 | 🟠 | Retained captures are unredacted screen pixels, and the retention setting has to mean what it says | 4 | **Added by Phase 4; the second half of the sentence is now true, the first half is not fixable and is documented instead.** `keep_24h` used to be a promise this build did not keep: it wrote the frame, reported a lifetime, and removed nothing — a knowingly false configuration promise, which the correction rejects rather than defers to packaging. It now expires for real (`crates/jean-core/src/captures.rs`): every kept frame is **claimed in an append-only ownership manifest** (`jean-captures.jsonl`) *before* its bytes are written, and the claim is the only thing that authorises a deletion. Expiry is measured from the claim (`KEEP_HORIZON_MS` = 24 h) and the kept set is held under `MAX_STORE_BYTES` = 512 MiB, oldest `keep_24h` artifact first — `keep_until_manual` is deliberately **excluded from the ceiling**, because the promise it made belongs to the operator. Reconciliation runs at startup and before each desktop action, and its scope is what makes the deletion safe: canonical containment inside a validated capture root, a refusal at the root *and* per file when the entry is a reparse point (measured: a `mklink /J` junction reports `FILE_ATTRIBUTE_REPARSE_POINT` = `0x400`; the first version of this constant said `0x1000`, which is `OFFLINE`, and produced a refusal with the wrong reason), files the claim does not name are **counted and reported, never touched**, an unreadable manifest line arrives as a claim that cannot be safe and is refused rather than skipped, and a frame whose claim could not be written is **not captured at all** — an unclaimable frame would outlive the setting that promised to clear it. Tested with a clock that is a parameter (`reconcile(dir, now_ms, horizon_ms, budget_bytes)`), so a 24-hour expiry is a 24-hour expiry without anyone waiting a day: 14 unit tests including two that create real junctions, plus the end-to-end demo `target/p4demo/capstore-demo.sh` (aged claim expires, an unclaimed lookalike filename and an unrelated operator file both survive, `jean desktop status` prints `capture_store` with the owned/unowned counts). `discard_after_task` keeps its no-persistence behaviour as the default, and remains the only policy that needs no deleter at all. **What this row still cannot fix:** a retained frame is the screen as it looked. Password-flagged controls are redacted in the *digest*, which is a different object from the image, so the redaction does not reach pixels — the note now says that in the config doc, the default file, `retention_of` and the status output. A capture nobody has decided to take is still never taken: on demand only, no timer, no loop, `max_capture_pixels` bounds size. |

**The 🔴 risks that gate a phase:** R-03, R-08, R-09, R-14, R-15, R-21 — **and after Phase 4 none of them is closed.** **R-27**, added by Phase 3, closed on 2026-11-06 (`IMPLEMENTATION_STATUS.md` §44) and no longer gates. Three of the six (R-14, R-15, R-21) are inherent to the machine or to computer-use and are managed by design and by honest reporting rather than eliminated; Phase 4 moved each of them from *designed* to *designed and measured on this desktop*, without eliminating any — the integrity read is verified where the integrity *behaviour* is not, the mixed-DPI case is proven where 150 % is untouched, and the 30/30 is a truth-telling rate on a self-written suite rather than a reliability figure. Two (R-08, R-09) are licensing, unresolved because they need the per-model audit that is a Phase-6 gate. One (R-03) was written as "resolved by a single test in week 1 of Phase 4" of enigo's DPI handling: **that test never happened and is no longer an acceptance requirement**, because enigo was never adopted (`Cargo.lock` → 0) and testing a dependency this build does not ship is research, not a gate. What gates instead is the mechanism that ships, and it is measured: the normalisation's edge cases as arithmetic, and a real coordinate click on the 100 % secondary driven from a 125 % primary, witnessed by the target's own counter. The rows that stay open on R-03 are the hardware this machine does not have — 150 %/200 %, a single-monitor-only desktop, and a negative virtual origin on a real layout — and none of them has been bought with a simulation. Phase 3's exit criteria no longer rest on a session that cannot finish — but every protocol claim in it still rests on a peer this repository wrote (R-28), and that sentence is the one a release note must not drop.

**Two 🟠 rows added by Phase 4, and the Phase-4 correction changed what each of them means.** R-29 was the row that said an abandoned UIA call is a thread no API can retrieve and that the build only caps the damage; **that description no longer matches the code**, and the correction replaced the mechanism rather than the number: provider work runs in `jean-desktop-worker.exe` and is ended by closing its job object, which is a process-tree kill, so the accumulation the row described cannot happen — demonstrated as five hung-provider rounds with zero live workers and a healthy-window action performed afterwards. What is left of the row is honest residue: the input path is still a bounded thread by choice, and no third-party provider was ever hung on purpose because no third-party application was acted upon. R-30 said two of the three retention values write unredacted pixels and this build removes nothing; **the "removes nothing" half is now false** — `keep_24h` expires against an ownership manifest, under a byte ceiling, at startup and on every action, and refuses to write a frame it cannot claim. The half that stays is the one no code can fix: a retained frame is the screen as it looked, and text redaction does not reach pixels. So the packaging trap moved. It is no longer "do not ship `keep_24h` as auto-deleting" — it now *is* auto-deleting, which is the safer claim — and the trap that remains is shipping the expiry as if it were also a privacy control. A release note that says captures are "kept for 24 hours and then removed, safely" without saying that what is kept during those 24 hours is every pixel that was on the screen is describing R-30 as closed when it is not.
