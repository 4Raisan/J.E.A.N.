# J.E.A.N. desktop learning, playbooks, and memory

## Performing Known Tasks vs. Solving New Tasks

### Known Tasks (Playbooks & Desktop Workflows)
J.E.A.N. stores reusable procedures in SQLite (`playbooks` + `playbooks_fts` + `playbook_runs`, schema v6) alongside desktop overlay shortcuts:

- **Voice & overlay commands**:
  - `Remember workflow morning as open YouTube; check my mail; what time is it` (automatically mirrored into Rust SQLite playbooks).
  - `Run workflow morning`, `Show workflow morning`, `Undo workflow morning`, `Forget workflow morning`, `List workflows`.
  - `Remember workflow clips as search {query} on YouTube`, then `Run workflow clips with query window automation` (or simply `Search window automation on YouTube`).
  - `List playbooks`, `Show playbook <name>`, `Run playbook <name>`, `Undo playbook <name>`, `Forget playbook <name>`.
  - `Remember this as quick check` / `Open it again` / `Run that again`.
- **CLI commands**:
  - `jean playbook list | show | teach | run | undo | forget | match | history`
- **Live resolution & verification**:
  - Replaying a known playbook re-resolves apps, windows, and tools against the current desktop/machine and routes every step through JEAN's policy engine and independent verifier.
  - Dispatch, `verified_success`, `failed`, `uncertain`, and `cancelled` runs are recorded separately in `playbook_runs`, and modifying a playbook preserves `previous_steps_json` so `undo` restores the prior version.

### New Tasks (Autonomous Solver + Automatic Skill Capture)
When a request does not match a known playbook, J.E.A.N.'s **New Task Solver** (`crates/jean-core/src/solver.rs`) inspects local capabilities, registered tools, workspace selection, and recalled memory lessons to pick a grounded execution route:

- **System & tool probes** (CPU/RAM processes, disk space, Git status, Windows version, file search) run directly through verified intents even when no model is configured.
- **Desktop apps** (`Open Calculator and calculate 7 times 8`, `In Notepad, type hello`, or **More → Work in an app…**): runs a bounded UI Automation loop (up to 10 verified actions: `invoke`, `set_value`, `toggle`, `select`, `expand`, `collapse`, `set_focus`) with explicit per-task observation consent, redacted witness verification, and password readback limited to masked length.
- **Workspace files** (**More → Work in a workspace…** or workspace file requests): reads, lists, or creates top-level workspace files with mandatory read-back verification and partial-write rollback.
- **Controlled browser** (`/browse <url>` or webpage inspection): runs in an isolated profile with post-navigation/post-click origin allowlist revalidation.
- **Coding tasks**: select a workspace and use `/delegate <goal>` or `Start with <worker>. <goal>`.
- **Self-learning from results**: when a new task completes and passes independent verification, J.E.A.N. records a `task_success` episode and automatically captures a reusable playbook (`source = "learned_from_task"`) so future requests run as a **Known Task**.

### Memory, Self-Updating & Dreaming
- **Explicit facts**: `Remember that my project uses Rust`, `What do you remember about Rust?`, `List memories`, `Forget memory <id>`.
- **Self-updating semantic memory**: statements like `My editor is VS Code` followed later by `Actually, my editor is Cursor` (or `Update memory: ...`) automatically supersede the outdated note on the same `topic_key` while preserving audit lineage and rejecting credential-shaped text.
- **Memory dreaming (consolidation)**:
  - Say `Dream now` / `Consolidate memories` (or run `jean memory dream`) for offline deterministic consolidation: harvests recent tasks and episodes, extracts recurring failure lessons, decays/archives stale untouched notes, and promotes repeated verified task patterns into playbooks (`source = "dream_promoted"`).
  - Say `Deep dream` (or run `jean memory dream --deep`) for read-only model-assisted synthesis over redacted episodes and active notes.
  - The desktop overlay also runs automatic background idle dreaming when idle after accumulating new activity.

---

## Evaluation & Stage Plans (Historical & Ongoing)

### Session 1 — Qoder, baseline and regression tests
> Work on J.E.A.N.'s Windows desktop learning feature. J.E.A.N. is an orchestrator that can call installed apps and delegate implementation to existing coding agents. Read this plan, the overlay, and the Rust memory and desktop entry points. Use disposable profiles and synthetic workflows only. Write regression tests that cover teach-without-execute, same-name replacement, recall after restart, case-insensitive recall, forgetting, malformed saved records, and a failed middle step preventing the remaining steps. Test that ordinary Chat, Wake, clock, Gmail, and browser routes still work. Report commands, exit codes, and actual versus simulated checks. Preserve existing changes.

### Session 2 — Typed SQLite Playbooks across contexts (Shipped in Schema v6)
Implemented in `crates/jean-store/src/cognitive.rs`, `crates/jean-core/src/playbooks.rs`, and `apps/jean-cli/src/playbook.rs`: typed multi-step playbook schema, explicit parameters (`{query}`, `{url}`, etc.), workspace scope, versioned updates with `undo`, per-step live re-resolution and verification, stop-on-failure semantics, and automatic desktop workflow mirroring.

### Session 3 — Holdout evaluation
> In a separate scratch profile, build an evaluation set independently of the implementation fixtures. Cover paraphrases, app aliases, follow-ups, changed input values, a second workspace, stale app identities, focus changes, corrections, cancellation, and restart. Measure retrieval success, verified completion, wrong-target or duplicate actions, p50/p95 latency, and sample counts. Compare exact retrieval first, then any proposed model-assisted approach on identical cases. Record failures with the shortest reproducible scenario.

### Session 4 — Learning from results & Dream promotion (Shipped in `dream.rs` & `playbooks.rs`)
`playbook_runs` records dispatch, `verified_success`, `failed`, `uncertain`, and `cancelled` separately. Verified new task completions capture learned playbooks, and `dream::dream()` promotes repeated verified task patterns into playbooks while keeping previous revisions recoverable via `undo`.
