# J.E.A.N. — Implementation Status

**Current baseline, 2026-11-08:** Start with the [source review and execution plan](PLANNING/09-LUNA-EXECUTION-PLAN.md). Sections below preserve batch evidence. Checkpoint helpers and Phase 6 groundwork now exist in source. Their integration and current-tree validation are pending. The owner's voice comparison supports Wispr input for Milestone 5.5 while native recognition is evaluated separately.

> **Two parts.** Sections 0–7 are the Phase-0 batch, kept verbatim: its counts (391 tests,
> 16,281 lines) describe that batch, not this one, and Phase 0's behavior is unchanged.
> Sections 8–15 are Phase 1, written after running the real binary against live Windows
> behavior. Section 8's table is the current state of the four claims.

**Phase 0's code compiles, its tests execute and pass, and its core behaviors have
been demonstrated with the real binary.** It is not yet *accepted*: the open decisions in
section 6 need a reviewer, and `PLANNING/05-ROADMAP.md` remains the authority on what
Phase 0 is for.

---

## 0. Four separate claims

| Claim | Status | Basis |
|---|---|---|
| **Implementation written** | **Yes.** 16,281 lines of Rust in 37 files across five workspace members; each required core capability has a real, non-stub implementation. | File inventory, section 1. |
| **Compiles** | **Yes.** `cargo build --workspace --locked` and `--all-targets` both exit **0** inside the VS x64 developer environment. | Section 2, section 3.1. |
| **Runtime behavior verified** | **Yes for the ten core behaviors.** Each was observed against the real `jean` binary with harmless fixtures and confirmed by inspecting the outcome — processes, artifact bytes, recorded state — not by trusting an exit code. | Section 3.3 demonstration table. |
| **Tests pass** | **Yes.** **391 `#[test]` functions executed, 391 passed, 0 failed.** | Section 3.2 per-target counts. |

What is **not** claimed: that Phase 0 has been reviewed and accepted, that
`cargo deny check` was run (the tool is not installed — section 3.1), or that the
demonstrations cover every setting (section 6 lists what is parsed but not acted on).

### Supplementary evidence is not verification

Section 3.5 records the symbol and arity sweeps from the pre-build batch. They remain
**supplementary**. Everything in sections 3.1–3.3 is compiler- or runtime-observed.

---

## 1. Implementation written

A Cargo workspace of five members. 16,281 lines of Rust in 37 files.

| Member | Role | Lines |
|---|---|---|
| `crates/jean-protocol` | Types with no dependency on the rest: config schema, tool schema, nine-state machine, redaction, events, IPC wire format, execution vocabulary | 3,525 |
| `crates/jean-store` | SQLite persistence, `PRAGMA user_version` migrations, WAL | 1,743 |
| `crates/jean-process` | Windows process supervision: Job Objects, `CREATE_SUSPENDED`, bounded capture, cancellation | 4,007 |
| `crates/jean-core` | Orchestration: layered config, tool resolution, environment allowlist, cancellation pipe, crash reconciliation, verification | 6,404 |
| `apps/jean-cli` | The `jean` binary — four commands | 602 |

### The seven required core capabilities — as written and as observed

1. **Typed configuration and tool definitions.** `AppConfig` with
   `deny_unknown_fields` on every section, so an unknown key is a hard error and not a
   silently ignored setting. `config_version` is compared against
   `SUPPORTED_CONFIG_VERSION`; a version from the future is refused with a message naming
   both numbers. Layered load: `<repo>/config/defaults.toml` → `<data_dir>/jean.toml` →
   `./jean.toml`, two-pass so a `JEAN_DATA_DIR` relocation still reads the configuration
   that lives there. `ToolRegistry` validates executable, argument vector, working
   directory, timeout and environment at load time. **Observed:** a registry containing a
   bad escape sequence was refused at startup with file, line, column and reason, and
   `jean run` then reported `Known tools: none are configured` instead of guessing.
2. **Launching registered tools with explicit arguments and working directories.**
   `plan_invocation` builds the vector in a fixed order (`tool.args`, then `extra_args`,
   then caller arguments); `cmdline.rs` encodes it with the MSVCRT quoting rules.
   `ToolDef` has **no field for a command line**, so there is no way to concatenate user
   input into a shell. **Observed** in demonstration D1.
3. **Supervising children and descendants.** `CreateProcessW` with `CREATE_SUSPENDED` →
   `AssignProcessToJobObject` into a job armed with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` →
   `ResumeThread`. If assignment fails the child is terminated and never resumed, and the
   run records `Termination::JobAssignmentFailed`. **Observed** in D4, D5 and D8:
   descendants J.E.A.N. did not directly spawn died with the job — on cancel, on timeout,
   and when the orchestrator was killed.
4. **Streaming bounded stdout/stderr.** Two anonymous pipes, two reader threads draining
   concurrently. The cap bounds the *artifact file including markers*; past the cap the
   reader keeps draining and discards, so the child cannot block on a full pipe.
   **Observed** in D2 and D3.
5. **State and events in SQLite.** `tasks`, `tool_runs`, `events`. Output is a path plus a
   byte count, never a blob. Every transition writes an event. **Observed** in D4, D5, D8
   and D9 via `jean task <id> --events`.
6. **Listing, inspecting, cancelling, timing out.** `jean tasks`, `jean task`,
   `jean cancel`, per-tool `timeout_secs` that kills the tree and classifies the run
   `Termination::Timeout`. **Observed** in D4–D7.
7. **Recovery after a crash.** `reconcile` walks in-flight tasks, tests the recorded
   runner on `runner_pid` **and** `runner_created_ms` together (a recycled pid with a
   different creation time is dead), marks unfinished runs `Termination::Orphaned`,
   transitions to `needs_attention`, clears ownership, and appends an event saying the
   command was not replayed. **Observed** in D8 and D9.

### Design points worth flagging for review

- **Cancellation from a second invocation** uses a named pipe at
  `\\.\pipe\jean.task.<task_id>` (`DEFAULT_PIPE_PREFIX` in
  `crates/jean-protocol/src/ipc.rs`; `ipc.pipe_prefix` overrides it), created with
  `FILE_FLAG_FIRST_PIPE_INSTANCE` and a protected DACL
  **`D:P(A;;FA;;;<the current user's SID>)`**, the SID read from the process token at run
  time. **There is no network listener** — verified in section 3.3, row "+1".
  `crates/jean-core/src/ipc.rs` records the rejected alternatives (TCP loopback, signal
  file, `GenerateConsoleCtrlEvent`).
- **The trustee is an explicit SID, not `CO`.** A Creator Owner ACE looks equivalent and
  is not: a pipe instance's owner comes from the *connecting client*, so `CO` authorised
  neither the server's own later instances nor a client `CreateFileW`. Measured here: with
  `CO`, `jean cancel` failed `ACCESS_DENIED`. The unit test asserts `CO` is absent from the
  descriptor.
- **A second client is not a crash.** `jean cancel` talks to the pipe; only if the pipe is
  absent does it fall back to reconciliation, since an absent listener means the runner is
  gone. Read-only commands never mutate: `jean tasks` and `jean task` report the runner as
  `alive` or `gone` without changing anything.
- **Verification is three-valued** (`Proven` / `NotProven` / `Unverifiable`). A forced
  termination can never be `Proven`, even if the child's exit code matches the declared
  expectation, because that code is the one J.E.A.N. chose. A truncated stream whose
  needle is absent is `Unverifiable`, not `NotProven`. `jean run` prints either "result
  completed and verified" or "result command completed; the requested outcome is NOT
  proven". **Observed** in D1–D5.
- **Redaction boundary.** `redact.rs` is a pattern filter, not a secret detector. It
  redacts the argument vector, the environment map written into `tool_runs`, and — since
  this batch — the caller-supplied `--goal` text before it is written into `tasks`. It
  does **not** redact captured stdout/stderr. Detection is not complete and is not claimed
  to be; `config/tools.example.toml` carries an explicit warning on
  `powershell.environment`. **Observed** in section 3.3, row "+2".
- **`orchestrator.resume_on_restart` is read and deliberately not honoured.** Replaying an
  interrupted command is what the specification forbids. **Observed** in D9.
- **Windows only.** `crates/jean-process/src/lib.rs` has
  `#[cfg(not(windows))] compile_error!(…)` because Job Objects are the only containment
  mechanism Phase 0 supports.

---

## 2. Build environment — resolved

### 2.1 What unblocked it

The Windows 11 SDK **10.0.26100.0** desktop components are now installed, and
`vcvars64.bat` populates them:

```
[env] WindowsSdkDir=C:\Program Files (x86)\Windows Kits\10\
[env] WindowsSDKVersion=10.0.26100.0\
[env] UCRTVersion=10.0.26100.0
[env] linker=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\link.exe
```

That last line is the point of the exercise: entering the developer environment is what
makes Cargo select **Microsoft's** linker instead of Git for Windows'
`usr\bin\link.exe` (a hardlink tool that was producing `link: extra operand`). No linker
was copied, no PATH or machine-wide setting was changed, and no software was installed in
this session. The developer environment is applied **per child process** by a scratch
batch file under `target\` (gitignored) that `call`s `vcvars64.bat`, echoes the four
values above, and then runs cargo. From Git Bash:

```bash
export MSYS2_ARG_CONV_EXCL='*'
cmd.exe /c 'target\dev.cmd build --workspace --locked'
```

`MSYS2_ARG_CONV_EXCL` stops Git Bash rewriting cargo's arguments into Windows paths;
without it, subcommand paths and flags arrive mangled.

### 2.2 History: why this was blocked for two batches

Preserved because the diagnosis is the useful part, and because vcvars64 fails **silently**
— it reports `Environment initialized for: 'x64'` with errorlevel `0` while leaving
`WindowsSdkDir` empty.

| Probe | Before the SDK install | Now |
|---|---|---|
| `WindowsSdkDir` from vcvars64 | empty | `C:\Program Files (x86)\Windows Kits\10\` |
| `UCRTVersion` | empty | `10.0.26100.0` |
| `kernel32.lib` under `Program Files` | not found | present under the SDK `Lib` tree |
| `HKLM\…\Windows Kits\Installed Roots\KitsRoot10` | absent | present |
| VS package cache entries matching `Windows1[01]SDK` | 0 of 247 | present |
| Link result | `LNK1181: cannot open input file 'kernel32.lib'`, exit 101 | exit 0 |

**Verify a repair in one command before retrying any build:**

```powershell
cmd /c "call `"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat`" && echo SdkDir=%WindowsSdkDir% && echo UCRT=%UCRTVersion%"
```

Pass condition: `WindowsSdkDir` ends in `Windows Kits\10\` and `UCRTVersion` is
`10.0.26100.0`. If either is empty the build fails `LNK1181` regardless of everything
else, and there is no build-side workaround — none was attempted.

---

## 3. Checks actually run, with observed results

All of the following were executed and their output inspected on 2026-11-04, same machine,
same session.

### 3.1 Toolchain checks

| Check | Command | Observed result |
|---|---|---|
| Build | `cargo build --workspace --locked` | **exit 0.** |
| Build incl. test targets | `cargo build --workspace --locked --all-targets` | **exit 0.** |
| Formatting | `cargo fmt --all -- --check` | **exit 0.** |
| Lints | `cargo clippy --workspace --all-targets --locked -- -D warnings` | **exit 0, no output.** |
| Tests | `cargo test --workspace --locked` | **exit 0**, 391 passed / 0 failed (section 3.2). |
| Dependency audit | `cargo deny check` | **Not run — the tool is not installed** (`error: no such command: 'deny'`). Not installed, per instruction. `deny.toml` exists and parses. |
| Linker identity | `for %i in (link.exe) do echo %~$PATH:i` inside the dev env | `…\MSVC\14.44.35207\bin\Hostx64\x64\link.exe` — Microsoft's linker is the one selected. |
| Lock file | `cargo build --workspace --locked` | cargo accepted `--locked` without complaint, so `Cargo.lock` matches every workspace manifest. |

### 3.2 Tests executed (not "written")

`cargo test --workspace --locked`, per target:

| Target | Passed | Failed |
|---|---|---|
| `jean-core` lib | 147 | 0 |
| `jean-core/tests/config_files.rs` | 11 | 0 |
| `jean-core/tests/recovery.rs` | 8 | 0 |
| `jean-core/tests/redaction.rs` | 2 | 0 |
| `jean-process` lib | 95 | 0 |
| `jean-process/tests/supervision.rs` | 14 | 0 |
| `jean-protocol` lib | 78 | 0 |
| `jean-store` lib | 36 | 0 |
| `jean-cli` bin | 0 (no unit tests) | — |
| doc-tests (4 crates) | 0 (none authored) | 0 |
| **Total** | **391** | **0** |

`jean-process/tests/supervision.rs` takes ~9 s because it spawns real processes;
`jean-core`'s pipe tests create and connect real named pipes.

### 3.3 The ten demonstrations, plus two extras

Fixtures and data live in an isolated demo directory (`%TEMP%\jean-demo`, selected with
`JEAN_DATA_DIR`) whose capture cap is 4 KiB, so truncation is observable without
generating 10 MB. Every row was checked by inspecting the result — `tasklist` for process
death, artifact bytes for capture, `jean task --events` for the record — not by trusting a
launch response.

| # | Requirement | Observed outcome |
|---|---|---|
| D1 | Registered tool, explicit args, working directory | `jean run demo.workdir alpha "beta gamma"` → the child printed `PWD=C:…\jean-demo\scripts` (the **declared** directory, while the CLI's own cwd was the repo root) and `ARGS=alpha\|beta gamma` — two arguments, the space preserved. `succeeded`, verdict `proven`. |
| D2 | stdout and stderr streamed correctly | The stdout artifact (40 B) contained only the two `Write-Output` lines; the `Write-Error` line appeared **only** in the 267 B stderr artifact. No cross-contamination. |
| D3 | Output over the cap is drained without deadlock and shows truncation | The child produced **24,800** bytes against a 4,096 cap and the run finished in 0.25 s (no deadlock). Artifact **3,297 B ≤ cap**, carrying both markers: the cut marker and `[JEAN: the child produced 24800 bytes on this stream; 3207 were retained before the cap.]`. The record says `24800 bytes (truncated)` with a note naming the cap. stderr written **after** the flood was captured intact, proving the second reader kept draining. |
| D4 | Cancellation terminates the owned process tree **including descendants** | `demo.tree` spawns three grandchild `powershell.exe` processes with redirected handles and prints every pid. Before cancel: parent `34460` and kids `26944, 4284, 24544` all present in `tasklist`. After `jean cancel`: **all four gone**, plus the runner. Record: `state cancelled`, `termination cancelled`, child exit 130, verdict `not_proven` — "the child was cancelled, so it never reached a conclusion" — note "cancel requested; the whole job was terminated". |
| D5 | Timeout terminates the tree and reports a timeout | `demo.timeout.tree`: 3 s tool timeout against a 900 s child. Returned after **3.034 s**, `termination timeout`, exit code 124, `state failed`, CLI exit **1**. The parent (`26564`) and all three grandchildren (`32776, 28212, 30268`) were gone. Verdict `not_proven`, note "the 3000ms deadline elapsed; the whole job was terminated". |
| D6 | A second CLI invocation can cancel a running task | `jean cancel <id>` from a **separate process** while `jean run` was blocked in the child: returned "cancellation requested", the runner exited, the record closed as `cancelled` / `not_proven`. Done five times, including against tasks started by a different parent process. |
| D7 | Concurrent CLI clients do not misclassify an active task as interrupted | While D4's task ran, `jean tasks --in-flight` listed it as `running` and `jean task <id>` printed `runner pid … — alive`. No state change, no `reconciled` event, nothing replayed. Repeated after the timeout run. |
| D8 | After killing the orchestrator and restarting, records recover | `taskkill /F` on the runner (pid 22352) mid-task. The job's kill-on-close took the whole tree with it (parent + 3 grandchildren gone). The next `jean run` printed `reconciled 1 task(s) had a runner that no longer exists` and moved the task to **`needs_attention`**. |
| D9 | Interrupted commands are not automatically replayed | The recovered task shows exactly **one** tool run, still `termination orphaned`, `exit code (none)`. Events: `running -> needs_attention: the recorded runner process no longer exists`, then `reconciled: … the command was not replayed and the outcome is unknown`. `orchestrator.resume_on_restart` was left unset and is ignored regardless. |
| D10 | Unrelated processes remain untouched | A `ping.exe` bystander started outside any job (pids `32408`, then `23604`) survived every one of D4, D5, D6 and D8 — cancel, timeout, and an orchestrator kill — and was still in `tasklist` afterwards. J.E.A.N. never enumerates or kills by name. |
| +1 | No network listener | With a task running (`jean` pid 24012): `netstat -ano -p tcp` matched **0** endpoints for that pid and **0** `LISTENING` sockets. The only channel is the named pipe. |
| +2 | Redaction boundary | `jean run --goal "…password=Sup3rSecret999 and token=ghp_AAAA…" demo.echo --token=sk-abcdefghijklmnop` → stored goal `… password=[REDACTED] and token=[REDACTED]`, stored `args_json` `--token=[REDACTED]`; `grep -c` for each raw secret in `jean.db` = **0**. Key names survive; values do not. |

### 3.4 Limitations observed while demonstrating

- **Two `jean cancel` attempts failed with `ACCESS_DENIED` against the binary that existed
  before this batch's rebuild.** The same scenario, same machine, same sources plus one
  debug log line, then succeeded five times out of five. An independent Win32 probe from a
  separate process proved that the descriptor the code writes
  (`D:P(A;;FA;;;S-1-5-21-…-1001)`) *does* authorise a cross-process, same-user open and
  connect, so the DACL is not the cause. This folder has no VCS, so the earlier binary
  cannot be recovered or compared; the discrepancy is recorded as an observation, not
  diagnosed.
- **`--goal` must come before the tool id.** `run`'s trailing argument list uses
  `allow_hyphen_values` (so `jean run powershell.echo -NoProfile -Command "…"` works),
  which means `jean run tool --goal x` forwards `--goal x` to the tool. Observed: the
  child's `ARGS` contained `--goal|prove argv and cwd`. A usability trap, not a
  correctness bug; worth a line in the CLI help.
- **A rejected registry is loud and terminal** — it does not fall through to
  `config/tools.example.toml`. Confirmed by breaking the demo registry on purpose.
- One record in the **demo** database, written before the goal-redaction fix, still
  contains a secret in its `goal` column. That is pre-fix behaviour preserved in scratch
  data under `%TEMP%`, not a project artifact.

### 3.5 Supplementary sweeps (not verification)

Re-run after the fixes: **0** `todo!` / `unimplemented!` / `dbg!` / `TODO` / `FIXME` in the
workspace; **0** `unwrap()` / `expect()` / `panic!` in non-test code (heuristic: the
production region ends at each file's first `#[cfg(test)]`). `config/defaults.toml` and
`config/tools.example.toml` parse against their schemas — now enforced by
`tests/config_files.rs` (11 executed tests) rather than by a script. These say nothing
about types or behaviour; the compiler and the 391 executed tests do.

---

## 4. Defects found and fixed in this batch

### 4.1 Compilation errors (all real, all found by `cargo build`)

1. `crates/jean-core/src/env.rs`, `crates/jean-process/tests/supervision.rs` — `E0753`:
   `//!` lines inside a function body, and a `///` line inside a `//!` header.
2. `crates/jean-core/src/paths.rs` — `impl Paths` never closed; a TAB had also eaten a
   character of the doc comment.
3. `crates/jean-core/src/env.rs` — two raw strings damaged by the same TAB
   (`C:<TAB>ools\bin` → `C:\tools\bin`), repaired byte-wise to preserve CRLF.
4. `crates/jean-process/src/cmdline.rs` — `'\v'`, not a valid Rust escape → `'\u{b}'`.
5. `crates/jean-process/src/output.rs` — `vec![b'old'; 5_000]` (invalid multi-codepoint
   byte literal), `E0308` on `self.error.or(error)`, and **two `E0499` double mutable
   borrows** in `write`/`write_marker`: the failure is now carried out of the
   `match &mut self.sink` instead of being recorded inside it.
6. `crates/jean-core/src/ipc.rs` — `open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE`: in
   `windows` 0.62.2 the flag newtypes derive only `Clone, Copy, Debug, Default, Eq,
   PartialEq`, **no `BitOr`/`BitOrAssign`**, so that line could never have compiled. Flags
   are combined as raw `u32` and rewrapped once.
7. `crates/jean-core/src/runner.rs` — `E0106` missing lifetime on `lookup_tool`; `E0382`
   `task_id` moved into the listener thread before the `map_err`.
8. `crates/jean-core/src/context.rs` — `load_registry` was a fake loop whose two arms both
   returned; replaced with an explicit "first candidate only" check.
9. `crates/jean-store` — `env_json` was `Option<String>` against a
   `TEXT NOT NULL DEFAULT '{}'` column (an impossible state); now `String`, and `Store`
   gained a manual `Debug` (schema version + journal mode) so tests can format it.
10. `crates/jean-protocol` / `crates/jean-process` — `E0382` partial moves in the
    `config.rs` and `spec.rs` tests; `Copy` on `EventKind`; two `never_loop` lints in
    `jean-process/src/runner.rs` and `jean-store/src/migrations.rs`; and the reserved
    `=`-prefixed environment-key check in `jean-process/src/env.rs` was unreachable
    because a general `contains('=')` check ran first — order swapped.
11. `apps/jean-cli/src/main.rs` — `std::iter::successors(e.source(), …)` does not typecheck
    (`anyhow::Error` has no inherent `source`); replaced with `error_chain()` over
    `err.chain()`. Plus a `TaskState::as_str` mapping.
12. `crates/jean-core/tests/recovery.rs` used `rusqlite` before it was a jean-core
    dev-dependency; added with a comment explaining why the pragma must be moved by hand.

### 4.2 Defects the executed tests and demonstrations exposed

13. **The pipe DACL's trustee was `CO` (Creator Owner), which cannot work.** A `CO` ACE
    never authorised the server's own later instances nor any client `CreateFileW`
    (`ACCESS_DENIED`, then `0x80070057` while the SDDL had five fields instead of six — an
    ACE is `type;flags;rights;object;inherit;trustee`). The trustee is now the current
    user's SID, read from the process token (`OpenProcessToken` →
    `GetTokenInformation(TokenUser)` → `ConvertSidToStringSidW` → `LocalFree`) and cached.
    The unit test asserts the SID shape and that `WD/AU/BA/SY/IU/BU/CO` are all absent.
14. **Liveness was answered from `OpenProcess` alone**, which succeeds for a
    terminated-but-referenced process. Added a `STILL_ACTIVE` + `GetExitCodeProcess` check
    behind `process_is_running`, so `pid_exists` and `is_same_process_alive` return false
    for a dead process. Before this, the timeout test waited 300 s for a child that had
    already exited and the tree-cancel test reported "child survived".
15. **A caller-supplied `--goal` was persisted unredacted**
    (`crates/jean-core/src/runner.rs:103` took `request.goal` verbatim while the derived
    goal was redacted). Now run through the same `Redactor`, with an end-to-end regression
    test in `crates/jean-core/tests/redaction.rs` asserting the secret is absent from
    `tasks.goal`, `tasks.request_json` and `tool_runs.args_json`, and that the key name
    survives.
16. **Test-harness defects** that were silently making tests assert nothing: PowerShell
    `-Command` does not populate `$args` (measured `count=0`), so the argv fixtures now use
    `-File`; the `cmd.exe /c ping` bystander orphaned a grandchild that printed into other
    tests, so the bystander is a direct `ping.exe` with null stdio; and ten `jean-protocol`
    config tests raced on process-global environment variables, so they now run behind a
    mutex that removes and restores the overrides.
17. **Two assertions that were wrong rather than failing:** `recovery.rs` expected four
    `StateChanged` events where the sequence records three transitions, and `state.rs`
    asserted that every state is either in-flight or terminal when `needs_attention` is
    neither — now asserted explicitly instead of by accident.
18. `logging.rs`'s invalid-filter test used `build_filter("jean_core=verbose")`… a bare word
    is a valid *target* directive, so the test asserted nothing. It now uses an actually
    invalid filter.

### 4.3 Observability added

`create_instance` logs the resolved pipe name and the exact DACL at `debug` level
(`crates/jean-core/src/ipc.rs`). That line is how section 3.4's first bullet was diagnosed
and is the first thing to read the next time cancellation looks broken. The demo
configuration sets `log_level = "debug"`; production `config/defaults.toml` is untouched.

---

## 5. Resolved: the two dependency inconsistencies

Both were open questions at the end of the previous session. Read the accepted decisions
first, then resolved. Neither was resolved by inventing work.

### DV-02 — tokio marked ADOPT but banned in `deny.toml`

**What the decisions actually say.** There is **no `D-0xx` decision mandating an async
runtime.** D-001's rationale states the opposite in terms: "Job Objects, ConPTY, UIA COM
and `SendInput` are all reachable from Rust **without a runtime**." The only place tokio is
prescribed is the dependency table itself and one row of `02-TOOLCHAIN.md` §3.1 that named
`tokio::net::windows::named_pipe` as the local-IPC implementation. D-007 does accept MCP —
and `rmcp` requires tokio.

**Resolution: defer, do not reject, and do not adopt now.** Tokio was **not** added to
satisfy an inventory entry; the inventory was corrected to match the implementation and the
accepted decisions.

- `PLANNING/03-DEPENDENCIES.md` §1: tokio's verdict is now **ADOPT — Phase 2+, not
  Phase 0**, with the reason: `rmcp` brings it in for MCP (D-007), and Phase 0 cannot use
  its named-pipe implementation because `ServerOptions` exposes no `SECURITY_ATTRIBUTES`,
  so it cannot apply the per-user DACL that cancellation requires — a claim now backed by
  measurement (section 4.2, item 13).
- `PLANNING/03-DEPENDENCIES.md` §3: the named-pipes row is **ADOPT — Phase 2+**, and a new
  row records the Phase-0 implementation as **RAW** (`Win32_System_Pipes` +
  `Win32_Security_Authorization`, `FILE_FLAG_FIRST_PIPE_INSTANCE`, SDDL
  `D:P(A;;FA;;;<user SID>)`).
- `PLANNING/02-TOOLCHAIN.md` §3.1 "Local IPC" records the same split.
- `deny.toml`: the blanket `tokio` ban is **removed** and replaced by a comment explaining
  why banning it would be a policy error. `[bans]` is not the license gate (that is
  `[licenses]`, and tokio is MIT, so D-022 does not object); a permanent ban would make
  `cargo deny check` fail the moment `rmcp` is added in an accepted later phase. Phase-0
  narrowness is enforced by the fact that nothing in `[workspace.dependencies]` references
  tokio.

### DV-01 — `regex` used but absent from the inventory

`regex` appeared in `Cargo.toml` and in `crates/jean-protocol/src/redact.rs` while being
absent from every planning document.

**Resolution: added to `PLANNING/03-DEPENDENCIES.md` §1 as ADOPT — added in
implementation**, with metadata fetched rather than guessed (crates.io: published
2026-07-15, `MIT OR Apache-2.0`, 151M downloads; MSRV 1.65 from the vendored
`Cargo.toml`), and the reason recorded: `process.redact_patterns` cannot be implemented
without a pattern engine.

### Also recorded while in `deny.toml`

`multiple-versions = "warn"` carries a comment explaining the duplicate `windows`
0.61/0.62, `windows-core` 0.61.2/0.62.2 and `thiserror` 1.0.69/2.0.21 pairs, which come
from `win32job` 2.0.3. They coexist only because win32job's API is `isize`-opaque and never
crosses a `windows` type; that invariant is stated in the boundary comment in
`crates/jean-process/src/win32.rs` (`impl Job`). Dropping win32job would remove three
duplicate crates, but `PLANNING/05-ROADMAP.md` names it as a Phase-0 deliverable, so that
has not been done unilaterally. **`cargo deny check` has still never been executed** —
see 3.1.

---

## 6. Remaining limitations and open decisions

### No code blocker remains

Two items are outside the code and need a decision, not a fix:

- **`cargo deny check` has never run** because cargo-deny is not installed and installing it
  was out of scope. The policy file exists and parses.
- **Phase 0 acceptance review.** The demonstrations in 3.3 are observed; nobody has signed
  off that they are what `PLANNING/05-ROADMAP.md` requires.

### Limitations that are design choices, not defects

- **Windows only**, by the `compile_error!` in `crates/jean-process/src/lib.rs`.
- **One task at a time.** `orchestrator.max_concurrent_tasks` is parsed and not acted on.
  Related: `jean_core::run` arms the process-wide console handler, which can only be armed
  **once per process** (`crates/jean-core/src/signals.rs`). That is correct for the CLI —
  one `jean run`, one task — and is why a library-level test cannot call `run` twice;
  `crates/jean-core/tests/redaction.rs` records the constraint.
- **Reconciliation runs on `jean run` startup and on the `jean cancel` no-listener path.**
  `jean tasks` and `jean task` are deliberately read-only: they report the runner as
  `alive`/`gone` without mutating. A crashed task therefore stays `running` in the listing
  until the next `jean run` — worth confirming that this is the intended operator
  experience.
- **Config keys parsed but not acted on in Phase 0**, all labelled as such in
  `config/defaults.toml`: `max_concurrent_tasks`, `default_retry_max`,
  `job_active_proc_limit`, `kill_grace_ms`, the whole `[terminal]` section, `[tools]` health
  checks, `[workspace]`, `policy.audit_*`, `ipc.min_client_integrity`.
- **Config values refused rather than ignored**: `process.use_job_objects = false`,
  `process.kill_on_job_close = false`, `store.migrations_on_start = false`,
  `process.inherit_environment = true`, `ipc.transport != "named_pipe"`,
  `ipc.http_enabled = true`, `policy.run_elevated_by_default = true`. Refused because the
  supervisor has no fallback behaviour; accepting `false` would make the setting look like
  it controlled something while the code did the opposite.
- **Captured stdout/stderr is not redacted.** A tool that prints a secret puts it in the
  artifact file.
- **The cancellation pipe authorises exactly one user.** A second process running as the
  same user can cancel; one running as another user cannot. `MAX_INSTANCES` is 4 but the
  listener holds one instance at a time, so simultaneous cancels are served in sequence.
- **`job_active_proc_limit` is not enforced**, so a tool that forks past the limit is not
  throttled; the value is recorded for Phase 1's policy engine.
- **The demonstration set covers the ten listed behaviors.** The refused/ignored settings
  above, the console-signal path (`Ctrl-C`, window close) and the
  `Termination::JobAssignmentFailed` branch were not driven by hand; the first two are
  covered by unit tests, and the third is not reachable with a harmless fixture.

### Phase scope held

Nothing from Phase 1 or later was built: no UIA, no screen capture, no MCP, no ACP, no LLM,
no tray icon, no voice, no elevation, no parallel coding agents, no browser automation.
`PLANNING/05-ROADMAP.md` was treated as the authoritative phase sequence. D-018 (browser
automation) remains **PROPOSED**; it is a Phase 3 concern and no Phase-0 code assumes either
outcome.

---

## 7. Setup and commands

### Prerequisites

- Rust stable `x86_64-pc-windows-msvc` (`rust-toolchain.toml` tracks `stable`; MSRV floor
  1.90 is declared in `Cargo.toml`, not pinned).
- MSVC v143 (14.44.35207) and **Windows SDK 10.0.26100.0** — both now present.
- Build inside a VS x64 developer environment (section 2.1). In Git Bash, without touching
  the machine PATH:

  ```bash
  export MSYS2_ARG_CONV_EXCL='*'
  cmd.exe /c 'target\dev.cmd build --workspace --locked'
  ```

  or from any cmd shell:

  ```powershell
  cmd.exe /c "call \"…\VC\Auxiliary\Build\vcvars64.bat\" && cargo build --workspace --locked"
  ```

  Do not copy `link.exe` anywhere; vcvars makes that unnecessary.

### Build and test

```powershell
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo test --workspace --locked
cargo deny check          # requires `cargo install cargo-deny` — not done here
```

`jean-protocol` and `jean-store` have no platform dependency beyond vendored SQLite, so if
they fail the fault is in the logic; if only `jean-process` fails, the fault is in the FFI.

### CLI usage

The registry is read from `<data_dir>\tools.toml`, or `tools.registry_path`, falling back to
`config/tools.example.toml` next to the executable.

```powershell
target\debug\jean.exe run --goal "prove the echo path works" powershell.echo
target\debug\jean.exe run windows.version
target\debug\jean.exe tasks
target\debug\jean.exe tasks --in-flight
target\debug\jean.exe task <id> --events --output --head 4096
target\debug\jean.exe cancel <id>
```

`--goal` goes **before** the tool id (section 3.4).

### Reproducing the demonstrations

`%TEMP%\jean-demo` holds the isolated demo configuration, a harmless eight-tool registry
(`demo_tools.toml`) and the fixtures under `scripts\`. Point a build at it with
`JEAN_DATA_DIR` and run, in order: D1 `run demo.workdir alpha "beta gamma"`,
D2 `run demo.streams`, D3 `run demo.flood`, D4 `run demo.tree` then `cancel <id>` from a
second shell, D5 `run demo.timeout.tree`, D8 `taskkill /F` the runner then
`run demo.streams`, and observe a bystander `ping.exe` started outside J.E.A.N. for D10. The
capture cap there is 4 KiB so D3 stays cheap; the production default is 10 MiB.

### Exit statuses (observed)

| Code | Meaning |
|---|---|
| 0 | Child completed **and** its declared verification passed (D1–D3) |
| 1 | Nonzero child exit, a timeout (D5), and J.E.A.N.'s own errors |
| 3 | Child completed, but nothing proves the goal was met |
| 4 | The task needs a human to look at it |
| 130 | Recorded as the child's exit code after a cancel (D4) |

---
---

# Phase 1 — Implementation Status

**Phase 1's code compiles, its tests execute and pass, and all four intents have been
demonstrated against the real `jean.exe` on a live Windows 11 machine.** Phase 0's behavior
and its regression tests are unchanged: every Phase-0 demonstration still holds, and the
Phase-0 test targets report the same results they did before this batch.

## 8. Four separate claims

| Claim | Status | Basis |
|---|---|---|
| **Implementation written** | **Yes.** 34,644 lines of Rust in 61 files across seven workspace members (Phase 0: 16,281 in 37). Four intents, tool health, three tiers, approvals, audit, window verification, ConPTY, bounded search, process inspection, toasts. | §9.1 |
| **Compiles** | **Yes.** `cargo check --workspace --all-targets` → `[exit] 0`; `cargo build -p jean-cli` produces `target\debug\jean.exe`, which is the binary every demonstration below ran. | §10.1 |
| **Runtime behavior verified** | **Yes, for 26 behaviors observed against the real binary** — not by trusting an exit code but by reading the process table, the artifact bytes, the SQLite record and the audit file afterwards. Two things are **not** verified by automation: a human typing an answer at a real console, and a toast actually appearing. §11 says exactly which. | §11, §13 |
| **Tests pass** | **Yes.** **559 tests executed, 559 passed, 0 failed, 1 ignored** across 18 targets. `cargo clippy --workspace --all-targets -- -D warnings` → `[exit] 0`. `cargo fmt --all -- --check` → `[exit] 0`. | §10.2 |

Not claimed: that `cargo deny check` ran (the tool is **not installed** on this machine —
the ban list was edited and is unverified by the tool); that Phase 1 has been reviewed; or
that the 559 tests cover the live-terminal path (§13).

## 9. What Phase 1 added

### 9.1 Inventory

| Member | Files | Lines | What is in it |
|---|---|---|---|
| `jean-protocol` | 9 | 4,103 | Phase-1 config keys (`[terminal]`, `[tools]` health, `[policy]` approval scope, `[search]` bounds, `[ui]` AUMID), the redactor, the nine-state machine |
| `jean-store` | 3 | 2,506 | Schema **v2**: `tool_health`, `approvals`; probe recording with the unhealthy threshold applied in SQL |
| `jean-process` | 10 | 6,254 | **ConPTY** (`conpty.rs`, 625 non-test lines), `TerminalMode::{Pipes, Pty}`, `launch_detached` for the launch intent |
| `jean-tools` | 8 | 6,399 | Application **discovery** (PATH / Start Menu / registry), `desktop.rs` window enumeration + `wait_for_window`, `process.rs` inspection, `search.rs` bounded walk |
| `jean-policy` | 8 | 1,849 | Tiers, `Operation` + `Scope` + `scope_digest`, `Prompter`/`ConsolePrompt`/`ScriptedPrompt`, `AuditWriter` JSONL |
| `jean-core` | 20 | 10,251 | `intent.rs` (the four intents, decide→act→verify→record), `health.rs`, `notify.rs`; Phase-0 `runner.rs` untouched in its semantics |
| `jean-cli` | 3 | 3,282 | `apps`, `launch`, `exec`, `ps`, `find`, `health`, `approvals`, `notify`, `status` + the Phase-0 commands |

### 9.2 The four intents, and what each one is allowed to claim

| Intent | CLI | Verification | Honest ceiling |
|---|---|---|---|
| launch an application | `jean launch <name> [--pick N]` | a top-level, visible, titled window belongs to the pid we started, within `--window-timeout` | a single-instance handoff is **`Unverifiable`**; a process that exits with no window is `NotProven`; `--no-window` downgrades the claim to "started, not proven" |
| run a registered command | `jean exec <tool-id> [args…]` | exit code **and** the registry's output needle | an interactive run has one merged stream; `console.log` proves the screen, not two separate streams |
| inspect a process | `jean ps --pid\|--name\|--self\|--list` | the pid is alive and every field this platform can report was obtained | `thread_count` is a **documented platform absence**, never counted as a refusal or a zero |
| locate a file | `jean find <root> <pattern>` | each reported path was re-opened for reading after the walk | a no-match whose root was walked to its bounds is `NotProven` (exit 3), not a failure |

Ambiguity is a refusal, not a choice: `jean launch` on a name with two candidates prints both
and exits without starting anything. Discovery never executes what it finds, and inspection
never terminates anything.

## 10. Checks actually run

### 10.1 Build and lint

| Check | Result |
|---|---|
| `cargo check --workspace --all-targets` | `[exit] 0` |
| `cargo build -p jean-cli` | `[exit] 0`, produces the binary used in §11 |
| `cargo clippy --workspace --all-targets -- -D warnings` | `[exit] 0` |
| `cargo fmt --all -- --check` | `[exit] 0` |
| `cargo deny check` | **not run** — `cargo-deny` is not installed on this machine. `deny.toml` gained a `portable-pty` ban (D-024) that the tool has therefore never enforced. |

All build commands run through `target\dev.cmd`, which enters the VS 2022 x64 developer
environment for that child process only.

### 10.2 Tests executed

`cargo test --workspace --all-targets` → `[exit] 0`, **559 passed / 0 failed / 1 ignored**.

| Target | Passed | Ignored |
|---|---|---|
| `jean-core` unit tests | 150 | 0 |
| `jean-core/tests/config_files.rs` | 12 | 0 |
| `jean-core/tests/intents.rs` | 18 | 0 |
| `jean-core/tests/notify.rs` | 17 | **1** (a real toast, which would draw on the operator's desktop) |
| `jean-core/tests/recovery.rs` | 8 | 0 |
| `jean-core/tests/redaction.rs` | 2 | 0 |
| `jean-policy/tests/audit.rs` | 8 | 0 |
| `jean-policy/tests/policy.rs` | 13 | 0 |
| `jean-process` unit tests | 113 | 0 |
| `jean-process/tests/supervision.rs` | 20 | 0 |
| `jean-protocol` unit tests | 81 | 0 |
| `jean-store` unit tests | 45 | 0 |
| `jean-tools` unit tests | 30 | 0 |
| `jean-tools/tests/apps.rs` | 13 | 0 |
| `jean-tools/tests/desktop.rs` | 18 | 0 |
| **`jean-cli/tests/cli_phase1.rs`** (new in this batch) | **11** | 0 |

The Phase-0 targets are the same ones that passed in the previous batch; nothing in this
batch removed or weakened a Phase-0 test. Four Phase-0 supervision fixtures were *replaced*,
not deleted, because they had become timing bombs (see §12.3).

## 11. Demonstrations against the real binary

Every row is a command run against `target\debug\jean.exe` in this session, with a throwaway
data directory (`JEAN_DATA_DIR` in `%TEMP%`) so nothing touched the developer's real
`%LOCALAPPDATA%\JEAN`. The exit code column is the code the process actually returned.

| # | Command | Observed | Exit |
|---|---|---|---|
| P1-1 | `jean --version` | `jean 0.1.0` | 0 |
| P1-2 | `jean --help` | 13 commands listed, each with a one-line description that says what it will not do | 0 |
| P1-3 | `jean apps notepad` | **3 candidates** — `C:\WINDOWS\System32\notepad.exe`, `C:\WINDOWS\notepad.exe`, `…\WindowsApps\notepad.exe` — `ambiguous yes`, and the footer "this command started nothing" | 0 |
| P1-4 | `jean apps charmap` | 1 found, 1 launchable | 0 |
| P1-5 | `jean launch notepad` | refused: "`notepad` matches 3 applications, and J.E.A.N. does not choose between them", list printed, **no task row created** | 1 |
| P1-6 | `jean launch notepad --pick 1` | pid 23168 created; **waited 5001 ms, 51 looks at 100 ms apart, no window**; verdict `not_proven`, "still running but opened no visible window". The window belonged to a Notepad that already existed (pid 7632) — the single-instance case, reported as it happened | 3 |
| P1-7 | `jean launch charmap --window-timeout 12` | pid 23900; **window found after 404 ms, 403 top-level windows examined**; title "Character Map"; verdict `proven` | 0 |
| P1-8 | `jean exec demo.echo hello world` | exited 0 **and** stdout contained the needle; `stdout.log` written and read back; artifact bytes shown with their unit | 0 |
| P1-9 | `jean exec demo.console` | **ConPTY**: `console.log` = 134 bytes, contents `Microsoft Windows [Version 10.0.26300.9818]`; stderr reported as "no separate stream", not "no diagnostics"; the note says keystrokes were not forwarded because `jean` itself had no console | 0 |
| P1-10 | `jean exec demo.elevated` | refused, names `jean-elevated.exe` and **D-010**; "child never started" | 5 |
| P1-11 | `jean exec demo.denied` | `format_volume` on the auto-deny list: refused **without a prompt**, "no approval can unlock it" | 5 |
| P1-12 | `echo y \| jean exec demo.approval` | denied: "standard input is not a terminal, so nobody is being asked" — a pipe cannot grant (D-011) | 5 |
| P1-13 | `jean exec demo.approval --yes` | `--yes` printed as reserved for Phase 2 and **changed nothing** | 5 |
| P1-14 | `jean ps --self` | row printed with `MEM`/`PRIV`/`CPU%`/`RUN`; `thread_count` named as a platform absence; "CPU percentages are of ONE core" | 0 |
| P1-15 | `jean ps --pid 16196` vs `Get-Process` | **unit cross-check**: `MEM 7.74 MiB` = `WorkingSet64`; the column then labelled `VIRT` = 6.54 MiB = `PrivateMemorySize64`, while `VirtualMemorySize64` for the same pid was ~2 TiB. The header was renamed `PRIV(MiB)` and a legend added (§12.2) | 0 |
| P1-16 | `jean ps --limit 3 --sort memory` | `truncated yes`, machine summary with units: `16076.87 MiB (16857817088 bytes)`, `10 physical, 16 logical` cores, uptime `67267 s`, `379 processes visible to this account` | 0 |
| P1-17 | `jean find <tree> *.txt` | 2 hits, each re-opened for reading; `reparse 1 seen and not followed` naming `tree\escape`; verdict `proven` | 0 |
| P1-18 | `jean find <tree> nearby*` | 0 hits — the file was reachable **only through the junction**, and the walk stayed inside the root; "walked to its bounds" | 3 |
| P1-19 | `jean find <tree> *.txt` with `tree\locked` denied via `icacls` | `access 1 location could not be read: …\locked: Access is denied. (os error 5)`, walk continued, still `proven`; the ACL was reset afterwards | 0 |
| P1-20 | `jean health` | 8 tools in 5 ms; `demo.absent` → `unknown` with the PATH it tried; `demo.disabled` → `disabled`, not `absent`; footer "a probe is a path lookup, not a run" | 0 |
| P1-21 | `jean status` | 10 rows, one per task, `STATE`/`INTENT/TOOL`/`CREATED`/`VERDICT`; after P1-24 the command rows read `run_command/demo.slow` | 0 |
| P1-22 | `jean task <id>` | full record: timings, verdict, tool runs with **child pid** and termination; the request rendered field by field with the environment as **names only** | 0 |
| P1-23 | `jean exec demo.slow` (4 s deadline, 300 s sleep) | `termination timeout`, child exit **124**, `duration_ms 4030`, job terminated; `Get-Process powershell` count **identical before and after** — no orphan | 1 |
| P1-24 | `jean status` column audit | `exec` rows and `run` rows were indistinguishable in SQLite; `RunRequest` now carries the intent name into `request_json` (§12.4) | — |
| P1-25 | `audit.jsonl` tail | decision line **and** outcome line per operation, each parseable as JSON, carrying actor, operation, redacted arguments, target, cwd, tier, capabilities, decision, authority, exit code, termination and duration | — |
| P1-26 | `jean notify status`, `jean approvals`, `jean approvals forget 999` | AUMID reported `registered no`, `mode none`, `delivered Disabled — nothing was attempted`; approvals ledger empty with its 86400 s window stated; forgetting a non-existent id says so and removes nothing | 0 |

Two of these are worth reading twice, because they are the ones a green test suite would
hide: **P1-6** is the real single-instance case — Notepad *was* started, a window *did*
appear, and neither belonged to the pid we launched, so the intent said `not_proven` and
returned 3. **P1-15** is the one where the number in the table was wrong about what it
measured, and the cross-check caught it.

## 12. Defects found and fixed in this batch

1. **`OwnedHandle::try_clone` did not compile** — `GetCurrentProcess` was imported from the
   wrong module and `DuplicateHandle`'s `b_inherit_handle` is a `bool`, not a `BOOL`. The
   dead `DUPLICATE_CLOSE_SOURCE` argument was removed.
2. **The `ps` table lied about a unit** — a column labelled `VIRT` carried private-page
   usage, not the virtual address space (measured: 6.54 MiB vs ~2 TiB for the same process).
   Renamed to `PRIV(MiB)`, documented on the field in `jean-tools/src/process.rs`, and a
   `columns` legend line added under the table. Pinned by a CLI test.
3. **Four Phase-0 supervision fixtures were timing bombs** — `a_timeout_kills…` reported
   "deadline was 3 s but the call took 180.69 s"; the PowerShell pipeline fixtures took
   2.5–3.3 s alone and 49.85 s / 73.13 s in isolation under parallel load, so the *child*
   reached its own deadline before the assertion did. Replaced with bulk `[Console]::Out.Write`
   and short `cmd.exe` fixtures. The cap, both-streams, timeout and console-timeout
   assertions are unchanged in what they prove.
4. **`jean exec` and `jean run` were indistinguishable in the store** — the command intent
   delegates its whole act to Phase-0's `run()`, so `request_json` was byte-identical.
   `RunRequest` gained an `intent` field; `jean run` passes `None`, and the status view now
   prints `run_command/<tool>`.
5. **A literal `{}` reached the operator** — `jean launch x --pick 0` printed
   "Say `jean launch {}` to see the list" because the message was built with `.to_owned()`
   instead of `format!`. Fixed to name the query, with a test that forbids `{}` in the text.
6. **A second intent could not be cancelled** — `signals::arm` used a `OnceLock`, so arming
   the second intent of one invocation failed and Ctrl-C could no longer stop it. Now
   `OnceLock<Mutex<Option<CancelToken>>>` with an `AtomicU32` install guard (§ D-028).
7. **A second `Context` panicked** — `tracing` allows one global subscriber, and every
   execute-path test builds one. `logging::init` is now idempotent, first configuration
   wins, and the flag rolls back if installing genuinely fails.
8. **`NOT NULL constraint failed: tasks.tool_id`** — the inspect and locate intents have no
   tool. `open_task` records the intent name in that column, which is documented rather than
   silently widened.
9. **Two test assertions were wrong, not the code** — the scripted prompter records only the
   summary (so the tool name must be asserted against `Prompt::render()`), and the audit
   lookup matched the decision line instead of the outcome line. Both fixtures were fixed
   after checking the implementation was right.

## 13. What is *not* verified, and how to verify it

| Not verified by automation | Why | How to close it |
|---|---|---|
| A human answering a prompt | Every test in this session ran with a redirected stdin, which the engine **refuses on purpose** (P1-12). `ConsolePrompt`'s read is covered only by `ScriptedPrompt` tests. | In a real terminal: `jean exec demo.approval` → the prompt prints `[y] / [a] / [n]`; answer `a`, re-run the same command (no prompt), then re-run with different arguments (prompt again — D-027). |
| A toast on the desktop | Drawing one requires writing the AUMID into `HKCU`, and this build makes that an explicit opt-in. The `Show` call is `#[ignore]`d. | `jean notify register` (writes one `DisplayName` value), set `ui.notifications = "toast"`, then `jean notify status --send "JEAN" "body"`. |
| Ctrl-C of a running `jean exec` | This harness has no console to interrupt. Tree termination itself is verified: P1-23 goes through the same job-kill path, `jean-process/tests/supervision.rs` covers the tree cases, and the cancellation pipe has its own tests. | Run `jean exec demo.slow` in a terminal, press Ctrl-C, then read `jean status` and the process table. |
| `cargo deny check` | The tool is not installed here. | `cargo install cargo-deny && cargo deny check` — the `portable-pty` ban added in this batch has never been executed. |
| Discovery's registry source on this machine | P1-3 reports the sources it actually consulted: `path, start_menu`. The registry source is implemented and unit-tested, but `notepad` matched before it was needed. | Query a name that exists only in `App Paths` / `Uninstall` and check `sources`. |
| The `HandedOff` verdict in the field | P1-6 produced `NoWindowInTime` because the launched process stayed alive; the handoff branch is unit-tested but this real instance did not exercise it. | Launch a single-instance app whose stub *exits* (e.g. a second `code` launch) and confirm `verdict unverifiable`. |
| Discovery's directory bound | P1-3 and P1-4 both printed "the search stopped at the `max_directories` bound (64 opened, 7154 entries)", so on a machine with a long `PATH` the candidate list **may be incomplete**. | Decide whether 64 is the right default, or whether the bound should scale with `PATH` length. Recorded as an open question, not a bug. |

## 14. Deviations from the planning documents, all recorded

| Deviation | Where it is written down |
|---|---|
| Raw ConPTY instead of `portable-pty` | **D-024** (supersedes the adoption half of D-016); `03-DEPENDENCIES.md` §3 and §7; `deny.toml` now bans `portable-pty` |
| Remembered approvals scoped to *operation + resolved target + validated arguments*, not `tool_and_tier` | **D-027**; `04-SETTINGS.md` `[approval]` block; `approval_scope = "tool_and_tier"` is now a **hard error at load** |
| A policy refusal is exit **5** | **D-028**; §15 below |
| The command intent keeps Phase-0's task lifecycle (starts at `Queued`, not `Planning`) rather than forking `run()` | `jean-core/src/intent.rs` module docs, and §12.4 |
| The foreground-lock trap is resolved by **not attempting it**: no `SetForegroundWindow`, `AttachThreadInput` or `ShowWindow` anywhere in the window surface | `05-ROADMAP.md` Phase 1 "Known Windows trap"; D-023 |
| A health probe never runs the tool | **D-026** |
| `jean launch` is detached: no Job Object, so a launched application outlives `jean` and `jean` cannot reap it | **D-023**, with the asymmetry stated rather than hidden |

## 15. Exit statuses (observed, Phase 1)

| Code | Meaning | Seen in |
|---|---|---|
| 0 | Ran **and** the declared verification proved it | P1-7, P1-8, P1-9, P1-14…P1-20, P1-26 |
| 1 | Failed, or the request could not be resolved at all (an ambiguous name with no `--pick`) | P1-5, P1-23 |
| 2 | Command-line usage error, from `clap` | `jean frobnicate`, `jean find --root …` |
| 3 | Completed, but nothing proves the outcome | P1-6, P1-18 |
| 4 | Needs a human | unchanged from Phase 0 |
| **5** | **Refused by policy** — nothing was attempted, no prompt was bypassed | P1-10…P1-13 |
| 130 | Cancelled | supervision tests |
| 124 | *Recorded as the child's exit code* after a timeout; the CLI still exits 1 | P1-23 |
---

# Phase 2 — model-driven planning

Written 2026-11-05. Sections 0–15 above are Phase 0 and Phase 1 and are **unchanged**, including the
outstanding manual checks in §13, which are still outstanding and are restated in §21 below.

The same discipline as before: what is *written* is not what is *verified*. Everything claimed as
observed here was executed on this machine in this session, with the output quoted or summarised.

---

## 16. Implementation written

### 16.1 Inventory

| Path | Lines | What it is |
|---|---|---|
| `crates/jean-model/` (new crate) | 2 648 | The provider-neutral client and two adapters. `lib.rs` 320, `http.rs` 583, `ollama.rs` 543, `openai.rs` 698, `secret.rs` 195, `error.rs` 135, `stub.rs` 174 (test-only). |
| `crates/jean-protocol/src/plan.rs` | 2 001 | Plan schema v1, the eight step states, structure validation, `execution_order`, `render_plan`, and 29 unit tests. |
| `crates/jean-core/src/planner.rs` | 820 | The prompt (schema + this machine's catalog + the operator's policy), the bounded replanning loop, and world validation. |
| `crates/jean-core/src/plan_runner.rs` | 1 254 | Plan-and-run, step-by-step through the existing intents, the parent task, cancellation, verdict combination, the summary, and the `jean task` plan view. |
| `crates/jean-core/tests/plans.rs` (new) | 1 785 | 25 Phase-2 scenarios driven by a deterministic fake provider. No network, no model. |
| `apps/jean-cli/src/phase2.rs` (new) | 332 | `jean plan`, `jean do`, `jean model status`, `jean model ping`. |
| `crates/jean-store` | +2 tables | Schema v3: `plans`, `plan_steps` (additive; earlier tables untouched). 15 new store tests. |
| `crates/jean-protocol/src/event.rs` | +8 kinds | `model_requested`, `model_responded`, `plan_proposed`, `plan_validated`, `plan_rejected`, `step_started`, `step_finished`, `step_skipped`, plus `EventKind::ALL` / `FromStr`. |
| `config/defaults.toml` | `[model]` | The whole Phase-2 configuration surface, with each refusal reason written next to the key. |

No third-party crate entered the workspace. `jean-model` depends on `jean-protocol`, `serde`,
`serde_json`, `thiserror`, `tracing` — all already ADOPTed. **`jean-model` cannot reach the store,
the policy engine or the process layer**, which is D-011 expressed as a dependency graph (D-029,
D-031).

### 16.2 The objective, capability by capability

Each row states what is implemented, and where it was *observed* — `[test]` names the test that
asserts it, `[binary]` means the shipped `jean.exe` was run and the output is in §18.

| Required capability | As written | Observed by |
|---|---|---|
| Provider-neutral interface | `ModelClient{generate,info,provider_name}`, synchronous, takes the caller's cancel flag. `build()` maps `none \| ollama \| openai_compatible`. | `[test]` jean-model 42 tests; `[binary]` `jean model status`, `jean model ping` |
| Ollama adapter | `POST {base}/api/chat`, JSON or NDJSON, `format:"json"` when JSON is preferred, provenance fields read when present. | `[test]` `ollama::tests` (13) against an in-process stub; live server **not** contacted (§20) |
| OpenAI-compatible adapter | `POST {base}/chat/completions`, bearer from `api_key_env`, `response_format{type:"json_object"}` only when JSON is preferred, SSE reassembly when streaming. | `[test]` `openai::tests` (17) against the same stub; live server not contacted (§20) |
| `provider = "none"` keeps Phase 1 | `planning_enabled()` is `provider != "none"`; `preview`/`run` refuse before touching a client, a task row or a child. | `[test]` `provider_none_keeps_every_deterministic_command_exactly_as_phase_one_left_it`; `[binary]` §18.4 |
| Configurable endpoint / model / timeouts / bounded sizes | `base_url`, `model`, `request_timeout_secs`, `max_response_bytes`, `max_prompt_bytes`, `max_output_tokens`, `temperature ≤ 0.3`, `stream`, `retry_backoff_ms`, `max_plan_steps`, `summary_wording`. Every one validated at load with the reason in the message. | `[test]` `config::tests`; `[binary]` `jean model status` prints all of them |
| Cancellation of model requests | The 50 ms read loop polls the same `AtomicBool` the plan uses; `ModelError::Cancelled` is never retried. | `[test]` jean-model transport tests; `[test]` `a_cancelled_run_never_asks_the_provider` |
| Credentials from a documented secret source, never logged | `api_key_env` holds the **name** of an env var; the value is read per request, is never in a record, log, event, prompt or error, and provider error bodies are scrubbed. | `[test]` `secret::tests`, `openai` credential tests, `a_secret_in_the_goal_in_an_argument_or_in_output_never_reaches_the_record`; `[binary]` `jean model status` prints only "is set / is not set" |
| Versioned typed plan | `schema_version = 1`, `goal`, ordered steps, registered tool ids, `{"type","value"}` arguments, `depends_on`, mandatory `verify`, compiled ceilings on steps/ids/titles/goal/args/patterns. | `[test]` `plan::tests` (29) |
| Whole-plan validation before any side effect | 30 diagnostic codes across two stages (§16.3); refusals write **no** plan row and **no** tool run. | `[test]` the four refusal scenarios in `plans.rs` each assert `count("plans") == 0` and `count("tool_runs") == 0` |
| Sequential execution, no workflow framework | Declaration order; `depends_on` means *stop if that did not succeed*; no value passes between steps; no parallel agents. | `[test]` `a_dependent_step_is_skipped…`, `an_independent_step_still_runs_after_a_failure…` |
| Approved steps reach the existing intents | Each step becomes exactly one `Intent` and goes through `intent::execute` — same policy engine, same audit writer, same Job Object supervisor, same verifier. | `[test]` `a_valid_plan_runs…` asserts the step's own task row carries `tool_id = "echo.tool"` and one supervised run with a child pid |
| Policy still decides; model output grants nothing | A denied step is denied at the same gate as the same intent typed at the console; the refusal note says "the plan proposed it, the rule decided it". | `[test]` `a_step_the_operator_declines_is_refused_at_the_same_gate_as_a_typed_intent`, `an_auto_denied_capability_is_refused_by_the_plan_layer_as_it_is_by_the_engine`, `output_that_claims_authority_is_evidence_and_changes_nothing` |
| Plan display, not a second consent gate | `jean plan` previews and writes nothing; `jean do` prints the plan before running it, then runs. | `[test]` `a_preview_validates_without_writing_a_single_row`; `[binary]` §18.3 |
| Lifecycle persistence with redaction | Parent task (operator's own words), plan row, one step row written **before** the act, tool-run links, verdicts, evidence — all through the `Redactor`. | `[test]` `a_valid_plan_runs…`, the redaction test; store: 15 plan/step tests |
| Interrupted plan → `needs_attention`, never replayed | A step in `running` cannot be reopened (`start_plan_step` requires `pending`); reconciliation adopts a dead parent and leaves the step row honestly `running`, and `jean task` prints the `UNCERTAIN` line. | `[test]` `a_step_left_running_is_named_as_uncertain_and_cannot_be_reopened`, `a_new_run_leaves_an_uncertain_plan_of_an_earlier_run_untouched` |
| Bounded retries for transport, none for tools | 3 proposals × up to 4 transport attempts; a tool run is never retried. | `[test]` `replanning_is_bounded…`; `[binary]` §18.2 — four attempts in 18 s, then one failed parent row |
| Bounded replanning, every replacement re-validated | The previous refusal is sent back as the instruction; the new proposal goes through both validation stages again. | `[test]` `a_malformed_answer_is_refused_and_the_model_is_asked_again_with_its_own_error` asserts the second prompt contains `attempt 1` and its code |
| Summary from records; deterministic without a model | `"succeeded: 3 of 3 step(s) proven"` built from step rows; `summary_wording = "model"` can only decorate it. | `[test]` `a_step_that_promises_more_than_the_tool_declares_is_checked_for_both`, `the_model_may_word_the_summary_but_cannot_write_it` |

### 16.3 The validation vocabulary

Structure stage (`Plan::validate_structure`, pure, no I/O): `schema_version`, `empty_goal`,
`goal_too_long`, `no_steps`, `too_many_steps`, `invalid_id`, `duplicate_id`, `invalid_title`,
`too_many_dependencies`, `self_dependency`, `unknown_dependency`, `forward_dependency`,
`dependency_cycle`, `missing_root`, `invalid_pattern`, `invalid_app_name`, `conflicting_selectors`,
`invalid_sort_key`, `missing_tool`, `invalid_argument_type`, `positional_after_positional`,
`blank_argument_name`, `duplicate_argument_name`, `too_many_arguments`, `argument_too_long`,
`argument_contains_nul`, `empty_needle`, `needle_too_long`, `verification_mismatch`,
`verification_without_a_probe`.

World stage (`jean_core::planner::validate`, needs this installation): `tool_not_registered`,
`tool_disabled`, `elevated_refused`, `capability_auto_denied`, `interactive_not_declared`,
`timeout_too_long`, `limit_too_large`, `missing_root`, `path_outside_workspace`,
`argument_rejected`, `empty_needle`.

The codes are printed to operators and sent back to the model, so they are a contract;
`the_diagnostics_are_a_stable_vocabulary` and `every_kind_reads_back_from_its_stored_name` are the
tests that keep them one.

### 16.4 Design points worth flagging for review

- **A refusal is a parse failure, not a filter.** There is no `command`, `shell`, `cwd`, `env`,
  `tier` or `path` field to put an invented capability in, so the model cannot ask for one in any
  spelling (D-031). `{"kind":"run_command","command":"notepad.exe"}` is refused with the field name.
- **`run_command` is not "any shell string".** It names a registered tool, and the tool's own
  `args` come from `tools.toml`. A registered `cmd.exe` tool can still be run — that is the
  operator's registry, not the model's invention. Argument-vector execution does **not** make an
  arbitrary script safe, and nothing here claims that it does: the vector is length-capped,
  NUL- and newline-checked, and a `path`-typed argument gets the same workspace containment question
  as a search root.
- **A model-named escape is refused outright, not prompted about.** `jean find` outside the
  workspace is a capability policy can ask about; a *plan* root outside `workspace.root` never
  reaches a prompt (D-032).
- **The stricter verification always wins.** A plan cannot weaken the tool it calls, and cannot
  strengthen its own exit by declaring less (D-034).
- **A launch step that declines its own window check is refused.** `expect_window = false` with
  `verify = window_for_pid` is a claim with no probe behind it, so the plan is rejected before it
  runs rather than reported as unverified afterwards.
- **A plan is a sequence of single tasks, so the existing single-active-task handler covers it.**
  No per-step thread, no second signal path (D-028, D-033).
- **`depends_on` is a premise, not a data flow.** Step two cannot read step one's output. Stated
  in the schema, in the preview, and in the refusal text for a forward reference.

---

## 17. Checks actually executed, with observed results

### 17.1 Toolchain

Every command below went through `target\dev.cmd`, which enters the existing VS 2022 x64 developer
environment for that child only — no machine-wide PATH edit, no installed tool, no copied linker.

| Command | Result |
|---|---|
| `cargo build --workspace --locked` | `[exit] 0`, no warnings |
| `cargo test --workspace --locked` | `[exit] 0` — **679 passed, 0 failed, 1 ignored** |
| `cargo fmt --all --check` | `[exit] 0` (no diff) |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | `[exit] 0` |
| `cargo deny check` | **Not executed — the tool is not installed on this machine and this batch may not install software.** Carried forward from §13 as still open. |

Per test binary, from the run above (last column is the Phase-1 count, for comparison):

```text
jean (bin unittests)          0        jean-model (lib)              42   (new)
cli_phase1.rs                11        jean_policy (lib)              0
jean_core (lib)             150        audit.rs                       8
config_files.rs              12        policy.rs                     13
intents.rs                   18        jean_process (lib)           113
notify.rs            17 + 1 ignored    supervision.rs                20
plans.rs                     25 (new)  jean_protocol (lib)          112
recovery.rs                   8        jean_store (lib)              63
redaction.rs                  2        jean_tools (lib)              34  (+4 this batch)
apps.rs                      13        desktop.rs                    18
```

The one ignored test is Phase 1's `notify` `Show` call, which needs a registered AUMID; it stays
ignored and stays a manual check.

### 17.2 What the Phase-2 scenarios actually cover

`crates/jean-core/tests/plans.rs` — 25 tests, all driven by a scripted `FakeModel` (no provider,
no network, deterministic). The list, grouped by the validation requirement each one answers:

| Requirement from the brief | Tests |
|---|---|
| A valid plan through the existing intents | `a_valid_plan_runs_every_step_through_the_existing_intents_and_records_each_one` |
| Step failure stops dependents, not the plan | `a_dependent_step_is_skipped_when_the_step_it_waits_for_does_not_succeed`, `an_independent_step_still_runs_after_a_failure_because_only_dependents_stop` |
| Malformed and oversized responses | `a_malformed_answer_is_refused_and_the_model_is_asked_again_with_its_own_error`, `replanning_is_bounded_and_a_truncated_answer_is_never_read_as_a_shorter_plan`, `a_plan_larger_than_the_configured_ceiling_is_refused_as_oversized`, `a_provider_that_fails_is_reported_and_its_parent_task_is_closed` (timeout **and** `ResponseTooLarge`) |
| Unknown tools, disabled/elevated tools, invalid arguments | `a_plan_naming_an_unregistered_tool_is_refused_and_writes_no_plan`, `a_disabled_tool_and_an_elevated_tool_are_both_dropped_before_the_run`, `an_argument_that_cannot_become_an_argument_vector_is_refused_with_every_other_problem`, `a_search_root_outside_the_workspace_is_refused_without_an_option_to_approve_it` |
| Denied actions the model proposed | `an_auto_denied_capability_is_refused_by_the_plan_layer_as_it_is_by_the_engine`, `a_step_the_operator_declines_is_refused_at_the_same_gate_as_a_typed_intent` |
| Tool output as prompt injection | `output_that_claims_authority_is_evidence_and_changes_nothing` |
| Timeout and cancellation | `a_provider_that_fails_is_reported_and_its_parent_task_is_closed`, `a_cancelled_run_never_asks_the_provider`, `a_cancel_between_steps_leaves_the_rest_recorded_as_cancelled` |
| No replay after uncertain execution | `a_step_left_running_is_named_as_uncertain_and_cannot_be_reopened`, `a_new_run_leaves_an_uncertain_plan_of_an_earlier_run_untouched` |
| Failed verification despite a success claim | `a_step_that_promises_more_than_the_tool_declares_is_checked_for_both`, `a_plan_cannot_weaken_a_verification_the_tool_itself_declares` |
| Redaction of credentials and sensitive fields | `a_secret_in_the_goal_in_an_argument_or_in_output_never_reaches_the_record` |
| `provider = "none"` preserves Phase 1 | `provider_none_keeps_every_deterministic_command_exactly_as_phase_one_left_it` |
| Read-only preview, records, wording | `a_preview_validates_without_writing_a_single_row`, `the_plan_view_names_the_step_whose_effect_is_unknown_and_says_nothing_for_a_plain_task`, `the_model_may_word_the_summary_but_cannot_write_it` |

### 17.3 Second pass, on the delivered tree

The same four commands were re-executed against the files as they now stand, to show the numbers
above are reproducible and not a transcript of one lucky run:

| Command | Second-pass result |
|---|---|
| `cargo build --workspace --locked` | `[exit] 0` |
| `cargo test --workspace --locked` | `[exit] 0` — **679 passed, 0 failed, 1 ignored** (identical) |
| `cargo fmt --all --check` | `[exit] 0` |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | `[exit] 0` |

Alongside the re-run, the Phase-2 sources were re-read looking for claims the code does not keep.
Three findings were checked and are recorded because they are the ones that turned out to be real,
not because the search found a fourth:

- **The prompt bound is enforced, not merely configured.** `model.max_prompt_bytes` is validated in
  `config.rs:1319` and *applied* before the socket is opened, in both adapters
  ([crates/jean-model/src/ollama.rs:98](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-model/src/ollama.rs#L98),
  [crates/jean-model/src/openai.rs:113](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-model/src/openai.rs#L113)).
  A ceiling that is only range-checked would have been a documented limit nobody honoured.
- **The five `shorten` helpers slice by `char`, not by byte** (`chars().take(n)`), and the redactor
  slices at regex match offsets, which are always char boundaries. A byte-index slice would panic
  on the non-ASCII goals and secrets this project actually handles, so the truncation path is the
  likeliest place for a crash a provider could trigger; it is not one.
- **`jean run echo` refuses** with the registry's own list of known ids rather than guessing, which
  is the same boundary the plan layer enforces for a model: a name that is not in the registry has
  no executable, whatever layer the request came from.

The request to find these went out as a delegated adversarial review with a fixed spec, and that
subagent reached its own turn limit before reporting. The three points above are this pass's own
reading of the code, not its conclusions; nothing it produced was adopted, and no finding from it
is relied on in this document.

---

## 18. Demonstrations against the real binary

`target\debug\jean.exe`, a scratch data dir at `target\p2demo` holding `tools.toml`, `jean.toml` and
one file to find. `JEAN_DATA_DIR` pointed there for every command; nothing outside the project tree
was touched, no service was installed or started, and no credential was created.

### 18.1 `jean model status` — a provider configured, and a provider absent

```text
$ jean model status                       # [model] provider = "ollama", base_url = http://127.0.0.1:11434
provider    ollama
planning    enabled
endpoint    http://127.0.0.1:11434
model       qwen2.5:3b
timeout_secs 120
response ceiling 1048576 bytes
prompt ceiling 262144 bytes
max output tokens 2048
temperature 0
stream      false
transport retries 3 backoff step(s): [500, 2000, 8000]
max plan steps 8
summary wording record
never delegated permission_checks, argument_validation, path_handling, process_supervision, task_state_transitions, verification, policy_decisions, secret_handling
credential  environment variable "JEAN_MODEL_API_KEY" is not set
`transport   plain HTTP/1.1 over TCP. HTTPS is refused at load: this build ships no TLS client, so point the endpoint at a loopback proxy that terminates TLS.
supported subset ollama POST /api/chat and openai_compatible POST {base}/chat/completions. No tool-calling API, no redirects, no chunked transfer encoding, no function schemas.
[exit 0]
```

With `JEAN_MODEL_PROVIDER=none` the same command prints
`provider none` / `planning disabled — every deterministic command still works` and the rest of the
block unchanged. No value of any variable is printed in either case — the credential line names the
variable and says whether it is set.

> **Superseded below by §25.** The last two lines quoted above (`transport … HTTPS is refused at
> load` and `no chunked transfer encoding`) were true of that build and are false of the current
> one; §25.3 quotes the binary again, after the transport correction. Everything else in this
> section still reproduces.

### 18.2 `jean do` against a provider nothing is listening on

```text
$ jean do "echo the word ran using the echo tool"
provider    ollama
endpoint    http://127.0.0.1:11434
model       qwen2.5:3b
credential  not used by this provider
prompt ceiling 262144 bytes
jean: the model provider failed: could not connect to http://127.0.0.1:11434/api/chat: No connection could be made because the target machine actively refused it. (os error 10061)
[exit 1]
```

The record it left, read back with `jean task <id> --events` (this is the Phase-2 lifecycle, in the
shipped binary, not in a test):

```text
task        t-18e514532f17af90-7a98-0
goal        echo the word ran using the echo tool
state       failed
tool        plan
created     2026-11-05 07:06:14.082Z
finished    2026-11-05 07:06:32.789Z          ← 18.7 s: four bounded transport attempts, then stopped
request     intent = plan / provider = ollama / model = qwen2.5:3b / max_plan_steps = 8 /
            request_timeout_secs = 120 / workspace_root = C:\…\target\p2demo
no tool runs were recorded
events      task_created        created in state queued
            state_changed       queued -> planning: a plan was requested from the configured provider
            model_requested     provider ollama model "qwen2.5:3b"; asking for a plan of at most 8 steps within 262144 bytes
            state_changed       planning -> failed: the model provider failed: … (os error 10061)
            supervisor_error    the model provider failed: …
```

The event names round-trip from the database as themselves (`model_requested`, `supervisor_error`) —
that was one of the defects fixed in this batch (§19, P2-3). `runner none recorded` on the closed row,
so a later `jean` run does not inherit a dead owner.

### 18.3 `jean plan` writes nothing

```text
$ jean plan "echo the word ran"
jean: the model provider failed: could not connect to http://127.0.0.1:11434/api/chat: … (os error 10061)
[exit 1]

$ jean status | grep "task(s)"
3 task(s)          ← identical to the count before the command
```

With `provider = "none"`:

```text
$ jean plan "open notepad and tell me what is running"
jean: the model provider failed: no model provider is configured (model.provider = "none"); the
deterministic commands still work, and `jean model status` says what to set
[exit 1]
```

### 18.4 Phase 1 still behaves exactly as before

Run against the same directory with `JEAN_MODEL_PROVIDER=none`:

| Command | Observed |
|---|---|
| `jean exec echo.tool --goal "phase one still runs"` | `intent run_command`, `decision allowed (owner command)`, `termination exited`, `exit code 0`, `state succeeded`, `verdict proven`, `[exit 0]` |
| `jean find <root> note.txt` | `hits 1 reported`, `stop complete — the authorised root was walked to its bounds`, `verdict proven`, `[exit 0]` |
| `jean status` | Both rows as before, `INTENT/TOOL` = `run_command/echo.tool` and `locate_file`; the new plan row appears only for `jean do` |
| `jean task <deterministic id>` | **No plan section at all** — the output is byte-for-byte the Phase-1 record. The plan view appears only for a task that has plan rows. |
| `jean ps --pid <pid>` | 11 `cli_phase1` tests pass unchanged; see §19 P2-8 for the one that was silently timing-dependent |

---

## 19. Defects found and fixed in this batch

Every one was found by a test or by reading the record afterwards, not by review alone.

| # | Defect | How it was found | Fix |
|---|---|---|---|
| P2-1 | **Successful step outcomes were never collected.** The loop ran the steps, wrote the rows, and pushed nothing into the report, so every multi-step plan reported `0 of 0 step(s)` and a parent state of `failed`. | `a_valid_plan_runs_every_step…` — the `PlanReport` debug dump in the assertion failure | `outcomes.push(outcome)` on the normal path ([`crates/jean-core/src/plan_runner.rs:613`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/plan_runner.rs#L613)) |
| P2-2 | **A failed step cancelled the rest of the plan**, and because the parent's state check looked for a cancelled outcome first, a plan whose *first* step failed was reported as `cancelled` (exit 130) rather than `failed` (exit 1). | `a_dependent_step_is_skipped…` expected `failed`, got the cascade; `an_independent_step…` showed the same | A failure stops only its dependents (`unmet_dependency` → `skipped`); only a cancel or an uncertain step halts the run; an uncertain stopper's later rows are `skipped`, not `cancelled` (D-033) |
| P2-3 | **Every Phase-2 event read back as a generic `note`.** `jean-store` kept its own hand-written name→kind table, which listed the Phase-0 and Phase-1 names only, so `plan_rejected`, `step_started`, `model_requested` and the rest were invisible in `jean task --events`. | `a_plan_naming_an_unregistered_tool…` asserting the `PlanRejected` event existed | One table owned by the enum: `EventKind::ALL` + `EventKind::from_name` + `impl FromStr`, store delegates; `every_kind_reads_back_from_its_stored_name` now fails the build if a kind is added without its name |
| P2-4 | **The `plan_proposed` audit line leaked a secret in the clear.** It quoted the model's restatement of the operator's goal verbatim, so `password=hunter2-GOALSECRET-4242` landed in the `events` table. | `a_secret_in_the_goal_in_an_argument_or_in_output_never_reaches_the_record` scanning every event detail | Redact, then shorten (shortening first can cut a pattern in half and leave a fragment that matches nothing) ([`crates/jean-core/src/plan_runner.rs:367`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/plan_runner.rs#L367)) |
| P2-5 | **A half-open code fence parsed as a complete plan.** `strip_json_fence` removed the opening fence and left the body; a truncated response whose visible prefix happened to be valid JSON would have been *executed*. | `a_fenced_response_is_unwrapped_and_prose_around_the_object_is_not` | An unclosed fence is now `invalid_json` with the reason "a plan inside a truncated response is not a plan" |
| P2-6 | **`forward_dependency` and the cycle search were not wired into `validate_structure`.** `execution_order` refused a forward reference, and `find_cycle` existed, but structure validation never called either — so a plan whose dependency came later would have reached the executor. | `a_dependency_on_a_later_step_is_reported…`, `a_dependency_cycle_is_reported_with_the_path_that_closes_it` | Positions are mapped before the loop; `unknown_dependency` no longer swallows a forward reference; both codes are emitted by `validate_structure`, which `jean-core` runs before any write. The refusal wording is shared by one helper so the preview and the executor cannot disagree |
| P2-7 | **`needle_too_long` could not fire.** The check compared against the *argument* ceiling (1 024) while the test fixture used the *pattern* ceiling (256), so an oversized needle was accepted. | `a_blank_needle_proves_nothing_and_an_oversized_one_is_not_a_needle` | A needle is bounded as a probe (`MAX_PATTERN_LEN`), with the reason in the message: "something that long is a payload, not a check" |
| P2-8 | **A process younger than one second was reported as refused by the OS.** `run_time_secs == 0` was classified as "not obtained", so `jean ps --pid <a young pid>` — and any plan step inspecting this process — came back `unverifiable` with a statement that blamed the OS for the sampler's own one-second granularity. Phase 1 had the same bug; its test only passed because the harness happened to be older than a second when it ran. | First seen as a flake in `cli_phase1::ps_describes…` (2 of 3 failing), then deterministically in `intents::inspecting_this_process_proves_the_pid_is_alive` and in the Phase-2 probe step | A zero age with a known `start_time` is a measurement, not a refusal; the field is only unavailable when the start time itself is missing. Four new unit tests in [`crates/jean-tools/src/process.rs:706`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-tools/src/process.rs#L706) pin all three cases. **No test was weakened**: the two sleeps this looked like needing were removed once the cause was fixed |
| P2-9 | **Two diagnostic paths disagreed about a declined window check.** `jean-core` emitted `verification_without_a_probe` for a launch that switched off its own window check, and `jean-protocol` emitted nothing — while the Phase-2 test asserted a *note*. The plan-level refusal was the right call; the duplicate was dead code. | `a_step_that_declines_its_only_probe_is_refused_before_it_runs` | Structure validation owns the code (a step with no probe cannot be honestly verified by any route); the planner's duplicate and its unreachable branch were removed |
| P2-10 | **The store's `supersede_plan` used the wrong column name** (`plan_id` for `id`), so replacing a plan would have failed at run time. | `a_replacement_plan_supersedes_the_first_without_erasing_it` | Fixed in the statement; the test now proves the old row survives with `superseded_at_ms` set |
| P2-11 | **Windows reset the model socket mid-response** (`os error 10054`), destroying an answer that had already arrived, because the client half-closed its write side and the stub dropped the socket with unread bytes. | jean-model transport tests, intermittently | The client does not `shutdown(Write)` — `Content-Length` says the request is complete — and the stub FINs then waits for the client's close. Documented in the module, because the fix looks wrong to a reader who has not met this |
| P2-12 | **Four credential tests shared one environment variable** and raced each other under `cargo test`'s threads. | Intermittent credential-test failures | One variable name per test |
| P2-13 | **A config fixture directory could collide across test threads**, so an unrelated `jean-protocol` config test failed with a TOML parse error on a file that was correct in the source. | `array_override_replaces_rather_than_concatenates` failing in a full-suite run and passing alone | `tempdir()` now mixes a process-wide counter into the clock nonce |

Clippy fixes in this batch (all `-D warnings`, so they were build failures): `should_implement_trait`
on `EventKind::from_str` → a real `FromStr`; `unneeded_concat_str` on four SQL constants;
`map_err` → `inspect_err`; two `usize as usize` casts left by a configuration type change;
`needless_borrow`; a literal-bool `assert_eq!` in a new test.

---

## 20. Live-provider compatibility: **not verified**

Stated plainly, because the tests do not cover it and nothing here should imply otherwise.

- **Nothing was installed, started, downloaded or authorised for this batch.** No Ollama, no
  `llama-server`, no model weights, no account, no paid API. The brief required exactly that.
- **No provider was listening.** Probed read-only: `curl -m 2 http://127.0.0.1:11434/api/version`
  → connection refused (exit 7); `netstat -ano -p tcp` shows no listener on 11434, 1234, 5000 or
  8000; no `Ollama` install directory and no `ollama` on `PATH`.
- **What *was* exercised:** the real HTTP/1.1 client, from the shipped binary, against a loopback
  address with nothing behind it (§18.2) — connect, four bounded retries with `[500, 2000, 8000]`
  backoff, one honest failure, one closed record. That is the transport and the retry bound, not the
  provider's answer.
- **What the adapters were tested against:** an in-process stub server bound to `127.0.0.1:0` inside
  `cargo test` (jean-model: 42 tests), with fixtures written from the documented response shapes of
  Ollama `/api/chat` (JSON and NDJSON) and an OpenAI-compatible `/chat/completions` (JSON and SSE).
  A stub proves the parser, not the vendor.

To close it, in a terminal, on a machine that already has one of these:

```text
1.  ollama serve                                   # or: llama-server --port 8080 --model …
2.  jean model status                              # confirm the endpoint and model name
3.  jean model ping                                # one real request; prints provider, model, ms
4.  jean plan "find the note under the workspace and read this process"
5.  jean do "echo the word ran using the echo tool"
6.  jean task <id>          # the plan, the steps, the verdicts, the evidence
```

Expected deltas if a provider is used: Ollama's `eval_count`/`prompt_eval_count` become the token
figures; a non-loopback OpenAI-compatible endpoint requires `api_key_env` to name a variable that is
set; a provider that returns `finish_reason = "length"` will be reported as truncated and re-asked
once, up to three proposals; a provider that emits a `response_format` the deployment rejects
(HTTP 400) will report that error verbatim and **not** retry it. None of those paths was observed
against a live model in this batch.

---

## 21. Remaining limitations and open decisions (Phase 2), and what is still owed from Phase 0/1

### 21.1 Still owed from Phase 0 and Phase 1 — unchanged, still manual

The seven rows of §13 remain open and are restated here so nothing is lost between batches:

| Outstanding manual check | Where it is written |
|---|---|
| A human answering a prompt (`[y]` / `[a]` / `[n]`, then a remembered approval expiring on a changed argument) | §13, D-027 |
| A toast actually drawn on the desktop (`jean notify register`, `ui.notifications = "toast"`) | §13 |
| Ctrl-C of a running `jean exec` in a real terminal | §13 |
| `cargo deny check` — the tool is still not installed, so the `portable-pty` ban and the Phase-2 dependency set have never been executed | §13, D-022 |
| Discovery's registry source on this machine (P1-3 reported `path, start_menu` only) | §13 |
| The `HandedOff` launch verdict in the field (this instance produced `NoWindowInTime`) | §13, D-023 |
| Whether `max_directories = 64` is the right discovery bound | §13 — an open question, not a bug |

Phase 2 adds one to that list and closes none of them.

### 21.2 Phase-2 limitations

| Limitation | Statement | What would change it |
|---|---|---|
| ~~No TLS~~ **closed by the correction batch** | D-029 is superseded by **D-038**: the transport is `ureq` with the Windows trust store. `https` is accepted and verified, chunked responses are decoded, and redirects are still refused — now by `max_redirects(0)` in the client rather than by a hand-written parser. Plain `http` to a **non-loopback** host became a load error, because the prompt and any bearer token would cross a network unencrypted. | Remaining: verification has never met a real certificate chain (see §20, still open) |
| Live-provider compatibility unverified | §20. The adapters were tested against stubs built from documentation, not against the vendors. | Six commands, listed in §20 |
| Sequential plans only | No parallel steps, no value passing between steps, no loops, no conditional branches, no sub-plans. A plan that needs them is refused as too many steps or as an operation it cannot name. | A scheduler — which would first need a per-step cancellation scope; D-028 gives one active task per process |
| No retries for acts | A lost tool response is reported uncertain, never replayed; an interrupted plan waits for a person. | Nothing should: this is the intended behaviour, listed so it is not mistaken for a gap |
| Redaction is pattern-based | `password=`, `token=`, `api_key=`, `sk-…` and the configured additions. A secret in an unrecognised shape can still be stored in the clear, and captured output is captured before it is redacted for the record. | Narrower detection is a configuration change (`core.redact_patterns`); structural guarantees would need field-level classification of what counts as a secret |
| ~~Authority is per step~~ **closed by the correction batch** | **D-039** supersedes the per-step reading: a person typing `jean do` authorises *the run*, bound to the plan validated from their goal, to each step's resolved scope digest, expiring when the run returns and never written to the approval ledger. `auto_deny` and the elevation boundary are still decided before any grant is consulted, and `--prompt-each` restores the per-step gate on request. `--yes` remains a no-op: authority comes from the operator, not a flag. | A step the operator was not shown is not covered; a plan resolved later is not covered |
| Model output cannot be *proven* harmless | Deterministic validation can prove a plan is inside the vocabulary this build can execute. It cannot prove that any sentence typed by an operator authorises any command a model generated; when the target or the action is materially ambiguous, the honest answer is to ask. | Nothing — this is the boundary, not a TODO |
| `jean do` has no plan-approval gate | The plan is displayed before it runs, and then it runs. A review-before-execute gate is a Phase-3+ product decision (D-033), not a missing check | An explicit `--review` that stops after `render_preview`, if wanted |
| Ambiguous launch targets are refused at the same gate as an ambiguous typed intent | A plan naming `code` where two matches exist exits 1 rather than guessing | `--pick` is expressible in a plan (`pick`), so a model can choose deliberately — but only when the operator's goal said which |
| Two more `windows` features were **not** needed | Phase 2 added no Windows API surface; the crate list is unchanged from Phase 1 | — |
| One process, one plan at a time | Two `jean do` invocations share the store but not a signal handler; the second's Ctrl-C cannot cancel the first | Concurrency in the supervisor, which D-028 currently forbids |

---

## 22. Exit statuses (observed, Phase 2)

| Code | Meaning | Seen in |
|---|---|---|
| 0 | Every step ran and every declared verification was found | §18.4 (Phase-1 commands unchanged); `a_valid_plan_runs…` |
| 1 | A step failed, or the plan never became executable (provider failure, all proposals refused) | §18.2, §18.3 — both observed on the binary |
| 2 | Command-line usage error from `clap` | `jean find --root …` (§18.4, and §15) |
| 3 | Steps ran but the outcome is not proven | `a_step_that_promises_more…` is `1` (failed); `3` is reached when a step ends `unverified` — e.g. a probe whose fields were genuinely not obtainable |
| 4 | A step's effect cannot be determined and needs a person | `a_step_left_running_is_named_as_uncertain…` (reconciled), `a_new_run_leaves_an_uncertain_plan…` |
| 5 | A step was refused by policy — nothing was attempted | `a_step_the_operator_declines…`, `output_that_claims_authority…` |
| 130 | Cancelled | `a_cancel_between_steps…`, `a_cancelled_run_never_asks_the_provider` |

`PlanReport::exit_status()` is the single place the ladder is decided for a plan, and it is derived
from the recorded step states — not from anything the model said about them.

---

## 23. Deviations from the planning documents, all recorded

| Deviation | Where it is written down |
|---|---|
| Model config is one flat `[model]` table, not `[model.ollama]`/`[model.openai_compatible]`/`[model.anthropic]`/`[model.openai]` sub-tables; `anthropic` and `openai` are refused at load | **D-030**; `04-SETTINGS.md` `[model]` block rewritten to the shipped shape, plus its environment-variable and defaults tables |
| `require_provider_for`, `deterministic_fallback` and `start_if_missing` are not configuration keys | **D-030**; stated in the settings block — `provider = "none"` *is* the fallback, and this build never launches a model server |
| ~~No HTTP crate and no TLS~~ | **D-029 superseded by D-038.** The transport is `ureq` 3.5.1 with `default-features = false, features = ["native-tls"]`; `03-DEPENDENCIES.md` now records the ADOPT verdict, and `reqwest` stays rejected because it would put tokio in the tree |
| The `ModelClient` output is a plan only, not "a plan or a tool request" | **D-031**; `05-ROADMAP.md` Phase 2 deliverable annotated. A single tool call is already `jean exec` (Phase 1) |
| Streaming is implemented but off by default | **D-029**, now **D-038**; roadmap row annotated. Streaming had always been written in both adapters and could never arrive, because an SSE or NDJSON answer is chunked and the old client refused chunked. Chunked decoding is what made `model.stream = true` reachable; the shipped default stays `false` |
| Plan preview is a display, not an approval gate | **D-033**; §21.2 |
| A plan's `depends_on` does not schedule and does not carry data | **D-031**, D-033; stated in the schema module docs and in the preview footer |
| Phase-2 exit criteria proved at the library seam with a fake provider rather than against a live one | §20; `05-ROADMAP.md` Phase 2 exit-criteria note |

Nothing in `01-SCOPE.md` changed: no Telegram, no desktop capture, no UI Automation actuation, no
voice, no ACP, no MCP, no tray UI, and Phase 3 was not started.

---

## 24. How to reproduce the Phase-2 checks

```text
cd "C:\Users\4Raisan\Desktop\MIII\Personal Projects\J.E.A.N. - The AI Companion"
export MSYS2_ARG_CONV_EXCL='*'

# build / test / lint
cmd.exe /c 'target\dev.cmd build  --workspace --locked 2>&1'
cmd.exe /c 'target\dev.cmd test   --workspace --locked 2>&1'
cmd.exe /c 'target\dev.cmd fmt    --all --check      2>&1'
cmd.exe /c 'target\dev.cmd clippy --workspace --all-targets --locked -- -D warnings 2>&1'
# Each prints its own "[exit] N". Read that line; do not pipe the output through tail.

# the Phase-2 scenarios alone
cmd.exe /c 'target\dev.cmd test -p jean-core --test plans 2>&1'
cmd.exe /c 'target\dev.cmd test -p jean-model 2>&1'

# the CLI demonstrations (§18), with a scratch data directory inside the project
export JEAN_DATA_DIR='C:\Users\4Raisan\Desktop\MIII\Personal Projects\J.E.A.N. - The AI Companion\target\p2demo'
./target/debug/jean.exe model status
./target/debug/jean.exe model ping                 # exit 1: nothing is listening on 127.0.0.1:11434
./target/debug/jean.exe plan  "echo the word ran"  # writes nothing
./target/debug/jean.exe do    "echo the word ran using the echo tool"
./target/debug/jean.exe status
./target/debug/jean.exe task  <id> --events
JEAN_MODEL_PROVIDER=none ./target/debug/jean.exe do "echo once"   # refuses; every deterministic command still works
```

`target/p2demo` is scratch data from this session and may be deleted; it holds one `tools.toml`,
one `jean.toml`, one file to find, and a database with three rows.

---

# Phase 2 — corrections: a maintained transport, and the operator's authority for a run

The batch recorded in §16–24 shipped two things the operator rejected: a hand-written HTTP client
with no TLS and no chunked support, and a plan that asked permission again at every step. Both are
now implemented, so this part replaces §17/§18/§21 on those two points. §16–24 stay as the record of
what was built before; the rows that became false are struck through there and point here.

## 25. What changed

### 25.1 The model transport is `ureq`, not `std::net`

| File | Change |
|---|---|
| `Cargo.toml` | New workspace dependency `ureq = { version = "3.5.1", default-features = false, features = ["native-tls"] }` |
| `crates/jean-model/Cargo.toml` | `ureq.workspace = true` |
| `crates/jean-model/src/http.rs` | Rewritten. `Target`, `OutgoingRequest`, `RawResponse` and `send()` keep their shape, so neither adapter changed; the `TcpStream`, the request writer, the header-line reader and the body reader are gone. 680 lines now, 391 of them outside `#[cfg(test)]`, with the transport itself in `agent_config` + `send` + `exchange` + `describe` |
| `crates/jean-model/src/stub.rs` | Gained `chunked_body()`, `spawn_stalling()` and `authority()`; the request-read bound dropped from 5 s to 1 s so a TLS test measures TLS rather than a race |
| `crates/jean-protocol/src/config.rs` | `https` is no longer a load error. **A non-loopback `http` endpoint now is** |
| `crates/jean-model/src/lib.rs` | The supported-subset table and the cancellation section rewritten to what the code does |
| `apps/jean-cli/src/phase2.rs` | The `transport` and `supported subset` lines an operator reads |

Each requirement, and where it is now enforced:

| Requirement | Enforcement |
|---|---|
| Verified HTTPS | `TlsProvider::NativeTls` (Schannel) with `RootCerts::PlatformVerifier`. Nothing in this workspace calls `disable_verification`, and §27 pins that assertion rather than trusting the prose |
| Chunked responses | Decoded by the client. `model.stream = true` became reachable: an SSE or NDJSON answer is chunked, so the reassembler in both adapters had never received one |
| Bounded streaming | A declared `Content-Length` above `model.max_response_bytes` is refused before reading; the body is read through `Read::take(max + 1)`, which is the bound that holds a chunked answer |
| Deadlines | `timeout_global` for the whole exchange; `timeout_connect` and `timeout_resolve` bound the two phases a shutdown cannot interrupt |
| Cancellation | The exchange runs on a worker thread; the watching thread polls the operator's flag and calls `Agent::shutdown()`, which closes the pooled sockets so a parked read returns at once instead of waiting out the deadline |
| Redirects off by default | `max_redirects(0)`, and `ureq` never re-attaches `Authorization` across a redirect. A 3xx is refused with the address it declined to visit |
| Redaction | Unchanged and still ours: the bearer header is built per attempt from the named variable, is never in a URL, and provider error bodies are scrubbed before becoming `ModelError` |

**Why `ureq` and not `reqwest`.** The supervisor is synchronous by decision (DV-02) and `jean-process`
owns the threads; `reqwest` would add tokio, hyper and h2 to the tree to make one blocking call.
`native-tls` on Windows is Schannel, so no C-built crypto (`ring`, `aws-lc`) and no vendored OpenSSL.
Verified from the resolved lockfile: **no `tokio`, `hyper`, `h2`, `rustls` or `ring` entered the
tree**; 156 packages total, the additions being `ureq`, `native-tls`, `schannel`, `webpki-roots` and
`rustls-pki-types` (a types crate `ureq` uses for its TLS surface even with the native provider).

### 25.2 Authority the operator gives to a run, not to a step

| File | Change |
|---|---|
| `crates/jean-policy/src/grant.rs` | New. `RunAuthority`, `RunGrant`, `GrantScope` |
| `crates/jean-policy/src/basis.rs` | New `Authority::RunScope`, audited as `run_scope` |
| `crates/jean-policy/src/lib.rs` | `Policy::decide_granted(...)`; `decide()` is now a one-line wrapper, so every Phase-1 caller and test is untouched |
| `crates/jean-core/src/intent.rs` | `execute_granted(...)`, `Enforcement.grant`, `Enforcement::would_prompt` (replacing the free `will_prompt`), and `operation_for(...)` so a granted digest and an executed digest come from the same computation |
| `crates/jean-core/src/plan_runner.rs` | `run(..., authority)`, an internal `RunMandate` that mints the grant from the **resolved** plan, and `run_authority`/`covered_steps` on `PlanReport` |
| `apps/jean-cli/src/main.rs`, `phase2.rs` | `jean do --prompt-each`; the `authority` line on `jean do` and in its report |

The consult order inside `decide_granted` is the whole safety argument:

| | |
|---|---|
| 1 | `auto_deny` — refused, no prompt, **before the grant exists** |
| 2 | Elevation — refused because this process holds no administrator token (D-010), **before the grant** |
| 3 | No capability on `require_approval` → `owner_command`, exactly as Phase 1 |
| 4 | **The run grant** — only if plan id, step key *and* resolved scope digest all match |
| 5 | A remembered approval for that digest |
| 6 | A person is asked |

A grant covers a step only when the step is one the operator was shown, at the scope it resolves to.
It is a value owned by `run()` and dropped when `run()` returns — that is the expiry, and it is a
lifetime rather than a timer. It is never written to the `approvals` ledger, so a later process has
nothing to match. It has no method that adds an entry, so nothing a model said or a step printed can
widen it. `--prompt-each` mints nothing, which is how the Phase-1 gate remains reachable on request.

### 25.3 The shipped binary, after both corrections

`target\debug\jean.exe`, scratch data directories inside the project only, nothing started outside
them and no credential created.

```text
$ jean model status | grep -E "transport|supported subset|credential"
transport   HTTP/1.1 over TCP, via ureq. https is verified against the Windows system trust store and
            cannot be configured to skip a certificate; plain http is accepted only for a loopback
            endpoint.
supported subset ollama POST /api/chat and openai_compatible POST {base}/chat/completions, whole or
            chunked, including NDJSON and SSE streams. No tool-calling API, no redirects (none are
            followed), no function schemas.
credential  environment variable "JEAN_MODEL_API_KEY" is not set
[exit 0]
```

An `https` endpoint is now dialed for real. Against a throwaway plaintext listener on loopback —
started for two seconds inside this project and stopped, not installed — the failure is the TLS
layer's, which is the evidence that verification is being attempted rather than skipped:

```text
$ jean do "say hello"          # [model] base_url = "https://127.0.0.1:18099"
provider    ollama
endpoint    https://127.0.0.1:18099
model       fixture
credential  not used by this provider
prompt ceiling 262144 bytes
authority   your instruction, for this run and the plan it validated
jean: the model provider failed: the provider's response could not be read as HTTP/1.1:
      https://127.0.0.1:18099/api/chat: native-tls: The token supplied to the function is invalid
      (os error -2146893048)
[exit 1]
```

A remote endpoint over plain `http` is refused before anything is sent (C-1):

```text
$ jean do "say hello"          # [model] base_url = "http://api.example.invalid/v1"
jean: configuration error: invalid configuration value for `model`: model.base_url
  "http://api.example.invalid/v1" is a non-loopback endpoint over plain http. Since this build
  verifies TLS, use https for anything that is not on this machine: a remote http endpoint would
  carry the prompt, and any configured bearer token, unencrypted
[exit 1]
```

`jean do` states its authority before it plans, and the report says which authority each step ran
under — `run_scope` in `jean audit` for a covered step, `approved_this_time` for one asked about:

```text
$ jean do --prompt-each "..."      # the operator opting back into the Phase-1 gate
authority   each step passes the approval gate on its own (--prompt-each)
$ jean do --not-a-flag x
[exit 2]                            # clap still rejects an invented flag; --prompt-each is real
```

## 26. Checks executed on the current tree, with observed results

Through `target\dev.cmd` (the existing VS 2022 x64 developer environment, per child), `--locked`
after the lockfile update:

| Command | Result |
|---|---|
| `cargo check --workspace --all-targets` | `[exit] 0` |
| `cargo build --workspace --locked` | `[exit] 0` |
| `cargo test --workspace --locked` | `[exit] 0` — **698 passed, 0 failed, 1 ignored** |
| `cargo fmt --all --check` | `[exit] 0` |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | `[exit] 0` |
| `cargo deny check` | **Not executed — still not installed, and this batch may not install tools.** It has never been run against any dependency set here, including `ureq` now |

The totals moved 679 → 698: `jean-model` 42 → 48, `jean-core/tests/plans.rs` 25 → 29,
`jean-policy/tests/grant.rs` 0 → 6 (new binary), `jean-cli/tests/cli_phase2.rs` 0 → 3 (new binary).
No test was deleted, no assertion was loosened, and the one ignored test is still Phase 1's `notify`
`Show` call. Nothing in the suite contacts a network except an in-process loopback stub.

## 27. New tests, and what each one pins

`crates/jean-model/src/http.rs` — 7:

| Test | Pins |
|---|---|
| `certificate_verification_is_never_switched_off_and_redirects_stay_unfollowed` | `disable_verification() == false`, the provider is the one compiled in, roots are the platform's, `max_redirects == 0`, and the configured deadline is applied to global **and** to connect/resolve |
| `an_https_endpoint_reaches_tls_instead_of_being_refused_at_load` | https is *attempted*: the failure names the TLS layer instead of a missing feature |
| `a_chunked_answer_is_decoded_and_the_ceiling_still_binds_it` | an oversized chunked body is refused (no `Content-Length` to warn first), and a chunked body that fits arrives whole |
| `a_stalled_answer_is_cancelled_mid_flight` | a provider that accepts the request and never answers is interrupted in well under the 30 s deadline |
| `a_redirect_is_refused_because_its_destination_was_never_configured` | the refusal, **and that the stub was contacted exactly once** — the client did not follow the `Location` |
| `endpoints_are_parsed_strictly` | scheme-appropriate default ports, and that a prefix is joined exactly once |
| `the_request_headers_reach_the_server_unchanged` | the bearer header and the body go out as built, with this build's own `user-agent` |

`crates/jean-model/src/openai.rs` — 2: `an_https_endpoint_is_built_and_dialed_rather_than_refused_at_load`
(replacing the test that asserted the refusal), and `a_chunked_sse_answer_is_reassembled_by_the_client_that_receives_it`,
which is the end-to-end proof that a streamed answer can now arrive at all.

`crates/jean-policy/tests/grant.rs` — 6: the grant answers its own step and nothing else (three
negative cases: unminted step, different plan, **drifted scope**); it cannot reach an `auto_deny`
capability; it cannot reach the elevation boundary; it leaves the ledger empty and the next run is
asked again; `PromptEachStep` mints nothing; the covered set is exactly what was minted and grows no
further.

`crates/jean-core/tests/plans.rs` — 4: an operator's run of **three gated steps** prompts **zero**
times, records `run_scope` for every step, writes nothing to `approvals`, and exits 0; the same plan
under `--prompt-each` prompts three times and records `approved_this_time`; a second run in the same
process against the same store is asked again and its decline is obeyed (exit 5); a plan containing a
denied capability and an elevated tool is refused whole with the grant in play, prompting nobody.

`apps/jean-cli/tests/cli_phase2.rs` — 3, against the spawned binary: `model status` reports a
verified HTTPS transport, mentions chunked, and never prints the credential's value while printing
its variable name; `provider = "none"` still refuses `jean do` and still runs `jean exec` unchanged;
`--prompt-each` parses (exit 1 = refused for want of a provider) where an invented flag is rejected
by clap (exit 2).

Carried forward: the preparation batch added five TLS handshake tests in `crates/jean-model/src/tls.rs`
and two assertions to `certificate_verification_is_never_switched_off_and_redirects_stay_unfollowed` —
they are listed and result-quoted in §32.2 rather than here, and the defect series continues at
§32.3 with C-6 to C-8.

## 28. Defects found and fixed in this batch

| # | Defect | How it was found | Fix |
|---|---|---|---|
| C-1 | **A non-loopback endpoint over plain `http` was accepted, and with `api_key_env` set it would have sent the bearer token and the whole prompt unencrypted.** The old rules refused `https` (no client) but never objected to a remote `http` — so the only reachable "remote" shape was the insecure one | Reading the endpoint checks once a real TLS client existed | A non-loopback `http` `base_url` is now a load error naming the reason. Loopback `http` stays allowed for a local model server |
| C-2 | `Target::parse` defaulted every scheme to port **80**, so a future `https://host` would dial `:80` | The new port-default assertion in `endpoints_are_parsed_strictly` | Default port follows the scheme (80 / 443) |
| C-3 | **My own regression, caught before it shipped:** the new `dial()` joined the path prefix a second time, producing `POST /v1/v1/chat/completions` for any llama.cpp-style `base_url` | The pre-existing adapter test `the_request_is_the_subset_every_compatible_server_agrees_on` failed on the raw request line | `dial()` takes the path the adapter already joined; a test now pins the once-only prefix |
| C-4 | `ureq`'s own default is `max_redirects: 10`. Adopting a maintained client would have **started** following redirects — a behaviour change hidden inside a dependency upgrade | Reading the resolved defaults in the crate's source before writing the config | `max_redirects(0)` explicitly, plus an assertion on the built config and a stub that counts connections |
| C-5 | `ScriptedPrompt::approving()` carries exactly **one** answer, so "a plan of three gated steps" was not expressible: the third question errored and the step became `needs_attention` | A new per-step test reported 2 prompts, not 3 — the harness told the truth about a fixture that could not represent the case | The per-step test scripts three answers. The `OperatorRun` test uses a *denying* prompter, so a leak of authority fails as a visible denial rather than as silence |

## 29. What is still not verified, and the one hard blocker

- **Live-provider compatibility: still unverified**, exactly as §20 says. Nothing was installed,
  started, downloaded or authorised; no model server was contacted.
- ~~**TLS is proven negatively, not positively.**~~ **Closed in §32.** The bullet below is what the
  correction batch actually had: the suite showed an `https` endpoint is attempted, that a plaintext
  listener is rejected by the TLS layer, and that verification is not disabled in the configuration
  this build constructs — which is not the same as validating a certificate chain. §32 now does that,
  positively and negatively, against an isolated authority on loopback. What is still open is only the
  *live provider* case above.
- **Cancellation has a documented ceiling.** `Agent::shutdown()` interrupts a parked read; it does not
  interrupt name resolution or an in-progress TCP connect, which are bounded by
  `timeout_resolve`/`timeout_connect`. Stated in `crates/jean-model/src/lib.rs` and pinned by the
  config assertion in §27.
- **`cargo deny check` has never been run**, including against the new crates. The tool is not
  installed on this machine and installing it is outside this batch. §33 has the exact user-local
  commands and what each needs, still unexecuted.
- **Disk is the binding operational constraint.** `target/` is ~13 GB and the volume holds 475 GB at
  100 % used. §31 measures it, separates the disposable compiler cache from the evidence worth
  keeping, and proposes what to remove — it has **not** removed anything, and the free space moved by
  gigabytes during one working session without any action from this project.

## 30. Reproduce

```text
cd "C:\Users\4Raisan\Desktop\MIII\Personal Projects\J.E.A.N. - The AI Companion"
export MSYS2_ARG_CONV_EXCL='*'
cmd.exe /c 'target\dev.cmd test --workspace --locked'
cmd.exe /c 'target\dev.cmd clippy --workspace --all-targets --locked -- -D warnings'
cmd.exe /c 'target\dev.cmd fmt --all --check'

cmd.exe /c 'target\dev.cmd test -p jean-model --locked'          # transport, chunked, TLS, cancel
cmd.exe /c 'target\dev.cmd test -p jean-policy --test grant'     # what a grant can and cannot reach
cmd.exe /c 'target\dev.cmd test -p jean-core  --test plans'      # three gated steps, zero prompts
cmd.exe /c 'target\dev.cmd test -p jean-cli   --test cli_phase2' # the shipped binary's strings and flags

export JEAN_DATA_DIR='C:\Users\4Raisan\Desktop\MIII\Personal Projects\J.E.A.N. - The AI Companion\target\p2demo'
./target/debug/jean.exe model status       # the transport and supported-subset lines
./target/debug/jean.exe do --prompt-each "..."   # per-step gate, from the operator's own option
./target/debug/jean.exe do --help          # the authority a run carries, in its own words
```

---

# Phase 2 → 3 preparation: the disk, the TLS gap, and the dependency check

Date: 2026-11-05. The brief was to prepare Phase 3 without starting it: measure the disk and propose
a cleanup without performing one, close the TLS test gap for real, and write down the dependency
check that has never been run. Source, configuration, `Cargo.lock`, documentation and the existing test
evidence were preserved; **nothing was deleted, no software was installed, no machine-wide setting was
changed, nothing was committed, pushed or published, and no Phase-3 capability was begun.**

## 31. Disk: what is there, what is disposable, what is evidence

### 31.1 The volume, measured more than once in one session

`df -h /c` on the 475 GB system volume, in the order the readings arrived:

| Reading | Available | What had just happened |
|---|---|---|
| start of this batch | 722 MB | carried over from the end of the correction batch |
| before the first build | **96 MB** | nothing in this project had run |
| before the first `cargo check` | **2.5 GB** | again, nothing in this project had run |
| after the test build | 2.4 GB | — |
| after `cargo build --workspace --locked` | 1.6 GB | this batch's own artifacts |
| at the end of the batch | **1.9 GB** | nothing was deleted; the number moved by itself again |

Two things follow from that table, and they are different in kind. Free space on this volume moves by
gigabytes in minutes with no action from JEAN, so any figure in a document is a snapshot — including
this one. And the batch did consume about 0.9 GB of it, which is ordinary for a build. The operational
consequence for Phase 3 is a procedure rather than a number: check `df -h /c` immediately before
starting anything that compiles, rather than trusting a measurement from earlier in the day. That is
also why the tests in §32 were deferred and then run: at 96 MB the guard said stop, at 2.5 GB it said
go, and both readings were true.

### 31.2 `target/`, measured

| Path | Size | Files | What it is | Verdict |
|---|---|---|---|---|
| `target/debug/deps` | **7.7 GB** | 1785 | `.rlib`, `.exe`, `.pdb`, `.d` for every crate in the graph including the test binaries | regenerable — deleting costs one full rebuild |
| `target/debug/incremental` | **4.5 GB** | 392 | rustc's incremental compilation cache | **disposable now**, no rebuild of dependencies |
| `target/debug/build` | 45 MB | 41 | build-script output (`rusqlite`/libsqlite3 among them) | regenerable; must stay consistent with `deps` |
| `target/debug/.fingerprint` | 5.2 MB | — | cargo's freshness records | same |
| `target/debug/examples` | 0 | 0 | nothing here | — |
| `target/x86_64-pc-windows-gnu` | 22 MB | — | the abandoned GNU-toolchain attempt; the finding is written up in §2, not held in these bytes | disposable |
| `target/doc` | 4.8 MB | — | rustdoc output from Phase 0 | regenerable |
| `target/depcrate` | 1.1 MB | — | the unpacked `ureq` 3.5.1 and `native-tls` 0.2.18 sources that §32's feasibility reading was done in | keep while the TLS work is young |
| `target/p2demo` | 137 KB | 11 | **the §18/§25.3 demonstration record** | keep — see 31.4 |
| `target/tmp` | 0 | 0 | — | — |
| 128 loose files directly in `target/` | 1.22 MB | 128 | every check transcript this project has quoted (`ws-final.log`, `p2-full.log`, `tls-test.log`, the `clippy-pN.log` series, `dev.cmd` itself) | keep — see 31.4 |

`target/debug` totals 13 GB; `target/` totals about 13.1 GB.

### 31.3 The proposal — written, **not executed**

Step 1, safe today, reclaims **~4.5 GB**:

```text
df -h /c                                    # confirm the reading in 31.1 before acting
rm -rf "target/debug/incremental"
df -h /c                                    # confirm the reclaim
```

The only cost is a slower next build: the incremental cache is rebuilt from the `deps` artifacts,
which are untouched, so no dependency is recompiled.

Step 2, only if the volume demands it, reclaims a further **~7.7 GB** (12.2 GB in total):

```text
rm -rf target/debug/deps target/debug/build target/debug/.fingerprint
```

That is a cold start: every crate in the graph, including `ureq`, `native-tls`, `schannel`, `rusqlite`
and `windows`, is rebuilt. Do not delete `deps` on its own — the fingerprints and build-script outputs
in `build/` describe exactly those artifacts, and the three are a set.

Explicitly **not** proposed:

* **`cargo clean`.** `target/` in this project is not purely cargo-managed. `target/dev.cmd` — the
  build wrapper every command in this document runs through — lives inside it, as do the check
  transcripts and the demonstration directory. `cargo clean` would take all of that with the cache.
* Anything outside `target/`, and anything belonging to the operator. The 12 MB of evidence above is
  not worth deleting to buy space that 4.5 GB of compiler cache buys instead.

### 31.4 The evidence, itemised

`target/p2demo` (137 KB) is the artifact set the shipped binary produced during the Phase-2
demonstrations: `jean.db` (120 KB, the task/plan/step rows), `audit.jsonl` (1.7 KB, the decision and
outcome lines quoted in §25.3), `artifacts/t-…-0/stdout.log` and `stderr.log` for the two executed
steps, `logs/jean.log.2026-11-05`, the `jean.toml` and `tools.toml` that configured the run, and
`work/note.txt` — the file the plan was told to write. It is 0.001 % of the directory and it is the
only on-disk proof that the demonstrations in this document were run against the real binary.

The 128 loose transcripts (1.2 MB) are where the `[exit] N` lines quoted throughout §3, §10, §17, §26
and §34 came from. Largest first: `tls-test.log` (57 KB), `ws-final.log`, `ws-test1.log`,
`ws-test2.log`, `final-test.log`, `p2-full.log`/`.txt`, `p2-final5.log`/`.txt`,
`p2-verify-test.log`. They are also what makes "the checks ran" a checkable claim rather than an
assertion, and they cost nothing to keep.

## 32. The TLS gap, closed

§29's honest position was that certificate verification had only ever been shown *negatively*: an
`https` endpoint was attempted, a plaintext listener was refused by the TLS layer, and the built
configuration did not have `disable_verification` set. None of that validates a chain. This section
does, in both directions, with no trust store on this machine touched.

### 32.1 What was added

| Piece | Purpose |
|---|---|
| `crates/jean-model/tests/fixtures/tls/generate.sh` | Repeatable throwaway PKI: two CAs and three leaves — `leaf-localhost` (SAN `localhost`, CA A), `leaf-other-host` (SAN `other.invalid`, CA A: trusted chain, wrong name) and `leaf-untrusted-ca-b` (SAN `localhost`, CA B: right name, untrusted chain). RSA-2048/SHA-256, `keyUsage`, `extendedKeyUsage=serverAuth`, `subjectAltName` on every leaf, valid to 2040-07-14. Also writes each leaf's PKCS#12 bundle for the server side and `ca-a.der` / `ca-b.der` for the client. 18 generated files, 86 KB |
| `native-tls = "0.2.18"` as a **dev-dependency** of `jean-model` | Only for `TlsAcceptor` — the tests need a TLS *server*. `ureq` already pulls the same crate at the same version, so on Windows nothing new compiles; `Cargo.lock` gained one edge in `jean-model`'s dependency list and no other change |
| `agent_config(request, anchor)` and `send_inner(…, anchor)` in [crates/jean-model/src/http.rs](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-model/src/http.rs) | The seam. Production `send()` passes `None`, which is `RootCerts::PlatformVerifier` — the Windows store, unchanged. A test may pass one DER, which becomes `RootCerts::Specific`, i.e. `disable_built_in_roots(true)` with that one certificate added: a **narrower** trust set, not a disabled check |
| `send_test_anchored`, `#[cfg(test)]` | The only way to reach the anchored path, compiled out of a shipped build |
| `crates/jean-model/src/tls.rs`, `#[cfg(test)] mod tls` | The stub TLS server and the five rows below |
| `describe()` now attaches the "this is a verified TLS connection" sentence to an `https` failure on **loopback too**, and to `Error::ConnectionFailed` | Previously only a non-loopback `Io` error got it, so a local handshake failure read as a bare socket error. The wording no longer names the system store, because it has to stay true when a test has pinned one root |

### 32.2 The five rows, and what each one proves

| # | Test | Server certificate | Dialed name | Pinned anchor | Result |
|---|---|---|---|---|---|
| 1 | `a_trusted_certificate_for_the_right_name_completes_the_handshake_and_answers` | `leaf-localhost` (CA A) | `localhost` | CA A | **ok** — 200, `{"schema_version":1}`, and the server's own record says *handshake completed and answer written* |
| 2 | `a_trusted_certificate_is_refused_when_the_dialed_host_is_not_in_it` | `leaf-localhost` (CA A) | `127.0.0.1` | CA A | **ok** — `native-tls: The certificate's CN name does not match the passed value. (os error -2146762481)` |
| 3 | `a_certificate_for_another_host_name_is_rejected` | `leaf-other-host` (CA A) | `localhost` | CA A | **ok** — the same `does not match` sentence |
| 4 | `a_certificate_that_does_not_chain_to_a_trusted_authority_is_rejected` | `leaf-untrusted-ca-b` (CA B) | `localhost` | CA A | **ok** — `native-tls: A certificate chain processed, but terminated in a root certificate which is not trusted by the trust provider. (os error -2146762487)` |
| 5 | `the_untrusted_fixture_is_accepted_once_its_own_authority_is_pinned` | `leaf-untrusted-ca-b` (CA B) | `localhost` | CA B | **ok** — accepted, so row 4's rejection was the anchor and nothing else |

Rows 1 and 5 are the controls. Row 1 differs from rows 2 and 3 only in the name; row 5 differs from
row 4 only in which CA the client was told to trust. Every rejection test additionally asserts that
the server answered nothing (`!answered`), that the failure is a transport failure rather than a
status, and — since C-8 below — that Schannel's own sentence names the check that fired.

Not changed, and asserted as unchanged: `disable_verification` is never set anywhere in the workspace
(the configuration test now checks both the production and the anchored branch), production roots stay
`PlatformVerifier`, and no certificate was imported into, exported from, or otherwise installed in any
Windows store. The pinned root is handed to one `Agent` in one test process.

Repeatability was run rather than assumed: `generate.sh` was executed again after the tests passed, the
fixed serial-file cleanup leaves 19 files and no litter, and the five rows pass against that freshly
generated certificate set — so the result does not depend on the particular keys that first happened to
be written.

### 32.3 Defects found on the way, all three in the harness

| # | Defect | How it was found | Fix |
|---|---|---|---|
| C-6 | `native_tls::Identity::from_pkcs8` on Windows associates the key with `KeySpec::key_exchange()`; the identity builds, `TlsAcceptor::new` succeeds, and every handshake then dies at `accept()` with *"No credentials are available in the security package (os error -2146893042)"* | The first version of `tls.rs` used the PEM form, so **all five rows failed** — and the three rejection rows were indistinguishable from success | `generate.sh` now emits a PKCS#12 bundle per leaf and the tests use `Identity::from_pkcs12`, written with `PBE-SHA1-3DES` and a SHA-1 MAC because those are the algorithms CryptoAPI has always imported. Whether OpenSSL 3.x's AES-PBES2 defaults would import here was **not** tested — the compatible choice was made deliberately rather than found by trial |
| C-7 | The stub read the request only to the header terminator. The body sat unread, so closing the socket sent an **RST** instead of a FIN, and the client reported *"An existing connection was forcibly closed by the remote host (os error 10054)"* on a connection whose TLS handshake had already completed | The server's own recorded line — *handshake completed and answer written* — contradicted the client's error. That contradiction is why `TlsServer` records what it saw | `read_request` parses the declared `content-length` and drains the body before answering |
| C-8 | Two of the five tests were passing while the harness was broken: `is_err` plus `!answered` is also exactly what a server that never completes a handshake produces | C-6 and C-7, found only because the accepted rows went red | The two accepted rows are permanent controls, and each rejection now asserts the specific Schannel sentence (`"does not match"`, `"not trusted"`) — a reset carries neither, so a repeat of C-6 or C-7 fails loudly instead of quietly green |

## 33. `cargo deny check`: still outstanding, with the exact commands

Confirmed on this machine before writing this section: `cargo-deny` is **not installed** (nothing named
`cargo-deny` in `%USERPROFILE%\.cargo\bin`, and `which cargo-deny` finds nothing), `deny.toml` **does**
exist (2.8 KB, Phase 0, amended for DV-02 and D-024), and it has therefore never been executed against
any dependency set in this project — including `ureq` and the `native-tls`/`schannel` edge this batch
added. Installing it was out of scope for a preparation batch, so the commands are recorded, unrun:

```text
cd "C:\Users\4Raisan\Desktop\MIII\Personal Projects\J.E.A.N. - The AI Companion"
export MSYS2_ARG_CONV_EXCL='*'
df -h /c                                   # see 31.1; do not start this under ~2 GB free

cmd.exe /c "target\dev.cmd install cargo-deny --locked"
```

That is the whole install, user-local: `dev.cmd` runs `cargo install` **inside the existing VS 2022 x64
developer environment for that child process only**, so the linker is found without any PATH or machine
change, and the product is one binary at `%USERPROFILE%\.cargo\bin\cargo-deny.exe`. It is not a
machine-wide installation, and it is reversible by deleting that one file.

Then, from the same directory:

```text
cmd.exe /c "target\dev.cmd deny check bans licenses sources --locked"   # no network needed
cmd.exe /c "target\dev.cmd deny check --locked"                          # the whole thing
```

`bans`, `licenses` and `sources` work from data already on disk: `Cargo.lock` (resolved) and the
registry cache under `%USERPROFILE%\.cargo\registry`, which holds every `.crate` this workspace uses.
`advisories` is the part that needs the network — cargo-deny clones or updates the RustSec advisory
database under `%USERPROFILE%\.cargo` on first run, so on a 100 %-used volume that is the step most
likely to fail, and the first command above is the one to run if only one can be afforded.

Storage, stated honestly rather than guessed: `cargo install` compiles cargo-deny's own dependency tree
in a staging directory under `%USERPROFILE%\.cargo\install-staging` and removes it on success. The exact
footprint is only knowable by building it, which this batch did not do, so it is **not** measured here —
only the shape of the cost is certain: a few hundred crates compiled once, into `~/.cargo` rather than
into this project's `target/`. What *is* measured is the current state (registry cache present, no
`install-staging` directory now) and the free space in 31.1. Read `df -h /c` immediately before the
install, treat several hundred megabytes of transient peak as the working assumption, and do not start
it with less than ~4 GB free on a volume that spends itself this casually.

What is expected from the check itself, from what is already known: `deny.toml` sets
`[graph] targets = ["x86_64-pc-windows-msvc"]`, so the non-Windows branches of `native-tls`
(`openssl`, `security-framework`) drop out of the analysis, and the `portable-pty` ban (D-024) has still
never been exercised because nothing pulls that crate. **One finding is predictable without the tool:**
`sysinfo` 0.40.1 → `ntapi` 0.4.3 → **`winapi` 0.3.9**, and `deny.toml` bans `winapi` as
*"frozen since 2020; windows-rs supersedes it"*. That edge has been in the graph since Phase 1; it is
not this batch's, and no run of cargo-deny has ever seen it. Expect the first `bans` check to say so.
The honest resolutions are a `[[bans.skip]]`/`skip-tree` entry naming `ntapi` as the sanctioned path to
`winapi`, or dropping `sysinfo` — both are policy decisions, and neither will be made by editing
`deny.toml` quietly to get a green result. Licence review of the `ureq`/`native-tls`/`schannel`/`der`/
`webpki-root-certs` additions is genuinely the tool's job and is **not** predicted here.

The first real `cargo deny check` will be the moment the "no async runtime entered the tree" claim is
tested rather than argued. `tokio`, `hyper` and `h2` appear **zero** times in `Cargo.lock` today, which
is what §34 of the correction batch claimed and what the ban list exists to keep true.

## 34. Checks executed in this batch

| Check | Command | Result |
|---|---|---|
| Build | `target\dev.cmd build --workspace --locked` | `[exit] 0`, no warnings |
| Tests | `target\dev.cmd test --workspace --locked` | **703 passed, 0 failed, 1 ignored** over 29 result blocks — unit, integration and doc suites (698 before, plus the five rows of §32) |
| TLS rows alone | `target\dev.cmd test -p jean-model --lib tls:: -- --test-threads=1` | `5 passed; 0 failed`, `[exit] 0` |
| Formatting | `target\dev.cmd fmt --all -- --check` | `[exit] 0` |
| Clippy | `target\dev.cmd clippy --workspace --all-targets --locked -- -D warnings` | `[exit] 0`. The **first** run failed with three `clippy::err_expect` findings on `.err().expect()` in `tls.rs`; fixed by `expect_err`, no `#[allow]` added and no test weakened |
| Dependency audit | `cargo deny check` | **not run** — see §33 |
| Live provider | `jean model ping` against a real endpoint | **not run** — no provider authorised (§20) |

The three new files/areas are covered by nothing else, so the repeat command is:

```text
cmd.exe /c "target\dev.cmd test -p jean-model --lib tls:: --offline -- --test-threads=1"
```

`--test-threads=1` is not a workaround for flakiness; it keeps the five rows' port and timing evidence
readable. They pass in parallel too — that is how the 703 above ran.

## 35. What Phase 3 inherits from here, unchanged and not started

1. **The disk decision (§31)** — 4.5 GB of incremental cache is proposed and untouched. Phase 3 adds
   capture, UIA and tray surfaces; it will want that space before it wants anything else.
2. **`cargo deny check` (§33)** — outstanding since Phase 0, now with exact commands. It becomes more
   than a formality the moment Phase 3 adopts a UIA or capture crate.
3. **Live-provider compatibility (§20)** — still one authorised `jean model ping` away.
4. **The seven Phase-0/1 manual checks (§21.1)** — still listed as manual, still not performed.
5. No Phase-3 work was begun: no Telegram, no desktop capture, no UI Automation, no voice, no ACP, no
   MCP, no tray UI, and no Phase-3 schema, module or command was added.

---

## 36. Phase 3 — external agents, MCP both ways, and the disk that stopped the browser

This section is the Phase-3 delivery record for the batch that ran 2026-11-05 → 2026-11-06. It
supersedes §35 item 5 ("no Phase-3 work was begun"): delegation, ACP supervision, MCP and the browser
decision are now in the tree, except the browser, which is decided and not built (§39).

### 36.1 What exists and runs

| Area | Crate / module | Command | State |
|---|---|---|---|
| ACP + CLI worker connectors | `crates/jean-acp` (`acp.rs`, `cli.rs`, `frames.rs`, `watchdog.rs`, `contract.rs`, `fake.rs`) | used by `jean delegate` | built, 40+ unit tests green, **live settlement defect open** (§38) |
| Delegation, isolation, independent verification | `crates/jean-core/src/delegation.rs` | `jean delegate "<goal>" [--workspace D:\repo] [--worker opencode] [--json]` | built, 21 integration tests green |
| Delegated permission authority | `jean-core` `WorkerAuthority`, `jean-policy` `Authority::DelegationScope`, `DenyReason::OutsideScope` | inside the same command | built and tested |
| MCP client (JEAN starts a server) | `crates/jean-mcp/src/client.rs` + `jean-core/src/mcp.rs` | `jean mcp tools <server>`, `jean mcp call <server> <tool> --args '<json>'` | built, 9 + 17 + 6 tests green |
| MCP server (JEAN *is* the server) | `crates/jean-mcp/src/server.rs` + `jean-core::mcp::serve` | `jean mcp serve` | built, 5 end-to-end tests against the real binary |
| Configuration and records | `[mcp]`, `[delegation]`, `[browser]` in `jean-protocol`; `delegations`, `worker_events`, `mcp_call`, `McpToolCalled`/`BrowserAction` in store/protocol | `jean mcp status`, `jean agent` | built |
| Browser under JEAN's control | `jean-protocol::BrowserConfig` (parsed, validated, disabled by default) | — | **config and decision only. No driver code.** (§39) |

### 36.2 Exact invocations, as an operator types them

```toml
# jean.toml — the shape these tests used
[delegation]
enabled = true
default_worker = "opencode"
verify_command = "cargo"
verify_args = ["test", "--offline"]
session_timeout_secs = 900
idle_timeout_secs = 180
permission_timeout_secs = 120
isolate_in_worktree = true
require_clean_baseline = true

[[delegation.workers]]
name = "opencode"
kind = "acp"                     # this build speaks ACP here
command = "opencode"
args = ["acp"]
capability = "agent.delegate"
sandbox = "workspace-write"

[[delegation.workers]]
name = "codex"
kind = "codex-cli"                # structured CLI fallback: codex has no `acp` subcommand
command = "codex"
args = ["exec"]
capability = "agent.delegate"
sandbox = "workspace-write"

[mcp]
enabled = true
request_timeout_secs = 30
max_message_bytes = 262144
max_result_bytes = 8388608
shutdown_grace_ms = 2000
expose = ["git.status"]           # only these registry tools are published by `jean mcp serve`

[[mcp.servers]]
name = "fs"
command = "node"
args = ["C:\tools\mcp-fs.js"]
allow_tools = ["read_file"]       # empty means NOTHING is callable; it does not mean "all"
capability = "mcp.call"
env_passthrough = []
```

```text
jean agent
jean delegate "make the failing date test pass" --workspace D:\repos\app --worker opencode
jean delegate "rename the utility module" --json
jean mcp status
jean mcp tools fs
jean mcp call fs read_file --args "{\"path\":\"README.md\"}"
jean mcp call fs read_file --args "{\"path\":\"README.md\"}" --json
jean mcp serve
```

Exit codes keep the Phase-0 vocabulary: `0` proven, `1` not proven (including a tool-level failure the
server *answered*), `3` nothing was checked — never read as a success, `4` needs a person, `130`
cancelled.

### 36.3 Commands executed in this batch, with what came back

Every row is a command run in this workspace on this machine, not a claim carried over.

| Command | Observed result |
|---|---|
| `target\dev.cmd test --offline -p jean-mcp` | `9 passed; 0 failed`, `[exit] 0` — char-boundary truncation, client identity, a plain-text peer refused, a silent peer stopped by `request_timeout`, and the four server-router rows |
| `target\dev.cmd test --offline -p jean-core --test mcp` | `17 passed; 0 failed`, `[exit] 0` — scripted peer; allowlist refusal before any spawn; `auto_deny` reaching neither prompter nor peer; run row with the peer's pid; remembered approval covering one argument vector and not another; stalled session closed exactly once; unopenable session recorded `launch_failed` |
| `target\dev.cmd test --offline -p jean-core --test mcp_live` | `6 passed; 0 failed` in **15.91 s**, `[exit] 0` — a **real Node MCP server**, spawned by JEAN, completing the `rmcp` handshake, `tools/list`, `tools/call`, an 9000-byte result cut at 4096, and a 9 KiB frame refused by a 2 KiB line ceiling |
| `target\dev.cmd test --offline -p jean-cli --test cli_mcp_serve` | `5 passed; 0 failed`, `[exit] 0` — the real `jean.exe` as an MCP **server**, driven by hand-written JSON-RPC over its stdin: handshake, published list, an allowed call returning `proven` **plus the captured output**, an unpublished tool refused by name with **zero task rows**, a wrong argument shape refused, and a capability needing a person refused with the reason on stderr |
| `target\dev.cmd test --offline -p jean-core --test delegation` | `21 passed; 0 failed`, `[exit] 0` — after §37's path fix |
| `target\dev.cmd test --offline -p jean-core --test delegation_live` | 2 tests `#[ignore]`d, both after proving the session gets as far as a performed write (§38) |
| `target\dev.cmd fmt --all` | `[exit] 0` |
| `target\dev.cmd check --offline --all-targets -p jean-core -p jean-cli -p jean-mcp -p jean-acp` | `[exit] 0`, no warnings |
| `target\dev.cmd clippy --workspace --all-targets --locked -- -D warnings` | **not run in this batch** — disk reached 0.39 GB free (§40) and a fresh Clippy build was no longer safe |
| `target\dev.cmd test --workspace --locked` | **not re-run after Phase 3** for the same reason. The Phase-2 figure (703 passed) is still the last full-suite number and is now stale |
| `cargo deny check` | **still not available** — see §33 and §41 |
| `jean model ping` | still not run — no provider authorised |

Note on §33 from this batch, **measured from `Cargo.lock` rather than estimated**: it is now
**276 packages** (was 68 before Phase 3). An earlier draft of this section said 119; that figure was
wrong and is corrected here — the difference matters, because a 208-package addition is a different
supply-chain conversation from a 51-package one, and this project's whole claim is that its numbers
come from a command.

First-tier entries and their **locked** versions: `rmcp 3.8.0`, `agent-client-protocol 5.0.0`,
`agent-client-protocol-schema 1.15.1`, `tokio 1.54.1`, `tokio-util 0.7.20`, `blocking 1.7.0`,
`futures 0.3.34` / `futures-util 0.3.34`, `base64 0.23.1`, `dyn-clone 1.0.20`. (Versions named here
earlier as `tokio 1.52.3`, `futures 0.3.36`, `derive_builder`, `educe`, `getset` and `pins` are not in
this lock at all — 1.52.3/0.3.36 were read from the wrong place, and the last four are dependencies the
pinned SDKs turned out not to need here.)

**Three duplicate-version pairs the tree now carries**, each of which `cargo deny check` would have an
opinion about, listed because the tool cannot be run: `schemars 0.9.0` **and** `1.2.3`;
`thiserror 1.0.69` **and** `2.0.21`; `windows 0.61.3` **and** `0.62.2` (with `windows-core 0.61.2` and
`0.62.2` beside them). These come from the two SDKs' own manifests, not from anything chosen here, and
`default-features = false` on `rmcp` plus the feature-free ACP declaration is what keeps the closure
from being larger — no `reqwest`, `hyper`, `rustls`, `tower` or `oauth2` anywhere in it.
`cargo deny` is **not installed on this machine and this batch may not install software**, so the
licence and duplicate-version audit was not run and no licence conclusion is claimed here. The offline
alternative that *is* available — `cargo tree` plus reading each new crate's vendored `LICENSE` under
`%USERPROFILE%\.cargo\registry\src` — was likewise not
performed, and is listed as outstanding in §41 rather than reported as done. The `sysinfo → ntapi →
winapi` conflict in §33 is unchanged, unweakened and still open. `deny.toml` was not edited.

## 37. A child process cannot be started in a verbatim path: found by the first live run

`resolve_workspace` canonicalized the operator's directory and passed the result to `LaunchSpec` as the
worker's and the checker's working directory. On Windows `std::fs::canonicalize` answers
`\?\C:\Users\…`, which is exact but is **not a path other programs understand**. The live run's own
artifacts are the proof — `artifacts/<task>/baseline-stderr.log` said, in `cmd.exe`'s words:

```text
'\?\C:\Users\…\work'
CMD.EXE was started with the above path as the current directory.
UNC paths are not supported.  Defaulting to Windows directory.
```

So the *baseline verification* — the check that decides whether a delegation can ever be called
proven — ran in `C:\Windows`, not in the workspace. Nothing in the 21 scripted delegation tests could
see this: the scripted runner never starts a child in that directory, and `cmd.exe`'s complaint goes to
a stderr file nothing asserted about.

`delegation.rs` now converts the canonical path to the plain drive spelling (`plain_windows_path`)
before it reaches a `LaunchSpec`, and keeps `\host\share` for a real UNC root, where the verbatim
prefix is the only thing making the path unambiguous. The containment tests are unaffected because
`resolves_inside` and `jean-acp`'s `inside_workspace` re-canonicalize both sides anyway. The delegation
test that compares the worker's directory now asserts the plain form *and* that no verbatim prefix
survives, so the regression is named rather than implied.

## 38. The ACP session does not settle after J.E.A.N. answers the agent: was open, now closed — see §44

> **Status 2026-11-06, later in the same day: closed.** Both scenarios below run to settlement in
> 3.98 s, un-`#[ignore]`d, in the default suite. The two defects this section describes were one defect,
> in the transport's `Drop` rather than in either the connector or the timers, and §44.1 records the trace
> that showed it. This section is kept unchanged below, because *what was ruled out, and how*, is the part
> a future reader needs — the wrong hypotheses here were the plausible ones.

`crates/jean-core/tests/delegation_live.rs` runs the whole §3 workflow against a Node script that
implements the agent side of ACP. It is `#[ignore]`d because it does not finish, and it is kept because
the artifacts it produced are the strongest ACP evidence in this repository. `artifacts/<task>/worker-transcript.log`:

```text
[     1] worker_started: worker fixture (acp) started as pid 12488
[     2] worker_progress: #1 agent_message: writing the file now
[     3] permission_requested: fixture asked to write \?\C:\Users\…\work\agent-note.txt — answered allow_once (allow_once)
```

and `work/agent-note.txt` exists afterwards, containing `written by the agent\n`.

That sequence proves four things no scripted test can prove: `initialize` and `session/new` complete
against a real peer; `session/prompt` is sent; `session/update` arrives and is recorded as a bounded,
redacted progress event; and an `fs/write_text_file` ask is answered from the delegation's own scope —
**no prompt**, `allow_once` taken from the agent's own offer list — with **J.E.A.N. performing the
write** inside the workspace.

What does not happen is settlement. After that reply the session never reports an end: the transcript
stops at three events, `worker_settled` is never emitted, and the run does not end inside
`[delegation] session_timeout_secs` either. So there are two distinct open defects:

1. **The connector does not finish a turn it started** when the agent answers after J.E.A.N. has
   replied to the agent's own `fs/write_text_file` request. Whether the agent's `session/prompt`
   response is unread, unrecognized, or dropped by the `read_update` loop was not established — the
   fixture and the assertions are in the ignored test so the investigation starts from a reproducing
   command, not from a description.
2. **The session deadline did not bound it.** The watchdog is armed before the runtime starts and the
   session `select!` waits on both the update stream and the signal channel, so a 90-second budget that
   does not end a 4-minute session is a second bug, not a detail of the first. It is the more serious
   of the two: an unbounded delegation is the failure mode §2 of the brief set the bounds against.

`--locked` consequence, stated plainly: **no live ACP coding agent has completed a delegation on this
machine**, and the `opencode acp` / `codex exec` connectors are therefore *unverified for live
compatibility*. The scripted-runner suites prove J.E.A.N.'s side of the contract — deciding, recording,
verdicting, tearing down — and prove nothing about a vendor's agent.

### 38.1 What the 2026-11-06 re-check ruled out, without a build

The artifacts of the hung run survived in `%TEMP%` (`jean-acp-live-inside-15432-…`, left in place —
nothing was deleted), so the pair could be probed with reading and Node alone. Four things are now
established, and they narrow the two hypotheses to one question:

| Check | Method | Result |
|---|---|---|
| Is the **fixture's** own state machine at fault — i.e. does it ever send the prompt response? | Extracted `AGENT_SCRIPT` from the test and replayed the four client frames into it with `node` | **It answers.** On receiving `{"jsonrpc":"2.0","id":"fs-1","result":null}` it emits `{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}`. The agent side is capable of settling. |
| Does the SDK's output framing suit a `readline` reader? | Read `agent-client-protocol-5.0.0/src/jsonrpc.rs:7255-7300` (`ByteStreams::into_lines`) and `write_line` | **Yes.** The writer pushes `b'\n'` and flushes; the reader is `BufReader::lines()`. Newline-delimited JSON both ways — the fixture reads what JEAN sends. |
| Does a **string** request id (`"fs-1"`) survive the SDK? | Read `jsonrpc/raw_id.rs:47-54` | **Yes.** `RawJsonRpcId` is `Null \| Number(i64) \| Str(String)` and replies are correlated in a `HashMap<RawJsonRpcId, _>`, so the echo is not the problem. |
| Did the agent crash or print anything? | `artifacts/t-18e539a7d055b900-3c48-0/stderr.log` | **Empty.** No Node error, no partial frame. |
| **How long did it actually wait, and did any timer fire?** | File timestamps in that artifact directory | Worker started and the last transcript event was written at **00:00:19–00:00:20**. The next thing the run wrote — the post-session `git status` probe captures — is stamped **00:04:01**, when the run was interrupted from outside. So the session sat unsettled for **≈221 seconds** while its own configuration said `idle_timeout_secs = 45`, `session_timeout_secs = 90` and `permission_timeout_secs = 15`. **Three timers, none of them fired.** That is defect 2 quantified, and it is the more serious defect on its own. The run itself cannot say which half failed — the watchdog never produced the signal, or the signal never reached the `select!` in `acp.rs::session` — and both are consistent with what is on disk, which is exactly why the instrumentation below comes before any fix. |
| Was the empty `git-status-after` capture a *summary* bug? | Re-ran the same read-only command in the same scratch repo now: `git -C …/work status --porcelain=v1` | **No.** It reports `?? agent-note.txt` exactly as it should, and `git status --porcelain=v1 -z` (what `delegation.rs:1440` runs) includes untracked files by default. The 0-byte captures are the artifact of the interruption at 00:04:01, not of a missed change. The changed-file summary itself is innocent, which matters because it is the step that would have been blamed. |

What is left is a single question the fixtures cannot answer: **did JEAN's reply frame actually reach
the agent's stdin?** Everything above says it *should* have; the agent's silence says it *did not* (or
that the agent's answer came back and was not correlated by `read_update`). Both remaining hypotheses
live below the connector's decision logic, in the byte pipe, so the discriminator is a line-level log on
each direction — and that is ours to instrument, not the SDK's: `jean-acp` already wraps both halves in
`frames.rs` (`BoundedRead` / `BoundedWrite`), which see every byte in and every byte out. A debug-gated
"one line per frame, direction + byte count, no content" there, run against the ignored test, splits the
two cases in a single session. (`Stdio::with_debug` exists in the SDK but that transport is not the one
this connector uses, and `AcpAgent::with_debug` sits on the spawner this build never constructs — see
D-041.)

**One coverage gap this re-check also exposed, and it is the honest kind:** `crates/jean-acp/src/fake.rs`
— the in-memory ACP peer the scripted tests use — **never sends an agent-initiated
`fs/write_text_file`**; it exercises permission requests only (grep for `write_text_file` there returns
nothing). So the path "agent asks the client to write → client answers → agent then ends the turn" has
**no scripted coverage at all**, which is precisely why it was the live run that found it. A fake-agent
turn that ends after an answered write is the test to add first: it reproduces the hang deterministically
and it belongs in `crates/jean-acp`, not in a `#[ignore]`d Node script.

## 39. The browser: decided, recorded, and **not built** — superseded by §45

> **Status, later the same day: built.** `crates/jean-browser`, `crates/jean-core/src/browser.rs`,
> `jean browser status|run`, and four live workflows against the installed Chrome — §45. What is kept
> below is the decision record and the two things a first implementation had to get wrong; both of them
> are now checked in code (`endpoint_is_loopback`, `ProfileNotEmpty`) rather than in prose.

The brief asks that the browser choice be settled before implementation. It is settled — D-046 — and
the implementation is missing, which §40 explains. Nothing in this section should be read as a browser
capability that exists.

**Decided.** Drive the **already-installed Chrome** (`C:\Program Files\Google\Chrome\Application\
chrome.exe`, 156.0.8078.81) as a **process J.E.A.N. starts, owns and closes**, with its own temporary
profile, over loopback CDP, using `headless_chrome` — already declared in `Cargo.toml` as
`headless_chrome = "1.0.22"`, 100 % synchronous, so it adds no new runtime to the browser path.

**Rejected, with the reason.** *Playwright or `@playwright/mcp`* — needs `npm install` of packages and
browsers, and installing software is out of scope for this batch; an MCP browser server would also put
a second process between JEAN's policy engine and the act. *`chrome --dump-dom`* — no interaction, so
the required workflow (navigate → inspect → interact → verify) cannot be expressed. *Attaching to the
browser the operator is already using* — that browser holds a person's sessions, and remote debugging
would have to be enabled on a profile they use; refused by design in
`jean_protocol::BrowserConfig`'s own documentation. Edge and Brave are also installed and were
examined the same way: present, not launched, not touched.

**What is actually in the tree.** `[browser]` parses and validates (`enabled` default `false`,
`browser_path`, `headless` default `true`, `startup_timeout_secs`, `action_timeout_secs`,
`max_page_bytes`, and `allowed_origins` restricted to `file:` plus loopback `http(s)`), the
`BrowserAction` event kind exists, and **no code reads any of it**. `headless_chrome` is declared but
unconsumed, so it is **absent from `Cargo.lock`** — adopting it requires a network fetch, which is what
§40 stopped.

**The two things a first implementation must not get wrong**, established from the crate's own source
rather than guessed: it recovers the websocket URL by scraping the child's stderr, and it performs **no
loopback validation** of it — so J.E.A.N. must check the scheme, host and port before connecting, and
must refuse any URL whose host is not loopback. It also does not pass `--remote-allow-origins`, so a
browser J.E.A.N. did not start cannot be reached by this route even by accident.

## 40. Disk: the reason the batch stopped where it did

C: free space, measured with `Get-PSDrive C` at each point. Nothing was deleted, no cache was cleared,
and no file named in this document was removed.

| Point in the batch | Free |
|---|---|
| Figure given in the brief | ~10.5 GiB |
| Measured at the start of this session | 8.25 GB (7.68 GiB) |
| After the delegation suites built | 5.63 GiB |
| After the jean-mcp suites | 4.55 GiB |
| After the MCP test targets built | 2.49 GB |
| Before the first live ACP run | 1.16 GB |
| **At the point work stopped** | **0.39 GB (402 MB)** |
| Re-checked while reconciling the planning documents (2026-11-06, `Get-PSDrive C`) | **0.51 GB (514 MB)** |
| **After the operator removed `target/debug/incremental` and the batch ran to completion** | **6.27 GiB → 4.45 GiB → 4.71 GiB — the byte-exact ladder, and what one whole-workspace run costs, is §46.5** |

The last drop happened between two small incremental test runs, which is not a rustc-scale change —
the machine is consuming space on its own while this work proceeds. At 400 MB free, `cargo build` of a
new dependency tree (`headless_chrome` and its websocket stack, un-fetched) and a full-workspace
`cargo test --locked` + `clippy --all-targets` were both refused as unsafe: they would take C: to zero
mid-write, which is how a build cache and a user profile get damaged together. §31's proposed 4.5 GB of
incremental cache cleanup is **still not performed**, because deleting caches is the user's call and
this batch may not delete files.

## 41. Outstanding, in the order that unblocks the rest

**Closed since this was written (2026-11-06 closing batch):** items 1–3 are done — the fetch, the browser
module and `jean browser` (§45), the two §38 ACP defects (§44), and the whole-tree test and Clippy runs
(§46.1). Items 4–7 are still open; §47 is the current list.

1. **Free disk** (user decision). Then, in this order: `cargo fetch` for `headless_chrome`, the browser
   module and `jean browser` commands (§39), and the two §38 ACP defects.
2. **`target\dev.cmd test --workspace --locked`** and **`clippy --workspace --all-targets --locked --
   -D warnings`** re-run across the whole tree after Phase 3. The last full-suite number is Phase 2's
   703 passed, and it is stale: this batch added `jean-mcp`'s 9, `jean-core`'s 17 + 6, and the CLI's 5,
   plus 2 ignored live-ACP rows.
3. **The two `#[ignore]`d live ACP scenarios** — reproduce with
   `target\dev.cmd test --offline -p jean-core --test delegation_live -- --ignored --nocapture`. But do
   not start there: §38.1 already exonerated the fixture, the framing, the string ids, the worker and the
   changed-file summary, and it leaves two candidates plus the timer question. The order that makes the
   next build worth its disk is (a) scripted `fs/write_text_file`-then-settle coverage in
   `crates/jean-acp/src/fake.rs` — today that path has no scripted test at all, (b) a deadline that
   actually bounds the update loop (three timers, none fired, in §38.1), (c) per-frame instrumentation in
   `jean-acp::frames`, and only then (d) the live run.
4. **`cargo deny check`** — still not installed, still open, with the `sysinfo → ntapi → winapi`
   conflict unweakened in `deny.toml`. The offline `cargo tree` + vendored-`LICENSE` reading is the
   substitute nobody has done yet either.
5. **Live connector compatibility** — `opencode acp` and `codex exec` are present on this machine and
   have never been driven by J.E.A.N. A vendor session also needs the operator's own authorisation and
   would spend their quota; until then this is *unverified*, and the mocks in this file do not prove it.
6. **`jean model ping`** and the **seven Phase-0/1 manual checks (§21.1)** — both carried forward
   unchanged from Phase 2.
7. **Concurrent workers.** `max_parallel` is validated to `1` and `jean agent` says so, because
   cancellation has one active task per process (D-028). Parallel delegation needs a cancellation
   router that does not exist; nothing in this batch claims otherwise.

## 42. The reconciliation pass (2026-11-06): what changed in the planning documents, and why

*This pass reconciled the documents to a tree without a browser and with R-27 open. §48 is the same exercise
after the checks; where the two disagree, §48 is current and this table stays as the record of what was
known at the time.*

No code changed in this pass, and nothing was compiled — free disk is 514 MB (§40), so this is the
documentation work the brief asked for, done against measured facts rather than against memory. Every
edit below was checked against the tree before writing: `Cargo.lock` counts and versions, the vendored
`agent-client-protocol 5.0.0` manifest, `crates/jean-protocol/src/config.rs` field-by-field, the
`tokio::runtime::Builder::new_current_thread` sites, and read-only `--version` / `--help` invocations
of the installed binaries.

| Document | What was wrong, and what it says now |
|---|---|
| `03-DEPENDENCIES.md` §2 | The `rmcp` feature paragraph listed **`transport-child-process` as adopted**; it is excluded (D-041). Now lists the seven features actually compiled, and separately names the two absences that carry a design reason — child-process spawning and every HTTP transport. |
| `03-DEPENDENCIES.md` §1 | `reqwest` was `ADOPT` with the claim that it arrives free through `rmcp`'s streamable-http feature. That feature is off, and `reqwest`, `hyper`, `rustls` and `tower` are **absent from the lock**. Now `AVOID`, pointing at D-038. |
| `03-DEPENDENCIES.md` §1 | The `tokio` row said "ADOPT — Phase 2+, not Phase 0". It arrived, in Phase 3. Now records the locked `1.54.1`, the feature set, the confinement to `jean-acp`/`jean-mcp`, and that each protocol session builds its **own current-thread runtime** (verified at `crates/jean-mcp/src/client.rs:81`, `crates/jean-mcp/src/server.rs:90`, `crates/jean-acp/src/acp.rs:132`). |
| `03-DEPENDENCIES.md` §2 | `headless_chrome 1.0.22` was **not in the document at all** despite being pinned in `[workspace.dependencies]`. Added as a row: declared, unconsumed, absent from `Cargo.lock`, and why (D-046 + disk). |
| `03-DEPENDENCIES.md` §5 | `@playwright/mcp` and Playwright said `ADOPT`. Now struck, marked superseded by D-046 with the reasons, plus a new row for the installed browsers as the actual child, carrying the three never-do's (running browser, personal profile, someone else's port). |
| `03-DEPENDENCIES.md` §5 | New subsection recording what is **installed here** as re-verified today — `codex-cli 0.160.1` whose `--help` lists no `acp` subcommand, `opencode 1.18.34` whose `--help` advertises `opencode acp`, `claude 2.1.303` with no connector, `node v24.22.1`, `git 2.56.0.windows.4` — and the live-verification sentence, so the upstream-release table above it cannot be misread as this machine's capability. This closes the outstanding feasibility item (§41.5) as far as it can go without a vendor session. |
| `05-ROADMAP.md` Phase 3 | Every deliverable was `[ ]`, including ones delivered, and the list still demanded `transport-child-process`, `@playwright/mcp` and `max_parallel = 3`. Rewritten with `[x]` / `[~]` / `[ ]` and a reason on each line; the exit criteria now say plainly that they are met by scripted evidence and name the two defects blocking a live run. |
| `05-ROADMAP.md` Phase 0 | One stale tick found while reconciling: "`deny.toml` + `rust-toolchain.toml` committed; `cargo deny check` green" (§41.4 says the opposite, correctly). Now `[~]`, with "has never been green because it has never been run" and the `sysinfo → ntapi → winapi` conflict still open. **`deny.toml` was not touched.** |
| `06-RISKS.md` R-01 | Said the SDK boundary is "in `jean-protocol`". It is not — `jean-protocol` has no async and no runtime. Now records the real boundary: `mcp::Peer` (`crates/jean-core/src/mcp.rs:247`) and `WorkerRunner` (`crates/jean-core/src/delegation.rs:228`), plus the ACP crate compiling with **zero features** (`default = []`, read from the 5.0.0 manifest). |
| `06-RISKS.md` R-05 | The browser decision is recorded where the plan asked for one, and the risk is **not** closed: decided ≠ built, `jean browser` is not a command, `headless_chrome` has never been fetched, licence-audited or sized. |
| `06-RISKS.md` R-26 | The disk risk is now a realised risk, with the measured ladder from 10.5 GiB to 402 MB and the three deliverables it blocked. |
| `06-RISKS.md` new | **R-27 🔴** — the live ACP session does not settle and its deadline does not bound it (§38), with the fix order (deadline first, then settlement) and an explicit "do not close this until an un-ignored `delegation_live` run is in the transcript". **R-28 🟠** — Phase 3's protocol evidence is peers JEAN wrote itself, and where that separation is enforced in code and output rather than only in prose. Register table and the gating-risk sentence updated for both. |
| `04-SETTINGS.md` | Three planned sections annotated *As built*: `[browser]` (the seven real keys, the inverted `headless` default, and why `backend`/`port` are not knobs), `[agents]` → `[delegation]` (every key with its default, `max_parallel` above 1 as a **load error**, empty `verify_command` as a refusal, the two shipped workers), and the `mcp.toml` section → `[mcp]` inside jean.toml with `[[mcp.servers]]` (plural, verified against `crates/jean-core/tests/mcp.rs:100`) and no `transport`/`url`/`auth` keys. `[browser]` stays in `DEFERRED_SECTIONS`, so a file that sets it still gets "recognised but not applied" — that warning is true and is kept. |
| `01-SCOPE.md` | The acceptance scenario's step 2 said JEAN "discovers the test command". It never does. Corrected in place, with the claim/verdict field names and the exit-code vocabulary (`failed` 1, `unverified` 3) that a reader needs to interpret a report. |
| `IMPLEMENTATION_STATUS.md` §33 note | **A number in this document was wrong.** It said `Cargo.lock` grew from 68 to 119 packages; the lock holds **276**, and `tokio 1.52.3` / `futures 0.3.36` / `derive_builder` / `educe` / `getset` / `pins` are not in it (locked: `tokio 1.54.1`, `futures 0.3.34`). Corrected where it stands, with the duplicate-version pairs the tree does carry (`schemars 0.9.0`+`1.2.3`, `thiserror 1.0.69`+`2.0.21`, `windows 0.61.3`+`0.62.2`) — information `cargo deny check` would have produced and this project has to produce by measurement instead. `07-DECISIONS.md` D-041's consequence line was carrying the same wrong figure and is fixed the same way. |
| `IMPLEMENTATION_STATUS.md` §38.1 (new) | The ACP defect, re-examined without a build. The hung run's artifacts were still in `%TEMP%`, and replaying JEAN's frames into the extracted Node fixture with `node` alone settled what is *not* wrong: the fixture does answer the prompt when it gets the write reply, the SDK's framing is newline-delimited in both directions, string JSON-RPC ids round-trip, the worker never errored, and `git status --porcelain` still reports the untracked file, so the changed-file summary is not implicated. The timestamps added the number the earlier section lacked — **≈221 s of waiting against `idle 45` / `session 90` / `permission 15`; three timers, none fired** — and reading `fake.rs` showed why this was found live at all: **the scripted ACP peer never sends an agent-initiated `fs/write_text_file`, so that path has had no scripted coverage.** `06-RISKS.md` R-27 now carries that ruled-out list and the (a)→(d) fix order. |
| `IMPLEMENTATION_STATUS.md` §40 | One more row: the free-space re-check on 2026-11-06 (514 MB), still with nothing deleted. |

## 43. Phase-3 documentation state, one line

*Superseded on the facts, correct on the method: see §47 for what is unproven now and §48 for the
reconciliation pass that followed the checks. The sentence below was true when it was written and is not
true any more — disk is no longer what the browser and the live ACP run are waiting on (§45, §44).*

Every Phase-3 claim in `PLANNING/01-SCOPE.md`, `03-DEPENDENCIES.md`, `04-SETTINGS.md`,
`05-ROADMAP.md`, `06-RISKS.md` and `07-DECISIONS.md` was re-read against the tree on 2026-11-06 and
corrected where the code disagreed (§42). What remains unproven is stated in the documents themselves,
not in this sentence: the live connectors (§38/R-27/R-28) and the browser (D-046/R-05), both waiting on
the same thing — free disk (§40).

**Commands run for this pass** (all read-only, none writing to `target/`): `Get-PSDrive C`, `codex
--version` / `codex --help`, `opencode --version` / `opencode --help`, `claude --version`,
`node --version`, `git --version`, and version/name extraction over `Cargo.lock` and the vendored
`agent-client-protocol-5.0.0/Cargo.toml`. Nothing was installed, authenticated, launched, committed,
pushed or deleted.

## 44. §38 closed: the session settles, the deadline binds, and the transport was the bug

Both halves of the defect are fixed, and the fix is not in the protocol handling at all.

| Evidence | Command | Result |
|---|---|---|
| The scripted settlement coverage §38.1 said did not exist | `target\dev.cmd test -p jean-acp --lib --offline --locked` | **44 passed, 0 failed in 2.52 s.** Six of them are `acp::wire_tests` — a byte-level ACP peer on a real duplex, driven through the production `BoundedRead` / `BoundedWrite` / `WireTrace` wrappers: an agent that asks the client to write and then **ends its turn**; that same ask pointed outside the workspace, refused by the connector although the host said yes; an answered ask then silence, ended by the **deadline**; a cancel arriving **while a handler is still running**; a silent peer ended by its **idle limit**; and a host whose own decision blocks past the deadline, which the deadline still outranks. |
| The external Node fixture, un-ignored | `target\dev.cmd test -p jean-core --test delegation_live --offline --locked` | **2 passed, 0 failed in 3.98 s** (17 s wall including the build). The same two scenarios that had sat for **≈221 s** and **≈201 s** and been killed from outside now run in the default suite; both `#[ignore]` attributes are gone from the file. |
| Teardown, over real pipes with a real process | `target\dev.cmd test -p jean-acp --test real_pipes --offline --locked` | **1 passed in 6.95 s.** See §44.3. |

### 44.1 The root cause, established by tracing rather than by reasoning

`JEAN_ACP_TRACE=2` on the live fixture printed the whole turn, and the turn was **complete**:

```text
[acp trace] dir=out frame=5  bytes=706 method=Some("session/prompt")     id=Some("0d3b671e-…")
[acp trace] dir=in  frame=6  bytes=183 method=Some("session/update")     id=None
[acp trace] dir=in  frame=7  bytes=249 method=Some("fs/write_text_file") id=Some("fs-1")
[acp trace] stage=update frame=8 / stage=dispatch frame=9 / update 10 / dispatch 11
[acp trace] dir=out frame=12 bytes=41  id=Some("fs-1")                   ← J.E.A.N.'s answer
[acp trace] dir=in  frame=13 bytes=96  id=Some("0d3b671e-…")             ← the agent's `end_turn`
[acp trace] stage=update frame=14                                        ← the loop took it, and built the report
```

Every frame crossed, the report was built — and `block_on` did not return. Not a protocol defect: the
transport's own **`Drop`**. `blocking::Unblock` joins its helper thread when it drops, and the read half's
thread is parked in `ReadFile` on the worker's stdout. The worker was still alive — an ACP agent stays
alive for the next prompt — so the join could only be released by the very process it was waiting for.
And because that drop runs on the runtime thread *inside* `block_on`, **no timer can fire on a thread that
is not polling**: the 90 s session deadline, the 45 s idle beat and the 15 s permission window were all
armed and all unable to run. That is §38.1's "three timers, none of them fired", and it is one bug, not
three.

Two facts made it worse than a hang. `PipedChild` held a second write end of the worker's stdin, so
dropping the transport never gave the agent EOF — it had no reason to exit. And the scripted harness could
not have caught this at any level of fidelity: **a `tokio::io::duplex` has no thread to join.** The mocks
were not weak; they were exercising a different transport.

### 44.2 What changed in the tree

* `crates/jean-acp/src/frames.rs` — **`SharedPipe<R>`**: a transport half that holds its handle behind an
  `Arc<Mutex<R>>` which the caller keeps. Dropping the transport joins nothing, wherever the SDK drops it;
  the cell is dropped by `acp::run`, after the worker is provably gone, which is when the join stops being
  a wait.
* `crates/jean-acp/src/acp.rs` — the child's own stdin end is closed as soon as the transport has its
  duplicate, so *dropping the transport is what tells the agent the session is over*. Then the order in
  `run` is: wait the grace for a clean exit (so an exit code is reported rather than inferred) → terminate
  if it is still running → drop the runtime → drop both pipe cells → join the stderr capture. Each step is
  a precondition for the next, and the file says so where it matters.
* `session()` races `timeout_at(session_deadline, …)` around the whole connection and stashes the settled
  report before returning, so a session abandoned at its deadline still reports what it observed — and
  reports `timeout` with **no claim** when it observed nothing. Each handler is raced against
  `permission_timeout` and against the signal channel, so a cancel stays responsive *during* a callback
  rather than only between them.
* `WireTrace` (`JEAN_ACP_TRACE=1` via `tracing`, `=2` also to stderr): direction, byte count, and the
  protocol's own `method` and `id` — **never a body**. Bounded at 512 records per session and 120 bytes of
  prefix per frame, with two tests pinning the bound so the redaction is measured rather than promised.
* An `fs/write_text_file` for a path outside the delegation workspace now **reaches the authority**, so the
  refusal is audited as `outside_scope`, and the connector still refuses it whatever the host answered: an
  answer widens a grant, never a boundary. Before this the escape was swallowed inside the connector — the
  behaviour was right and the audit trail said nothing had happened, which is the worst kind of `no`.
* `delegation_live.rs`'s expectation that `tasks.outcome` reads `"proven"` was wrong and now reads
  `"succeeded"`: a tool run's outcome is its verdict, a delegation's is its **result**, because "cancelled"
  and "not proven" are different answers to different questions. The verification label is its own
  `verification_recorded` event, which the same test asserts.

### 44.3 What the suite now proves about teardown — and what it still does not

`crates/jean-acp/tests/real_pipes.rs` (new, 1 test, **6.95 s**) starts a **real** silent worker — a Node
process that spawns a **grandchild** and then says nothing — under a 2 s session deadline. It asserts the
deadline was honoured (`stop_reason = timeout`, a failure naming the deadline, **no claim**, no observed
turn) and that the grandchild's four-second witness file never appears: the job object took the whole tree.
That is the "stop the owned worker tree, settle the task, preserve uncertainty" claim made against actual
processes, so R-27 is closed on this plus the live fixture — not on the duplex tests, which cannot see a
process at all.

Still unverified, unchanged from §38: **`opencode acp` and `codex exec` have never been driven by J.E.A.N.**
Speaking the protocol to a peer this repository wrote is not compatibility with a vendor's agent, and no
number of green tests here changes that sentence. R-28 stays open on purpose.

## 45. The browser is built, and one workflow is verified against the installed Chrome

D-046 recorded a decision with no implementation. There is an implementation now, and the workflow the
phase owes runs and is judged.

**Adoption.** `cargo fetch` pulled `headless_chrome 1.0.22` (MIT, published 2026-06-11 — maintained, not a
frozen crate) and **61 new lock entries: 276 → 337 packages**. Its default feature is `offline`, so the CDP
schema ships with the crate rather than being fetched at build time; the `fetch` feature — which downloads
a Chromium build — is **not enabled and is not reachable from this tree**. Its CDP client is synchronous, so
the tokio confinement of D-041 is intact: `jean-browser` has no runtime dependency.

**The crate.** `crates/jean-browser` is mechanism only — it cannot decide, record, or prompt:

* `OwnedBrowser::start` spawns the browser through **`jean_process::piped`**, so it is born inside a
  kill-on-close job object like every other worker here, and `headless_chrome`'s own launcher is *not*
  used: `Browser::connect(ws)` **attaches** (and deliberately does not send `Browser.close` to a browser it
  did not start). That split is exactly the one the brief asked for — the SDK speaks, J.E.A.N. supervises.
* The endpoint comes from the child's own stderr line and is **checked for a loopback host before it is
  dialled**; `wss://`, an `http://` URL and a routable host are all refused by shape. The stderr pipe is
  drained continuously, because a 64 KiB unread pipe blocks the browser and looks exactly like a browser
  that stopped working.
* A profile that already contains something is **refused** (`ProfileNotEmpty`). That gate is what turns
  "never the operator's browser" from a promise into a checked fact: J.E.A.N. starts on an empty directory
  it created under its own artifact path, or it does not start at all.
* `allowed_origins` is enforced on the URL string in `navigate`, before CDP ever sees it, with the rule the
  config validator documents: a bare scheme binds the scheme, a host with no port covers every port on it,
  and a pinned port means that port and nothing else.

**The workflow.** `crates/jean-core/src/browser.rs` runs it through the pipeline that already existed:
config gate → origin gate → executable resolution → **task row → policy decision → audit line** → start →
navigate → read → act → screenshot → **second read** → `Verdict` → close. `browser.drive` and the new
`OperationKind::DriveBrowser` make a remembered approval bind to the **origin**, not to a URL that could
carry a token. The verdict is a fresh read of an element named *before* the interaction, with the rendered
PNG beside it as a second witness; the click's own return value is not used, because a dispatched event
proves the browser heard, not that the page changed. Proven → `succeeded` (0), not proven → `failed` (1),
no expectation stated → `needs_attention` (3).

**The demonstration** — real Chrome, real CDP, one local offline page:

```text
$ jean browser run "file:///…/target/browser-demo/fixture.html" \
    --inspect "#status" --type "#name=hello from jean" --click "#send" \
    --expect "#status=sent: hello from jean"
task        t-18e5606dee9a00c8-68b8-0
pid         32512
title       J.E.A.N. browser fixture
before      not sent
after       sent: hello from jean
verdict     proven: #status shows "sent: hello from jean" after the interaction — screenshot …\page.png
            started pid 32512 on ws://127.0.0.1:61832/devtools/browser/3ceb025f-…
            navigated to file:///…/fixture.html
            read 876 bytes of page markup
            #status showed "not sent" before anything was done to it
            typed into #name
            clicked #send with a real pointer event
            captured the rendered page (19446 bytes)
            verification: #status shows "sent: hello from jean" after the interaction
[exit] 0      wall 17 s
```

and the three other paths, on the same page:

| Run | Observed | Exit |
|---|---|---|
| click, nothing typed | `after = refused: nothing to send` → `not_proven`. **The page declined, and J.E.A.N. reported the decline** rather than its own click. | 1 |
| read, no `--expect` | `unverifiable: … the page was read and nothing was proven`; task `needs_attention` | 3 |
| `jean browser run https://example.com/` | refused at the origin gate — *"is outside [browser] allowed_origins"*. No pid, no task row, no decision recorded. | 1 |

`jean browser status` prints `browser C:\Program Files\Google\Chrome\Application\chrome.exe` from
detection — the path D-046 recorded — and starts nothing.

**Containment, checked on the machine rather than assumed.** After the four demo runs and four more under
test: `Get-CimInstance Win32_Process -Filter "Name='chrome.exe'"`, filtered on a J.E.A.N.-owned profile in
`CommandLine`, returns **0** — while the operator's own **12** `chrome.exe` processes were untouched. The
profile directory is removed on the way out, and `OwnedBrowser::drop` terminates the job on a panic or an
early `?`.

**Coverage.** `crates/jean-core/tests/browser_live.rs` (new) runs the proven workflow, the page's refusal,
the no-expectation case and the origin gate against the installed browser: **4 passed, 0 failed in 31.40 s**,
inside the default suite, skipping out loud on a machine with no browser. `crates/jean-browser`'s own 7
tests are deterministic and browserless. The first real run found one defect, now fixed: the state machine
requires `running → verifying → succeeded`, and the workflow jumped straight to `succeeded`, which the
store correctly refused — the workflow had already proven the page by then, and only the record failed.

**Configuration, now documented.** `config/defaults.toml` did not carry `[delegation]`, `[mcp]` or
`[browser]` at all, and the test that compares the shipped file with the compiled schema
(`every_configurable_key_is_documented_in_the_shipped_file`) had been failing since §40 added those
sections. All three are written out now with their real defaults and the reasons, and `"browser"` is
removed from `DEFERRED_SECTIONS` because the warning "recognised but not applied" had become false.
`crates/jean-core/tests/config_files.rs`: **12 passed, 0 failed.**

**What is not claimed.** No sign-in, no authenticated site, no download, no extension, no visible window
(`headless = true` by default), no second engine, and no general automation surface: `jean browser run`
executes one stated workflow per invocation, and `[browser] allowed_origins` is the reason it cannot become
something else by accident.

## 46. The batch's checks, and the one test that was asserting Windows' discretion

### 46.1 The four checks, on the final tree

Every command below ran in the VS 2022 x64 developer environment through `target\dev.cmd`, with
`CARGO_INCREMENTAL=0` exported in that child process only. The wrapper always returns 0; the number that
matters is the log's own `[exit]` line, and the transcripts are named below.

| Check | Command | Result |
|---|---|---|
| Build | `target\dev.cmd build --workspace --all-targets --locked` | **`[exit] 0`** (`target/p3-build-final.log`) |
| Tests | `target\dev.cmd test --workspace --all-targets --locked` | **`[exit] 0` — 824 passed, 0 failed, 1 ignored across 32 suites** (`target/p3-test-final.log`). The ignored one is `a_real_toast_delivers_without_panicking`, which reaches the live WinRT API and is run by hand at a desk (§21.1). |
| Format | `target\dev.cmd fmt --all -- --check` | **`[exit] 0`** (`target/p3-fmt5.log`), after one `cargo fmt --all` pass over blocks added in this batch |
| Lints | `target\dev.cmd clippy --workspace --all-targets --locked -- -D warnings` | **`[exit] 0`** (`target/p3-clippy4.log`) — on the fourth run. Three earlier runs reported **17 findings** between them (`target/p3-clippy.log`, `-2`, `-3`), listed in §46.2 |

Phase 2's whole-suite number was 703 passed, and §41.2's "562 passed, 1 failed" undercounted rather than
overcounted: cargo stops at the first failing target, `jean-process` sits late in the list, and **six of the
32 suites — 262 of the 825 tests — never ran at all behind that failure.**

Two of those suites are the ones the phase's honest claims rest on, and both ran inside the whole-tree count
rather than as a favour: `delegation_live` — the external Node ACP fixture over real pipes, the two
scenarios that used to hang — **2 passed in 2.75 s**, against the ≈221 s and ≈201 s of §38; and
`browser_live`, the four real-Chrome paths, **4 passed**. Nothing was installed, authenticated, committed,
pushed or deleted to produce any of it, and `CARGO_INCREMENTAL=0` was set in the child process only, with no
machine-wide environment change.

### 46.2 Clippy's seventeen findings, and the three it did not take

All seventeen were in Phase-3 code written this batch; none was in a file Phase 3 had not touched. They came
in three runs because a crate cannot be linted until the crates below it compile, and `jean-core` sits above
both `jean-browser` and `jean-acp` — not because they were missed.

| Where | Finding | Disposition |
|---|---|---|
| `jean-browser/src/lib.rs` | `port.map_or(true, …)` | `is_none_or`, which says what it means |
| `jean-browser/src/lib.rs` | a `loop` that could only ever run once, in `read_endpoint` | deleted — `recv_timeout` already holds the whole budget, and the pump only forwards lines that parse as an endpoint, so a timeout and a closed stderr are the same answer |
| `jean-acp/src/acp/wire_tests.rs` | `drive` takes 8 arguments | allowed, bare, matching the seven sites the tree already has for the same lint |
| `jean-core/src/browser.rs` ×4 | `map_err` that only logged, `&[x.clone()]` to build a slice, and two `.clone()`s on `TaskState`, which is `Copy` | fixed; the slice is `std::slice::from_ref(&request.url)` |
| `jean-core/src/mcp.rs` ×5 | same `map_err`, two `contains()`/borrow slips, and two wide private helpers (`run_session` 8, `settle` 9) | the three real ones fixed; the two signatures allowed, because `Recording` already groups what travels together and a second struct of leftovers would move code without moving the problem |
| `jean-core/tests/*` ×4, `jean-cli/src/phase3.rs` ×1 | an unused import, `== false` twice, `.into_iter()` inside `extend`, and a `format!` inside a `format!` | all fixed. The last one moved `"steps": [{}]` into the outer template, which is why §46.5 re-runs the command rather than trusting the edit |

Nothing was silenced with `#[allow]` that the tree did not already allow in the same shape, and
`clippy.toml` / lint configuration was not touched.

### 46.3 The stripped-environment test: what was measured

The first full-suite run of this batch — 562 passed, 1 failed — failed in `jean-process`, which Phase 3 has
not touched. It is recorded here rather than waved away because the *instability* is a fact about this test
binary, the mechanism is not known, and the next reader will hit the failure again without either of those
pieces of context.

| Probe | Command | Result |
|---|---|---|
| Reproduce | `target\dev.cmd test -p jean-process --lib -- --test-threads=1` | **116 passed, 1 failed** (`target/jp-full-1.log`). It also failed in the default parallel run — that is the `test --workspace --all-targets` result in `target/p3-test.log` that produced this batch's "562 passed, 1 failed". |
| Run it alone | same, with `--exact piped::tests::a_child_with_a_stripped_environment_is_refused_by_windows_rather_than_started_broken` | **passes in 0.00 s** — `Err(… The parameter is incorrect. (0x80070057))` (`target/jp-iso-1.log`). |
| Which module "causes" it | the full suite with `--skip <module>::` for each of the five modules that run before it | skipping `env::`, `output::` **or** `conpty::` makes it pass; skipping `cmdline::` or `error::` does not (`target/jp-skip-*.log`). |
| Is any of them the cause? | each candidate module's tests **plus** this test, nothing else | `conpty::` + it: **11 passed**. `env::`: 13. `output::`: 17. `cmdline::`: 19. All pass — none of them is sufficient. |
| Appending an unrelated test | add one test to the binary — a diagnostic that spawns the same child — and run all 118 | **green, twice, with two different diagnostic bodies** (`target/jp-diag-full.log`, `target/jp-diag-full2.log`): with the extra test in the binary, Windows **refused** the child here. The added test sorts *last*, so every call that runs before the assertion is unchanged. |

The direction of those results is worth stating exactly, because it is the reason the assertion was wrong
rather than merely unlucky. **Green means Windows refused the child; red means it started one.** The
117-test library starts it. The same library with one unrelated test appended — which sorts *after* this
one, so every call that runs beforehand is identical — refuses it. And a subset bisect can close with three
"necessary" modules, no sufficient one, and a result that is not monotone in the number of tests that ran
(`--skip cmdline::` leaves 98 and starts the child; `--skip output::` leaves 101 and refuses it).

**No mechanism was established for that, and this section does not claim one.** What the batch checked and
what it did not: the environment encoding, the pipe creation, the job object and the spawn arguments are
byte-identical in every row above — they are the same `spawn(&spec_with(…))` call — so the variation is in
the loader's answer, not in anything J.E.A.N. passes to it. Beyond that, nobody has isolated it, and a
future reader should not treat the table as a diagnosis. It is a demonstration that the property being
asserted is not a fact this binary can rely on, which is enough to justify changing the assertion and not
enough to explain Windows.

**A correction, recorded because this document is the project's only memory of it.** An earlier draft of
this section asserted a mechanism — that a `FreeConsole` somewhere in the suite detaches the test process
from its console, and that a console-less parent creates the child — and cited a probe, a pid and a log for
it. Checking those citations against the tree and `target/` before finalising showed that no `FreeConsole`
call exists anywhere in this repository (`grep -rn FreeConsole --include=*.rs crates apps`: no matches), that
the named log files were never written, and that the probe test is not in `piped.rs`. The paragraph was
deleted rather than hedged. The lesson is the one the brief already states and this project keeps needing:
a mechanism is a measurement, not an explanation that fits.

What is stable, and what the replacement test can therefore assert, is the two-sided observation captured
verbatim from the child during the probes:

```text
refused →  could not start C:\WINDOWS\System32\cmd.exe: The parameter is incorrect. (0x80070057)
started →  COMSPEC=C:\WINDOWS\System32\cmd.exe · JEAN_CANARY=leak-check · PATH=C:\WINDOWS\System32;C:\WINDOWS
           PATHEXT=.COM;.EXE;.BAT;.CMD · PROMPT=$P$G · SystemRoot=C:\WINDOWS
```

The second line is `cmd.exe /C set` printing **its own** environment: the five variables the supervisor
passed — the four of `standard_environment()` plus the canary — and `PROMPT`, which cmd invents for itself.
No `USERPROFILE`, no `TEMP`, nothing else from the process that launched it. That is the property J.E.A.N.
actually depends on, and it holds whichever way the loader feels about the empty block.

### 46.4 What the test says now

`crates/jean-process/src/piped.rs` now carries two tests where one brittle one stood:

* `a_stripped_environment_is_refused_or_hermetic_but_never_the_parents` — passes on either branch of
  Windows' choice, and **fails if an empty allowlist ever degrades into J.E.A.N.'s own environment**, which
  is the failure `win32::EMPTY_ENVIRONMENT_BLOCK` exists to prevent and which the old test could not see:
  it asserted a refusal, and a silent inheritance of the parent's environment is not a refusal.
* `a_worker_sees_the_environment_it_was_given_and_no_other` — the positive half, read out of the child's own
  `set`, with the canary proving the check would notice a variable that is really there.

The doc comment on the first states plainly that the refusal is the loader's discretion and was observed both
ways here, so nobody re-adds the stronger claim later. `crates/jean-process/src/conpty.rs`'s one-line comment
that asserted the same refusal now says *may* refuse. No production behaviour changed, and
`[delegation] workers.env_passthrough` defaults are untouched.

`jean-process` after the change: **118 lib + 20 integration passed, 0 failed** — three consecutive default
runs (`target/jp-all-1..3.log`), one `--test-threads=1` run (`target/jp-full-2.log`), the whole-workspace
run in §46.1, and a final `--lib` run after the doc-comment correction above (`target/jp-final.log`).

### 46.5 Disk for this batch, and the re-verification the lint fixes asked for

`(Get-PSDrive C).Free`, in bytes, at the points where a build was about to be trusted:

| Point | Free |
|---|---|
| Start of this continuation (after the operator removed `target/debug/incremental`) | 6,740,004,864 — 6.27 GiB |
| After the first whole-workspace test run | 4,782,034,944 — 4.45 GiB |
| After the final build + test + Clippy + one more browser run | 5,054,959,616 — 4.71 GiB |

One workspace test run costs roughly 1.8 GiB with `CARGO_INCREMENTAL=0`. The last row is *larger* than the
one before it, with nothing deleted by this batch in between, which is the same unexplained recovery R-26's
**Fact** row recorded at a much worse ratio; the number is reported because it was measured, not because it
is stable.

The Clippy fixes touched the code the demonstration runs through (`jean-core/src/browser.rs`,
`crates/jean-browser/src/lib.rs`, and `browser_json` in the CLI, whose `[{}]` template change has no test
behind it). So the workflow was executed again rather than assumed:

```text
JEAN_DATA_DIR=…/target/browser-demo/data jean browser run "file:///…/fixture.html" \
  --inspect "#status" --type "#name=hello from jean" --click "#send" \
  --expect "#status=sent: hello from jean" --json
```

`[exit] 0`, and the stdout parses as JSON: `verdict=proven`, `state=succeeded`,
`after="sent: hello from jean"`, `pid=15136`, `screenshot` present, 8 steps. Containment, re-checked after
that run: `Get-CimInstance Win32_Process -Filter "Name='chrome.exe'"` filtered on the demo profile returns
**0** JEAN-owned processes, with **11** of the operator's own `chrome.exe` still running. (§45 counted 12 of
the operator's at the time; that number is theirs to change, and this batch has not touched those processes
in either direction.)

**Two hazards found while measuring, both recorded because both can produce a false report.**
First, the background-task notification in this environment reports the *wrapper's* exit status, so a
`cargo test` still three suites from finishing arrives as "completed (exit code 0)"; every number above was
taken from the log's own `[exit]` line after the file stopped growing. Second, reading a log while cargo is
still appending to it can return **content from an earlier, unrelated run** — at 12:16 this session one
`tail` showed `Running tests\trace.rs` and `error: test failed, caused by the cargo binary binary "jean"`
for a target that does not exist anywhere in this repository (`find . -name trace.rs` confirms it), in a run
that had not reached that suite. Neither check is trustworthy against a file that is still being written.

## 47. Outstanding at the end of this batch

Phase 3's own work is delivered and measured (§44, §45, §46). What is **not** proven, in the order that
unblocks the most:

1. **`cargo deny check` — still never run**, because `cargo-deny` is not installed and this batch may not
   install it (§33 has the exact commands). The debt grew with the phase and is stated plainly: the tree went
   from 276 to **337 lock entries**, and the 61 that `headless_chrome` brought in have had **no licence
   audit and no size measurement**. The `sysinfo → ntapi → winapi` policy conflict is unresolved and
   `deny.toml` has not been weakened to make the check look passable.
2. **Live vendor compatibility (R-28).** `opencode acp` and `codex exec` are installed here and J.E.A.N.
   has still never driven either. Every protocol claim in §44 is against a peer this repository wrote, and
   the word for that is *unverified*, not *supported*. A vendor session also spends the operator's quota and
   needs their own authorisation, which this batch did not have.
3. **The browser has been proven against one offline local fixture.** That is the workflow the phase owed —
   navigate, inspect, interact, verify — and it is not a claim about the web. Untested by construction here:
   any real or authenticated site, a page that needs JS to settle beyond the fixed `settle_delay`, a
   download, a dialog, a second tab, `[browser] headless = false` (the flag is implemented and was never
   run, so nobody has seen the window), and the Edge/Brave entries of `KNOWN_BROWSERS` — detection offers
   three paths and the demonstration used the Chrome one. `max_page_bytes` is unit-tested, not exercised
   against a page large enough to need it.
4. **`jean model ping`** and the **seven Phase-0/1 manual checks (§21.1)** — carried forward, unchanged, from
   Phase 2.
5. **Concurrent workers.** `max_parallel` is validated to `1` and `jean agent` says so, because cancellation
   has one active task per process (D-028). Parallel delegation needs a cancellation router that does not
   exist.
6. **The ACP trace against a secret.** `WireTrace` is bounded and body-free by test (§44.2), but it has never
   been run against a prompt that actually contains a credential, so "the trace cannot leak the delegation"
   rests on the design and the two bounds tests rather than on a captured transcript.
7. **Disk (R-26)** is a fact about the machine, not the code: 4.71 GiB free at the last measurement, with the
   volume moving on its own between measurements.

**Phase 4 (desktop vision, UIA, voice, tray) has not been started**, per the brief, and nothing in this
batch added a Telegram path, a screen-reading capability, a network listener, or a machine-wide setting.

## 48. The second reconciliation pass (2026-11-06, after the checks)

§42 reconciled the planning documents against a tree that had no browser and an open R-27. The checks in
§46.1 changed that, so every Phase-3 statement in `PLANNING/` was re-read again — and three of them turned
out to be **false by measurement rather than merely stale**, which is the only kind of documentation bug
worth a section of its own.

| Document | Was | Now |
|---|---|---|
| `05-ROADMAP.md` Phase 3 | Crates listed as `jean-acp`, `jean-mcp`; **Browser integration `[ ]` — "decision recorded, code not written … No browser has been launched on this machine by this project"**; the cancellation line carried the §38 defect as open; the exit criteria blamed the gap on "the two ACP settlement defects above"; evidence range `§36–§41`. | `jean-browser` added. Browser integration is `[x]` with the four demonstrated paths, the profile/loopback/origin gates, and an explicit *not claimed* list. The cancellation line records the root cause and the 221 s → 3.98 s number. The exit criteria now blame the remaining gap on the one thing it actually is — no vendor agent (R-28) — and say so in the same breath as the live-fixture run. Range is `§36–§47`. |
| `06-RISKS.md` R-27 | 🔴 **OPEN**, with a "do not mark closed until an ignored-test-free `delegation_live` run is in the transcript" row. | Headed **CLOSED 2026-11-06**, with the `Drop`-joins-a-parked-thread cause, the fix, and the three evidence tiers (scripted / real pipes / external fixture) in a **Closed** row — and a following **What this does not close** row, because closing R-27 must not read as closing R-28. |
| `06-RISKS.md` R-05 | "Decided, and still not delivered… `headless_chrome` is absent from `Cargo.lock`… its dependency closure has never been fetched, compiled, licence-audited or sized." | **Delivered**, with what was measured rather than intended: the `ProfileNotEmpty` gate, the endpoint check before the dial, the four paths, the 0-against-11 process containment count — and the one quarter of that sentence that is still true: **the 61 new entries have had no licence audit and no size measurement**, because `cargo deny check` is still unrunnable. |
| `06-RISKS.md` R-26 | "Realised… three Phase-3 items are blocked on it and nothing else." | The three blocked items are named as done, with the byte-exact free-space figures from §46.5 and the cost of one whole-workspace run (~1.8 GiB at `CARGO_INCREMENTAL=0`). The risk stays 🟠 on purpose: the volume still has no slack of its own, and it moved between measurements with this batch doing nothing. |
| `06-RISKS.md` register + gating sentence | R-26 "browser, full suite and Clippy all deferred"; R-27 **OPEN**; R-28 pointing at "the `#[ignore]`d live test"; "🔴 risks that gate a phase … four of the seven (R-14, R-15, R-21, R-27)". | All four updated to what the tree shows. The gating list is **six** risks now that R-27 closed, and the recount is stated rather than quietly patched — including the sentence a release note must not drop: every protocol claim still rests on a peer this repository wrote. |
| `07-DECISIONS.md` D-046 | Register and status both read "**implementation not started; blocked on free disk**", and a consequence said "**Nothing is usable yet.** `[browser]` parses and validates… no module reads either." | Register and status say built and demonstrated, §45/§46 named. The false consequence is struck **in place** with the correction underneath — `[browser]` gates the run and `BrowserAction` is emitted per step (`crates/jean-core/src/browser.rs:256`, `:525-533`) — while the part that survives is kept visible: the fetch happened, the licence audit did not. The "Chrome-shaped answer" consequence gained the honest detail that detection offers three engines and only Chrome has been run. |
| `03-DEPENDENCIES.md` | `headless_chrome`: "**ADOPT — declared, not yet consumed**", "with no TLS backend requested — the endpoint is a loopback pipe, not a network", "`Cargo.lock` contains no `headless_chrome` and no `chrome-devtools-protocol`", "blocked on free disk". | Consumed, with the measurements: lock line 927, **276 → 337 entries**, `tungstenite 0.29.0` as its one runtime addition **resolved without a TLS backend** (verified through its whole normal-dependency subtree with `cargo tree --workspace --locked -e normal`), `chrome-devtools-protocol` genuinely absent because `offline` ships the schema, and a build-time path `auto_generate_cdp → ureq → webpki-roots` that the old sentence's blanket "no TLS backend requested" glossed over. Plus the two absences that are design, not omission: the `fetch` feature is off, and `wss://` cannot be reached even in principle — `OwnedBrowser` refuses any endpoint that is not `ws://` on loopback before a socket exists. |
| `04-SETTINGS.md` | Twice asserted that `[browser]` "is still listed in `DEFERRED_SECTIONS`… that warning is true, and is deliberately kept", and used it as the contrast case for unknown keys in `[mcp]`. | Both corrected against `crates/jean-protocol/src/config.rs:33`, which now reads `["voice", "vision", "agents", "telemetry"]`: `[browser]` is a section this build reads end to end, so the warning would have been a lie, and the `[mcp]` contrast case is rewritten to say so rather than being deleted. |

**Method, because the last reader asked.** No document line was changed because a build went green; each
was changed against a measurement taken for the purpose — `(Get-PSDrive C).Free` in bytes, `grep -c
'^name = ' Cargo.lock`, `cargo tree -i native-tls -i tungstenite -i webpki-roots`, `grep -an
'DEFERRED_SECTIONS'`, `Get-CimInstance Win32_Process`. Two of the claims that fell out (the TLS sentence and
the deferred-section sentence) were written confidently in the previous pass, which is the argument for
re-measuring rather than re-reading. Nothing was installed, authenticated, committed, pushed, published or
deleted, in the tree or on the machine.

---

## 49. Phase 4 is built: the desktop boundary, the seven verbs, and what each one answers

§47's last line — "**Phase 4 (desktop vision, UIA, voice, tray) has not been started**" — is superseded by
this section for the desktop half. Voice and tray have not been started, and nothing in this batch opened a
network listener.

> **Read §55 with this section.** §49 describes the phase as it was first built, and one of its paragraphs —
> the isolation model, in §49.1 — was replaced by the correction and now carries the correction's wording with
> a note saying what it used to say. The verbs, the boundary, the four swappable surfaces and the diagnostics
> are unchanged.

### 49.1 The crate, and the four things that can be swapped

`crates/jean-windows` is new: **8,318 lines** across nine modules plus a fixture binary. Everything native
lives behind one of four traits, and `jean-core` names no Win32 type at all:

| Trait | Question it answers | Implementation shipped | What replacing it costs |
|---|---|---|---|
| `accessibility::AccessibilityBackend` | five questions about a window's UIA tree, its controls and their state | `uia::UiaBackend` over `uiautomation` **0.25.1**, pinned `=0.25.1` | one file — `crates/jean-windows/src/uia.rs` (789 lines) is the only module that names a UIA type |
| `capture::CaptureBackend` | "give me this window's pixels, once" | `print_window`, then `screen_bitblt` as the recovery path, over the `windows` crate directly | the backend enum and its two draw functions |
| `input::InputBackend` | "put this event list on the input queue, after re-verifying the target" | `SendInput` with `MOUSEEVENTF_ABSOLUTE \| MOUSEEVENTF_VIRTUALDESK`, normalised against `SM_XVIRTUALSCREEN` | one `send` impl |
| `ocr::TextExtractor` | "read text out of this frame" | **`NoOcr` only** (`ocr.rs:284`, the `[vision] ocr_backend = "none"` case) — it returns `InvalidSetting` and reports `BackendStatus::Unavailable`, and `SidecarProtocol` ships the line-translation half with no transport | an extractor that produces a token; the contract, the bounds and the translation tests already exist |

Three things are load-bearing across all four. `worker.rs` is the supervised child process the brief's
isolation requirement turns into code — a spawn through `jean_process::piped`, a startup bound, a request
bound, a shutdown bound and a job object that ends the whole tree (D-051); `bounded.rs` (348 lines) is the
same shape kept for the one path that is not provider work, input;
`identity.rs` (1,012 lines) is the single source of "which window is this, who owns it, what does it allow",
including the integrity read R-14 needs; and every module establishes DPI awareness before it computes a
number, because the awareness question cannot be answered after the fact.

**The isolation is a process, not a thread name.** *(This paragraph described the throwaway-thread design
and was superseded on 2026-11-06 by §55; it is corrected here rather than left standing.)* A UIA or capture
call now runs in `jean-desktop-worker.exe`, spawned through the same supervision every other J.E.A.N. child
gets, and the parent talks to it over one JSON line in and one out. When the deadline fires, the parent
closes the worker's **job object**, which terminates the process and everything it had started — there is no
thread left parked, no stack retained and no COM apartment carried by a long-running core, because the unit
that was abandoned is the unit that got killed. Nothing is shared across the boundary: no element, no
apartment, no `UIAutomation` instance, so walking away cannot corrupt a later call. A missing worker binary
is a **refusal**, not an in-process fallback — the reason the isolation exists is that the in-process answer
was a thread nobody could retrieve. `jean desktop status` prints `desktop_workers_alive 0 of 4`, where the
4 is `[desktop] max_concurrent_workers` counted against live worker *processes* now, and cases T22 and T23
measure the timeout and the still-answering-unrelated-windows halves on one running desktop. What the change
costs is measured rather than asserted: ~3 ms of supervision, ~56 ms for a handshake, ~150 ms for an 18-control
UIA walk and ~88 ms for a PrintWindow capture (§55.2).

### 49.2 The fifth accessibility question, added because the fourth was hiding a bug

`AccessibilityBackend` shipped as four questions — read the tree, resolve a locator, probe capabilities, act
on a pattern — and the act path carries its own witness read inside the pattern call, which is what makes
its before/after pair atomic. Injected input has no pattern call to carry anything inside, so `jean-core`
read the witness out of the **tree** instead, and a tree listing reports `name`, `kind`, `enabled` and
`focus` and never a `Value`. The consequence, measured rather than reasoned (§51.3): `--expect "value=…"`
could not hold on the input path for any control in any application, so typing was never provable.

The fix is the fifth question — `read_state(window, locator)` returning `StateRead::Found / Absent /
Ambiguous` — which asks a control what its own patterns say, with the password redaction applied inside the
reader rather than after it, and is bounded on the same worker as everything else. Both paths now emit one
digest grammar, which is what the brief's "one `--expect` language" actually requires.
`crates/jean-windows/tests/native_desktop.rs` carries `a_state_read_reports_the_controls_own_patterns_without_acting`
against the real provider, including the absent and ambiguous arms, because those two are answers and not
errors.

### 49.3 What the operator calls

Seven verbs (`apps/jean-cli/src/phase4.rs`, 1,253 lines), in the order that is itself the design — see, then
understand, then touch:

| Command | Reads or writes | Runs with `[desktop] enabled = false` | Leaves a task row and an audit line |
|---|---|---|---|
| `jean desktop status` | neither — describes *this* process: bounds, awareness, integrity, session, outstanding workers | yes | no |
| `jean desktop windows [--json]` | read | yes | no |
| `jean desktop inspect` + a window selector | read | yes | no |
| `jean desktop diagnose` + a selector, or `--all --limit N` | read | yes | no |
| `jean desktop act` + a control + one of eight pattern actions, `--witness-id`/`--witness-name`, `--expect`, `--capture` | **write** | no — exit 1 | yes |
| `jean desktop input` + one of seven inputs, aimed at a control, a point or a window, `--witness`, `--expect`, `--capture` | **write** | no — exit 1, and it also needs `allow_synthetic_input` | yes |
| `jean desktop capture [--out path]` | **write** (pixels, once) | no | yes |

Exactly one action per `act` and exactly one input per `input` are enforced by the parser, not by
convention: a command that could take two actions cannot say which one its witness proves. A window
selector that matches nothing is an error, and one that matches several is a refusal, because "the first
window named notepad" is the kind of guess that types into somebody else's application.

The exit vocabulary for the three writing verbs is this phase's addition to the table, and it is not the
usual one:

| Code | Meaning |
|---|---|
| 0 | The check the operator stated is what the desktop now shows |
| 1 | The desktop does not show it — the mechanism refused, nothing changed, or (as in every other command) a configuration gate is off |
| 3 | Something was done and nothing proves the goal, usually because no `--expect` was stated to prove it by |
| 4 | The outcome is **uncertain**: the action may have landed, and this build will not send it again |
| 5 | Refused by policy, or by the answer at a prompt nobody could give |

Code 4 does not exist in Phase 0–3 and had to here: a provider that accepts a press and then stops answering
leaves a click nobody can confirm, and a script that retries on *failure* would retry it. An uncertain
outcome is therefore not a failure code — it is the hand-off to a person.

### 49.4 Authority, and the two gates that are not the policy engine

`[desktop] enabled` gates actions and `[desktop] allow_synthetic_input` gates keystrokes. Both return a
`CoreError::Intent` — an operator-facing error, exit 1 — because "you have not told me I may" is a fact
about the configuration, not a decision about this window. A **policy** refusal is different in kind: it is
returned as a report with `"refused": true` and exit 5, so the refusal is recorded, readable in JSON, and
distinct from a crash. Reads sit outside all of it — `crates/jean-core/src/desktop.rs` builds no `Operation`
for `windows`, `inspect` or `diagnose`, creates no task row, and logs metadata only. That is measured, not
asserted: the suite's `off` configuration directory has **no `audit.jsonl` at all** after six read cases and
two gated attempts ran against it.

`desktop.act` and `desktop.input` are deliberately **out of the shipped `require_approval` list**, and the
reason is recorded where both lists live (`default_require_approval` in
`crates/jean-protocol/src/config.rs`, and `config/defaults.toml`): `ConsolePrompt` answers
`Denied(Unanswerable)` when stdin is not a terminal, so adding them to the default list would close the
entire actuator to every script and every delegated agent on this machine while still prompting a human at a
desk. Case T27 measures the other half — with an approval required and no terminal to answer it, the command
refuses, reports `"refused": true`, exits 5, and leaves the record behind.

### 49.5 The rules the brief stated and the code now holds

- **Semantic first.** Application API → UIA pattern → verified keyboard/mouse → pixels. `input` is behind its
  own switch precisely so that the fallback stays a decision, and `jean desktop diagnose` prints the rung it
  recommends (`next  UseAccessibility` and its neighbours) rather than leaving the ladder implicit.
- **Revalidate, never reuse.** `resolve_window` re-reads identity before every action and refuses ambiguity
  and staleness; `input::Target::verify` re-checks the handle, the foreground and the window's *current*
  rectangle inside the worker immediately before `SendInput`; focus is never stolen to make a check pass.
- **A black capture is not a dead window.** Capture exclusion is one `Observation` among six — `available`,
  `unsupported`, `permission_denied`, `explicit_protection`, `temporarily_unavailable`, `unknown` — and it
  gates only `capture`. Case T21 operates a window that has declared `SetWindowDisplayAffinity` exclusion
  through its own UIA patterns and proves the result, and T05/T08 show the same window answering the tree.
- **Nothing continuous.** No timer, no polling loop, no frame retained without a decision. The default
  retention is `discard_after_task`, which writes no file. *(As first written this sentence continued "… the
  only retention promise this build can honour without a cleaner"; §55.2 delivered the cleaner, so the
  default is now the privacy choice rather than the only keepable one, and `keep_24h` expires what it
  claims to expire.)*
- **Private text stays out of the records.** Names are bounded at `MAX_NAME_CHARS`, values are redacted in
  the reader for password-flagged controls, `--set-value` and `--type` are recorded as `set_value(13 chars)`
  and `type 14 chars`, and — after §51.5 — so is `--expect`.

## 50. The thirty-task desktop suite, and what each row actually measured

> **Re-run on the corrected build on 2026-11-06 at 23:02, with the same 30/30 outcome**, on the worker,
> retention and input changes described in §55 — `target/p4v-suite-1.log`, run directory
> `target/p4demo/suite/run-20261106-230225`. §50.3's thirty rows are unchanged in what they ask, and §55.4
> gives the exit-code classes of this later run — 19 / 7 / 1 / 1 / 1 across the same five verdict shapes.
> What changed is *why* T22 and T23 pass: a bounded call now ends a process rather than abandoning a thread.

The roadmap's Phase-4 benchmark line asks for a self-defined 30-task Windows suite with a published pass
rate. That is what this is, and the first thing honest about it is what kind of suite it is **not**: the
tasks are single-command desktop operations with a stated check, written so a row can be re-run by hand from
the transcript. They are not `microsoft/WindowsAgentArena` or `xlang-ai/OSWorld` tasks, they are not scored
by an environment oracle, and nothing here is comparable to the 21.3% OSWorld-V2 number the roadmap quotes
for calibration. The pass rate below means exactly "of thirty operations this build was asked to perform on
this desktop, thirty reported the truth about what happened".

### 50.1 Where it ran

Measured at 18:47 local on 2026-11-06, from the suite's own transcript rather than from memory:

| Fact | Value |
|---|---|
| Machine / OS | `4R`, Microsoft Windows [Version **10.0.26300.9818**] |
| Monitors, from `jean desktop windows` itself | **1920x1080+0+0 at 125%** (primary, 16 top-level windows on it) and **1366x768+1920+306 at 100%** (9 windows) |
| DPI awareness of the runner | `per-monitor-aware-v2, set by this process` |
| Integrity of the runner | `Medium (S-1-16-8192)` |
| Session | `Available`, input desktop `Default`, console session 1 |
| Desktops enumerated | 25 top-level windows an operator would call a window |
| Toolchain | rustc/cargo 1.99.1, MSVC 14.44.35207, `--locked`, `CARGO_INCREMENTAL=0` in the build child only |
| Free space on `C:` before the runs | 3,637,952,512 bytes |

The mixed-DPI dual-monitor condition the roadmap's exit criteria name is **this machine's actual
configuration** — a 125% primary and a 100% secondary already present. No display setting was changed to
manufacture it, and the fixture places a window at `2100,400` to sit it on the second monitor.

### 50.2 What the suite is

`target/p4demo/suite.sh` (a scratch harness under `target/`, not shipped code) starts six fixtures — two
normal, one `SetWindowDisplayAffinity`-excluded, one dark, one whose second window's thread never pumps
messages, and one on the secondary monitor — and runs **thirty numbered tasks** against them from five
separate configuration directories (`on`, `keep`, `off`, `noinput`, `approve`), each with its own
`jean.toml` and its own `JEAN_DATA_DIR`, so no case edits a shipped default and no case touches the
operator's `%APPDATA%\JEAN`.

A row **passes when the report is the honest report for what the desktop did**, which is not the same as a
green verdict: T06, T10, T18, T20, T25, T26, T27 and T28 all pass on a refusal, a not-proven, or a gate
error. Each row records the command, its exit code, and the pattern its output had to contain; the full
transcript of every command and its output is kept beside the results file.

- Results: `target/p4demo/suite/run-20261106-184746/results.tsv`
- Transcript: `target/p4demo/suite/run-20261106-184746/transcript.log` (every command, every report)
- Kept frame: `…/run-20261106-184746/caps/jean-55514782-1793971162138.bmp`, **691,254 bytes** = 54-byte
  header + 360 rows × 1,920 bytes, i.e. the frame the report said it wrote, written where it said. That run
  kept it because the build of that day could not have removed it; on the corrected tree the same frame is
  claimed in `jean-captures.jsonl` when it is written and expires twenty-four hours after that claim, which
  is the only deletion this mechanism is permitted to perform and is described in §55.2.

**30 passed · 0 failed · 0 skipped.**

### 50.3 The thirty rows

| # | Task | Result |
|---|---|---|
| T01 | the desktop overview answers, and creates no task row | passed — exit 0 |
| T02 | every top-level window is enumerable with pid, dpi and integrity | passed — exit 0 |
| T03 | the fixture is attributable to the process that started it | passed — `"pid": 33416` |
| T04 | the accessible tree carries AutomationIds and pattern names | passed — `Populated` |
| T05 | a capture-excluded window still answers the accessibility tree | passed — 18 controls |
| T06 | a handle that is not a window is refused, not guessed at | passed — refused, exit 1 |
| T07 | diagnose reports per-capability availability across the desktop | passed — `workable` |
| T08 | an explicit capture exclusion is named as itself | passed — `explicit_protection` |
| T09 | a normal window yields a frame the classifier calls usable | passed — `print_window` |
| T10 | a capture-excluded window refuses capture and says why | passed — refused, exit 1 |
| T11 | a dark window is measured, not assumed to be protected | passed — `luma=` with `uniform=false` |
| T12 | `discard_after_task` keeps its promise and writes nothing | passed — no file |
| T13 | `keep_until_manual` writes the frame it says it kept | passed — file exists, 691,254 bytes |
| T14 | invoke is proven by the status label it changes | passed — `clicked:1`, exit 0 |
| T15 | a multi-word value is set and read back exactly | passed — `value=typed by jean` |
| T16 | toggle moves between two named states and the check says so | passed — `toggle=On` |
| T17 | SelectionItem selects and the readback agrees | passed — `selected=true` |
| T18 | a delivered action is still **not** proven against a check the desktop does not meet | passed — `not_proven`, exit 1 |
| T19 | a witness the action destroyed reports unverifiable, not unchanged | passed — exit 3 |
| T20 | an ambiguous name is refused; picking one is not a tie-break | passed — `2 controls match name=Continue` |
| T21 | an excluded window is still operated through its own controls | passed — proven, exit 0 |
| T22 | a provider that never answers is bounded and recorded as uncertain | passed — exit 4, `will not be repeated` |
| T23 | a blocked desktop worker does not stop unrelated windows answering | passed — listing answered while the worker was blocked |
| T24 | a window on the 100%-DPI secondary monitor is worked by locator, not by saved coordinates | passed — proven, exit 0 |
| T25 | input aimed at a window that is not in front is refused before `SendInput` | passed — refused, exit 1 |
| T26 | synthetic input without its own switch is an error, not a silent no | passed — exit 1 |
| T27 | an approval nobody could answer refuses, and the record says so | passed — exit 5, `"refused": true` |
| T28 | desktop actions switched off are an intent error, not a failed action | passed — exit 1 |
| T29 | the work is recorded, and none of its text reaches the audit trail | passed — 92 audit rows, 40 desktop task rows, **0 files containing the typed text** |
| T30 | a typed batch lands as the characters that were asked for | passed — proven against the `Value` pattern, exit 0 |

Two rows deserve their fine print. **T29** counts rows in a data directory that persists between suite runs
(a new `run-<timestamp>` is created for evidence; the five config directories are reused), so 92 and 40 are
cumulative, not one-run numbers; the zero that matters is the leak count. **T30** is the one row the machine
can veto — focus belongs to the operator, and Brave taking the foreground between two commands makes it a
refusal rather than a pass. In the earlier runs it *was* refused, and the harness is written so that a
foreground refusal on that row is reported **skipped with the window that was actually in front**, never
passed. In this run the fixture held focus and the batch was proven.

### 50.4 How it got to thirty, because the first four runs did not get there

The suite was run five times. The failures were in the harness and the configuration files, not in the
desktop code, and each one is named because a reader who re-runs this will meet the same traps:

| Run | Passed | What the failures were |
|---|---|---|
| 1 | 24 / 30 | `desktop status` has no `--json` (clap refused, exit 2); the `keep` case ran in the wrong config directory, as did the `noinput`, `approve` and `off` cases; the "pattern answered but state did not change" case was aimed at a combo box whose expansion is not deterministic. |
| 2 | 29 / 30 | the same `keep` directory error, plus `retention = "keep"` — which is not one of the three values the validator accepts, and the command said so and exited 1 rather than defaulting. |
| 3 | 30 / 30 | first clean pass. One thing it exposed: `capture_dir` given as a bash-style `/c/…` is not absolute to Rust, so the kept frame landed under the temp `JEAN\captures` directory. Disclosed rather than quietly re-run, and the file was left where it was. |
| 4 | 29 / 30 | "fixed" by writing a real `C:\Users\…` into a TOML basic string, which is an escape error the parser refuses loudly (`too few unicode value digits`). A configuration error is still a refusal the build got right and the harness got wrong. |
| 5 | **30 / 30** | forward slashes, which are absolute to Rust, legal in TOML, and accepted by Windows. The kept frame is inside the run directory. |

Between runs 1 and 5 the *product* changed twice, both of them defects the suite itself surfaced (§51.5 and
the retention vocabulary), and the suite changed five times.

### 50.5 What the thirty rows do not cover, and were never going to

These are **not** counted anywhere above, and the list is the deliverable rather than an appendix:

| Case | Status here | Why |
|---|---|---|
| 150% and 200% scaling | **not exercised** | this machine has two monitors at 125% and 100%. The roadmap's test matrix names 100/125/150/200; two of the four are the reality here and two are not, and the display configuration was not changed to pretend otherwise. |
| A locked workstation, a second interactive session, or a remote/console-switched desktop | **not exercised** | the `session` capability is measured and reported (`Available`, input desktop `Default`, console session 1); the other states would mean leaving the operator's session, which is theirs, not this batch's. |
| An elevated (High-integrity) target | **measured, not operated** | the listing shows Taskmgr at High and a Low-integrity `RtkUWP` window; the integrity read that makes the blocker explicit works, and acting on an elevated window was not attempted — that needs a token this build does not have and a decision the brief did not give. |
| Telegram, live | **pending — see §52** | measured: the installed client is **Unigram 13.0.1.0**, a packaged app at `C:\Program Files\WindowsApps\38833FF26BA1D.UnigramPreview_13.0.1.0_x64__g9c9v27vpyspw\Telegram.exe` (pid 14640, with its `Telegram.Stub.exe` broker at pid 27628), and **neither pid has a visible top-level window right now**: `jean desktop diagnose --pid 14640` says so and exits 1. Nothing was reproduced, so nothing about its capture or UIA behaviour is claimed. |
| OCR and visual grounding at runtime | **unverified by design** | §52; no interpreter, model weight or paid API was introduced. |
| A real third-party application under automation | **not attempted** | every scored action is against this repository's own fixture. The brief asked for a harmless fixture with verifiable state changes, and the honest limit of "operates the desktop" from this batch is "operates a Win32 window it does not own, through the same APIs a third-party app exposes". |

### 50.6 The live evidence beyond the thirty rows

The suite is the scored artifact; three earlier transcripts are the ones that found the defects, and they
are kept because the defects are the more interesting result:

- `target/p4demo/probe.log` — 24 commands across six fixture modes with `--json` reports. This is where the
  combo box's `ExpandCollapse` call answered without moving `expanded` off `Collapsed`, where the
  `print_window` backend was measured rendering child controls into a *dark* fixture's frame (`luma=72
  uniform=false`, i.e. not a black capture after all), and where the multi-word expectation first read
  `not_proven`.
- `target/p4demo/input-demo.log` — the first injected click proven end to end (`name=idle` →
  `name=clicked:1`, exit 0) and the first typed batch whose witness digest exposed that text was arriving
  as key codes rather than characters.
- `target/p4demo/tight-type.json` and `tight-type2.json` — before and after the encoding fix: the first
  shows `value= typed by jeanready` (14 characters, exact, inserted at the caret) and the second is a live
  foreground refusal naming Brave's window. Neither was smoothed into a pass: the first is `not_proven`
  because the expectation said the text would be appended, and the desktop disagreed.

## 51. Seven defects, all found by running the feature rather than by testing a fake

The brief for this batch warned that the previous one had contained invented explanations and nonexistent
probe citations, and asked for hypotheses to be separated from measurements. Every row below is the second
kind: each was visible only because a command was run against a real window and its report was read, and
each names the file, the observation, and what changed. The order is the order they were found in.

| # | What was wrong | How it showed up | What it became |
|---|---|---|---|
| 1 | **A multi-word expectation could never hold.** `expectation_holds` split the digest on whitespace, so `--expect "value=typed by jean"` compared `value=typed` against `value=typed by jean` and answered `not_proven`. | `--set-value "typed by jean" --expect "value=typed by jean"` on the fixture's edit, exit 1, while the report line printed the right thing. | The split is now on the *field names the producers emit* (`DIGEST_FIELDS`, `digest_fields`, `unquote`) with the whole-digest form as the escape hatch, and one pathological case stated in the doc comment rather than hidden: a value that itself contains ` enabled=` reads as two fields. The test is `the_expectation_is_a_whole_token_or_a_whole_value_and_never_a_substring`, which keeps the substring trap (`--expect 1` must not match `clicked:12`) and adds the phrase and last-field cases. |
| 2 | **Timeouts named "window 0".** `ProviderTimeout` and four `CaptureUnusable` sites carried a hard-coded `hwnd: 0`, so the report that exists to say *which* application misbehaved said `window 0`. | A hung-provider demonstration printed `the accessibility provider for window 0 did not answer within 5s (waited 5005ms)` — the exact `#[error(…)]` template in `crates/jean-windows/src/error.rs:39` — while the same command's own header knew the handle. | `accessibility::run`, `capture::run` / `draw_window` and `input::send` all take the `WindowId` they were given and pass its `hwnd` into the error. Verified live: the suite's T22 names the hung fixture's real handle, and the suite's expand timeout reports `window 33820020`. |
| 3 | **A false focus change refused every injected input.** `identity::foreground()` returned whatever `GetForegroundWindow` said, which on a real window is often a *child* — the fixture's edit control, whose text happened to be `not sent`. The input path compares it with the top-level the operator named, so it refused with "focus moved: expected `…` and found `hwnd=8392632`". | The first live `--click` demonstration, refused although nothing had moved. | `GetAncestor(hwnd, GA_ROOT)`, with the measured child-HWND behaviour recorded in the doc comment rather than the intent. After it: `T30` and `input-demo.log` are proven, and refusals now happen for a *real* reason rather than a phantom one — in the scored suite T25 refuses with the found window being another fixture (`and found hwnd=14419090 pid=36624 jean-desktop-fixture.exe`), and in the earlier scripted demonstration the found window was genuinely somebody else's: `input-demo2.log` records `and found hwnd=41684688 pid=21384 brave.exe`, with the keystroke never sent. Both are the case that was previously indistinguishable from the bug. (Corrected in §54.5's spirit: an earlier draft of this row credited Brave to T25 itself, which run 5's transcript does not say.) |
| 4 | **A refusal before `SendInput` was reported `unverifiable` (exit 3).** That code tells the operator "something may have happened", when the one thing known is that nothing was sent — and it costs a script the difference between looking and retrying. | Reading the verdict table against the message: `focus moved …` came back as unverifiable. | `input_drive` now separates `uncertain` from `refusal`, and `input_verdict` maps a refusal to `not_proven`/exit 1 while an incomplete batch stays `uncertain`/exit 4. Unit-tested by `an_input_that_was_never_sent_is_disproven_and_one_maybe_half_sent_is_uncertain`, and measured at T25, T26 and `tight-type2.json`. |
| 5 | **The input path's witness could not see a `Value`.** It read the tree, so its digest was `name=… kind=… enabled=… focus=…`, and `--expect "value=…"` was unsatisfiable for any control in any application — typing was, in the phase's own vocabulary, never provable. | Same session as defect 4: the typed batch came back `not_proven` with a witness line that contained no value at all. | The fifth accessibility question, `read_state` (§49.2), plus `witness_digest`, so both paths emit the same grammar and `focus=`/`kind=` survive as the listing half. `T30`, `a_state_read_reports_the_controls_own_patterns_without_acting`, `a_witness_digest_is_one_controls_state_in_the_words_both_paths_use`. |
| 6 | **Injected text delivered key codes, not characters.** `Input::Text` built its events with `key_event(VIRTUAL_KEY(unit), KEYEVENTF_UNICODE)` and `wScan: 0`, which is the mirror image of what that flag means: the code unit belongs in `wScan`, with `wVk` zero. | Directly above it: after defect 5 made the value readable, `injected text` arrived in the edit as **`9.*5354 5`** while the report said `SendInput accepted 26/26 events`. Decoding is exact — `i`=0x69 is numpad 9, `n`=0x6E is the decimal key, `j`=0x6A is multiply, `e`=0x65 is numpad 5, `t`=0x74 is **F5** and types nothing, which is why 13 characters became 9. | `unicode_event(unit, key_up)` puts the unit in `wScan` with `wVk(0)`, the wrong comment next to it is replaced by the measurement, and `typed_text_carries_its_characters_in_the_scan_field_and_names_no_key` asserts the encoding field by field — because `SendInput` counts events, not characters, and an acceptance count can never see this class of bug. Live after the fix: `tight-type.json` reads `value= typed by jeanready`, fourteen exact characters. |
| 7 | **`--expect` reached the audit trail verbatim.** `set_value(13 chars)` and `type 14 chars` were bounded; the expectation next to them in the same `arguments` list was not, so `--expect "value=<whatever the operator intends to see>"` wrote desired UI text into `audit.jsonl`. | Grepping the demo directory's own `audit.jsonl` after a redaction case: six lines carrying `typed by jean`. | `describe_expect` records `expect stated (34 chars)`. The report an operator reads still shows the expectation in full — the record is the thing that must not carry it. `a_recorded_expectation_says_one_was_stated_and_not_what_it_says` plus suite T29 (**0 files containing the typed text**, 92 audit rows and 40 desktop task rows present). |

One more change belongs to the same list, though it is a half-fix rather than a found bug: a
`MOUSEEVENTF_WHEEL` event was pushed with the coordinates but without
`MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, which is how the click path addresses
them — so a notch was read wherever the pointer happened to be, while the command had just verified a
different point. It now carries the same addressing (`a_wheel_notch_is_scaled_and_aimed_and_a_move_presses_nothing`),
which makes the *aiming* claim true. What it does **not** make true is which window Windows then routes the
notch to: the fixture has no scrollable control, so no state changes that would prove it, and that half is
in §52 rather than in the suite.

### 51.8 Three things this batch believed and the fixture disproved

Recording these matters more than recording the passes, because all three were written into plans before
anything was observed.

- **"The Send button renames itself to `clicked:{n}`."** It does not: `name=Send` before and after, and the
  thing that changes is the *status label*, control 1006. Every invoke demonstration names 1006 as its
  witness for that reason, and the earlier belief would have produced a `not_proven` for a perfectly
  successful click.
- **"An empty UIA tree proves a protection, so a Chromium app must have been launched with a flag."** What
  the fixture actually killed is the *equation* of those two things, and it did so by being observed:
  `TreeExplanation` has five answers (`Populated`, `NoElements`, `NoInteractiveContent { frames,
  class_name, looks_like_web_content }`, `Capped`, `Unreachable`), and the diagnostic for an empty tree says
  *"what the tree reported, not a claim about the application"* (`capability.rs:486`) instead of naming a
  protection. The web-content class goes one step further and says the honest version: the frame class is
  *"one an embedded browser surface uses, where the content tree is exposed only when the application is
  told to expose it"* — a conditional, with a unit test at `desktop.rs:2500`, and **no flag injected
  anywhere** (`grep -r accessibility_flag crates apps` → nothing). What is **not** disproved: no Chromium
  or Electron window's tree was read as a test target in this batch, so *"exposes nothing without the
  flag"* remains the library author's README claim and not a measurement of ours. The distinction is why
  this bullet is about the equation rather than about the flag.
- **"`--mode blank` gives a black capture."** It gives `luma=72 uniform=false`, i.e. a frame the classifier
  correctly calls content-bearing, because `PrintWindow` renders the child controls regardless of the class
  background. So the *unknown blank capture* class is real in the code and the vocabulary, and this fixture
  does not reproduce it — the honest place for that is §52, not a pass.

## 52. What is not verified, in the order that unblocks the most

Nothing below is counted as a pass anywhere in this document, and each row says which claim it stops.

| # | Not verified | Why not, measured | What would verify it |
|---|---|---|---|
| 1 | **OCR and visual grounding at runtime.** | `ocr::TextExtractor` ships one implementation, `NoOcr` (`crates/jean-windows/src/ocr.rs:284`), which refuses with `InvalidSetting("vision.ocr_backend is \"none\"; no text extraction is configured")` and reports `BackendStatus::Unavailable` — an honest stop, not a mock token. `SidecarProtocol` ships the request/reply **translation** and no transport: the spawn, supervision and timeout belong to `jean_core::desktop`, and its tests test the translation. No model server was started, no paid API was called, no weights were downloaded and no dependency was added: `ocrs`, `enigo`, `xcap` and `windows-capture` all have **`lock=0`** in `Cargo.lock` (`grep -c '^name = "…"'` → 0 for each), which is the measured form of "not adopted". What *is* on the machine, and is worth stating because it changes what the next batch would cost: the **sources** of `ocrs 0.13.1`, `enigo 0.6.1`, `xcap 0.9.8` and `windows-capture 2.0.1` are already in `~/.cargo/registry/src` (fetched when the roadmap's candidates were assessed), and C:\Python314 has `onnxruntime` and `numpy` importable while `rapidocr` and `cv2` are absent. So the Python sidecar needs one pip install plus its weights, not an interpreter. | An extraction that produces a token. The contract, the bounds and the fixture tests are delivered so that adding a backend is a config change rather than a design change. Do **not** read the fixture tests as inference: no model has produced a character on this machine. |
| 2 | **Telegram, live.** | The installed client is **Unigram 13.0.1.0** — a packaged app (`C:\Program Files\WindowsApps\38833FF26BA1D.UnigramPreview_13.0.1.0_x64__g9c9v27vpyspw\Telegram.exe`, pid 14640, broker `Telegram.Stub.exe` pid 27628) — and **neither pid has a visible top-level window**, so `jean desktop diagnose --pid 14640` refuses and exits 1. Nothing about its UIA exposure or its capture behaviour has been observed, and the black-screen case was therefore never reproduced. | The client with a window on screen: open it, `jean desktop diagnose --pid …`, and read the seven capability lines. That is the *first* measurement, not the fix; §4 of the plan's request — "operate these applications themselves" — needs the UIA read to say something actionable. No privacy setting was touched and no client was patched. |
| 3 | **150% and 200% scaling.** | This machine has 1920x1080@125% and 1366x768@100%. The roadmap's matrix names four scales; two are present and two are not, and the display configuration was not changed to manufacture the others. | A machine or session at those scales. The conversion code path is the same one the two real monitors already exercise, which is what makes row 3 a coverage gap and not a design unknown. |
| 4 | **A locked workstation, a disconnected session, or an elevated target.** | `session` reads `Available` / input desktop `Default` / console session 1, integrity reads `Medium (S-1-16-8192)` for this process, `High` for Taskmgr and `Low` for a `RtkUWP` window — so the *reads* that make those cases legible are verified, and the *behaviour* under them is not. | A second session or a scheduled elevated task, both of which belong to the operator. Acting on a High-integrity window would need a token this build does not have and was not given. |
| 5 | **A virtual desktop whose origin is negative, on real hardware — and `enigo`, which is no longer an acceptance item.** | Two halves, in the order they now matter. (a) *A monitor left of or above the primary*: this machine's secondary sits at a **positive** offset (`1366x768+1920+306`), so `SM_XVIRTUALSCREEN` is 0 and the negative-origin branch is arithmetic only — `to_normalised` is unit-tested at a `-1366` origin and at a `-768` top (`crates/jean-windows/src/input.rs`, `target/input-test-1.log`), and the same tests are what caught the zero-divisor panic on a desktop reporting no geometry. It is **not** proven on a real left-of-primary layout, and the display configuration was not changed to buy one. (b) *enigo*: never added to this workspace (`grep -c '^name = "enigo"' Cargo.lock` → **0**), never run, and its per-monitor-DPI behaviour is **no longer something this project owes a test for** — the roadmap's "test it explicitly" was an instruction for a build that planned to depend on it, and running a dependency nobody ships is research, not a gate. What that instruction stood in for is now measured on the shipped path: a **fixed coordinate** click on the 100 % secondary driven from a 125 % primary, at (2408,410) → normalised (48039,25041), witnessed by the fixture's own click counter, with two refused sends that left the counter unchanged (`a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on`, `target/input-demo-coordinate.log`). | (a) A second monitor physically placed left of or above the primary, then the same native test — or a virtual layout Windows reports that way. (b) Nothing, unless someone chooses to adopt enigo; `PLANNING/03-DEPENDENCIES.md`'s row now says *not adopted here, and not a gate*, and R-03 records the retirement instead of a promise. |
| 6 | **Which window a wheel notch is delivered to.** | §51's last row: the aim is verified, the delivery is not, because no fixture control scrolls. | A fixture with a scrollable list box and a state the readback can see. |
| 7 | ~~The whole-workspace test number for this final tree.~~ **Closed by §55.4.** | What this row said when it was written: `cargo build --workspace --all-targets --locked` failed with 13 `LNK1318` PDB errors and `cargo test -p jean-core --lib` then failed with `os error 112, There is not enough space on the disk` (§53.2), so no single aggregate test run existed for the tree as it then stood. What *did* exist, in order: `cargo check --workspace --all-targets --locked` **`[exit] 0`** (`target/p4-check2.log`) and `cargo clippy --workspace --all-targets -- -D warnings` **`[exit] 0`** (`target/p4-clippy-6.log`) — both of which compile **every** target, tests included — then the only later source edit was `describe_expect` in `jean-core`'s lib plus one unit test, re-verified by `clippy -p jean-core -p jean-cli --lib --bins` `[exit] 0` (`p4-clippy-7.log`) and by the lib test binary running green. No integration target references `jean_core::desktop`: grepping `crates/jean-core/tests` for "desktop" returns three prose lines in `notify.rs` about a user's desk. So the gap is **one aggregate run of tests**, not a compilation unknown. | **Closed in §55.4:** `cargo test --workspace --all-targets --locked -j 2` on the corrected tree is **`[exit] 0`, 36 targets, 911 passed, 0 failed, 1 ignored** (`target/p4v-test-7.log`). The reason the earlier aggregate could not be produced — a stale `target/` artifact shape that made `--all-targets` fail with `required to be available in rlib format` — is recorded there, along with the pagefile commit-limit failure that only appears at default parallelism. |
| 8 | **`cargo deny check`, third consecutive batch.** | `cargo-deny` is still not installed and this batch may not install it; the commands remain in §33. The tree went **337 → 340** lock entries (`grep -ac '^name = ' Cargo.lock`), and the Phase-4 additions `uiautomation 0.25.1` and `uiautomation_derive 0.7.9` were read for their licence from the registry cache — both `Apache-2.0`, from `~/.cargo/registry/src/*/uiautomation*/Cargo.toml` — which is a manual spot check, not the audit. | The audit, plus the unresolved `sysinfo → ntapi → winapi` policy conflict from Phase 1 that `deny.toml` has not been weakened to hide. |
| 9 | **Phase 0–3's seven manual checks (§21.1), `jean model ping`, live vendor agents (R-28), concurrent workers (D-042).** | Carried forward unchanged from §47. Nothing in this batch touched them, and nothing in this batch should be read as closing them. | As listed in §47. |

## 53. The batch's checks, and the volume that stopped the last one

> **Superseded on 2026-11-06 by §55.4.** Every row below is left as the record of what that pass measured,
> and it is no longer the current tree: `jean-windows` is now **28** library tests and **24** native desktop
> tests, the aggregate whole-workspace run exists (911 passed, 0 failed, 1 ignored), and §53.2's disk and
> link failures have been diagnosed as a stale-artifact shape and a commit-limit, not as open blockers.

Every command below ran in the VS 2022 x64 developer environment through `target\dev.cmd`, with
`CARGO_INCREMENTAL=0` exported in that child process only and `--locked` wherever the lock mattered. The
wrapper always returns 0; the number that matters is the log's own `[exit]` line, and the transcripts are
named.

### 53.1 Green on this tree

| Check | Command | Result |
|---|---|---|
| Type-check, everything | `target\dev.cmd check --workspace --all-targets --locked` | **`[exit] 0`** (`target/p4-check2.log`) — every crate, every test target, the fixture binary |
| Lints, everything | `target\dev.cmd clippy --workspace --all-targets --locked -- -D warnings` | **`[exit] 0`** (`target/p4-clippy-6.log`), on the **fifth** attempt. **Twenty** findings came out of the three lint runs before it — sixteen in `p4-clippy-2.log`, two in `-3`, two in `-5` — every one in Phase-4 code, each counted out of its own log with `grep -a '^error'` piped through `sort` and `uniq -c` so §53.4's numbers are arithmetic on the file; `p4-clippy-4.log` is not a lint run but two compile errors of my own making (`E0308`, `E0277` on an `unsigned_abs` result), which is what `-3` and `-5` were re-testing |
| Lints, after the last edit | `target\dev.cmd clippy -p jean-core -p jean-cli --lib --bins --locked -- -D warnings` | **`[exit] 0`** (`target/p4-clippy-7.log`) — the narrow form because §53.2 says why the wide one cannot link |
| Format | `target\dev.cmd fmt --all -- --check` | **`[exit] 0`** (`fmt-check-3.log`). The history is three logs: `fmt-check-1.log` recorded **126 diff points** (`grep -ac '^Diff in'`) against `[exit] 1`, one `cargo fmt --all` pass fixed them (`fmt-apply-1.log`, `[exit] 0`), and `fmt-check-2.log` came back `[exit] 1` with **one** diff — `jean-windows/src/input.rs:238`, introduced by the defect-6 Unicode edit after the apply pass. Fixed, then green. Grouped by file, all 126 sit in Phase-4 code — `jean-windows` throughout, `jean-core/src/desktop.rs`, `jean-protocol/src/config.rs`'s `[desktop]` validation, `jean-cli/src/phase4.rs` and the Phase-4 dispatch arm in `main.rs`. None is in a file this phase did not touch |
| Build, the shipped binary | `target\dev.cmd build -p jean-cli --locked` | **`[exit] 0`** (`p4-build-7.log`, run after every source change in this batch) |
| Tests, `jean-windows` library | `target\dev.cmd test -p jean-windows --lib --locked` | **9 passed, 0 failed** (`test-windows-9.log`) — six `bounded` worker tests and the three new `input` tests. The full `test -p jean-windows --locked` aggregate that once reported **6 lib + 20 native** (`test-windows-5.log`, `[exit] 0`) has not been re-run since the seventh test was added: it relinks `jean-desktop-fixture.exe`, and that link fails while a fixture is up (§53.2's other failure mode). **The 9 and the 21 below are two runs, not one**, and are reported that way rather than as a single green number |
| Tests, `jean-windows` native desktop suite | `target\dev.cmd test -p jean-windows --test native_desktop --locked` | **21 passed, 0 failed in 43.76 s** (`test-windows-6.log`) — these are the real-desktop tests, and the 43 s is 43 s of a human's desktop with six fixture windows on it. The predecessor run had 20 (`test-windows-5.log`) |
| Tests, `jean-core` library | `target\dev.cmd test -p jean-core --lib --locked` | **163 passed, 0 failed** (`test-core-lib-2.log`); the Phase-4 module filtered alone is **14 passed, 150 filtered out** (`p4-build-7.log`) — `test-core-desktop-4.log` is the same filter at its previous 13 |
| The desktop suite | `bash target/p4demo/suite.sh` | **30 passed, 0 failed, 0 skipped** (§50) |

Phase 3's whole-suite number was **824 passed across 32 suites**; the honest statement about this tree is
that its aggregate run does not exist yet (§53.2), not that it would pass.

### 53.2 The checks that did not run green, each with its own failure

```text
target\dev.cmd build --workspace --all-targets --locked              # target/p4-build-final.log, [exit] 101
  LINK : fatal error LNK1318: Unexpected PDB error; FILE_SYSTEM (3) '…\target\debug\deps\jean.pdb'
  … 13 of them, eight FILE_SYSTEM (3) and five LIMIT (12), and 13 matching `error: could not compile`
     lines: eleven `jean-core` targets (ten integration suites — redaction, notify, recovery, mcp,
     delegation, delegation_live, browser_live, mcp_live, intents, plans — plus `lib test`) and two
     `jean-cli` links of `bin "jean"` (the binary and its test harness). Counted with
     `grep -a 'LNK1318' target/p4-build-final.log` and `grep -ac 'could not compile'` on the same file

target\dev.cmd test -p jean-core --lib --locked                       # target/test-core-lib-3.log, [exit] 101
  error: failed to build archive at `…\target\debug\deps\libheadless_chrome-0977b7912f0a9b98.rlib`:
         There is not enough space on the disk. (os error 112)
```

The third failure mode is neither of those, and it is the reason §53.1 splits the `jean-windows` numbers:

```text
target\dev.cmd test -p jean-windows --locked                          # target/test-windows-2.log, [exit] 101
  error: failed to remove file `…\target\debug\jean-desktop-fixture.exe`
```

Cargo relinks the fixture binary before running the suite, and a fixture process started by an earlier
run still holds the file. That is a build-ordering fact about this workspace, not a defect in the desktop
code — the two commands in §53.1 that avoid the relink are both green — but it is also why the aggregate
`jean-windows` number has not been re-taken since the seventh native test and the three new input tests
landed. **This batch did not kill the operator's fixture processes to get that number**; the run is left
for the next one, with the fixtures stopped first.

The volume, in bytes, at the points where a build was about to be trusted:

| When | Free on `C:` |
|---|---|
| after the `jean-cli` rebuilds and the crate suites | 3,647,328,256 |
| at the start of the scored suite runs, `target` measured at 25 GB | 3,637,952,512 |
| immediately after the failed `--all-targets` build | **194,969,600** |
| after the narrow clippy | 193,695,744 |
| after the failed `--lib` test attempt, whose partial output cargo rolled back | 283,414,528 |
| after the documentation and measurement pass that follows §53.3 | 280,719,360 |
| at the time of writing | 277,938,176 (280,719,360 a few minutes earlier; the volume is shared with the operator's session and is still moving) |

`target\` went from 25 GB to **26.87 GB** across this window (`target\debug`, measured by summing each
subdirectory: `26.87`, of which `target\debug\deps` is **25.98 GB** and its **288 `.pdb` files are 13.18
GB**). Roughly 1.4 GB of the drop is not attributable to this batch's builds, and nobody has accounted for
it — the volume is shared with the operator's session, and that is the honest end of the arithmetic rather
than a search for somebody to blame.

Two things follow, and both are stated rather than acted on. First, **there is no incremental cache to
clean**: `CARGO_INCREMENTAL=0` has been set on every build in this project since the Phase-2 disk episode,
so the 26.87 GB is legitimate compiled output for 340 lock entries across 11 crates and 1 app (`Cargo.toml`
`members`, counted again at the time of writing), and
the only lever that frees gigabytes is `cargo clean` — which costs a full rebuild of a workspace that needs
several gigabytes of headroom to complete, and which this batch is told not to run. Second, **nothing was
deleted**: not the caches, not the five suite-run directories, not the `target/p4demo/*.log` evidence, not
the kept BMP at `…\suite\run-20261106-184746\caps\jean-55514782-1793971162138.bmp`, not the earlier one the
suite left in the temp captures directory before §50.4's fix. The free space is the operator's decision.

### 53.3 What the next batch has to run, in this order

```bash
# 1 · the aggregate number §53.1 does not have, once the volume has gigabytes of headroom
target\dev.cmd build --workspace --all-targets --locked      # expect [exit] 0
target\dev.cmd test  --workspace --all-targets --locked      # expect [exit] 0; the 30 Phase-4 native tests are in this count
# 2 · the audit that has been deferred for three batches (needs the binary, which needs disk too)
cargo install cargo-deny --locked && cargo deny check        # §33 has the exact shapes; deny.toml has NOT been weakened
# 3 · re-run the desktop suite on whatever the operator's desktop looks like; it re-measures its own
#     environment header, so a different monitor set changes the report rather than invalidating it
bash target/p4demo/suite.sh
```

### 53.4 The twenty lint findings, and what they say about the phase

| Where | Finding | Disposition |
|---|---|---|
| `jean-windows/src/accessibility.rs` | `large_enum_variant`: verbatim from the log, *"the entire enum is at least 296 bytes"* pointing at `StateRead`, with the fix suggestion naming `control: Control` → `control: Box<Control>` at the then-line 529 — the `Found` arm's named field, before that variant became the one boxed `ControlState` arm at `accessibility.rs:539` | `StateRead::Found` boxes a `ControlState { control, state }` instead. The allocation is per witness read, on a path that has already crossed a process boundary twice |
| `jean-windows/src/uia.rs` ×4 | `if let Some(p) = element.get_pattern::<X>().ok()` | `if let Ok(pattern) = element.get_pattern::<X>()` at `uia.rs:231/243/249/252` — the `.ok()` was turning an **error** into a *different* absence |
| `jean-windows/src/capture.rs` ×7 | `chunks_exact(4)` ×3, `&mut info` where the API takes `*const`, `if *seen == 0 { 0 } else { sum / *seen }`, `(w*3+3)/4*4`, and a pad loop that pushed one zero at a time | `as_chunks::<4>()`, `&info`, `checked_div().unwrap_or(0)`, `div_ceil(4)`, and `out.resize(start_row + row_bytes, 0)` — the row is grown to its padded length, which is the thing the loop was awkwardly doing |
| `jean-windows/src/identity.rs` | `match integrity { Ok(v) => Ok(v), Err(e) => Err(e) }` | `integrity` |
| `jean-windows/src/bounded.rs` | `match value { Ok(v) => Some(v), Err(_) => None }` | `value.ok()` |
| `jean-windows/src/ocr.rs` ×2 | `push_str("…")`, and `as i64 as i32` where the value is already `i64` | `push('…')`, one cast |
| `jean-windows/src/bin/…` and `tests/native_desktop.rs` | `child()` at 9 arguments; `.abs() as usize` | the signature allowed bare — it *is* `CreateWindowExW`'s parameter list, and the tree already allowed this lint at **15** sites before Phase 4 (`grep -ra too_many_arguments crates apps --include=*.rs` → 19 now, 4 of them Phase-4's: `bin/jean_desktop_fixture.rs:220`, `jean-core/src/desktop.rs:903`, `phase4.rs:662` and `:717`); `unsigned_abs()` for the BMP height |
| `jean-cli/src/phase4.rs` ×2 | `chosen_action` at 8 arguments; `!matches!(key.as_deref(), Some(_))` | allowed bare, for the same reason: the arguments are the flags; and `key.is_none()` |

Nothing was silenced that the tree did not already allow in the same shape. There is **no `clippy.toml` in
this tree to touch** — checked, not assumed: there is no `clippy.toml` and no `.cargo/` directory in the
workspace at all, so no lint configuration was added, changed, or leaned on — and the four `too_many_arguments` allowances Phase 4 added are named above with their line numbers, two of
which (`decided` in `jean-core`, `chosen_input` in `jean-cli`) were written as the functions were written
rather than after a run named them; the retained logs start at `p4-clippy-2.log`, so no earlier run that
would have flagged them is available to cite, and this row says so instead of implying otherwise.


## 54. What the planning documents said before this pass, and what is true now

§49–§53 describe the build. This section describes the *documents*, because the brief asks for the two to be
reconciled, and because the last batch's correction — an invented explanation with probe citations that did
not exist — is the reason every line below was re-measured instead of re-read. Three of the errors fixed here
were made by this project's own documentation in an earlier pass, and they are named as such rather than
quietly corrected.

### 54.1 `PLANNING/03-DEPENDENCIES.md`

| Line | Said | Now says, and why |
|---|---|---|
| §3, UIA row | `**ADOPT** — see risk R-04` | **ADOPT — consumed (D-048)**, with what that took: `=0.25.1`, `default-features = false`, `["control", "pattern", "input"]`, and the reason `input` is not optional — `core.rs:38` imports `crate::inputs::MouseButton` unconditionally while `lib.rs:13` gates that module behind the feature, so the crate **does not compile** without it. The verbatim compiler output is retained (`target/p4-fixture-build.log`, `[exit] 101`), which is the difference between a documented workaround and a guess. |
| §3, UIA fallback row | `**RAW** — escape hatch for gaps` | **RAW — still an escape hatch, and still unused.** `Win32_UI_Accessibility` is not in our feature list (`grep -c` on `Cargo.toml` → 0) because the fallback was **never needed and never written**; the gaps turned out to be four `.ok()`-swallowed pattern errors and a missing fifth question. |
| §3, `windows-capture` / `xcap` / `enigo` rows | three plain **ADOPT** verdicts | three **NOT ADOPTED here** verdicts (D-049), each with the same measurement (`lock=0`, sources already in the registry cache) and different reasons: WGC is a frame-pipeline shape mismatch — and the row now states plainly that WGC was **never measured on this machine**, so the decision rests on the requirement, not on a failure nobody observed; `xcap`'s row now carries the fixture's actual capture results (T05/T08/T10/T11/T21); `enigo`'s row now retires its DPI instruction as an **acceptance requirement** — the claim it stood for is measured on the path that ships, and running a library this build does not depend on is not a gate (§52 row 5). |
| §3, `ocrs` row | CANDIDATE, unchanged from the research phase | still a candidate, plus `lock=0`, plus the correction that the shipped extractor type is **`NoOcr`** at `ocr.rs:284`, not the name the §49 table used. |
| §3, "Feature flags … (default: `input`, `control`, `pattern`)" | three defaults | **two**, read from the 0.25.1 manifest: `default = ["input", "control"]`; `pattern` arrives transitively through `control`. The flag list sentence also gained `screenshot = ["png", …]` and `input = ["windows/Win32_UI_Input_KeyboardAndMouse"]`, which are why the two decisions on that row cost what they cost. |
| §3, "UIA from Rust — the honest picture" | mitigation "mandatory", fallback "~10× the code" | an **As shipped in Phase 4** paragraph: the trait shipped, the raw-COM path did not, FlaUI stayed out for the install rule, and R-04 is therefore *unchanged* rather than closed. |
| §1, `windows` feature block | "This is the live list in the workspace root manifest", 17 features, no Phase-4 names | the six Phase-4 features (`HiDpi`, `Input_KeyboardAndMouse`, `Graphics_Gdi`, `Storage_Xps`, `RemoteDesktop`, `StationsAndDesktops`), the count taken from the file rather than the block (**22**), and the reason **`Win32_UI_Accessibility` and `Win32_System_Ole` are absent** while UIA still works: the wrapper requests them against the same pinned version, so cargo builds them once. The "still to be switched on" table became a per-subsystem *as built* table. |
| §5, RapidOCR row | `**ADOPT** (vision sidecar)` | **NOT ADOPTED here**, with the interpreter measured today: `rapidocr` and `cv2` do not import, `onnxruntime 1.27.0` / `numpy 2.4.6` / `PIL 12.3.0` do, and the brief's no-install rule is the reason that is all this row can say. |

### 54.2 `PLANNING/05-ROADMAP.md`

- Phase 4's thirteen deliverables were all still `- [ ]`, as written before the work. They are now marked in
  the same three states Phase 3 uses, with the original wording kept inside each line, and the marking is
  deliberately uneven: **ten `[x]`, one `[~]`, two `[ ]`** (recounted with `awk` over lines 230–340 on
  2026-11-06). The `[~]` is capture (the *capability* shipped, the two *named libraries* did not); the `[ ]`s
  are OCR and pixel grounding, which the brief forbade installing and which §52 keeps open. The enigo row is
  now `[x]`, and not because enigo was tested: its acceptance instruction was **retired** as inapplicable to
  an unadopted dependency, while the instruction's actual subject — coordinate input proven at non-100 %
  scaling — is now measured on the shipped `SendInput` path (§52 row 5).
- The exit criterion is marked **met in form, qualified in the noun**: the suite is 30/30 on locator-addressed
  tasks at real 125%/100% mixed DPI, and the tasks are against J.E.A.N.'s own fixture application. **No
  third-party application was acted upon** — the live `brave.exe` in the evidence is the window that was in
  front when an input was *refused* (`target/p4demo/input-demo2.log`), not a window that was driven.
- The benchmark sentence lost its implied equivalence. Nothing here is tracked against
  WindowsAgentArena or OSWorld, and the 21.3% figure is quoted in §50 and in the roadmap as *a different kind
  of number*.
- The Chromium trap kept its original paragraph and gained the observation. `accessibility_flag` exists in
  this planning set and in `docs/` and **nowhere in code** (verified with `grep -r`); the shipped handling is
  `TreeExplanation::NoElements` versus `NoInteractiveContent { looks_like_web_content }`, whose message names
  the flag hypothesis as a *conditional* and is unit-tested at `desktop.rs:2500`. The Telegram case is
  reported **pending**, with the Unigram measurement, and no claim about a black screen.

### 54.3 `PLANNING/06-RISKS.md`

R-03, R-04, R-14, R-15, R-16, R-18, R-19, R-20 and R-21 each gained an *As of Phase 4* row (or a register
cell, for the shorter ones), and none of them says "closed". The pattern across all nine is the same and worth
stating once: **the design mitigation shipped, and the specific test the row asked for mostly did not.** R-03
is the clearest case — "test enigo's DPI behaviour explicitly in week 1" — and that instruction is now
**retired rather than marked done**, because enigo ships nothing here and testing an unadopted dependency is
not an acceptance requirement. The claim it was standing in for — that fixed-coordinate input works at
mixed scaling on a second monitor — is now measured on the path that ships, by coordinate, against the
fixture's own counter. R-14 distinguishes the verified *read* (Medium/High/Low all observed on this desktop) from the
unverified *behaviour* (no elevated window acted upon). R-15 gained the part nobody planned for: the awareness
call is process-global and irreversible, so `ensure_dpi_awareness()` **reports** `Set`/`AlreadyFixed`/
`Unavailable` rather than asserting success. R-20's "never loop" was checked in source: the only
`SetForegroundWindow` in the workspace is in the **fixture**, and `input.rs:31` lists the fighting APIs JEAN
does not use. R-21 is the honest one — the pass rate is narrower than it sounds, and §50's own definition
(*"thirty reported the truth about what happened"*) is quoted there rather than paraphrased.

Two rows were **added**, both about things this phase does rather than things it hopes:

- **R-29 — an abandoned desktop worker is a leaked thread, and no API can un-leak it.** *(as written then; the
  mechanism it described was replaced by §55.1, and the row now reads "a hung call costs a process, and the
  process is terminated with its job object". At the time of writing `bounded.rs` really did say so in its
  header, the cap really was `[desktop] max_concurrent_workers` (default **4**, `config.rs:1252`), and the
  evidence really was T22/T23 plus the two budget/abandonment tests.)*
- **R-30 — two of the three retention values write unredacted screen text and this build removes nothing.**
  *(The second half of that sentence is no longer true: §55.2 delivers the expiry the setting always
  promised, claimed-before-written and bounded. What survives is the first half — a retained frame is
  pixels, and pixels are not redacted — and the packaging trap is now "do not describe the expiry as a
  privacy control", not "do not describe it as auto-deleting".)* At the time: `discard_after_task` was the
  default because it was the only promise kept, and `keep_24h` "reported an expiry rather than enforcing
  one" in the code's own words.

The gating paragraph now says the uncomfortable thing: **after Phase 4, none of the six 🔴 risks is closed.**

### 54.4 `PLANNING/07-DECISIONS.md`

D-048, D-049 and D-050 registered (index rows and full entries: the wrapper and its four boundaries; the
dependency-free capture/input/OCR path; abandon-not-cancel with a counted, capped leak). D-017 gained an *As
built* note that corrects two things — the trait's name (`AccessibilityBackend`, not `UiAutomationBackend`)
and the fact that its "partially implemented raw-COM path from day one" was never written, so the entry's
*"a stall costs days, not a rewrite"* is now labelled for what it is: **an unexercised remedy.**

### 54.5 Three documentation errors this pass found and fixed, with how

All three were written confidently in an earlier part of this same batch. They are listed here because the
brief's anti-recurrence instruction is about the mechanism, not the mood.

| Was written | Is | How it was caught |
|---|---|---|
| `ocr::TextExtractor` ships `UnconfiguredExtractor` (§49 table, §52 row 1) | **`NoOcr`**, `crates/jean-windows/src/ocr.rs:284`; the other name appears nowhere in the tree | The reconciliation went to look up the type in order to describe its refusal, and `grep -rn UnconfiguredExtractor crates apps` returned nothing. Both occurrences corrected; the refusal is now quoted from the code. |
| `uiautomation`'s default features are `input, control, pattern` (§3) | `default = ["input", "control"]`; `pattern` comes transitively | Read out of the registry manifest with `sed -n '/^\[features\]/,/^\[/p'`, not from the previous document. |
| `accessibility_flag` is a tools-manifest field (in `04-SETTINGS.md:574`, `05-ROADMAP.md`, `06-RISKS.md`) | **not implemented** — `grep -r accessibility_flag crates apps` → no match; `jean-protocol/src/tool.rs` has no such field; `docs/desktop-control-plan.md` never contained it either | Chased while writing the Chromium row, on the assumption that the plan described the code. It described an intention. |

**Four pre-existing table rows in the files this pass edited were repaired, and nothing else about them
changed.** `03-DEPENDENCIES.md:315` and `06-RISKS.md:242` were missing their closing pipe (so the cell after
them swallowed the line break), and `06-RISKS.md:61` and this document's own `:231` carried an unescaped `|`
inside a code span (`jean browser status|run`, `ARGS=alpha|beta gamma`), which markdown reads as a column
boundary. The repair is pipe-level only — no word was changed — and it was found by a checker written for
this pass (`target/p4demo/md_table_check.py`), which now reports **0 mismatched rows** across the five
documents it covers. The defects were from earlier batches, and they are recorded here rather than silently
tidied, because a batch that edits a document is responsible for what its edits make visible.

Also corrected in §53 — several of them **after** the section had been appended, which is the honest sequence, since the first pass re-read the batch's own claims instead of re-measuring them: the `jean-windows` test line had cited
`test-windows-8.log` for a green 30-test run — that log is a **compile failure** (`[exit] 101`, union-field
`E0133`), and the 9 lib tests are in `test-windows-9.log` and the 21 native tests in `test-windows-6.log`,
from two separate commands; the format line had cited `p4-clippy-7.log` (a clippy log) when the green `fmt
--check` is `fmt-check-3.log`, and the diff count is **126**, not 111, spread across **fifteen** distinct
Phase-4 files (eleven in `jean-windows`, plus `jean-protocol/src/config.rs`, `jean-core/src/desktop.rs`,
`jean-cli/src/phase4.rs` and the Phase-4 dispatch arm in `jean-cli/src/main.rs`); the
linker line had said nine `FILE_SYSTEM (3)` and four `LIMIT (12)` **across 26 test targets** when `p4-build-final.log`
contains eight and five, with 13 `could not compile` lines naming 11 `jean-core` targets and 2 `jean-cli`
links; the volume had moved again (26.87 GB in `target\debug`, 25.98 GB of it in `deps`, 288 `.pdb` files at
13.18 GB, 277,938,176 bytes free); the clippy history was four runs where there were five attempts;
`clippy.toml` was described as untouched when **the tree has no `clippy.toml` and no `.cargo` directory**; and
"the tree already allows this lint at seventeen sites" is **15** pre-existing sites plus 4 Phase-4 ones, of
which two (`decided`, `chosen_input`) were never flagged by a run whose log survives.

### 54.6 Method, because the last reader asked

Every line above was changed against a measurement taken for this purpose, not against the previous
document: `grep -c '^name = "X"' Cargo.lock` per candidate crate; `grep -ra -A1 '^name = "uiautomation"'
Cargo.lock` for its resolved sub-tree; the registry manifests of `uiautomation-0.25.1` and
`uiautomation_derive-0.7.9` read directly; `cargo tree --workspace --locked -e normal --invert …` for the
windows-rs duplication (`windows 0.61.3` belongs to `win32job`, `0.62.2` is ours, `syn 3.0.9` predates this
phase via `clap_derive`/`rmcp`/`agent-client-protocol`); `tr -d '\000' < log | grep -ac …` for every count
taken from a retained log; `awk '/^windows = \{ version/,/^\] \}/' Cargo.toml | grep -oE '"[A-Za-z0-9_]+"' |
sort` for the feature list; `(Get-PSDrive C).Free` and a per-subdirectory size pass for the volume; and
`C:\Python314\python.exe -c "import X"` for each vision-side module, re-run today. Identifiers named in the
documents were checked for existence by path (24 cited paths, all present) and by name (48 backticked
identifiers from §49–§53), which is how the `UnconfiguredExtractor` class of error is caught before it ships
rather than after.

`docs/desktop-control-plan.md` was **left alone, deliberately.** It is the one document in this set that
already says the true thing about the hardest line: *"UNVERIFIED: which Telegram controls and messages the
user's installed client exposes through UI Automation, and which capture failures reproduce on this machine.
Do not infer these from protocol documentation"* (line 16), next to the instruction to test the installed
client and version *"before claiming support"* (line 9). The documents that needed correcting were the ones
that had already answered that question in code-shaped language; this one had not, and editing an honest
sentence to make it look current would have been the same mistake in the other direction.

Nothing was installed, authenticated, committed, pushed, published, deleted, or reconfigured — in the tree or
on the machine — during this pass. The `target/p4demo/` scratch files this batch wrote are left in place as
the evidence the sections cite, and the pre-existing scratch script `target/p4_name_errors.py` remains there
too; both are the batch's own output, and removing files is outside what this batch is permitted to do.

## 55. The Phase-4 correction: what changed, what was run, what is still open

This section is the handoff for the correction pass of 2026-11-06, whose instruction was *correct and
stabilize Phase 4; do not start Phase 5*. It supersedes the parts of §49–§54 that described the
abandoned-thread worker, the non-expiring `keep_24h`, and the coordinate path that had only ever been proven
by locator. Those sections keep their own text where it is a record of what an earlier pass measured, and
point here where they stated something this build no longer does.

### 55.0 Storage, measured before building

| Measurement | Value |
|---|---|
| Free on `C:` at the start of this pass | **19 GB** of 475 GB (`df`: 457 GB used, 97 %) — enough headroom; nothing was deleted and nothing was moved |
| Physical memory | 16,462,712 KB total, 6,583,488 KB free at the measurement |
| Pagefile | `C:\pagefile.sys`, **20,185 MB allocated**, peak usage 15,499 MB |
| `CARGO_INCREMENTAL` | `0`, exported in the build child only, as before |
| Free on `C:` at the end | **11 GB** — one whole-workspace debug build of every target costs about 8 GB here, which is a fact the next validation pass should plan around rather than discover |

No display setting, no machine setting, no software, no install: the pagefile figure below is why the test
command is run with `-j 2`, and that is a flag on the command line rather than a change to the machine.

### 55.1 Item 1 — the hung provider now costs a process that gets killed

**The requirement the previous design did not meet.** A timed-out thread is not isolation: the thread stays
parked, its stack stays allocated, and a UIA call into a window that is not pumping its messages cannot be
cancelled by anything short of that window going away. The fix is not a bigger number — `[desktop]
max_concurrent_workers` is still **4**, unchanged — it is that the unit which gets abandoned is now the unit
which gets terminated.

**What was built.**

* `crates/jean-windows/src/worker.rs` is the parent half: one `jean-desktop-worker.exe` per desktop call,
  spawned through the supervision every other J.E.A.N. child already gets — `jean_process::piped::spawn`,
  `CREATE_SUSPENDED` so the tree is contained before it runs, a job object with `KILL_ON_JOB_CLOSE`, pipe
  halves the parent owns. `crates/jean-windows/src/bin/jean_desktop_worker.rs` is the child, and it calls
  the *backend implementations* (`UiaBackend`, `PrintWindowCapture`, `ScreenCapture`) rather than the free
  functions, which is what stops the child recursing into itself.
* **Typed, bounded local IPC.** `Request`/`Reply` over JSON lines, one line each way, `PROTOCOL_VERSION = 3`,
  a `Hello`/`Pong` handshake that names a stale binary as itself rather than as a hung provider, a 1 MB
  request cap and an 8 MB reply cap, and `Body`/`ReplyBody` as the whole vocabulary — tree read, resolve,
  state read, act, capture, probe. `WireError` mirrors the crate's error enum so the refusal an operator
  reads is the one the desktop gave.
* **Three deadlines, not one.** A startup bound, a request bound (`uia_timeout_secs` / `capture_timeout_secs`)
  and a shutdown bound (`worker::SHUTDOWN_BOUND`, 2 s). At any of them the parent closes the job object, and
  the worker plus every process it started is ended by the OS rather by a promise.
* **`Stop::{TimedOut, Cancelled, Lost, Unavailable}`** is mapped in one place per path, not in one place
  per call site: `accessibility::run` turns them into `ProviderTimeout` / `Cancelled` /
  `AccessibilityUnavailable`, and `capture::run` into `CaptureUnusable` / `Cancelled` / `CaptureUnusable`, so
  the error an operator reads for an interrupted call cannot depend on which verb happened to be running.
  `may_have_acted()` is `false` only for `Unavailable`; everything else that touched a control is **uncertain
  and is not replayed** (D-050 holds across the new boundary: killing a worker does not undo an `Invoke` it
  had already delivered). `accessibility.rs`'s comment on the mapping says the part the thread design could
  not say — *both* a timeout and a cancellation mean the worker has already been terminated with its tree,
  so nothing is left running behind the error.
* **No in-process fallback.** If the worker binary cannot be located, desktop work is *refused*, naming every
  path tried — the alternative was the behaviour this item exists to remove.
* **Frame pixels cross the boundary honestly.** The worker writes the BMP into a directory the parent owns
  (`%TEMP%\jean-desktop-frames\<pid>-<seq>`, removed with the session), and `one()` loads the bytes *before*
  the session ends; a short frame is reported incomplete and a long one is refused, because a frame that
  disagrees with its own header is not evidence about a window.
* `[desktop] max_concurrent_workers` is now a count of **live worker processes**
  (`worker::live_workers()`), gated in `jean_core::desktop::gate_workers` before any task row is opened, and
  reported by `jean desktop status` as `desktop_workers_alive 0 of 4`.
* **COM threading is preserved inside the worker, not shared across the boundary.** Nothing is cached between
  calls: the apartment, the `UIAutomation` instance and every element are created in one call and die with the
  process (`uia.rs`'s first rule now says so in the process's own words).
* Input deliberately stayed on the bounded-thread path, and `bounded.rs` says why: `SendInput` posts to the
  system queue and the verification around it reads the window manager's own records, so no foreign provider
  can hold it open — and killing a process *mid-action* would make the no-replay rule harder to honour, not
  easier.

**Two defects the new design produced, and how they were found rather than guessed.**

1. **Every healthy call cost 2.03 s.** Measured, not suspected: `one(Hello)` was 2.03 s while a bare
   `CreateProcess` was 58 ms and `piped::spawn` 3 ms. The parent's duplicate stdin handle was still open,
   because `Drop::drop` runs before the struct's fields drop, so the worker never saw the EOF it was waiting
   for and each session paid the full `SHUTDOWN_BOUND`. `Session::shutdown` now takes the handle before
   closing the pipe, and the measurement test asserts `supervised < worker::SHUTDOWN_BOUND` so the number is
   a gate, not a note.
2. **A combobox test went red** (`selection_and_expansion_are_read_from_the_controls_that_have_them`) because
   the worker's `Resolve` arm hard-coded `probe_patterns: false` — a bound the caller set and the child
   ignored. It is carried on the wire now, the protocol version went to 3, and
   `every_bound_the_worker_is_told_to_honour_arrives_intact` pins all of them.

**The demonstration the instruction asked for** — repeated intentional hangs beyond the old four-worker
limit, then a successful action on a healthy window:

`a_storm_of_hung_providers_leaves_no_worker_behind_and_the_desktop_still_works` drives **five rounds**
against the hung-provider fixture. Each round: `worker::live_workers() == 0` afterwards, at least one
`ProviderTimeout` counted, and `tasklist` naming **no** `jean-desktop-worker.exe` belonging to this session;
the round after the storm performs a real `Invoke` on the healthy fixture and reads `clicked:1` from the
status label — the desktop still works, and the parent was responsive throughout, because the parent never
made a provider call at all. Cancellation is tested the same way: a blocked call that is cancelled ends with
`live_workers() == 0`.

**What the isolation costs, measured on this machine** (`the_fixed_cost_of_a_desktop_call_is_reported_as_a_
measurement`, `target/storm-demo-12.log`): `CreateProcess` with no pipes and no job object **58.17 ms**; the
same binary started the way J.E.A.N. starts it **3.11 ms**; supervision plus one handshake **56.58 ms**; a
UIA walk of 18 controls through it **149.67 ms**; a PrintWindow capture of 640x360 **88.05 ms**. The
supervision is nearly free; the provider work is what was always going to be paid.

**Documentation that had become false was corrected, not left to be rediscovered**: `config.rs`'s
`d_max_stuck_workers` and `uia_timeout_secs` docs, `config/defaults.toml`'s two matching comments,
`error.rs`'s `ProviderTimeout`/`Cancelled` (which said *the thread that asked is left parked and abandoned on
purpose*), `capability.rs`'s observation line (now "its worker process was terminated, and the call was not
cancelled"), `uia.rs`'s isolation rule, `bounded.rs`'s `IN_FLIGHT` doc, and `desktop.rs`'s cancellation
header.

### 55.2 Item 2 — `keep_24h` now means what it says, and deletes only its own artifacts

`crates/jean-core/src/captures.rs` (new, 14 unit tests) is the expiry, and the thing it is built around is
**ownership**, because the requirement is not "delete old files" but "delete old files that this build wrote,
and nothing else".

* An **append-only ownership manifest**, `jean-captures.jsonl`, one JSON line per claim. `keep_frame` claims
  the frame **before** writing its bytes and **refuses to capture** if the claim cannot be made — an
  unclaimable frame is one no expiry is permitted to remove, and writing it anyway is exactly how the setting
  would become false again.
* Deletion is authorised **only** by a claim naming the file. Files in the same directory that look like
  captures, and files an operator put there, are counted and reported (`2 file(s) in the directory that this
  build does not own and will not delete`) and never touched.
* **Containment is canonical and reparse-aware**: the root is resolved and refused if it is a reparse point,
  and so is every file before it is removed, so a junction cannot lead the deleter outside the root. Measured
  rather than remembered: a `mklink /J` junction reports `dwFileAttributes = 0x410`, and
  `FILE_ATTRIBUTE_REPARSE_POINT` is **`0x400`** — the first version of this module used `0x1000`, which is
  `OFFLINE`, and every junction it looked at was refused with a *wrong reason*. Two tests create real
  junctions to prove both the refusal and the reason.
* An unreadable manifest line does not vanish and does not abort the sweep: it arrives as a claim whose name
  cannot be safe and is **refused in `classify`**, so "I could not read this claim" and "this claim authorises
  a deletion" cannot come from two different code paths that disagree.
* **Bounded storage**: expiry at `KEEP_HORIZON_MS` (24 h from the claim), and the kept set held under
  `MAX_STORE_BYTES` (512 MiB) by removing the oldest `keep_24h` artifact first. `keep_until_manual` is
  excluded from the ceiling on purpose — it made a different promise, and paying for `keep_24h`'s bound with
  the operator's manual keeps would be a silent change to it.
* **Startup reconciliation**: `reconcile_at_startup` runs once per process, on the path before any desktop
  action writes anything new (`decided()` calls it right after the worker gate), which is what "startup" means
  for a tool that runs one command per process. `jean desktop status`'s `capture_store` line reads the same
  store **without** deleting anything, because a diagnostic must not be the thing that starts removing files.
* `discard_after_task` keeps its no-persistence behaviour and is still the default; the clock is a parameter
  (`reconcile(dir, now_ms, horizon_ms, budget_bytes)`), so **the 24-hour rule is tested without anyone waiting
  24 hours**.
* Because safe expiry *is* delivered, the config gate the instruction named was not needed — nothing has to
  reject `keep_24h` at load. The failure mode it was guarding against is closed at the write instead.
* **The caveat that no code can fix is stated where the setting is set**: `config.rs`, `config/defaults.toml`,
  `retention_of()` and `jean desktop status` all say that a retained frame is the screen as it looked, that
  password-flagged controls are redacted in the *digest* which is a different object from the image, and that
  the redaction applied to text does not apply to a picture.

**Demonstrated end to end**, not only unit-tested: `target/p4demo/capstore-demo.sh` runs the real CLI with its
own `JEAN_DATA_DIR`, ages one claim past the horizon, plants an unclaimed lookalike filename and an unrelated
operator file beside it, then checks the store: the expired claimed frame is gone, **both** other files
survive, every remaining BMP is claimed, every claim has a file, and `jean desktop status` reports
`1 owned frame(s) totalling 691254 bytes, 0 due to expire now, 2 file(s) … this build does not own and will
not delete` (`target/p4demo/capstore-demo-2.log`, `[exit] 0`).

### 55.3 Item 3 — the backend that ships is the one that was tested, and enigo's rows are retired

`enigo` is still not in the lockfile (`grep -c '^name = "enigo"' Cargo.lock` → **0**), so the
enigo-specific acceptance requirement is **retired, not marked done**: testing a dependency this build does
not use, to compare against one it does, is research and not a gate. The rows that said otherwise are
corrected in `PLANNING/05-ROADMAP.md` (the deliverable, and the multi-monitor coordinate row),
`PLANNING/03-DEPENDENCIES.md`'s input-injection row, `PLANNING/06-RISKS.md` (R-03's heading, mitigation, test
and status rows, its register cell, and the gating paragraph), `PLANNING/07-DECISIONS.md` (D-049's row and
its cost bullet) and `IMPLEMENTATION_STATUS.md` §52 row 5 and §54. They say the requirement is withdrawn and
they say what replaced it, which is evidence about the shipped path:

* **Virtual-desktop offsets.** `to_normalised` is unit-tested at a `-1366` left origin and a `-768` top, for
  origin-shift invariance (the same offset from the origin normalises the same on either side), at the far
  edge — where the documented `- 1` in the divisor is the difference between 65535 and **65515**, one pixel
  short of every second monitor's right-hand edge — for out-of-range points in both directions, and for a
  desktop reporting no geometry. **That last case was a live panic**: the floor was applied to the width
  rather than to the divisor, so `SM_CXVIRTUALSCREEN == 0` (no display attached) divided by zero inside the
  core process. `normalised_axis` now floors the divisor and answers "the first pixel" (`input.rs`, 7 input
  tests green, `target/input-test-1.log`).
* **Supported DPI, on real hardware.** `a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on`
  places a per-monitor-v2 fixture on the 1366x768@1920,306 **100 %** secondary while the primary runs at
  **125 %**, checks the window's own `GetDpiForWindow` against the monitor it landed on, aims at the button's
  UIA-reported bounds **by coordinate** rather than by locator, and reads the fixture's counter as the
  witness: `MEASURED: a coordinate click at (2408,410) — normalised (48039, 25041) of a virtual desktop
  3286x1074@0,0 — landed on monitor 1366x768@1920,306 at dpi 96 (100 %) and the fixture's own counter read
  "clicked:1"; two refused sends after it left that counter unchanged` (`target/input-demo-coordinate.log`).
* **Focus changes and stale targets, through the real send path.** The same test calls `input::send` with a
  point 4000 px outside the window and with a foreground expectation the target cannot meet; both are refused
  and the counter stays at `clicked:1`, which is what proves the refusal happened *before* `SendInput` rather
  than after it.
* **What was found while building this.** The fixture would not take the foreground from a test-spawned
  child (`granted=false`, repeatedly) and did from a shell-launched one (`granted=true`), and the difference
  was measured rather than assumed: `.spawn()` → false, `cmd /c start` → true, `start /B` (inherited console)
  → false. So `Fixture::start_shell` launches the way an operator launches an application, which is also more
  honest about what J.E.A.N. acts on — it never forces focus (R-20), so the test may only use focus the
  desktop was willing to give. Separately, `Fixture::start` was closing the fixture's stdout pipe as soon as
  it had read the ready line, which kills any fixture that prints afterwards — a panic on a broken pipe
  inside a window procedure. The pipe is now drained for the fixture's life.
* **Still unverified, and not simulated:** 150 %/200 % scaling, a single-monitor-only session, three
  monitors, and a monitor *physically* left of or above the primary (that branch is arithmetic-proven only).
  No display configuration was changed to manufacture any of them, and none of the unit tests above counts as
  physical-device evidence.

### 55.4 Item 4 — the four commands, with their real exit codes

Every line below is what the log's own `[exit]` says; the background-task notifications reported the wrapper
and are not evidence.

| Command | Result on this tree |
|---|---|
| `target\dev.cmd build --workspace --all-targets --locked` | first attempt **`[exit] 101`** (`p4v-build-1.log`): thirteen `crate X required to be available in rlib format` and `can't find crate for jean_core`, across `jean-core`'s ten integration suites and both `jean-cli` link targets — a stale-artifact shape in `target/`, not a source defect, and the same class §53.2 recorded. Cleared without deleting anything by running `build --workspace` alone first (`p4v-build-2.log`, **`[exit] 0`**), after which the required command itself is **`[exit] 0`** (`p4v-build-3.log`), **re-confirmed on the final tree** (`p4v-build-4.log`, `[exit] 0`, run after every source change in this pass). The recipe for CI on this machine is therefore *build, then build --all-targets* |
| `target\dev.cmd test --workspace --all-targets --locked -j 2` | **`[exit] 0`** — **36 targets, 911 passed, 0 failed, 1 ignored** (`p4v-test-7.log`, the run of the final tree; `p4v-test-6.log` is the same result one edit earlier). The one ignored test is pre-Phase-4 and stays ignored: `a_real_toast_delivers_without_panicking` in `crates/jean-core/tests/notify.rs`, whose own reason says it *"reaches the live WinRT notification API; run by hand at a desktop with `cargo test -p jean-core -- --ignored`"*. At default parallelism the same command **`[exit] 101`** with `failed to mmap … libtest-*.rlib: The paging file is too small for this operation to complete. (os error 1455)` (`p4v-test-4.log`) — the machine's commit limit, not a code failure; `-j 2` is the flag that fits, and no machine setting was changed to get it |
| `target\dev.cmd fmt --all -- --check` | **`[exit] 0`** (`p4v-fmt-3.log`, re-confirmed `p4v-fmt-5.log` after the clippy fixes and `p4v-fmt-6.log` on the final tree). It did not start green: the first check was **`[exit] 1`** with **80 `Diff in` points** (`p4v-fmt-1.log`), every one in Phase-4 files — `jean-windows` throughout (including the new `worker.rs`, its bin, `capture.rs`, `accessibility.rs`, `native_desktop.rs`), `jean-core/src/captures.rs`, `desktop.rs` and `phase4.rs`. The Phase-4 code had been *written* but never formatted: rustfmt 1.10.0-stable expands single-line struct variants and long `match` arms, and a first-version-of-everything module written by hand does not do that by instinct. One `cargo fmt --all` pass (`p4v-fmt-2.log`, `[exit] 0`) and the check is clean; §55.5's two fixes and §55.4's two clippy fixes were formatted the same way |
| `target\dev.cmd clippy --workspace --all-targets --locked -j 2 -- -D warnings` | **`[exit] 0`** with zero `error` lines (`p4v-clippy-3.log`). Two attempts failed first, both findings real and both in code this pass added: `large_enum_variant` — `ReplyBody::Effect` was 744 bytes against `Probe`'s 144, so the variant is boxed (`Box<Effect>`, with the reason in the doc comment) rather than allowed; and `zombie_processes` on the new shell launcher, which now reads the shell's own exit status and fails with "the shell refused to launch the fixture" instead of leaving an unwaited child |

**The 30-task suite, re-run against this corrected build** (`bash target/p4demo/suite.sh`,`target/p4v-suite-1.log`, run directory `target/p4demo/suite/run-20261106-230225`): **30 passed, 0 failed, 0
skipped** — with the rows separated the way the instruction asks, because "30/30" is a statement about
truthfulness and not about success:

| Class | Rows | Count |
|---|---|---|
| Performed and proven (`exit=0`) | T01, T02, T03, T04, T05, T07, T08, T09, T11, T12, T13, T14, T15, T16, T17, T21, T23, T24, T30 — including T12 (`discard_after_task` writes nothing) and T13 (keep writes what it says it kept) | **19** |
| Recorded without a verdict field, because the check is about the record | T29 — work recorded, no typed text reaches the audit trail (`audit rows=110 desktop task rows=40`, no leaking files) | 1 |
| **Expected refusals** — the task passes because J.E.A.N. said no, in words | T06 a handle that is not a window, T10 an excluded window refuses capture, T18 delivered-but-not-proven against a check the desktop does not meet, T20 an ambiguous locator refused rather than tie-broken, T25 input refused before `SendInput`, T26 synthetic input without its own switch, T28 actions disabled | `exit=1` ×**7** |
| **Unverifiable, reported as such** | T19 — the witness the action destroyed is not "unchanged" | `exit=3` ×1 |
| **Bounded / uncertain, deliberately not replayed** | T22 — a provider that never answers | `exit=4` ×1 |
| **Policy refusal** | T27 — an approval nobody could answer | `exit=5` ×1 |
| **Skipped** | none | 0 |

Rechecked against all 30 rows of `results.tsv`: exit-code counts are 19 / 7 / 1 / 1 / 1,
which total **29**, plus **T29**, the audit-only check without an exit field, for **30**.
T29 is not an inferred success or a missing task; its recorded evidence is `audit rows=110 desktop task
rows=40` and no file leaking the typed text. Exit-code counting alone does not count this row.

T12 (`discard_after_task` writes nothing) and T13 (keep writes what it says it kept) are the retention pair;
the *expiry* half of T13's setting is not something a 30-second suite can wait 24 hours for, which is why it
is the clock-parameterised unit tests and the demo above that carry that claim rather than a suite row.

**Diagnostic-only, and reported as such, not as coverage:** `the_fixed_cost_of_a_desktop_call_is_reported_as_
measurement` asserts one shape (a healthy call must not pay the shutdown bound) and prints the rest as a
NOTE; `coordinates_are_computed_in_the_space_the_monitors_actually_use` prints
`SKIPPED mixed-DPI cross-check` instead of passing when the machine does not present two geometries. Neither
is counted as a demonstration of anything the other tests did not demonstrate.

### 55.5 Two defects that only the working `--all-targets` build could find

Both sat in `jean-cli`'s test target, which every earlier run of this phase failed to compile (§53.2), so
neither had ever executed. This is the value of fixing the build shape rather than reading the suite as done.

1. `one_call_sends_one_input_and_a_modifier_needs_a_key` asserted the *modifier* refusal for a call that
   names **no input at all** — which correctly returns `no input was named` first. The assertion was wrong,
   not the code. The test now names a click plus a modifier to reach the chord refusal, and separately pins
   the bare-modifier case, so both refusals are covered instead of one being asserted against the wrong call.
2. The same test expected `key ctrl+shift+Enter` while `VirtualKey::name()` returned the Win32 spellings
   `Return`, `Back`, `Prior`, `Next`. The audit line and the operator's own `--key enter` were naming one key
   in two vocabularies, and only one of them is the vocabulary the CLI documents and accepts. `name()` now
   returns the names `--key` takes (`input.rs`, with the reason in its doc comment); the key codes are
   unchanged, so what is injected is identical to what it was.

### 55.6 What this pass did not deliver, stated plainly

* **OCR and visual grounding at runtime.** `NoOcr` still refuses and `BackendStatus::Unavailable` still
  reports it; no model produced a character on this machine. The contract, the bounds and the fixture tests
  are the deliverable, and none of them is inference.
* **Live Unigram/Telegram behaviour.** Still pending, with the version measured and no claim about a black
  screen. The client was not authenticated, opened, or reconfigured.
* **`cargo deny check`**, for a third consecutive batch: cargo-deny is not installed and this pass may not
  install it. The lock went **340 → 340** entries while this pass added `serde` to `jean-core` — verified by
  diffing against the backup (`target/p4demo/Cargo.lock.before2`): the lock change is the dependency list
  line, not a version bump. `--locked` therefore holds for every command in §55.4.
* **No third-party application was acted upon**, and no personal profile, browser session or debugging
  endpoint was attached to. The `brave.exe` in the earlier evidence is a window that was in front when an
  input was refused, not a window that was driven.
* **Elevated targets, a locked workstation and a disconnected session**: the reads that would make those
  cases legible are verified; the behaviour under them is not, and acting on a High-integrity window would
  need a token this build was not given.
* **Wheel-notch delivery**: the aim is verified, the delivery is not, because no fixture control scrolls.
* **The physical DPI rows** in §55.3, and the negative virtual origin on real hardware.
* Phase 4's two 🟠 rows are **corrected, not closed**: R-29's leak is gone and what remains is the input path
  by design plus the untested third-party hang; R-30's expiry is delivered and what remains is that a
  retained frame is unredacted pixels.

### 55.7 Corrected Phase-4 completion status

| Instruction | Status |
|---|---|
| Storage: measure, do not delete, report the blocker | **Done.** 19 GB at the start, 11 GB at the end, nothing deleted, `CARGO_INCREMENTAL=0` kept. The one real blocker is the commit-limit failure at default test parallelism, reported with its numbers and worked around with `-j 2` rather than with a machine change |
| 1. Move blocking UIA/provider work into supervised child processes | **Done and demonstrated.** Typed bounded IPC, three deadlines, process-tree kill, fresh worker per call, responsive cancellation, uncertain effects never replayed, no in-process fallback, no accumulated threads — hung providers driven **five rounds past** the old four-worker ceiling with `live_workers() == 0` and `tasklist` agreeing, then a successful action on a healthy window. The worker limit was **not** raised, and leakage is not described as solved: it is not possible, and the test says so each round |
| 2. Make retention settings truthful | **Done.** `keep_24h` expires, bounded by a manifest that authorises deletion only of what this build claimed, refusing reparse points and never touching an unowned file, tested with a clock instead of a day, and demonstrated end to end. The pixels caveat is documented in four places. `discard_after_task`'s no-persistence behaviour is retained |
| 3. Test the shipped input backend | **Done, with the limits named.** Offsets, DPI, focus and staleness tested through `input::send` on the real desktop, including a coordinate click on the secondary; the enigo-specific acceptance rows are retired in every document that carried them; the unavailable physical cases remain declared unverified rather than counted |
| 4. Final validation | **Done.** Build, test, fmt and clippy all at `[exit] 0` on the final tree, each with the failed attempts and their causes recorded; the 30-task suite re-run at 30/30 with refusals, uncertainty and diagnostic-only rows separated |
| 5. Handoff | **This section** |

**Phase 4 is corrected and stable, and is not closed.** Its two dependency-free mechanism claims — provider
work in a killable process, and a retention setting that deletes exactly what it owns — are now delivered and
measured on this desktop rather than promised; the remaining open rows are in §55.6, and none of them is
described above as done. **Phase 5 was not started.** Nothing was installed, machine-wide settings were not
changed, nothing was committed, pushed or published, and the only runtime deletion performed anywhere in this
pass is the narrowly scoped capture-expiry implementation operating on its own claimed artifacts — which is
also the only thing that deletes anything at all in this build.

### 55.8 The decision register this pass wrote back

Both mechanisms above are decisions, so they are recorded as ones rather than only as code:

| ID | What it decides | State |
|---|---|---|
| **D-051** | A hung desktop call costs a **process**, and the process is terminated — provider work moves to a supervised child with typed bounded IPC and three deadlines | ACCEPTED, built, demonstrated (§55.1). **Partially supersedes D-050**: its isolation mechanism is replaced, its *uncertain-never-replayed* rule survives unchanged |
| **D-052** | Retention expiry is authorised by an **ownership manifest**, never by a filename pattern, and a frame that cannot be claimed is not captured | ACCEPTED, built, demonstrated (§55.2). Supersedes the "no cleaner exists" reasoning that had made `discard_after_task` the only honest default |

D-050's entry now carries a `partially SUPERSEDED` status with what still stands and what does not written
into it, because the value of that entry is the reasoning about `SendMessage` — which was right, and is the
reason the new design terminates rather than cancels — rather than the thread count that came with it.
`04-SETTINGS.md`, `05-ROADMAP.md`, `06-RISKS.md` (R-29, R-30, and the closing register paragraph) and
`config/defaults.toml` were corrected against the same two IDs, and `IMPLEMENTATION_STATUS.md` §49.1, §50.2,
§52 row 5, §53, §54.2 and §54.3 carry the pointers that keep their older sentences from being read as current.

---

## 56. Phase-5 concurrency foundation — 2026-11-07

**Delivered: independent task and voice control with supervised real-process fixtures. Native speech is not delivered; Phase 5 remains open.** Phase 4's outstanding cases in §55.6 are unchanged. Before implementation, the earlier desktop-suite arithmetic was corrected: 19 + 7 + 1 + 1 + 1 = 29 exit-bearing rows, plus audit-only T29 = 30 rows in `target/p4demo/suite/run-20261106-230225/results.tsv`; T29 is not another successful execution.

### 56.1 Architecture and authority

| Boundary | Implementation |
|---|---|
| Single execution/state owner | `crates/jean-core/src/interaction.rs:207`, `Controller`: one task worker and one independent voice worker per control session; model/tool work never runs in its control loop |
| Existing execution path | `crates/jean-core/src/task_worker.rs:20` runs each goal in a fresh `jean-task-worker.exe`, calling `plan_runner::run_owned` (`crates/jean-core/src/plan_runner.rs:270`) with the existing validated `OperatorRun` authority |
| Independent voice | `workers/voice/src/lib.rs`: replaceable `SpeechEngine`/`Playback` boundaries, explicit device-free fixture backend, unavailable native backend that fails closed rather than falling back |
| Typed local transport | `crates/jean-protocol/src/interaction.rs`, `crates/jean-process/src/channel.rs`: version 1 JSONL, 16 KiB frames, 4096-byte text, queues/outstanding requests bounded at 8; session/request IDs and deadlines; no audio payload variant |
| Process containment | Existing restricted handle inheritance and suspended-start Job assignment; Job active-process accounting, bounded writer cancellation/join and retained unreaped peers prevent silent accumulation |
| CLI control | `jean interact`, implemented in `apps/jean-cli/src/phase5.rs`: initial session snapshot, then typed request/reply envelopes; bounded asynchronous client output keeps a non-reading client from freezing the owner |

**Transport deviation:** private inherited anonymous stdio pipes replace the roadmap's proposed named-pipe server for these parent-owned workers. They reuse existing containment, expose no discoverable listener and keep audio inside the voice process.

Typed submission and explicitly submitted transcripts share the same planning, registry validation, policy, audit and verification path. PTT requires **BeginCapture → matching EndCapture → finalized Transcript → explicit SubmitTranscript**; transcript receipt alone grants no execution authority. Ambient audio, TTS and model/tool output cannot submit through that worker protocol. Uncovered approvals fail closed; they are not silently approved or remembered, and the existing CLI remains the approval surface.

### 56.2 Lifecycle guarantees and limits

- **Interrupt utterance** affects only the matching voice utterance. **Cancel task** addresses the exact task ID and its owned process tree. **Stop voice** clears capture/utterance state without cancelling the task. **Shutdown JEAN** stops both scopes.
- Stale sessions, duplicate/expired requests, mismatched task/activation/utterance IDs and duplicate start events are rejected. Interrupting an acknowledgement cannot undo or replay an accepted submission. Voice restart is explicit, uses a fresh session and never resubmits the task.
- Cooperative cancellation uses the existing cancellation mechanisms; deadline/disconnect failure terminates the task Job and reports `NeedsAttention`. Completion observed before a late cancellation remains completion: cancellation is not rollback. Task failures leave voice responsive, and voice failures leave task execution alone.
- Startup, request, capture, utterance and shutdown deadlines are bounded. Failed teardown blocks replacement workers rather than detaching them. Final uncancellable I/O teardown has a process-abort guard; that emergency path was **not induced** in tests.
- `[voice] enabled = false` and `JEAN_NO_VOICE` prevent voice process creation. No speech assets are loaded; the disabled process contributes no RSS. This is not a native model-load/peak-memory benchmark.
- Acceptance is **in-memory within the owner session**, not a durable submission queue or cross-crash exactly-once guarantee. Bootstrap failure can leave no persisted task row and reports `NeedsAttention`. Persisted uncertain runs are reconciled before the next task execution and at CLI shutdown, not on every controller tick; uncertain effects are never automatically replayed. The one-task limit is per owner, not a machine-wide lock.

### 56.3 Demonstrated evidence

`crates/jean-core/tests/interaction.rs` has **8 passing real-process tests**: task/descendant barriers; 24 utterance interruptions during a held task; status responsiveness; voice crash/restart without replay; explicit PTT submission and duplicate refusal; cooperative and forced task-tree cancellation; failed task with usable voice; stale/expired/duplicate worker frames; hung startup/backpressure; stop-voice isolation; late-cancel completion; disabled voice and previous-owner rejection.

The production-worker test uses an owned loopback model stub, not a paid/external provider. Typed and PTT goals each execute two registered gated commands through the real planner/policy pipeline: four `run_scope` audit grants, zero remembered approvals. It also cancels a real held tool and descendant, preserves `auto_deny`, and reports malformed worker configuration as `NeedsAttention` without freezing voice.

`apps/jean-cli/tests/cli_phase5.rs` adds **4 passing CLI tests**: status/interrupt/shutdown with stdin still open, the environment disable override and clean EOF, a deliberately non-reading client with a controlled-capacity pipe, and oversized input rejected without creating tasks. Protocol, configuration-parity and device-free speech tests also pass.

Final serial fixture measurements (`target/p5-measurements-final.log:19`):

| Observation | Measured |
|---|---:|
| Status response | 12.1 µs |
| Maximum of 24 utterance interruptions | 252.6 µs |
| Cooperative task-tree cancellation, including process-exit barriers | 2.6041 ms |
| Owner handles before → after | 114 → 114 |

The barrier bound was fixed at 8 seconds before measurement. These are **fixture control measurements, not microphone, acoustic barge-in or physical playback latency**. Process-exit handles supplement Job accounting to avoid mistaking a Windows termination race for a live descendant. Final owned-worker count was zero (`target/p5-owned-workers.txt`). Fixture directories and failed-attempt logs are retained.

### 56.4 Final validation

Commands ran sequentially through the existing `target/dev.cmd`, with `CARGO_INCREMENTAL=0`, `--offline --locked` and `-j 2` for build/test/Clippy. Actual `[exit]` markers, not wrapper notifications, establish success.

| Command | Retained log | Exit |
|---|---|---:|
| `cargo build --workspace --all-targets --offline --locked -j 2` | `target/p5-build-final-2.log` | 0 |
| `cargo test --workspace --offline --locked -j 2` | `target/p5-test-final-2.log` | 0 |
| `cargo fmt --all --check` | `target/p5-fmt-final-2.log` | 0 |
| `cargo clippy --workspace --all-targets --offline --locked -j 2 -- -D warnings` | `target/p5-clippy-final.log` | 0 |
| `cargo test -p jean-core --test interaction --offline --locked -j 2 -- --nocapture --test-threads=1` | `target/p5-measurements-final.log` | 0 |

Full workspace total: **927 passed, 0 failed, 1 ignored**; the existing live-toast test remains ignored, and opt-in physical skips are not new coverage. `target/p5-validation-summary.json` records totals and exits. Storage was checked before substantial builds: 8,605,786,112 bytes free before final validation (`target/p5-storage-final.txt`), 8,160,854,016 afterward. No caches or existing evidence were deleted. Lockfile comparison against `target/p4demo/Cargo.lock.before2` found 340 → 341 package identities: only local `jean-voice` was added, with no external version/checksum changes.

### 56.5 Native feasibility and exact remaining work

Official sherpa-onnx **2.0.1** metadata/source was inspected, not installed: Apache-2.0 Rust wrapper, default static linking, Windows x64 static-MT release archive **122,863,596 bytes**. Its build script accepts `SHERPA_ONNX_LIB_DIR` or `SHERPA_ONNX_ARCHIVE_DIR`, otherwise downloading native libraries automatically; no dependency was added that would trigger that prohibited download. Evidence: `target/p5-sherpa-cargo.toml`, `target/p5-sherpa-build.rs`, `target/p5-sherpa-release.json`.

Official streaming transducer configuration/`accept_waveform`/decode APIs and VITS `generate_with_config` with a cancellable callback were checked (`target/p5-sherpa-asr.rs`, `target/p5-sherpa-tts.rs`). The referenced ASR source model card declares Apache-2.0 (`target/p5-asr-model-card.txt`), but no exported bundle was downloaded or verified. Amy-low's card says dataset license **“See URL”** and identifies Ryan-low fine-tuning; it does not establish explicit redistribution terms (`target/p5-amy-model-card.txt`). Model licensing and Windows CRT/link compatibility remain unresolved runtime-integration checks.

Remaining Phase 5:
- Authorize/provision license-verified assets and native libraries; implement/link real streaming ASR, VAD and TTS. Preserve bounded control during inference rather than blocking the worker's synchronous engine boundary indefinitely.
- Implement microphone capture, physical playback, real PTT/hotkey controls and acoustic barge-in; measure actual latency, model load and peak memory. No microphone was accessed and no audio was played in this batch.
- Add conversational status/cancel routing, automatic progress announcements and demonstrate the roadmap's full spoken 60-second GUI-task interruption/update/cancellation scenario. Current controls are typed IPC with fixture speech, not that end-to-end demonstration.
- Implement searchable history/FTS5 and user-approved long-term preferences/facts. Durable owner reconnect/submission recovery is also not provided by this foundation.

No software/tools/native libraries/models were installed or downloaded; only research text/metadata was fetched. No paid services/subagents, private chats/profiles, machine-wide changes, commits, pushes or publication were used. Wake-word detection, tray UI, packaging and Phase 6 were not implemented.

---

## 57. Phase-5 searchable history and explicit memory — 2026-11-07

**Delivered: local FTS5 history search and operator-controlled saved facts/preferences through the CLI. The independently supervised task/voice architecture is unchanged** — `interaction.rs`, `task_worker.rs` and both `workers/voice` sources are byte-identical to §56 (SHA-256 in `target/p5m-validation-summary.json`). Native speech remains open; Phase 4's open cases are untouched.

### 57.1 What shipped

| Capability | Implementation |
|---|---|
| Schema v5 | `crates/jean-store/src/migrations.rs`: `tasks_fts`, `events_fts`, `saved_facts_fts` (FTS5, `unicode61 remove_diacritics 2`), `saved_facts`, `preferences`, `memory_changes`. Append-only; v1–v4 unedited; existing rows backfilled inside the migration transaction |
| Index synchronisation | AFTER INSERT/UPDATE/DELETE triggers on all three sources, so every existing write path — including `prune_events` and a second JEAN process — keeps the index in step without knowing it exists |
| Index redaction | `jean_redact` / `jean_event_text` SQLite scalars registered before migration. Redaction happens **before** truncation, at schema level, so a legacy row written in the clear is indexed redacted. `worker_progress` and `model_responded` details are not indexed at all |
| Bounded search | `crates/jean-store/src/memory.rs`: 256-character queries, ≤100 results, 12-token snippets, `PRAGMA query_only` for the read window, and a 2 s interrupt-handle watchdog that stops an over-budget query rather than falling back to a scan |
| Literal vs syntax | Default is one quoted FTS5 phrase (`""` escaping); `--fts-syntax` opts into expressions. Both bind parameters. Malformed expressions raise an actionable error, including when FTS5 fails at row-step time |
| Saved facts | Identity, `operator_command` provenance, timestamps, optional scope; uniqueness on (scope, normalised text). Duplicate add returns the existing record; update into another fact's words is refused by id; forget returns the record and leaves no content in the audit |
| Preferences | Four keys with real readers: `voice.enabled`, `voice.mode`, `desktop.enabled`, `desktop.retention` (`crates/jean-protocol/src/preferences.rs`). Values type-checked and canonicalised; the candidate configuration is validated before the row is written; `JEAN_NO_VOICE` is applied last |
| CLI | `jean history search/status/rebuild`, `jean memory add/list/search/get/update/forget`, `jean preferences list/get/set/clear` (`apps/jean-cli/src/memory.rs`). Output escapes control/bidi characters and is capped at 2048 bytes of escaped text |
| Bounded read interface | `jean_core::memory::MemoryView`: ≤20 facts, ≤4000 characters, reports truncation, excludes history. **Nothing injects it automatically** — the planner is not a caller |

**Deviations, stated:** facts use a dedicated `saved_facts` table rather than key/value rows in `preferences`, because a fact has its own identity and lifetime and a synthetic key is where silent overwrites come from. Only four preference keys are supported; the settings document's longer list (`ui.theme`, `model.provider`, `workspace.root`, …) has no reader in this build and is refused rather than stored inert.

### 57.2 Guarantees and limits

- Search is read-only and grants nothing: `query_only` is set for the window, hits carry ids/timestamps/state/outcome/snippet with no executable field, and tests assert no task, approval or preference row appears after reading instruction-shaped text.
- Only explicit operator commands write memory. `saved_facts.source` and `memory_changes.provenance` are CHECK-constrained to `operator_command`, so there is no label a model, transcript, tool result or worker could write. **The CHECK constrains the value, not the caller**: the guarantee that writes come only from the CLI is the call graph, and the schema is the backstop.
- `memory_changes` commits in the same transaction as the change and holds only action, record id, provenance and time — no text, scope, value or content hash. A log of a deletion must not be a second copy of what was deleted.
- Credentials: the store writer applies the shipped pattern floor and refuses credential-shaped text or scope; `jean-core` redacts first and refuses if redaction changed anything or a pattern failed to compile. **The limit is the redactor's** — patterns recognise shapes, so a secret in an unknown shape can still be saved; `process.redact_patterns` is the operator's to extend.
- Scope: no scope means **global only**; a scope means global plus that exact project. Tests prove one project's facts never appear in another's listing or search.
- The scalars are per-connection, so an external writer (e.g. `sqlite3.exe`) cannot satisfy the triggers and its write is **refused** — fail-closed, and hand-editing history requires a JEAN connection.
- Tokenisation: a run of Han characters is one token, so the whole run is findable and a substring is not. This build does not word-segment CJK; a test pins the behaviour.
- `store.retain_events_days` still has **no production caller** (`prune_events` is exercised only by tests). The delete trigger means that if pruning runs the index follows; this batch wired no new deletion path and purged no existing user data.

### 57.3 Evidence

FTS5 was verified in the shipped binary rather than assumed from `bundled`: `PRAGMA compile_options` reports `ENABLE_FTS5`, and a real virtual table is created, matched and snippeted (`crates/jean-store/tests/fts5_availability.rs`). No incompatibility was found, so no fallback scan exists in the code.

New suites: `fts5_availability` 4, `history_memory` 24, `memory_review` 8, `cli_memory` 7, plus protocol preference tests. Coverage includes v4→v5 backfill, new/updated rows searchable, live state on hits, pruning and orphan drift with rebuild, unicode and malformed queries, bounded results/snippets, fact lifecycle, scope isolation, duplicates/conflicts, credential refusal, legacy clear-text redaction, metadata-only auditing, and CLI operation with `provider = none` and voice disabled.

| Command (`CARGO_INCREMENTAL=0`, `-j 2`, `--offline --locked`) | Log | Exit |
|---|---|---:|
| `cargo build --workspace --all-targets` | `target/p5m-build-final.log` | 0 |
| `cargo test --workspace` | `target/p5m-test-final.log` | 0 |
| `cargo fmt --all --check` | `target/p5m-fmt-final.log` | 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | `target/p5m-clippy-5.log` | 0 |

**995 passed, 0 failed, 1 ignored** across 58 suites (the existing live-toast test). Failed attempts are retained (`p5m-check-1..7`, `p5m-store-test-1..11`, `p5m-clippy-1..5`, `p5m-cli-test-1`); causes included a renamed error classifier, `snippet()` returning NULL for an unmatched column, `progress_handler` requiring `&mut` (replaced by the watchdog), per-connection scalars refusing an external writer, and three wrong test expectations. Lockfile stayed at **341 packages**: only `rusqlite` features `functions` and `hooks` were enabled on the existing crate — no new dependency, no download. Disk: 7,997,718,528 bytes free before the final build, **4,405,497,856 after**; artifacts consumed ~3.3 GiB and nothing was deleted.

### 57.4 Remaining Phase 5

Native sherpa linking/inference, license-verified assets, VAD, microphone capture, physical playback, hotkey/acoustic barge-in and real speech latency/memory measurements; conversational status/cancel routing and progress announcements; the full spoken 60-second GUI-task exit demonstration; durable owner reconnect/submission recovery. Richer memory is delivered as search plus explicit facts/preferences, but **no automatic promotion, no vector store and no model-context injection** — wiring `MemoryView` into a prompt is a later decision that must also decide how a retrieved fact is framed against live instructions. Phase 6, wake word, tray and packaging were not started.

Two subagents were launched for storage hardening and CLI integration; both terminated on a credit limit. The CLI/core/protocol files they had written were kept and verified, storage hardening was completed directly, and every claim above was re-checked against the final test run rather than an agent report. No software was installed, no models or native libraries downloaded, no microphone or audio device accessed, no paid API contacted, no machine-wide setting changed, and nothing was committed, pushed or published.

---

## 58. Phase-5 native speech: the audio path, inference off the control loop, and the asset boundary — 2026-11-07

**Delivered: a real Windows audio implementation that compiles, a bounded push-to-talk capture path, engine ownership moved off the control loop so that cancellation stays responsive during a long recognition or synthesis, spoken status/cancel and opt-in announcements, and a spoken-facing configuration and CLI. NOT delivered: no native speech ran.** The sherpa-onnx dependency is still a commented line, so `workers/voice/src/sherpa.rs` has never been compiled; `workers/voice/src/wasapi.rs` has been compiled but never executed; no microphone was opened, no audio was played, no model was loaded, and nothing was downloaded. **Phase 5 therefore remains open**, and §58.7 lists the exact steps that close it. §56's concurrency architecture and §57's searchable memory are both intact and both still pass.

> **Corrected by §59 (same day).** Two sentences above are no longer current. The dependency is **not** a commented line any more — it is a real `optional = true` dependency behind `dep:sherpa-onnx`, and `sherpa.rs` **has** been compiled against the published 2.0.1 crate, which found one genuine signature error of mine (§59.3). And "nothing was downloaded" now needs the distinction §59.4 draws: the Apache-2.0 **crate source** was fetched when the brief asked for the dependency to be wired, while the 122 MB **native archive** was refused by both of the build script's declared-provisioning branches and never arrived. §58.6's package count and §58.7's step 2 ("uncomment") are corrected there. What §58 got right and §59 did not change: no link, no inference, no transcript, no microphone, no speaker, and TTS still pending.

### 58.1 What shipped

| Capability | Implementation |
|---|---|
| Device-free audio core | `workers/voice/src/audio.rs` (556 lines): `Pcm`/`SampleFormat`/`DeviceFormat`, `to_mono_f32` (audio.rs:85) averaging channels and refusing a trailing partial frame, a chunked linear `Resampler` (audio.rs:160) that carries its position across packets and exposes `flush()`, and `BoundedPcm` (audio.rs:215) which drops the **oldest** frames on overflow and counts the drops instead of hiding them |
| Push-to-talk state machine | `audio.rs:273` `PttState{Idle,Opening,Capturing,Finalizing}` with `opened()` (audio.rs:361) and `release()` (audio.rs:377): one activation at a time, a release is honoured from *either* `Opening` or `Capturing`, and the budget comes from `voice.finalize_budget_ms` |
| Device boundary | `device.rs:71` `AudioBackend: Send + Sync` with `devices`/`open_capture`/`open_render`; the streams themselves are deliberately **not** `Send`. `CancelFlag` (device.rs:38) is the `Arc<AtomicBool>` shared into inference and rendering. `FixtureAudio` (device.rs:136) scripts packets, device loss, refused opens and render failures |
| Real Windows audio | `workers/voice/src/wasapi.rs` (500 lines, `#[cfg(windows)]`): `IMMDeviceEnumerator` endpoint listing, event-driven capture with `IAudioCaptureClient`, padding-bounded render with `IAudioRenderClient`, silent-packet zero-filling, `CoTaskMemFree`-owning mix-format guard, and `AUDCLNT_E_DEVICE_INVALIDATED` mapped to `DeviceLost`. **Compiled and linted; never run.** |
| Engine boundary | `workers/voice/src/speech.rs`: `Recognizer{begin,feed,finish}` and `Synthesizer{synthesize}` both take `&CancelFlag`; `EngineFactory` is the only `Send + Sync` object and it is path-only. The fixture's own label says `"fixture (no inference, no device)"` |
| Inference off the control loop | `runtime.rs:69` `recognition_thread` / `:220` `synthesis_thread` create their engine **once** and own it for life; `capture_once` (runtime.rs:112) and `speak_once` (runtime.rs:260) run there, not in the worker's message loop |
| Native adapter | `workers/voice/src/sherpa.rs` (290 lines, `#[cfg(feature = "sherpa")]`): streaming transducer ASR, optional Silero VAD, cancellable `generate_with_config`. Its header records which upstream source files each signature came from. **Never compiled in this batch.** |
| Asset contract | `assets.rs:15` `required()` (6 keys) and `optional()` (`tts_lexicon`, `vad`, `native_lib_dir`); `native_unavailable` (assets.rs:78) distinguishes *"built without the `sherpa` feature"* from *"sherpa-onnx is linked but the configured speech assets are unusable"*, naming each key and pointing at `docs/voice-assets.md` |
| Worker | `lib.rs:182` `event_loop`: drain both engine channels, expire deadlines, then block on commands with a 10 ms timeout; every deferred reply is paid whether or not the engine answered |
| Spoken interaction | `crates/jean-core/src/speech.rs`: `spoken_control` (:29) matches an exact normalised vocabulary only — four phrasings for status (`status`, `task status`, `report status`, `what is the status`) and five for cancel (`cancel`, `cancel task`, `cancel the task`, `stop task`, `stop the task`), and nothing at all unless a task is active — and `Announcements` (:69) dedupes per (task, state), caps at 4 and drops entries whose task was superseded |
| Controller wiring | `interaction.rs:392` intercepts a spoken control **before** submission, :649 `pump_announcements` speaks only into a free utterance slot on a ready worker, :723 uses the new `configure_timeout_ms` for model loading |
| Configuration | `config.rs`: 9 new `[voice]` keys and 2 new `[voice.models]` keys, each with a reader; `AudioBackend` (:68), `audio_backend()` (:203), `Hotkey` (:221) with `parse` (:240) requiring Ctrl/Alt/Win and `virtual_key` (:296) |
| CLI | `jean voice devices \| doctor \| ptt` (`phase5.rs:94`), with `ptt` (:170) driving press → release → transcript → submit and `hotkey_down` (:259) reading `GetAsyncKeyState` for the configured binding |
| Documentation | `docs/voice-assets.md` (277 lines): the asset manifest, licences, directory layout, provisioning commands and the four evidence tiers |

`[voice]` is **no longer a deferred section** — `DEFERRED_SECTIONS` (`crates/jean-protocol/src/config.rs:33`) is now `["vision", "agents", "telemetry"]`, because every key in it is read by this build.

### 58.2 The blocking problem, and the cost of solving it

The requirement was that inference and playback must not block the voice control loop. `sherpa`'s engine objects and the WASAPI COM interfaces are both `!Send`, so they cannot be moved into a task per request and cannot be shared with the loop. The shape that satisfies both facts is **two long-lived, thread-affine engine owners**: the control loop sends a request down a channel and continues, and the engine thread replies when it is done. Cancellation is an `Arc<AtomicBool>` that the engine checks *between decode steps*, in the flush loop, and inside the TTS generation callback — the callback returns `false` when the flag is set, which aborts sherpa's generation rather than waiting for it.

Two consequences are measured rather than asserted:

- **A cancelled press produces no transcript.** `capture_once` returns through a guarded arm that discards the result when the flag is set (a test holds a 20-second flush and gets control back in under 500 ms with no `Transcript` event emitted). Cancellation is not rollback: whatever the recogniser finished is simply not submitted.
- **An interrupt round trip now costs milliseconds, not microseconds.** `§56` measured 252.6 µs; this tree measures **22.19 ms** for the same 24-interrupt fixture (`target/p5e-measured-final.log`). The reason is `lib.rs:188`: `std::sync::mpsc` cannot select across the command channel and the two engine-event channels, so the loop polls with a 10 ms timeout and a control message waits up to two intervals. Responsiveness against a 20-second inference is the requirement this batch was given; equalising the interrupt latency back is the price it paid, and the fix is a merged event channel or `WaitForMultipleObjects` on the handles. It is listed as open, not glossed.

Replies are paid *deferred and unconditionally*: `Opened` always pays the owed `Capturing`, and `abort_capture` (lib.rs:358) pays both owed replies, so the controller is never left holding a request the worker decided not to answer.

### 58.3 Authority and the four cancellation scopes

Nothing about who may act changed. A transcript becomes a task only through an explicit `SubmitTranscript` bound to the activation that produced it; `jean voice ptt` submits only what it just heard, and the transcript test asserts one submission per press. A spoken phrase can report or cancel — never start work — and it is intercepted before the pipeline, so a control cannot smuggle a goal. Ambient audio, TTS output and announcements are not inputs to anything that authorises a command. There is no always-on listening: capture begins at `BeginCapture` and ends at release or `voice.capture_timeout_ms`, and no wake word exists (D-013 still rules out the pretrained openWakeWord weights).

Utterance / task / voice-worker / application scopes stay separate: `Interrupt` stops playback through a control message and touches nothing else; `CancelTask` addresses one task id and its process tree; stopping the voice worker leaves the task running; `Shutdown` takes both. `drain_synthesis` (lib.rs:377) replies `SpeechStopped` to the interrupt's own request id when an interrupt is what ended the utterance, and to the `Speak` id otherwise — the two are not conflated.

### 58.4 Evidence, kept in the four tiers the code is separated into

| Tier | Status | What it proves | What it does **not** prove |
|---|---|---|---|
| 1 — device-free deterministic | **run**: 40 new tests | format round-trips, stereo averaging, partial-frame refusal, resampling continuity across packet boundaries, 48 kHz→16 kHz conversion, the buffer bound (a 10 000-frame packet into a 100 ms / 1600-frame ceiling: `*fed == vec![1600]` so only the bound reaches the engine, and `dropped: 8400` is reported to the control loop rather than hidden), PTT transitions including release-during-`Opening`, device loss, refused open, render failure, cancellation inside a 20 s flush, pre-cancelled synthesis never opening a speaker, no duplicate submission | that any of it works against a real device or model |
| 2 — native compile/link | **partial**: WASAPI compiles and passes clippy `-D warnings`; sherpa does not compile because it is not in the tree | the `windows` 0.62.2 audio surface is used correctly — packed `WAVEFORMATEX` read by value, `PROPVARIANT` unions handled, COM release ordered before `CoUninitialize` | that sherpa's signatures are right; the adapter was written from upstream sources and has never met a compiler |
| 3 — real inference | **not run** | — | everything; requires assets the batch was forbidden to download |
| 4 — physical audio | **not run** | — | microphone capture, speaker output, device loss on real hardware, latency, model load time, peak RSS |

Tier 1 is `workers/voice/tests/speech_path.rs` (12), `jean_voice` unit tests (16), `apps/jean-cli/tests/cli_voice.rs` (6) and `crates/jean-core/tests/interaction.rs` (13, of which 5 are new spoken-control/announcement/finalization tests).

### 58.5 Two defects found by running the feature rather than a fake

- **A release during device startup killed the worker.** The first PTT draft treated `Opening` as not-yet-capturing and refused the release, so a quick press hit `push-to-talk is not capturing` and the worker died on a protocol violation. The state machine now accepts release from `Opening`, `opened()` returns whether the press is still live, and both deferred replies are paid on abort. Pinned by `a_press_released_before_the_device_opened_still_completes` (`audio.rs:542`).
- **The handle assertion was passing vacuously.** `supervised_task_voice_status_cancel_restart_and_resource_bounds` failed 3/3 in isolation (before=111, after=117) while passing in every full run. A throwaway probe showed the *first* worker lifecycle in a process pays a one-time six-handle warm-up that no later cycle repeats, so the assertion was really measuring test ordering. The fix warms up before measuring and reports the delta; the probe was deleted. This is a correction to a §56-era claim, not a new measurement of speech.

One open item from before this batch is unchanged: `browser_live` intermittently failed three times and passed twice. It was proved outside this diff (no browser file references `voice`, `VoiceConfig` or `Controller`), the mechanism is `crates/jean-core/src/browser.rs:284` removing a profile directory a Chrome child still holds, and it was deliberately **not** fixed here.

### 58.6 The no-download boundary

`workers/voice/Cargo.toml` carries a `sherpa = []` feature and the dependency itself as a **comment** with the measured reason: `sherpa-onnx-sys`'s build script resolves `SHERPA_ONNX_LIB_DIR`, then `SHERPA_ONNX_ARCHIVE_DIR`, and otherwise downloads a 122,863,596-byte archive. The default build therefore loads no speech assets, links no native library and starts no worker when `[voice] enabled = false` or `JEAN_NO_VOICE` is set. `Cargo.lock` stayed at **341 package identities**; the batch added dependency *edges and features* (the audio feature list in `workers/voice`, `windows` promoted to a regular `jean-cli` dependency), which required one re-resolve without `--locked`; no new package entered the tree. **This last clause is the one §59.2 replaced:** wiring the dependency as optional — the arrangement §58 rejected in favour of a comment, on a claim about `--locked --offline` that measurement disproved — puts 39 more package identities in the lock (341 → 380), none of them compiled in the default build. `jean voice doctor` reports the native state honestly and does **not** enumerate endpoints, so a diagnostic cannot itself touch a device.

`backend = "sherpa_onnx"` with the feature off is a **refusal**, not a fallback: `engine_factory` (lib.rs:44) returns `assets::native_unavailable(config, false)` and the worker fails closed. A placeholder is never reported as working speech — the fixture backend says so in its own label, and `audio_backend()` (config.rs:203) deliberately offers no key that pairs a fixture engine with a real microphone.

### 58.7 Exact remaining provisioning and live-validation steps

All of it is written out in `docs/voice-assets.md` §1–§7; nothing in this repository performs it. The short form:

1. **Native libraries** (§1): download `sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2` yourself — 122,863,596 bytes, `sha256:23e7b986b197eda0bbc47ff3d7d5c87afd6ef2b9077214ba26b0f1ec52ef0e66`, release published 2026-10-26 — verify the digest, unpack, and point `SHERPA_ONNX_LIB_DIR` at `<dir>/lib`. Static `-MT` linking matches the CRT this workspace already builds with.
2. **Uncomment** the `sherpa-onnx` dependency in `workers/voice/Cargo.toml`, then `cargo build -p jean-voice --features sherpa`. *This is the first time `sherpa.rs` will be compiled; expect signature drift.*
3. **ASR bundle** (§2): `encoder-epoch-99-avg-1.int8.onnx`, `decoder-epoch-99-avg-1.onnx`, `joiner-epoch-99-avg-1.int8.onnx` and `tokens.txt` from one unpacked directory into `voice.models.{encoder,decoder,joiner,tokens}` — from `sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2`, 506,956,414 bytes measured with **no published digest**, Apache-2.0 on the exported weights per the model card inside the bundle. The planning table's earlier "~169 MB lean / ~415 MB full" provisioning estimate was wrong by more than 3× and `PLANNING/03-DEPENDENCIES.md` §6 now carries the measured figures instead.
4. **VAD** (§3, optional): Silero `silero_vad.onnx` (MIT, verified) — only usable at `sample_rate = 16000` with a 512 window, which `sherpa.rs` enforces before it is created.
5. **TTS** (§4): **blocked on licence, not on code.** The model card's redistribution terms are not explicit enough for this repository to ship or fetch it, and a dataset licence is not a voice-model licence. Synthesis stays fixture-only until a bundle with explicit terms is chosen.
6. Then live validation, tier by tier (§7): compile/link → one model load → one microphone press against a known phrase → speaker output and interrupt — measuring actual latency, load time and peak RSS, none of which this batch has a number for.

Still open in Phase 5 after assets: acoustic barge-in (**not implemented** — `Interrupt` is a control message, and claiming otherwise would be false); the 60-second spoken GUI-task interruption/update/cancellation demonstration; `PKEY_Device_FriendlyName` selection against real hardware; the 10 ms poll interval in §58.2; a non-linear resampler if fixture-quality audio proves insufficient (`LinearResampler` exists upstream and is unused here).

### 58.8 Validation

Sequential, through `target\dev.cmd`, with `CARGO_INCREMENTAL=0`, `-j 2` and `--offline --locked`; the `[exit]` markers in the retained logs are the evidence, not the wrapper. The four commands that predated the last formatting pass were re-run on the final tree.

| Command | Retained log | Exit |
|---|---|---:|
| `cargo build --workspace --all-targets --offline --locked -j 2` | `target/p5e-build-final2.log` | 0 |
| `cargo test --workspace --offline --locked -j 2` | `target/p5e-test-final2.log` | 0 |
| `cargo fmt --all -- --check` | `target/p5e-fmt-final3.log` | 0 |
| `cargo clippy --workspace --all-targets --offline --locked -j 2 -- -D warnings` | `target/p5e-clippy-final2.log` | 0 |
| `cargo test -p jean-core --test interaction --offline --locked -j 2 -- --nocapture --test-threads=1` | `target/p5e-measured-final.log` | 0 |

**1035 passed, 0 failed, 1 ignored** across 61 test-result blocks (59 in `target/p5m-test-final.log` — §57's "58 suites" undercounted its own log by one). The +40 is `jean_voice` unit tests 2→16, `speech_path` 0→12, `cli_voice` 0→6, `interaction` 8→13, `jean_core` unit tests 180→183. The ignored test is still the pre-existing live-toast one; no physical-audio test was added to the pass count. Measurements: status 26.3 µs, maximum of 24 interruptions 22.1949 ms, cooperative task-tree cancellation 4.3694 ms, owner handles 113→113. `target/p5e-validation-summary.json` records totals, deltas, exits and what was not executed.

Failed attempts are retained (`p5e-check-1..3`, `p5e-test-voice..7`, `p5e-clippy..3`, `p5e-fmt*`, `p5e-test-all*`, `p5e-measured.log`). Their causes, in order: three rounds of real compile errors in the WASAPI bindings (packed-struct borrows, `unsafe` statement placement), the `--locked` edge re-resolve, a `std::sync`/`mpsc` import alias collision, `Iterator::is_empty`, two clippy lints, thirteen dead-code warnings from modules that were not yet `pub`, an incomplete `CaptureScript`→`FixtureScript` rename, two wrong test expectations of my own, the `Opening`-release defect, and the vacuous handle assertion.

**Disk.** The volume ran down during this batch and `link.exe` failed with exit 1318 at roughly 30 MiB free. The reported figures were measured, the volumes and sizes were listed, and permission was asked before anything was removed; on "continue" **14 orphaned `.pdb` files under `target/`** were deleted after verifying nothing referenced them (~1.12 GiB). No source, test, fixture or evidence file was deleted, and the remaining space recovered on the volume was Windows-side, not mine. Free space: 25,350,975,488 bytes before the final builds, 25,293,971,456 after (`target/p5e-validation-summary.json`).

No subagent, paid API or delegation was used. Nothing was installed, downloaded, committed, pushed or published; no machine-wide setting was changed; no microphone was opened and no audio was played. Phase 6, wake word, tray and packaging were not started.

---

## 59. Phase-5 native compilation, the download guards, and the microphone-free ASR path — 2026-11-07

> **Corrected by §60 (same day).** Three statements below are no longer current.
> §59.1's "absent" is obsolete: both archives were supplied, checksum-verified and
> extracted. §59.5's "link with the real archive — cannot be tested here" has been
> tested, and the probe's prediction held exactly. §59.9's "native inference never
> ran" is false — it ran, on three published clips, and produced correct
> transcripts. What §59 established and §60 did not change: the dependency wiring,
> the executed download guards, the recognition/synthesis asset split, the Kokoro
> selection with TTS still pending, the CRT configuration options, and the fact
> that no physical audio was touched.

**Delivered: `sherpa.rs` now compiles against the real sherpa-onnx 2.0.1 crate — for the first time — and the one signature error that compile found is fixed; the dependency is genuinely wired as an optional feature; both of the build script's non-download branches were executed and neither fetched a binary; the CRT/link question was measured with a probe instead of read off the archive name, which corrected a false claim this document and `docs/voice-assets.md` were both making; recognition and synthesis provisioning were separated, fixing a defect that refused the whole engine when only TTS was missing; Kokoro is selected as the synthesis bundle on an explicit weights licence; and `jean voice transcribe` gives tier-3 evidence without a microphone.** **NOT delivered: the native link, model load, transcript, synthesis, or any physical audio** — the native library archive and the ASR bundle are both absent from this machine (§59.1 names the exact paths). No cancellation hang was observed, so there is no reproduction to preserve; what would count as one is written into §9 of the operator procedure.

### 59.1 The two prerequisites, named as missing paths

| Prerequisite | Exact path the code looks for | State measured |
|---|---|---|
| Native library archive | `sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2` (122,863,596 bytes, `sha256:23e7b986…f0e66`) inside `SHERPA_ONNX_ARCHIVE_DIR`, or its extracted `…\lib` as `SHERPA_ONNX_LIB_DIR` | **absent.** Neither variable was set at batch start; `find` over Desktop/Downloads/Documents/AppData returned no `sherpa*` and no `.tar.bz2`; `target/sherpa-onnx-prebuilt` is 0 bytes after the guard runs |
| ASR bundle | `%LOCALAPPDATA%\JEAN\models\sherpa-onnx-streaming-zipformer-en-2023-06-21\{encoder-epoch-99-avg-1.int8.onnx, decoder-epoch-99-avg-1.onnx, joiner-epoch-99-avg-1.int8.onnx, tokens.txt}` | **absent.** `%LOCALAPPDATA%\JEAN\` contains only `artifacts`, `audit.jsonl`, `jean.db`, `logs`. The only `.onnx` anywhere under the profile is an unrelated MiniLM embedding model in `C:\Users\4Raisan\.ruvector\models\` |
| A prerecorded non-sensitive clip | any WAV the operator or this repository supplies | **none exists in the tree, and none was recorded** — so §59.8's command has no fixture of its own |

Because both are absent, the ASR demonstration in the batch brief cannot run here: there is no expected phrase and no actual transcript to record. The independent work below is what does not need them.

### 59.2 What wiring the dependency actually cost

`workers/voice/Cargo.toml` now has `sherpa = ["dep:sherpa-onnx"]` and an `optional = true` dependency on the workspace pin (`=2.0.1`, `default-features = false`, `features = ["static"]`) — the commented line from §58 is gone.

| Measured | Result |
|---|---|
| `cargo tree -p jean-voice -e normal` (feature off) | **0** sherpa packages — optional really does mean not built |
| `cargo tree -p jean-voice --features sherpa -e normal` | `sherpa-onnx v2.0.1` → `sherpa-onnx-sys v2.0.1` |
| `Cargo.lock` | 341 → **380** package identities, +39 added, 0 removed (`target/p5n-Cargo.lock.before`) |
| Default workspace build after that | `--offline --locked` **`[exit] 0`** (`p5n-default-offline-locked.log`) |
| Before the one online resolve | `cargo check -p jean-voice --offline` → `no matching package named sherpa-onnx found` (`p5n-default-offline.log`) |

Two consequences that are worth stating rather than smoothing over: the +39 includes C-building packages (`zstd-sys`, `bzip2-sys`, `libbz2-rs-sys`) and a **second `ureq`** (2.12.1, the archive fetcher) beside this workspace's 3.5.1; and adding an optional dependency required **one online resolve** to write the lock. §58's claim that the dependency had to stay commented out because an optional one would break `--offline --locked` for the default build was wrong — it breaks only until the lock is written once.

A trap this batch fell into and should record: `cargo test -p jean-cli` does **not** rebuild `jean-voice.exe`, so two new CLI tests failed against a stale worker binary (`expected --session <id>, --list-devices or --doctor`) until `cargo build --workspace --all-targets` ran. Any CI recipe for this repo must build before testing those suites.

### 59.3 The first compile against the actual SDK

`cargo check -p jean-voice --features sherpa` with `DOCS_RS=1` — the crate's own documented mode in which its build script returns before any download or link directive. That is what makes compiling possible with no archive, and it is compile-only evidence.

- First attempt: **`[exit] 101`** (`p5n-sherpa-check-1.log`) with one real error, mine: `E0609 workers\voice\src\sherpa.rs:51: no field 'joiner' on type OnlineModelConfig`. The transducer trio lives at `model_config.transducer.{encoder,decoder,joiner}` (`online_asr.rs:50`), and the compiler's own note named the fix.
- After the fix: **`[exit] 0`** (`p5n-sherpa-check-2.log`), and `cargo clippy -p jean-voice --features sherpa --all-targets -- -D warnings` is **`[exit] 0`** (`p5n-sherpa-clippy-final.log`).
- So of the API assumptions §58 wrote from upstream source files, one was wrong and the rest typecheck. `sherpa.rs`'s header now says so, with the file and line.
- **Not proved by any of this:** that the symbols exist, that the archive matches, or that inference works. `cargo check` never links.

### 59.4 "Cargo cannot silently download the native archive" — executed

The §58 version of this was a reading of the build script. This batch ran both non-download branches:

```text
SHERPA_ONNX_ARCHIVE_DIR=<dir without the archive>   → p5n-guard-archivedir.log, [exit] 101
  SHERPA_ONNX_ARCHIVE_DIR does not contain expected archive:
  …\target\voice-assets\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2

SHERPA_ONNX_LIB_DIR=<not a directory>               → p5n-guard-libdir.log, [exit] 101
  SHERPA_ONNX_LIB_DIR does not exist or is not a directory: …\target\no-such-lib-dir
```

`target/sherpa-onnx-prebuilt/` was created and left at **0 bytes** by both, and the only fetch in either run was the Apache-2.0 **crate source**, which is what the brief asked to be wired and compiled. The 87 crates that compiled in the first guard run (visible in that log) are the build-script dependencies — that C+Rust tree is a real, measured cost of enabling the feature, not a hypothetical one. Download happens only when *neither* variable is set, which is why §6 of the asset document puts the variable in the same command as `--features sherpa`.

### 59.5 CRT and linker: measured, and a claim in §58 corrected

§58 and `docs/voice-assets.md` §1 both asserted that mixing the `-MT` archive with this workspace's build is "a link error, not a warning". A local probe (`target/p5n-crt/`, sources retained) says otherwise:

| Measurement | Result |
|---|---|
| What `jean-voice.exe` links today | imports `VCRUNTIME140.dll` + `api-ms-win-crt-*.dll`; rustc's own link line carries `/defaultlib:msvcrt` — the **dynamic** CRT |
| `/MT` static library linked into it | **links, `[exit] 0`**, with `LNK4098: defaultlib 'LIBCMT' conflicts with use of other libs`; the probe binary runs |
| Same library with `-C target-feature=+crt-static` | links with no conflict; `jean-voice` itself builds this way (`p5n-crtstatic-voice.log`, `[exit] 0`), and the produced exe imports **no CRT DLL at all** (1,674,752 B vs 1,598,464 B) |
| Link with the real archive | **cannot be tested here** — `p5n-link-attempt.log`, `[exit] 101`: `could not find native static library 'sherpa-onnx-c-api'`, reached with `SHERPA_ONNX_LIB_DIR` pointed at an empty directory so no download path existed |

The honest reading: `-MT` into today's build is a **warning plus a shared-process, two-CRT risk**, not a failed link, and the correct mitigation is configuration (`crt-static`, measured to build) rather than the archive name. What the probe does *not* establish is heap safety across the boundary: a 32-byte `malloc`/`free` round trip returning cleanly is meaningless under load, and this document refuses to present it as evidence. The archive itself still has to be checksum-verified against `sha256:23e7b986…` by the operator — nothing here has seen its bytes.

### 59.6 Synthesis: a bundle selected on its licence, still pending

| | |
|---|---|
| Selected | `kokoro-en-v0_19.tar.bz2` — 319,625,534 bytes, `sha256:912804855a04745fa77a30be545b3f9a5d15c4d66db00b88cbcd4921df605ac7` (GitHub API, measured this session); int8 variant 103,248,205 bytes with its own published digest |
| Source and revision | `hexgrad/Kokoro-82M` at `f3ff3571791e39611d31c381e3a41a3af07b4987`, whose card declares `license: apache-2.0` **on the weights** |
| Why this and not Piper | `rhasspy/piper-voices` answers **HTTP 401** to an anonymous dataset request, and the `amy` card's grant is still "Dataset … License: See URL". A licence a machine cannot read is a licence the operator must take on trust, and the `amy` row stays as the rejected alternative in `docs/voice-assets.md` §4b |
| Code path | `OfflineTtsKokoroModelConfig` wired from upstream `rust-api-examples/examples/kokoro_tts_en_lexicon.rs` at tag `v2.0.1`: model + voices + tokens + lexicon, `sid` from `tts_speaker`, and **no `dict_dir` for English** — which is why that key is measured only when set |
| Status | **PENDING.** Selected, documented and compiled; never downloaded, never created, never synthesised. TTS is not delivered and is not claimed |

### 59.7 A defect that would have blocked the operator's first run

`assets::required()` listed the ASR files *and* `tts_model`/`tts_tokens`, and `SherpaEngine::new` refused the engine if any was absent — so a machine that had provisioned recognition correctly would still have been told speech was unavailable, with synthesis keys in the message. The lists are now `assets::recognition()` and `assets::synthesis()`, with `assets::synthesis_unavailable()` reporting only synthesis and saying "Recognition is unaffected" (`workers/voice/src/assets.rs`). `jean voice doctor` gained `recognition_assets`, `synthesis_assets` and the two provisioning variables as the worker sees them. Pinned by two new tests, one of which points all four recognition keys at a real file and asserts the native message does **not** mention `tts_model`.

### 59.8 `jean voice transcribe` — real ASR evidence with no microphone

The gap between "it compiled" and "someone has to speak into a device" is closed by a file path: `jean voice transcribe clip.wav` (worker flag `--transcribe`, `lib.rs` at the end) reads a WAV, feeds it through the **same `EngineFactory`/`Recognizer` boundary** a live press uses, in 100 ms packets like a WASAPI callback, and prints the transcript with the engine label, both sample rates, frames and duration. Bounds: 64 MiB (`MAX_TRANSCRIBE_BYTES`), exceeded → an actionable refusal telling you to cut the clip.

- `workers/voice/src/wav.rs` (new, 4 unit tests): 16-bit PCM, 32-bit float, stereo averaged to mono, extensible headers decoded through the subtype GUID, and refusals that name what they saw — 24-bit PCM, ADPCM, 64-bit float, no `fmt `/no `data` chunk, and a non-RIFF file.
- Fail-closed and honest: with the feature off, `jean voice transcribe` exits **1** with `native_unavailable` naming all four unset recognition keys (measured by hand, output retained in the session log); with `backend = 'fixture'` it answers **and** labels the transcript as a test double's, because a transcript that looks right is not by itself evidence of inference.
- Two `cli_voice` tests cover both, driving the real binaries with a generated one-second WAV.

### 59.9 Cancellation during inference, and the Sonus question

Nothing new was *observed* to hang, because native inference never ran. What exists and is measured: a 20-second scripted flush is interrupted in **< 500 ms with no transcript delivered**, and TTS generation aborts through the callback — both in tier 1, against the real worker threads (`speech_path.rs`, §58.4). The native path inherits that structure — `sherpa.rs` checks the flag between decodes, in the flush loop, and in `generate_with_config`'s callback — and that is a compile-checked claim, not an executed one.

Per the brief: had native cancellation hung, the reproduction to keep would be the command, the wall-clock timings and whether `jean voice doctor` still answers; that recipe is written into `docs/voice-assets.md` §9 step 6 so the operator captures it in the moment. **The decision to use Sonus is not supported by any evidence from this batch** — there is no hang, and there is also no working inference to compare against.

### 59.10 Validation

`CARGO_INCREMENTAL=0`, `-j 2`, `--offline --locked` except where a fetch is named; `[exit]` markers from the retained logs, run in sequence.

| Command | Log | Exit |
|---|---|---:|
| `cargo build --workspace --all-targets --offline --locked -j 2` | `p5n-build-final.log` | 0 |
| `cargo test --workspace --offline --locked -j 2` | `p5n-test-all.log`, re-run as `p5n-test-final.log` after the last manifest-comment edit | 0 |
| `cargo fmt --all -- --check` | `p5n-fmt-2.log` (after `p5n-fmt-1.log` = 1 with 9 diffs, fixed by `p5n-fmt-fix.log`) | 0 |
| `cargo clippy --workspace --all-targets --offline --locked -j 2 -- -D warnings` | `p5n-clippy-1.log` | 0 |
| `cargo clippy -p jean-voice --features sherpa --all-targets -- -D warnings` (with the build script's skip) | `p5n-sherpa-clippy-final.log` | 0 |
| `cargo check --workspace --all-targets --offline --locked -j 2` | `p5n-check-final.log` | 0 |

**1043 passed, 0 failed, 1 ignored** across 61 result blocks (1035 in §58). The +8 is `jean_voice` unit tests 16→22 (4 WAV reader, 2 asset split, 2 already-counted) and `cli_voice` 6→8. The ignored test is still the live-toast one; **no native, inference or physical-audio test was added to that count**, and the four evidence tiers remain separately labelled in `docs/voice-assets.md` §7.

Failed attempts kept and attributable: `p5n-default-offline.log` (resolution before the lock existed), `p5n-sherpa-check-1.log` (the one real SDK signature error), `p5n-voice-lib-tests.log` (E0716 in my own new test), `p5n-voice-lib-tests2.log` (two wrong WAV expectations — interleaved stereo math and a 38-byte instead of 40-byte extensible header), `p5n-clivoice2.log` (stale worker binary), `p5n-fmt-1.log`, `p5n-guard-*.log` and `p5n-link-attempt.log` (the three intended failures above).

Disk: 25,291,259,904 bytes free at batch start, **23,711,252,480** after the final test run. The decrease
is the feature-on dependency tree (87 build-script crates plus the sherpa source), the probe artifacts and
the usual incremental-free rebuilds. **Nothing was deleted.** `target/voice-assets/`,
`target/sherpa-onnx-prebuilt/` and `target/p5n-crt/` are retained as the evidence for §59.4/§59.5, and
`target/p5n-validation-summary.json` records the counts, deltas, exits and the list of what was not
executed.

`browser_live`'s intermittent teardown race stays open, exactly as §58.5 left it: `crates/jean-core/src/browser.rs:284` removes a profile directory a Chrome child may still hold. It was not run to failure in this batch's suites, and a later pass does not resolve it.

Within Phase 5, with local assets and no machine-wide change: nothing installed, no native archive or model downloaded (the crate source only, by the brief's own instruction), no paid service, no subagent, no commit, push or publication, no microphone opened, no audio played, no settings or permissions changed. Wake word, tray, packaging and Phase 6 were not started.

## 60. Phase-5 native speech provisioned, linked, and hearing real audio — 2026-11-07

**Delivered: both supplied archives located, measured and verified — the native one against its published digest, which its bytes had never been checked against before — extracted byte-exactly into `%LOCALAPPDATA%\JEAN\native` and `\models`; `--features sherpa` **linked** for the first time, `[exit] 0`, with the single `LNK4098` §59.5's probe had predicted and no other linker message; three published clips recognised through the real engine, one of them word-for-word; the isolated test configuration proved through `jean voice doctor`, which reported `sherpa_feature_linked: true`, all four recognition assets present and synthesis still honestly absent; and one diagnostic defect found by actually reading the output and fixed.** **NOT delivered: TTS (the Kokoro bundle was not supplied, so synthesis remains pending and unclaimed), anything at tier 4 — microphone, speaker, push-to-talk, playback, interruption — and cancellation during *live* native inference, which this build cannot reach from configuration at all (§60.7).**

### 60.1 The two archives, as supplied

| | |
|---|---|
| `sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2` | 122,863,596 bytes — matches the GitHub API size. SHA-256 `23e7b986b197eda0bbc47ff3d7d5c87afd6ef2b9077214ba26b0f1ec52ef0e66` — **matches the published digest exactly**, so the sentence in §59.5 that "the archive still has to be checksum-verified by the operator" is closed: this is the documented artifact |
| `sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2` | 506,956,414 bytes — matches the published size. SHA-256 `455f40e556aa2b20ac9d3bffd603b58002075c1193b4070938540c11efe0a4da` — **ours, not a publisher's**: the release API publishes no digest for ASR assets, so this is recorded as "the hash of the file this machine was given", which is a comparison point, not a verification. It was not treated as a mismatch trigger |
| Both originals | still in `C:\Users\4Raisan\Downloads`, untouched. Nothing was deleted and nothing was downloaded: the batch used the two files supplied |

### 60.2 Extraction, and what the native archive really contains

Inspected before unpacking (`tar -tjf`, then `-tvjf` for entry sizes), then extracted
with `tar -xjf -C <destination>`; both returned exit 0.

| Destination | Measured |
|---|---|
| `%LOCALAPPDATA%\JEAN\native\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib\lib` | **12** `.lib` files, 1,057,860,400 bytes. `onnxruntime.lib` alone is 944,135,846 — the ONNX Runtime is linked *statically*, which is why no DLL belongs beside the worker |
| `%LOCALAPPDATA%\JEAN\models\sherpa-onnx-streaming-zipformer-en-2023-06-21` | **13** files, 546,279,902 bytes, including both int8 and fp32 model sets and the bundle's own `test_wavs` |
| Integrity | every extracted file's length equals its archive entry (`target/p5o-verify-extract.py`: 13 matched, 0 mismatched, 0 unexpected; the same comparison for the native set shows the 12 lengths identical) |
| Conflicts | none. `native\` and `models\` did not exist and were created by this batch, so nothing was overwritten |

One documentation error the inspection caught: §5's tree in `docs/voice-assets.md`
claimed the `-lib` archive ships `include\sherpa-onnx\c-api\`. It does not — it is
`lib\` only, and the Rust wrapper declares the C API itself, so nothing needed
headers. Corrected in place.

### 60.3 The configuration stayed isolated

`%LOCALAPPDATA%\JEAN\jean.toml` does not exist on this machine and still does not.
The test config is `target/p5o-data\jean.toml`, reached through `JEAN_DATA_DIR`
pointed at that directory — the documented `<data_dir>\jean.toml` layer — which also
redirects anything the run writes. Four recognition keys hold absolute paths into
`models\`; `backend = "sherpa_onnx"`; every `tts_*` key is deliberately left empty
so no synthesis claim can come from this file.

> **Superseded for the keys, unchanged for the isolation (§61.3).** The same
> scratch file now also holds the Kokoro paths — `tts_model`, `tts_tokens`,
> `tts_voices`, `tts_dict_dir`, `tts_lang`, `tts_speaker` — because synthesis was
> in scope for that batch. Nothing about the isolation changed: `%LOCALAPPDATA%\JEAN\
> jean.toml` was still neither read nor written, `JEAN_DATA_DIR` still pointed at
> `target/p5o-data`, and `voice.enabled` is still off in the operator's own file.

Verified rather than asserted: after every run above, `%LOCALAPPDATA%\JEAN\jean.db`
and `audit.jsonl` still carry their 2026-11-05 12:45 timestamps, and the only new
entries under that directory are `models\` and `native\` — the two this batch was
told to fill. The scratch directory holds its own `jean.db`, which is what the
redirect was for.

### 60.4 The link — tier 2b, achieved on the first attempt

`SHERPA_ONNX_LIB_DIR` set to the extracted `lib` directory, then
`target\dev.cmd build -p jean-voice --features sherpa --offline --locked -j 2` with
`CARGO_INCREMENTAL=0`: **`[exit] 0`**, `target/p5o-build-sherpa.log`. The complete
linker output is one line — `LNK4098: defaultlib 'LIBCMT' conflicts with use of
other libs` — which is precisely what §59.5's probe measured for a `/MT` library
entering today's dynamic-CRT build. **No build-scoped fix was applied or needed**;
`crt-static` remains documented as the alternative, not used. *(§61.4 later built
and ran that alternative as a second preserved executable and removed the warning
there; the default build this section describes still emits it, deliberately, and
D-056 stands.)*

| Fact | Value |
|---|---|
| `jean-voice.exe`, native | 24,688,128 bytes (vs 1,665,536 for the default build) |
| Its imports | still `VCRUNTIME140.dll` + `api-ms-win-crt-*.dll` — dynamic CRT in the exe, static `LIBCMT` inside the linked libraries, i.e. two CRTs in one process, as §59.5 said the warning means |
| Its size | 24,692,224 bytes on the first link (`p5o-build-sherpa.log`), 24,688,128 on the final one after the doctor fix (`p5o-build-sherpa-final.log`); the default feature-off build is 1,665,536 |
| Behaviour under that arrangement | three recognitions, four model loads, clean exits. That is observed small-load behaviour, not a proof of cross-boundary safety, and it is reported as the former |
| `cargo clippy -p jean-voice --features sherpa --all-targets -- -D warnings` | `[exit] 0` (`p5o-native-clippy.log`) — clippy does not link, so this is a second, weaker confirmation of the same signatures |

The older guide's step 1 ("uncomment the dependency") was skipped as instructed:
`workers/voice/Cargo.toml` already carries `sherpa = ["dep:sherpa-onnx"]` plus the
optional dependency, and that manifest is the authority.

### 60.5 Real inference — tier 3, achieved, with expected text beside actual

The ASR bundle ships its own clips and its own transcripts in
`test_wavs/trans.txt`, so the brief's "known, non-sensitive prerecorded clip"
needed no recording: three public-domain passages from *The Scarlet Letter*,
distributed by the model's own authors. Run as `jean voice transcribe`, through
the CLI, against the final binary:

| Clip | Audio | Expected | Actual | Exit | Wall |
|---|---|---|---|---|---|
| `0.wav` | 6.625 s, 106,000 frames @ 16 kHz | `AFTER EARLY NIGHTFALL … THE SQUALID QUARTER OF THE BROTHELS` | **identical, 20/20 words** | 0 | ~3 s |
| `1.wav` | 16.715 s, 267,440 frames @ 16 kHz | `GOD AS A DIRECT CONSEQUENCE … DISHONOURED BOSOM …` | one spelling variant, `DISHONORED`, in 39 words | 0 | ~4 s |
| `8k.wav` | 4.825 s, 77,201 frames from **8 kHz** | `…LESS WITH HOPE THAN APPREHENSION` | ends `…THAN APPREHENS` | 0 | ~2 s |

So the three results stay separate, as the brief demanded: **link** succeeded
(§60.4), **model load** succeeded four times (three int8-configured loads plus one
fp32 set), and a **correct transcript** was obtained for two of three clips, the
third partially. Wall time includes process start and model load; the clip lengths
are reported by the tool itself, not estimated.

`jean voice doctor` against the same configuration reported
`sherpa_feature_linked: true`, `engine = {available: true, label: "sherpa-onnx
2.0.1 (streaming transducer ASR; synthesis engine per config)"}`, all four
`recognition_assets` present, both `synthesis_assets` absent, and
`audio_backend: "System"` — the honest statement that a live run would open a
device. Everything above was re-run against the last binary built in this batch
(`p5o-doctor-verified.json`, `p5o-final-{0,1,8k}.json`): four exits of 0, the same
three transcripts, the path printed as text.

### 60.6 The one imperfect clip was attributed, not excused

> **Corrected by §61.2 (same day).** The last sentence's attribution is **wrong**,
> and the paragraph above it is kept because the reasoning is what §61 disproved.
> The loss was not the 8→16 kHz resample: an independently resampled copy of the
> same clip (scipy, not `audio.rs`) truncates identically, and the cause turned out
> to be the *trailing* audio the transducer needs before it will emit its final
> token. Measured and fixed in §61.2; `8k.wav` now transcribes word-for-word. What
> survives of this paragraph: the fp32/int8 agreement, the uncapped `decode_ready`,
> and the discipline of refusing to call an unattributed failure a model limit.

The 8 kHz clip losing its final word could have been quantisation, could have been
our flush, and could have been the resampler. Measured: pointing the same config at
the **fp32** encoder and joiner reproduced `…APPREHENS` character-for-character
(`p5o-transcribe-8k-fp32.json`), so int8 is excluded; and `decode_ready`
(`workers/voice/src/sherpa.rs:208`) is an uncapped `while is_ready` loop followed by
a 0.3 s silence pad and `input_finished()`, so a truncated decode budget is
excluded. What remains is the linear resample at 8→16 kHz, which §8 of the asset
document had already named as an unverified risk — it is now verified, at the tail
of a word, on a clip the model was not fed at its native rate. No code was changed
for it: the finding is a measurement, and the fix (sherpa's own resampler) is a
decision, not this batch's to make silently.

### 60.7 Two things using the tool taught that building it did not

* **`doctor` printed the provisioning path as `[67, 58, 92, …]`.** `serde_json`
  serialises a Windows `OsString` as a code-unit array, so the one field an
  operator reads to confirm their own `SHERPA_ONNX_LIB_DIR` was unreadable. Fixed
  with `provisioned()` (`workers/voice/src/lib.rs`), which reports lossy text and
  still `null` when unset. Verified: the final run prints the path. Pinned by two
  new unit tests in the same file — one sets the variable and asserts the reported
  value serialises as a path, one clears it and asserts absence is `null` rather
  than an empty string — because a diagnostic that silently reverts to an array is
  the kind of defect that reappears on the next refactor.
* **Native inference cannot be reached without a device, by construction.**
  `VoiceConfig::audio_backend()` (`crates/jean-protocol/src/config.rs:214`) derives
  the audio source from the speech backend: `sherpa_onnx ⇒ System`, `fixture ⇒
  Fixture`. There is no configuration that runs the native engine against the
  scripted device the tier-1 tests use. Consequence for the brief's item 2: the
  cancellation architecture is intact and tested at tier 1, `sherpa.rs` checks the
  flag between decodes and in the flush loop, and compile-checks it — but **native
  cancellation during live inference remains a tier-4 observation**, and the reason
  is this coupling, not a missing asset. Nothing hung, so there is still no
  reproduction and still no evidence for the Sonus question either way.

### 60.8 What is still open, stated as open

TTS: the Kokoro bundle was **not** among the two files supplied, so `tts_model`,
`tts_tokens` and `voices` are still empty, `synthesis_assets` still reports false,
and synthesis stays **PENDING** — selected, compiled, never synthesised. Physical
audio: no endpoint enumerated, no microphone opened, no speaker activated, no
hotkey pressed, no preferences written. Both are recorded as operator steps in
`docs/voice-assets.md` §9, whose pre-flight is now the bundle's own `0.wav`.

### 60.9 Validation

`CARGO_INCREMENTAL=0`, `-j 2`, `MSYS2_ARG_CONV_EXCL='*'`, everything through
`target\dev.cmd`; `[exit]` read from each log.

| Command | Log | Exit |
|---|---|---:|
| `cargo fmt --all` | `p5o-fmt-fix.log`, again as `p5o-fmt-fix2.log` | 0 / 0 |
| `cargo clippy -p jean-voice --features sherpa --all-targets --offline --locked -j 2 -- -D warnings` | `p5o-native-clippy.log` | 0 |
| `cargo build -p jean-voice --features sherpa --offline --locked -j 2` | `p5o-build-sherpa.log`, `-2` after the doctor fix, `-final` after the workspace pass, `-3` after the two new tests | 0 / 0 / 0 / 0 |
| `cargo build --workspace --all-targets --offline --locked -j 2` | `p5o-build-workspace.log` | 0 |
| `cargo test -p jean-voice --offline --locked -j 2` | `p5o-voice-tests.log` (24 unit tests, both new ones green) | 0 |
| `cargo test --workspace --offline --locked -j 2` | `p5o-test-workspace.log` 0 · `p5o-test-workspace-2.log` **101** · `p5o-test-workspace-3.log` with `--no-fail-fast` 0 | see below |
| `cargo clippy --workspace --all-targets --offline --locked -j 2 -- -D warnings` | `p5o-clippy-workspace.log`, `p5o-clippy-workspace-2.log` | 0 / 0 |
| `cargo fmt --all -- --check` | `p5o-fmt-check.log`, `p5o-fmt-check-2.log` | 0 / 0 |

**1045 passed, 0 failed, 1 ignored** across 61 result blocks — §59.10's 1043 plus
the two provisioning tests §60.7 added, and nothing else moved. No test was added to
flatter the batch (those two pin a defect this batch actually caused and fixed) and
none was removed. The ignored test is still the live-toast one, and **no
physical-audio test is in that number**.

**The `browser_live` teardown race fired in this batch, and it is still open.** The
plain workspace run stopped at `[exit] 101` because
`a_page_is_navigated_read_acted_on_and_then_proven_by_a_second_read` panicked at
`crates/jean-core/tests/browser_live.rs:201` with *"a disposable profile was left
behind"* — `…\Temp\jean-browser-live-proven-29632-…\artifacts\browser-t-18e5c13dffa55184-73c0-1-…`.
This is the race §58.5 named and §59.10 recorded as never run to failure; it is now
run to failure, and per the brief it was **not fixed**. Re-running immediately with
`--no-fail-fast` passed every target including that one, which is what intermittency
means: the second green is not a resolution and is not reported as one.

The leak is measured rather than assumed: `%LOCALAPPDATA%\Temp` holds **84**
`jean-browser-live*` directories totalling **≈156 MiB**, accumulated across batches.
Nothing was deleted — clearing them is an operator decision on a 96 %-full C: drive,
not a side effect of a speech batch, and the real fix belongs in the test's teardown
that this batch leaves alone.

Apart from that failure, every non-zero exit in this batch was my own shell work, and
they are kept here because they read like code failures: a bash loop that collapsed
`$M\$f.wav` into one token, so three `jean voice transcribe` calls answered
`could not read … The system cannot find the file specified` (`[exit] 1`) before the
paths were separated and re-run green, and two Python heredocs that died on backslash
quoting — the same trap §58 recorded, still live.

Ordering, because it is a trap the next batch will hit: the workspace build and
tests necessarily rebuilt `jean-voice.exe` **without** the native feature (1,665,536
bytes), which is the shipped default and what the suites are written against. The
native worker was therefore linked last, so the binary now on the machine is the
24,688,128-byte one. Any `cargo build --workspace` between now and the physical test
silently reverts it; re-linking is the single command in §60.4.

Disk: 23,018,287,104 bytes free at the batch start, **20,227,379,200** at the end.
The ~2.6 GiB decrease is the 1.6 GiB of extracted assets and libraries, the feature-on
build artifacts, and the browser profiles above. **Nothing was deleted and no cache
was cleared.** Evidence retained: `target/p5o-*.log`, `p5o-doctor-verified.json`, the
per-clip transcript JSONs (int8, fp32 and final), `p5o-asr-tar-sizes.txt`,
`p5o-verify-extract.py`, `p5o-md-check.py`, `p5o-validation-summary.json`, and
`target/p5o-data\jean.toml`.

`browser_live`'s teardown race is now **observed**, not merely documented, and stays
open exactly as §58.5 left it: `crates/jean-core/src/browser.rs:284` removes a profile
directory a Chrome child may still hold. A later pass does not resolve it — this
batch saw it fail once and pass once, in that order.

Within Phase 5, using only the two supplied downloads: nothing fetched, nothing
installed, no machine-wide change, no paid service, no subagent, no commit, push or
publication, no microphone opened, no audio played, no settings or permissions
changed. Writing into `%LOCALAPPDATA%\JEAN\native` and `\models` was the operator's
explicit instruction for this batch, and the originals in `Downloads` were left in
place; `jean.toml`, preferences and the data directory were not touched (§60.3). TTS
and physical audio remain open, and are reported as open.

---

## 61. Phase-5 speech assets proven against the engine, not the manual — 2026-11-07

**Delivered: the supplied Kokoro archive verified against its published digest, extracted byte-consistently (360 files, 0 missing / 0 extra / 0 size mismatch) and its in-bundle Apache-2.0 `LICENSE` read; the 8 kHz truncation reproduced, separated into end-of-input and final-decode, attributed by measurement to trailing context rather than to the resampler — which §60.6 had wrongly blamed — and fixed, so all three published clips now transcribe; real native synthesis through a file-output seam (24 kHz, 1.03 s, peak 0.332, independently measured from the written file); the first cancellation measurements taken during live native inference, through a supervised worker, with an independent 20-second task that survived them; `LNK4098` traced to the CRT mismatch and removed by a build-scoped flag, verified on the real Sherpa libraries and reported with the DLL import lists of both variants; and three preserved executables that no ordinary workspace build can substitute for one another.**

**NOT delivered: text-to-speech for arbitrary phrases — the engine loads the Kokoro model, voices and tokens and then demands a lexicon file that the supplied bundle does not contain, so the acoustic model is proven and the front end is not; anything on the physical plane (microphone, speaker, push-to-talk, playback); cancellation during *native synthesis*, which remains a tier-1 fixture observation because the `Speak` path takes text and text needs that lexicon; and acoustic barge-in, which is still not implemented and is not claimed.**

### 61.1 The Kokoro archive, as supplied

| | |
|---|---|
| File | `C:\Users\4Raisan\Downloads\kokoro-en-v0_19.tar.bz2`, **319,625,534 bytes** — matches the documented size |
| SHA-256 computed | `912804855a04745fa77a30be545b3f9a5d15c4d66db00b88cbcd4921df605ac7` |
| Documented digest | `docs/voice-assets.md` §4a, from the GitHub API: **identical**, so this is a verification against a published checksum, not a recording of a self-computed one |
| Extraction | `%LOCALAPPDATA%\JEAN\models\tts\kokoro-en-v0_19`, **360 files, 369,315,617 bytes**, `missing 0 / extra 0 / size mismatch 0` against the archive's own `tar tvf` listing (`target/p5p-verify-kokoro-2.py`) |
| Licence | `LICENSE` **inside the bundle** (11,358 bytes) is Apache-2.0, read rather than inferred |
| Contents | `model.onnx` 345,555,491 B, `voices.bin` 5,755,904 B, `tokens.txt` 1,078 B (**177 tokens**, IPA-shaped, not ARPAbet), `README.md` 135 B, and `espeak-ng-data\` (392 files) |
| Originals | `Downloads\kokoro-en-v0_19.tar.bz2` untouched, as instructed |

Two things about the first verification attempt are worth keeping. It reported **398 archive entries against 360 files**, which was not an extraction problem at all: `tar tvf` prints `perms owner/group size date time name`, and the script read `size` from field 1 (the owner) and skipped lines on that misparse. Correct field indices, re-run, 360/360. And the earlier script keyed names with their trailing newline, which made *every* file look missing — the same class of harness bug §60.7 recorded, in my own tooling rather than in the product.

### 61.2 The tail of the 8 kHz clip: reproduced, separated, attributed, fixed

The claim §60.6 closed with — that the lost final word was the linear 8→16 kHz resample — is **false**, and this is how it was disproved rather than argued:

* **Reproduced**: `8k.wav` through the int8 bundle still ended `…THAN APPREHENS`.
* **Independent resample (variant B)**: the same clip resampled to 16 kHz by **scipy**, not by `workers/voice/src/audio.rs`, then fed through the identical engine produced the **identical truncation**. A different resampler, the same loss. The resampler is therefore excluded, and §60.6's attribution is corrected at its own heading.
* **End-of-input separated from final decoding (variant C/D)**: appending **100 ms of real silence** to the *scipy-resampled* file recovered `…APPREHENSION`. So the stream did reach `input_finished()` and the decoder was not capped — the model simply had not yet earned its last token. The controlling variable is trailing audio, not decode budget.
* **Bracketed**: 0 ms external on top of the internal 300 ms pad fails, 100 ms external (400 ms total) passes. The boundary is between 300 and 400 ms of trailing audio after the speech.
* **A hypothesis of mine that measurement killed first**: reordering the pad so it decodes *before* `input_finished()` was implemented, built, and run — `P0000` failed identically before and after, so the reorder changed nothing observable. It was reverted rather than kept as an unverified "improvement".

The fix is the smallest one the measurement supports: `workers/voice/src/sherpa.rs:286` pads **0.5 s** of silence at the engine rate before `input_finished()`, bounded so a release cannot become an unbounded decode. Regression on the clips that never needed it: `0.wav` 20/20 words exact, `1.wav` 38/39 (the same `DISHONORED` spelling variant as §60.5), `8k.wav` **8/8 words, `…LESS WITH HOPE THAN APPREHENSION`** — verified twice more in this batch on both CRT variants (§61.4).

### 61.3 Native synthesis: what the engine accepted, what it refused, and the seam that reached the model

`jean voice synthesize <out.wav> <phrase>` (`apps/jean-cli/src/phase5.rs`, worker `--synthesize`) writes a WAV and reports the levels it measured before encoding. With the Kokoro keys configured and **no lexicon**:

```
--kokoro-lang is deprecated and ignored in sherpa-onnx >= v2.0.0. espeak-ng is no longer used.
   Language is determined by the lexicon files you provide.
No lexicon available. Please provide --kokoro-lexicon
Failed to convert '…' to token IDs
```

`voice.models.tts_lang` is therefore an ignored key on 2.0.1, and the `espeak-ng-data` directory the archive ships is the front end that version stopped using — which is why `docs/voice-assets.md` §4a and §5 no longer describe the bundle the way they did (§4a cited an upstream example whose shape 2.0.1 does not accept). `jean voice doctor` now lists `voice.models.tts_lexicon` as **absent** for a Kokoro configuration instead of reporting a bundle the engine cannot use as ready; before this batch it was in neither the required nor the optional list, so nothing was wrong according to the tool.

What *is* reachable without that asset is the acoustic model, through the documented alternative: `GenerationConfig::phoneme_codepoints` (`sherpa-onnx-2.0.1/src/tts.rs:435`, "used in place of the `text` argument"). `jean voice synthesize … --phonemes` renders a phrase already in the bundle's own 177-token alphabet — `hɛlˈoʊ wˈɜːld`, every character checked against `tokens.txt` first (ids 50, 86, 54, 156, 57, 135, 16, 65, 156, 87, 158, 54, 46). The engine, run against `target/p5p/bin/jean-voice-sherpa-dynamic.exe`:

```
sample_rate 24000 · frames 24697 · duration_ms 1029 · peak 0.3322 · rms 0.0559
clipped false · silent false · generated_ms 2063 · bytes_written 49438
```

and measured **from the file, by a different program** (`wave` + `struct`, not the worker): mono, 16-bit, **24,000 Hz, 24,644 frames = 1.027 s**, peak 0.3266, rms 0.0558, **2,682 zero crossings**, **57.4 %** of samples above −30 dBFS. That is structured, non-silent audio at the model's native rate, and the file is left for the operator to play.

Three limits on that sentence, stated because the number looks like a delivery:

* The text→phoneme half of text-to-speech is **still missing one file**. Any real sentence still refuses with the message above, so nothing here entitles JEAN to speak an answer yet. `voice.models.tts_lexicon` is the key; the remedy is an asset, not code.
* The render path does not resample or open an endpoint — 24 kHz is what Kokoro reports, and `voice.sample_rate = 16000` governs the recognizer. Playback-level behaviour is untested and belongs to §9.
* Kokoro output is **not bit-reproducible here**: the same 13 codepoints produced 24,644 frames and 24,697 frames on different runs of the same binary (2.2 ms apart, statistics agreeing to the third decimal). Any future assertion about synthesis must compare levels and duration within a tolerance, never a digest.

`--phonemes` deliberately narrows only the lexicon requirement (`assets::synthesis_for(config, needs_lexicon)`): model, tokens and voices are still checked, so a bundle that cannot load cannot be reported as synthesized. Pinned in `workers/voice/src/assets.rs` by assertions that the phoneme list drops `tts_lexicon`, keeps the other three, and produces a refusal that does not tell the operator to go find a lexicon.

### 61.4 `LNK4098`: the cause, the build-scoped fix, and what it costs

The warning was never about Sherpa's symbols. Rust links the **shared** CRT (`MSVCRT`/`VCRUNTIME140`) while the supplied `-static-MT-` archive was compiled against the **static** one (`LIBCMT`), and the linker reports exactly that collision. Measured both ways with `dumpbin /DEPENDENTS`, on executables built from the same source:

| Build | Linker messages | `VCRUNTIME140*.dll` / `api-ms-win-crt-*` | DLL imports | Bytes |
|---|---|---|---|---|
| `cargo build -p jean-voice --features sherpa` (default flags) | **1 × `LNK4098`** | **present** | 25 | 24,739,328 |
| same, `RUSTFLAGS=-C target-feature=+crt-static` | **none** | **absent** | **12** (kernel32, ntdll, ole32, combase, oleaut32, SETUPAPI, dxgi, ADVAPI32, dbghelp, api-ms synch/path) | 25,115,136 |

So the fix is one build-scoped variable, and its operator-visible value is bigger than a quiet log: the static worker imports **no Visual C++ runtime DLL at all** — no redistributable to install or to get out of date — for **+375,808 bytes** (1.5 %). Both variants were then *run*, not just linked: each transcribed `0.wav`/`1.wav`/`8k.wav` with the transcripts above, each synthesized the phoneme phrase, and each passed `workers/voice/tests/native_session.rs` **3/3**. The alternative reading of `LNK4098` — that mixing the two CRTs is unsafe in principle — is not dismissed by these runs; the C API boundary here passes pointers to POD config and buffers, allocation stays on one side, and the sessions that exercised it were short. Long-session behaviour is still an assumption, recorded as one.

**Disk, measured before choosing the layout, because there was no room to choose.** `C:` had **17.00 GB free of 474.72 GB (97 % used)** and `target/debug` alone is **46.05 GB across 16,878 files** (`target/p5p-measure-disk.ps1`). A second target directory to hold the second configuration was therefore ruled out on arithmetic, and the build-scoped flag in the *same* cache was the only layout that fits. What that costs in practice, recorded so the next batch does not rediscover it: switching `RUSTFLAGS` rebuilds the whole `jean-voice` graph — **110 crates, 3 m 21 s** at `-j 2` — and switching back rebuilds it again; the second static build after the seam landed in 18.37 s and 1 crate, because only the final link carries the CRT choice.

Preserved, so "run the workspace tests" cannot quietly swap the native worker out from under the operator (§60.9's ordering trap, which this batch hit and now closes):

```
target\p5p\bin\jean-voice-sherpa-dynamic.exe   24,739,328   shared CRT, LNK4098, sherpa linked
target\p5p\bin\jean-voice-sherpa-static.exe    25,115,136   static CRT, no linker warnings
target\p5p\bin\jean-voice-default.exe           1,707,520   default features: no sherpa, no assets
target\p5p\bin\jean-default-cli.exe            46,345,216   the CLI that drives them
```

Selected per run by `JEAN_VOICE_WORKER` (CLI) and `JEAN_NATIVE_WORKER_EXE` (test harness); both refuse a path that is not a file rather than falling back silently, and every measurement in this section names the executable it used. Copies are taken only when the build's own `[exit]` is 0 — a failed build in an earlier batch had overwritten a preserved worker with the stale default binary, and that guard is why it cannot happen twice.

### 61.5 Cancellation during real native inference, and an independent task that kept running

§60.7 recorded that native inference could not be reached without a device *by construction*: `VoiceConfig::audio_backend()` derives the endpoint from the speech backend, so `sherpa_onnx` always implies `System` audio, and the tier-1 cancellation tests necessarily drove a fixture. That coupling is now broken by configuration rather than by a new binary: **`voice.offline_wav`** (`crates/jean-protocol/src/config.rs`, documented in `config/defaults.toml`) makes `workers/voice/src/device.rs`'s `FileAudio` the backend — a WAV in place of the microphone, a counted sink in place of the speaker, and nothing opened. Empty, the shipped default, real endpoints are used; that is the only form an operator can actually speak to.

`FileCapture` delivers the file in 100 ms packets **at 1× real time**. That pacing is the whole point of the seam and it took a failure to establish it: the first three supervised runs each refused with *"the recognizer returned an empty or oversized transcript"*, and the cause was my harness replaying a 6.6-second clip as fast as it could read it. A release 40 ms into the file has genuinely heard 40 ms, and an engine returning nothing for it is **correct**. With 1× pacing the same tests pass, and the press holds are now expressed as `CLIP_0_HOLD = 7 s`, a clip-shaped constant rather than a guess.

Measured through the real zipformer bundle, `jean-voice-sherpa-dynamic.exe`, `target/p5p-native-session*.log`:

| Observation | Value |
|---|---|
| Press opened (`BeginCapture` → `Capturing`) | **61 ms** (30–76 ms across runs) |
| 28 `Ping` requests during a 7,020 ms press of live decode | answered, **slowest 0 ms** (2 ms worst seen) |
| Release → `Transcript` | **72 ms**, 104 bytes of text (58–116 ms across the runs below) |
| Expired press: release after the deadline | refused, `stale push-to-talk activation`, **no transcript delivered** |
| Worker after the interruption | still answerable, still recognising, `Bye` accepted |
| `ConfigureVoice` (model load) | 2.84 s alone; 5.06–5.13 s when three supervised sessions start together |
| Suite | **3 passed, 0 failed**, 19.41 s |

One protocol fact came out of writing the test wrong: an **expired `BeginCapture` is answered with `Capturing`, not `Refused`** — the deadline gates the *release*, so the refusal arrives 30–60 ms later at `EndCapture`. That is the existing design; the assertion, not the worker, was corrected.

**"An independent task keeps running"** was then tested against a real process rather than a mock (`target/p5p-indep/witness.sh`, isolated `JEAN_DATA_DIR` and its own `tools.toml`, so `%LOCALAPPDATA%\JEAN` was not read):

| Step | Result |
|---|---|
| `jean exec indep.sleeper` — PowerShell, 20 s sleep, `verify = output_contains "TASK_ALIVE_AFTER_VOICE_INTERRUPT"` | task `t-18e5ceced3978af0-7d3c-0` **running** at t+2 s |
| Native expired-press cancellation run *while it is in flight* | `test result: ok`, press opened 30 ms, release refused as stale |
| `jean tasks` immediately after the interruption | still **running** — not cancelled, not reaped |
| Task at completion | **20.236 s**, exit 0, state **succeeded**, verdict **proven** ("exited with 0 and its output contains …") |

The mechanism, since "it survived" is not an explanation: the interruption is a `CancelFlag` set inside the worker's own recognition thread plus a refusal on the stale activation id; the task's process is owned by a different CLI process's Job Object, and no voice path broadcasts a cancel to tasks. That is what the witness demonstrates, and it is a statement about *this* interruption — a Ctrl-C or a worker kill still takes the Job Object route §58 tested, not this one.

Fixture timings stay separate from native ones: the tier-1 suite's scripted endpoints remain the place where 10 ms-scale cancellation is asserted, and every number above came from `native_session.rs` driving a real ONNX decode.

### 61.6 Defects this batch found, and what each one was

1. **A failed synthesis load vetoed recognition.** `maybe_configured` gated `Configured` on both engines being loaded, so a missing Kokoro asset made a working zipformer report itself unusable — exactly the "placeholder reported as broken" inversion of the rule this project keeps. Recognition alone now gates it (`workers/voice/src/lib.rs`), `SynthesisEvent::LoadFailed` records the reason instead of failing configuration, and `Speak` refuses verbatim. `an_expired_press…` / `a_missing_synthesis_bundle_refuses_speech_but_not_listening` pin both halves: the same session that refuses speech transcribes the expected sentence.
2. **`doctor` could not see the Kokoro blocker.** `tts_lexicon` was listed as *optional* for Kokoro because an upstream example suggested one; measured, 2.0.1 requires it. It is now required-when-Kokoro and optional-when-VITS, and never both.
3. **The phoneme seam inherited a refusal it did not deserve.** With the gate in §61.3 left unconditional, `--phonemes` stopped at "voice.models.tts_lexicon is unset" before loading anything — a refusal to do work that does not need the missing thing. Split into `synthesis_for(config, needs_lexicon)`.
4. **Wording that read as a bug.** "…`voice.models.tts_lexicon` is unset **are unusable**" — the frame assumed a plural. Now "speech synthesis is unavailable: *list*."
5. **`cargo fmt --all` reformatted `assets.rs` mid-edit** and my follow-up assertion used `str::as_str`, which is unstable on this toolchain: `[exit] 101` on the first test run, fixed to the same `.map(|c| c.key)` coercion the neighbouring test already uses.
6. **There is no operator-facing way to interrupt an utterance.** Writing the exact physical commands for this batch's item 5 is what found it: `OwnerCommand::Interrupt` (`crates/jean-core/src/interaction.rs:334`) is implemented, refuses a stale utterance id, and is sent by the tier-1 and tier-3a tests — but nothing in `jean interact`, in `jean voice`, or on a key binding reaches it. `docs/voice-assets.md` §9 step 4 had been telling the operator to press "the interrupt key", which does not exist. The instruction is corrected to what is true, the gap is listed as open, and no verb was invented here to close it silently — that is a product decision with a keyboard-shortcut argument attached to it. **Closed in §62.2**, which is where the decision was taken (an operator command in the session that owns playback, not a global hotkey), and §62.3 is the measurement this section could not make.

### 61.7 Validation

Every stage run through the Visual Studio developer environment with
`CARGO_INCREMENTAL=0`, `--offline --locked -j 2`, and read out of its own log's
`[exit]` line rather than the wrapper's:

| Stage | Command | Result | Log |
|---|---|---|---|
| Workspace build | `build --workspace --all-targets` | `[exit] 0` | `target/p5p-check-default-2.log` |
| Workspace tests | `test --workspace` | **1047 passed, 0 failed, 1 ignored**, 62 result blocks (§60.9: 1045 across 61) | `target/p5p-test-workspace.log` |
| Workspace lint | `clippy --workspace --all-targets -- -D warnings` | `[exit] 0` | `target/p5p-clippy-workspace.log` |
| Formatting | `fmt --all --check` | `[exit] 0` | `target/p5p-fmt-check.log` |
| Voice, feature on | `test -p jean-voice --features sherpa --lib --tests` | 26 + 3 + 12 passed, 0 failed | `target/p5p-test-voice-sherpa-3.log` |
| Voice, feature on | `clippy -p jean-voice --features sherpa --all-targets -- -D warnings` | `[exit] 0` | `target/p5p-clippy-sherpa-3.log` |
| Native, dynamic CRT | `native_session` (3 tests) | **3 passed**, 19.41 s | this section's table in §61.5 |
| Native, static CRT | `native_session` (3 tests) | **3 passed**, 20.42 s | `target/p5p-native-session-static.log` |
| Assets | `target/p5p-verify-kokoro-2.py` | 360 / 360, 0 missing, 0 extra, 0 size mismatch | §61.1 |
| Independence | `target/p5p-indep/witness.sh` | task `running` → `succeeded`, verdict `proven`, 20.236 s | `target/p5p-indep-witness.log` |

`browser_live` passed 4/4 in this batch's run (34.34 s), so the teardown race did
not fire here — which is not a fix and it stays open exactly where §60.9 left it,
reserved for its own batch.

The one failure during validation was mine and was caught by the build: an
assertion used `str::as_str`, unstable on this toolchain, so the first
`--lib --tests` run stopped at `[exit] 101` and passed after switching to the same
`.map(|c| c.key)` coercion the neighbouring test already relies on.

**Disk, at the end.** `C:` free: **18,253,611,008 bytes (17.00 GB) at the batch
start → 16,311,349,248 (15.19 GB) at the end**, ≈1.81 GiB consumed by 369,315,617
bytes of extracted Kokoro files, two complete native-worker link configurations in
`target/debug`, and this batch's logs and WAVs. `target/debug` was already 46.05 GB
/ 16,878 files before any of it. **Nothing was deleted, no cache was cleared, and
no existing file was overwritten except the preserved copies under
`target/p5p/bin`, each refreshed only after its own build reported `[exit] 0`.**
Evidence: `target/p5p-*.log`, `p5p-deps-dynamic.txt`, `p5p-deps-static.txt`,
`p5p-tts-phonemes.wav`, `p5p-tts-phonemes-static.wav`,
`p5p-kokoro-entries.txt`, `p5p-verify-kokoro-2.py`, `p5p-measure-disk.ps1`,
`p5p-indep/{tools.toml,witness.sh}`, `p5p-offline/jean.toml`, `p5o-data/jean.toml`.

Within Phase 5, using only the supplied download: nothing fetched, nothing
installed, no machine-wide change, no paid service, no subagent, no commit, push or
publication, **no microphone opened and no audio played** — every capture and every
render in this batch went through a file or a counted sink. The writes outside the
repository were the operator's instruction for this batch (`%LOCALAPPDATA%\JEAN`),
the originals in `Downloads` remain in place, and the real settings file,
preferences and data directory were not read or written: isolation held via
`JEAN_DATA_DIR` pointing at `target/p5o-data`, `target/p5p-offline` and
`target/p5p-indep`. Physical audio, the lexicon, acoustic barge-in and the spoken
task demonstration remain open and are reported as open (§61.8).

### 61.8 Open, stated as open

* **The one missing asset.** A Kokoro lexicon for `voice.models.tts_lexicon`. Until it exists, sentences do not speak and this batch's synthesis claim stays bounded to §61.3's limits. It is an operator download decision, not a code change.
* **Physical plane (§9, unchanged in status).** Microphone, speaker, push-to-talk, playback, and a spoken task end-to-end. Exact commands are in the handoff below and in `docs/voice-assets.md` §9; the file-based pre-flight in §7 is now `jean voice transcribe` plus `… synthesize --phonemes`.
* **Acoustic barge-in.** Not implemented. `Interrupt` is a control message that stops playback; nothing listens while the speaker is busy and nothing measures whether the owner spoke over it.
* **The `browser_live` teardown race** stays open for its own batch, untouched here (§60.9); it did not fire in this batch's workspace run.
* **Native *synthesis* cancellation.** Still tier 1 only. `Speak` takes text, text needs the lexicon, so the generation-callback path has never been exercised against a running Kokoro — reachable the moment the asset arrives, or through a phoneme-shaped `Speak` if that is ever made a product path rather than a test seam.
* **Long-session behaviour of either CRT variant**, and the shared/static heap boundary at the C API, rest on short supervised sessions plus the argument in §61.4.
* **No operator-facing utterance interrupt (§61.6.6).** *(Closed in §62.2; the paragraph below is kept as the record of what could be claimed while it stood.)* The command exists in the owner API and in the worker and is proven in tests; nothing a person can press sends it. Until that is wired, §9 step 4 measures "the utterance finished and the slot freed", not interruption, and any claim about stopping JEAN mid-sentence should say which of the two was observed.

**Milestone 5.5 (PLANNING/08-OWNER-WORKFLOW.md) is preserved as the next milestone.** What this batch changes for it, in the terms that milestone uses:

* *Autonomous agent handoffs* are unaffected by voice, and voice does not gate them — but a spoken handoff now has a working transcription path with a measured 58–116 ms release-to-transcript and a bounded tail flush, so the "give JEAN the goal out loud" demo is limited by the microphone step, not by the recognizer.
* *Model fallback* has a precedent worth reusing: the two CRT workers coexist and are selected by an explicit variable that fails loudly on a bad path, rather than one build silently replacing another. The same "preserve both, name which ran" discipline is what a fallback between a primary and a secondary model needs.
* *Learned owner preferences* cannot yet be trained on voice at all, and nothing here should be read as if it could: `voice.announce_task_state` and `voice.spoken_controls` stay off by default precisely because an announcement takes the single utterance slot and a spoken control intercepts a goal. The first preference worth learning is the one §61.3 exposes — the operator's own lexicon and voice choices — and that is an asset decision before it is a learning one.

## 62. Phase-5: the text front end is identified, and interruption became something an operator can press — 2026-11-07

Three items: make Kokoro speak ordinary text, put an operator-facing playback interrupt
into the session that owns playback, and write one copy-paste physical procedure. This
batch ran under a stated validation budget — affected binaries only, focused checks only,
one demonstration per available path — so the full workspace build, test and Clippy runs
are deferred to Phase-5 closeout rather than claimed here.

### 62.1 Ordinary text: the front end exists, is version-matched, and is not installed

> **Superseded below, in two places.** This batch's compatibility figures were measured on a
> bounded 64 KiB read and §63.1 shows a prefix **understates** them (46 glyphs / 1 outsider
> became 53 / 2, across 15 of 168,654 entries). And the section's whole premise — that the
> asset was still missing — ended with §63: ordinary text synthesizes now. The identification
> work, the licence lineage and the version-matching argument all still stand as written.

**No sentence has been spoken by this build yet, and that is an asset fact, not a code
one.** §61.3's limit still holds: sherpa-onnx 2.0.1 retired the espeak-ng path and demands
a lexicon, and neither the supplied Kokoro archive nor this machine contains one. What this
batch added is the identification of the exact file that closes it, recorded in
`docs/voice-assets.md` §4a-ii:

| | |
|---|---|
| asset | `lexicon-en-us.txt`, published by the **same v2.0.1 release** as the runtime |
| link | <https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/lexicon-en-us.txt> |
| size / digest | 6,305,773 bytes / `sha256:8b67ae549a465eee78f92ecb74644ad90c3c58ce65f19c68baf85878a10ef73a`, both read from the GitHub release API and confirmed by a 64 KiB `Range` fetch that returned `206` with `Content-Range: bytes 0-65535/6305773` |
| destination | `%LOCALAPPDATA%\JEAN\models\tts\lexicon-en-us.txt` under key `voice.models.tts_lexicon` |
| why this one | upstream's own `kokoro_tts_en_lexicon.rs` and `kokoro-en-lexicon.sh`, at tag `v2.0.1`, pair `./kokoro-en-v0_19/{model.onnx,voices.bin,tokens.txt}` with `lexicon: Some("./lexicon-en-us.txt")` and no `dict_dir`/`lang` — the same bundle that is installed here |
| licence | derived from **CMUdict (BSD-2-Clause, attribution required)** plus the bundle's own exception list; **no licence file ships beside the asset**, so the manifest also names a licence-anchored alternative (`lexicon-us-en.txt` inside `kokoro-multi-lang-v1_0.tar.bz2`, which carries an in-bundle LICENSE) |
| compatibility, measured | on the 1,983 complete entries readable in that 64 KiB: 46 distinct phoneme code points, **45 inside the bundle's own 177-glyph `tokens.txt`**, one outside — U+0303 in `accouchement` — 1 entry in 1,983 |

The comparison method was itself corrected once. Judging each space-delimited phone token
(`bˈ`, `tˌ`, `iːˈ`) against the glyph table reported 55 false mismatches; Kokoro consumes
phonemes **per character** — upstream's own generator writes
`tokens = " ".join(list(phones))` — and per-character comparison is what produced the 45/46
above. `target/p5q-check-lexicon.py` re-runs digest **and** alphabet coverage against the
whole file in one step and prints `PARTIAL` loudly rather than silently scoring a prefix,
which is the check to run after the download and before any claim about text synthesis.

### 62.2 The interrupt an operator can press: `jean interact --tty`

§61.6.6 recorded the gap: `OwnerCommand::Interrupt` existed in the owner, in the worker, and
in tests, and nothing a person could press reached it. It is closed.

`jean interact` is the session that owns playback (`apps/jean-cli/src/main.rs:119-127`,
`apps/jean-cli/src/phase5.rs:37`), and it now has an operator surface next to the
machine-facing one, selected with `--tty`. What it changes and what it does not:

| Command | Sends | Notes |
|---|---|---|
| `/say <text>` | `Speak` | the existing 1..=4096-byte and busy checks, unchanged |
| `/stop` | `Interrupt { utterance }` | the id is the one `snapshot.utterance` says this session owns; with nothing playing it answers `refused: nothing is playing to interrupt` and **invents no id** |
| `/stop <id>` | `Interrupt { id }` | goes to the existing identity check verbatim, which is how a stale interrupt is refused |
| `/status`, `/do`, `/cancel`, `/voice on\|off`, `/quit`, `/help` | `Status`, `Submit`, `CancelTask`, `StartVoice`/`StopVoice`, `Shutdown` | one command each; no new protocol message anywhere |
| `anything else` | — | refused. A line without a `/` is never executed, and neither is an unknown `/word` |

The JSON-lines contract is untouched: `ClientOutput` is constructed only in JSON mode, the
framed initial snapshot and per-request replies are the same bytes as before, and the four
pre-existing `cli_phase5` tests still pass. The tty reader thread reads **bounded** lines
(4096 bytes kept, the remainder drained, `apps/jean-cli/src/phase5.rs:18,299`) so a pasted
wall of text cannot grow an unbounded buffer, and it feeds the same `sync_channel` the JSON
reader uses, so the control loop keeps ticking — and keeps printing state transitions — while
a line is unread. Transition printing is the reason an interrupt is *visible*: the session
reprints `state:` when `voice`, `utterance`, `activation`, the task, or `rejected_events`
moves, so `/stop` shows up as `utterance=2` → `utterance=-` without typing `/status`.

### 62.3 Measured, device-free, through the real entry point

The honest obstacle was that there was nothing to interrupt. `FixtureSynthesizer` returned
160 frames — about 10 ms at 16 kHz — so an utterance opened and closed inside one pipe
round trip, and the only interruption a session could observe was one that had already
finished. `voice.fixture_utterance_ms` (`crates/jean-protocol/src/config.rs:137`, default
**10** so every existing assertion still measures the same thing, bounded 10..=30000 ms) sets
that length for the fixture engine only, and it becomes measurable because
`voice.offline_wav` makes the render side a **sink that paces at 1× real time**
(`workers/voice/src/device.rs:376`). Raising the key without the offline seam still plays
into an unpaced fixture endpoint and buys nothing; the pairing is documented at both keys.

`target/p5r-tty-demo.py`, output kept in `target/p5r-tty-demo.log`, ran
`target\debug\jean.exe interact --tty` with `target\debug\jean-voice.exe` (default sibling,
`JEAN_VOICE_WORKER` unset), `backend = 'fixture'`, `offline_wav` set and
`fixture_utterance_ms = 2000`. Timestamps are when each line **arrived**:

| Observation | Measured |
|---|---|
| an utterance nobody interrupts closes | **2,091 ms** after `/say`, against a configured 2,000 ms |
| the same length, `/stop` sent 200 ms in | **31 ms** to the `utterance=-` transition, **233 ms** total |
| `/stop 2` while utterance 3 is playing | `refused: stale utterance`, and `state:` still shows `utterance=3` |
| that newer utterance's own close | **2,105 ms** after its `/say` — the stale interrupt did not stop it |
| a bare `status` line | `refused: only commands are acted on; /help lists them and /do starts a task` |
| `/frobnicate` | `refused: unknown command /frobnicate; /help lists them` |

The sink's pacing, separately, at the layer that owns it:
`workers/voice/src/device.rs:530` hands the offline sink one second of frames, drains it
uninterrupted (**≥900 ms**, every frame counted), then cancels 150 ms into a second drain and
requires the drain to return in under 500 ms — playback stopped part-way rather than being
handed over and finished. `workers/voice/src/speech.rs:219` pins that the fixture length is
the length it claims.

Interruption and task independence, through the same entry point:
`apps/jean-cli/tests/cli_phase5.rs:518` starts the session, launches the registered
`indep.sleeper` tool as a **second process**, waits for that task to reach `running`, then
runs four `/say` + `/stop` rounds — each asserted closed in under 600 ms — and finally asserts
the session's own task slot stayed `task=-`, that the recorded task was **still `running`**
after the last interrupt, and that it exited 0 with its verification needle
`TASK_ALIVE_AFTER_VOICE_INTERRUPT` and finished `succeeded`. `apps/jean-cli/tests/cli_phase5.rs:458`
covers control-versus-interrupted timing, the stale refusal and the refused non-commands in
one pass. Both are green (§62.6).

**What this is not.** It is not audible speech: the fixture engine renders silence into a
counter, so no operator heard anything and no endpoint opened. It is not real-text playback
interruption, which stays gated on §62.1's asset. And it is not acoustic barge-in, which does
not exist: nothing listens while the speaker is busy (§8 of the manifest). Keyboard
interruption and barge-in remain two separate outcomes and are reported as two.

### 62.4 The session can now be pointed at the worker the operator provisioned

`Settings::from_context` hard-wired `jean-voice.exe` next to `jean.exe`, so the only session
that can speak could never use the preserved native workers §61.4 deliberately kept — any
`cargo build --workspace` replaced the sibling with a feature-less one, which is the
silent-replacement failure those copies exist to prevent. `crates/jean-core/src/interaction.rs:73`
now honours `JEAN_VOICE_WORKER` for the voice worker, the same variable `jean voice …` verbs
already use, and refuses a value that is not a file with its name in the message rather than
letting it surface as an `unavailable:` voice state. `apps/jean-cli/tests/cli_phase5.rs:634`
proves both halves: a named worker answers (`voice=ready`), and the same variable pointed at a
path that is not a file stops the command nonzero, naming the variable. The task worker is
unchanged.

### 62.5 Defects this batch found, and what each one was

1. **Two JEAN processes on a brand-new data directory race through the schema.** Starting `jean interact` and `jean exec` against the same empty `JEAN_DATA_DIR` at the same instant made one of them exit with `jean: task store error: sqlite error at migration: table tool_health already exists`. `migrate` (`crates/jean-store/src/lib.rs:538`) reads `PRAGMA user_version`, then applies each step in its own transaction; two processes that both read version 0 both try to create the same tables, and there is no lock between them. The first `cli_phase5` run failed on exactly this (`target/p5r-test-cli.log`, `[exit] 101`) and the test now sequences the two processes, which is why it passes. **The race itself is open** — making the migration idempotent, or serialising creation behind a lock, is a persistence change that deserves its own batch and its own regression tests, not a tolerance clause added between two other tasks.
2. **A doc comment claimed a measurement that the counter does not take.** `workers/voice/src/device.rs:306` said `rendered_frames` "says how far" an interrupted utterance got. Writes are counted as they are handed over and the pacing happens afterwards in `drain`, so for any utterance longer than a chunk the counter reads *complete* even when playback stopped at a tenth of it. The comment now says what is true, and the cut-short evidence in §62.3 is the drain's elapsed time, not the frame count.

### 62.6 Validation

Built and run through the Visual Studio developer environment with `CARGO_INCREMENTAL=0`,
`--offline --locked -j 2`, each result read from its own log's `[exit]`:

| Stage | Scope | Result | Log |
|---|---|---|---|
| Formatting | `fmt --all` | `[exit] 0`, re-checked after the last edit | `target/p5r-fmt.log`, `p5r-fmt-2.log`, `p5r-fmt-3.log` |
| Build | `-p jean-cli -p jean-voice -p jean-protocol -p jean-core --tests` | `[exit] 0`, warning-free after 4 dead-code/unused warnings were removed | `target/p5r-build.log`, `p5r-build-2.log` |
| CLI session | `test -p jean-cli --test cli_phase5` | **7 passed, 0 failed** (4 pre-existing JSON-frame tests + 3 new) | `target/p5r-test-cli-2.log` |
| Owner core | `test -p jean-core --test interaction` | **13 passed, 0 failed** | `target/p5r-test-interaction.log` |
| Voice worker | `test -p jean-voice --lib` | **28 passed, 0 failed**, including the two new pacing/length tests | `target/p5r-test-voice.log` |
| Config agreement | `test -p jean-core --test config_files` | **12 passed, 0 failed** — the new key is in `config/defaults.toml` with the same default, documented, and no invented/undocumented key | `target/p5r-test-config.log` |
| Lint | `clippy -p jean-cli -p jean-core -p jean-voice -p jean-protocol --all-targets -- -D warnings` | `[exit] 0` | `target/p5r-clippy.log` |
| Demonstration | `target/p5r-tty-demo.py` | exit 0, all four claims in the §62.3 table observed | `target/p5r-tty-demo.log` |

The first `cli_phase5` run failed 1 of 6 for the defect in §62.5.1; nothing else failed, and
no check was re-run for a cause that had not been identified. **The workspace-wide build, the
full test suite and workspace Clippy were deliberately not run** — deferred to Phase-5
closeout by this batch's budget — so §61.7's 1047-passed number remains the last full-tree
measurement and is now known to cover only the sources as of §61.

**Disk.** `C:` free at the end of this batch: **14,278,938,624 bytes (13.30 GB)**, down from
§61.7's 15.19 GB; the difference is rebuilt artifacts in `target/debug` plus this batch's logs
and per-test directories (`target/p5r-tty-*`, ≈262 KB each). Nothing was deleted, no cache was
cleared, no existing file was overwritten; the writes outside the repository are this batch's
own isolated directories (`target/p5r-demo`) and nothing touched `%LOCALAPPDATA%\JEAN`
settings, preferences or data. The operator's normal `jean.toml` was neither read nor written:
every run above used an isolated directory via `JEAN_DATA_DIR`. No microphone was opened and no
audio was played — capture came from a file and render went to a counted sink. No download was
made, no paid service used, no subagent delegated, no dependency added, no commit, push or
publication, and Milestone 5.5 was not started.

### 62.7 Open, stated as open

* **The lexicon, and the one claim it gates.** Ordinary text synthesis, and with it any assertion that a *real* utterance was interrupted audibly. The operator step is `docs/voice-assets.md` §4a-ii; after it, `python target\p5q-check-lexicon.py <path>` must print `matches published`, `lexicon entries` ≈ the full count and `glyphs outside model 0` (or name each outsider), then `voice.models.tts_lexicon` is set and `jean voice doctor` should stop listing it absent.
* **Native *synthesis* cancellation is now reachable for the first time.** §61.8 left it tier-1 because no session could speak a real sentence and no operator could interrupt one. Both halves exist now: with the asset installed, `JEAN_VOICE_WORKER=<preserved sherpa worker> jean interact --tty`, `/say <sentence>`, `/stop` is the measurement that has never been taken — generation-callback abort against a running Kokoro, watched from the outside.
* **The two-process migration race (§62.5.1).** Reproduced, described, sequenced around in the test, unfixed in the store.
* **Physical plane.** Microphone, speaker, push-to-talk and a spoken task, with the copy-paste procedure rewritten in `docs/voice-assets.md` §9 to start from a harmless status request and to use `--tty` for interruption.
* **Acoustic barge-in.** Not implemented, unchanged, and deliberately not conflated with the keyboard interrupt measured above.
* **`browser_live` teardown race and temporary profiles** stay with their own batch, untouched here.
* **Full workspace validation** (§61.7's scope) now belongs to Phase-5 closeout.

**Milestone 5.5 stays the next implementation milestone.** Two findings matter for it in its own
terms: the operator surface and the worker choice are now separate decisions an owner can make
per run — which is the shape a *preference* needs before it can be learned — and §62.4 means a
provisioned speech engine survives the build that would otherwise replace it, so a milestone
that hands work to a model can still assume the voice it configured is the voice that runs.
Nothing in this batch hands a decision to a model, and no spoken phrase gained any new
authority: the interrupt only stops playback, never a task.

## 63. Phase-5: ordinary text speaks — the lexicon arrived, was verified, and the preserved worker did not survive it — 2026-11-07

§62.1 ended with one asset between this build and a spoken sentence. The operator supplied
`lexicon-en-us.txt`, and the sentence now exists. This batch stayed on that path: verify,
provision, doctor, synthesize, measure. The interrupt evidence from §62.3 was **reused, not
re-run** — nothing in `apps/jean-cli/src/phase5.rs`, `crates/jean-core/src/interaction.rs` or
the owner protocol has changed since it was taken.

### 63.1 The asset as received, and what the full file actually contains

| Check | Result |
|---|---|
| bytes | **6,305,773** — matches the published `size` exactly |
| SHA-256 | `8b67ae549a465eee78f92ecb74644ad90c3c58ce65f19c68baf85878a10ef73a` — **matches the published digest**, so provisioning continued |
| entries | 168,654, and every one parses as `word \|\| phones` |
| glyphs used | 53 distinct code points |
| glyphs outside the model alphabet | **2**: `U+0303 COMBINING TILDE` (13 entries — `accouchement`, `blanc`, `croissant`, `croissants`, …) and `U+0327 COMBINING CEDILLA` (2 — `dolgellau`, `utrecht`) |
| affected share | **15 of 168,654 entries (0.0089 %)** |

§62.1's 46-glyph / 1-outsider figures were measured on a bounded 64 KiB read and are
**superseded**, not refined: a prefix of this file understates both the glyph count and the
number of outsiders, which is exactly why the checker prints `PARTIAL` when it is handed a
truncated file. The verdict line is deliberately not `compatible`: fifteen words in this
table cannot be rendered by this model, all of them French or Welsh proper nouns, and the
engine would answer `Failed to convert … to token IDs` for them. That is a named, bounded
limit of the asset, not a reason to reject it.

### 63.2 Where it went

Copied (not moved) to `C:\Users\4Raisan\AppData\Local\JEAN\models\tts\lexicon-en-us.txt`,
re-digested at the destination, and the download in `C:\Users\4Raisan\Downloads` is untouched.
`voice.models.tts_lexicon` was set **only** in the isolated
`target/p5o-data/jean.toml`; `%LOCALAPPDATA%\JEAN\jean.toml` was neither read nor written, and
nothing was deleted or overwritten in the models tree (`kokoro-en-v0_19` was already there and
is unchanged).

### 63.3 The preserved Sherpa worker is contract-stale, and that is the finding

The §61.4 rule was: keep a runnable Sherpa worker beside the default one so no build can
silently replace it. What that rule did not anticipate is that the worker and the CLI are
coupled by a **schema**, not only by a feature: `VoiceConfig` is
`#[serde(default, deny_unknown_fields)]`, and the CLI hands the worker its config as JSON. The
`fixture_utterance_ms` key added in §62.3 therefore made every binary built before it reject
the current CLI — measured, not theorised:

```
expected a VoiceConfig JSON object: unknown field `fixture_utterance_ms`, expected one of
`enabled`, … `fixture_transcript` at line 1 column 1595          # exit 1
```

…from `target\p5p\bin\jean-voice-sherpa-dynamic.exe` (21:56, §61.4) driven by the current
`target\debug\jean.exe`. The refusal is loud, correctly attributed and exits nonzero, so the
rule held in the sense that matters — nothing silently degraded — but a preserved worker is
only usable as a **pair** with the CLI that emits its schema. Response: rebuild the affected
binary rather than downgrade the schema.

`-p jean-voice --features sherpa --offline --locked -j 2` with `CARGO_INCREMENTAL=0` and
`SHERPA_ONNX_LIB_DIR` at the extracted archive: `[exit] 0` in
`target/p5q-build-sherpa.log`, with the single `LNK4098` that §61.4 attributes to the
dynamic-CRT pairing (this is the dynamic variant; the `+crt-static` build is the one that
silences it, and it is now equally stale until rebuilt the same way). Fresh executable
**24,741,376 bytes**, preserved to `target/p5q/bin/jean-voice-sherpa-dynamic.exe` only because
its own build reported `[exit] 0`. **The §61.4 copies were left in place, unmodified** — they
are evidence, and a later batch that wants them works against the pre-§62 schema.

### 63.4 Voice doctor against the isolated configuration

`target/debug/jean.exe voice doctor` with `JEAN_DATA_DIR=target\p5o-data` and
`JEAN_VOICE_WORKER=target\p5q\bin\jean-voice-sherpa-dynamic.exe`
(`target/p5q-doctor-fresh-worker.log`, exit 0):

`sherpa_feature_linked: true` · `backend: SherpaOnnx` · `engine.available: true`, label
`sherpa-onnx 2.0.1 (streaming transducer ASR; synthesis engine per config)` ·
`required_assets` missing: none · `synthesis_assets` present: `tts_model`, `tts_tokens`,
`tts_voices`, **`tts_lexicon`**, `tts_dict_dir` · `recognition_assets` present: all four ·
`offline_wav: ""` and `audio_backend: System`, which is the line the operator must read before
Step 4: **with this configuration `/say` opens the real speaker and makes sound.** Doctor
itself opens no endpoint.

### 63.5 Ordinary text, synthesized and measured twice

```
jean voice synthesize "…\target\p5q-text.wav" "The quick brown fox jumps over the lazy dog."
```

No `--phonemes`: this went through the text front end. `"input": "text"` in the report is the
field that distinguishes the two paths, and §61.3's phoneme seam is no longer the only route to
the acoustic model.

| From the worker | Value | Independently re-read from the file |
|---|---|---|
| sample rate | 24,000 Hz | 24,000 Hz, 1 channel ✓ |
| frames | 68,283 | 68,283 ✓ |
| duration | 2,845 ms | 2,845 ms ✓ |
| peak | 0.39542 | 0.3954 (−8.1 dBFS) ✓ |
| rms | 0.05698 | 0.0570 ✓ |
| silent / clipped | false / false | 10,457 zero crossings; 35.8 % of samples above −30 dBFS |
| generated_ms | 4,912 | — timed after model construction, so this is **generation only**: ≈1.7× real time on this CPU, and it is the number that sets how long an operator waits between `/say` and the first sound |

`target/p5q-measure-wav.py` produced the right-hand column by reading the WAV itself, so the
non-silence claim does not rest on the engine's own report. One warning line came back on
stderr and is expected rather than suppressed: `--kokoro-lang is deprecated and ignored in
sherpa-onnx >= v2.0.0. espeak-ng is no longer used. Language is determined by the lexicon files
you provide.` — this config still sets `tts_lang = 'en-us'` (harmlessly ignored) and the lexicon
is now the thing that decides language, which is precisely the §4a diagnosis confirmed by the
engine itself.

**What this proves and what it does not.** Proved: an ordinary English sentence becomes
non-silent 24 kHz audio through the installed Kokoro bundle and the version-matched lexicon,
with no phoneme seam. Not proved: that it *sounds* right — intelligibility is a listening
judgement and the speaker stays the operator's (§9 Step 4). Also unchanged: acoustic barge-in
is still not implemented, and keyboard interruption is still the only interrupt path.

### 63.6 Which process owns the task in a cancellation test — measured both ways

`/do report the running processes` in a tty session printed
`ok: task t-18e5d63ac6e89ed4-3fe8-2 accepted exactly once; execution is not an utterance`, and
the same session's `/status` then walked that id through `queued → running → failed` — **the
session owns a task it submitted**, and the failure is the `provider = none` planning gate, not
a voice fault. So the session's own `/status` is a valid ownership check for `/do` tasks.

A task launched by a **second process** is a different matter and must not be expected there:
`apps/jean-cli/tests/cli_phase5.rs:518` runs `jean exec indep.sleeper` as its own process while
the session speaks and interrupts, and asserts the session's state line stayed `task=-` the
whole time while the recorded task went `running → succeeded`. `jean tasks` — or the store — is
therefore the **external** status check for a second-process task, and `jean cancel <id>` the
way to stop one; `/cancel` in the session answers `stale or settled task cancellation` for a
task it never owned, which is the correct refusal.

### 63.7 Validation

| Stage | Scope | Result | Log |
|---|---|---|---|
| Asset | `target/p5q-check-lexicon.py` on the full file | digest `matches published`; 168,654 entries; 2 glyphs outside | §63.1 |
| Worker build | `build -p jean-voice --features sherpa --offline --locked -j 2` | `[exit] 0`, one expected `LNK4098` | `target/p5q-build-sherpa.log` |
| Config | `voice doctor` through the real CLI + preserved worker | exit 0, `tts_lexicon present: true`, engine available | `target/p5q-doctor-fresh-worker.log` |
| Stale pair | `voice doctor` with the §61.4 worker | exit 1 naming `fixture_utterance_ms` | `target/p5q-doctor-stale-worker.log` |
| Synthesis | one ordinary sentence, no `--phonemes` | exit 0; 2,845 ms at 24 kHz; `silent: false` | `target/p5q-synthesize-text.log`, `target/p5q-text.wav` |
| Re-measure | `target/p5q-measure-wav.py` | every figure agrees with the worker's own report | §63.5 |
| Ownership | `/do` + `/status` in a live tty session | session-owned id walks `queued → running → failed` | §63.6 |

No Rust source changed in this batch, so **no test was re-run**: the §62.6 results (7 CLI
session tests, 13 owner tests, 28 worker unit tests, 12 config-agreement tests, targeted
Clippy `[exit] 0`) still describe the current tree, and the interrupt numbers in §62.3 are
reused as-is. Full workspace build, test and Clippy remain deferred to Phase-5 closeout, along
with `jean-voice-sherpa-static.exe` (still on the pre-§62 schema) and the two preserved workers
in `target/p5p/bin`, which are now knowingly stale-by-contract.

**Disk.** `C:` free: **14,278,938,624 bytes (13.30 GB) before → 14,192,238,592 (13.22 GB)
after — 86,700,032 bytes ≈ 82.7 MB consumed**, which is the 6,305,773-byte asset at two
locations, the rebuilt 24,741,376-byte worker, and this batch's logs and WAV. Nothing deleted,
no cache cleared, no existing file overwritten: the asset destination did not exist beforehand,
the §61.4 binaries were left where they are, and the only files created are `target/p5q*` and
`target/p5q/bin`. No microphone was opened and **no audio was played** — the sentence went to a
file and was read back as numbers. No download, no paid service, no subagent, no dependency, no
commit or publication.

### 63.8 Open, stated as open

* **Native text interruption has never been measured, and now can be.** §61.8 and §62.7 both left this open for the same reason (no real utterance to interrupt). With the lexicon installed there are two ways to close it: the operator's `/say` + `/stop` on a real speaker, or the device-free route — add `tts_lexicon` to `target/p5p-offline/jean.toml`, name the `target/p5q/bin` worker, and interrupt a 2.8-second sentence into the counted sink. The second is a measurement; the first is the physical proof.
* **Intelligibility.** This batch measured amplitude, not whether the words are the words. Listening is the operator's.
* **The two preserved-worker pairs in `target/p5p/bin`** are stale against the current config schema (§63.3) and are now a documented pair-with-old-CLI artifact rather than a runnable path forward.
* **The two-process store-startup migration race** (`table tool_health already exists`, §62.5.1) stays open, and the `browser_live` teardown race with its temporary profiles stays in its own batch, untouched here.
* **Workspace-wide validation** still belongs to Phase-5 closeout.

**Milestone 5.5 is next and was not started.** The relevant change for it is narrow but real: a
spoken sentence now round-trips in both directions with measured costs — 58–116 ms
release-to-transcript on the input side (§61.5) and ≈1.7× real-time generation on the output
side (§63.5) — so the "hand JEAN a goal out loud" demo is limited by CPU and by a model
provider, not by the speech stack, and the generation figure is the one that decides whether a
spoken acknowledgement is worth putting in front of a handoff.

## 65. Phase-5: a field the operator can dictate into, and an offline engine that turns out to be supported but not reachable — 2026-11-08

Two questions, one of which inverted the answer. The tester's read-only transcript box became a
real edit control so an external dictation engine has something to type into. And the pinned
Sherpa ONNX turned out to support Parakeet TDT after all — the blocker is not the library, the
crate, or the asset's availability, but the fact that this worker only ever builds a *streaming*
recognizer.

### 65.1 The dictation field

`tools/voice-tester/voice-tester.ps1` now carries a `Dictation` caption over a standard multiline
`System.Windows.Forms.TextBox` (`WordWrap`, vertical scroll, `HideSelection = $false`,
`AcceptsTab`), with `Copy all` and `Clear` beside it, `Start/Stop listening` and
`Play/Stop playback` underneath, and the field as the window's initial `ActiveControl`.

It is a plain edit control on purpose. A hotkey dictation tool — Wispr Flow or anything else that
sends text to the focused control — needs a caret, a selection, and the system's own clipboard
accelerators; a read-only box silently discards the result and a custom-drawn one is invisible to
half the accessibility and injection paths. Nothing about the tester changed on the protocol side:
the seven commands it can send are still the seven in §64.1, and **no submission path was added**.
The header comment now says why the field stops at review: handing corrected text to JEAN as a
goal is a separate, explicit action for the task UI, and putting a submit affordance here would
reintroduce exactly the coupling the window exists to avoid.

The one behavior that had to change was the sync. §64's timer wrote the transcript into the box
every tick whenever the two differed, which is harmless while the box is read-only and fatal the
moment it is editable — the operator's correction would be walked over 100 ms later. Delivery is a
one-shot now: the state machine parks the words in `$script:InsertWords`, `Insert-Words` places
them **at the caret** (replacing a selection, spacing itself off the text before, leaving the caret
after the inserted phrase so the next phrase continues the paragraph), and the queue is cleared.
The self test drives the control directly and asserts four properties — appends at the caret,
inserts mid-edit without losing either half, parks the caret after the words, and an empty delivery
changes nothing — so "editable" is a checked claim rather than a description.

### 65.2 Verifying it from outside the window, which is the only side that matters

Requirement: the field must accept text from *another process*. Three harnesses were needed to get
a trustworthy answer, and the first two were wrong in instructive ways.

1. **The managed UI Automation client in Windows PowerShell sees nothing.** `UIAutomationClient`
   enumerated the tester's children as ten `ControlType.Pane` elements, every one
   `IsKeyboardFocusable = false`, with **no patterns at all** — no Edit, no Value, no Invoke — even
   though the caption and status text came through as Names. `jean desktop inspect` on the same
   window reports the truth: `Edit id=6293880 name="Dictation field" enabled=true patterns=Value`
   plus six `Button … patterns=Invoke`. Same OS, same window, same API family: the legacy managed
   wrapper under-resolves a WinForms provider process, and a verification built on it would have
   concluded that an editable control was not editable.
2. **Cross-process keystroke-level writes did not land, and the harness was the reason it looked
   ambiguous.** `SendMessage`/`PostMessage` `WM_CHAR` and `EM_REPLACESEL` from the probe left
   `GetWindowText` returning an empty string, while the *identical* calls against a TextBox in the
   probe's own process produced `abc` and `replaced-selection`. Combined with (1), the honest
   reading is that the probe could neither write nor read the control reliably — not that the
   control rejects input. Real keystrokes need the foreground, which Windows will not grant a
   background process, and the tester's own window is not something to steal focus from
   repeatedly on an operator's desktop.
3. **The proof that holds is JEAN's own actuator, plus a third process reading the clipboard.**
   With `[desktop] enabled = true` in an isolated `JEAN_DATA_DIR` (`target/p5u-data`, so the
   operator's settings and approvals are untouched):

   | Action, from another process | Observed |
   |---|---|
   | `desktop act --control "Dictation field" --set-value "dictate the refactor, then run cargo test"` | `before … value=` → `after … value=dictate the refactor, then run cargo test`, `changed true` |
   | `desktop inspect` afterwards | one `Edit name="Dictation field" patterns=Value`, caption `Text name="Dictation"` separate |
   | `desktop act --control "Copy all" --invoke`, then `Get-Clipboard` in the probe | clipboard held exactly that 41-character string — the window copied what the *window* sees |
   | `desktop act --control "Clear" --invoke` | ran through the same policy/task/audit path |
   | close the window | `tester exited: True`, and `no JEAN process left behind` (0 new) |

   So see, edit and copy are each demonstrated from outside, through the accessibility layer
   rather than through a screenshot.

**A defect that this verification found, which the tester did not have before.** The first `act`
refused with `not_proven: 2 controls match name=Dictation; naming one is not a tie-break` — the
caption `Label` and the edit were *both* named `Dictation`, because a WinForms edit inherits its
accessible name from the nearest label. The refusal was correct behavior by the locator and a real
usability bug for anything that wants to target the field: it is why `$dictation.AccessibleName =
'Dictation field'` exists now, and why the shipped field is addressable at all. My own earlier
`New-Button`/label pattern had the same collision for every caption in the window; it simply had
no consumer until now.

### 65.3 Parakeet TDT 0.6B v2 INT8: the pinned version is not the limit

`Cargo.lock` pins `sherpa-onnx 2.0.1` and `sherpa-onnx-sys 2.0.1` (crates.io, checksums
`6956e347…` and `5bc46e55…`), and the provisioned native tree is
`%LOCALAPPDATA%\JEAN\native\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib\lib`, which is what
`target/p5q/bin/jean-voice-sherpa-dynamic.exe` linked against.

At first look the answer is no: `sys::OfflineModelConfig` has `transducer`, `paraformer`,
`nemo_ctc`, `whisper`, `tdnn`, `sense_voice`, `moonshine`, … and **no `tdt` and no
`nemo_transducer` field** (`grep -i tdt` over both crates' `src/` finds the string only inside doc
comments). The pinned crate's own documentation then answers it
([sherpa-onnx-2.0.1/src/offline_asr.rs:664](file:///C:/Users/4Raisan/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sherpa-onnx-2.0.1/src/offline_asr.rs#L664)):
it configures a Parakeet TDT export through the **ordinary offline transducer** fields —

```rust
config.model_config.transducer = OfflineTransducerModelConfig {
    encoder: Some("…/encoder.int8.onnx"), decoder: Some("…/decoder.int8.onnx"),
    joiner: Some("…/joiner.int8.onnx"), … };
```

and upstream's CLI for the **v2** asset is
`sherpa-onnx-offline --encoder=… --decoder=… --joiner=… --tokens=… --model-type=nemo_transducer`.
`model_type` is a field on `OfflineModelConfig` in both the `-sys` and the high-level crate, so the
whole path exists at 2.0.1. **Verdict: supported by the pinned version, unreachable from this
build** — because [workers/voice/src/sherpa.rs:162](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/workers/voice/src/sherpa.rs#L162)
calls `OnlineRecognizer::create` and that is the only recognizer the worker has; a TDT export has no
streaming cache inputs for it to be fed with.

### 65.4 The asset, the destination, and what would have to be built

| Item | Value |
|---|---|
| Official download | `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2` |
| Size (from the GitHub API) | **482,440,461 bytes** (460.1 MiB compressed) |
| Checksum published by that release | **none** — the asset's `digests` field is `null`, and the model docs page publishes none |
| Per-file SHA-256 that *is* published | mirror `csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`, LFS oids below |
| Extracted destination | `%LOCALAPPDATA%\JEAN\models\asr\sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8\` |

| File | Bytes | SHA-256 (HF LFS oid) |
|---|---|---|
| `encoder.int8.onnx` | 652,282,309 | `92a10bd9c23936ea0566c29f963bc57c2a5df4261b6a8921c39abea66557ca8b` |
| `decoder.int8.onnx` | 7,257,753 | `4d097bae3bac5c96018d5293ba39f1b657296102b142affda40378f38b020f41` |
| `joiner.int8.onnx` | 1,739,080 | `77a29f3e5ac7d37074e0af465391b80f314015d5b97f0f90c6c7767f1d56f428` |
| `tokens.txt` | 9,384 | not an LFS file, so no SHA-256 published |

≈661 MB extracted. Nothing was downloaded by this batch, and disk says that is the operator's call
anyway: `C:` free was **10,882,764,800 bytes (10.13 GiB)** at the end of it.

Integration reuses the worker and the capture pipeline exactly as asked, and needs two things:

1. **An offline recognition pass.** The press already accumulates samples through the device-free
   buffer §79 built, so `EndCapture` can hand the whole span to an `OfflineRecognizer` instead of
   feeding `AcceptWaveform` per chunk — the device, the PTT state machine, the owner protocol and
   the tester are all unchanged, and the transcript still arrives as one `Event::Transcript`.
2. **Config keys for a second model family.** `SpeechModels.encoder/decoder/joiner/tokens`
   ([crates/jean-protocol/src/config.rs:45](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-protocol/src/config.rs#L45))
   already *mean* the streaming bundle, and `VoiceConfig` denies unknown fields, so pointing them at
   Parakeet files is not a shortcut — it would hand a non-streaming export to a streaming
   recognizer. A selector (`[voice] asr = "streaming" | "offline_tdt"`, defaulting to `streaming`,
   plus an offline asset block with `model_type = "nemo_transducer"`) is what keeps **Zipformer
   selectable** rather than replaced, which is also what makes a side-by-side comparison possible
   later without a rebuild.

### 65.5 The comparison, one column of it

`target/p5u-compare.ps1` runs the same two clips through each engine and prints expected words,
actual transcript, elapsed and a word-by-word diff, and it refuses to measure the wrong thing: an
engine with a stated code seam reports `not run` with the seam and the asset status rather than
quietly transcribing on whatever is configured.

| Engine | Clip | Result |
|---|---|---|
| zipformer (streaming, installed) | normal sentence, the bundle's own `0.wav`, 6,625 ms | **exact word match**, 2,592 ms wall |
| zipformer | developer terms, 5,085 ms clip | **exact word match**, 2,413 ms wall |
| parakeet-tdt-0.6b-v2-int8 | both | **not run** — 4 files absent at the destination *and* the §65.4 code seam |

Both wall figures include process start and model load, so they are an upper bound on latency, not
a decoding time. The developer-terms clip is `target/p5u-dev-terms.wav`, synthesized by Kokoro from
`Compile the branch, debug the database query, fix the schema, then deploy the server.` (24 kHz, non-
silent, peak 0.456) and then transcribed from a file: no microphone, and the same bytes for both
engines. It is machine-made speech, which is a fair *relative* input and not a fair absolute one.

**The clip's first draft was wrong and the engine said so.** The sentence I wrote first was "Run
cargo clippy and fix the async handler before you push the json payload", and Kokoro's stderr
reported `OOV word skipped: 'clippy'`, `'async'`, `'json'` — the three words the test was about.
A lexicon check of 24 candidate terms found `repository, branch, commit, deploy, server, function,
pipeline, compile, debug, syntax, terminal, database, query, schema, cache, thread, binary,
endpoint` present and `clippy, async, json, callback, refactor, webhook` absent, so the clip was
rebuilt from in-lexicon terms and re-synthesized with no OOV line. Had I trusted the expectation
instead of the warning, both engines would have been graded against words no one ever said.

The engine also repeated a warning `docs/voice-assets.md` §4a already records — `--kokoro-lang is
deprecated and ignored in sherpa-onnx >= v2.0.0. espeak-ng is no longer used. Language is
determined by the lexicon files you provide.` — which is the machine saying out loud, on every
synthesis since §63, that our `voice.models.tts_lang` and `tts_dict_dir` are set and ignored. Not a
new finding; a reason to stop leaving two keys in a config file that imply behavior 2.0.1 does not
have.

### 65.6 Validation

Every stage through the Visual Studio developer environment with `CARGO_INCREMENTAL=0`,
`--offline --locked -j 2`, read out of its own log:

| Check | Result | Log |
|---|---|---|
| Tester parse | `parse errors : 0`, non-ASCII `0`, 666 lines | `target/p5t-parse-check.ps1` |
| `voice-tester.ps1 -SelfTest -Offline`, delivered script | **11 verdicts, all PASS**, exit 0 | `target/p5u-selftest-3.log` |
| Same, twice before it | 11 PASS each; `worker ready` 5,759 / 3,751 ms, start-to-`Listening` 243 / 199 ms, release-to-words 301 / 256 / 194 ms, and 2,120 ms to free the interrupted utterance — a cold model load on this CPU, varying by about a third | `p5u-selftest.log`, `-2` |
| External field proof (`desktop act`/`inspect` + clipboard) | set-value `changed true`, Copy all → clipboard, close → exit, 0 processes left | `target/p5u-external-proof.ps1` |
| UIA tree, JEAN's own client | `Edit … patterns=Value`, 6 `Button … patterns=Invoke`, dpi 96 | run output |
| Legacy managed UIA client, same window | 10 × `Pane`, no patterns, 0 Edit — recorded as a harness limitation, not a product one | `target/p5u-tree.ps1` |
| `voice synthesize` developer clip | 5,085 ms, 244,136 B, 24 kHz, `silent false`, no OOV lines | `target/p5u-dev-terms.wav` |
| `voice transcribe`, both clips, zipformer | exact word match ×2 | `target/p5u-compare.ps1` |
| Parakeet support question | answered from `Cargo.lock`, the pinned crate sources and the docs CLI, not from memory | §65.3 |
| Workspace build/test/clippy | **not run**, deferred to Phase-5 closeout as instructed | — |

No Rust code changed in this batch, so no binary was rebuilt: `target\debug\jean.exe` is still the
§64.5a build and still matches `target/p5q/bin/jean-voice-sherpa-dynamic.exe`, which the passing
self test re-proved end to end. Nothing was downloaded or installed; the microphone and speaker
were not touched by any check (the clips are files, the sink is a counter); the operator's
`%LOCALAPPDATA%\JEAN\jean.toml`, preferences and task database were not read or written, with
isolation via `tools\voice-tester\config\*` and `target\p5u-data`.

### 65.7 Open, stated as open

* **The physical dictation test is the operator's, and it is a five-second one:** double-click the
  tester, click in the field, press the Wispr Flow hotkey, speak. What is proven here is that the
  field is a real, enabled, value-pattern edit control that an external process can write, read and
  copy; what is not proven is that *that particular app's* injection path lands in it, which needs
  the foreground and a voice.
* **Parakeet is one asset download and two code changes away**, and neither was done here: the
  decision to spend ≈661 MB extracted, and the offline-recognizer pass with a selector that leaves
  Zipformer the default (§65.4). The comparison harness is ready and self-gating.
* **`tts_lang` and `tts_dict_dir` are ignored by sherpa-onnx 2.0.1** (§65.5; `docs/voice-assets.md`
  §4a already notes the shipped `espeak-ng-data` as unused). They should either be documented as
  legacy-accepted or removed with a config migration decision; deleting them silently would break
  every existing config file's shape.
* **The managed `UIAutomationClient` under-resolution is a trap for any future PowerShell-side UIA
  test** of a WinForms or Win32 process in this repository: use `jean desktop inspect`, not
  `System.Windows.Automation`.
* **Cross-process `WM_CHAR`/`EM_REPLACESEL` into a WinForms edit did not land from a background
  probe, and is unresolved.** It did land in-process; the field's editability is proven by the
  pattern path. It is listed because an unexplained negative is not the same as a disproven one.
* **Submitting reviewed text is still unbuilt**, by design (§65.1): it belongs to the task UI as its
  own explicit action.
* **The two-process store-startup migration race** and **the `browser_live` teardown race** stay
  open, untouched here; **workspace-wide validation** still belongs to Phase-5 closeout.

**Milestone 5.5 is next and was not started.** This batch moves one specific dependency inside it:
a dictated goal now has somewhere to be reviewed before it is trusted, which is the precondition for
handing JEAN work by voice at all — and the offline-vs-streaming question the milestone inherits
(accuracy and latency of a spoken handoff) now has a named, priced answer instead of a guess.

## 64. Phase-5: a window the operator can double-click, and the three bytes a Windows client cannot help sending — 2026-11-08

The request this batch was an *experience*, not an API: double-click, press four buttons, hear
and read the result, close the window. Building it against the existing owner protocol is what
found the two gaps below — one in the protocol (a transcript could not be read without turning
it into a task) and one in the wire decoder (a stock-Windows client could not send a single
frame at all). No new speech stack was written: the window drives `jean interact` and the same
preserved Sherpa worker as §63.

### 64.1 What the window is, and what it is structurally unable to do

| File | Role |
|---|---|
| `tools/voice-tester/voice-tester.ps1` | the window: 575 lines, ASCII, parse-clean (`parse errors : 0`), WinForms on STA |
| `tools/voice-tester/JEAN Voice Tester.cmd` | double-click launcher, real microphone and speaker |
| `tools/voice-tester/JEAN Voice Tester (offline).cmd` | same window, device-free rehearsal (`-Offline`) |
| `tools/voice-tester/config/mic/jean.toml` | isolated `JEAN_DATA_DIR`: real endpoints |
| `tools/voice-tester/config/offline/jean.toml` | isolated `JEAN_DATA_DIR`: `offline_wav` is the microphone, the speaker is the counted sink |

Both launchers resolve everything from `%~dp0`, because Explorer hands a double-clicked
launcher a working directory the operator did not choose. One window: a bold status line, a
read-only transcript box, four buttons (Start listening / Stop listening / Play test sentence /
Stop playback) and a gray diagnostics line carrying `voice`, phase, task presence and utterance
id. 100 ms timer, no worker thread of its own.

* **Responsiveness is a queue, not a promise.** stdout and stderr arrive on the process's own
  event threads and only ever `Enqueue` into a `ConcurrentQueue`; the UI thread does nothing but
  dequeue, parse and compare. No `ReadLine`, no `WaitForExit`, no `Sleep` runs on it — the
  worker can be loading a 6 MB lexicon for nine seconds and `/status`-equivalent polling still
  cannot block the window. Every wait is a phase with a deadline, so no state can hang.
* **"Never submits a task" is structural, not a policy.** The seven owner commands this window
  can put on the wire are `status`, `begin_capture`, `end_capture`, `clear_capture`, `speak`,
  `interrupt`, `shutdown`. `Submit` and `SubmitTranscript` appear nowhere in the file, and the
  server side of a transcript only stores words
  ([crates/jean-core/src/interaction.rs:799](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/interaction.rs#L799))
  — it has no path from `Transcript` to a task. `model.provider = "none"` in both configs is the
  second lock on the same door.
* **Isolation is set in the script, not only on the child.** `$env:JEAN_DATA_DIR` and
  `$env:JEAN_VOICE_WORKER` are assigned inside the script process, because the self test also
  runs `jean tasks` to prove nothing was submitted — an unqualified call there would have read
  the operator's real task database. That was a defect in my own harness, caught while writing
  the proof it was needed for.

### 64.2 The two fields and one command the window needed from the protocol

`activation` is set the moment a press is *requested*; the worker answers `Capturing` when the
device stream actually *opened*. Those are different seconds — a blocked or absent default
device produces the first and never the second — and the window is required to show
"Starting microphone…" and then "Listening — speak now." So `Snapshot` gained
[`capturing`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/interaction.rs#L289),
driven only by the worker's own answer
([:792](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/interaction.rs#L792)),
never inferred from the request.

Reading the words was the harder half. `transcript_ready` already existed and said only *a
phrase exists*; the phrase itself was reachable exactly one way — `SubmitTranscript`, which is
the door to a task. There was no transcript-only path, so the tester could have shown
"Transcribing…" and then nothing, or had to start work to say what it heard. `Snapshot` gained
[`transcript`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/interaction.rs#L297)
and the protocol gained
[`ClearCapture`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-protocol/src/interaction.rs#L99)
([dispatch](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/src/interaction.rs#L413)):
release the press if it is still open, drop the activation, answer `capture discarded; no
transcript kept and nothing submitted`, and refuse a stale id. Discard is explicit rather than
automatic-on-read because a client that re-reads the snapshot must keep seeing the same words
until it decides otherwise, and because auto-clearing would silently take away the id an
operator might still hand to `SubmitTranscript` on purpose.

The focused check the batch asked for is
[`a_transcript_can_be_read_and_discarded_without_creating_a_task`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-core/tests/interaction.rs#L920):
it waits for `capturing` rather than for `activation`, refuses a second press while one is open,
reads the fixture sentence out of the snapshot after `EndCapture`, asserts `task.is_none()` at
both the transcript and the discard, asserts a stale discard is refused and that the next press
gets a different id, and asserts `read_dir(dir).count() == 0` — no task-start file was ever
written, i.e. no task worker was ever launched. **14 passed, 0 failed** in that file.

### 64.3 `EF BB BF`: every frame this tester sent was rejected, and the client could not stop it

The window could not get one command through. `jean interact` exited 1 with
`jean: control input malformed, oversized, or overloaded` and closed stdout, on the first frame,
every time. The identical JSON hand-fed from a bash pipe was accepted and answered
(`stale session or protocol version`), in both LF and CRLF form — so framing, termination and
field names were all fine, which left the bytes in front of the frame.

`target/p5s-bom-probe.ps1` and `target/p5s-bom-probe2.ps1` measured it by using python as the
child and printing the hex it actually received:

| How the parent wrote | bytes the child saw |
|---|---|
| `StandardInput.BaseStream.Write` + `StandardInput.Flush()` | `efbbbf7b2276657273696f6e223a317d0a` |
| `StandardInput.BaseStream.Write` + `BaseStream.Flush()` | `efbbbf7b2276657273696f6e223a317d0a` |
| `BaseStream.Write` + `BaseStream.Flush/Close`, writer never flushed | `efbbbf7b2276223a317d0a` |
| writer merely *accessed*, then closed, **zero bytes written by the client** | `efbbbf` |

So the three bytes are the .NET `StreamWriter`'s UTF-8 **encoding preamble**, and the last two
rows say it is not the write that emits it: touching `$p.StandardInput` in Windows PowerShell
5.1 is enough, which is why the earlier fix attempt — moving from `WriteLine` to explicit
`Encoding.UTF8.GetBytes` — changed nothing. The bytes are unavoidable from this client, and
`serde_json` will not decode `ï»¿{…}`. The CLI's reader thread has one answer for an
undecodable frame ([apps/jean-cli/src/phase5.rs:72](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/apps/jean-cli/src/phase5.rs#L72)
→ [:103](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/apps/jean-cli/src/phase5.rs#L103)):
close the session, because a client that cannot frame is not a client.

The fix belongs at that boundary. [`wire::read`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-protocol/src/interaction.rs#L176)
now drops a leading `UTF8_PREAMBLE` before decoding
([:197](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/crates/jean-protocol/src/interaction.rs#L197))
and nothing else changed: the oversize, truncated and unknown-op cases still error, and the new
test asserts a *preambled garbage* frame still errors, so the strictness the workers depend on
is intact and only the client's own preamble is tolerated. Three lines here rather than in the
client, because the alternatives were worse: `StandardInputEncoding` needs PowerShell 7 on a
launcher that must work on a stock Windows 11, a hand-made pipe plus `CreateProcess` with a
handle list is a hundred lines of P/Invoke inside a GUI script, and driving the tty text surface
instead would trade a decoder change for a text-parsing one.

`a_leading_encoding_preamble_does_not_reject_the_frame` then failed, and the failure was the
test's: it called `read(&mut stream.as_slice())` twice, which restarts at byte 0, so the second
read re-decoded the *first* frame and the assertion on `Status` failed. With one shared cursor
across both frames the test says what it needs to say — the preamble is stripped once, at the
head of the stream, and a second frame arriving without one still decodes. A version of this
test written the obvious way would have passed against an implementation that stripped a
preamble from *every* frame, which is not the property.

### 64.4 What the window says now when the machine underneath it is wrong

Three defects were found by writing the physical commands rather than by the protocol work, and
all three are reported rather than papered over. A fourth came from the launcher itself.

1. **A stale CLI was invisible.** `Snapshot` gained fields, so a `jean.exe` built before this
   batch answers a snapshot that has no `capturing` at all — and the window would have waited
   20 seconds for a microphone that was never the problem. The boot phase now checks for the
   field and says `JEAN CLI is older than this tester. Rebuild it: cargo build -p jean-cli`.
   This is not a hypothetical: it fired for real, on the still-stale 23:33 `target/debug/jean.exe`,
   at 0 ms, with `jean: control input malformed, oversized, or overloaded` on the log line below
   it (`target/p5t-stale-demo.log`) — that run is simultaneously the demonstration of the probe
   and the reproduction of §64.3. The script exits 1, which is what makes the launcher's
   `if errorlevel 1 … pause` print something instead of vanishing.
2. **A session that died before its first frame left the window spinning.** The exit check sits
   *ahead* of the `if (-not $script:Session) { return }` guard for exactly that reason: a bad
   config produces a process that exits with a stderr line and never sends a snapshot, and
   "Starting JEAN…" forever is the worst possible answer to a mistake the operator can fix.
3. **An answer could be overwritten by a poll.** `starting`, `transcribing` and `speaking` poll
   `status` on the same pipe that carries the reply the machine is waiting for, and `Pump-Frames`
   used to latch every reply it saw: if a poll's reply arrived in the same batch as the awaited
   one, the awaited one was gone and the transition never happened. `Pump-Frames` now latches a
   reply only when its request id is the pending one
   ([tools/voice-tester/voice-tester.ps1:186](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/tools/voice-tester/voice-tester.ps1#L186)).
   Found by reading the state machine, not by a hang — which is why the self test exists.
4. **A launcher that hides its console also hides its own death.** `-WindowStyle Hidden` is what
   makes the deliverable "one small window", but it means a script that throws before
   `ShowDialog` — a missing assembly, an unexpected config shape — leaves the operator with
   nothing at all, and the `.cmd`'s `echo … pause` writes into the console it just hid. There is
   now a script-scope `trap` that answers with a message box, and with a printed line plus exit 1
   under `-SelfTest`, where a dialog nobody can click would hang an unattended run.

The relink itself is gated on something outside the code: `target\debug\jean.exe` is the image
the operator's own `jean voice ptt` session (PID 25864) is running, so
`cargo build -p jean-cli` stops at `failed to remove file … Access is denied. (os error 5)` and
`[exit] 101`. That process is the operator's, was not killed, and the rebuild was armed to run the
moment it closed — which it did, 251 polls later (§64.5a).

### 64.5 What has been measured

The claim that mattered most — that a window built on PowerShell can put a valid frame in front
of `jean interact` — was made verifiable without the relink. `target/p5t-frames.ps1` extracts the
tester's **own `Send-Owner` body** from the script text, swaps its output stream for a
`MemoryStream`, runs the seven ops, and prints the bytes. Those are:

```text
{"version":1,"session":"t-18e5d8df3ad16618-1304-0","request":3,"deadline_ms":1794080109133,"body":{"op":"end_capture","activation":7}}
{"version":1,"session":"t-18e5d8df3ad16618-1304-0","request":6,"deadline_ms":1794080109138,"body":{"op":"interrupt","utterance":9}}
```

Four of those frames — preamble included, byte for byte — are a test now
(`the_exact_frames_a_powershell_client_emits_decode`), so the decoder is proven against a real
Windows client's output rather than against a hand-written Rust fixture, and `Envelope`'s
`deny_unknown_fields` means a wrong key name or a stringified number would have failed it. That
the frames arrive at all was separately confirmed on the pre-existing surface: the same JSON fed
by hand from bash was accepted and answered, which is what ruled framing, CRLF and field order
out in §64.3 before the preamble was found.

| Stage | Result | Log |
|---|---|---|
| `test -p jean-protocol --lib` | **126 passed, 0 failed** (was 124 + 2 new: the preamble, the client's exact frames) | `target/p5t-test-protocol-4.log` |
| `test -p jean-core --test interaction` | **14 passed, 0 failed**, including §64.2's transcript-only check | `target/p5t-test-protocol-2.log` |
| `clippy -p jean-protocol -p jean-core --all-targets -- -D warnings` | **`[exit] 0`** (first run `[exit] 101`: `assert_eq!(x, false)` in the new test, §64.6.4) | `target/p5t-clippy-2.log` |
| `fmt -p jean-protocol -p jean-core -- --check` | `[exit] 0` | `target/p5t-fmt-2.log`, `-3` |
| Tester parse | `parse errors : 0`, non-ASCII `0`, no BOM needed because it is pure ASCII | `target/p5t-parse-check.ps1` |
| Startup validation | `Get-SetupProblems`, extracted and driven headlessly, says nothing about the two shipped configs and, on three deliberately broken ones, names the missing worker, each unset `voice.models.*` key, and the absent offline WAV. Its first draft put **275 characters** into a 436 px label, so each message is now one ≤103-character action line with the path as its own entry. | `target/p5t-setup-problems.ps1` |
| Launcher path resolution | run from working directory `C:\`, `%~dp0` resolved the script and `Test-Path` said `True` | throwaway `_dry-run-check.cmd`, deleted |
| Self test, stale CLI | exits **1** (so the launcher's `errorlevel 1 … pause` fires), reports the CLI-version fault at 0 ms and the malformed-frame line beneath it | `target/p5t-stale-demo.log` |
| Offline audio seam | `offline_wav` outranks both audio backends, and `backend = "sherpa_onnx"` maps to `AudioBackend::System`, so the offline launcher opens no endpoint and the mic launcher does | `workers/voice/src/lib.rs:61`, `crates/jean-protocol/src/config.rs:235` |

What remained after that was the window itself, and it could not be run until the CLI relinked
(§64.4). Everything below §64.5a was measured against the binary that was already on disk —
which is why §64.5's headline result is a *frame-level* one rather than a live session.

### 64.5a The relink, and the window exercised end to end with no devices

The operator closed their own `jean voice ptt` session; the armed chain saw the image released
after 251 polls and ran the rest:

| Stage | Result |
|---|---|
| `build -p jean-cli -p jean-core -p jean-protocol --tests` | `[exit] 0` |
| `test -p jean-cli --test cli_phase5` | **7 passed, 0 failed**, 12.95 s (`target/p5t-test-cli.log`) |
| `clippy -p jean-protocol -p jean-core -p jean-cli --all-targets -- -D warnings` | `[exit] 0` (`target/p5t-clippy.log`) |
| `voice-tester.ps1 -SelfTest -Offline` | **7 verdicts, all PASS**, exit 0 (`target/p5t-selftest-2.log`) |

The self test drives the same functions the buttons call, in the same order, against the real
zipformer and Kokoro engines through `FileAudio`:

| Step | Measured |
|---|---|
| session start → `voice = ready` | **5,759 ms** (recognizer + Kokoro + the 6.3 MB lexicon) |
| `begin_capture` → `capturing` true | **243 ms** — the window shows `Starting microphone…` for that span and only then `Listening - speak now.` |
| press held | 7.0 s of the 6,625 ms published clip |
| `end_capture` → words in `snapshot.transcript` | **301 ms** from the click, 210 ms from the `Transcribing…` phase |
| the words | `AFTER EARLY NIGHTFALL THE YELLOW LAMPS WOULD LIGHT UP HERE AND THERE THE SQUALID QUARTER OF THE BROTHELS` — the first line of the bundle's own `trans.txt` |
| task presence | `snapshot.task` stayed `None` through the transcript and the discard; `jean tasks` in the tester's own data directory: **`no tasks match`**, against a `jean.db` that run created |
| `clear_capture` | `capture discarded; no transcript kept and nothing submitted`, and the words stayed in the box |
| `speak` → `interrupt` | interrupt sent **1,273 ms** into a sentence that takes ≈4.8 s to generate, i.e. during synthesis, not during playback |
| `interrupt` accepted → slot free | **2,111 ms**; whole utterance 3,454 ms |
| `shutdown` | `session exited with code 0`, no kill needed |

The 2.1-second release is the one number here I would not quote as "instant": the interrupt is
accepted immediately (`speech interruption requested; task unchanged`) and `Interrupt` is checked
in the engine's own generation callback, but the utterance slot frees about two seconds later
when the generation is native and still in progress. The tty/fixture measurement in §62.3 was
faster. Whether the gap is chunk granularity in the Sherpa callback or drain of an already
rendered span is a question for its own batch, and it is listed as open (§64.7).

Whether the gap is chunk granularity in the Sherpa callback or drain of an already
rendered span is a question for its own batch, and it is listed as open (§64.7).

**Disk, at the end.** `C:` free: 16,311,349,248 bytes (15.19 GiB) when §63 closed → 13,579,550,720
(12.65 GiB) at the start of this section's checks → **11,365,814,272 (10.59 GiB)** now, ≈4.6 GiB
consumed across this batch's build, test and clippy stages — four packages with `--tests`, which
link a second set of harnesses against the same `target\debug`. Nothing was deleted and no cache
was cleared; the tester's own `jean.db`, `logs/` and `artifacts/` from the two rehearsal runs were
**moved** to `target/p5t-run-0` and `target/p5t-run-1` so the delivered `tools\voice-tester\config`
folders contain only their `jean.toml`. At 10.6 GiB this drive is the constraint the next batch
should plan around, not this one's (§35's standing offer to size the build cache still stands, and
still deletes nothing unprompted).

Within Phase 5, using only what was already on this machine: nothing fetched, nothing installed,
no machine-wide change, no paid service, no subagent, no commit, push or publication, **no
microphone opened and no audio played** — every capture and every render in this batch went through
a file or a counted sink. The operator's `%LOCALAPPDATA%\JEAN\jean.toml`, preferences and task
database were not read or written: the tester's data directory is its own folder, and the real
`jean voice ptt` session the operator was running was left alive and untouched for the sixteen
minutes the relink waited on it.

### 64.6 Defects this batch found, and what each one was

1. **A Windows client could not speak to the session at all** (§64.3), and the message blamed the
   client's framing rather than the three bytes in front of it.
2. **A transcript could only be obtained by starting a task** (§64.2).
3. **Four of the tester's own failure modes were silent** (§64.4): stale CLI, dead session,
   clobbered reply, and a hidden console swallowing the script's own death.
4. **`assert_eq!(heard.capturing, false)`** — clippy's `bool_assert_comparison` under
   `-D warnings`, `[exit] 101` on the first clippy run of the batch, fixed to `assert!(!…)`.
5. **The preamble test passed for the wrong reason at first** (§64.3, last paragraph): two
   `read(&mut stream.as_slice())` calls each restart at byte 0.
6. **A run that printed all-PASS and `0 ms` everywhere was a run with no clocks.** The first
   `-SelfTest -Offline` pass (`target/p5t-selftest.log`) reported every duration as 0 ms —
   worker ready, capture confirmation, transcript latency, stop latency. `[System.Diagnostics
   .Stopwatch]::new()` returns a *stopped* stopwatch, so `Elapsed` never moved, which did not
   only falsify the numbers: every phase deadline is computed from it
   (`voice-tester.ps1:283`, `:306`), so the 20-second "the microphone never confirmed" and the
   45-second "no transcript arrived" guards could never fire and the window would have waited
   forever on a stuck worker. `StartNew()` fixes both, and the second run produced §64.5a's
   table. A test that passes without measuring anything is the reason this batch's self test
   prints durations at all.
7. **`'Interrupting utterance $($script:Utterance)…'` in single quotes** printed the variable
   name to the operator instead of the id. Visible in the same first log, one line below the
   zeros.

### 64.7 Open, stated as open

* **The physical plane, unchanged in kind.** No microphone was opened and no audio was played by
  this batch: the window's rehearsal ran on `FileAudio` in and a counted sink out, and whether the
  words it displays are the words you spoke, and whether the sentence it speaks is intelligible,
  remain the operator's judgement (`docs/voice-assets.md` §9 Step 1b). What is now proven is the
  wiring on both sides of that judgement, at the millisecond costs in §64.5a.
* **`interrupt` takes ≈2.1 s to free the slot while synthesis is still running** (§64.5a), against
  a fast release on the fixture path (§62.3). Not a failure — the utterance does stop and nothing
  else is cancelled — but unexplained between chunk granularity in the Sherpa progress callback
  and drain of an already-rendered span, and worth its own measurement before anyone calls the
  button "instant".
* **The window's timings are now real, and only as good as one machine.** 5,759 ms to `voice =
  ready` on this CPU is a cold model load on every launch, and the launcher offers no warm start.
* **The window's chrome is the one part I did not exercise.** `-SelfTest` drives the same four
  actions, the same `Step-Machine` and the same process supervision on the same 100 ms cadence, but
  it never constructs the form: no button was clicked, nothing was painted, and the `FormClosing`
  handler — the code that makes requirement 5 true — ran as a direct call to `Stop-JeanSession`
  rather than as a close. The operator's first double-click is the first time WinForms runs here.
* **Acoustic barge-in** is still not implemented — *Stop playback* is a control message, not a
  listener (§61.8, unchanged).
* **The two-process store-startup migration race** and **the `browser_live` teardown race** stay
  open, both untouched here.
* **A preamble-tolerant decoder is a wider door than "strip a BOM".** It is one accepted prefix
  at the head of a stream, and a preambled *garbage* frame still errors — but it is the kind of
  leniency that should be re-checked if this wire ever crosses a trust boundary it does not
  already cross.
* **Workspace-wide validation** still belongs to Phase-5 closeout.

**Milestone 5.5 is next and was not started.** What this batch adds for it is an operator-facing
surface with a measured latency budget behind it: the "give JEAN the goal out loud" demo no longer
needs a terminal, and the transcript-only read means a spoken phrase can be *shown* for
confirmation before anything is submitted — which is the shape a handoff review wants, and is not
the same thing as the existing auto-submit path.

