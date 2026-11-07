//! The structured plan a model returns, and the shape it must have arrived in.
//!
//! ## Why the schema is this narrow
//!
//! A model may choose *which* supported operation runs and *what* it acts on. It may not
//! describe how to run anything. Every field here names something that already exists in
//! this build — a registered tool id, an application name to resolve, a pid, a search
//! root — and there is deliberately no field for an executable path, a command string, a
//! working directory, an environment variable, a policy tier, or a timeout longer than the
//! tool's own. A model that invents one produces a plan that does not parse. That is the
//! mechanism, not a policy: there is no vocabulary in which to ask.
//!
//! ## Validation is split in two, on purpose
//!
//! * This module answers questions about the plan text alone: schema version, unknown
//!   fields, step count, id shape, argument types and bounds, the dependency graph, and
//!   whether the verification a step declares is one its operation can produce. It is pure,
//!   so it runs before any store, policy or process exists.
//! * `jean_core::planner` answers the questions that need the live world: is the tool
//!   registered, enabled and healthy; do its declared parameters accept these arguments; is
//!   this root inside the authorised workspace; would policy refuse this operation outright.
//!
//! Both pass before the first side effect.
//!
//! ## Sequential only
//!
//! Steps run in declaration order. `depends_on` means *stop if that one failed* — it does
//! not schedule, and no value flows between steps. There is no concurrency here because the
//! cancellation design has one active task per process (D-028).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The plan schema this build understands. Any other version is refused rather than
/// reinterpreted, for the same reason as `config_version`.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// Ceilings that bind regardless of configuration, so a runaway model cannot ask for a
/// 10,000-step plan even where `model.max_plan_steps` is generous. Configuration may
/// tighten these, never loosen them.
pub const MAX_PLAN_STEPS: usize = 32;
pub const MAX_ID_LEN: usize = 32;
pub const MAX_TITLE_LEN: usize = 200;
pub const MAX_GOAL_LEN: usize = 2_000;
pub const MAX_DEPENDS_PER_STEP: usize = 8;
pub const MAX_ARGUMENTS_PER_STEP: usize = 16;
/// One argument's rendered length. Long enough for a path or a query, short enough that an
/// argument is never a channel for a script body.
pub const MAX_ARGUMENT_LEN: usize = 1_024;
pub const MAX_PATTERN_LEN: usize = 256;

/// What a step declares as its proof of success.
///
/// Required on every step: D-012 says a missing verification is a bug, not a default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeclaredVerification {
    /// The child exits with this code — the weakest honest claim.
    ExitCode {
        #[serde(default = "default_expected_exit")]
        expected: i32,
    },
    /// The child exits with this code *and* its captured output contains the needle.
    OutputContains {
        #[serde(default = "default_expected_exit")]
        expected: i32,
        needle: String,
    },
    /// A window an operator can see belongs to the process that was started (D-023).
    WindowForPid,
    /// The pid is alive and the fields this platform can report are non-null.
    FieldsAvailable,
    /// Each reported path exists and opened for reading.
    PathReadable,
}

fn default_expected_exit() -> i32 {
    0
}

impl DeclaredVerification {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExitCode { .. } => "exit_code",
            Self::OutputContains { .. } => "output_contains",
            Self::WindowForPid => "window_for_pid",
            Self::FieldsAvailable => "fields_available",
            Self::PathReadable => "path_readable",
        }
    }

    pub fn needle(&self) -> Option<&str> {
        match self {
            Self::OutputContains { needle, .. } => Some(needle),
            _ => None,
        }
    }

    pub fn expected_exit(&self) -> Option<i32> {
        match self {
            Self::ExitCode { expected } | Self::OutputContains { expected, .. } => Some(*expected),
            _ => None,
        }
    }

    /// The tool-level verification this declaration corresponds to, when one exists.
    ///
    /// `jean-core` uses this so a command step is checked by the *same* verifier that Phase 0
    /// and Phase 1 use — a plan does not get to invent a second notion of "verified". The three
    /// non-process declarations return `None` because their evidence is a window, a field or a
    /// directory entry, which the intent pipeline already gathers and `verify::verify` never sees.
    pub fn to_verify_spec(&self) -> Option<crate::tool::VerifySpec> {
        match self {
            Self::ExitCode { expected } => Some(crate::tool::VerifySpec::ExitCode {
                expected: *expected,
            }),
            Self::OutputContains { expected, needle } => {
                Some(crate::tool::VerifySpec::OutputContains {
                    expected: *expected,
                    needle: needle.clone(),
                    // The plan schema declares no case option, and defaulting to insensitive would
                    // weaken a check the operator cannot see they agreed to.
                    case_insensitive: false,
                })
            }
            Self::WindowForPid | Self::FieldsAvailable | Self::PathReadable => None,
        }
    }
}

/// The type a plan argument claims to carry.
///
/// The type is part of the wire format rather than inferred from the text, so `1` as a pid
/// and `"1"` as free text are distinguishable and a mismatch is reported instead of guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValue {
    Text(String),
    Integer(i64),
    Boolean(bool),
    /// A path, as text. Whether it is inside the authorised workspace is a live check in
    /// `jean_core::planner`; this type only says what the value is shaped like.
    Path(String),
    Pid(u32),
}

impl ArgValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Integer(_) => "integer",
            Self::Boolean(_) => "boolean",
            Self::Path(_) => "path",
            Self::Pid(_) => "pid",
        }
    }

    /// The text that goes into an argument vector, and what an operator sees.
    pub fn render(&self) -> String {
        match self {
            Self::Text(s) | Self::Path(s) => s.clone(),
            Self::Integer(i) => i.to_string(),
            Self::Boolean(b) => b.to_string(),
            Self::Pid(p) => p.to_string(),
        }
    }
}

/// One argument on the wire: `{"type":"path","value":"C:/x"}`.
///
/// Kept as a struct with a `serde_json::Value` payload rather than an adjacently-tagged enum
/// so a wrong type produces *our* diagnostic — naming the argument, the declared type and
/// what was found — instead of serde's union message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanArgument {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub arg_type: String,
    pub value: serde_json::Value,
}

impl PlanArgument {
    /// Interpret the value against its declared type.
    pub fn to_value(&self) -> Result<ArgValue, String> {
        let label = || {
            self.name
                .as_ref()
                .map(|n| format!("argument {n:?}"))
                .unwrap_or_else(|| "argument".to_owned())
        };
        match self.arg_type.as_str() {
            "text" => self
                .value
                .as_str()
                .map(|s| ArgValue::Text(s.to_owned()))
                .ok_or_else(|| {
                    format!(
                        "{} is type \"text\" but carries {}",
                        label(),
                        shape(&self.value)
                    )
                }),
            "integer" => self.value.as_i64().map(ArgValue::Integer).ok_or_else(|| {
                format!(
                    "{} is type \"integer\" but carries {}",
                    label(),
                    shape(&self.value)
                )
            }),
            "boolean" => self.value.as_bool().map(ArgValue::Boolean).ok_or_else(|| {
                format!(
                    "{} is type \"boolean\" but carries {}",
                    label(),
                    shape(&self.value)
                )
            }),
            "path" => self
                .value
                .as_str()
                .map(|s| ArgValue::Path(s.to_owned()))
                .ok_or_else(|| {
                    format!(
                        "{} is type \"path\" but carries {}",
                        label(),
                        shape(&self.value)
                    )
                }),
            "pid" => self
                .value
                .as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .map(ArgValue::Pid)
                .ok_or_else(|| {
                    format!(
                        "{} is type \"pid\" but carries {}",
                        label(),
                        shape(&self.value)
                    )
                }),
            other => Err(format!(
                "{} has unknown type {other:?}; supported types are text, integer, boolean, \
                 path and pid",
                label()
            )),
        }
    }
}

/// What one step does. Tagged, so a step cannot carry another operation's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanOperation {
    /// Run a registered tool. `tool` is a registry id, never a path or program name.
    RunCommand {
        tool: String,
        #[serde(default)]
        arguments: Vec<PlanArgument>,
        /// Ask for the pseudo console. Refused in `jean_core::planner` for a tool that does
        /// not declare `interactive = true`.
        #[serde(default)]
        interactive: Option<bool>,
        /// Tighten the tool's deadline. Shorter only: a plan may not extend how long J.E.A.N.
        /// waits beyond what the registry declared.
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Start a discovered application, by name.
    LaunchApp {
        app: String,
        #[serde(default)]
        arguments: Vec<PlanArgument>,
        /// Which candidate, when discovery finds several. A hint, not an override: an
        /// ambiguous name is still refused unless the operator has seen the list.
        #[serde(default)]
        pick: Option<usize>,
        #[serde(default)]
        expect_window: Option<bool>,
        #[serde(default)]
        window_timeout_secs: Option<u64>,
    },
    /// Read the process table. Never terminates anything.
    InspectProcess {
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        this_process: bool,
        #[serde(default)]
        list: bool,
        /// One of cpu, memory, pid — exactly what `jean_tools::SortKey` can do. Checked here and
        /// again against the sampler in the executor, because a key that does not exist must not
        /// silently fall back to the default.
        #[serde(default)]
        sort: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        sample_window_ms: Option<u64>,
    },
    /// Locate names under one explicit root. Contents are never read.
    LocateFile {
        root: String,
        pattern: String,
        #[serde(default)]
        max_results: Option<usize>,
    },
}

impl PlanOperation {
    /// The operation's name in the audit vocabulary. `jean-core` pins these four strings
    /// against `jean_policy::OperationKind::as_str`, because this crate cannot depend on
    /// `jean-policy` (it is the one `jean-policy` depends on).
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::RunCommand { .. } => "run_command",
            Self::LaunchApp { .. } => "launch_app",
            Self::InspectProcess { .. } => "inspect_process",
            Self::LocateFile { .. } => "locate_file",
        }
    }

    pub fn tool_id(&self) -> Option<&str> {
        match self {
            Self::RunCommand { tool, .. } => Some(tool),
            _ => None,
        }
    }

    /// The verifications this operation can honestly produce.
    pub fn accepted_verifications(&self) -> &'static [&'static str] {
        match self {
            Self::RunCommand { .. } => &["exit_code", "output_contains"],
            Self::LaunchApp { .. } => &["window_for_pid"],
            Self::InspectProcess { .. } => &["fields_available"],
            Self::LocateFile { .. } => &["path_readable"],
        }
    }

    pub fn arguments(&self) -> &[PlanArgument] {
        match self {
            Self::RunCommand { arguments, .. } | Self::LaunchApp { arguments, .. } => arguments,
            _ => &[],
        }
    }

    pub fn interactive(&self) -> Option<bool> {
        match self {
            Self::RunCommand { interactive, .. } => *interactive,
            _ => None,
        }
    }

    pub fn timeout_secs(&self) -> Option<u64> {
        match self {
            Self::RunCommand { timeout_secs, .. } => *timeout_secs,
            _ => None,
        }
    }

    pub fn window_check_declined(&self) -> bool {
        matches!(
            self,
            Self::LaunchApp {
                expect_window: Some(false),
                ..
            }
        )
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// Unique within the plan.
    pub id: String,
    /// One line of display text for the operator. Never executed, never an instruction, and
    /// labelled as model wording wherever it is printed.
    pub title: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub verify: DeclaredVerification,
    pub operation: PlanOperation,
}

/// A whole plan, as the model returned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Must equal [`PLAN_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The model's restatement of the goal. Stored alongside, never instead of, what the
    /// operator typed: a paraphrase must not become the instruction.
    pub goal: String,
    pub steps: Vec<PlanStep>,
}

/// Where one plan step has got to.
///
/// Not the same enum as [`crate::state::TaskState`], and not a subset of it. A step never enters
/// `awaiting_approval` — that decision belongs to the task, once, for the whole plan — and a step
/// can be `skipped`, which a task can never be. The task states answer "what is J.E.A.N. doing
/// right now"; these answer "what happened to this line of the plan", and the difference is why
/// `needs_attention` means something different in each: for a task it is a human's to resolve, for
/// a step it is the reconciler's honest reading of an effect it cannot confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepState {
    /// Accepted with the plan, not yet reached.
    Pending,
    /// Handed to the intent pipeline. A step left in this state after a crash is the case where
    /// the side effect may or may not have happened, and replay is forbidden.
    Running,
    /// Ran, and the verification it declared was found.
    Succeeded,
    /// Ran, and either failed or its declared verification was not found.
    Failed,
    /// Not executed, because something it depends on did not run.
    Skipped,
    /// Not executed, because the operator cancelled while the plan was running.
    Cancelled,
    /// Ran to completion but nothing proves the goal was met. Distinct from failed: the step did
    /// what it said, the evidence simply was not there to look for.
    Unverified,
    /// Its outcome cannot be determined from the record, and a human has to say what happened.
    NeedsAttention,
}

impl PlanStepState {
    /// Every state, in the order the schema's CHECK constraint lists them.
    pub const ALL: &'static [PlanStepState] = &[
        Self::Pending,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Skipped,
        Self::Cancelled,
        Self::Unverified,
        Self::NeedsAttention,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::Unverified => "unverified",
            Self::NeedsAttention => "needs_attention",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|state| state.as_str() == s)
    }

    /// Whether a step in this state will be attempted again. `Running` is deliberately not
    /// included: an effect that may already have happened is never re-issued automatically.
    pub fn is_reachable(self) -> bool {
        matches!(self, Self::Pending)
    }

    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

impl std::fmt::Display for PlanStepState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn shape(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Why a plan is not acceptable, as far as structure alone can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStructureError {
    /// Machine-readable code, e.g. `duplicate_id`. Part of the operator-facing output, and
    /// asserted by tests, so these names are a contract.
    pub code: &'static str,
    /// The field or step the complaint is about.
    pub location: String,
    /// What to do about it. A rejection nobody can act on is worse than no rejection.
    pub reason: String,
}

impl std::fmt::Display for PlanStructureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.location.is_empty() {
            write!(f, "[{}] {}", self.code, self.reason)
        } else {
            write!(f, "[{}] {}: {}", self.code, self.location, self.reason)
        }
    }
}

impl std::error::Error for PlanStructureError {}

impl PlanStructureError {
    /// Build one diagnostic.
    ///
    /// Public because validation continues past this crate: `jean_core::planner` reports the same
    /// shape for problems only the live registry and policy can see — an unregistered tool, a root
    /// outside the workspace — so the operator reads one list, not two vocabularies.
    pub fn new(code: &'static str, location: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            code,
            location: location.into(),
            reason: reason.into(),
        }
    }
}

fn error(
    code: &'static str,
    location: impl Into<String>,
    reason: impl Into<String>,
) -> PlanStructureError {
    PlanStructureError::new(code, location, reason)
}

/// The outcome of [`Plan::validate_structure`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructureReport {
    pub errors: Vec<PlanStructureError>,
    /// Non-fatal observations worth showing the operator — a step that declares it expects a
    /// failing exit code, for instance.
    pub notes: Vec<String>,
}

impl StructureReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Everything worth saying, one line at a time, errors first.
    pub fn diagnostics(&self) -> Vec<String> {
        let mut out: Vec<String> = self.errors.iter().map(ToString::to_string).collect();
        out.extend(self.notes.iter().cloned());
        out
    }
}

impl Plan {
    /// Parse a plan from the text a model produced.
    ///
    /// Takes a string rather than a `Value` because providers wrap structured output
    /// differently, and unwrapping that is the adapter's job. The only concession here is a
    /// complete ``` fence: removing a *whole* wrapper is unambiguous, while mining a paragraph
    /// for the last braced region would be guessing at which text was the plan. A half fence
    /// means the response was truncated, which stays a failure.
    pub fn parse(text: &str) -> Result<Self, PlanStructureError> {
        let (body, fence_closed) = strip_json_fence(text.trim());
        if body.is_empty() {
            return Err(error(
                "empty_response",
                "",
                "the model returned no text to parse",
            ));
        }
        if !fence_closed {
            return Err(error(
                "invalid_json",
                "",
                "the response opens a code fence that never closes. A plan inside a truncated \
                 response is not a plan, even if the JSON visible here happens to parse",
            ));
        }
        if !body.starts_with('{') {
            return Err(error(
                "not_an_object",
                "",
                format!(
                    "a plan must be one JSON object; the response starts with {:?}",
                    shorten(body)
                ),
            ));
        }
        let value: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            error(
                "invalid_json",
                "",
                format!("the plan is not one complete JSON object: {e}"),
            )
        })?;
        serde_json::from_value::<Plan>(value).map_err(|e| {
            error(
                "schema_mismatch",
                "",
                format!("the plan does not match schema v{PLAN_SCHEMA_VERSION}: {e}"),
            )
        })
    }

    /// Every structural problem found, not just the first.
    ///
    /// A model that produced three violations should be shown all three: that is what makes
    /// one bounded replanning attempt worth spending.
    pub fn validate_structure(&self, max_steps: usize) -> StructureReport {
        let mut report = StructureReport::default();

        if self.schema_version != PLAN_SCHEMA_VERSION {
            report.errors.push(error(
                "schema_version",
                "schema_version",
                format!(
                    "this build understands plan schema v{PLAN_SCHEMA_VERSION}; the plan \
                     declares v{}",
                    self.schema_version
                ),
            ));
        }

        let goal_len = self.goal.chars().count();
        if self.goal.trim().is_empty() {
            report.errors.push(error(
                "empty_goal",
                "goal",
                "the plan restates no goal, so there is nothing to compare the steps against",
            ));
        } else if goal_len > MAX_GOAL_LEN {
            report.errors.push(error(
                "goal_too_long",
                "goal",
                format!("the goal is {goal_len} characters; the ceiling is {MAX_GOAL_LEN}"),
            ));
        }

        if self.steps.is_empty() {
            report.errors.push(error(
                "no_steps",
                "steps",
                "an empty plan does nothing, and `jean do` will not report success for nothing. \
                 Return one step that gathers the missing information, or ask the operator to \
                 clarify the goal",
            ));
        }

        let ceiling = max_steps.min(MAX_PLAN_STEPS);
        if self.steps.len() > ceiling {
            report.errors.push(error(
                "too_many_steps",
                "steps",
                format!(
                    "{} steps exceeds the ceiling of {ceiling} (configured {max_steps}, \
                     absolute {MAX_PLAN_STEPS}). A plan this long is a workflow engine, and \
                     this build runs sequential plans only",
                    self.steps.len()
                ),
            ));
        }

        let mut first_seen: BTreeMap<String, usize> = BTreeMap::new();
        // Every id mapped to where it is declared, before any step is examined. Built up front
        // because a forward reference is not a hole in the graph: reporting it as
        // `unknown_dependency` would hide both the cycle search and the real fix.
        let mut positions: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, step) in self.steps.iter().enumerate() {
            positions.entry(step.id.as_str()).or_insert(index);
        }
        for (index, step) in self.steps.iter().enumerate() {
            if !is_valid_id(&step.id) {
                report.errors.push(error(
                    "invalid_id",
                    format!("steps[{index}].id"),
                    format!(
                        "{:?} is not a step id: use 1..{MAX_ID_LEN} characters of ASCII letters, \
                         digits, `-` or `_`, starting with a letter or digit",
                        shorten(&step.id)
                    ),
                ));
            }
            if let Some(first) = first_seen.insert(step.id.clone(), index) {
                report.errors.push(error(
                    "duplicate_id",
                    format!("steps[{index}].id"),
                    format!(
                        "id {:?} is used by step {first} and step {index}; dependencies would be \
                         ambiguous, and a plan with two meanings is not a plan",
                        step.id
                    ),
                ));
            }

            let title_len = step.title.chars().count();
            if title_len == 0 || title_len > MAX_TITLE_LEN {
                report.errors.push(error(
                    "invalid_title",
                    format!("{}.title", step.id),
                    format!(
                        "the title is display text for a human and must be 1..{MAX_TITLE_LEN} \
                         characters; it is {title_len}"
                    ),
                ));
            }

            if step.depends_on.len() > MAX_DEPENDS_PER_STEP {
                report.errors.push(error(
                    "too_many_dependencies",
                    format!("{}.depends_on", step.id),
                    format!(
                        "{} entries exceeds the limit of {MAX_DEPENDS_PER_STEP} per step",
                        step.depends_on.len()
                    ),
                ));
            }
            for dependency in &step.depends_on {
                if dependency == &step.id {
                    report.errors.push(error(
                        "self_dependency",
                        format!("{}.depends_on", step.id),
                        "a step cannot depend on itself: it would never become runnable",
                    ));
                    continue;
                }
                match positions.get(dependency.as_str()) {
                    // Checked here as well as after the loop so the message can name the
                    // position, and so a cycle search never runs on a graph with holes.
                    None => report.errors.push(error(
                        "unknown_dependency",
                        format!("{}.depends_on", step.id),
                        format!("no step declares id {dependency:?}"),
                    )),
                    Some(&at) if at >= index => {
                        report.errors.push(forward_dependency(step, dependency))
                    }
                    Some(_) => {}
                }
            }

            self.check_operation_bounds(&mut report, step);
            self.check_arguments(&mut report, step);
            self.check_verification(&mut report, step);
        }

        let graph_unreadable = report.errors.iter().any(|e| {
            matches!(
                e.code,
                "invalid_id" | "duplicate_id" | "unknown_dependency" | "self_dependency"
            )
        });
        if !graph_unreadable {
            if let Some(cycle) = find_cycle(&self.steps) {
                report.errors.push(error(
                    "dependency_cycle",
                    cycle.join(" -> "),
                    "the dependency graph contains a cycle, so no execution order exists. A \
                     sequential plan must be a directed acyclic graph",
                ));
            }
        }

        report
    }

    /// Bounds that live inside an operation rather than alongside it.
    fn check_operation_bounds(&self, report: &mut StructureReport, step: &PlanStep) {
        match &step.operation {
            PlanOperation::LocateFile { root, pattern, .. } => {
                if root.trim().is_empty() {
                    report.errors.push(error(
                        "missing_root",
                        format!("{}.operation.root", step.id),
                        "a search needs an explicit root; this build will not pick one, because \
                         the root is the privacy boundary",
                    ));
                }
                let len = pattern.chars().count();
                if len == 0 || len > MAX_PATTERN_LEN {
                    report.errors.push(error(
                        "invalid_pattern",
                        format!("{}.operation.pattern", step.id),
                        format!("the pattern must be 1..{MAX_PATTERN_LEN} characters; it is {len}"),
                    ));
                }
            }
            PlanOperation::LaunchApp { app, .. } => {
                let len = app.trim().chars().count();
                if len == 0 || len > MAX_ARGUMENT_LEN {
                    report.errors.push(error(
                        "invalid_app_name",
                        format!("{}.operation.app", step.id),
                        format!("the application name must be 1..{MAX_ARGUMENT_LEN} characters"),
                    ));
                }
            }
            PlanOperation::InspectProcess {
                pid,
                name,
                this_process,
                list,
                sort,
                ..
            } => {
                let selectors = [pid.is_some(), name.is_some(), *this_process, *list];
                if selectors.iter().filter(|s| **s).count() > 1 {
                    report.errors.push(error(
                        "conflicting_selectors",
                        format!("{}.operation", step.id),
                        "the step gives more than one way to choose a process; name exactly one \
                         of pid, name, this_process or list",
                    ));
                }
                if let Some(key) = sort {
                    if !matches!(key.as_str(), "cpu" | "memory" | "pid") {
                        report.errors.push(error(
                            "invalid_sort_key",
                            format!("{}.operation.sort", step.id),
                            format!("{key:?} is not a sort key; the sampler sorts by cpu, memory or pid"),
                        ));
                    }
                }
            }
            PlanOperation::RunCommand { tool, .. } => {
                if tool.trim().is_empty() {
                    report.errors.push(error(
                        "missing_tool",
                        format!("{}.operation.tool", step.id),
                        "a run_command step must name a registered tool id",
                    ));
                }
            }
        }
    }

    fn check_arguments(&self, report: &mut StructureReport, step: &PlanStep) {
        let arguments = step.operation.arguments();
        if arguments.len() > MAX_ARGUMENTS_PER_STEP {
            report.errors.push(error(
                "too_many_arguments",
                format!("{}.arguments", step.id),
                format!(
                    "{} arguments exceeds the limit of {MAX_ARGUMENTS_PER_STEP}",
                    arguments.len()
                ),
            ));
        }

        let mut named: BTreeSet<String> = BTreeSet::new();
        let mut unnamed_seen = false;
        for (index, argument) in arguments.iter().enumerate() {
            let at = || format!("{}.arguments[{index}]", step.id);
            match &argument.name {
                Some(name) => {
                    if name.trim().is_empty() {
                        report.errors.push(error(
                            "blank_argument_name",
                            at(),
                            "an argument name must not be blank; name it or omit the field",
                        ));
                    } else if !named.insert(name.clone()) {
                        report.errors.push(error(
                            "duplicate_argument_name",
                            at(),
                            format!("{name:?} is supplied twice, and only one value can be passed"),
                        ));
                    }
                }
                None => {
                    if unnamed_seen {
                        report.errors.push(error(
                            "positional_after_positional",
                            at(),
                            "two arguments without a name have no determinable order. Either \
                             name them or pass one",
                        ));
                    }
                    unnamed_seen = true;
                }
            }

            match argument.to_value() {
                Err(reason) => report
                    .errors
                    .push(error("invalid_argument_type", at(), reason)),
                Ok(value) => {
                    let rendered = value.render();
                    let len = rendered.chars().count();
                    if len > MAX_ARGUMENT_LEN {
                        report.errors.push(error(
                            "argument_too_long",
                            at(),
                            format!(
                                "{len} characters exceeds the limit of {MAX_ARGUMENT_LEN}. A plan \
                                 is not a channel for a script body"
                            ),
                        ));
                    }
                    if rendered.contains('\0') {
                        report.errors.push(error(
                            "argument_contains_nul",
                            at(),
                            "an argument may not contain a NUL byte; it cannot reach an argument \
                             vector intact",
                        ));
                    }
                    if matches!(value, ArgValue::Text(_))
                        && (rendered.contains('\n') || rendered.contains('\r'))
                    {
                        report.notes.push(format!(
                            "step {}: argument {index} contains a line break. That is legal in an \
                             argument vector and unreadable in a plan display",
                            step.id
                        ));
                    }
                }
            }
        }
    }

    fn check_verification(&self, report: &mut StructureReport, step: &PlanStep) {
        let declared = step.verify.as_str();
        let accepted = step.operation.accepted_verifications();
        if !accepted.contains(&declared) {
            report.errors.push(error(
                "verification_mismatch",
                format!("{}.verify", step.id),
                format!(
                    "a {} step cannot be proven by {declared:?}; what this operation can observe \
                     is {}",
                    step.operation.kind_name(),
                    accepted.join(" or ")
                ),
            ));
        }

        if let Some(needle) = step.verify.needle() {
            if needle.is_empty() {
                report.errors.push(error(
                    "empty_needle",
                    format!("{}.verify.needle", step.id),
                    "an empty needle is contained in every output, so it proves nothing",
                ));
            } else if needle.chars().count() > MAX_PATTERN_LEN {
                report.errors.push(error(
                    "needle_too_long",
                    format!("{}.verify.needle", step.id),
                    format!(
                        "the needle is {len} characters; the ceiling for a probe is \
                         {MAX_PATTERN_LEN}. Something that long is a payload, not a check",
                        len = needle.chars().count()
                    ),
                ));
            }
        }

        if let Some(expected) = step.verify.expected_exit() {
            if expected != 0 {
                report.notes.push(format!(
                    "step {}: declares it expects exit code {expected}, not 0 — the run is called \
                     verified if the child fails that way",
                    step.id
                ));
            }
        }

        // A step that turns off the only probe it could have, then claims that probe as proof.
        if step.operation.window_check_declined() {
            report.errors.push(error(
                "verification_without_a_probe",
                format!("{}.verify", step.id),
                "the step declines the window check and then declares the window check as its \
                 proof. Either expect a window, or declare no verification path",
            ));
        }
    }
}

/// True for ids that are safe to print, embed in a message, and compare.
fn is_valid_id(id: &str) -> bool {
    let chars: Vec<char> = id.chars().collect();
    if chars.is_empty() || chars.len() > MAX_ID_LEN {
        return false;
    }
    chars[0].is_ascii_alphanumeric()
        && chars
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
}

/// A cycle in the dependency graph, returned as the offending path.
///
/// "There is a cycle" is a much worse diagnostic than "s1 -> s2 -> s1", so the path is built
/// rather than merely detected.
fn find_cycle(steps: &[PlanStep]) -> Option<Vec<String>> {
    let index: BTreeMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| (step.id.as_str(), i))
        .collect();
    // 0 = unvisited, 1 = on the current path, 2 = settled.
    let mut state = vec![0u8; steps.len()];
    let mut path: Vec<usize> = Vec::new();
    for start in 0..steps.len() {
        if state[start] == 0 {
            if let Some(cycle) = walk(steps, &index, &mut state, &mut path, start) {
                return Some(cycle);
            }
        }
    }
    None
}

fn walk(
    steps: &[PlanStep],
    index: &BTreeMap<&str, usize>,
    state: &mut [u8],
    path: &mut Vec<usize>,
    node: usize,
) -> Option<Vec<String>> {
    state[node] = 1;
    path.push(node);
    for dependency in &steps[node].depends_on {
        let Some(&next) = index.get(dependency.as_str()) else {
            continue;
        };
        match state[next] {
            1 => {
                let begin = path.iter().position(|n| *n == next).unwrap_or(0);
                let mut cycle: Vec<String> =
                    path[begin..].iter().map(|n| steps[*n].id.clone()).collect();
                cycle.push(steps[next].id.clone());
                return Some(cycle);
            }
            0 => {
                if let Some(found) = walk(steps, index, state, path, next) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    state[node] = 2;
    path.pop();
    None
}

/// Remove one complete ``` fence, with its language tag, and report whether a fence that
/// opened also closed.
///
/// An unclosed fence stays a failure. The wrapper is evidence about the response as a whole,
/// and a plan that lost its closing fence lost something else too — so a body that happens to
/// parse anyway is precisely the case this build must not paper over.
fn strip_json_fence(text: &str) -> (&str, bool) {
    let body = text.trim();
    if !body.starts_with("```") {
        return (body, true);
    }
    let Some(newline) = body.find('\n') else {
        return ("", false);
    };
    let body = body[newline + 1..].trim_start();
    match body.rfind("```") {
        Some(end) => (body[..end].trim_end(), true),
        None => (body, false),
    }
}

/// The one wording for "that dependency is declared later, or here".
///
/// Shared by structure validation and [`execution_order`] so the operator cannot be told two
/// different things about the same plan.
fn forward_dependency(step: &PlanStep, dependency: &str) -> PlanStructureError {
    error(
        "forward_dependency",
        format!("{}.depends_on", step.id),
        format!(
            "{dependency:?} is declared at or after this step, and this build runs steps in the \
             order they are declared rather than reordering them. Put the dependency first."
        ),
    )
}

fn shorten(text: &str) -> String {
    text.chars().take(80).collect()
}

/// The order a plan will run in: declaration order.
///
/// A topological sort that silently reordered steps would mean the operator previewed one
/// sequence and ran another. So a step that depends on one listed *after* it is a plan error,
/// reported with the fix, rather than something tidied up behind the operator's back.
pub fn execution_order(plan: &Plan) -> Result<Vec<String>, PlanStructureError> {
    let position: BTreeMap<&str, usize> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| (step.id.as_str(), i))
        .collect();
    for step in &plan.steps {
        let here = position.get(step.id.as_str()).copied().unwrap_or(0);
        for dependency in &step.depends_on {
            if let Some(&at) = position.get(dependency.as_str()) {
                if at >= here {
                    return Err(forward_dependency(step, dependency));
                }
            }
        }
    }
    Ok(plan.steps.iter().map(|step| step.id.clone()).collect())
}

/// Ids in run order, falling back to declaration order.
///
/// The fallback is only reached by a display path that has already reported the error.
pub fn order_ids(plan: &Plan) -> Vec<String> {
    execution_order(plan)
        .unwrap_or_else(|_| plan.steps.iter().map(|step| step.id.clone()).collect())
}

/// The plan as it is shown to the operator: two lines per step, in run order.
///
/// Built here so the preview and the executed plan cannot disagree about what the plan said.
/// Titles are model text and are rendered as such — nothing here is an instruction to the
/// executor.
pub fn render_plan(plan: &Plan, order: &[String]) -> Vec<String> {
    let by_id: BTreeMap<&str, &PlanStep> = plan
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    let mut out = Vec::new();
    for (position, id) in order.iter().enumerate() {
        let Some(step) = by_id.get(id.as_str()) else {
            continue;
        };
        out.push(format!(
            "{}. [{}] {} — {}",
            position + 1,
            step.id,
            step.title,
            describe_operation(&step.operation)
        ));
        out.push(format!(
            "      verify: {} · depends on: {}",
            describe_verification(&step.verify),
            if step.depends_on.is_empty() {
                "nothing".to_owned()
            } else {
                step.depends_on.join(", ")
            }
        ));
    }
    out
}

/// The probe with the value it looks for, so a preview shows what will actually be checked.
///
/// The needle is model text and part of the plan the operator approved, so it is displayed; a
/// reader who sees only `output_contains` cannot tell a real check from a placeholder.
fn describe_verification(verify: &DeclaredVerification) -> String {
    match verify.needle() {
        Some(needle) => format!("{} {needle:?}", verify.as_str()),
        None => verify.as_str().to_owned(),
    }
}

fn describe_operation(operation: &PlanOperation) -> String {
    match operation {
        PlanOperation::RunCommand {
            tool,
            arguments,
            interactive,
            timeout_secs,
        } => {
            let mut text = format!("run registered tool `{tool}`");
            if !arguments.is_empty() {
                text.push_str(&format!(" with {}", render_arguments(arguments)));
            }
            if matches!(interactive, Some(true)) {
                text.push_str(" in a pseudo console");
            }
            if let Some(secs) = timeout_secs {
                text.push_str(&format!(" (deadline {secs}s)"));
            }
            text
        }
        PlanOperation::LaunchApp {
            app,
            arguments,
            pick,
            expect_window,
            window_timeout_secs,
        } => {
            let mut text = format!("launch application `{app}`");
            if !arguments.is_empty() {
                text.push_str(&format!(" with {}", render_arguments(arguments)));
            }
            if let Some(number) = pick {
                text.push_str(&format!(" (candidate #{number})"));
            }
            if matches!(expect_window, Some(false)) {
                text.push_str(" — window check declined");
            } else if let Some(secs) = window_timeout_secs {
                text.push_str(&format!(" (window within {secs}s)"));
            }
            text
        }
        PlanOperation::InspectProcess {
            pid,
            name,
            this_process,
            list,
            sort,
            limit,
            sample_window_ms,
        } => {
            let subject = match (pid, name, this_process, list) {
                (Some(number), _, _, _) => format!("process {number}"),
                (_, Some(given), _, _) => format!("processes named `{given}`"),
                (_, _, true, _) => "this process".to_owned(),
                _ => "the process table".to_owned(),
            };
            let mut text = format!("inspect {subject}");
            let mut qualifiers = Vec::new();
            if let Some(key) = sort {
                qualifiers.push(format!("sorted by {key}"));
            }
            if let Some(rows) = limit {
                qualifiers.push(format!("at most {rows} rows"));
            }
            if let Some(ms) = sample_window_ms {
                qualifiers.push(format!("sampled over {ms}ms"));
            }
            if !qualifiers.is_empty() {
                text.push_str(&format!(" ({})", qualifiers.join(", ")));
            }
            text
        }
        PlanOperation::LocateFile {
            root,
            pattern,
            max_results,
        } => {
            let mut text = format!("find `{pattern}` under `{root}`");
            if let Some(count) = max_results {
                text.push_str(&format!(" (at most {count} results)"));
            }
            text
        }
    }
}

fn render_arguments(arguments: &[PlanArgument]) -> String {
    let parts: Vec<String> = arguments
        .iter()
        .map(|argument| {
            let value = shorten(&argument.value.to_string());
            match &argument.name {
                Some(name) => format!("{name}:{}:{value}", argument.arg_type),
                None => format!("{}:{value}", argument.arg_type),
            }
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest plan that means anything: one command step, verified by exit code.
    fn minimal() -> Plan {
        parse_ok(
            r#"{"schema_version":1,"goal":"get the thing done","steps":[
                 {"id":"s1","title":"echo once","verify":{"method":"exit_code","expected":0},
                  "operation":{"kind":"run_command","tool":"echo.tool"}}]}"#,
        )
    }

    fn parse_ok(text: &str) -> Plan {
        Plan::parse(text).unwrap_or_else(|e| panic!("this fixture must parse: {e}\n{text}"))
    }

    fn codes(plan: &Plan) -> Vec<&'static str> {
        plan.validate_structure(MAX_PLAN_STEPS)
            .errors
            .iter()
            .map(|error| error.code)
            .collect()
    }

    fn find(plan: &Plan, code: &str) -> Option<PlanStructureError> {
        plan.validate_structure(MAX_PLAN_STEPS)
            .errors
            .into_iter()
            .find(|error| error.code == code)
    }

    #[test]
    fn the_minimal_plan_parses_and_validates() {
        let plan = minimal();
        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.validate_structure(MAX_PLAN_STEPS).is_valid());
        assert_eq!(plan.steps[0].operation.kind_name(), "run_command");
        assert_eq!(plan.steps[0].operation.tool_id(), Some("echo.tool"));
        assert_eq!(plan.steps[0].verify.as_str(), "exit_code");
        assert_eq!(plan.steps[0].verify.expected_exit(), Some(0));
        assert!(plan.steps[0].operation.arguments().is_empty());
    }

    #[test]
    fn a_fenced_response_is_unwrapped_and_prose_around_the_object_is_not() {
        // A complete fence is a delivery detail. Mining a paragraph for "the braced region" would
        // be guessing at which text was the plan, so a chatty reply stays a failure.
        let body = r#"{"schema_version":1,"goal":"g","steps":[{"id":"s1","title":"t","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"x"}}]}"#;
        let fenced = format!("```json\n{body}\n```");
        assert_eq!(
            Plan::parse(&fenced).expect("fence unwrapped").steps.len(),
            1
        );

        let chatty = format!("Here is your plan:\n{body}\nHope that helps!");
        assert_eq!(Plan::parse(&chatty).unwrap_err().code, "not_an_object");

        let half_open = format!("```json\n{body}");
        assert_eq!(Plan::parse(&half_open).unwrap_err().code, "invalid_json");
    }

    #[test]
    fn an_empty_or_malformed_reply_is_reported_as_such() {
        assert_eq!(Plan::parse("").unwrap_err().code, "empty_response");
        assert_eq!(
            Plan::parse(r#"{"schema_version":1,"#).unwrap_err().code,
            "invalid_json"
        );
        assert_eq!(Plan::parse("[1,2,3]").unwrap_err().code, "not_an_object");
    }

    #[test]
    fn a_field_the_schema_does_not_have_is_refused_not_ignored() {
        // This is the load-bearing test for "the model cannot invent an executable path": there is
        // no `command` field to put one in, and an answer that tries is rejected rather than
        // quietly dropping the field the model thought it had set.
        let with_command = r#"{"schema_version":1,"goal":"g","steps":[{"id":"s1","title":"t",
            "command":"notepad.exe","verify":{"method":"exit_code"},
            "operation":{"kind":"run_command","tool":"x"}}]}"#;
        let error = Plan::parse(with_command).unwrap_err();
        assert_eq!(error.code, "schema_mismatch", "{error:?}");
        assert!(error.reason.contains("command"), "{}", error.reason);

        let with_shell = r#"{"schema_version":1,"goal":"g","steps":[{"id":"s1","title":"t",
            "verify":{"method":"exit_code"},
            "operation":{"kind":"run_command","tool":"x","shell":true}}]}"#;
        assert_eq!(Plan::parse(with_shell).unwrap_err().code, "schema_mismatch");

        // A verification carrying a field its own method does not define.
        let wrong_verify = r#"{"schema_version":1,"goal":"g","steps":[{"id":"s1","title":"t",
            "verify":{"method":"exit_code","needle":"x"},
            "operation":{"kind":"run_command","tool":"x"}}]}"#;
        assert_eq!(
            Plan::parse(wrong_verify).unwrap_err().code,
            "schema_mismatch"
        );
    }

    #[test]
    fn an_unknown_operation_kind_is_a_schema_failure() {
        let plan = r#"{"schema_version":1,"goal":"g","steps":[{"id":"s1","title":"t",
            "verify":{"method":"exit_code"},
            "operation":{"kind":"send_email","to":"someone"}}]}"#;
        assert_eq!(Plan::parse(plan).unwrap_err().code, "schema_mismatch");
    }

    #[test]
    fn a_plan_declaring_another_schema_version_is_refused_rather_than_reinterpreted() {
        let mut plan = minimal();
        plan.schema_version = 2;
        let problem = find(&plan, "schema_version").expect("v2 must be refused");
        assert!(
            problem.reason.contains("v1") || problem.reason.contains("v2"),
            "{}",
            problem.reason
        );
    }

    #[test]
    fn the_goal_is_required_bounded_and_kept_separate_from_the_instruction() {
        let mut plan = minimal();
        plan.goal = "   ".to_owned();
        assert!(codes(&plan).contains(&"empty_goal"));

        plan.goal = "x".repeat(MAX_GOAL_LEN + 1);
        assert!(codes(&plan).contains(&"goal_too_long"));
    }

    #[test]
    fn a_plan_with_no_steps_is_a_failure_not_a_successful_no_op() {
        let plan = parse_ok(r#"{"schema_version":1,"goal":"nothing to do","steps":[]}"#);
        assert!(codes(&plan).contains(&"no_steps"));
    }

    #[test]
    fn step_ids_are_unique_short_and_pipe_safe() {
        let duplicate = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"a","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"x"}},
                 {"id":"s1","title":"b","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"y"}}]}"#,
        );
        let problem = find(&duplicate, "duplicate_id").expect("a repeated id must be found");
        assert!(problem.reason.contains("s1"), "{}", problem.reason);

        for bad in ["s 1", "../s1", "s|1", "", &"s".repeat(MAX_ID_LEN + 1)] {
            let mut plan = minimal();
            plan.steps[0].id = bad.to_owned();
            assert!(
                codes(&plan).contains(&"invalid_id"),
                "id {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_title_is_display_text_and_is_bounded_as_one() {
        let mut plan = minimal();
        plan.steps[0].title = "t".repeat(MAX_TITLE_LEN + 1);
        assert!(codes(&plan).contains(&"invalid_title"));

        plan.steps[0].title = String::new();
        assert!(codes(&plan).contains(&"invalid_title"));
    }

    #[test]
    fn a_verification_is_mandatory_and_must_be_something_the_operation_can_observe() {
        // Missing verification is a serde failure, so there is no default that means "unchecked".
        let missing = r#"{"schema_version":1,"goal":"g","steps":[{"id":"s1","title":"t",
            "operation":{"kind":"run_command","tool":"x"}}]}"#;
        assert_eq!(Plan::parse(missing).unwrap_err().code, "schema_mismatch");

        // A launch verified by an exit code would be a claim nothing can support.
        let wrong = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"open it","verify":{"method":"exit_code","expected":0},
                  "operation":{"kind":"launch_app","app":"notepad"}}]}"#,
        );
        let problem =
            find(&wrong, "verification_mismatch").expect("a launch cannot report an exit code");
        assert!(
            problem.reason.contains("window_for_pid"),
            "the diagnostic must say what would fit: {}",
            problem.reason
        );

        // The four kinds and the probes each can honestly produce.
        assert_eq!(
            PlanOperation::RunCommand {
                tool: "x".to_owned(),
                arguments: vec![],
                interactive: None,
                timeout_secs: None,
            }
            .accepted_verifications(),
            &["exit_code", "output_contains"]
        );
        assert_eq!(
            PlanOperation::LaunchApp {
                app: "x".to_owned(),
                arguments: vec![],
                pick: None,
                expect_window: None,
                window_timeout_secs: None,
            }
            .accepted_verifications(),
            &["window_for_pid"]
        );
        assert_eq!(
            PlanOperation::InspectProcess {
                pid: None,
                name: None,
                this_process: true,
                list: false,
                sort: None,
                limit: None,
                sample_window_ms: None,
            }
            .accepted_verifications(),
            &["fields_available"]
        );
        assert_eq!(
            PlanOperation::LocateFile {
                root: "C:\\src".to_owned(),
                pattern: "*.rs".to_owned(),
                max_results: None,
            }
            .accepted_verifications(),
            &["path_readable"]
        );
    }

    #[test]
    fn a_dependency_cycle_is_reported_with_the_path_that_closes_it() {
        let mut plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"a","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"x"}},
                 {"id":"s2","title":"b","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"y"}}]}"#,
        );
        plan.steps[0].depends_on = vec!["s2".to_owned()];
        plan.steps[1].depends_on = vec!["s1".to_owned()];

        let problem = find(&plan, "dependency_cycle").expect("a cycle must be found");
        // The location is the cycle itself, so a reader can see the loop without redrawing it.
        assert!(
            problem.location.contains("s1 -> s2"),
            "{}",
            problem.location
        );
    }

    #[test]
    fn a_self_dependency_and_an_unknown_dependency_are_both_named() {
        let mut plan = minimal();
        plan.steps[0].depends_on = vec!["s1".to_owned()];
        assert!(codes(&plan).contains(&"self_dependency"));

        plan.steps[0].depends_on = vec!["s9".to_owned()];
        let found = codes(&plan);
        assert!(found.contains(&"unknown_dependency"), "{found:?}");
        // The cycle search is skipped when the graph has holes, so a hole is the only report.
        assert!(!found.contains(&"dependency_cycle"), "{found:?}");
    }

    #[test]
    fn a_dependency_on_a_later_step_is_reported_rather_than_silently_reordered() {
        // Execution follows declaration order. Reordering would mean the operator previewed one
        // plan and a different one ran, so the forward reference is an error instead.
        let mut plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"a","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"x"}},
                 {"id":"s2","title":"b","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"y"}}]}"#,
        );
        plan.steps[0].depends_on = vec!["s2".to_owned()];
        assert!(codes(&plan).contains(&"forward_dependency"));

        // The reverse, which is what a plan should say, is valid.
        plan.steps[0].depends_on = Vec::new();
        plan.steps[1].depends_on = vec!["s1".to_owned()];
        assert!(plan.validate_structure(MAX_PLAN_STEPS).is_valid());
    }

    #[test]
    fn too_many_dependencies_for_one_step_is_refused() {
        let mut plan = minimal();
        plan.steps[0].depends_on = vec!["a".to_owned(); MAX_DEPENDS_PER_STEP + 1];
        let report = plan.validate_structure(MAX_PLAN_STEPS);
        assert!(report
            .errors
            .iter()
            .any(|e| e.code == "too_many_dependencies"));
    }

    #[test]
    fn the_step_ceiling_is_absolute_and_configuration_may_only_tighten_it() {
        let many: Vec<String> = (0..MAX_PLAN_STEPS + 1)
            .map(|i| {
                format!(
                    r#"{{"id":"s{i}","title":"t","verify":{{"method":"exit_code"}},
                        "operation":{{"kind":"run_command","tool":"x"}}}}"#
                )
            })
            .collect();
        let plan = parse_ok(&format!(
            r#"{{"schema_version":1,"goal":"g","steps":[{}]}}"#,
            many.join(",")
        ));
        assert_eq!(plan.steps.len(), MAX_PLAN_STEPS + 1);
        assert!(codes(&plan).contains(&"too_many_steps"));

        // A tighter configuration ceiling is honoured.
        assert!(plan
            .validate_structure(2)
            .errors
            .iter()
            .any(|e| e.code == "too_many_steps"));
    }

    #[test]
    fn arguments_are_typed_and_the_message_names_the_argument_that_is_wrong() {
        let plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"name":"target","type":"path","value":"C:\\dir"},
                    {"name":"count","type":"integer","value":7},
                    {"name":"force","type":"boolean","value":true},
                    {"type":"pid","value":4242}]}}]}"#,
        );
        assert!(plan.validate_structure(MAX_PLAN_STEPS).is_valid());
        let arguments = plan.steps[0].operation.arguments();
        assert_eq!(arguments.len(), 4);
        assert_eq!(arguments[0].name.as_deref(), Some("target"));
        assert_eq!(arguments[0].to_value().unwrap().type_name(), "path");
        assert!(matches!(
            arguments[1].to_value().unwrap(),
            ArgValue::Integer(7)
        ));
        assert_eq!(
            arguments[3].to_value().unwrap().render(),
            "4242",
            "a pid travels as its number"
        );

        // The shape a model produces most often: a declared type and a different value.
        let mismatched = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"name":"count","type":"integer","value":"seven"}]}}]}"#,
        );
        let problem =
            find(&mismatched, "invalid_argument_type").expect("\"seven\" is not an integer");
        assert!(
            problem.reason.contains("count"),
            "the diagnostic must name the argument: {}",
            problem.reason
        );

        let unknown_type = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"type":"script","value":"rm -rf"}]}}]}"#,
        );
        assert!(codes(&unknown_type).contains(&"invalid_argument_type"));
    }

    #[test]
    fn two_positional_arguments_have_no_determinable_order() {
        let plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"type":"text","value":"a"},{"type":"text","value":"b"}]}}]}"#,
        );
        assert!(codes(&plan).contains(&"positional_after_positional"));

        let named = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"name":"first","type":"text","value":"a"},
                    {"name":"second","type":"text","value":"b"}]}}]}"#,
        );
        assert!(named.validate_structure(MAX_PLAN_STEPS).is_valid());
    }

    #[test]
    fn argument_names_must_be_meaningful_and_unique() {
        let blank = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"name":"  ","type":"text","value":"a"}]}}]}"#,
        );
        assert!(codes(&blank).contains(&"blank_argument_name"));

        let twice = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","arguments":[
                    {"name":"path","type":"text","value":"a"},
                    {"name":"path","type":"text","value":"b"}]}}]}"#,
        );
        assert!(codes(&twice).contains(&"duplicate_argument_name"));
    }

    #[test]
    fn an_argument_cannot_become_a_script_body() {
        let big = "x".repeat(MAX_ARGUMENT_LEN + 1);
        let plan = parse_ok(&format!(
            r#"{{"schema_version":1,"goal":"g","steps":[
                 {{"id":"s1","title":"t","verify":{{"method":"exit_code"}},
                  "operation":{{"kind":"run_command","tool":"x","arguments":[
                    {{"name":"blob","type":"text","value":"{big}"}}]}}}}]}}"#
        ));
        let problem = find(&plan, "argument_too_long").expect("2 KB is not an argument");
        assert!(
            problem.reason.contains("script body"),
            "the diagnostic must say why: {}",
            problem.reason
        );

        // More arguments than the ceiling, each individually fine: the bound is on the step.
        let arguments: Vec<String> = (0..MAX_ARGUMENTS_PER_STEP + 1)
            .map(|i| format!(r#"{{"name":"a{i}","type":"text","value":"v"}}"#))
            .collect();
        let crowded = parse_ok(&format!(
            r#"{{"schema_version":1,"goal":"g","steps":[
                 {{"id":"s1","title":"t","verify":{{"method":"exit_code"}},
                  "operation":{{"kind":"run_command","tool":"x","arguments":[{}]}}}}]}}"#,
            arguments.join(",")
        ));
        assert!(codes(&crowded).contains(&"too_many_arguments"));
    }

    #[test]
    fn an_argument_may_not_contain_a_nul_byte() {
        // A NUL truncates the wide string the child receives, so it cannot be carried at all.
        // The fixture escapes it the way a model would: a *raw* control byte never reaches this
        // check because the JSON layer refuses it first, while `\u0000` is legal JSON and
        // decodes to exactly the byte that cannot survive an argument vector.
        let escaped = r"a\u0000b";
        let plan = parse_ok(&format!(
            r#"{{"schema_version":1,"goal":"g","steps":[
                 {{"id":"s1","title":"t","verify":{{"method":"exit_code"}},
                  "operation":{{"kind":"run_command","tool":"x","arguments":[
                    {{"name":"n","type":"text","value":"{escaped}"}}]}}}}]}}"#
        ));
        let problem = find(&plan, "argument_contains_nul").expect("\\u0000 decodes to a NUL");
        assert!(
            problem.reason.contains("NUL"),
            "the diagnostic must say why: {}",
            problem.reason
        );

        // The raw byte is refused one layer down, which is also a refusal, not a pass.
        let raw: String = format!("a{}b", '\u{0}');
        let error = Plan::parse(&format!(
            r#"{{"schema_version":1,"goal":"g","steps":[
                 {{"id":"s1","title":"t","verify":{{"method":"exit_code"}},
                  "operation":{{"kind":"run_command","tool":"x","arguments":[
                    {{"name":"n","type":"text","value":"{raw}"}}]}}}}]}}"#
        ))
        .expect_err("a raw control byte is not valid JSON");
        assert_eq!(error.code, "invalid_json", "{error:?}");
    }

    #[test]
    fn a_blank_needle_proves_nothing_and_an_oversized_one_is_not_a_needle() {
        let blank = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"output_contains","needle":""},
                  "operation":{"kind":"run_command","tool":"x"}}]}"#,
        );
        assert!(codes(&blank).contains(&"empty_needle"));

        let long = "n".repeat(MAX_PATTERN_LEN + 1);
        let oversized = parse_ok(&format!(
            r#"{{"schema_version":1,"goal":"g","steps":[
                 {{"id":"s1","title":"t","verify":{{"method":"output_contains","needle":"{long}"}},
                  "operation":{{"kind":"run_command","tool":"x"}}}}]}}"#
        ));
        assert!(codes(&oversized).contains(&"needle_too_long"));

        // A needle is read out of the captured output, so it must be a probe and not a payload.
        assert_eq!(
            DeclaredVerification::OutputContains {
                expected: 0,
                needle: "ran".to_owned()
            }
            .needle(),
            Some("ran")
        );
        assert_eq!(DeclaredVerification::WindowForPid.needle(), None);
    }

    #[test]
    fn every_operation_needs_the_fields_that_make_it_executable() {
        // Omitting `tool` entirely is a schema failure, so the field is required at the parse
        // layer; an empty one is what a model that had nothing to name actually produces, and
        // that is what structure validation refuses.
        let no_tool = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"","arguments":[]}}]}"#,
        );
        let problem = find(&no_tool, "missing_tool").expect("a step must name a tool");
        assert!(
            problem.reason.contains("registered tool"),
            "{}",
            problem.reason
        );

        let absent_tool = r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","arguments":[]}}]}"#;
        assert_eq!(
            Plan::parse(absent_tool).unwrap_err().code,
            "schema_mismatch"
        );

        let no_root = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"path_readable"},
                  "operation":{"kind":"locate_file","root":"","pattern":"*.rs"}}]}"#,
        );
        assert!(codes(&no_root).contains(&"missing_root"));

        let empty_pattern = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"path_readable"},
                  "operation":{"kind":"locate_file","root":"C:\\src","pattern":""}}]}"#,
        );
        assert!(codes(&empty_pattern).contains(&"invalid_pattern"));

        let two_selectors = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"fields_available"},
                  "operation":{"kind":"inspect_process","pid":42,"name":"cmd.exe"}}]}"#,
        );
        assert!(codes(&two_selectors).contains(&"conflicting_selectors"));

        // A sort key that the sampler cannot honour is refused here rather than defaulted later.
        for bad in ["disk", "name"] {
            let plan = parse_ok(&format!(
                r#"{{"schema_version":1,"goal":"g","steps":[
                     {{"id":"s1","title":"t","verify":{{"method":"fields_available"}},
                      "operation":{{"kind":"inspect_process","list":true,"sort":"{bad}"}}}}]}}"#
            ));
            assert!(
                codes(&plan).contains(&"invalid_sort_key"),
                "{bad} is not a sort key the sampler implements"
            );
        }

        let no_app = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"window_for_pid"},
                  "operation":{"kind":"launch_app","app":"   "}}]}"#,
        );
        assert!(codes(&no_app).contains(&"invalid_app_name"));
    }

    #[test]
    fn a_step_that_declines_its_only_probe_is_refused_before_it_runs() {
        // D-012: an absent check is a bug, not a weaker claim. A launch whose window check is
        // switched off has left itself with the one proof it says it will use, so the plan is
        // refused rather than run and reported later as unverified.
        let plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"exit_code"},
                  "operation":{"kind":"run_command","tool":"x","interactive":true}}]}"#,
        );
        assert!(plan.validate_structure(MAX_PLAN_STEPS).is_valid());
        assert_eq!(plan.steps[0].operation.interactive(), Some(true));

        let declined = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"window_for_pid"},
                  "operation":{"kind":"launch_app","app":"helper","expect_window":false}}]}"#,
        );
        assert!(declined.steps[0].operation.window_check_declined());
        let report = declined.validate_structure(MAX_PLAN_STEPS);
        assert!(!report.is_valid(), "{report:?}");
        let problem = find(&declined, "verification_without_a_probe").expect("a declined probe");
        assert!(
            problem.reason.contains("window"),
            "the diagnostic must name the probe: {}",
            problem.reason
        );

        // The same step with the probe switched on is executable and stays valid.
        let expected = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"t","verify":{"method":"window_for_pid"},
                  "operation":{"kind":"launch_app","app":"helper","expect_window":true}}]}"#,
        );
        assert!(expected.validate_structure(MAX_PLAN_STEPS).is_valid());
    }

    #[test]
    fn the_verification_maps_onto_the_verifier_the_pipeline_already_uses() {
        // A plan does not get a second notion of "verified". These two mappings are what let
        // jean-core hand the declaration to `jean_core::verify::verify`.
        assert_eq!(
            DeclaredVerification::ExitCode { expected: 3 }.to_verify_spec(),
            Some(crate::tool::VerifySpec::ExitCode { expected: 3 })
        );
        assert_eq!(
            DeclaredVerification::OutputContains {
                expected: 0,
                needle: "ran".to_owned()
            }
            .to_verify_spec(),
            Some(crate::tool::VerifySpec::OutputContains {
                expected: 0,
                needle: "ran".to_owned(),
                // Case-sensitive by default: a plan cannot weaken the check it proposed.
                case_insensitive: false,
            })
        );
        for probe in [
            DeclaredVerification::WindowForPid,
            DeclaredVerification::FieldsAvailable,
            DeclaredVerification::PathReadable,
        ] {
            assert_eq!(
                probe.to_verify_spec(),
                None,
                "evidence that is not a child process has no VerifySpec"
            );
        }
    }

    #[test]
    fn execution_order_is_declaration_order_because_a_reorder_would_break_the_preview() {
        let plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"a","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"x"}},
                 {"id":"s2","title":"b","verify":{"method":"exit_code"},"operation":{"kind":"run_command","tool":"y"}}]}"#,
        );
        let order = execution_order(&plan).expect("a valid plan has an order");
        assert_eq!(order, vec!["s1".to_owned(), "s2".to_owned()]);
        assert_eq!(order_ids(&plan), order);
    }

    #[test]
    fn a_rendered_plan_shows_the_probe_and_the_target_for_every_step() {
        let plan = parse_ok(
            r#"{"schema_version":1,"goal":"g","steps":[
                 {"id":"s1","title":"echo once","verify":{"method":"output_contains","needle":"ran"},
                  "operation":{"kind":"run_command","tool":"echo.tool","arguments":[
                    {"name":"word","type":"text","value":"hi"}]}}]}"#,
        );
        let joined = render_plan(&plan, &order_ids(&plan)).join("\n");
        for expected in ["s1", "echo once", "echo.tool", "output_contains", "ran"] {
            assert!(joined.contains(expected), "missing {expected}:\n{joined}");
        }
    }

    #[test]
    fn plan_step_states_round_trip_and_only_pending_is_reachable() {
        assert_eq!(PlanStepState::ALL.len(), 8);
        for state in PlanStepState::ALL {
            assert_eq!(
                PlanStepState::from_str(state.as_str()),
                Some(*state),
                "{} must parse back",
                state.as_str()
            );
            let json = serde_json::to_string(state).unwrap();
            assert_eq!(json, format!("\"{}\"", state.as_str()));
            assert_eq!(
                serde_json::from_str::<PlanStepState>(&json).unwrap(),
                *state
            );
            assert_eq!(state.to_string(), state.as_str());
        }
        assert_eq!(PlanStepState::from_str("awaiting_approval"), None);
        assert_eq!(PlanStepState::from_str(""), None);

        // The no-replay rule as a predicate: `running` is not reachable again, because an effect
        // that may already have happened is reconciled, not re-issued.
        assert!(PlanStepState::Pending.is_reachable());
        for state in PlanStepState::ALL {
            if *state != PlanStepState::Pending {
                assert!(!state.is_reachable(), "{state} must not be re-run");
            }
        }
        assert!(!PlanStepState::Running.is_settled());
        assert!(PlanStepState::NeedsAttention.is_settled());

        // A step never borrows the task's vocabulary. The two enums answer different questions,
        // and the names a reader sees must not be ambiguous between them.
        for task_state in crate::state::ALL_STATES {
            if matches!(
                task_state.as_str(),
                "queued" | "planning" | "awaiting_approval" | "verifying"
            ) {
                assert!(
                    PlanStepState::from_str(task_state.as_str()).is_none(),
                    "\"{}\" is a task state and must not read as a step state",
                    task_state.as_str()
                );
            }
        }
    }

    #[test]
    fn the_diagnostics_are_a_stable_vocabulary() {
        // The codes are printed to operators and asserted by tests, so a rename is a contract
        // change. This lists what structure validation can produce, in one readable place.
        let mut plan = minimal();
        plan.steps[0].id = "s 1".to_owned();
        plan.steps[0].title = String::new();
        plan.steps[0].depends_on = vec!["nope".to_owned()];
        let found = codes(&plan);
        for expected in ["invalid_id", "invalid_title", "unknown_dependency"] {
            assert!(
                found.contains(&expected),
                "{expected} missing from {found:?}"
            );
        }
    }
}
