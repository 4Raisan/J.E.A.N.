# Architecture (shipped reality, Phase 6+)

- **Core**: Rust `jean` Cargo workspace (14 crates/apps/workers: `jean-cli`, `jean-tray`, `jean-elevated`, `jean-voice`, `jean-core`, `jean-store`, `jean-policy`, `jean-process`, `jean-protocol`, `jean-tools`, `jean-model`, `jean-acp`, `jean-mcp`, `jean-browser`, `jean-windows`) plus the PyQt6 desktop overlay (`tools/jean-desktop`).
- **Supervision**: Windows Job Objects with `KILL_ON_JOB_CLOSE`, ConPTY, named-pipe cancellation with per-user SID DACL (`D:P(A;;FA;;;<SID>)`), allowlisted child environments, and crash reconciliation.
- **Persistence & Cognitive Memory (SQLite WAL + FTS5, Schema v6)**:
  - v1–v4: `tasks`, `tool_runs`, `events`, `tool_health`, `approvals`, `plans`, `plan_steps`, `delegations`, `worker_events`.
  - v5: `tasks_fts`, `events_fts`, `saved_facts` + `saved_facts_fts` (strictly operator-authored), `preferences`, metadata-only `memory_changes`.
  - v6: `memory_episodes` + `memory_episodes_fts` (redacted episodic observations), `memory_notes` + `memory_notes_fts` (self-updating semantic memory with `topic_key` contradiction superseding, salience reinforcement/decay, and archival), `dream_cycles` (offline deterministic + optional deep model-assisted consolidation logs), and `playbooks` + `playbooks_fts` + `playbook_runs` (typed versioned recipes for Known Tasks).
  - All FTS5 triggers and `rebuild_history_index()` enforce `jean_redact` and 512-char bounds; all writers enforce the credential floor.
- **Known vs. New Task Execution**:
  - **Known Tasks** (`jean_core::playbooks`): matched by exact name, exact trigger, `{param}` trigger template, or high-confidence FTS5 recall over verified playbooks; re-resolved live and executed step-by-step through policy and verification.
  - **New Tasks** (`jean_core::solver` + `jean_core::planner`): deterministic capability routing + optional model-assisted strategy selection across registered tools/plans (`plan_runner`), bounded UI Automation (`desktop_goal`), bounded workspace file tasks (`workspace_goal`), controlled browser (`jean-browser`), coding-agent delegation (`delegation`), or single-question clarification. Verified new task completions automatically capture reusable playbooks.
- **Verification**: every act declares `Proven` / `NotProven` / `Unverifiable`; exit codes `0 / 1 / 2 / 3 / 4 / 5 / 130`.
- **Policy**: tiers, `run_scope` grants, explicit approval for `workspace_goal_access` and `desktop_goal_observation`, JSONL audit trail; model output has no path to grant permission.
- **Integrations**: ACP (`opencode`) + `codex-cli`, bidirectional MCP stdio, isolated Chrome/Edge CDP (loopback, empty profile, origin revalidation after actions), Windows UIA (locator-first, relative depth from search root, masked-length-only password readback) + `SendInput` fallback, OCR sidecar.
- **Workers & UI**: `jean-voice` (Fixture / Windows SAPI / optional `sherpa` feature with clean per-turn recovery), `jean-tray` (Win32 message loop + embedded RGBA icon + readable status console), and `tools/jean-desktop` (audio-reactive orb, continuous wake/conversation voice loop, non-blocking memory/dream/trash background threads, Core Audio endpoint selectors). `jean-elevated` remains a refusal stub.
- **Budget**: target <50 MB idle core; voice, overlay, browser, and coding-agent workers run as separate processes.
