//! `jean` — the command line.
//!
//! Phase 0 shipped four commands; Phase 1's engine layer adds the surface that reaches
//! it. The implementations of the Phase-1 commands live in [`phase1`], and every one of
//! them goes through `jean_core::intent::execute` — there is no command that starts a
//! process without going past the permission engine first.
//!
//! ```text
//! jean run <tool> [args…]     run a registered tool under supervision
//! jean tasks                  list tasks
//! jean task <id>              inspect one task, its runs and its events
//! jean cancel <id>            stop a task that is running right now
//!
//! jean apps <query>           what would `jean launch <query>` offer? (read-only)
//! jean launch <app> [args…]   start a discovered application and prove a window opened
//! jean exec <tool> [args…]    run a registered tool, through the intent layer
//! jean ps […]                 read the process table (touches nothing)
//! jean find <root> <pattern>  locate names under one root (reads nothing's contents)
//!
//! jean health [--now]         is each registered tool still where the registry says?
//! jean approvals […]          the ledger of remembered approvals
//! jean notify <status|register> [--send TITLE BODY]
//! jean status [<id>]          the compact task view (read-only)
//!
//! jean desktop windows        which windows an operator would call windows, and who owns them
//! jean desktop inspect …      what one window exposes through UI Automation
//! jean desktop diagnose …     what each desktop operation can actually do, and why the rest cannot
//! jean desktop act …          one accessible-pattern action, then the read-back that proves it
//! ```
//!
//! ## Exit statuses
//!
//! `jean run` distinguishes four outcomes, because a script driving J.E.A.N. needs
//! to tell them apart, and the Phase-1 intents extend that table by one code:
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | The child completed **and** its declared verification passed |
//! | 3 | The child completed, but nothing proves the goal was met |
//! | 4 | The task needs a human to look at it |
//! | 5 | Refused by policy — by configuration, by the operator, or by a timeout |
//! | 130 | The task was cancelled |
//! | 1 | Anything else, including a nonzero exit code and J.E.A.N.'s own errors |
//!
//! Code 3 is the reason the table exists. A zero exit code from a child is not
//! evidence that the user's request was satisfied, and `jean run` refuses to
//! pretend otherwise.
//!
//! Code 5 is `jean_core::intent::IntentReport::exit_status`, and it is separate from 1 on
//! purpose: "the command failed" and "J.E.A.N. would not run it" look identical in a log
//! and need opposite fixes. When it is returned, the reason is printed on stderr exactly
//! as `jean_policy::DenyReason` words it — no paraphrase.
//!
//! `jean desktop` splits 4 and 5 further, because the phase has a state neither table named. An
//! accessibility provider that stops answering *after* accepting a press leaves a click whose delivery
//! nobody can confirm; that outcome is 4, and it is not a failure a script is free to retry. Code 5 is
//! only a policy or prompt refusal — a `[desktop]` switch that is off is an error, and an error still
//! exits 1, the same way `[browser] enabled = false` does. See `phase4` for the full table.
//!
//! ## Which commands leave a record
//!
//! `jean run` and `jean exec` supervise a child; `jean ps` and `jean find` are intents with
//! verifiable outcomes, so the engine records them like any other act. `jean apps`,
//! `jean health`, `jean approvals list` and `jean status` read: they create no task row,
//! write no audit line, and prepare no directory. `jean desktop windows|inspect|diagnose` read in
//! that same sense — and `jean desktop act|input|capture` are acts, so they open a task row, ask
//! the policy, audit the answer, and record the verdict against what the desktop said afterwards.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use jean_core::ipc::{cancel_pipe_name, request_cancel};
use jean_core::runner::cancel_timeout;
use jean_core::{Context, RunReport, RunRequest, TaskState};
use jean_protocol::ipc::validate_task_id;
use jean_store::{format_ms, EventRecord, TaskFilter, ToolRunRecord};
use jean_tools::SortKey;

mod memory;
mod owner_preferences;
mod phase1;
mod phase2;
mod phase3;
mod phase4;
mod phase5;
mod spoken_reply;

#[derive(Parser)]
#[command(
    name = "jean",
    version,
    about = "J.E.A.N. — run registered tools under supervision and keep an honest record of what happened.",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Speak UTF-8 text from stdin using Windows Speech.
    Speak {
        /// Condense a long task result before speaking it.
        #[arg(long)]
        task_result: bool,
        /// Installed Windows voice name.
        #[arg(long)]
        voice: Option<String>,
        /// Speaking rate from -10 to 10.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-10..=10))]
        rate: i32,
    },
    /// Search recorded history without launching a model or voice worker.
    History {
        #[command(subcommand)]
        command: memory::History,
    },
    /// Manage explicitly saved facts (global only unless --scope is selected).
    Memory {
        #[command(subcommand)]
        command: memory::Memory,
    },
    /// Manage the four supported runtime preferences.
    Preferences {
        #[command(subcommand)]
        command: memory::Preferences,
    },
    /// Manage inspectable workspace-scoped learned preferences.
    Preference {
        #[command(subcommand)]
        command: owner_preferences::Command,
    },
    /// Run the owner-operated control session with independent voice and task workers.
    ///
    /// JSON-lines on stdin and stdout by default; `--tty` replaces that with typed
    /// commands, which is how an operator interrupts playback from the keyboard.
    Interact {
        /// Read commands instead of JSON frames, and answer in lines instead of frames.
        #[arg(long)]
        tty: bool,
    },
    /// Have a read-only conversation turn with the configured model. This never runs a task.
    Chat {
        /// The operator's message.
        message: String,
        /// Prior turns from the current conversation, serialized as readable text.
        #[arg(long, default_value = "")]
        context: String,
    },
    /// Audio endpoints, voice diagnostics and hotkey push-to-talk.
    Voice {
        #[command(subcommand)]
        action: phase5::VoiceAction,
    },
    /// Run a tool from the registry.
    Run {
        /// A tool id from tools.toml, not a program name.
        tool: String,

        /// Arguments for the tool, passed through verbatim.
        ///
        /// `allow_hyphen_values` is what makes `jean run powershell.echo
        /// -NoProfile -Command "…"` work: without it, clap would try to parse
        /// `-NoProfile` as one of its own flags.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,

        /// Free text describing what you actually want, for the task list.
        #[arg(long)]
        goal: Option<String>,
    },

    /// List tasks.
    Tasks {
        /// Only tasks in this state.
        #[arg(long, value_parser = parse_state)]
        state: Option<TaskState>,

        /// Only tasks that have not reached a final state.
        #[arg(long, conflicts_with = "state")]
        in_flight: bool,

        /// How many rows to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Inspect one task in full.
    Task {
        /// The task id, as printed by `jean tasks`.
        id: String,

        /// Also print the event log.
        #[arg(long)]
        events: bool,

        /// Also print the head of the captured stdout and stderr.
        #[arg(long)]
        output: bool,

        /// How many bytes of captured output to print with `--output`.
        #[arg(long, default_value_t = 2048, requires = "output")]
        head: usize,
    },

    /// Ask a running task to stop.
    Cancel {
        /// The task id, as printed by `jean tasks`.
        id: String,

        /// How long to wait for the runner to answer.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },

    /// List what installed-application discovery would offer for a name.
    ///
    /// Read-only: it resolves names and reports them. Nothing is launched, no task row is
    /// created and no audit line is written.
    Apps {
        /// The name as you would type it for `jean launch`.
        query: String,
    },

    /// Start a discovered application, then prove a window an operator can see belongs to
    /// the process that was started.
    ///
    /// Put this command's own flags before the application name or its arguments: after
    /// the first argument value, everything is passed through to the application verbatim.
    Launch {
        /// The application name, resolved through `tools.discovery` — never a path you
        /// typed.
        app: String,

        /// Arguments for the application, passed through verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,

        /// Which candidate to launch, 1-based, after `jean apps` or a refusal showed you
        /// the list.
        #[arg(long)]
        pick: Option<usize>,

        /// How long to wait for a window, in seconds. The engine raises anything below one
        /// second to one second, because a zero budget cannot answer the question.
        #[arg(long, default_value_t = phase1::DEFAULT_WINDOW_TIMEOUT_SECS)]
        window_timeout: u64,

        /// Decline the window check. The verdict then says completion is asserted, not
        /// proven — this removes the only evidence the intent can gather.
        #[arg(long)]
        no_window: bool,

        /// Reserved for Phase 2. Accepted, reported, and not acted on: it does not bypass
        /// `policy.require_approval`.
        #[arg(long)]
        yes: bool,
    },

    /// Run a registered tool through the intent layer, so the decision and the audit line
    /// are on the way there.
    ///
    /// As with `jean run`, this command's own flags come before the tool's arguments.
    Exec {
        /// A tool id from tools.toml, not a program name.
        tool: String,

        /// Arguments for the tool, passed through verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,

        /// Free text describing what you actually want, for the task list.
        #[arg(long)]
        goal: Option<String>,

        /// Ask for the pseudo console, so the child can read keystrokes. The registry still
        /// wins when it says otherwise, and a refusal is printed as a note.
        #[arg(long)]
        interactive: bool,

        /// Reserved for Phase 2. Accepted, reported, and not acted on.
        #[arg(long)]
        yes: bool,
    },

    /// Read the process table. Nothing is started, stopped or signalled.
    Ps {
        /// One process by id.
        #[arg(long, conflicts_with_all = ["name", "this_process", "list"])]
        pid: Option<u32>,

        /// Every process whose image name matches.
        #[arg(long, conflicts_with_all = ["pid", "this_process", "list"])]
        name: Option<String>,

        /// Inspect this `jean` process, which demonstrates the sampling semantics without
        /// depending on what else is running.
        #[arg(long = "self", conflicts_with_all = ["pid", "name", "list"])]
        this_process: bool,

        /// The bounded top-N listing — what a bare `jean ps` already is.
        #[arg(long, conflicts_with_all = ["pid", "name", "this_process"])]
        list: bool,

        /// Which column orders the listing.
        #[arg(long, value_parser = parse_sort)]
        sort: Option<SortKey>,

        /// How many rows the listing may carry.
        #[arg(long)]
        limit: Option<usize>,

        /// The sampling window, in milliseconds. CPU percentages are of ONE core across it.
        #[arg(long)]
        sample_ms: Option<u64>,

        /// Reserved for Phase 2. Accepted, reported, and not acted on.
        #[arg(long)]
        yes: bool,
    },

    /// Locate file names under one explicit root. Contents are never read.
    Find {
        /// The one directory the walk is authorised to enter. A symlink or junction below
        /// it is reported and not followed.
        root: PathBuf,

        /// A name pattern: `*.rs`, `Cargo.toml`, or a substring, matched case-insensitively
        /// unless `search.ignore_case` says otherwise.
        pattern: String,

        /// A further cut on the reported hits. Never above `search.max_results`, which is
        /// the ceiling your configuration chose; going above it is capped and says so.
        #[arg(long)]
        max_results: Option<usize>,

        /// Reserved for Phase 2. Accepted, reported, and not acted on.
        #[arg(long)]
        yes: bool,
    },

    /// Look up every registered tool's executable and record whether it is still there.
    ///
    /// A probe is a path lookup. It never runs the tool it is checking.
    Health {
        /// Re-resolve every tool, ignoring how fresh the recorded answer is.
        #[arg(long)]
        now: bool,
    },

    /// The ledger of approvals you agreed to remember.
    ///
    /// With no verb, `list`. Removing a grant writes to the approvals table and nothing
    /// else: no task row, no audit line.
    Approvals {
        #[command(subcommand)]
        action: Option<ApprovalsCommand>,
    },

    /// The notification identity in `HKCU`, and a display-only toast.
    Notify {
        #[command(subcommand)]
        action: NotifyAction,
    },

    /// The compact task view. Read-only.
    ///
    /// With no id, one row per task. With an id, the recorded shape of that intent's
    /// report: state, verdict, decision, notes, and what the child did if there was one.
    Status {
        /// The task id, as printed by `jean tasks`.
        id: Option<String>,
    },

    /// Phase 2. Ask the configured model for a plan, validate it, and stop.
    ///
    /// Read-only: no task row, no plan row, no audit line, nothing executed. This is the door that
    /// answers "what would you do?" without finding out by doing it.
    Plan {
        /// The outcome you want, in your own words.
        goal: String,

        /// Print the validated plan as the JSON that would be stored.
        #[arg(long)]
        json: bool,
    },

    /// Phase 2. Plan, then run each step through the intent pipeline.
    ///
    /// Every step goes through the same policy decision, audit trail and process supervision as the
    /// matching `jean exec`/`launch`/`ps`/`find` command, so a plan cannot do anything the equivalent
    /// typed commands would not.
    ///
    /// Typing this command is itself the authorisation for the run: a step whose capability is on
    /// `policy.require_approval` is covered once, for the plan validated from your goal, at the scope
    /// each step resolves to, and expires when the run ends. It is not written to the approval ledger,
    /// so the next run is asked again, and it reaches nothing that `policy.auto_deny` or the
    /// elevation boundary refuses — those are decided before any grant is consulted.
    Do {
        /// The outcome you want, in your own words.
        goal: String,

        /// Accepted and reported, and changes nothing: the authority for a run comes from you typing
        /// the command, never from a flag. Use `--prompt-each` to alter how you are asked.
        #[arg(long)]
        yes: bool,

        /// Ask about every step separately, as Phase 1 did, rather than once for the run.
        ///
        /// The refusal path is identical either way — `policy.auto_deny` and the elevation boundary
        /// never depend on being asked — this only decides whether a person is consulted per step.
        #[arg(long)]
        prompt_each: bool,
    },

    /// Phase 2. What model provider this build would ask, and the limits around it.
    ///
    /// With no verb, `status`. Reports configuration only — it never prints a credential value, and
    /// `status` opens no connection.
    Model {
        #[command(subcommand)]
        action: Option<ModelCommand>,
    },

    /// Phase 3. Hand a goal to an external coding agent, then check the result yourself.
    ///
    /// The whole workflow in one command: J.E.A.N. resolves the workspace and the verification command
    /// from `[delegation]` **before** the agent exists, runs the check to record a baseline, starts the
    /// worker, records its progress and its permission asks through the same policy engine a typed
    /// command uses, lists what the worker changed, and runs the same check again. What the agent says
    /// about its own success is reported next to that verdict and never replaces it — an agent that
    /// ends its turn politely with no passing check exits 3, not 0.
    ///
    /// The delegation is itself gated: starting an agent with a working directory is an operation, so
    /// `auto_deny`, the elevation boundary and your own approval all apply before a process exists.
    #[command(alias = "goal")]
    Delegate {
        /// The outcome you want, in your own words. Passed to the worker verbatim.
        goal: String,

        /// Workspace-scoped quality profile for frontend goals.
        #[arg(long, value_parser = ["quick-prototype", "polished-ui"])]
        profile: Option<String>,

        /// Task-specific quality instruction; takes precedence over profile guidance.
        #[arg(long)]
        instruction: Option<String>,

        /// The directory the worker works in. Defaults to `[workspace] root`.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Which `[delegation.workers]` entry to use. Defaults to `default_worker`.
        #[arg(long)]
        worker: Option<String>,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Restore and inspect a durable owner-workflow checkpoint.
    Checkpoint {
        /// Task id printed by `jean delegate` or `jean goal`.
        id: String,
    },

    /// Reconcile and resume one interrupted owner handoff.
    Resume {
        /// Original task id printed by `jean goal`.
        id: String,
        /// Confirm sign-in or network remediation for a checkpoint with that blocker.
        #[arg(long)]
        condition_cleared: bool,
    },

    /// Phase 3. Which external agents are configured, and what this build can speak to them.
    ///
    /// Read-only: it resolves each worker's command against `PATH`, names the protocol its `kind`
    /// implies, and reports the bounds a session will run under. It never starts a worker, never
    /// authenticates to one, and never contacts a service.
    Agent,

    /// Phase 3. MCP, both ways, over stdio.
    ///
    /// With no verb, `status`.
    Mcp {
        #[command(subcommand)]
        action: Option<McpCommand>,
    },

    /// Phase 3. A browser J.E.A.N. starts, owns and closes.
    ///
    /// With no verb, `status`: which browser would be started, and under what bounds. Nothing in this
    /// command connects to a browser somebody else opened, and nothing downloads one.
    Browser {
        #[command(subcommand)]
        action: Option<BrowserCommand>,
    },

    /// Phase 4. The operator's own desktop: its windows, their accessible controls, their pixels.
    ///
    /// With no verb, `status`. `windows`, `inspect` and `diagnose` read and run with `[desktop] enabled =
    /// false`; `act`, `input` and `capture` are the verbs that reach into another application, and every
    /// one of them goes through the policy engine, the approvals ledger, the task row and the audit trail
    /// exactly as a typed command does. Nothing here attaches to a process it did not identify, and an
    /// action whose result could not be seen is recorded as uncertain and never sent again.
    Desktop {
        #[command(subcommand)]
        action: Option<DesktopCommand>,
    },
}

/// `jean mcp …`
#[derive(Subcommand)]
enum McpCommand {
    /// Report the configured servers, the bounds, and what `expose` would publish. Starts nothing.
    Status,

    /// Start one configured server and list what it offers.
    Tools {
        /// A `[[mcp.servers]]` name.
        server: String,
    },

    /// Call one allowlisted tool on one configured server.
    Call {
        /// A `[[mcp.servers]]` name.
        server: String,

        /// A tool name from that server's `allow_tools`.
        tool: String,

        /// The arguments as one JSON object, e.g. '{"path":"README.md"}'.
        #[arg(long)]
        args: Option<String>,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Act as the MCP server on this process's own standard input and output.
    ///
    /// Publishes `[mcp] expose` and runs each call through the same policy, approval, audit and
    /// verification path as `jean run`. No port, no listener, no personal browser: the only client is
    /// the one already attached to these pipes. Human output goes to stderr, because stdout is the
    /// protocol.
    Serve,
}

/// `jean browser …`
#[derive(Subcommand)]
enum BrowserCommand {
    /// Report whether the subsystem is enabled, which browser would be started, and its bounds.
    ///
    /// Starts nothing, and never connects to a running browser.
    Status,

    /// Open one page, read it, optionally act on it, and check what the page says afterwards.
    ///
    /// The browser is a child J.E.A.N. starts, in its own empty profile, with a debugging endpoint on
    /// loopback that this process created; it is closed on every path out, including a panic.
    Run {
        /// The page to open. Must be covered by `[browser] allowed_origins`.
        url: String,

        /// Read this element's text before doing anything to the page.
        #[arg(long)]
        inspect: Option<String>,

        /// Focus and type into an element: `--type "input#name=hello"`.
        #[arg(long)]
        r#type: Option<String>,

        /// A real pointer click on an element: `--click "button#send"`.
        #[arg(long)]
        click: Option<String>,

        /// The check, stated before the interaction: `--expect "#status=clicked"`. Without it the run
        /// reports `unverifiable`, because nothing was declared that could have failed.
        #[arg(long)]
        expect: Option<String>,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// `jean desktop …`
///
/// The four read verbs work with `[desktop] enabled = false`, and say so when they do: a window that
/// cannot be worked on still has to be diagnosed, and the diagnosis is what the operator decides from.
#[derive(Subcommand)]
enum DesktopCommand {
    /// What this process can see: the bounds, the DPI awareness actually in force, the integrity level,
    /// the session. Sets up nothing and touches nothing.
    Status,

    /// Every top-level window an operator would call a window, with its process, integrity and DPI.
    ///
    /// The same definition of "a window" `jean launch` verifies against, so the two commands cannot
    /// disagree about what is on the screen.
    Windows {
        /// How many windows to print.
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// The accessible controls of one window, and the reason if there are none.
    ///
    /// An empty tree is reported as an absence of evidence, never as a protection: that distinction is
    /// R-16, and it is the difference between "this application has not enabled its accessibility tree"
    /// and "this application is refusing us", which need opposite responses.
    Inspect {
        #[command(flatten)]
        window: WindowArgs,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// What one window — or every window — was actually observed to support, and why the rest failed.
    ///
    /// Discovery, accessibility, patterns, capture, input and verification are tracked separately, because
    /// a window whose capture is excluded can still be driven through its controls, and reporting that as
    /// one boolean is how a protected application ends up disabling unrelated work.
    Diagnose {
        #[command(flatten)]
        window: WindowArgs,

        /// Sweep every window instead of one. Costs one bounded probe per window.
        #[arg(long)]
        all: bool,

        /// How many windows to probe with `--all`.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Perform one accessible-pattern action, then read back what the desktop shows.
    ///
    /// The pattern call is the semantic path (D-004): invoking a button says *press this button*, which is
    /// a different claim from a click at a coordinate. The window, the control and the witness are all
    /// re-resolved immediately before the call, so a stale plan cannot act on a control that moved.
    Act {
        #[command(flatten)]
        window: WindowArgs,

        /// The control's `AutomationId` — a dialog control id on a standard Win32 control.
        #[arg(long, conflicts_with = "control")]
        automation_id: Option<String>,

        /// The control's accessible name.
        #[arg(long, conflicts_with = "automation_id")]
        control: Option<String>,

        #[arg(long)]
        invoke: bool,

        /// Set the control's value. The text goes to the control, not to the log or the audit line.
        #[arg(long, value_name = "TEXT")]
        set_value: Option<String>,

        #[arg(long)]
        toggle: bool,

        #[arg(long)]
        select: bool,

        #[arg(long)]
        expand: bool,

        #[arg(long)]
        collapse: bool,

        #[arg(long)]
        scroll_into_view: bool,

        /// Give the control the keyboard focus. The sanctioned way to change what typing would reach.
        #[arg(long)]
        set_focus: bool,

        /// A second control whose state proves the action — usually the one that is not the button.
        #[arg(long)]
        witness_id: Option<String>,

        /// The same, named by accessible name.
        #[arg(long)]
        witness_name: Option<String>,

        /// The check, stated before the action: `--expect "name=clicked:1"`, or just the value.
        ///
        /// Without it the run reports `unverifiable` and prints what changed. The change is evidence of
        /// the action; the goal is only proven against something you said would prove it.
        #[arg(long)]
        expect: Option<String>,

        /// Capture the window afterwards, as a second witness.
        #[arg(long)]
        capture: bool,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Inject one keyboard or mouse input, then read back. Needs `[desktop] allow_synthetic_input`.
    ///
    /// The fallback, deliberately behind its own switch and never taken because a capture came back blank.
    /// Before anything is sent the target is re-checked three ways: the handle is still a window, the
    /// window named is the window in front, and the point is still inside its current rectangle. Focus is
    /// not stolen to make a check pass — if the window is not in front, this refuses and says so.
    Input {
        #[command(flatten)]
        window: WindowArgs,

        /// Aim at a physical coordinate instead of a control: `--at 1500,420`.
        #[arg(long, value_name = "X,Y", conflicts_with = "control")]
        at: Option<String>,

        /// Aim at the centre of the control with this `AutomationId`.
        #[arg(long)]
        automation_id: Option<String>,

        /// Aim at the centre of the control with this accessible name.
        #[arg(long)]
        control: Option<String>,

        #[arg(long)]
        click: bool,

        #[arg(long)]
        right_click: bool,

        #[arg(long)]
        double_click: bool,

        /// Move the cursor without pressing anything: the one input that changes nothing outside.
        #[arg(long = "move")]
        move_cursor: bool,

        /// Scroll wheel ticks; negative scrolls up.
        #[arg(long)]
        scroll: Option<i32>,

        /// Type text, as Unicode events rather than key presses — layout-independent, and it cannot
        /// express Escape or Tab, which is what `--key` is for.
        #[arg(long)]
        r#type: Option<String>,

        /// A named key (`Enter`, `Escape`, `Tab`, `Up`, …) or a virtual-key code such as `0x1e`.
        #[arg(long)]
        key: Option<String>,

        /// A modifier to hold with `--key`. Repeat it: `--modifier ctrl --modifier shift`.
        #[arg(long)]
        modifiers: Vec<String>,

        #[arg(long)]
        witness_id: Option<String>,

        #[arg(long)]
        witness_name: Option<String>,

        #[arg(long)]
        expect: Option<String>,

        #[arg(long)]
        capture: bool,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// One window's pixels, once, judged by `[desktop] retention`.
    ///
    /// Nothing captures continuously, and a frame's content never enters a log: what is recorded is its
    /// geometry, its hash and whether it says anything. A blank frame is a diagnosis, not a conclusion —
    /// an excluded window and an unexplained black one are different sentences here.
    Capture {
        #[command(flatten)]
        window: WindowArgs,

        /// Write the frame to this path whatever the retention policy says.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Print the observed report as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// The four ways an operator names a window, shared by every `jean desktop` verb.
///
/// All of them are questions asked of the desktop *now*. A handle is re-checked and re-read; a title or
/// image is re-enumerated. Nothing here is a cached coordinate, and an ambiguous answer is a refusal
/// rather than a first match.
#[derive(clap::Args)]
struct WindowArgs {
    /// The window handle, as `jean desktop windows` printed it (`0x…` or decimal).
    #[arg(long, value_parser = phase4::parse_hwnd)]
    hwnd: Option<isize>,

    /// The owning process id; matches a visible window of that process.
    #[arg(long)]
    pid: Option<u32>,

    /// A case-insensitive substring of the window title.
    #[arg(long)]
    title: Option<String>,

    /// A case-insensitive substring of the owning executable, or of the window class.
    #[arg(long)]
    image: Option<String>,
}

impl WindowArgs {
    fn into_selector(self) -> jean_core::desktop::WindowSelector {
        phase4::selector(self.hwnd, self.pid, self.title, self.image)
    }
}

/// `jean model …`
#[derive(Subcommand)]
enum ModelCommand {
    /// Report provider, endpoint, model, bounds and credential presence. Connects to nothing.
    Status,

    /// Send one real request to the endpoint already in configuration.
    ///
    /// The only way this build reaches a provider outside a test: nothing is downloaded, no server
    /// is started, and no account is used. A refusal or a timeout is reported with its code.
    Ping,
}

/// `jean approvals …`
#[derive(Subcommand)]
enum ApprovalsCommand {
    /// List the remembered approvals, and say which have already expired.
    List,

    /// Forget one approval by the id `jean approvals list` printed.
    Forget {
        /// The approval row's id.
        id: i64,
    },

    /// Forget every remembered approval.
    Clear,
}

/// `jean notify …`
#[derive(Subcommand)]
enum NotifyAction {
    /// Report whether the AppUserModelId is registered and what `ui.notifications` says.
    ///
    /// Reads one registry value and writes nothing.
    Status {
        /// Deliver one toast with this title and body after the report.
        #[arg(long, num_args = 2, value_names = ["TITLE", "BODY"])]
        send: Option<Vec<String>>,
    },

    /// Write the `DisplayName` value for the AppUserModelId under `HKCU`.
    ///
    /// This is the only per-user state change the notification path makes: one value, in
    /// `HKCU`, and nothing else — no HKLM, no Start Menu shortcut, no activation handler.
    Register {
        /// Deliver one toast with this title and body after the report.
        #[arg(long, num_args = 2, value_names = ["TITLE", "BODY"])]
        send: Option<Vec<String>>,
    },
}

impl NotifyAction {
    /// The verb, and the raw `--send` value list if it was passed.
    ///
    /// Both verbs carry the flag, because `jean notify status --send …` has to parse: an
    /// option on the parent would have to be typed *before* the verb, which is not the
    /// order anybody reads this command in.
    fn into_parts(self) -> (bool, Option<Vec<String>>) {
        match self {
            NotifyAction::Status { send } => (false, send),
            NotifyAction::Register { send } => (true, send),
        }
    }
}

fn parse_state(text: &str) -> Result<TaskState, String> {
    text.parse::<TaskState>().map_err(|e| {
        format!(
            "{e}. Valid states: {}",
            jean_protocol::state::ALL_STATES
                .iter()
                .map(|state| state.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn parse_sort(text: &str) -> Result<SortKey, String> {
    match text {
        "cpu" => Ok(SortKey::Cpu),
        "memory" => Ok(SortKey::Memory),
        "pid" => Ok(SortKey::Pid),
        other => Err(format!(
            "`{other}` is not a sort key. Valid keys: cpu, memory, pid"
        )),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match dispatch(cli.command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jean: {}", memory::terminal(&e.to_string()));
            // Print the chain, because the interesting part of a Win32 or SQLite
            // failure is usually one level down.
            for cause in error_chain(&e) {
                eprintln!("  caused by: {}", memory::terminal(&cause));
            }
            ExitCode::from(1)
        }
    }
}

/// Every cause below `err`, outermost first.
///
/// `anyhow::Error` has no inherent `source`, and `std::iter::successors` does not
/// fit `Error::source`'s `&Self` signature, so this walks `chain()` and drops its
/// first element — which is the error `main` has already printed.
fn error_chain(err: &anyhow::Error) -> Vec<String> {
    err.chain().skip(1).map(|cause| cause.to_string()).collect()
}

fn dispatch(command: Command) -> anyhow::Result<ExitCode> {
    if let Command::Speak { task_result, voice, rate } = command {
        spoken_reply::speak_from_stdin(task_result, voice, rate)?;
        return Ok(ExitCode::SUCCESS);
    }
    let exe_dir = executable_directory();
    let mut ctx = Context::load(exe_dir.as_deref())?;
    report_warnings(&ctx);

    match command {
        Command::Speak { .. } => unreachable!("speech is handled before loading context"),
        Command::History { command } => memory::history(&mut ctx, command),
        Command::Memory { command } => memory::facts(&mut ctx, command),
        Command::Preferences { command } => memory::preferences(&mut ctx, command),
        Command::Preference { command } => {
            owner_preferences::command(&ctx.paths.data_dir, &ctx.store, command)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Interact { tty } => phase5::interact(&mut ctx, tty),
        Command::Chat { message, context } => phase2::cmd_chat(&ctx, message, context),
        Command::Voice { action } => phase5::voice(&mut ctx, action),
        Command::Run { tool, args, goal } => cmd_run(&mut ctx, tool, args, goal),
        Command::Tasks {
            state,
            in_flight,
            limit,
        } => cmd_tasks(&ctx, state, in_flight, limit),
        Command::Task {
            id,
            events,
            output,
            head,
        } => cmd_task(&ctx, id, events, output, head),
        Command::Cancel { id, timeout_secs } => cmd_cancel(&ctx, id, timeout_secs),

        // Phase 1. The four intents go through `jean_core::intent::execute`, so no command
        // here can act without the decision and the audit line. The rest read.
        Command::Apps { query } => phase1::cmd_apps(&ctx, query),
        Command::Launch {
            app,
            args,
            pick,
            window_timeout,
            no_window,
            yes,
        } => phase1::cmd_launch(
            &mut ctx,
            phase1::LaunchOptions {
                app,
                args,
                pick,
                window_timeout_secs: window_timeout,
                no_window,
                yes,
            },
        ),
        Command::Exec {
            tool,
            args,
            goal,
            interactive,
            yes,
        } => phase1::cmd_exec(
            &mut ctx,
            phase1::ExecOptions {
                tool,
                args,
                goal,
                interactive,
                yes,
            },
        ),
        Command::Ps {
            pid,
            name,
            this_process,
            list,
            sort,
            limit,
            sample_ms,
            yes,
        } => phase1::cmd_ps(
            &mut ctx,
            phase1::PsOptions {
                pid,
                name,
                this_process,
                list,
                sort,
                limit,
                sample_ms,
                yes,
            },
        ),
        Command::Find {
            root,
            pattern,
            max_results,
            yes,
        } => phase1::cmd_find(
            &mut ctx,
            phase1::FindOptions {
                root,
                pattern,
                max_results,
                yes,
            },
        ),
        Command::Health { now } => phase1::cmd_health(&ctx, now),
        Command::Approvals { action } => phase1::cmd_approvals(&ctx, action.into()),
        Command::Notify { action } => {
            let (register, send) = action.into_parts();
            phase1::cmd_notify(
                &ctx,
                phase1::NotifyOptions {
                    register,
                    send: send_title_and_body(send.as_deref())?,
                },
            )
        }
        Command::Status { id } => phase1::cmd_status(&ctx, id),

        // Phase 2. `plan` writes nothing at all; `do` delegates every step to `intent::execute`, so
        // the decision, the audit line and the supervision are Phase 1's, not a second copy.
        Command::Plan { goal, json } => phase2::cmd_plan(&ctx, phase2::PlanOptions { goal, json }),
        Command::Do {
            goal,
            yes,
            prompt_each,
        } => phase2::cmd_do(
            &mut ctx,
            phase2::DoOptions {
                goal,
                yes,
                prompt_each,
            },
        ),
        Command::Model { action } => phase2::cmd_model(&ctx, action.into()),

        // Phase 3. `delegate` runs the worker under the existing policy, audit and supervision;
        // `agent` reads configuration and resolves names, and starts nothing.
        Command::Delegate {
            goal,
            profile,
            instruction,
            workspace,
            worker,
            json,
        } => phase3::cmd_delegate(
            &mut ctx,
            phase3::DelegateOptions {
                goal,
                profile,
                instruction,
                workspace,
                worker,
                json,
            },
        ),
        Command::Checkpoint { id } => {
            let checkpoint = jean_core::owner::read_checkpoint(&ctx.paths.data_dir, &id)?;
            println!("task: {}", checkpoint.task_id);
            println!("goal: {}", checkpoint.goal);
            println!("scope: {}", checkpoint.resolved_scope);
            println!("uncertain effects: {}", checkpoint.uncertain);
            println!("resumable: {}", checkpoint.resumable);
            if let Some(blocker) = checkpoint.blocked_on {
                println!("blocked on: {}", blocker.status());
            }
            println!(
                "resume attempts: {}/{}",
                checkpoint.attempts, checkpoint.max_attempts
            );
            if !checkpoint.remaining_work.is_empty() {
                println!("remaining work: {}", checkpoint.remaining_work);
            }
            for effect in &checkpoint.completed_effects {
                println!(
                    "completed effect: {} ({} bytes, sha256 {})",
                    effect.path, effect.bytes, effect.sha256
                );
            }
            if !checkpoint.eligible_free_workers.is_empty() {
                println!(
                    "eligible free workers: {}",
                    checkpoint.eligible_free_workers.join(", ")
                );
            }
            for result in checkpoint.results {
                println!("result: {result}");
            }
            println!("next action: {}", checkpoint.next_action);
            if checkpoint.uncertain {
                println!(
                    "resume: reconcile task and workspace evidence before retrying; no effect was replayed"
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Resume {
            id,
            condition_cleared,
        } => {
            let mut prompter = jean_policy::ConsolePrompt::new(&ctx.config.policy);
            let resumed = if condition_cleared {
                jean_core::delegation::resume_checkpoint_after_condition_cleared(
                    &mut ctx,
                    &id,
                    &mut prompter,
                )?
            } else {
                jean_core::delegation::resume_checkpoint(&mut ctx, &id, &mut prompter)?
            };
            match resumed {
                jean_core::delegation::ResumeOutcome::Completed(report) => {
                    for line in report.lines() {
                        println!("{line}");
                    }
                    Ok(ExitCode::from(report.result.exit_status() as u16 as u8))
                }
                jean_core::delegation::ResumeOutcome::NeedsAttention { task_id, reason } => {
                    println!("task: {task_id}");
                    println!("result: needs_attention");
                    println!("reason: {reason}");
                    Ok(ExitCode::from(4))
                }
                jean_core::delegation::ResumeOutcome::Blocked {
                    task_id,
                    status,
                    reason,
                } => {
                    println!("task: {task_id}");
                    println!("result: {status}");
                    println!("reason: {reason}");
                    Ok(ExitCode::from(4))
                }
            }
        }
        Command::Agent => phase3::cmd_agent(&ctx.config.delegation),
        Command::Mcp { action } => phase3::cmd_mcp(&mut ctx, action.into()),
        Command::Browser { action } => phase3::cmd_browser(&mut ctx, action.into()),

        // Phase 4. The reads are outside `[desktop] enabled` and outside the task record; the acts go
        // through the same policy, approval, audit and verification path as every other act in this build.
        Command::Desktop { action } => phase4::cmd_desktop(&mut ctx, phase4::translate(action)?),
    }
}

/// `jean mcp` with no verb means `status`.
impl From<Option<McpCommand>> for phase3::McpAction {
    fn from(action: Option<McpCommand>) -> Self {
        match action {
            None | Some(McpCommand::Status) => phase3::McpAction::Status,
            Some(McpCommand::Tools { server }) => phase3::McpAction::Tools { server },
            Some(McpCommand::Call {
                server,
                tool,
                args,
                json,
            }) => phase3::McpAction::Call {
                server,
                tool,
                arguments: args,
                json,
            },
            Some(McpCommand::Serve) => phase3::McpAction::Serve,
        }
    }
}

/// `jean browser` with no verb means `status`.
impl From<Option<BrowserCommand>> for phase3::BrowserAction {
    fn from(action: Option<BrowserCommand>) -> Self {
        match action {
            None | Some(BrowserCommand::Status) => phase3::BrowserAction::Status,
            Some(BrowserCommand::Run {
                url,
                inspect,
                r#type,
                click,
                expect,
                json,
            }) => phase3::BrowserAction::Run {
                url,
                inspect,
                r#type,
                click,
                expect,
                json,
            },
        }
    }
}

/// `jean model` with no verb means `status`.
impl From<Option<ModelCommand>> for phase2::ModelAction {
    fn from(action: Option<ModelCommand>) -> Self {
        match action {
            None | Some(ModelCommand::Status) => phase2::ModelAction::Status,
            Some(ModelCommand::Ping) => phase2::ModelAction::Ping,
        }
    }
}

/// `jean approvals` with no verb means `list`.
impl From<Option<ApprovalsCommand>> for phase1::ApprovalAction {
    fn from(action: Option<ApprovalsCommand>) -> Self {
        match action {
            None | Some(ApprovalsCommand::List) => phase1::ApprovalAction::List,
            Some(ApprovalsCommand::Forget { id }) => phase1::ApprovalAction::Forget { id },
            Some(ApprovalsCommand::Clear) => phase1::ApprovalAction::Clear,
        }
    }
}

/// The two `--send` values, checked where a wrong count is a stated error rather than an
/// index that panics.
fn send_title_and_body(values: Option<&[String]>) -> anyhow::Result<Option<(String, String)>> {
    let Some(values) = values else {
        return Ok(None);
    };
    // `num_args = 2` already makes a wrong count a clap error on a normal command line.
    // This is the belt: a mis-shaped value list is reported, never indexed.
    let [title, body] = values else {
        return Err(anyhow::anyhow!(
            "--send takes exactly two values (TITLE BODY); {} were given",
            values.len()
        ));
    };
    Ok(Some((title.clone(), body.clone())))
}

fn executable_directory() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn report_warnings(ctx: &Context) {
    for warning in &ctx.warnings {
        eprintln!("jean: warning: {warning}");
    }
}

// ── run ─────────────────────────────────────────────────────────────────────

fn cmd_run(
    ctx: &mut Context,
    tool: String,
    args: Vec<String>,
    goal: Option<String>,
) -> anyhow::Result<ExitCode> {
    // This command writes, so the directories and the log file are created here
    // and only here.
    ctx.prepare_for_writing()?;

    let report = jean_core::run(
        ctx,
        RunRequest {
            tool_id: tool,
            args,
            goal,
            // `jean run` keeps Phase 0's meaning: the registry decides whether the
            // child gets a console. `jean exec` is the Phase-1 command that can ask.
            interactive: None,
            // Not an intent: this is the Phase-0 door, and the task record says so by
            // carrying no `intent` at all.
            intent: None,
        },
    )?;

    if let Some(summary) = report.reconciled.summary() {
        eprintln!("jean: {summary}");
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    print_run_report(&mut out, &report)?;

    Ok(ExitCode::from(report.exit_status() as u8))
}

/// The body `jean run` prints, and the body `jean exec` prints inside its intent report.
///
/// One function, two callers: the two commands must not disagree about what "the child
/// completed and its declared verification passed" looks like on screen.
fn print_run_report(out: &mut dyn Write, report: &RunReport) -> anyhow::Result<()> {
    writeln!(out, "task        {}", report.task_id)?;
    writeln!(out, "tool        {}", report.tool_id)?;
    writeln!(out, "executable  {}", report.executable.display())?;
    writeln!(
        out,
        "duration    {:.3}s",
        report.outcome.duration.as_secs_f64()
    )?;
    writeln!(out, "termination {}", report.outcome.termination)?;
    match report.outcome.exit_code {
        Some(code) => writeln!(out, "exit code   {code}")?,
        None => writeln!(out, "exit code   (none recorded)")?,
    }
    writeln!(
        out,
        "stdout      {} bytes{}{}",
        report.outcome.stdout.bytes,
        if report.outcome.stdout.truncated {
            " (truncated)"
        } else {
            ""
        },
        report
            .outcome
            .stdout
            .path
            .as_ref()
            .map(|p| format!("  {p}"))
            .unwrap_or_default()
    )?;
    writeln!(
        out,
        "stderr      {} bytes{}{}",
        report.outcome.stderr.bytes,
        if report.outcome.stderr.truncated {
            " (truncated)"
        } else {
            ""
        },
        report
            .outcome
            .stderr
            .path
            .as_ref()
            .map(|p| format!("  {p}"))
            .unwrap_or_default()
    )?;
    writeln!(out, "state       {}", report.state)?;
    writeln!(out, "verdict     {}", report.verdict.label())?;
    writeln!(out, "            {}", report.verdict.detail())?;

    if !report.notes.is_empty() {
        writeln!(out, "notes")?;
        for note in &report.notes {
            writeln!(out, "  - {note}")?;
        }
    }

    // The single most important line in the output. Completion and proof are
    // reported separately everywhere else, and this says which one applies.
    if report.is_success() {
        writeln!(out, "\nresult      completed and verified")?;
    } else {
        writeln!(
            out,
            "\nresult      command completed; the requested outcome is NOT proven"
        )?;
    }
    Ok(())
}

// ── tasks ───────────────────────────────────────────────────────────────────

fn cmd_tasks(
    ctx: &Context,
    state: Option<TaskState>,
    in_flight: bool,
    limit: usize,
) -> anyhow::Result<ExitCode> {
    let filter = if in_flight {
        TaskFilter::InFlight
    } else {
        match state {
            Some(s) => TaskFilter::State(s),
            None => TaskFilter::All,
        }
    };

    let tasks = ctx.store.list_tasks(filter, limit)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if tasks.is_empty() {
        writeln!(out, "no tasks match")?;
        return Ok(ExitCode::SUCCESS);
    }

    // The header must be padded to exactly the widths the rows below use, so the
    // column specs live in the format string rather than in a spelled-out literal
    // that would silently drift from the row layout.
    #[allow(clippy::write_literal)]
    writeln!(
        out,
        "{:<26} {:<16} {:<24} {}",
        "ID", "STATE", "TOOL", "CREATED"
    )?;
    for task in &tasks {
        writeln!(
            out,
            "{:<26} {:<16} {:<24} {}",
            task.id,
            task.state.as_str(),
            task.tool_id.as_deref().unwrap_or("-"),
            format_ms(task.created_at_ms)
        )?;
    }
    writeln!(out, "\n{} task(s)", tasks.len())?;
    writeln!(out, "use `jean task <id>` for the full record")?;
    Ok(ExitCode::SUCCESS)
}

// ── task ────────────────────────────────────────────────────────────────────

/// The recorded request, rendered one field per line.
///
/// A request carries the child's whole allowlisted environment, which is a several
/// kilobyte single line in raw JSON — unreadable in a terminal, and the record is
/// already in the database for anyone who needs the values. The env is therefore
/// shown as the *names* that were passed, which is the question an operator asking
/// "what did this run get?" usually means, plus the count.
fn render_request_json(raw: &str) -> String {
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str::<serde_json::Value>(raw)
    else {
        // Not JSON we can shape: print exactly what was stored rather than
        // inventing a rendering that hides part of it.
        return raw.to_owned();
    };
    let mut lines = Vec::new();
    for key in fields.keys() {
        if key == "env" {
            continue;
        }
        let value = &fields[key];
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        lines.push(format!("{key} = {text}"));
    }
    if let Some(serde_json::Value::Object(env)) = fields.get("env") {
        let mut names: Vec<&str> = env.keys().map(String::as_str).collect();
        names.sort_unstable();
        lines.push(format!(
            "env = {} variable(s), values recorded in the database: {}",
            names.len(),
            names.join(", ")
        ));
    }
    // The first line rides on the `request     ` label; the rest are indented to
    // the same column as every other continuation in this output.
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index == 0 {
            out.push_str(line);
        } else {
            out.push_str("\n            ");
            out.push_str(line);
        }
    }
    out
}

fn print_delegation_activity(
    out: &mut impl std::io::Write,
    ctx: &Context,
    id: &str,
) -> anyhow::Result<()> {
    use jean_protocol::EventKind;

    let delegations = ctx.store.delegations_for_task(id)?;
    let Some(delegation) = delegations.last() else {
        return Ok(());
    };
    let model = ctx
        .store
        .latest_worker_detail(&delegation.id, EventKind::WorkerModelConfirmed)?;
    let progress = ctx
        .store
        .latest_worker_detail(&delegation.id, EventKind::WorkerProgress)?;
    let verifier = delegation
        .verification_verdict
        .as_deref()
        .unwrap_or("not recorded");
    writeln!(
        out,
        "worker      {} ({})",
        delegation.worker, delegation.kind
    )?;
    writeln!(
        out,
        "model       {}",
        model.as_deref().unwrap_or("confirmation not recorded")
    )?;
    writeln!(
        out,
        "latest step {}",
        progress
            .as_deref()
            .unwrap_or("no worker progress recorded")
            .escape_default()
    )?;
    writeln!(out, "verifier    {verifier}")?;

    let path = jean_core::owner::checkpoint_path(&ctx.paths.data_dir, id)?;
    match path.try_exists() {
        Ok(false) => writeln!(out, "checkpoint  absent")?,
        Ok(true) => match jean_core::owner::read_checkpoint(&ctx.paths.data_dir, id) {
            Ok(checkpoint) => {
                let state = if checkpoint.uncertain {
                    "uncertain; review required before replay"
                } else if checkpoint.resumed_task_id.is_some() {
                    "resumed"
                } else if checkpoint.blocked_on.is_some() {
                    "blocked"
                } else if checkpoint.resumable {
                    "resumable"
                } else {
                    "recorded"
                };
                writeln!(out, "checkpoint  {state}")?;
                if let Some(resumed) = checkpoint.resumed_task_id {
                    writeln!(out, "resumed as  {resumed}")?;
                }
            }
            Err(_) => writeln!(out, "checkpoint  unreadable")?,
        },
        Err(_) => writeln!(out, "checkpoint  inaccessible")?,
    }
    Ok(())
}

fn cmd_task(
    ctx: &Context,
    id: String,
    show_events: bool,
    show_output: bool,
    head: usize,
) -> anyhow::Result<ExitCode> {
    validate_task_id(&id)?;
    let task = ctx.store.require_task(&id)?;
    let runs = ctx.store.tool_runs(&id)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    writeln!(out, "task        {}", task.id)?;
    writeln!(out, "goal        {}", task.goal)?;
    writeln!(out, "state       {}", task.state.as_str())?;
    writeln!(
        out,
        "tool        {}",
        task.tool_id.as_deref().unwrap_or("-")
    )?;
    writeln!(out, "created     {}", format_ms(task.created_at_ms))?;
    writeln!(out, "updated     {}", format_ms(task.updated_at_ms))?;
    if let Some(started) = task.started_at_ms {
        writeln!(out, "started     {}", format_ms(started))?;
    }
    if let Some(finished) = task.finished_at_ms {
        writeln!(out, "finished    {}", format_ms(finished))?;
        if let Some(started) = task.started_at_ms {
            writeln!(
                out,
                "elapsed     {:.3}s",
                (finished - started) as f64 / 1000.0
            )?;
        }
    }
    match (task.runner_pid, task.runner_created_ms) {
        (Some(pid), Some(created)) => {
            // A pid on its own proves nothing, because Windows recycles them. The
            // recorded creation time is what makes "still alive" a real answer, and
            // it is the same check reconciliation uses.
            let alive = jean_core::reconcile::runner_is_alive(pid, created);
            writeln!(
                out,
                "runner      pid {pid} (started {}) — {}",
                format_ms(created),
                if alive { "alive" } else { "gone" }
            )?;
        }
        (Some(pid), None) => writeln!(out, "runner      pid {pid} (creation time not recorded)")?,
        (None, _) => writeln!(out, "runner      none recorded")?,
    }
    if let Some(outcome) = &task.outcome {
        writeln!(out, "verdict     {outcome}")?;
    }
    if let Some(detail) = &task.outcome_detail {
        writeln!(out, "            {detail}")?;
    }
    writeln!(
        out,
        "request     {}",
        render_request_json(&task.request_json)
    )?;

    print_delegation_activity(&mut out, ctx, &id)?;
    print_plan(&mut out, ctx, &id)?;
    print_runs(&mut out, &runs)?;

    if show_events {
        print_events(&mut out, &ctx.store.events(&id)?)?;
    }

    if show_output {
        for run in &runs {
            print_capture(&mut out, "stdout", run.stdout_path.as_deref(), head)?;
            print_capture(&mut out, "stderr", run.stderr_path.as_deref(), head)?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// The plan a task ran, printed from the lines `jean-core` builds from the rows.
///
/// A Phase-1 task has no plan rows, so nothing here appears for it: the record an operator read
/// before Phase 2 is the record they read after.
fn print_plan(out: &mut impl Write, ctx: &Context, id: &str) -> anyhow::Result<()> {
    let lines = jean_core::plan_runner::plan_lines(ctx, id, &|ms| format_ms(ms))?;
    if lines.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn print_runs(out: &mut impl Write, runs: &[ToolRunRecord]) -> anyhow::Result<()> {
    if runs.is_empty() {
        writeln!(out, "\nno tool runs were recorded")?;
        return Ok(());
    }
    writeln!(out, "\ntool runs")?;
    for run in runs {
        writeln!(out, "  #{} {}", run.id, run.tool_id)?;
        writeln!(out, "      executable  {}", run.executable)?;
        writeln!(out, "      args        {}", run.args_json)?;
        if let Some(dir) = &run.working_dir {
            writeln!(out, "      cwd         {dir}")?;
        }
        match run.child_pid {
            Some(pid) => writeln!(out, "      child pid   {pid}")?,
            None => writeln!(out, "      child pid   (never started)")?,
        }
        if let Some(started) = run.started_at_ms {
            writeln!(out, "      started     {}", format_ms(started))?;
        }
        if let Some(ended) = run.ended_at_ms {
            writeln!(out, "      ended       {}", format_ms(ended))?;
        }
        match run.termination {
            Some(t) => writeln!(out, "      termination {t}")?,
            None => writeln!(out, "      termination (unfinished)")?,
        }
        match run.exit_code {
            Some(code) => writeln!(out, "      exit code   {code}")?,
            None => writeln!(out, "      exit code   (none)")?,
        }
        writeln!(
            out,
            "      stdout      {} bytes{}",
            run.stdout_bytes,
            if run.stdout_truncated {
                " (truncated)"
            } else {
                ""
            }
        )?;
        writeln!(
            out,
            "      stderr      {} bytes{}",
            run.stderr_bytes,
            if run.stderr_truncated {
                " (truncated)"
            } else {
                ""
            }
        )?;
        if let Some(path) = &run.stdout_path {
            writeln!(out, "      artifact    {path}")?;
        }
        if let Some(error) = &run.error {
            writeln!(out, "      error       {error}")?;
        }
    }
    Ok(())
}

fn print_events(out: &mut impl Write, events: &[EventRecord]) -> anyhow::Result<()> {
    writeln!(out, "\nevents")?;
    if events.is_empty() {
        writeln!(out, "  (none)")?;
        return Ok(());
    }
    for event in events {
        writeln!(
            out,
            "  {} {:<18} {}",
            format_ms(event.at_ms),
            event.kind.as_str(),
            event.detail
        )?;
    }
    Ok(())
}

fn print_capture(
    out: &mut impl Write,
    stream: &str,
    path: Option<&str>,
    head: usize,
) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    writeln!(out, "\n{stream} — {path}")?;

    // Bounded read: an artifact can be as large as the configured per-stream cap,
    // and pulling all of it into memory to print the first 2 KiB would be
    // wasteful. `--head` controls how much is shown, not how much is loaded.
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            writeln!(out, "  (could not be opened: {e})")?;
            return Ok(());
        }
    };
    let mut buffer = vec![0u8; head];
    let mut filled = 0usize;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                writeln!(out, "  (could not be read: {e})")?;
                return Ok(());
            }
        }
    }
    buffer.truncate(filled);

    let text = String::from_utf8_lossy(&buffer);
    for line in text.lines() {
        writeln!(out, "  | {line}")?;
    }
    if filled == head {
        let total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if total > head as u64 {
            writeln!(
                out,
                "  | … {} more bytes; pass --head to see further",
                total - head as u64
            )?;
        }
    }
    Ok(())
}

// ── cancel ──────────────────────────────────────────────────────────────────

fn cmd_cancel(ctx: &Context, id: String, timeout_secs: u64) -> anyhow::Result<ExitCode> {
    validate_task_id(&id)?;
    let task = ctx.store.require_task(&id)?;

    if !task.state.is_in_flight() {
        // Not an error worth a nonzero exit: the task is already finished, which
        // is what the caller wanted. Saying so plainly is the useful answer.
        println!(
            "task {id} is already {} — nothing to cancel",
            task.state.as_str()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let pipe_name = cancel_pipe_name(&ctx.config.ipc.pipe_prefix, &id)?;
    let timeout = std::time::Duration::from_secs(timeout_secs.max(1)).min(cancel_timeout());

    match request_cancel(&pipe_name, &id, timeout) {
        Ok(response) => {
            if response.is_failure() {
                eprintln!("jean: the runner refused the cancel request: {response:?}");
                return Ok(ExitCode::from(1));
            }
            println!("cancellation requested for {id}");
            println!(
                "the runner will terminate the whole process tree; use `jean task {id}` to see the recorded outcome"
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            // No listener. That means the runner is gone, which is a crash rather
            // than a live task — so reconciling now is correct, and it is the
            // opposite of the second-client case, where the runner answers.
            eprintln!("jean: no runner is listening on {pipe_name}: {e}");
            let reconciled = jean_core::reconcile(&ctx.store)?;
            if let Some(summary) = reconciled.summary() {
                println!("{summary}");
            }
            let after = ctx.store.require_task(&id)?;
            println!("task {id} is now {}", after.state.as_str());
            if after.state == TaskState::NeedsAttention {
                println!(
                    "the command was not replayed; inspect it with `jean task {id} --events --output`"
                );
                return Ok(ExitCode::from(4));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
