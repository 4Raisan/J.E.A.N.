# 03 — Dependencies

Use the [adoption gate in the Luna plan](09-LUNA-EXECUTION-PLAN.md) for new code, runtimes and model weights. Parakeet and Jev are trial candidates. Chif needs an identifiable upstream source before evaluation.

> **Everything we need for the implementation.** Complete inventory with verified versions, licenses, and adopt/avoid verdicts.
> Verified 2026-11-04 against the crates.io API, npm registry, PyPI, GitHub release APIs and official docs.

**Legend:** `ADOPT` = committed dependency · `CANDIDATE` = evaluated, not yet committed · `AVOID` = rejected, do not add · `RAW` = no crate; use `windows`-rs FFI directly.

---

## 1. Rust crates — core

| Crate | Version | Published | License | MSRV | Verdict | Role |
|---|---|---|---|---|---|---|
| `tokio` | **1.54.1** | 2026-10-21 | MIT | 1.85 | **ADOPT — arrived in Phase 3 (D-041)** | Async runtime, `tokio-util 0.7.20` (`compat`) and `blocking 1.7.0` with it. *Not* in Phase 0: D-001 notes Job Objects, ConPTY, UIA and `SendInput` are all reachable "without a runtime", Phase 0 runs one sequential task with two reader threads, and `tokio::net::windows::named_pipe::ServerOptions` **cannot set a `SECURITY_ATTRIBUTES`** — so it cannot apply the creator-owner DACL that per-user cancellation requires. Recorded as deviation DV-02. **Where it actually landed:** `Cargo.lock` pins 1.54.1 and it is depended on by `jean-acp` and `jean-mcp` and by nothing else — every crate above them keeps the synchronous surface Phases 0-2 were built on, so no Phase-0/1/2 code path changed. Features requested: `rt-multi-thread`, `rt`, `macros`, `process`, `io-util`, `io-std`, `sync`, `time`, `net`; each run of a protocol session builds its own **current-thread** runtime inside the worker thread rather than sharing one across the process. The `SECURITY_ATTRIBUTES` limitation above is unchanged and is why the pipe that carries ACP/MCP bytes is still created by `jean-process`, with the SDK handed the two halves. |
| `regex` | **1.13.1** | 2026-07-15 | MIT OR Apache-2.0 | 1.65 | **ADOPT — added in implementation** | Pattern engine for `process.redact_patterns`. Not in the original table; 151M downloads. Recorded as deviation DV-01. |
| `serde` | **1.0.229** | 2026-07-18 | MIT OR Apache-2.0 | — | **ADOPT** | Typed config and IPC messages |
| `serde_json` | **1.0.154** | 2026-10-11 | MIT OR Apache-2.0 | — | **ADOPT** | JSON for MCP/ACP/NDJSON streams |
| `toml` | **1.1.8** | 2026-10-09 | MIT OR Apache-2.0 | 1.85 | **ADOPT** | Config files. Note: 1.x now. |
| `tracing` | **0.1.44** | 2025-12-18 | MIT | — | **ADOPT** | Structured diagnostics. 0.x but de-facto standard (1B+ downloads). |
| `tracing-subscriber` | **0.3.23** | 2026-03-13 | MIT | — | **ADOPT** | Layered output, filtering |
| `tracing-appender` | **0.2.5** | 2026-04-17 | MIT | — | **ADOPT** | Non-blocking rolling file logs |
| `rusqlite` | **0.41.1** | 2026-10-28 | MIT | 1.89 | **ADOPT** | Persistence. **`bundled` feature** → vendored SQLite, no system dependency. 143M downloads. |
| `anyhow` | **1.0.104** | 2026-07-18 | MIT OR Apache-2.0 | — | **ADOPT** | Application-level errors |
| `thiserror` | **2.0.21** | 2026-09-23 | MIT OR Apache-2.0 | — | **ADOPT** | Library-level typed errors |
| `clap` | **4.6.7** | 2026-09-14 | MIT OR Apache-2.0 | 1.85 | **ADOPT** | CLI parsing (`jean-cli`) |
| `reqwest` | **0.13.5** | 2026-09-08 | MIT OR Apache-2.0 | — | **AVOID** (was ADOPT) | HTTP to Ollama / `llama-server`. Superseded by D-038: `jean-model` talks with `ureq` + Schannel, and the §7 row below is the verdict that matches the build. The claim that it came in for free "pulled by `rmcp`'s streamable-http feature" was true only if that feature were enabled — it is **not** (see §2), and `reqwest`, `hyper` and `rustls` are absent from `Cargo.lock`, so nothing in Phase 3 drags them in either. |
| `notify` | **8.2.1** | 2026-10-27 | **CC0-1.0** | 1.85 | **ADOPT** | Filesystem watching (`ReadDirectoryChangesW` on Windows) |
| `windows` | **0.62.2** | **2025-10-06** | MIT OR Apache-2.0 | — | **ADOPT** | All raw Win32/WinRT. Repo pushed 2026-11-03; windows-rs releases the meta-crate infrequently by policy. |

### Phase 1: two internal crates, no new external crate

`crates/jean-tools` (discovery, process inspection, bounded search) and `crates/jean-policy` (tiers, approvals, the audit writer) were added as workspace members. Their complete external dependency lists:

| Crate | External dependencies | Notes |
|---|---|---|
| `jean-tools` | `jean-protocol`, `serde_json`, `thiserror`, `tracing` + (Windows-only) `sysinfo`, `windows` | `.lnk` targets are read via `IShellLinkW`/`IPersistFile`; both features were already declared at the workspace root, so nothing here widens the graph. |
| `jean-policy` | `jean-protocol`, `jean-store`, `serde`, `serde_json`, `thiserror`, `tracing` | The approval digest is `std`'s `DefaultHasher` over a length-prefixed canonical form (D-027) — deliberately not a hash crate. |

`jean-core` gained `jean-policy` + `jean-tools` as internal edges only. The only third-party crate that enters the tree with Phase 1 is `sysinfo`, which this document already listed as ADOPT (§3) and which arrives with process inspection. No dependency was added that had not been evaluated and recorded first.


### Phase 2: one internal crate, one external crate, and one test-only crate

`crates/jean-model` (provider-neutral client, two adapters, credential handling) was added as a
workspace member. Its complete external dependency list, current as of **D-038** and **D-040**:

| Crate | Dependencies | Note |
|---|---|---|
| `jean-model` | `jean-protocol`, `serde`, `serde_json`, `thiserror`, `tracing`, **`ureq`** | `ureq` 3.5.1 with `default-features = false, features = ["native-tls"]` — a maintained blocking client giving verified HTTPS and chunked transfer (D-038, superseding D-029). Still no async runtime, no `ring`/`aws-lc`, no vendored OpenSSL: on Windows `native-tls` is Schannel, which uses the OS trust store and the `windows` lineage already in the tree. |
| `jean-model` *(dev-only)* | **`native-tls`** 0.2.18 | D-040. The TLS verification tests need a TLS **server**, and `TlsAcceptor` is not re-exported by `ureq`. This is the same crate and version `ureq` already resolves, on the Schannel path that is already compiled, so nothing new is downloaded or built on Windows and `Cargo.lock` gained one edge in `jean-model`'s list. It is in `[dev-dependencies]` only: the shipped binary's dependency graph does not contain it as a direct dependency, and no runtime code imports it. |

`jean-model` depends on `jean-protocol` and on nothing else internal — it cannot reach the store, the
policy engine or the process layer, which is what D-011 ("the model may request, never grant") looks
like as a dependency graph. `jean-core` gained `jean-model` as an internal edge; `jean-policy` and
`jean-tools` were not touched, and neither can now import the model crate even by accident.

`jean-protocol` gained the plan schema (`plan.rs`) and eight new event kinds; `jean-store` gained
schema v3 (`plans`, `plan_steps`). No new crate entered the *shipped* graph after `ureq`; the only
addition since is the dev-only `native-tls` above.

### `windows` crate feature flags — the set as built

Features are hierarchical namespaces mirroring Win32 metadata (`Win32_<Area>_<SubArea>`). Nothing compiles unless its exact path is enabled, and unused namespaces cost zero compile time and zero binary size. **The compiler names the missing feature verbatim in the error** — grow the list reactively.

This is the live list in the workspace root manifest, one comment per feature explaining which crate needs it:

```toml
windows = { version = "0.62.2", features = [
  # Phase 0 — supervision, cancellation, logging
  "Win32_Foundation",               # HANDLE, BOOL, error types — almost always needed
  "Win32_Security",                 # handles, DACLs for pipe security, TokenIntegrityLevel
  "Win32_Security_Authorization",    # SDDL -> DACL for the per-user cancellation pipe
  "Win32_Storage_FileSystem",         # reparse points and file attributes during search
  "Win32_System_Console",             # Ctrl-C/Ctrl-Close handler, ConPTY
  "Win32_System_IO",                  # ReadFile / GetOverlappedResult / pipe flushing
  "Win32_System_JobObjects",          # child containment, KILL_ON_JOB_CLOSE
  "Win32_System_Pipes",               # output capture + the cancellation pipe
  "Win32_System_Threading",           # CreateProcess, tokens, ResumeThread

  # Phase 1 — discovery, windows, toasts
  "Win32_System_Com",                 # IPersistFile, for reading a .lnk target
  "Win32_System_Registry",            # InstallLocations, App Paths
  "Win32_UI_Shell",                   # .lnk resolution, AUMID registration
  "Win32_UI_WindowsAndMessaging",     # EnumWindows + the style/title queries (read-only)
  "UI_Notifications",                 # WinRT toast runtime
  "Data_Xml",                         # the namespace itself
  "Data_Xml_Dom",                     # XmlDocument::LoadXml — the parent does NOT bind it

  # Phase 4 — desktop awareness, capture, input, session diagnosis
  "Win32_UI_HiDpi",                   # per-monitor-v2 awareness + GetDpiForWindow, before any coordinate (R-15)
  "Win32_UI_Input_KeyboardAndMouse",  # SendInput, and the key-name table
  "Win32_Graphics_Gdi",               # the DCs, DIB section and BitBlt a still needs
  "Win32_Storage_Xps",                # PrintWindow — it really does live here, not in Graphics or UI
  "Win32_System_RemoteDesktop",       # which session the console is attached to
  "Win32_System_StationsAndDesktops", # which desktop the injected input will actually reach
] }
```

Twenty-two features, counted out of the manifest with `awk '/^windows = \{ version/,/^\] \}/' Cargo.toml | grep -oE '"[A-Za-z0-9_]+"' | sort`. That count is the reason the block above was rewritten against the file instead of copied from it: the six Phase-4 names were missing from it entirely, and it had never carried the two session/desktop names either, which are Phase-4 reads (`Win32_System_RemoteDesktop` for the session the console is attached to, `Win32_System_StationsAndDesktops` for the desktop the keyboard input actually reaches — Cargo.toml:153-158).

**And one the plan expected is not there.** UIA's row in the table below named `Win32_UI_Accessibility`, `Win32_System_Com` and `Win32_System_Ole`; only `Win32_System_Com` (Phase 1, `.lnk` resolution) is in our list, because the other two arrive with the wrapper: `uiautomation`'s own manifest requests `Win32_System_Variant`, `Win32_System_Com`, `Win32_System_Ole` and `Win32_UI_Accessibility` against **exactly the version this workspace pins**, so cargo's feature unification builds them once. Adding them to our list would have been a second name for a binding we never reference — and it is worth being exact about *why*, because "we didn't need it" and "we didn't need the *wrapper* to need it" are different claims; the first is measured, the second is arithmetic on the resolved versions.

Two Phase-1 additions, recorded as **D-025**. `Win32_UI_WindowsAndMessaging` is used for enumeration and queries only: the launch verification path deliberately does not import `SetForegroundWindow`, `ShowWindow` or `AttachThreadInput` behaviour, because taking the user's focus is not something an orchestrator gets to do silently. `Data_Xml_Dom` exists because `XmlDocument` and `LoadXml` live in the sub-feature, not in the `Data_Xml` parent.

What happened to that list, phase by phase:

| Subsystem | Additional features | As built |
|---|---|---|
| UIA | `Win32_UI_Accessibility`, `Win32_System_Com`, `Win32_System_Ole` | **Switched on by the wrapper, not by us** — see the paragraph above. Only `Win32_System_Com` is in our list, and it has been since Phase 1 for `.lnk` resolution. |
| Input injection | `Win32_UI_Input_KeyboardAndMouse` | **Switched on in Phase 4**, and it is the whole input path (§53.1's defect-6 fix lives inside it). |
| DPI correctness | `Win32_Graphics_Gdi`, `Win32_UI_HiDpi` | **Both switched on in Phase 4.** The measured form of "correctness" here is not the flag but the behaviour: per-monitor-v2 awareness established before coordinate work, `GetDpiForWindow` per target, and `SM_XVIRTUALSCREEN` normalisation with `MOUSEEVENTF_VIRTUALDESK` — which is what let a 125 % primary drive a 100 % secondary to a `proven` verdict (suite T24, T30). |
| Elevation / integrity | `Win32_Security` (`GetTokenInformation` + `TokenIntegrityLevel`, `ChangeWindowMessageFilterEx`) | `Win32_Security` has been in the list since Phase 0; `GetTokenInformation`/`TokenIntegrityLevel` are **read and reported** by `jean desktop diagnose` (Medium for this process, High for Taskmgr, Low for a `RtkUWP` frame — §52 row 4). `ChangeWindowMessageFilterEx` is **not used and not needed**: JEAN never sends a message into another application's queue, it drives UIA patterns and `SendInput`. |

---

## 2. Rust crates — protocols

| Crate | Version | Published | License | MSRV | Verdict | Role |
|---|---|---|---|---|---|---|
| **`rmcp`** | **3.8.0** | 2026-10-26 | Apache-2.0 | 1.88 | **ADOPT** | Official MCP Rust SDK (`modelcontextprotocol/rust-sdk`, 4,085★, pushed 2026-11-03). 44.3M downloads. Implements MCP spec **2026-07-28**. |
| `rmcp-macros` | (with rmcp) | — | Apache-2.0 | — | **ADOPT** | `#[tool]` derive |
| **`agent-client-protocol`** | **5.0.0** | 2026-11-02 | Apache-2.0 | 1.88 | **ADOPT** | Official ACP Rust SDK (`agentclientprotocol/rust-sdk`, 236★, pushed 2026-11-02). 5.87M downloads. Originated in Zed; now under the ACP org. **We do not depend on Zed.** |
| `agent-client-protocol-rmcp` | (with ACP) | — | Apache-2.0 | — | **CANDIDATE** | ACP↔MCP bridge — directly relevant to "MCP tools + ACP agents" |
| `agent-client-protocol-http` | (with ACP) | — | Apache-2.0 | — | CANDIDATE | HTTP/SSE + WebSocket transports, if ever needed |
| **`headless_chrome`** | **1.0.22** | `Cargo.lock` line 927, fetched 2026-11-06 | MIT (crate manifest) | not measured | **ADOPT — consumed by `jean-browser` (D-046, built and demonstrated)** | The CDP client for the Phase-3 browser workflow: talks to an *installed* Chrome/Edge/Brave over a **loopback** debugging pipe rather than shipping a browser. Declared in `[workspace.dependencies]` with `default-features` left on so `offline` keeps the CDP schema bundled instead of fetching it at build time; the `fetch` feature — which downloads a Chromium — is **not enabled**, and no `wss://` endpoint is reachable even if one were offered, because `OwnedBrowser::start` refuses any URL that is not `ws://` on a loopback host before a socket exists. **Adopting it took the lock from 276 to 337 entries (61 new).** What that closure actually contributes is narrower than the number: one runtime dependency, `tungstenite 0.29.0`, resolved **without a TLS backend**, and a build-time path through `auto_generate_cdp → ureq → webpki-roots` used for schema regeneration; `chrome-devtools-protocol` is **absent from the lock**, which is what `offline` buying means in practice. It adds **no async runtime**: the CDP client is synchronous, so D-041's confinement of `tokio` to `jean-acp`/`jean-mcp` is intact and `jean-browser` has no runtime dependency. **Open:** those 61 entries have never been licence-audited or sized, because `cargo deny check` is still unrunnable (§33, R-26) and this batch may not install it. See IMPLEMENTATION_STATUS.md §45 and D-046. |

### `rmcp` feature flags

Available: `client`, `server`, `transport-io` (**stdio**), `transport-child-process`, `transport-streamable-http-client`, `transport-streamable-http-server`, `transport-async-rw`, `auth` (OAuth), `macros`, `schemars`, `tower`, `elicitation`.

**Compiled in this build** (`Cargo.toml`, `default-features = false`): `client`, `server`, `transport-io`, `transport-async-rw`, `macros`, `base64`, `elicitation`.

The two absences are the ones that matter:

- **`transport-child-process` — deliberately excluded.** It would have `rmcp` spawn the server with its own process handling, which is a second execution path outside the Job Object, the kill-on-close guarantee and the audit trail. JEAN spawns and supervises the child through `jean-process` and hands `rmcp` the two pipe halves over `transport-async-rw` (D-041, D-045). The same reasoning excludes the ACP SDK's `process` feature.
- **`transport-streamable-http-*` — not requested.** A network listener is a boundary this build does not open (D-045: both MCP roles are local stdio). Consequence worth recording: the earlier note that `reqwest` arrives "for free" through this feature is therefore not true here — `reqwest`, `hyper`, `rustls` and `tower` are absent from `Cargo.lock`.

`auth` and `schemars` are off for the same reason: no remote server is contacted and no schema is published to a client. `elicitation` is on because a server may ask JEAN for a value mid-call, and refusing that channel would make the server hang rather than answer.

### `agent-client-protocol` feature flags

`stdio`, `process`, `schemars`, `wasm_js`, plus unstable: `unstable_protocol_v2`, `unstable_mcp_over_acp`, `unstable_subagents`, `unstable_session_fork`. `default = []` (verified from the 5.0.0 manifest).

**Compiled in this build: none of them** — the workspace dependency is declared with no `features` and default features are empty, so neither `stdio` nor `process` is requested. `process` would let the SDK spawn the agent; JEAN does (above). `stdio` would put a session on *this* process's standard streams, which belong to the operator's terminal, not to a child. `blocking`, the crate `stdio` would have pulled, is requested directly because `jean-acp` needs `blocking::Unblock` to wrap the pipes it owns.

Stable v1 API: `Client.v1()`, `Agent.v1()`, `Proxy.v1()`. **J.E.A.N. speaks ACP as Client** (possibly Proxy).

> ⚠ **Both SDKs churn majors fast** — `rmcp` went 3.1→3.8 in ~10 weeks; ACP went 3.0→5.0 in 4 weeks. **Pin exact versions** (`=5.0.0`) and wrap both behind our own trait boundary so an upgrade touches one module.

---

## 3. Rust crates — Windows integration

| Area | Crate | Version | Published | License | Repo (★ / pushed) | Verdict |
|---|---|---|---|---|---|---|
| **UIA** | `uiautomation` | **0.25.1** | 2026-09-04 | Apache-2.0 | leexgone/uiautomation-rs (204★ / 2026-09-04), ~1.04M dl | **ADOPT — consumed** (D-048). Pinned `=0.25.1`, `default-features = false`, `features = ["control", "pattern", "input"]`. `input` is in that list for a reason that is **not** ours and was measured, not inferred: 0.25.1's `core.rs:38` imports `crate::inputs::MouseButton` unconditionally while `lib.rs:12` gates the `inputs` module behind `feature = "input"`, so the crate **does not compile** with `control` and `pattern` alone — the first build of this tree failed with `unresolved import crate::inputs` at `core.rs:38`, "could not find `inputs` in the crate root", "note: found an item that was configured out" at `lib.rs:13` — the verbatim compiler output is in `target/p4-fixture-build.log` (`[exit] 101`). Nothing in J.E.A.N. calls the crate's input API; the cost of that bug is one extra `windows` feature this workspace already requests. Licence read from the local registry manifests for both halves: `uiautomation` **and** `uiautomation_derive 0.7.9` are `Apache-2.0`. Every call goes through `accessibility::AccessibilityBackend`, so R-04's bus factor stays a crate-local swap. See IMPLEMENTATION_STATUS.md §49 and D-048. |
| UIA fallback | `windows` feat `Win32_UI_Accessibility` | 0.62.2 | — | MIT OR Apache-2.0 | microsoft/windows-rs | **RAW — still an escape hatch, and still unused.** The plan called this the fallback for "some pattern nuances and UIA event handling may require dropping to raw COM"; **none was needed and none was written.** What the wrapper could not do turned out to be a question about *our* call sites, not about COM: the fifth boundary question (`read_state`) is built from patterns the wrapper already exposes (`CurrentNameValue`/`Toggle`/`SelectionItem`/`ExpandCollapse` plus `HasKeyboardFocus`), and the four `if let Some(p) = ….ok()` sites that were silently turning *errors* into absences became `if let Ok(p) = …` (§53.4). `Win32_UI_Accessibility` is therefore not in the workspace feature list at all. |
| **Capture (streaming/per-window)** | `windows-capture` | **2.0.1** | 2026-08-08 | MIT | NiiightmareXD (523★ / 2026-10-05) | **NOT ADOPTED here** (D-049). `grep -c '^name = "windows-capture"' Cargo.lock` → **0**; the 2.0.1 source is already in the local registry cache, so this is a choice, not a download problem. The crate wraps `Windows.Graphics.Capture`, which is a *frame-pipeline* API: it wants a subscription and a Direct3D surface for something J.E.A.N. asks for once per action — an on-demand still of one window — and adopting it would have put a DXGI device and a background frame loop in the path the brief says must not capture continuously. **What was *not* done: measuring WGC on this machine.** Its behaviour under capture-exclusion, minimisation and the D3D surface is unmeasured, so this row cannot claim the choice was forced by a failure; it was forced by the shape of the requirement. |
| **Capture (on-demand stills)** | `xcap` | **0.9.8** | 2026-08-01 | Apache-2.0 | nashaofu (1.0k★ / 2026-11-01) | **NOT ADOPTED here** (D-049), same measurement: `lock=0`, sources present in the registry cache. The shipped path is `PrintWindow` against the target's own DC as the **primary** backend, with a virtual-screen `BitBlt` clipped to the window rectangle as the **alternate** — run only when the primary produced nothing or produced nothing informative, and never against a window that declared an exclusion (`capture.rs:691`) — plus a local BMP writer: 24bpp, bottom-up rows padded to 4 bytes. It adds **zero** lock entries. What that path was measured doing, against the fixture: `PW_RENDERFULLCONTENT` answered for every ordinary fixture window; a window that has declared `SetWindowDisplayAffinity` exclusion **refuses before either API runs** (suite row T10, `exit 1`, "excluded by the window itself") while the *same* window keeps answering the tree (T05, 18 controls), names its exclusion as itself (T08, `explicit_protection`) and is operated through its own controls to a `proven` verdict (T21); a dark-but-not-excluded window is measured (`luma=`, T11) rather than assumed. That separation is the brief's "a black capture must not disable otherwise usable accessibility", and it needed no dependency. |
| **Input injection** | `enigo` | **0.6.1** | 2025-08-28 | **MIT** | enigo-rs (1.8k★ / 2026-11-02) | **NOT ADOPTED here** (D-049; was "ADOPT — pre-1.0, 14-month publish gap, repo active"). `lock=0`; the input path went straight to `SendInput`, because the two things Phase 4 must guarantee are *before* the send, not inside the send: re-verify the real foreground window and re-resolve the target rectangle, then normalise absolute coordinates against `SM_XVIRTUALSCREEN` with `MOUSEEVENTF_VIRTUALDESK`. **enigo's per-monitor-DPI behaviour is no longer an acceptance item** — the roadmap's "where enigo's handling proves wrong" was written for a build that intended to depend on it, and enigo ships nothing here, so running it as a comparison is research rather than a gate (R-03, §52 row 5). The DPI claim that *is* gated belongs to the shipped path and is measured: per-monitor-v2 awareness, normalisation against `SM_XVIRTUALSCREEN`, and a coordinate click on the 100 % secondary driven from a 125 % primary witnessed by the target's own counter. |
| Input fallback | `windows` feat `Win32_UI_Input_KeyboardAndMouse` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW — ADOPTED (D-049)**, for exactly the reason this row already gave. Two measured details the plan did not have: `SendInput` counts **events, not characters**, and `KEYEVENTF_UNICODE` requires `wVk = 0` with the UTF-16 unit in **`wScan`** — writing the character into `wVk` produced `9.*5354 5` on screen, which is now `unicode_event()` plus the unit test `typed_text_carries_its_characters_in_the_scan_field_and_names_no_key` (§51 defect 6). Wheel notches carry the move, absolute and `MOUSEEVENTF_VIRTUALDESK` flags together, in one event pair. |
| **Job Objects** | `win32job` | **2.0.3** | 2025-05-15 | MIT OR Apache-2.0 | ohadravid/win32job-rs, 1.6M dl | **ADOPT** — safe wrapper |
| Job Objects (raw) | `windows` feat `Win32_System_JobObjects` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW** — `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` |
| **ConPTY** | `windows` feats `Win32_System_Console` + `Win32_System_Pipes` + `Win32_System_JobObjects` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW — adopted in Phase 1 (D-024)**: `CreatePseudoConsole` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` + `STARTUPINFOEXW`, so the pty path keeps the *same* Job Object, `CancelToken`, timeout arithmetic and output caps as the pipe path |
| ConPTY (was planned) | `portable-pty` | 0.9.0 | 2025-02-11 | MIT | wez/wezTerm (29.4k★ / 2026-10-26), 26M dl | **AVOID as of D-024** — still a fine library, but it is a *terminal emulator's* pty: its `DynChild` does not hand back the process handle `AssignProcessToJobObject` needs, so our supervision loop would have had to be forked. Not a dependency. |
| ConPTY alt | `pty-process` | 0.5.4 | 2026-10-19 | MIT | self-hosted Gitea (`git.tozt.self`), 5.7M dl | CANDIDATE — freshest, but self-hosted repo = supply-chain consideration |
| **Named pipes** | `tokio::net::windows::named_pipe` | 1.54.1 | 2026-10-21 | MIT | tokio-rs | **ADOPT — Phase 2+** — first-party once tokio arrives. **Not usable for Phase 0 cancellation**: `ServerOptions` exposes no `SECURITY_ATTRIBUTES`, so it cannot restrict the pipe to the current user |
| Named pipes (Phase 0) | `windows` feats `Win32_System_Pipes` + `Win32_Security_Authorization` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW** — `CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE` and SDDL `D:P(A;;GA;;;CO)`. The DACL is the access control, because the pipe name is guessable |
| Named pipes alt | `interprocess` | 2.4.7 | 2026-10-23 | 0BSD OR Apache-2.0 | kotauskas (606★), 17M dl | CANDIDATE — only if a non-tokio consumer appears |
| **Clipboard** | `arboard` | **3.7.0** | 2026-10-15 | MIT OR Apache-2.0 | 1Password (980★ / 2026-11-02) | **ADOPT** |
| **System info** | `sysinfo` | **0.40.1** | 2026-10-30 | MIT | GuillaumeGomez (2.75k★), 238M dl | **ADOPT — used in Phase 1** — pinned `=0.40.1` with `default-features = false, features = ["system"]`. Process inspection only; it is the crate that reports *no* `thread_count` on Windows, which the intent surfaces as a documented platform absence rather than a refusal. |
| **Tray** | `tray-icon` | **0.26.1** | 2026-10-07 | MIT OR Apache-2.0 | tauri-apps | **ADOPT** — MSRV 1.90 |
| **Menus** | `muda` | **0.21.2** | 2026-10-08 | Apache-2.0 OR MIT | tauri-apps | **ADOPT** — MSRV 1.90 |
| **Toast** | `windows` feat `UI_Notifications` + `Data_Xml` + `Data_Xml_Dom` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW — built in Phase 1.** No crate was added. **AUMID is mandatory on Win11**: JEAN registers `JEAN.Orchestrator` under `HKCU\Software\Classes\AppUserModelId` with a display name only — no Start Menu shortcut is written. |
| **Window mgmt** | `windows` feat `Win32_UI_WindowsAndMessaging` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW — read-only in Phase 1.** `EnumWindows` + `GetWindowThreadProcessId` + `IsWindowVisible`/`IsIconic` + `GetWindowLongPtrW(GWL_EXSTYLE)` + `GetWindowTextW`, bounded at 8 192 windows examined / 256 reported. Nothing that *changes* a window is imported: no `SetForegroundWindow`, no `ShowWindow`, no `AttachThreadInput` (D-023). |
| **Elevation / UAC** | `windows` `ShellExecuteW` verb `runas` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW** — handle `ERROR_CANCELLED` (1223) for a declined prompt |
| **Integrity level / UIPI** | `windows` feat `Win32_Security` | 0.62.2 | — | MIT OR Apache-2.0 | — | **RAW** — `GetTokenInformation(TokenIntegrityLevel)`; parse SID RID: `0x2000`=Medium, `0x3000`=High, `0x4000`=System |
| **OCR (Rust)** | `ocrs` | 0.13.1 | 2026-09-13 | MIT OR Apache-2.0 | robertknight (1.9k★ / 2026-11-01) | CANDIDATE — clean English only, weak on UI screenshots/CJK. **Still a candidate as shipped** (D-049): `grep -c '^name = "ocrs"' Cargo.lock` → **0**, so it was never resolved, let alone run — although its **0.13.1 source is already in `~/.cargo/registry/src`**, fetched while the roadmap's candidates were assessed, which is why the next batch's cost is a lock entry and a run rather than a download. The shipped `ocr::TextExtractor` trait has exactly one implementation, `NoOcr` (`crates/jean-windows/src/ocr.rs:284`), which is the `[vision] ocr_backend = "none"` case: it returns `InvalidSetting("vision.ocr_backend is \"none\"; no text extraction is configured")`, reports `BackendStatus::Unavailable`, and is documented in-source as "not a mock — it is never asked to recognise anything and cannot pretend to". The sidecar half that *does* ship, `SidecarProtocol`, translates a request into one line and a reply line into a result, and nothing more: the spawn, supervision and timeout belong to `jean_core::desktop`. Its tests test the **translation**, and are not inference; no model has produced a character on this machine (IMPLEMENTATION_STATUS.md §52 row 1). |
| **ONNX runtime** | `ort` | **2.0.0-rc.15** | 2026-10-19 | MIT OR Apache-2.0 | pykeio, 24.7M dl | CANDIDATE — **pre-release, no stable on crates.io** |

### UIA from Rust — the honest picture

`uiautomation` 0.25.1 is a **real, alive wrapper** built directly on `windows ^0.62.2` + `windows-core ^0.62.2` (verified from its dependency manifest — current windows-rs, not a stale fork). It wraps `IUIAutomation`, `IUIAutomationElement`, patterns, `UITreeWalker`, conditions and caching in safe Rust with derive macros.

Feature flags, read from the 0.25.1 manifest rather than from this table's memory of it: `process`, `dialog`, `input`, `clipboard`, `pattern`, `control`, `event`, `log`, `screenshot`, and **`default = ["input", "control"]`** — `pattern` is *not* a default, it arrives transitively because `control = ["pattern", "uiautomation_derive/action_derive", "uiautomation_derive/control_derive", "windows/Win32_UI_WindowsAndMessaging"]`. This line previously asserted defaults of `input, control, pattern`; the manifest says two, not three. `screenshot = ["png", …]` is the reason it is left off (an encoder nobody asked for), and `input = ["windows/Win32_UI_Input_KeyboardAndMouse"]` is the reason switching it on costs this workspace nothing — that feature is already requested for `SendInput`.

```rust
let automation = UIAutomation::new()?;
let walker = automation.get_control_view_walker()?;
let root   = automation.get_root_element()?;
// element.get_name()?, element.get_classname()?, walker.get_first_child(&el)?
```

**Caveats:** pre-1.0, single primary author, 204★ — **bus factor 1**. It has historically tracked windows-rs releases with lag. Some pattern nuances and UIA event handling may require dropping to raw COM.

**Mitigation (mandatory):** put a thin internal trait layer beneath it so the wrapper can be swapped out. Raw windows-rs COM is the documented fallback (~10× the code). FlaUI (C#, MIT, 3.2k★) as a sidecar is the nuclear option — it adds a .NET runtime.

**As shipped in Phase 4 (D-048).** The mandatory half was built and the optional half was not. The trait
layer is `accessibility::AccessibilityBackend` — five questions, `read_tree` / `resolve` / `probe` /
`read_state` / `act` — with exactly one implementation, `uia::UiaBackend`, and `crates/jean-windows/src/uia.rs`
(789 lines) is the only module in the workspace that imports it — `grep -rl uiautomation crates apps --include=*.rs` returns exactly `uia.rs` and `accessibility.rs`, and both of the latter's two hits are doc comments; nothing above it can tell the
wrapper is underneath. `Win32_UI_Accessibility` is **not** in the `windows` feature list, because
**the raw-COM fallback was never written**: the two gaps that actually appeared were in our call sites
(pattern retrievals whose `.ok()` turned errors into absences, and a state read that needed a fifth
question rather than a new API), not in the wrapper's coverage. The bus factor therefore remains a
crate-local swap that has not been exercised, and R-04 stays open on that basis rather than on a
demonstrated failure. FlaUI was not considered further: it would have added the .NET runtime the brief
forbids installing, to replace a dependency that answered 21 native desktop tests.

> Crates that **do not exist** on crates.io (404-verified — do not plan around them): `windows-uiautomation`, `uiautomation-rs`, `job-objects`, `win-job-object`, `win-toast`, `rapidocr`, `conpty-rs`.

---

## 4. Rust crates — voice worker

| Crate | Version | Published | License | Verdict | Role |
|---|---|---|---|---|---|
| **`sherpa-onnx`** | **2.0.1** | 2026-10-27 | **Apache-2.0** | **ADOPTED — wired as `optional = true` behind `sherpa = ["dep:sherpa-onnx"]` (D-055, supersedes the "not in `Cargo.toml`" rule in D-054)** | Official Rust binding. Sources at `sherpa-onnx/rust/`; examples at `rust-api-examples/`. Link statically (`default-features = false` + static feature). Measured with `cargo tree -p jean-voice`: the default feature set pulls **no** sherpa package, and `--features sherpa` pulls `sherpa-onnx` then `sherpa-onnx-sys`. Linking and inference are no longer assumptions: the feature-on build links and three published clips were recognised through it (§60.5). |
| `sherpa-onnx-sys` | 2.0.1 | 2026-10-27 | Apache-2.0 | **ADOPTED — same** | FFI layer, pulled by the above. **Its build script downloads `sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2` (122,863,596 bytes, `sha256:23e7b986b197eda0bbc47ff3d7d5c87afd6ef2b9077214ba26b0f1ec52ef0e66`, release `v2.0.1` published 2026-10-26T15:24:29Z — all measured via the GitHub API) unless `SHERPA_ONNX_LIB_DIR` or `SHERPA_ONNX_ARCHIVE_DIR` is set.** Both refusal branches were **executed** (§59.4): with a declared directory lacking the archive the build stops naming the exact expected file, with a non-directory `LIB_DIR` it stops naming that path, and `target/sherpa-onnx-prebuilt/` stayed 0 bytes. The earlier claim that listing an optional dependency breaks `--locked --offline` for the default build was measured and found wrong — it needs **one online resolve** to write the lock (341 → 380 packages, +39, none of them built by default), after which `check --workspace --all-targets --offline --locked` passes. **The archive has now been supplied, its bytes verified against that digest, extracted to `%LOCALAPPDATA%\JEAN\native`, and linked** (§60.1, §60.4): `--features sherpa` builds with `[exit] 0` and one `LNK4098`, exactly the CRT outcome §59.5 had predicted from a local probe. **And that warning has since been removed rather than accepted** (§61.4): it is the collision between Rust's shared CRT and the archive's `/MT` static one, and `RUSTFLAGS=-C target-feature=+crt-static` builds the same feature on `[exit] 0` with **no linker message, no `VCRUNTIME140*` and no `api-ms-win-crt-*` import** — 12 DLL imports instead of 25, +375,808 bytes, and the same recognition and cancellation results. Both executables are kept; the flag costs a 110-crate rebuild per switch, so it is a deliberate choice, not a default. |

> This is the single biggest correction to the original plan: **the voice worker can be Rust.** No Python or C++ sidecar needed for speech.

> **2026-11-07 correction.** "ADOPT" was written as though adding the line were the whole decision. It is
> not: adopting this crate means accepting a build script that fetches 122 MB of binaries as a side effect
> of `cargo build`. `workers/voice/src/sherpa.rs` is written against the published 2.0.1 signatures and
> gated behind a `sherpa` feature, and the dependency line ships as a comment in `workers/voice/Cargo.toml`
> with the two environment variables that make the fetch impossible. Provisioning is an operator act,
> recorded in `docs/voice-assets.md`. The crate's own compile/link evidence is therefore **tier 2 and still
> empty** — do not read this row as built.

---

## 5. External binaries and services

| Component | Version | License | Windows distribution | Requires | Verdict |
|---|---|---|---|---|---|
| **Ollama** | **v0.40.14** (2026-11-03) | MIT | `ollama-windows-amd64.zip`, `-arm64.zip`, `-rocm.zip`, `-mlx.zip` | nothing | **ADOPT** (primary local model server) |
| **llama.cpp** | build **b12078** (2026-11-03) | MIT | `llama-b12078-bin-win-{cpu-x64, cpu-arm64, vulkan-x64, vulkan-arm64, cuda-12.4-x64, cuda-13.4-x64, rocm-10.0-x64, openvino-2026.4.1-x64, sycl-x64, opencl-adreno-arm64}.zip` | nothing | **CANDIDATE** (alternative; `llama-server` is OpenAI-compatible) |
| ~~**`@playwright/mcp`**~~ | **0.0.84** | Apache-2.0 | `npx @playwright/mcp@latest` | Node.js | **SUPERSEDED BY D-046 — was ADOPT.** Rejected on this machine, not on the project's account: it is a *runtime requirement* JEAN does not get to install silently, it downloads its own browser builds, and it would put the browser's authority behind a third party's process. The Phase-3 browser workflow drives an **already-installed** Chrome over a loopback CDP pipe instead (D-046). |
| ~~Playwright~~ | 1.64.1 | Apache-2.0 | npm | Node.js | **SUPERSEDED BY D-046** — was only ever pulled in by `@playwright/mcp`. No npm browser automation is installed, and installing it was outside this phase's "do not install software" constraint. |
| **Chrome / Edge / Brave** | installed: Chrome **156.0.8078.81** (Edge and Brave present, versions not pinned here) | proprietary (freely distributed) | ships with the machine | nothing new | **ADOPT as the browser child (D-046)** — the user's own browser, launched by JEAN with a **dedicated profile directory and an explicit loopback `--remote-debugging-port`**, only for a task the operator approved. Never: attaching to the user's existing running browser, their personal profile, or a debugging endpoint someone else opened (D-046, IMPLEMENTATION_STATUS.md §39). |
| **RapidOCR** | `rapidocr-onnxruntime` **1.4.4** | Apache-2.0 | pip | Python | **NOT ADOPTED here** (D-049; was ADOPT as the vision sidecar). Measured in the interpreter, not the lock, and re-measured for this line: `C:\Python314\python.exe -c "import rapidocr"` → `ModuleNotFoundError`, same for `cv2`, while `onnxruntime 1.27.0`, `numpy 2.4.6` and `PIL 12.3.0` **do** import. So the interpreter and the runtime already exist on this machine, which changes what the plan assumed about cost — the sidecar needs one pip install plus its weights, not a toolchain — and this batch may do neither: *"Do not install interpreters, download model weights, launch model servers, or call paid APIs."* What shipped instead is the **replaceable contract** (`ocr::TextExtractor`, `OcrRequest`/`OcrResult` with their bounds, `SidecarProtocol`'s one-line request and one-line reply, `BackendStatus` with a `Verified` arm nothing in this build can return) plus the tests of the translation, and §52 row 1 records the runtime as unverified rather than passing it off as working inference. |
| PowerShell (`pwsh`) | current | MIT | ships with Win11 / winget | — | **ADOPT** |
| Git | current | GPL-2.0 (tool, not linked) | winget | — | **ADOPT** — used as an external CLI only |
| Docker CLI | current | Apache-2.0 | user-installed | Docker Desktop | CANDIDATE — only if the user has it |

### Coding agents (all external, all supervised)

The versions below are what the upstream releases offered on 2026-11-04. What is *installed here*, and what this build has actually proved itself able to speak to, is the next subsection — the two are not the same claim.

| Agent | Version | License | Windows | Mechanism | Verdict |
|---|---|---|---|---|---|
| **Codex** | `rust-v0.166.0` | **Apache-2.0** | Native MSVC x64/arm64 | ACP adapter · `codex exec --json` · `codex app-server` (JSON-RPC stdio) | **ADOPT (first)** |
| **OpenCode** | v1.19.5 | **MIT** | `opencode-windows-x64.zip` | Native `opencode acp` · `opencode serve` (HTTP + OpenAPI 3.1) | **ADOPT (second)** — cleanest license + API |
| **Goose** | v1.57.0 | Apache-2.0 | `goose-acp-x86_64-pc-windows-msvc.zip` | Native `goose acp` | CANDIDATE |
| **Gemini CLI** | npm 0.66.0 | Apache-2.0 | Node | Native `gemini --acp` | CANDIDATE — needs a Google account |
| **Qwen Code** | npm 0.26.6 | Apache-2.0 | Node | `qwen serve` (SSE) · **`--experimental-acp`** | CANDIDATE — flag is experimental |
| **Claude Code** | npm 2.1.317 | **PROPRIETARY** | npm | `claude -p --output-format stream-json` · ACP via `@agentclientprotocol/claude-agent-acp` 0.100.0 (Apache-2.0) | CANDIDATE — **we can drive it, we cannot ship or redistribute it** |
| **Aider** | last push 2026-05-22 | Apache-2.0 | pip | **none** — no `--json`, no MCP, no ACP | **AVOID** — PTY-scraping only |

### What is installed on this machine, and what each one gives JEAN (re-verified 2026-11-05, read-only `--version` / `--help`)

Nothing here was installed, authenticated, updated or paid for by this phase, and no agent was launched into a session. Versions are the machine's, not upstream's newest: that gap is why the verdicts below speak about *this* build.

| Binary | Version here | Interface this build can actually use | Evidence |
|---|---|---|---|
| `codex` | **codex-cli 0.160.1** | **Structured CLI fallback** — `jean-acp`'s `codex-cli` kind, reading `codex exec --json` JSONL. Its `--help` lists `exec`, `app-server`, `mcp`, `review` and others, and **no `acp` subcommand**, so the ACP connector cannot be pointed at it as installed. An adapter is a third-party process this build does not install. | `codex --help` output; the `app-server` handshake is a *different* protocol from ACP and is not implemented here. |
| `opencode` | **1.18.34** | **Native ACP** — `opencode acp`, which its own help text describes as "start ACP (Agent Client Protocol) server". This is the worker the `acp` kind is written for. | `opencode --help`; *the session itself has never been run* — see the live-verification line below. |
| `claude` | **2.1.303** | Nothing in this build. No ACP in the binary itself; the ACP path needs `@agentclientprotocol/claude-agent-acp`, which is not installed and would be a third-party runtime. | `claude --version`; `WORKER_KINDS` is `acp \| codex-cli`, so a Claude worker is a configuration this build refuses at load, not one it runs and hopes. |
| `node` | **v24.22.1** (`C:\Program Files\Volta\node.exe`) | The interpreter for the fixture peers the tests use, and for any `command`-type MCP server. | Used by `crates/jean-core/tests/{mcp_live,delegation_live}.rs` and `apps/jean-cli/tests/cli_mcp_serve.rs`. |
| `git` | **2.56.0.windows.4** | Baseline and changed-file measurement for delegation, and `worktree` isolation. | `crates/jean-core/src/delegation.rs`; the PnP verbatim-path fix (D-044) was found *because* a real `git` repo was involved. |
| Chrome / Edge / Brave | Chrome **156.0.8078.81** | The D-046 browser child. **No browser has been launched, no debugging port opened.** | IMPLEMENTATION_STATUS.md §39; the driver code does not exist yet (D-046 is ACCEPTED, implementation blocked on free disk). |

**Live-verification status, stated precisely:** the ACP connector has completed a full handshake, session, streamed message, permission request and prompt-response against a **Node script that JEAN writes and starts** — that proves JEAN's protocol side, not compatibility with a vendor agent (D-047). `jean delegate` has run end-to-end against a scripted worker. **No live coding agent — Codex, OpenCode or otherwise — has completed a delegation on this machine**, and `crates/jean-core/tests/delegation_live.rs` carries the two open defects (§38 of IMPLEMENTATION_STATUS.md: a session that does not settle on the agent's final response, and a session deadline that does not bound it) as `#[ignore]`d tests with the reason in the attribute. `jean agent status` says this in its own output rather than letting "supported" mean more than it does.

### Codex detail (verified from `codex-rs/exec/src/exec_events.rs` and `codex-rs/core/config.schema.json`)

- **JSONL events:** `thread.started`, `turn.started`, `turn.completed`, `turn.failed`, `item.started`, `item.updated`, `item.completed`, `error`
- **Item types:** `agent_message`, `reasoning`, `command_execution`, `file_change`, `mcp_tool_call`
- **Flags:** `--json` (alias `--experimental-json`), `--output-last-message <FILE>`, `--output-schema <FILE>`, `--skip-git-repo-check`
- **Hooks (`HooksToml`, 14 events):** `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreModelRequest`, `PostModelRequest`, `PreToolUse`, `PostToolUse`, `PermissionRequest`, `PreCompact`, `PostCompact`, `Stop`, `Interrupt`, `SubagentStart`, `SubagentStop`. Handler fields: `command`, **`commandWindows`**, `timeout`, `async`. Types: `command | mcp_tool | prompt | agent`.
- **`codex app-server`** is a separate Windows asset (`codex-app-server-x86_64-pc-windows-msvc.exe`); the in-repo client crate documents the initialize handshake, `thread/list`, `thread/read`.

> The `commandWindows` hook field and the `mcp_tool` handler type are the highest-value integration surface in the whole coding-agent landscape: Codex can call **our** MCP server as a hook handler, letting the policy engine approve, block or rewrite its actions in real time.

---

## 6. Models

### Voice models (sherpa-onnx releases)

| Purpose | Asset | Size | License |
|---|---|---|---|
| **Streaming ASR (chosen, D-054)** | **`sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2`** | **506,956,414 B — matched by the supplied file; no `digest` is published, so this machine recorded its own hash: `455f40e556aa2b20ac9d3bffd603b58002075c1193b4070938540c11efe0a4da`** | **Apache-2.0 on the exported weights, read from the model card shipped inside the bundle (`license: apache-2.0`, naming `marcoyang/icefall-libri-giga-pruned-transducer-stateless7-streaming-2023-04-04`). LibriSpeech + GigaSpeech are the *training data* and carry their own terms — the grant above is the one on the weights, which is the thing that matters here. Extracted byte-exactly and **used for real inference**: 13 files, 546,279,902 B, transcripts in §60.5** |
| Streaming ASR (lean) | `sherpa-onnx-streaming-zipformer-en-20M-2023-02-17.tar.bz2` | 127.9 MB | `UNVERIFIED` — not measured this phase |
| Streaming ASR (accurate) | `sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2` | 310.4 MB | `UNVERIFIED` — not measured this phase |
| **VAD (chosen)** | **`silero_vad_v5.onnx`** | **2,313,101 B (measured; no `digest` published)** | **MIT — `snakers4/silero-vad` repository licence, read from the GitHub API. Runs at 16 kHz with a 512-sample window only; the code skips it rather than feeding it other rates** |
| VAD (digest available) | `silero_vad.onnx` | 643,854 B (measured) | MIT. `sha256:9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6` |
| TTS (lean, int8) | `vits-piper-en_US-lessac-medium-int8.tar.bz2` | 21.0 MB | `UNVERIFIED` (per-voice MODEL_CARD) |
| TTS (full) | `vits-piper-en_US-lessac-medium.tar.bz2` | 67.2 MB | `UNVERIFIED` |
| **TTS (selected, D-055)** | **`kokoro-en-v0_19.tar.bz2`** | **319,625,534 B, `sha256:912804855a04745fa77a30be545b3f9a5d15c4d66db00b88cbcd4921df605ac7`; int8 variant `kokoro-int8-en-v0_19` 103,248,205 B, `sha256:c9f0dd393615805b0bab050c340834d5e684e732aec91c0e860cd30e982c08bd` (GitHub API, measured)** | **`Apache-2.0` on the exported weights — from the source model `hexgrad/Kokoro-82M`, revision `f3ff3571791e39611d31c381e3a41a3af07b4987`, whose model card's own `license` field says apache-2.0. The same grant is **inside the archive** as `LICENSE` (11,358 B), read rather than inferred. This is the grant D-054 §7 said was missing, and it is on the weights rather than on a dataset. SUPPLIED, VERIFIED (digest matched, 360/360 files extracted byte-consistently), AND RUN — with one asset short: 2.0.1 loads `model.onnx` + `voices.bin` + `tokens.txt` and then refuses text with *"No lexicon available. Please provide --kokoro-lexicon"*, because it retired the espeak-ng path the bundled `espeak-ng-data` belongs to. The acoustic model itself was reached through `GenerationConfig::phoneme_codepoints` and synthesized 1.03 s of real audio at 24 kHz (§61.3). Text-to-speech stays PENDING on `voice.models.tts_lexicon`** |
| **TTS (examined, D-054)** | **`vits-piper-en_US-amy-low.tar.bz2`** | **67,095,344 B, `sha256:c70f5284a09a7fd4ed203b39b2ff51cac1432b422b852eb647b481dade3cf639` (measured)** | **`UNVERIFIED — rejected, kept for the record.` Its MODEL_CARD says dataset `MycroftAI/mimic3-voices`, licence "See URL", and "finetuned from U.S. English Ryan voice (low quality)". A dataset pointer plus a base voice with its own terms is not a redistribution grant for the derived weights — and the `rhasspy/piper-voices` dataset answers **HTTP 401** to an anonymous API request, so the terms are not machine-readable either** |
| TTS (alt, measured) | `vits-piper-en_US-amy-low-int8.tar.bz2` | 21,099,246 B, `sha256:93070ac9fadf512e56c46bdd0c5d2ce96b424fdc4e683d560167410bd2c4df7d` | `UNVERIFIED` — same card, same gap |
| TTS lexicon (measured) | `lexicon-en-us.txt` | 6,305,773 B, `sha256:8b67ae549a465eee78f92ecb74644ad90c3c58ce65f19c68baf85878a10ef73a` | as above |
| TTS (alt) | `matcha-icefall-en_US-ljspeech.tar.bz2` | 76.8 MB | `UNVERIFIED` |
| Keyword spotting | `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2` | 17.6 MB | `UNVERIFIED` — and unused: no wake word is implemented (D-013) |

**~169 MB lean / ~415 MB full** — that estimate predates the measurement, and both
numbers are now superseded by provisioned facts (§60.2): the four files recognition
actually loads (int8 encoder + fp32 decoder + int8 joiner + tokens) are
**190,180,941 bytes**, while the unpacked ASR *directory* is 546,279,902 because it
also carries the fp32 duplicates and the test clips. What neither estimate counted
is the native library set, which is a build-time cost of 1,057,860,400 bytes —
944,135,846 of it `onnxruntime.lib`. So "provisioned" on this machine means
**~1.2 GiB of assets and libraries**, not ~415 MB. *Every MODEL_CARD must be read
before bundling* — and reading one is not the
same as it granting redistribution: `amy`'s card was read, and what it says is "See URL".
`docs/voice-assets.md` §4 is the gate.

### Vision / grounding models

| Model | Weight license | GGUF available | Verdict |
|---|---|---|---|
| **ShowUI-2B** | **MIT** (HF confirmed) | `localattention/ShowUI-2B-Q4_K_M-GGUF` (MIT tag) | **ADOPT** — light grounding, runs on CPU/Vulkan |
| **UI-TARS-1.5-7B** | **Apache-2.0** (HF confirmed) | `mradermacher/UI-TARS-1.5-7B-GGUF` (20.6k dl), `Lucy-in-the-Sky/…-{Q4_K_M,Q6_K,Q8_0}-GGUF` | **CANDIDATE** — best-licensed, but 7B: strictly on demand |
| **OmniParser v2.0** | weights **MIT**; **repo CC-BY-4.0** | Python only | CANDIDATE — v2 weights only |
| OmniParser v1 detector | **AGPL-3.0 lineage** (Ultralytics YOLO) | — | **AVOID — never** |
| **Jedi-3B-1080p** | upstream HF **gated (401) → `UNVERIFIED`**; quantizer tags are not proof | `mradermacher/Jedi-3B-1080p-GGUF` | **BLOCKED** pending license diligence |

---

## 7. Rejected dependencies — with reasons

| Rejected | Version | Why |
|---|---|---|
| `conpty` (zhiburt) | 0.7.0 | Published 2024-09-23, repo last push same day, 24★. **Unmaintained > 2 years.** Rejected by D-016; raw ConPTY (D-024) replaced the need for any crate here. |
| `portable-pty` | 0.9.0 | **Maintained and well-licensed — and still not ours to use.** It abstracts a pty for a *terminal*: `PtyPair`/`SlavePty` and a `Box<dyn DynChild>` that does not expose the process handle, so Job Object assignment, `ResumeThread` ordering and the cancel pipe all have to be re-derived, and our supervision loop forks in two. D-024. |
| `reqwest` | 0.12.x | **Maintained, MIT/Apache, and not ours to take.** Its dependency closure (hyper, tokio, rustls/native-tls, tower, http-body variants) is several times the size of this workspace, for one POST per command with a byte ceiling, a deadline and a cancel flag of our own. D-029. |
| ~~`ureq`~~ | 3.5.1 | **No longer rejected — ADOPTED by D-038 (2026-11-05).** The reason recorded here ("pulls a TLS provider this build cannot use or test, because the two supported providers are loopback") held only while loopback was the only supported shape, and reading it back after a TLS client existed exposed the inverse hole: `https` was refused while a remote `http` endpoint was accepted. Taken now with `default-features = false, features = ["native-tls"]`, so the TLS provider is the operating system's own. The original reasoning is kept in D-029, which D-038 supersedes. |
| `winapi` | 0.3.9 | Frozen since 2020-06-26. `windows` supersedes it. |
| `com` (com-rs) | 0.6.0 | Last publish 2021-10. Superseded by windows-rs native COM. |
| `device_query` | 4.0.1 | **Repo archived 2026-03-31.** Polling only, not injection. |
| `rdev` | 0.5.3 | No publish since 2023-06 (29 months). It is a global *listener*, not an injector; hook-based input on Win11 is fragile. |
| `screenshots` | 0.8.10 | 20 months stale; superseded by the same author's `xcap`. |
| `captrs` | 0.3.1 | Dead since 2020-12. |
| `scap` | 0.0.8 | Video-workshop oriented, pre-1.0, quiet 15 months. |
| `wgc` | 3.1.0 | 10★ — too small a bus factor to bet the capture layer on. |
| `named_pipe` | 0.4.1 | Dead since 2019. |
| `winrt-notification` | 0.5.1 | Built on `windows ^0.24` (2021) — incompatible dependency era; last publish 2022. |
| `win-toast-notify` | 0.1.6 | Pre-1.0, 12k downloads, low adoption. Raw WinRT is a well-trodden path. |
| `copypasta` | 0.10.2 | Alacritty-internal, X11/Wayland-centric. |
| `clipboard-win` | 5.4.3 | **BSL-1.0** — permissive and OSI-approved, but `arboard` is dual MIT/Apache and maintained by 1Password. |
| `elevate` | 0.6.1 | Stale (2023), pre-1.0, 26k downloads. |
| `sudo` | 0.6.0 | Stale (2021). |
| `runas` (mitsuhiko) | 1.2.0 | Works (~50 LOC over `ShellExecuteW` verb `runas`) but unpublished 20 months and lists no repo. Raw `ShellExecuteW` is safer and equally short. |
| `token-privilege` | 0.1.2 | 113 downloads, unknown-vendor repo — `UNVERIFIED`. Do not depend on it. |
| `leptess` | 0.14.0 | Stale (2023) + external Tesseract C dependency. |
| `sqlx` | 0.9.0 | MSRV **1.94** (highest in the stack), still 0.x, async overhead unnecessary for a single-user local store. Keep as a migration path. |
| `diesel` | 2.3.14 | ORM + codegen disproportionate to a supervisor's modest schema. |
| ~~`ureq`~~ | 3.5.1 | **Adopted for `jean-model` by D-038.** The row as written judged it for a different purpose — an otherwise-async app that already pulls `reqwest` through `rmcp`; that appraisal still holds there, and this workspace is not that app. |
| **`tauri`** | 2.12.5 | WebView2's process tree puts RSS at **80–200 MB — blows the 50 MB idle budget** — and pulls the heaviest dependency tree in this list. Revisit only as a separate optional UI binary. |
| `winit` | 0.30.13 | No visible window needed for a tray-first orchestrator in v1. |
| `interprocess` | 2.4.7 | Redundant given tokio's first-party named pipes. Keep only if cross-platform local sockets become a requirement. |
| `playwright` (Rust crate) | 0.0.20 | **Dead since 2022-08-20.** There is no maintained Rust Playwright client. |
| **Aider** | — | No machine-readable output, no MCP, no ACP. PTY-scraping only. |
| **Piper standalone** (`piper1-gpl`) | — | **GPL-3.0** code. Its MIT-licensed VITS voices survive via sherpa-onnx — use that path. |
| **TEN-VAD** | — | Non-SPDX custom license. Use Silero (MIT). |
| `whisper-rs` | 0.16.0 | Works, but drags a C++/cmake build into Rust CI, and whisper.cpp is **batch-only** (no true streaming). Fallback only. |
| `moonshine` (direct) | — | **No Rust binding exists** (python/swift/android/wasm only). Use its models *through* sherpa-onnx instead (`rust-api-examples/run-moonshine-v2.sh`). |
| `processkit` | 3.3.4 | Async kill-on-drop supervision on Job Objects — interesting, but younger than `win32job`. Evaluate later. |

---

## 8. License posture

**No copyleft anywhere in the adopted stack.** All `ADOPT` crates are MIT, Apache-2.0, CC0-1.0 or 0BSD.

Enforced mechanically, not by discipline:

```toml
# deny.toml
[licenses]
allow = ["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "CC0-1.0", "0BSD", "ISC", "Unlicense", "PSF-2.0"]
confidence-threshold = 0.8

[bans]
multiple-versions = "warn"
deny = [
  { name = "winapi" },        # legacy, frozen since 2020
  { name = "tauri" },         # blows the idle-RSS budget in the core
  { name = "conpty" },        # unmaintained since 2024
  { name = "device_query" },  # archived upstream
]

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

### Things that are NOT open source and must never be treated as shippable

| Component | License | Consequence |
|---|---|---|
| **Claude Code** | Proprietary — "© Anthropic PBC. All rights reserved" | We may **drive** it if the user installed it. We may not bundle, redistribute, or depend on it. The ACP *adapter* is Apache-2.0; the underlying CLI is not. |
| **OmniParser v1 icon detector** | AGPL-3.0 lineage (Ultralytics) | Never. v2 weights (MIT) only. |
| **OmniParser repo** | CC-BY-4.0 (a content license, not a software license) | Attribution required if we quote its docs. |
| **Git** | GPL-2.0 | Fine — it is an external CLI we invoke, not a linked library. |
| **Piper (`piper1-gpl`)** | GPL-3.0 | Not linked. Its MIT voices are consumed through Apache-2.0 sherpa-onnx. |

### Model weights are a separate question from code

Every voice and vision model above needs an individual `MODEL_CARD` read before it is bundled or redistributed. Several are `UNVERIFIED` because the HuggingFace pages are gated (401). **Do not bundle a model whose card you have not read.** This is tracked as risk **R-08** in [`06-RISKS.md`](06-RISKS.md).
