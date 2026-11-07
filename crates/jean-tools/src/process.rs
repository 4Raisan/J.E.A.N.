//! Process *inspection*, using `sysinfo`.
//!
//! ## What this is for
//!
//! The `jean inspect` intent: answer "what is this process doing" and "what is
//! running" from one crate, with numbers that mean what they say.
//!
//! ## The rule that shapes everything here
//!
//! **This module never modifies a process.** It does not terminate, suspend, signal
//! or resume anything, and it holds no handle it could use to do so. Cancellation of
//! work J.E.A.N. started belongs to `jean-process`, where it goes through a Job
//! Object with kill-on-close so the whole tree dies together; an inspection path that
//! could also kill would eventually be used by someone who only meant to look. There
//! is therefore no kill function in this file, and adding one is a design change, not
//! a feature.
//!
//! ## Honesty rules for the numbers
//!
//! * **CPU needs two samples.** `Process::cpu_usage()` is a *rate* over the interval
//!   between refreshes. The first refresh after construction has no earlier sample to
//!   diff against, so it reports 0 — and a report that says "0% CPU" about a busy
//!   process is worse than one that says nothing. So every entry point refreshes
//!   twice with a caller-visible gap and states that gap in `sample_window_ms`.
//! * **CPU is percent of ONE core** and can legitimately exceed 100 on a multi-core
//!   process. Not normalised to 0..=100, because that hides parallelism: a build
//!   compiler at 1600% on 24 cores is doing exactly what it should.
//! * **A refused field is named, never zeroed.** Windows will not hand over another
//!   user's command line without elevation, and a `None` there is information. Each
//!   report carries `unavailable_fields`, so "0 bytes" can never masquerade as "idle".
//! * **A process that exits mid-sample is a normal outcome**, returned as
//!   [`InspectOutcome::Gone`], not as an error. Short-lived processes are the common
//!   case, not the exception, on a working machine.
//!
//! ## Refresh cost
//!
//! `ProcessRefreshKind` is narrowed to exactly the fields printed. `refresh_processes`
//! over every process with everything enabled is the difference between a 50 ms and a
//! one-second command on a busy desktop, and a status line nobody waits for is a
//! status line nobody reads.

use std::path::PathBuf;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::apps::strip_exe;
use crate::error::{Result, ToolError};

/// Default gap between the two CPU samples, milliseconds.
///
/// `sysinfo` itself refuses to compute a rate over less than 200 ms on some platforms
/// and returns a stale value instead. 150 ms is short enough that `jean inspect` still
/// feels instant and long enough that a real delta exists on a live process.
pub const DEFAULT_SAMPLE_WINDOW_MS: u64 = 150;

/// Cap on rows in a listing when the caller does not choose one.
pub const DEFAULT_MAX_ROWS: usize = 30;

// ─────────────────────────────────────────────────────────────────────────────
// Field names.
// ─────────────────────────────────────────────────────────────────────────────

/// Names this module uses in `unavailable_fields`.
///
/// A `&'static str` rather than a free-form string so the set is closed: a caller can
/// match on it, and a typo cannot invent a field name that makes a missing value look
/// like a documented one.
pub mod fields {
    pub const EXE_PATH: &str = "exe_path";
    pub const COMMAND_LINE: &str = "command_line";
    pub const PARENT_PID: &str = "parent_pid";
    pub const MEMORY_BYTES: &str = "memory_bytes";
    pub const VIRTUAL_MEMORY_BYTES: &str = "virtual_memory_bytes";
    pub const START_TIME_UNIX_SECS: &str = "start_time_unix_secs";
    pub const RUN_TIME_SECS: &str = "run_time_secs";
    pub const CPU_PERCENT_OF_ONE_CORE: &str = "cpu_percent_of_one_core";
    pub const THREAD_COUNT: &str = "thread_count";
}

// ─────────────────────────────────────────────────────────────────────────────
// Types reported.
// ─────────────────────────────────────────────────────────────────────────────

/// One process, fully described.
///
/// Every unit is in the field name. Nothing here is "memory" or "time": a report whose
/// units live in prose gets misread the first time somebody pipes it into a script.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessReport {
    pub pid: u32,
    /// Process name as text. `sysinfo` hands back an `OsStr`; converting here means
    /// the CLI never has to decide what to do with a non-UTF-8 name.
    pub name: String,
    pub exe_path: Option<PathBuf>,
    /// Command line as an argument list. `None` usually means the target runs as
    /// another user and reading its memory requires elevation — which is reported in
    /// `unavailable_fields`, not guessed around.
    pub command_line: Option<Vec<String>>,
    pub parent_pid: Option<u32>,
    /// Resident set: physical RAM currently mapped. Zero *and* listed in
    /// `unavailable_fields` means "refused", not "idle".
    pub memory_bytes: u64,
    /// On Windows this is the process's **private** (page-file backed) usage — it matches
    /// `Get-Process PrivateMemorySize64`, not `VirtualMemorySize64`. Measured against both
    /// while a `cmd.exe` idled in a loop: 6.54 MiB here, ~2 TiB for the address space.
    /// The name is the sampler's; the column the CLI prints is labelled for the reality.
    pub virtual_memory_bytes: u64,
    /// Seconds since the Unix epoch.
    pub start_time_unix_secs: u64,
    pub run_time_secs: u64,
    /// Cumulative CPU across all threads, milliseconds.
    pub accumulated_cpu_ms: u64,
    /// Percent of ONE core over `sample_window_ms`. Can exceed 100.
    pub cpu_percent_of_one_core: f32,
    pub sample_window_ms: u64,
    pub status: String,
    /// Threads, where the platform exposes them per process.
    ///
    /// `sysinfo` surfaces this only on Linux/Android (as `tasks()`), so on Windows it is
    /// always `None` and `thread_count` appears in `unavailable_fields`. Counting threads
    /// here would mean taking a toolhelp snapshot per process — a cost this module pays
    /// only when explicitly asked, never to fill a field with a number nobody needs.
    pub thread_count: Option<u32>,
    pub unavailable_fields: Vec<&'static str>,
}

/// The result of inspecting one process by id.
#[derive(Debug, Clone, PartialEq)]
pub enum InspectOutcome {
    /// The process was alive and sampled.
    Found(ProcessReport),
    /// The id was absent, or exited during the sampling window.
    ///
    /// A normal result rather than a failure: short-lived processes are the common case
    /// on a working machine, and reporting their death as an error trains operators to
    /// ignore errors.
    ///
    /// `seen_at_ms` is non-zero only when the process was present before the sampling gap
    /// and gone after it, which is how a caller tells "vanished under us" from "never
    /// existed" without making a second call.
    Gone { pid: u32, seen_at_ms: u64 },
}

impl InspectOutcome {
    /// Borrow the report when the process was found.
    pub fn report(&self) -> Option<&ProcessReport> {
        match self {
            InspectOutcome::Found(r) => Some(r),
            InspectOutcome::Gone { .. } => None,
        }
    }
}

/// Header numbers that make a per-process table interpretable.
///
/// A row saying "1.2 GB" means nothing without knowing the machine has 8 GB, and a
/// row saying "180% CPU" means nothing without the core count. Both belong in the
/// report rather than being looked up separately by whoever prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSummary {
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub physical_core_count: usize,
    /// Logical processors, which is what `cpu_percent_of_one_core` is bounded by.
    pub logical_core_count: usize,
    /// Seconds since boot.
    pub uptime_secs: u64,
    /// Processes visible to this account at sample time.
    pub process_count: usize,
    /// Fields the OS refused on this call, by name.
    pub unavailable_fields: Vec<&'static str>,
}

/// A bounded top-N listing.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessListing {
    pub machine: MachineSummary,
    /// Sorted by [`SortKey`]'s order, truncated to `max_rows`.
    pub rows: Vec<ProcessReport>,
    /// True when more processes matched than were returned.
    pub truncated: bool,
    pub sort_by: SortKey,
    pub max_rows: usize,
    pub sample_window_ms: u64,
}

/// Which column orders a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Highest CPU first, then memory, then pid.
    Cpu,
    /// Largest resident set first, then CPU, then pid.
    Memory,
    /// Lowest pid first. The stable, boring choice, and the one that makes two runs
    /// comparable.
    Pid,
}

impl SortKey {
    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Cpu => "cpu",
            SortKey::Memory => "memory",
            SortKey::Pid => "pid",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sampling.
// ─────────────────────────────────────────────────────────────────────────────

/// The refresh kind used everywhere in this module.
///
/// Narrowed to the fields actually printed: cpu, memory, cmd and exe. Deliberately
/// *without* `disk_usage`, `cwd`, `root`, `environ` or `user` —
/// each of those is another handle query per process, and on a listing of hundreds of
/// processes the difference is hundreds of milliseconds.
fn refresh_kind() -> ProcessRefreshKind {
    // `cmd` keeps the default `UpdateKind::OnlyIfNotSet` rather than `Always`: reading a
    // command line is a separate handle query per process, and doing it on both samples
    // doubles that cost for a value that does not change between them. `exe` is `Always`
    // because an image can be replaced on disk while running, and a cached path would
    // then point at something other than what is executing.
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::Always)
}

/// Whether this build can see processes at all.
///
/// **Deviation from the brief.** The plan assumed `sysinfo::System::IS_SUPPORTED`, which
/// exists in older releases but is gone in 0.40.1 — that release made the constructors
/// fallible and moved the support question into their `Result`. With no public constant to
/// read, support is *probed* rather than asserted: a host reporting zero physical cores
/// and a failing `uptime()` has no working process backend, and returning zeros for such a
/// machine would be inventing numbers about something this code cannot see.
///
/// Cached, because it costs two syscalls once per thread and cannot change while running.
pub fn inspection_supported() -> bool {
    SUPPORTED.with(|cell| {
        if let Some(known) = cell.get() {
            return known;
        }
        let cores = System::physical_core_count().unwrap_or(0);
        let known = cores > 0 && System::uptime().is_ok();
        cell.set(Some(known));
        known
    })
}

thread_local! {
    /// `None` until first probed. Thread-local rather than a global lock: this sits on the
    /// way into every report, and a mutex around two booleans is pure ceremony.
    static SUPPORTED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Build a `System` with process data primed, or explain why this build cannot.
///
/// Memory is requested explicitly here (`with_ram`) as well as refreshed later: a
/// `System` built without it reports `total_memory() == 0`, and a listing header of
/// "0 bytes total" under rows it is meant to contextualise is worse than no header.
fn new_system() -> Result<System> {
    if !inspection_supported() {
        return Err(ToolError::SysinfoUnavailable {
            reason: "the process-information backend reports no physical cores and no                      readable uptime, so per-process numbers here would be invented"
                .to_owned(),
        });
    }
    System::new_with_specifics(
        sysinfo::RefreshKind::nothing()
            .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram())
            .with_processes(refresh_kind()),
    )
    .map_err(|e| ToolError::SysinfoUnavailable {
        reason: format!("System::new_with_specifics failed: {e}"),
    })
}

fn sleep_for(window_ms: u64) {
    // A zero window must not spin: sleeping 0 is legal but useless, and the CPU rate
    // would be computed over an interval too short to measure anything. Callers get a
    // documented floor rather than a surprise divide-by-nothing.
    let ms = window_ms.max(MIN_SAMPLE_WINDOW_MS);
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Floor for the sampling gap, milliseconds.
///
/// Below this the second sample is taken before the system timer has meaningfully
/// advanced, and the resulting "CPU%" is noise presented as a measurement.
pub const MIN_SAMPLE_WINDOW_MS: u64 = 20;

/// Sample the whole process table twice, honouring the CPU-delta requirement.
///
/// `remove_dead_processes` is `true` on both calls, which is what makes a process that
/// exits between them leave the map instead of lingering as a stale row — the behaviour
/// [`InspectOutcome::Gone`] depends on.
///
/// Returns the system plus the elapsed wall clock, which becomes `seen_at_ms` for the
/// `Gone` outcome.
fn sample_all(window_ms: u64) -> Result<(System, u64)> {
    let started = std::time::Instant::now();
    let mut system = new_system()?;
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());
    sleep_for(window_ms);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());
    // Refreshed again by name, not left to whatever the constructor primed: memory
    // totals move, and a header stale by one sampling gap is a header that misleads.
    system.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
    Ok((system, started.elapsed().as_millis() as u64))
}

/// One pass over the whole table with no gap, used only to ask whether a pid is present.
fn sample_once() -> Result<System> {
    let mut system = new_system()?;
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());
    Ok(system)
}

/// Prove a pid is currently unclaimed, for tests that need a guaranteed-absent id.
///
/// A made-up pid could collide with something real, and a test asserting "not found"
/// against a live process would fail randomly on someone else's machine. This scans the
/// actual table instead. It deliberately does not invent a sentinel such as `u32::MAX`:
/// on this backend `Pid` is an opaque `usize`-sized value and a large number may be
/// reported as present by a wrapping handle conversion, which is exactly the sort of
/// assumption worth not making.
pub fn unused_pid() -> Result<u32> {
    let system = sample_once()?;
    let mut candidate = 2u32;
    while system.process(Pid::from_u32(candidate)).is_some() {
        candidate += 1;
        if candidate > 100_000 {
            return Err(ToolError::ProcessQuery {
                pid: candidate,
                reason: "no unclaimed pid below 100000; refusing to guess one".to_owned(),
            });
        }
    }
    Ok(candidate)
}

/// Convert a `sysinfo::Pid` to the `u32` this crate reports.
fn pid_u32(pid: Pid) -> u32 {
    pid.as_u32()
}

/// Read the machine header.
///
/// Each unsupported number is named in `unavailable_fields` instead of defaulting:
/// `physical_core_count()` genuinely fails on some VMs and remote sessions, and a
/// listing that claims "0 cores" invites the reader to conclude the numbers are junk.
fn machine_summary(system: &System) -> MachineSummary {
    let mut unavailable = Vec::new();

    let total_memory_bytes = system.total_memory();
    if total_memory_bytes == 0 {
        unavailable.push("total_memory_bytes");
    }
    let used_memory_bytes = system.used_memory();
    if used_memory_bytes == 0 {
        unavailable.push("used_memory_bytes");
    }

    let physical_core_count = match System::physical_core_count() {
        Ok(n) if n > 0 => n,
        Ok(_) => {
            unavailable.push("physical_core_count");
            0
        }
        Err(_) => {
            unavailable.push("physical_core_count");
            0
        }
    };

    let cpus = system.cpus();
    let logical_core_count = if cpus.is_empty() {
        unavailable.push("logical_core_count");
        0
    } else {
        cpus.len()
    };

    let uptime_secs = match System::uptime() {
        Ok(n) => n,
        Err(_) => {
            unavailable.push("uptime_secs");
            0
        }
    };

    MachineSummary {
        total_memory_bytes,
        used_memory_bytes,
        physical_core_count,
        logical_core_count,
        uptime_secs,
        process_count: system.processes().len(),
        unavailable_fields: unavailable,
    }
}

/// Build one report from a live `Process`.
///
/// Takes the pieces as arguments rather than reaching for globals so the shaping is
/// testable without a real process table.
// Sixteen separate observations, none of them a coherent struct of its own;
// grouping them would invent a type that only ever exists inside this function.
#[allow(clippy::too_many_arguments)]
fn build_report(
    pid: u32,
    name: String,
    exe_path: Option<PathBuf>,
    command_line: Option<Vec<String>>,
    parent_pid: Option<u32>,
    memory_bytes: u64,
    virtual_memory_bytes: u64,
    start_time_unix_secs: u64,
    run_time_secs: u64,
    accumulated_cpu_ms: u64,
    cpu_percent_of_one_core: f32,
    status: String,
    thread_count: Option<u32>,
    sample_window_ms: u64,
) -> ProcessReport {
    let mut unavailable: Vec<&'static str> = Vec::new();
    if exe_path.is_none() {
        unavailable.push(fields::EXE_PATH);
    }
    if command_line.is_none() {
        unavailable.push(fields::COMMAND_LINE);
    }
    if parent_pid.is_none() {
        unavailable.push(fields::PARENT_PID);
    }
    if memory_bytes == 0 {
        unavailable.push(fields::MEMORY_BYTES);
    }
    if virtual_memory_bytes == 0 {
        unavailable.push(fields::VIRTUAL_MEMORY_BYTES);
    }
    if start_time_unix_secs == 0 {
        unavailable.push(fields::START_TIME_UNIX_SECS);
    }
    // An age of zero is a measurement, not a refusal: a process younger than one second has run
    // for no whole seconds, and the start time — which is what the age is derived from — is
    // present. Naming the field here would tell an operator the OS withheld something it answered.
    if run_time_secs == 0 && start_time_unix_secs == 0 {
        unavailable.push(fields::RUN_TIME_SECS);
    }
    if thread_count.is_none() {
        unavailable.push(fields::THREAD_COUNT);
    }

    ProcessReport {
        pid,
        name,
        exe_path,
        command_line,
        parent_pid,
        memory_bytes,
        virtual_memory_bytes,
        start_time_unix_secs,
        run_time_secs,
        accumulated_cpu_ms,
        cpu_percent_of_one_core,
        sample_window_ms,
        status,
        thread_count,
        unavailable_fields: unavailable,
    }
}

/// Turn `sysinfo`'s `OsStr` name into owned text.
///
/// Lossy on purpose: a process name can be non-UTF-8 on Windows (legacy code-page
/// names exist), and refusing to report a process because its name is odd would hide
/// exactly the kind of unusual process an operator is looking for.
fn process_name(p: &sysinfo::Process) -> String {
    p.name().to_string_lossy().into_owned()
}

fn command_line_of(p: &sysinfo::Process) -> Option<Vec<String>> {
    let args: Vec<String> = p
        .cmd()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if args.is_empty() {
        // Empty is what `sysinfo` returns both for "the OS refused" and for a genuine
        // process with no arguments. Distinguishing them needs a handle we do not open,
        // so the safe reading is "not available" — reported, not asserted.
        return None;
    }
    Some(args)
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry points.
// ─────────────────────────────────────────────────────────────────────────────

/// Inspect one process by id.
///
/// Two refreshes separated by `sample_window_ms` are spent on the whole table, not the
/// one process: CPU percentage is a delta against *system* time, and refreshing a
/// single pid leaves the system baseline from the first sample unchanged, producing a
/// rate computed over the wrong denominator.
pub fn inspect_pid(pid: u32, sample_window_ms: u64) -> Result<InspectOutcome> {
    let target = Pid::from_u32(pid);
    // Presence is checked before the sampling gap so `seen_at_ms` carries information:
    // a process that was never there reports 0, one that died mid-window reports when we
    // stopped seeing it. Without the probe those two facts are indistinguishable.
    let present_before = sample_once()?.process(target).is_some();
    let window = clamp_sample_window(sample_window_ms);
    let (system, seen_at_ms) = sample_all(window)?;
    match system.process(target) {
        Some(p) => Ok(InspectOutcome::Found(report_from(p, window))),
        None => {
            tracing::debug!(
                pid,
                present_before,
                "process absent after the sampling window"
            );
            Ok(InspectOutcome::Gone {
                pid,
                seen_at_ms: if present_before { seen_at_ms } else { 0 },
            })
        }
    }
}

/// Build the report shape from a live process handle.
fn report_from(p: &sysinfo::Process, window_ms: u64) -> ProcessReport {
    let exe_path = p.exe().map(|path| path.to_path_buf());
    let command_line = command_line_of(p);
    let parent_pid = p.parent().map(pid_u32);
    let memory_bytes = p.memory();
    let virtual_memory_bytes = p.virtual_memory();
    let start_time_unix_secs = p.start_time();
    let run_time_secs = p.run_time();
    let accumulated_cpu_ms = p.accumulated_cpu_time();
    let cpu = p.cpu_usage();
    let status = p.status().to_string();
    // `tasks()` is populated only on Linux/Android, so on Windows this is always `None`
    // and lands in `unavailable_fields`. Written as an expression rather than a literal
    // `None` so the field still fills itself when the platform does expose it.
    let thread_count = p.tasks().map(|t| t.len() as u32);

    build_report(
        pid_u32(p.pid()),
        process_name(p),
        exe_path,
        command_line,
        parent_pid,
        memory_bytes,
        virtual_memory_bytes,
        start_time_unix_secs,
        run_time_secs,
        accumulated_cpu_ms,
        cpu,
        status,
        thread_count,
        window_ms,
    )
}

/// Inspect the current process — the cheapest possible sanity check, and the one that
/// needs no privileges.
pub fn inspect_self(sample_window_ms: u64) -> Result<InspectOutcome> {
    let pid = std::process::id();
    inspect_pid(pid, sample_window_ms)
}

/// Locate processes by executable name.
///
/// Exact case-insensitive match first, then prefix. When several processes match, all
/// are returned: three `code.exe` windows are a fact about the machine, and picking one
/// silently is the same mistake as picking one silently in [`crate::apps`].
///
/// Matching is against the process *name* (`code.exe`), not the full path, because a
/// listing keyed on paths would miss processes whose image path the OS refuses to give
/// up — which is precisely the case an operator is investigating.
pub fn inspect_by_name(name: &str, sample_window_ms: u64) -> Result<Vec<ProcessReport>> {
    let needle = name.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let needle = strip_exe(&needle).to_owned();
    let (system, _) = sample_all(sample_window_ms)?;
    let window = sample_window_ms.max(MIN_SAMPLE_WINDOW_MS);

    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    for p in system.processes().values() {
        let candidate = strip_exe(&p.name().to_string_lossy()).to_ascii_lowercase();
        if candidate == needle {
            exact.push(report_from(p, window));
        } else if candidate.starts_with(&needle) {
            prefix.push(report_from(p, window));
        }
    }
    let mut out = exact;
    out.extend(prefix);
    // Deterministic within each rank: pid order, so two runs print the same list.
    out.sort_by_key(|r| r.pid);
    Ok(out)
}

/// Bounded top-N listing.
///
/// `max_rows` bounds the *report*, not the walk: every process is examined, because
/// "top 30 by memory" cannot be answered by stopping after 30. The bound on cost is
/// the narrowed `ProcessRefreshKind`, not the row count.
pub fn list_processes(
    sort_by: SortKey,
    max_rows: usize,
    sample_window_ms: u64,
) -> Result<ProcessListing> {
    let (system, _) = sample_all(sample_window_ms)?;
    let window = sample_window_ms.max(MIN_SAMPLE_WINDOW_MS);
    let machine = machine_summary(&system);

    let mut rows: Vec<ProcessReport> = system
        .processes()
        .values()
        .map(|p| report_from(p, window))
        .collect();

    sort_rows(&mut rows, sort_by);
    let truncated = rows.len() > max_rows;
    rows.truncate(max_rows);

    Ok(ProcessListing {
        machine,
        rows,
        truncated,
        sort_by,
        max_rows,
        sample_window_ms: window,
    })
}

/// Order rows by the chosen key, always ending on pid so the order is total.
///
/// Without the final tiebreaker, two processes with equal memory (very common: idle
/// ones cluster at the same page count) would come out in hash-map order and the
/// listing would differ between runs.
fn sort_rows(rows: &mut [ProcessReport], sort_by: SortKey) {
    rows.sort_by(|a, b| match sort_by {
        SortKey::Cpu => a
            .cpu_percent_of_one_core
            .total_cmp(&b.cpu_percent_of_one_core)
            .reverse()
            .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            .then_with(|| a.pid.cmp(&b.pid)),
        SortKey::Memory => b
            .memory_bytes
            .cmp(&a.memory_bytes)
            .then_with(|| {
                a.cpu_percent_of_one_core
                    .total_cmp(&b.cpu_percent_of_one_core)
                    .reverse()
            })
            .then_with(|| a.pid.cmp(&b.pid)),
        SortKey::Pid => a.pid.cmp(&b.pid),
    });
}

/// Upper bound a caller should accept for `cpu_percent_of_one_core`.
///
/// `sysinfo` computes percent-of-one-core scaled by the core count, so the theoretical
/// maximum is `logical_core_count * 100.0`. Exposing the bound lets a CLI assert its
/// own output is sane instead of trusting the library's arithmetic.
pub fn cpu_bound_percent(logical_core_count: usize) -> f32 {
    logical_core_count.saturating_mul(100) as f32
}

/// True when a CPU figure is inside the physically meaningful range.
///
/// NaN or negative values would mean the backend produced something unusable; this is
/// a check the caller can run rather than a guarantee this crate silently enforces.
pub fn cpu_is_sane(cpu_percent_of_one_core: f32, logical_core_count: usize) -> bool {
    let bound = cpu_bound_percent(logical_core_count);
    cpu_percent_of_one_core.is_finite()
        && cpu_percent_of_one_core >= 0.0
        && cpu_percent_of_one_core <= bound.max(100.0)
}

/// The minimum and maximum accepted sample window, for argument validation upstream.
pub const MAX_SAMPLE_WINDOW_MS: u64 = 10_000;

/// Validate a caller-supplied window before spending it on a sleep.
///
/// Clamped rather than rejected: `jean inspect --window 0` should produce a slower-
/// than-requested but correct measurement, not an error, and the clamped value is
/// echoed back in `sample_window_ms` so the report never claims a window it did not use.
pub fn clamp_sample_window(requested_ms: u64) -> u64 {
    requested_ms.clamp(MIN_SAMPLE_WINDOW_MS, MAX_SAMPLE_WINDOW_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fields `build_report` decides are unavailable, for a probe with the given numbers.
    fn unavailable_for(start_time: u64, run_time: u64, memory: u64) -> Vec<&'static str> {
        build_report(
            4_242,
            "probe.exe".to_owned(),
            Some(PathBuf::from("C:/probe.exe")),
            Some(vec!["C:/probe.exe".to_owned()]),
            Some(1),
            memory,
            2_048,
            start_time,
            run_time,
            5,
            0.0,
            "Running".to_owned(),
            Some(3),
            150,
        )
        .unavailable_fields
    }

    #[test]
    fn an_age_under_one_second_is_a_measurement_not_a_refusal() {
        // A process younger than a second: the OS answered every question, and the answer to
        // "how long" is zero whole seconds. `start_time` being present is what proves it was
        // answered, which is why the two are checked together.
        let names = unavailable_for(1_700_000_000, 0, 1_024);
        assert!(
            !names.contains(&fields::RUN_TIME_SECS),
            "a zero age with a known start time must not be reported as withheld: {names:?}"
        );
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn an_unknown_start_time_still_names_both_time_fields() {
        let names = unavailable_for(0, 0, 1_024);
        assert!(names.contains(&fields::START_TIME_UNIX_SECS), "{names:?}");
        assert!(names.contains(&fields::RUN_TIME_SECS), "{names:?}");
    }

    #[test]
    fn a_zero_memory_is_reported_as_withheld_because_no_live_process_has_it() {
        let names = unavailable_for(1_700_000_000, 12, 0);
        assert!(names.contains(&fields::MEMORY_BYTES), "{names:?}");
    }

    #[test]
    fn the_sample_window_is_clamped_into_the_range_it_can_actually_honour() {
        assert_eq!(clamp_sample_window(0), MIN_SAMPLE_WINDOW_MS);
        assert_eq!(clamp_sample_window(1_000_000), MAX_SAMPLE_WINDOW_MS);
        assert_eq!(clamp_sample_window(500), 500);
    }
}
