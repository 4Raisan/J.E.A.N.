# 04 — Settings

> **How J.E.A.N. is configured.** Every config file, every key, its default, and what it controls.
> This is the specification the implementation must match — not a sketch.

---

## Configuration model

| Rule | Detail |
|---|---|
| **Format** | TOML for all static config; SQLite `preferences` table for user-toggled runtime settings |
| **Layering** | compiled defaults → `config/defaults.toml` → `%APPDATA%\JEAN\jean.toml` (user) → `JEAN_*` environment variables → CLI flags. Later layers win. |
| **Typed** | Every file deserializes into a `serde` struct. An unknown key is a **warning**, not a silent ignore. A wrong type is a **hard error** at startup. |
| **Secrets** | **Never in a config file.** API keys come from environment variables or the OS credential store only. Config holds the *name* of the env var, not the value. |
| **Reload** | `tools.toml`, `mcp.toml` and `agents.toml` hot-reload on change (via `notify`). `defaults.toml` and `jean.toml` require a restart — say so in the log. |
| **Migration** | A `config_version` integer at the top of every file. On mismatch, migrate forward or refuse with a clear message. Never silently reinterpret old keys. |

### File map

| File | Location | Purpose | Reload |
|---|---|---|---|
| `defaults.toml` | repo `config/` → shipped in the binary's resource dir | Compiled-in defaults, versioned | restart |
| `jean.toml` | `%APPDATA%\JEAN\jean.toml` | **User** overrides | restart |
| `tools.toml` | `%APPDATA%\JEAN\tools.toml` | Discovered/declared tools and capability manifests | **hot** |
| `mcp.toml` | `%APPDATA%\JEAN\mcp.toml` | MCP servers to spawn or connect to | **hot** |
| `agents.toml` | `%APPDATA%\JEAN\agents.toml` | Coding-agent connectors | **hot** |
| `policy.toml` | `%APPDATA%\JEAN\policy.toml` | Permission tiers and approval rules | **hot** |
| `jean.db` | `%LOCALAPPDATA%\JEAN\jean.db` | Task state, history, events, approvals, preferences | n/a |
| `audit.jsonl` | `%LOCALAPPDATA%\JEAN\audit.jsonl` | Append-only audit trail | n/a |
| `logs/` | `%LOCALAPPDATA%\JEAN\logs\` | Rolling `tracing` output | n/a |
| `models/` | `%LOCALAPPDATA%\JEAN\models\` | Downloaded voice/vision models | n/a |
| `captures/` | `%LOCALAPPDATA%\JEAN\captures\` | Screen captures, **only if retention allows** | n/a |

> **Why two roots:** `%APPDATA%` (roaming) for config a user might sync; `%LOCALAPPDATA%` (local) for databases, logs, models and captures that must never roam.

---

## `defaults.toml` — complete specification

```toml
config_version = 1

# ─────────────────────────────────────────────────────────────
[core]
# ─────────────────────────────────────────────────────────────
name                = "J.E.A.N."
log_level           = "info"        # trace | debug | info | warn | error
log_dir             = "%LOCALAPPDATA%/JEAN/logs"
log_rotation        = "daily"       # minutely | hourly | daily | never
log_retention_days  = 14
log_max_files       = 30
data_dir            = "%LOCALAPPDATA%/JEAN"
single_instance     = true          # second launch focuses the first and exits
start_minimized     = true
idle_shutdown_secs  = 0             # 0 = never. Core is meant to be always-on.
panic_hook_writes_dump = true       # a panic produces a report, never a silent death

# ─────────────────────────────────────────────────────────────
[store]
# ─────────────────────────────────────────────────────────────
path                = "%LOCALAPPDATA%/JEAN/jean.db"
busy_timeout_ms     = 5000
journal_mode        = "wal"         # wal | delete
synchronous         = "NORMAL"      # OFF | NORMAL | FULL
migrations_on_start = true
retain_events_days  = 90            # events table pruning
retain_captures     = "task"        # see [vision].retention

# ─────────────────────────────────────────────────────────────
[ipc]
# ─────────────────────────────────────────────────────────────
transport           = "named_pipe"  # named_pipe | http  (http is opt-in, see below)
pipe_name           = "\\\\.\\pipe\\jean.core"
pipe_sddl           = "D:(A;;GRGW;;;BA)(A;;GRGW;;;IU)"  # Administrators + the interactive user only
min_client_integrity = "medium"     # reject clients below this integrity level
auth                = "peer_token"  # verify the connecting PID's token before serving

# HTTP is OFF by default and should stay off unless a real use case appears.
[ipc.http]
enabled             = false
bind                = "127.0.0.1"   # loopback only — never 0.0.0.0
port                = 0             # 0 = ephemeral; the chosen port is written to a local file
auth                = "token"       # per-session random token, written to %LOCALAPPDATA%\JEAN\ipc-token
cors                = false
max_body_bytes      = 1048576       # 1 MB

# ─────────────────────────────────────────────────────────────
[orchestrator]
# ─────────────────────────────────────────────────────────────
max_concurrent_tasks        = 4
max_concurrent_children     = 8
default_timeout_secs        = 300
default_retry_max           = 2
retry_backoff_ms            = [500, 2000, 8000]
verification_required       = true   # R5: never report success on launch alone
plan_preview_for_multi_step = true   # show the plan before executing >1 step
bounded_task_queue          = 64     # reject, do not grow unbounded
resume_on_restart           = true   # recover task state from SQLite after a crash

# Task states (fixed enum — not configurable):
#   queued, planning, awaiting_approval, running, verifying,
#   succeeded, failed, cancelled, needs_attention

# ─────────────────────────────────────────────────────────────
[process]
# ─────────────────────────────────────────────────────────────
use_job_objects        = true       # R3: every child is in a Job Object
kill_on_job_close      = true       # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
job_active_proc_limit  = 64
inherit_environment    = false      # allowlist only — never blanket-inherit
env_allowlist = [
  "PATH", "PATHEXT", "SystemRoot", "USERPROFILE", "APPDATA", "LOCALAPPDATA",
  "TEMP", "TMP", "HOME", "LANG", "LC_ALL", "TERM", "SHELL", "COMSPEC",
]
redact_patterns = [
  '(?i)(token|key|secret|password|passwd|authorization|cookie)\s*[=:]\s*\S+',
  '(?i)bearer\s+[A-Za-z0-9._\-]+',
  'sk-[A-Za-z0-9]{16,}',
]
capture_output_max_bytes = 10485760  # 10 MB per stream, then truncate with a marker
output_store             = "file"    # file | memory — large output never goes in SQLite
kill_grace_ms            = 3000      # SIGTERM-equivalent grace before a hard tree kill

# ─────────────────────────────────────────────────────────────
[terminal]
# ─────────────────────────────────────────────────────────────
interactive_backend = "conpty"       # conpty (raw WinCon via `windows`, D-024) | plain (anonymous pipes)
pty_rows            = 40
pty_cols            = 120
default_shell       = "powershell"

[terminal.shells.powershell]
command = "pwsh"
args    = ["-NoLogo", "-NoProfile"]
fallback_command = "powershell"      # Windows PowerShell when pwsh is absent
fallback_args    = ["-NoLogo", "-NoProfile"]

[terminal.shells.cmd]
command = "cmd"
args    = ["/d", "/q"]

[terminal.shells.gitbash]
command = "bash"
args    = ["--norc"]
enabled = false                      # only if the user has Git Bash

# ─────────────────────────────────────────────────────────────
[policy]
# ─────────────────────────────────────────────────────────────
default_tier            = "normal"   # read_only | normal | elevated
approval_timeout_secs   = 120
on_timeout              = "deny"     # deny | allow — MUST be deny by default
audit_enabled           = true
audit_path              = "%LOCALAPPDATA%/JEAN/audit.jsonl"
audit_redacts_secrets   = true
run_elevated_by_default = false      # never run the whole app elevated
elevation_helper        = "jean-elevated.exe"
never_store_credentials = true
block_on_uipi           = true       # detect integrity mismatch and report, do not silently retry

# Actions requiring explicit approval (tier: elevated/destructive)
require_approval = [
  "write_outside_workspace", "delete_permanent", "process_kill",
  "service_control", "registry_write", "install_software",
  "credential_read", "network_egress_new_domain", "elevated",
  "modify_security_settings", "bulk_file_change",
]

# Who is asked, and how often (D-039).
# A person typing `jean do "<goal>"` authorises THAT RUN: a step whose capability appears in
# require_approval is covered once, bound to the plan validated from their goal, to the step's
# resolved scope digest, and it expires when the run returns. It is never written to the approval
# ledger, so the next run is asked again, and nothing a model or a tool printed can widen it.
# `auto_deny` and the elevation boundary are decided BEFORE any grant is consulted, so a granted run
# refuses exactly what an ungranted one refuses. `jean do --prompt-each` opts back into being asked
# about every step. There is no configuration key for this: a file cannot answer on a person's behalf.

# Actions that are refused outright, with no approval path
auto_deny = [
  "format_volume", "registry_hive_load", "disable_defender",
  "modify_uac", "shadow_copy_delete", "disable_audit_logging",
  "write_to_own_config_from_model", "grant_self_permission",
]

# ─────────────────────────────────────────────────────────────
[workspace]
# ─────────────────────────────────────────────────────────────
root                     = ""        # empty = prompt on first run
allow_write_outside_root = false
delete_mode              = "recycle_bin"   # recycle_bin | permanent
max_file_read_bytes      = 52428800        # 50 MB
max_file_write_bytes     = 104857600       # 100 MB
follow_symlinks_outside_root = false       # a symlink is not an escape hatch

# ─────────────────────────────────────────────────────────────
[tools]
# ─────────────────────────────────────────────────────────────
registry_path           = "%APPDATA%/JEAN/tools.toml"
mcp_config_path         = "%APPDATA%/JEAN/mcp.toml"
discovery               = ["path", "registry", "mcp", "acp"]
health_check_interval_secs = 300
health_check_timeout_ms    = 5000
mark_unhealthy_after       = 3
auto_discover_on_start     = true

# ── As built in Phase 1 ──────────────────────────────────────
# tools.discovery              shipped value is ["path", "start_menu", "registry"].
#                              `mcp` and `acp` are REJECTED at load with a message:
#                              they are Phase 3 sources, and naming one is a
#                              promise of a result that will never arrive.
# tools.health_check_interval_secs
#                              the freshness window for a cached probe answer, not a
#                              background timer. Phase 1 has no scheduler; `jean
#                              health [--now]` and the pre-run check consult it.
# tools.health_check_timeout_ms
#                              accepted and stored. A probe is a filesystem lookup
#                              (D-026), so today nothing can spend the budget; the
#                              key exists so a later running probe needs no schema
#                              change to explain itself.
# tools.mark_unhealthy_after   live. Default 3; `0` is a hard error, because a tool
#                              would be unhealthy before its first probe.
# tools.mcp_config_path, tools.auto_discover_on_start
#                              NOT in the Phase-1 struct. Kept here as the Phase 2/3
#                              intent they carry; the loader warns on an unknown key,
#                              so these lines are documentation, not switches.

# ─────────────────────────────────────────────────────────────
[model]
# ─────────────────────────────────────────────────────────────
provider = "none"       # none | ollama | openai_compatible — and nothing else.
# These NEVER go to a model, whatever the provider is (R4):
never_delegate_to_model = [
  "permission_checks", "argument_validation", "path_handling",
  "process_supervision", "task_state_transitions", "verification",
  "policy_decisions", "secret_handling",
]


# Shipped shape, as of Phase 2. One flat `[model]` table, not one sub-table per provider:
# a configuration selects exactly one provider, so the endpoint, the model name and the
# credential-variable name belong to the choice that is live (D-030).
#
# `anthropic` and `openai` are NOT accepted. Naming a cloud API is a load error, not a
# request that might spend money, because this batch was told not to use a paid API
# without explicit authorization.
#
# `require_provider_for` and `deterministic_fallback` are not keys either. `provider = "none"`
# IS the deterministic fallback: every Phase-0 and Phase-1 command runs exactly as before and
# `jean plan` / `jean do` refuse. `start_if_missing` is deliberately absent — this build never
# launches a model server.

base_url             = "http://127.0.0.1:11434"  # https is accepted and VERIFIED (D-038). Plain http
#                                        # is allowed only for a loopback host: a remote http endpoint
#                                        # would carry the prompt and any bearer token unencrypted
model                = ""                        # required whenever a provider is selected
api_key_env          = "JEAN_MODEL_API_KEY"       # the env var NAME, never the value; allowed empty on loopback
request_timeout_secs = 120                        # one request: connect, resolve and body reading all bounded
max_response_bytes   = 1048576                    # larger is a transport failure, never a truncation. Applied while
#                                        # reading, so it holds a chunked answer that declares no length
max_prompt_bytes     = 262144                     # the ceiling on what JEAN sends
max_output_tokens    = 2048                       # asked of the provider where it understands one
temperature          = 0.0                        # refused above 0.3: a plan must be reproducible from the record
stream               = false                      # NDJSON (Ollama) or SSE (OpenAI-compatible) when true; both are
#                                        # chunked, which the transport decodes (D-038). The bound still applies
retry_backoff_ms     = [500, 2000, 8000]          # transport retries only. A tool is NEVER retried
max_plan_steps       = 8                          # may only tighten the compiled ceiling of 32
summary_wording      = "record"                   # record | model. The facts always come from the rows (D-036)

# ─────────────────────────────────────────────────────────────
[voice]
# ─────────────────────────────────────────────────────────────
# Phase 5 rebuilt this section around the keys the build actually reads, and the ones it does not are
# listed at the bottom rather than left to fail `deny_unknown_fields` in someone's file. `[voice]` left
# `DEFERRED_SECTIONS`, so these are applied — but "applied" needs the honest split: `enabled` is the
# whole gate, and `backend = 'sherpa_onnx'` is refused with the key names to fix until the speech
# assets and the `sherpa` feature are provisioned (docs/voice-assets.md).
#
# R6: disabled is the shipped default, and disabled means no worker process, no model loaded and no
# device opened — not a worker that loaded everything and then idled.
enabled       = false
mode          = "push_to_talk"          # the only mode. `hotkey` below is a *trigger* for a press, not
                                        # a second listening mode, and there is no wake word: nothing is
                                        # captured until a press begins, and nothing is submitted until
                                        # the transcript is explicitly released AND submitted.
backend       = "sherpa_onnx"           # sherpa_onnx (real) | fixture (test double, labelled as one)

startup_timeout_ms   = 5000             # Hello, the process handshake
request_timeout_ms   = 2000             # every short control: Ping, Interrupt, BeginCapture
shutdown_timeout_ms  = 2000             # Bye and the reap
configure_timeout_ms = 30000            # model loading, timed apart because it is the one voice request
                                        # that legitimately takes seconds
capture_timeout_ms   = 30000            # how long one press may stay open
utterance_timeout_ms = 120000           # how long one utterance may run

finalize_budget_ms   = 1200             # tail flush after release. Must stay BELOW
                                        # request_timeout_ms, or the owner reaps a worker that is merely
                                        # still decoding — validation refuses the combination instead of
                                        # letting it look like a hang.
capture_buffer_ms    = 2000             # ceiling on audio held between the device and the recognizer.
                                        # Overflow drops the OLDEST frames and counts them; the count
                                        # reaches the control loop and is reported, never hidden.
sample_rate          = 16000            # the ENGINE rate. Whatever the device delivers is resampled to
                                        # it; this is not a device selection and not a display setting.
input_device         = ""               # endpoint id from `jean voice devices`; empty = system default
output_device        = ""
offline_wav          = ""               # VALIDATION SEAM, empty by default. Set to a WAV path and the
                                        # worker reads that file instead of any endpoint and renders to
                                        # a counted sink instead of any speaker: one whole
                                        # press-release-transcript cycle runs against a real engine with
                                        # nothing recorded and nothing played. It outranks the
                                        # backend-derived audio choice, which is how native inference and
                                        # native cancellation became measurable at all (§61.5). A file is
                                        # replayed at 1x, so a release 40 ms in has genuinely heard 40 ms.
hotkey               = "Ctrl+Alt+Space" # parsed and validated. Needs Ctrl, Alt or Win, so ordinary
                                        # typing cannot start a capture. Used by `jean voice ptt`.
announce_task_state  = false            # OFF on purpose: an announcement occupies the single utterance
                                        # slot, so enabling it changes when an explicit `speak` fits.
spoken_controls      = false            # OFF on purpose: it intercepts a transcript that would
                                        # otherwise become a task goal. Exact-match only, and only while
                                        # a task is active — it can report or cancel, never start work.
fixture_transcript   = "inspect this process"   # only read when backend = 'fixture'
fixture_utterance_ms = 10                       # only read when backend = 'fixture'. The real engine's
                                                # utterance length comes from the text; the fixture's is a
                                                # fixed block of silence, and at 10 ms it is over before an
                                                # interrupt can land in it. Raise it (with `offline_wav`
                                                # set, so render paces at 1x) and playback becomes long
                                                # enough to stop part-way, which is how the operator-facing
                                                # interruption path is tested without a synthesizer.

# Device authority: with backend = 'fixture' the worker uses a simulated endpoint and can never open a
# microphone. There is no key that pairs a fixture engine with real capture, by design — a test that
# records the operator is not a test. `offline_wav` is the other direction and obeys the same rule:
# it pairs a real engine with a file, so neither the microphone nor the speaker is involved.

# The keys below are NOT read, and are named rather than silently rejected:
#   wake_word                  needs a trained keyword model (openWakeWord weights are CC BY-NC-SA)
#   process / transport / pipe_name
#                              the worker is a sibling executable over inherited stdio pipes; there is
#                              no configurable pipe name in this build
#   cancel_scope               not a setting. Separate voice cancellation is structural (D-006), so a
#                              key that could turn it off would be a lie
#   barge_in                   not implemented as planned. `Interrupt` stops playback through a control
#                              message; nothing detects speech over the speaker, so this document does
#                              not claim it does
#   unload_when_idle_secs      models stay loaded while enabled; there is no idle unload to schedule
#   voice.models.dir / asr / asr_accurate / vad / tts / kws
#                              the plan named bundles; the build takes explicit file paths instead,
#                              because a directory plus a bundle name is a second resolution rule to
#                              get wrong and it cannot report which file it actually meant

[voice.models]
# Two separate prerequisites, measured and reported separately (assets.rs): a machine with only
# the four recognition files recognises, and `jean voice doctor` says so instead of blaming it
# for a voice bundle it never asked for. Both halves are now filled in the isolated test
# configuration — the zipformer bundle transcribes real audio (§60.5) and Kokoro loads and
# synthesizes (§61.3) — while the shipped defaults stay empty. To try a set of paths without
# editing your own settings, keep them in `<data_dir>\jean.toml` with `JEAN_DATA_DIR` pointed at
# that directory; it redirects the writes as well as the read.
encoder          = ""   # streaming transducer encoder (.onnx)
decoder          = ""   # streaming transducer decoder (.onnx)
joiner           = ""   # streaming transducer joiner (.onnx)
tokens           = ""   # ASR tokens.txt — must come from the same bundle as the three above
tts_model        = ""   # VITS model, or Kokoro's model.onnx when tts_voices is set
tts_tokens       = ""   # TTS tokens.txt
tts_lexicon      = ""   # phoneme lexicon. OPTIONAL for VITS, REQUIRED for Kokoro on sherpa-onnx
                        # 2.0.1: measured, the engine loads model+voices+tokens and then stops with
                        # "No lexicon available. Please provide --kokoro-lexicon". `jean voice
                        # doctor` reports it as absent for a Kokoro configuration, and
                        # `jean voice synthesize --phonemes` is the only path that does not need it
tts_voices       = ""   # Kokoro voices.bin. SETTING THIS IS THE ENGINE SWITCH: neither engine can
                        # infer the bundle type from a filename, and feeding one engine the other's
                        # weights is a load failure with no useful message
tts_dict_dir     = ""   # text-frontend directory. The English Kokoro archive does ship
                        # espeak-ng-data and 2.0.1 no longer uses espeak-ng, so naming it changes
                        # nothing about whether text can be spoken; measured only when set
tts_lang         = ""   # language tag for the synthesis frontend, e.g. en-us. DEPRECATED and
                        # ignored by Kokoro since 2.0.0 — the engine prints a warning and derives
                        # the language from the lexicon instead
tts_speaker      = 0    # speaker index inside tts_voices -> GenerationConfig::sid. Identity is an
                        # index, not a name; the bundle decides what 0 is
vad              = ""   # optional Silero VAD. Used only at sample_rate = 16000
native_lib_dir   = ""   # diagnostic; a static link needs no runtime library directory

# ─────────────────────────────────────────────────────────────
[vision]
# ─────────────────────────────────────────────────────────────
enabled                = false
capture_on_demand_only = true            # a task must request visual context
continuous_capture     = false           # HARD NON-GOAL — see 01-SCOPE.md
backend                = "windows_capture"   # windows_capture | xcap
still_backend          = "xcap"
ocr_backend            = "none"          # none | sidecar (RapidOCR) | ocrs (Rust)
vlm_backend            = "none"          # none | ollama | openai_compatible
vlm_model              = ""              # e.g. "showui-2b" via llama.cpp
retention              = "discard_after_task"   # discard_after_task | keep_24h | keep_until_manual
capture_dir            = "%LOCALAPPDATA%/JEAN/captures"
max_capture_pixels     = 4194304         # 4 MP cap per capture
capture_cursor         = false
capture_border         = false           # disable the WGC yellow border
prefer_uia_text        = true            # R1: UIA text patterns BEFORE OCR

[vision.order_of_operations]             # R1 — semantic first, pixels second
sequence = ["application_api", "uia", "synthetic_input", "ocr", "vlm_grounding"]

# ─────────────────────────────────────────────────────────────
[browser]
# ─────────────────────────────────────────────────────────────
backend         = "playwright_mcp"      # playwright_mcp | cdp | webdriver
command         = "npx"
args            = ["@playwright/mcp@latest"]
headless        = false                 # the user usually wants to see it
prefer_dom      = true                  # R1
visual_fallback = false
timeout_secs    = 120
user_data_dir   = ""                    # empty = a temp profile, never the user's real profile by default

[browser.cdp]                             # fallback: no Node required, Edge ships with Win11
executable = ""                           # empty = auto-detect msedge.exe / chrome.exe
port       = 9222

# ── As built in Phase 3 ──────────────────────────────────────
# The keys above are the plan's. What this build actually has is a flat `[browser]`
# section, parsed and validated, with nothing acting on it yet:
#
#   enabled              bool   = false   — nothing launches a browser until an operator says so
#   browser_path         String = ""      — empty = detect an installed Chrome/Edge/Brave.
#                                           Detection that finds nothing is a reported blocker,
#                                           never a download (D-046).
#   headless             bool   = true    — the default is inverted from the plan above: a window
#                                           the operator did not ask for is a surprise, so `false`
#                                           is the choice that needs a reason.
#   startup_timeout_secs u64    = 30
#   action_timeout_secs  u64    = 30
#   max_page_bytes       usize  = 524288  — ceiling on page content returned to a caller
#   allowed_origins      [String] = ["file", "http://127.0.0.1"]
#                                           — a navigation outside this list is refused at the
#                                           intent, not by a rule inside the browser; an entry that
#                                           allows "anywhere" is a load error.
#
# `backend` / `command` / `args` / `prefer_dom` / `visual_fallback` / `user_data_dir` /
# `[browser.cdp]` are NOT in the struct. `backend` is not a knob this build offers because
# there is one decided path (D-046: loopback CDP against an installed Chrome), and the
# debugging port is allocated per run by the process JEAN starts — a fixed 9222 in config
# would be a port someone else could already be holding.
#
# `[browser]` is **no longer** in `DEFERRED_SECTIONS` in `jean-protocol/src/config.rs` (as of
# 2026-11-06): every key above is read by the driver, and a file that sets this section gets no
# "recognised but not applied" warning because that sentence had become false — `enabled` gates the
# command, `browser_path` (or detection) chooses the executable, `allowed_origins` is enforced before
# navigation, the two timeouts bound the startup and each action, and `max_page_bytes` caps what is
# read back. The deferred list was then `voice`, `vision`, `agents`, `telemetry` — **`voice` has since
# left it** (2026-11-07, §58): `[voice]` is now read key by key and the list is `vision`, `agents`,
# `telemetry`; `agents` stays there because the real delegation settings live in `[delegation]`.
# **The sentence after it was false as
# written** — it said "the two Phase-4 sections stay because nothing in this build reads them". Nothing
# in this build reads `[voice]` or `[vision]`, and the Phase-4 settings that *are* read live in a
# section this document had never specified: `[desktop]`, below. Evidence:
# IMPLEMENTATION_STATUS.md §45, §46 for the browser; §49–§52 for the desktop.

# ─────────────────────────────────────────────────────────────
[desktop]
# ─────────────────────────────────────────────────────────────
# No such section existed in the plan: Phase 4 was specified as capabilities, not as keys. This is the
# section as shipped, and every key below is read by the build — `[desktop]` is not in
# `DEFERRED_SECTIONS` (`jean-protocol/src/config.rs:33` is `["vision", "agents", "telemetry"]` —
# `voice` left that list in Phase 5 and is read now, see `[voice]` above),
# so setting it neither earns an "recognised but not applied" warning nor goes unknown-and-pruned.
#
#   enabled               bool   = false — gates `act`, `input` and `capture`. `windows`, `inspect` and
#                                                `diagnose` run either way: a window that cannot be
#                                                worked on still has to be diagnosable, and a read that
#                                                created a task row would make every diagnostic an
#                                                auditable event.
#   uia_timeout_secs      u64    = 5     — how long one UI Automation conversation is waited for before
#                                                its worker **process** is terminated (D-051). The bound
#                                                is the whole protection a hung provider gets, and the
#                                                thing it ends is now a process tree rather than a thread
#                                                that could only be walked away from; the effect of a
#                                                call cut off mid-flight is still uncertain and is never
#                                                replayed.
#   capture_timeout_secs  u64    = 10    — the same bound, for one frame.
#   max_tree_nodes        usize  = 2000  — hitting it is reported as a truncation, never as a complete
#                                                tree.
#   max_capture_pixels    usize  = 4194304 — a window larger than this is refused, not resized: there is
#                                                no scaler in this tree and a shrunken frame makes every
#                                                coordinate built from it false.
#   capture_dir           String = "%LOCALAPPDATA%/JEAN/captures" — consulted only when frames are kept.
#                                                A *relative* value is anchored to the temp directory
#                                                rather than left to wherever the command was started.
#   retention             String = "discard_after_task" — the other two accepted values are `keep_24h`
#                                                and `keep_until_manual`. Anything else is a **load
#                                                error**, measured here: the suite's first run wrote
#                                                `retention = "keep"` and the command refused rather than
#                                                quietly defaulting. `keep_24h` **expires** (2026-11-06):
#                                                a kept frame is claimed in `jean-captures.jsonl` before
#                                                its bytes are written, that claim is the only thing that
#                                                authorises its removal, the kept set is held under a
#                                                512 MiB ceiling oldest-first, and a frame that cannot be
#                                                claimed is not captured. Files the claim does not name
#                                                are reported as unowned and never touched.
#                                                What no value does is redact: a retained frame is the
#                                                screen as it looked, and the masking that text readback
#                                                applies to password-flagged controls does not reach a
#                                                picture.
#   allow_synthetic_input bool   = false — a separate decision from `enabled`, because turning the
#                                                actuator on is agreeing to pattern actions, not to
#                                                synthesised keystrokes.
#   max_concurrent_workers u32   = 4     — the number of **live `jean-desktop-worker.exe` processes**
#                                                tolerated at once (D-051). Every call ends its own
#                                                worker, so reaching this means workers are not exiting;
#                                                past it desktop work is refused rather than queued. The
#                                                value is unchanged by the correction — what changed is
#                                                that it counts processes that can be killed instead of
#                                                threads that could only be abandoned.
#
# The capabilities the gate names are `desktop.act`, `desktop.input` and `desktop.capture`, and all three
# are shipped **out of** `require_approval` — deliberately, not by omission. A prompt nobody can answer
# (a non-terminal stdin answers `denied`) would close the whole actuator to scripts and agents, and the
# `operation_and_arguments` scope already binds any remembered approval to the exact control and action.
# Evidence: `crates/jean-protocol/src/config.rs` (`default_require_approval`), `config/defaults.toml`,
# IMPLEMENTATION_STATUS.md §49.

# ─────────────────────────────────────────────────────────────
[agents]
# ─────────────────────────────────────────────────────────────
acp_enabled        = true
config_path        = "%APPDATA%/JEAN/agents.toml"
default_agent      = ""
worktree_isolation = true                # git worktree per parallel agent run
max_parallel       = 3

# ACP HAS NO session/stop. Interruption is session/cancel; teardown is a process kill.
cancel_via = "session/cancel"
kill_via   = "process"
kill_grace_ms = 5000

verify_after_agent = true                 # R5: an agent reporting "done" is not evidence
verification_command = ""                 # e.g. "npm test" — empty = infer from the project

# ── As built in Phase 3 ──────────────────────────────────────
# There is no `[agents]` section and no `agents.toml`. The shipped section is
# `[delegation]`, in jean.toml, and it is worth naming the differences because each one is
# a place the plan would have let something happen that the code now refuses:
#
#   enabled                    bool   = true   — false makes `jean delegate` refuse and leaves
#                                                 every Phase-0/1/2 command untouched
#   default_worker             String = "opencode"
#   max_parallel               u32    = 1      — **a value above 1 is a load error**, not a
#                                                 clamp: one active task per process (D-028), so
#                                                 concurrent workers ship only after the
#                                                 cancellation design is resolved (D-042)
#   session_timeout_secs       u64    = 900    — spawn to teardown
#   idle_timeout_secs          u64    = 180    — silence ≠ slowness; must be < the session deadline
#   permission_timeout_secs    u64    = 120    — a worker waiting forever for an answer is refused
#   shutdown_grace_ms          u32    = 2000
#   max_event_queue            usize  = 256    — a full queue stops the reader, which back-pressures
#                                                 the worker; dropping progress is not an option
#   max_frame_bytes            usize  = 262144 — the SDK has no frame ceiling, so this is the only
#                                                 bound between a chatty agent and this memory
#   max_output_bytes           u64    = 8388608 — per worker, per stream
#   [[delegation.workers]]      name, kind (`acp` | `codex-cli`), command, args, enabled, model,
#                              env_passthrough, capability (`agent.delegate`),
#                              sandbox (`read-only` | `workspace-write`)
#                              Two workers ship by default: `opencode acp` (ACP) and
#                              `codex exec` (the structured CLI fallback — the installed Codex
#                              0.160.1 has no `acp` subcommand, so the adapter named in the plan
#                              above is not what this build talks to). An unknown kind is a load
#                              error naming what exists; `danger-full-access` is absent on purpose.
#   verify_command             String = ""     — **empty = `jean delegate` refuses.** This is the
#   verify_args                [String]         inverse of the plan's "infer from the project":
#   verify_timeout_secs        u64    = 900     JEAN never infers, and never runs a command the
#   isolate_in_worktree        bool   = true    agent suggested — the check is resolved from config
#   worktree_root              String = ""      *before* the delegation starts, so the worker can be
#   require_clean_baseline     bool   = true    told what has to pass (D-043)
#   max_changed_files_reported usize  = 200
#
# `acp_enabled`, `config_path`, `default_agent`, `worktree_isolation`, `cancel_via`, `kill_via`,
# `kill_grace_ms`, `verify_after_agent` and `verification_command` are not keys. Cancellation is
# not configurable because it is not a preference: `session/cancel` is sent, then the job object
# closes the tree, and `shutdown_grace_ms` is the only knob in that path. `verify_after_agent`
# does not exist because verification is not optional — an empty `verify_command` is a refusal.

# ─────────────────────────────────────────────────────────────
[ui]
# ─────────────────────────────────────────────────────────────
shell               = "tray"            # tray | cli   (a rich UI is a SEPARATE process, later)
tray_tooltip        = "J.E.A.N."
tray_left_click     = "open_task_board"
tray_right_click    = "menu"
notifications       = "none"            # toast | none — shipped default is `none`, see below
app_user_model_id   = "JEAN.Orchestrator"   # MANDATORY for Win11 toast attribution

# ── As built in Phase 1 ──────────────────────────────────────
# ui.notifications           the ONLY accepted values are `none` and `toast`, and the
#                            default is `none` (config/defaults.toml). A toast is drawn
#                            on the operator's own desktop and, to be attributed at all,
#                            needs its AppUserModelId written into
#                            HKCU\Software\Classes\AppUserModelId\JEAN.Orchestrator.
#                            Completing a command does not earn the right to make that
#                            per-user change, so `jean notify register` is the explicit
#                            opt-in and `deliver` refuses politely while it is unset.
#                            `is_registered` answers without writing anything.
# shell / tray_* / theme / disclosure_arrow
#                            NOT in the Phase-1 struct — there is no tray in this build
#                            (D-009); they are Phase 4 UI keys held here for intent.
show_task_board     = true
show_approval_queue = true
theme               = "system"          # system | light | dark
disclosure_arrow    = "right_down"      # right = collapsed, down = expanded

[ui.aumid_registry]                     # written under HKCU for unpackaged apps
path        = "Software\\Classes\\AppUserModelId\\JEAN.Orchestrator"
display_name = "J.E.A.N."

# ─────────────────────────────────────────────────────────────
[telemetry]
# ─────────────────────────────────────────────────────────────
enabled          = false
phone_home       = false               # default: the product sends nothing anywhere
crash_reports    = false
anonymous_stats  = false
update_check     = true                # local version check only; no telemetry attached
update_channel   = "stable"            # stable | beta
```

---

## `tools.toml` — tool registry

```toml
config_version = 1

[[tool]]
id            = "powershell"
kind          = "shell"                # shell | app | cli | agent | mcp | browser
display_name  = "PowerShell"
command       = "pwsh"
fallback_command = "powershell"
args          = ["-NoLogo", "-NoProfile"]
interactive   = true                   # uses ConPTY
tier          = "normal"               # read_only | normal | elevated
capabilities  = ["exec", "script", "pipeline"]
health_check  = { command = "pwsh", args = ["-NoLogo", "-NoProfile", "-Command", "exit 0"], timeout_ms = 5000 }
verify        = { method = "exit_code_and_output" }
enabled       = true

[[tool]]
id            = "git"
kind          = "cli"
display_name  = "Git"
command       = "git"
tier          = "normal"
capabilities  = ["vcs", "worktree", "diff", "log"]
discovery     = { method = "path" }
verify        = { method = "exit_code" }
enabled       = true

[[tool]]
id            = "vscode"
kind          = "app"
display_name  = "VS Code"
command       = "code"
tier          = "normal"
capabilities  = ["editor.open", "editor.install_extension"]
discovery     = { method = "path", also = "registry" }
uia_target    = { process = "Code.exe", window_class = "Chrome_WidgetWin_1" }
enabled       = true

[[tool]]
id            = "brave"
kind          = "app"
display_name  = "Brave"
command       = "brave"
tier          = "normal"
capabilities  = ["browser.open"]
accessibility_flag = "--force-renderer-accessibility"   # REQUIRED for a UIA tree in Chromium
enabled       = true

[[tool]]
id            = "docker"
kind          = "cli"
display_name  = "Docker"
command       = "docker"
tier          = "elevated"             # daemon control is treated as elevated
capabilities  = ["container.run", "container.list", "image.list"]
enabled       = false                  # off unless the user has Docker
```

**Required fields for every tool:** `id`, `kind`, `command`, `tier`, `capabilities`, `verify`. A tool without a `verify` method is rejected at load — R5 is not optional.

---

## `mcp.toml` — MCP servers

```toml
config_version = 1

[[server]]
id        = "playwright"
transport = "stdio"                    # stdio | streamable_http
command   = "npx"
args      = ["@playwright/mcp@latest"]
tier      = "normal"
enabled   = true
# rmcp's transport-child-process spawns and supervises this.

[[server]]
id        = "filesystem"
transport = "stdio"
command   = "npx"
args      = ["-y", "@modelcontextprotocol/server-filesystem", "%JEAN_WORKSPACE_ROOT%"]
tier      = "normal"
enabled   = false                      # J.E.A.N. has its own workspace-scoped file tools

[[server]]
id        = "local-http-example"
transport = "streamable_http"
url       = "http://127.0.0.1:8000/mcp"
auth      = { type = "none" }          # none | bearer_env
bearer_env = ""
tier      = "read_only"
enabled   = false
```

**As built in Phase 3.** There is no `mcp.toml`; MCP lives in jean.toml, and the shape is different
in three ways that each carry a reason:

```toml
[mcp]
enabled             = true           # false refuses every mcp command before any task row exists
request_timeout_secs = 120
max_message_bytes   = 262144         # one JSON-RPC line, either direction
max_result_bytes    = 1048576        # one tool result's content, after which `truncated` is set
shutdown_grace_ms   = 2000
expose              = ["echo.tool"]  # tool ids published when JEAN is the server — opt-in per id

[[mcp.servers]]
name           = "fixture"
command         = "node"
args            = ["mcp-server.js"]
enabled         = true
allow_tools     = ["greet"]          # empty means NONE; a server with 30 tools grants 0 capabilities
capability      = "mcp.call"
env_passthrough = []                 # names beyond [process].env_allowlist
```

- **`transport` is not a key, because only stdio exists here.** JEAN listens on no port and dials
  no URL; both roles use this process's own standard streams or a pipe JEAN creates for a child it
  spawned (D-045). So `streamable_http`, `url`, `auth` and `bearer_env` are absent — writing one
  into `[mcp]` earns the ordinary "unknown configuration key … ignored" warning. `[browser]` was once the
  contrast case here — it sat on the deferred list and was told "recognised but not applied" — and since
  2026-11-06 it is a section this build reads end to end, so the comparison no longer holds and the list
  was `voice`, `vision`, `agents`, `telemetry`. `[voice]` left that list on 2026-11-07 (§58), so the
  list is now `vision`, `agents`, `telemetry`.
- **JEAN spawns the child, not `rmcp`.** The comment in the plan ("rmcp's `transport-child-process`
  spawns and supervises this") describes the path that was rejected: the SDK's own process handling
  would sit outside the job object, the kill-on-close guarantee and the audit trail. `jean-process`
  starts the server, `rmcp` gets the two pipe halves over `transport-async-rw` (D-041).
- **`id` / `tier` are `name` / `capability`.** A call is decided by the same policy engine as any
  other command, on a capability the operator configured (`mcp.call` by default), and an allowlist
  refusal happens **before anything is spawned** — a server that will not be used is not started to
  be told "no".
- **`enabled = false` on the playwright row is now moot:** `@playwright/mcp` is superseded for the
  browser entirely (D-046), and no npm-installed MCP server is configured on this machine.
- **The server role is a command, not a config file:** `jean mcp serve` publishes exactly
  `[mcp] expose`, and a published call still goes through the policy pipeline — an ask that would
  need a question JEAN cannot ask in stdio is refused, with the question printed to stderr
  (IMPLEMENTATION_STATUS.md §36.1).

---

## `agents.toml` — coding-agent connectors

```toml
config_version = 1

[[agent]]
id           = "codex"
protocol     = "acp"                   # acp | jsonl | http | pty
command      = "codex-acp"             # the @agentclientprotocol/codex-acp adapter
fallback     = { protocol = "jsonl", command = "codex", args = ["exec", "--json"] }
app_server   = { command = "codex", args = ["app-server"] }   # JSON-RPC over stdio
license      = "Apache-2.0"
windows      = "native"
hooks_enabled = true                   # 14 Codex hook events; handler type may be "mcp_tool"
tier         = "normal"
enabled      = true

[[agent]]
id           = "opencode"
protocol     = "acp"
command      = "opencode"
args         = ["acp"]
http_server  = { command = "opencode", args = ["serve", "--port", "4096", "--hostname", "127.0.0.1"] }
license      = "MIT"
windows      = "native"
tier         = "normal"
enabled      = true

[[agent]]
id           = "claude-code"
protocol     = "acp"
command      = "claude-agent-acp"      # @agentclientprotocol/claude-agent-acp 0.100.0, Apache-2.0
fallback     = { protocol = "jsonl", command = "claude", args = ["-p", "--output-format", "stream-json", "--verbose"] }
license      = "PROPRIETARY"           # we may DRIVE it; we may never bundle or redistribute it
requires     = "user-installed"
windows      = "npm"
tier         = "normal"
enabled      = false                   # off unless the user installed Claude Code themselves

[[agent]]
id           = "gemini-cli"
protocol     = "acp"
command      = "gemini"
args         = ["--acp"]
license      = "Apache-2.0"
requires     = "google-account"
enabled      = false

[[agent]]
id           = "qwen-code"
protocol     = "acp"
command      = "qwen"
args         = ["--experimental-acp"]  # NOTE: NOT --acp. Experimental.
http_server  = { command = "qwen", args = ["serve"], port = 4170 }
license      = "Apache-2.0"
enabled      = false

[[agent]]
id           = "goose"
protocol     = "acp"
command      = "goose"
args         = ["acp"]
license      = "Apache-2.0"
windows      = "native"
enabled      = false
```

---

## `policy.toml` — permission tiers

```toml
config_version = 1

[tier.read_only]
description = "Observe only. No state change anywhere."
prompt      = false
examples    = ["screen capture", "UIA tree read", "file read inside workspace",
               "process list", "window enumeration", "git status", "git log"]

[tier.normal]
description = "Change state inside the declared workspace, or drive an application."
prompt      = "first_use_per_scope"    # never | first_use_per_scope | always
examples    = ["input injection into an app", "browser navigation", "file write in workspace",
               "git commit", "npm test", "launch an app", "coding-agent run"]

[tier.elevated]
description = "Requires administrator integrity, or is destructive/irreversible."
prompt      = "always"
examples    = ["service control", "registry write", "permanent delete", "install software",
               "kill a process outside the workspace", "credential read", "bulk file change"]
uac         = true
helper      = "jean-elevated.exe"
scope       = "single_operation"       # never a standing elevation
show        = ["operation", "target", "reason"]   # all three must appear in the prompt

[approval]
timeout_secs        = 120
on_timeout          = "deny"
allow_remember      = true             # "always allow this operation, with these arguments"
remember_scope      = "operation_and_arguments"   # never a blanket remember
approval_remember_secs   = 86400       # a grant expires; it does not persist forever
max_remembered_approvals = 128         # the ledger is bounded, oldest evicted first
bulk_approval       = false            # approving 40 actions at once is not allowed
```

> The shipped key is `policy.approval_scope`, and the only accepted value is
> `operation_and_arguments`. Setting `tool_and_tier` is a **hard error at load**, not a
> warning: the engine cannot honour it, so pretending to accept it would let a user
> believe they had a narrower grant than they did. Recorded as D-027.

### The tier decision, made concrete

| Action | Tier | Prompt |
|---|---|---|
| Read the UIA tree of Notepad | `read_only` | never |
| Click "Save" in Notepad | `normal` | first use |
| Write `src/main.rs` inside the workspace | `normal` | first use |
| Write to `C:\Windows\...` | `elevated` | always |
| `rm` a file inside the workspace | `normal` (→ Recycle Bin) | first use |
| Permanently delete anything | `elevated` | always |
| `Stop-Process` on a user app | `normal` | first use |
| `Stop-Process` on a system service | `elevated` | always |
| Start/stop a Windows service | `elevated` | always |
| Read a stored credential | `elevated` | always |
| `format`, hive load, disable Defender, modify UAC | — | **auto-denied, no prompt** |

---

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `JEAN_CONFIG_DIR` | `%APPDATA%\JEAN` | Override the config root |
| `JEAN_DATA_DIR` | `%LOCALAPPDATA%\JEAN` | Override the data root |
| `JEAN_LOG_LEVEL` | from config | Override log level (debugging) |
| `JEAN_PIPE_NAME` | from config | Override the IPC pipe name |
| `JEAN_MODEL_API_KEY` | — | The bearer token, named by `model.api_key_env` — the *name* is config, the value is here and nowhere else. Read at request time, never written to a record, log or audit line, and scrubbed out of provider error text (D-037). Required for a non-loopback endpoint, optional on loopback |
| `JEAN_MODEL_PROVIDER` | from config | Override `model.provider`. `none` \| `ollama` \| `openai_compatible`; any other value is a load error |
| `JEAN_MODEL` | from config | Override `model.model` |
| `JEAN_WORKSPACE_ROOT` | from config | Substituted into tool/MCP args |
| `JEAN_NO_VOICE` | unset | Force-disable the voice worker regardless of config |
| `JEAN_NO_VISION` | unset | Force-disable the vision worker regardless of config |
| `JEAN_ELEVATED_CHILD` | unset | Set internally by the elevation helper; **never honoured from an external caller** |

> **Rule:** a secret is only ever named in config (`api_key_env = "JEAN_MODEL_API_KEY"`) and only ever read from the environment or the OS credential store. `cargo-deny` cannot catch a hardcoded key — code review must.

---

## Settings that are user-toggled at runtime (SQLite `preferences`)

These live in the DB, not in TOML, because they change during a session and must survive a restart without a file rewrite:

| Key | Type | Default |
|---|---|---|
| `voice.enabled` | bool | `false` |
| `voice.mode` | enum | `push_to_talk` |
| `vision.enabled` | bool | `false` |
| `ui.theme` | enum | `system` |
| `ui.show_task_board` | bool | `true` |
| `policy.default_tier` | enum | `normal` |
| `policy.remembered_approvals` | list | `[]` |
| `model.provider` | enum | `none` |
| `model.base_url` | http URL | `http://127.0.0.1:11434` |
| `model.model` | string | `""` |
| `workspace.root` | path | `""` |

Runtime changes to `policy.remembered_approvals` are written to the **audit log as well as** the DB — an approval grant is a security event.

---

## Performance budget as configuration, not aspiration

These are enforced in CI, not merely documented (see `02-TOOLCHAIN.md` §2):

| Metric | Budget | Measured how |
|---|---|---|
| Idle core RSS | **< 50 MB** | `Get-Process` working set, core **plus** children |
| Idle CPU | **≈ 0%** | Performance Monitor over a 60 s idle window |
| Startup → command-ready | **< 2 s** | timestamp from `main()` to IPC listener accepting |
| Core binary size | **< 15 MB** release | `cargo bloat` |
| Model RAM while `[voice] enabled = false` | **0** | assert no worker process exists |
| Capture frequency while `[vision] continuous_capture = false` | **0/s** | assert no capture timer is armed |
