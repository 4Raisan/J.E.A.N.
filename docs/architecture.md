# Architecture (shipped reality, Phase 6)

Core: Rust `jean` (Cargo workspace 14 members incl. tray/elevated/voice).
Supervision: Job Objects `KILL_ON_JOB_CLOSE`, ConPTY, named-pipe cancel w/ per-user DACL, SQLite WAL + FTS5.
Verification: every act declares `Proven/NotProven/Unverifiable`; exit codes 0/1/2/3/4/5/130.
Policy: tiers, `run_scope` grants (operator goal authorizes run, never a step auto-grant), audit JSONL.
Integrations: ACP (opencode) + codex-cli, MCP stdio both ways, installed-Chrome CDP (loopback, empty profile), UIA locator-first + `SendInput` fallback, OCR sidecar (RapidOCR stdio JSON).
Workers: `jean-voice` (sherpa-onnx optional, `sherpa` feature) and `jean-tray`. The reserved `jean-elevated` binary only refuses requests.
Budget: target <50MB idle core; workers separate processes, measured separately.
