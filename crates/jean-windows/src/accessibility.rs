//! Accessibility: what a window exposes through UI Automation, and what can be done to it.
//!
//! This is the semantic path — the one D-004 puts above pixels — and it is also the one that can hang.
//! Every entry point here is a cross-process COM conversation with an application that is not under
//! J.E.A.N.'s supervision, so nothing in this module is called directly by a caller: the public functions
//! all route through [`crate::bounded`], which puts the conversation on its own thread and walks away
//! from it when it does not answer.
//!
//! ## Why a trait, and what the trait is for
//!
//! `PLANNING/05-ROADMAP.md` requires the UIA wrapper to sit behind an internal trait so the wrapper is
//! swappable (D-017). The reason that matters is concrete: the wrapper in use, `uiautomation`, has a bus
//! factor of one and ships breaking releases, and raw `IUIAutomation` through the `windows` crate is a
//! fallback that works. Both of those are implementations of [`AccessibilityBackend`]; neither changes a
//! caller.
//!
//! The trait is deliberately expressed in *owned data only* — a [`WindowId`], a [`Locator`], an
//! [`Action`] in; a [`Control`] or an [`Effect`] out. That is not tidiness: a UIA element is a COM pointer
//! meaningful on the thread that obtained it, so any signature that returned one would make the
//! off-thread execution in [`crate::bounded`] impossible. The shape of the trait is what forces the
//! isolation to be real rather than aspirational.
//!
//! ## What an empty tree means, and what it does not
//!
//! R-16 is the failure this module is designed against: Chromium and Electron applications expose almost
//! nothing through UIA until they are told to, and a naive reader concludes the application has no
//! controls. That conclusion is wrong in a way that costs the operator a working desktop, so
//! [`Tree::explanation`] and [`Probe::explanation`] carry *why* the list looks the way it does. An empty
//! tree is evidence of unavailable content, never proof of a protection, and the two are not merged here.
//!
//! ## Staleness
//!
//! A [`Control`] is a photograph, not a handle. Every action re-resolves its [`Locator`] before touching
//! anything and reports what it resolved to, because the alternative — acting on a coordinate or an
//! element found before a layout change — is the failure that clicks the wrong thing. The runtime id is
//! carried along so a caller can tell "the same control, unchanged" from "a control that happens to have
//! the same name now".

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::error::{WindowsError, WindowsResult};
use crate::identity::{Rect, WindowId};
use crate::worker::{self, Body, ReplyBody, Stop};

/// The largest number of controls a single tree read may return.
///
/// UIA will happily walk a virtualised list of ten thousand rows and hand back ten thousand elements, and
/// the cost of that is paid by the caller's deadline and memory. Hitting the cap is reported through
/// [`Tree::truncated`] rather than being allowed to look like a complete answer.
pub const MAX_NODES: usize = 2_000;

/// The longest control name or id this module keeps.
///
/// Names are text another process chose. They end up in reports and in audit lines, so they are bounded
/// the same way window titles are.
pub const MAX_NAME_CHARS: usize = 256;

/// How a control is addressed.
///
/// Every variant is a *question asked of the tree at action time*, never a cached element, which is what
/// makes revalidation the default rather than a discipline someone has to remember.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Locator {
    /// The `AutomationId` the control author assigned. On a standard Win32 control this is the dialog
    /// control id, and on a well-built custom control it is a stable name — which is why an
    /// `AutomationId` hit is trusted more than a name hit, and why neither is trusted forever.
    AutomationId(String),
    /// The accessible name, optionally narrowed to one control type.
    Name {
        name: String,
        control_type: Option<ControlKind>,
    },
    /// The nth descendant in tree order. Only meaningful inside a [`Locator::Path`].
    Index(usize),
    /// A chain from the window root: each step narrows the search to the children of the previous step.
    ///
    /// This is the ancestry locator. It exists because "the second `Continue` button" is not ambiguous
    /// once you say *in which group*, and because a flat name search across a real application's tree
    /// almost always finds several matches.
    Path(Vec<Locator>),
}

impl Locator {
    /// A short, non-secret description of how something was addressed.
    ///
    /// Used for the audit line. It never carries the *content* of a control, only what was looked for.
    pub fn describe(&self) -> String {
        match self {
            Locator::AutomationId(id) => format!("automation_id={id}"),
            Locator::Name { name, control_type } => match control_type {
                Some(kind) => format!("name={name} type={}", kind.name()),
                None => format!("name={name}"),
            },
            Locator::Index(index) => format!("index={index}"),
            Locator::Path(steps) => {
                let joined = steps
                    .iter()
                    .map(Locator::describe)
                    .collect::<Vec<_>>()
                    .join(" > ");
                format!("path[{joined}]")
            }
        }
    }

    /// The single-step form, for a backend that resolves one level at a time.
    pub fn is_path(&self) -> bool {
        matches!(self, Locator::Path(_))
    }
}

/// The control types this module can name.
///
/// Deliberately a closed set rather than the raw UIA integer: the number is a property of a COM interface
/// this crate does not have to expose, and a caller comparing against a name cannot be wrong in a way that
/// silently changes which control an action lands on. Anything unrecognised stays [`ControlKind::Other`]
/// with both its id and the localised name the provider gave.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ControlKind {
    Button,
    Calendar,
    CheckBox,
    ComboBox,
    Custom,
    DataGrid,
    Edit,
    Group,
    Header,
    Hyperlink,
    Image,
    List,
    ListItem,
    Menu,
    MenuBar,
    MenuItem,
    Pane,
    ProgressBar,
    RadioButton,
    ScrollBar,
    Slider,
    Spinner,
    SplitButton,
    StatusBar,
    Tab,
    TabItem,
    Table,
    Text,
    Thumb,
    TitleBar,
    ToolBar,
    ToolTip,
    Tree,
    TreeItem,
    Window,
    Other { id: i32, localized: String },
}

impl ControlKind {
    /// From the UIA control-type id, with the localised name kept for reporting.
    pub fn from_uia(id: i32, localized: &str) -> Self {
        match id {
            50000 => Self::Button,
            50001 => Self::Calendar,
            50002 => Self::CheckBox,
            50003 => Self::ComboBox,
            50025 => Self::Custom,
            50028 => Self::DataGrid,
            50004 => Self::Edit,
            50026 => Self::Group,
            50034 => Self::Header,
            50005 => Self::Hyperlink,
            50006 => Self::Image,
            50008 => Self::List,
            50007 => Self::ListItem,
            50009 => Self::Menu,
            50010 => Self::MenuBar,
            50011 => Self::MenuItem,
            50033 => Self::Pane,
            50012 => Self::ProgressBar,
            50013 => Self::RadioButton,
            50014 => Self::ScrollBar,
            50015 => Self::Slider,
            50016 => Self::Spinner,
            50030 => Self::SplitButton,
            50027 => Self::StatusBar,
            50018 => Self::Tab,
            50019 => Self::TabItem,
            50020 => Self::Text,
            50029 => Self::Text,
            50031 => Self::Thumb,
            50037 => Self::TitleBar,
            50021 => Self::ToolBar,
            50023 => Self::ToolTip,
            50022 => Self::Tree,
            50024 => Self::TreeItem,
            50032 => Self::Window,
            other => Self::Other {
                id: other,
                localized: localized.to_owned(),
            },
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Other { id, localized } => format!("{localized}({id})"),
            other => format!("{other:?}"),
        }
    }

    /// A control that is only chrome around content.
    ///
    /// Used by [`TreeExplanation`] to say "nothing interactive was found" instead of "nothing was found",
    /// which is the distinction R-16 turns on.
    pub fn is_frame_like(&self) -> bool {
        matches!(self, Self::Window | Self::Pane | Self::Custom)
    }
}

/// The UIA control patterns this module is willing to use.
///
/// Each one is a *different kind of action* and they do not substitute for each other: invoking a button
/// and setting the value of an edit are not the same operation, and a fallback that pretends they are
/// would be the kind of thing that silently sends a message instead of drafting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pattern {
    Invoke,
    Value,
    Toggle,
    SelectionItem,
    Selection,
    ExpandCollapse,
    ScrollItem,
    Transform,
}

impl Pattern {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Invoke => "Invoke",
            Self::Value => "Value",
            Self::Toggle => "Toggle",
            Self::SelectionItem => "SelectionItem",
            Self::Selection => "Selection",
            Self::ExpandCollapse => "ExpandCollapse",
            Self::ScrollItem => "ScrollItem",
            Self::Transform => "Transform",
        }
    }

    /// Which pattern an action needs, when it needs exactly one.
    pub fn required_by(action: &Action) -> Option<Self> {
        match action {
            Action::Invoke => Some(Self::Invoke),
            Action::SetValue { .. } => Some(Self::Value),
            Action::Toggle => Some(Self::Toggle),
            Action::Select | Action::AddToSelection | Action::RemoveFromSelection => {
                Some(Self::SelectionItem)
            }
            Action::Expand | Action::Collapse => Some(Self::ExpandCollapse),
            Action::ScrollIntoView => Some(Self::ScrollItem),
            Action::SetFocus => None,
        }
    }
}

/// What may be done to a control through its accessible patterns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Action {
    /// Press the control, the way a click would, but expressed as the control's own semantics.
    Invoke,
    /// Set the value of an edit-like control.
    SetValue {
        value: String,
    },
    Toggle,
    /// Choose this item and deselect its siblings.
    Select,
    AddToSelection,
    RemoveFromSelection,
    Expand,
    Collapse,
    ScrollIntoView,
    /// Give this control the keyboard focus.
    ///
    /// An action in its own right, and the only sanctioned way to change what typed input would reach —
    /// which is why the input module does not get to do it as a side effect of a move.
    SetFocus,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Invoke => "invoke",
            Self::SetValue { .. } => "set_value",
            Self::Toggle => "toggle",
            Self::Select => "select",
            Self::AddToSelection => "add_to_selection",
            Self::RemoveFromSelection => "remove_from_selection",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::ScrollIntoView => "scroll_into_view",
            Self::SetFocus => "set_focus",
        }
    }

    /// Whether this action can change anything outside this process.
    ///
    /// Everything except focus and scrolling can, which is what gates the "an uncertain action is never
    /// replayed" rule in `jean_core::desktop` rather than leaving that to a name lookup.
    pub fn has_side_effects(&self) -> bool {
        !matches!(self, Self::ScrollIntoView | Self::SetFocus)
    }

    /// A description safe for the audit line: the action, never its content.
    pub fn describe(&self) -> String {
        match self {
            Self::SetValue { value } => {
                // The typed text is the operator's, not the audit's. Length only, so a reader can tell
                // "wrote something" from "wrote nothing" without the content.
                format!("set_value({} chars)", value.chars().count())
            }
            other => other.name().to_owned(),
        }
    }
}

/// One control, as observed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Control {
    /// The `AutomationId` as the provider reported it. Empty when the provider reported none.
    pub automation_id: String,
    pub name: String,
    /// `true` when the name was longer than [`MAX_NAME_CHARS`] and had to be cut.
    pub name_truncated: bool,
    pub kind: ControlKind,
    pub class_name: String,
    pub is_enabled: bool,
    pub has_keyboard_focus: bool,
    /// Whether the provider marks this control's value as a password.
    ///
    /// This is the mechanism by which [`Readback::value`] gets redacted: the control itself says its
    /// content is secret, so nothing above this module has to guess.
    pub is_password: bool,
    /// Physical-pixel bounds on the virtual desktop, when the provider reported them.
    pub rect: Option<Rect>,
    /// UIA's runtime id, for staleness comparison across two reads.
    pub runtime_id: Vec<i32>,
    /// Which of [`Pattern`] this control answered for.
    ///
    /// Probed, never assumed: a control that reports itself as a button and supports no Invoke pattern is
    /// a real thing that happens, and the only way to know is to ask. Empty unless
    /// [`Settings::probe_patterns`] was set or this control was the target of an action.
    pub patterns: Vec<Pattern>,
    /// Depth from the window root, when the read computed it.
    ///
    /// `None` for the flat subtree read, which asks UIA for every descendant in one call and therefore
    /// does not see the parent links. A path resolution does compute it, and the difference is reported
    /// rather than being papered over with a `0` that would look like "this is the root".
    pub depth: Option<usize>,
}

impl Control {
    /// The control centre in physical pixels, for the input fallback.
    ///
    /// `None` when the provider reported no rectangle *or* reported an empty one: clicking the default
    /// `(0, 0)` of a missing rectangle is exactly the blind click on a misleading image that the plan
    /// forbids.
    pub fn click_point(&self) -> Option<(i32, i32)> {
        let rect = self.rect.filter(|rect| !rect.is_empty())?;
        Some(rect.centre())
    }

    /// A compact identity that survives a re-read: id, kind, and where it was.
    pub fn fingerprint(&self) -> String {
        let bounds = self
            .rect
            .filter(|rect| !rect.is_empty())
            .map(|rect| {
                format!(
                    "{}x{}@{},{}",
                    rect.width(),
                    rect.height(),
                    rect.left,
                    rect.top
                )
            })
            .unwrap_or_else(|| "no-bounds".to_owned());
        format!(
            "{}|{}|{bounds}",
            if self.automation_id.is_empty() {
                "-"
            } else {
                self.automation_id.as_str()
            },
            self.kind.name()
        )
    }

    /// Whether this is the same control as another read reported.
    ///
    /// Runtime ids are the only handle UIA offers, and they are not guaranteed unique across an
    /// application's lifetime, so `false` means "this is a different observation" and `true` means
    /// "probably the same" — which is why an action still re-reads the properties before acting.
    pub fn is_same_as(&self, other: &Control) -> bool {
        !self.runtime_id.is_empty() && self.runtime_id == other.runtime_id
    }

    /// Does this control match a single-step locator?
    pub fn matches(&self, locator: &Locator) -> bool {
        match locator {
            Locator::AutomationId(id) => self.automation_id == *id,
            Locator::Name { name, control_type } => {
                self.name.eq_ignore_ascii_case(name)
                    && control_type
                        .as_ref()
                        .map(|want| want == &self.kind)
                        .unwrap_or(true)
            }
            // Only meaningful once a list exists; the backend applies it after collecting.
            Locator::Index(_) | Locator::Path(_) => true,
        }
    }
}

/// The observable state of a control, read before and after an action.
///
/// This is the verifier's raw material. "The call returned success" is not a result (R-21), so an action
/// reports what it could see beforehand, what it can see afterwards, and whether the two differ.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Readback {
    /// The control's accessible name, which for a status label is the status itself.
    pub name: Option<String>,
    /// The Value pattern's content, redacted when the provider marked the control as a password.
    pub value: Option<String>,
    /// The Toggle pattern's state as the provider named it.
    pub toggle: Option<String>,
    pub selected: Option<bool>,
    pub expanded: Option<String>,
    pub enabled: Option<bool>,
    /// `true` when the control reported a value and it was withheld because `is_password` said so.
    pub value_redacted: bool,
}

impl Readback {
    /// `true` when the provider reported nothing this digest can carry.
    ///
    /// [`Self::digest`] has to say something for a log line, and it invents a placeholder; a caller that is
    /// composing a longer digest needs the difference between "no fields" and "one field that says so".
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.value.is_none()
            && self.toggle.is_none()
            && self.selected.is_none()
            && self.expanded.is_none()
            && self.enabled.is_none()
    }

    /// A single string that changes when the control's state changes.
    ///
    /// Used as the comparison behind [`Effect::changed`]. It is deliberately lossy about content and
    /// precise about *difference*: two readbacks with different digests describe different states, and the
    /// digest is safe to put in a log because the password case is already redacted.
    pub fn digest(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(name) = &self.name {
            parts.push(format!("name={name}"));
        }
        match (&self.value, self.value_redacted) {
            (Some(text), true) => {
                parts.push(format!("value={} chars (redacted)", text.chars().count()))
            }
            (Some(text), false) => parts.push(format!("value={text}")),
            (None, _) => {}
        }
        if let Some(toggle) = &self.toggle {
            parts.push(format!("toggle={toggle}"));
        }
        if let Some(selected) = self.selected {
            parts.push(format!("selected={selected}"));
        }
        if let Some(expanded) = &self.expanded {
            parts.push(format!("expanded={expanded}"));
        }
        if let Some(enabled) = self.enabled {
            parts.push(format!("enabled={enabled}"));
        }
        if parts.is_empty() {
            "<nothing observable>".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

/// A second control, watched across the action because the control the action touched is usually not the
/// one that changes.
///
/// This is the shape the brief asks for — "perform an authorized action → observe the resulting state →
/// report evidence" — and the reason it is a separate observation rather than a field of [`Effect`] is
/// that *which* control proves an action is a property of the task, not of the pattern. Pressing a send
/// button changes a status line; ticking a checkbox changes a total; typing changes nothing observable
/// until something reads it back. Only the caller knows which, so the caller names it and this is where
/// the before and after are captured, on the same thread and inside the same bound as the action itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Witness {
    pub locator: Locator,
    pub before: Readback,
    /// `None` when the witness could not be read after the action — which is an outcome to report, not a
    /// zero to invent.
    pub after: Option<Readback>,
    pub changed: bool,
    /// `true` when the witness named nothing at all. A witness that cannot be found is a plan that
    /// pointed at the wrong thing, and the report has to be able to say that separately from "nothing
    /// happened".
    pub unresolved: bool,
}

/// One control, and what its own patterns say about it.
///
/// Boxed inside [`StateRead::Found`] because the two alternatives are a `usize` and nothing: an enum sized
/// by its largest arm would make every failed witness read carry three hundred bytes it does not use.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlState {
    /// The listing half: class, kind, focus, bounds, patterns.
    pub control: Control,
    /// The state half: the pattern properties, with a password value already redacted by the reader.
    pub state: Readback,
}

/// What one control's state read found.
///
/// Three answers, not one, because "the witness is not there" and "the witness did not change" are
/// opposite claims about the world and a caller cannot tell them apart from a digest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum StateRead {
    /// Exactly one control matched the locator, and this is what it reports about itself.
    Found(Box<ControlState>),
    /// Nothing matched. Either the control is gone or the locator never described this application.
    Absent,
    /// Several matched, so none of them is evidence of anything.
    Ambiguous { count: usize },
}

/// The result of one action, including what could and could not be seen about it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Effect {
    /// The control the action was actually applied to, re-resolved immediately before it.
    pub control: Control,
    pub action: Action,
    /// The pattern that carried it.
    pub pattern: Pattern,
    pub before: Readback,
    /// `None` when the control could not be read again after the action.
    ///
    /// That is not the same as "nothing changed", and the distinction is the whole reason this is an
    /// `Option`: the caller has to know it is holding an *uncertain* side effect.
    pub after: Option<Readback>,
    /// `true` when the *acted control's own* state differed before and after.
    ///
    /// Not the same as "the action did something": invoking a send button does not change the button.
    /// [`Effect::state_changed`] is the claim a verifier should be asking about, and it includes the
    /// witness.
    pub changed: bool,
    /// The second observation the caller asked for, if it asked for one.
    pub witness: Option<Witness>,
    /// `true` when the outcome cannot be established, so the action must never be replayed.
    pub uncertain: bool,
    /// Why the outcome is uncertain, in the words of the call that failed.
    pub uncertainty_reason: Option<String>,
    /// The foreground window as seen immediately after the action.
    ///
    /// Reported because a control's own action can move focus, and a caller about to type has to learn
    /// that from the *current* state rather than from what was true when the plan was written.
    pub foreground_after: Option<WindowId>,
}

impl Effect {
    /// Whether anything the caller pointed at is now different.
    ///
    /// The acted control *or* the witness, because for most real actions the witness is the only one that
    /// moves. A false here means the action ran and the desktop looks the same as it did, which is a
    /// result — a failed one — and not an unknown.
    pub fn state_changed(&self) -> bool {
        self.changed
            || self
                .witness
                .as_ref()
                .map(|witness| witness.changed)
                .unwrap_or(false)
    }

    /// Whether the caller may treat the world as this action's result.
    ///
    /// Requires an observable difference *and* an outcome that could be established. The second half is
    /// what keeps `is_verified()` from meaning "we did not hear otherwise".
    pub fn is_verified(&self) -> bool {
        self.state_changed() && !self.uncertain && self.after.is_some()
    }

    /// Whether the action's outcome is unknowable but its side effects may have landed.
    ///
    /// The state that forbids an automatic retry. A verifier reports it; nothing here decides it.
    pub fn is_uncertain_side_effect(&self) -> bool {
        self.uncertain && self.action.has_side_effects()
    }
}

/// A tree read.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tree {
    pub window: WindowId,
    pub controls: Vec<Control>,
    /// The number of controls the provider reported before the cap cut the walk.
    pub truncated: bool,
    pub observed_at_ms: u64,
    pub explanation: TreeExplanation,
}

/// The explanation attached to a tree or probe read.
///
/// R-16 and `docs/desktop-control-plan.md` both insist that "no controls" and "no controls *visible to
/// us*" are different sentences, and the only way to keep them apart is to compute the difference where
/// the evidence still is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TreeExplanation {
    /// Interactive controls were found. Nothing to explain.
    Populated,
    /// The window exists but reported no UIA element at all.
    ///
    /// Consistent with an application that has not enabled its accessibility tree, and consistent with a
    /// provider that is simply absent. Not consistent with a conclusion about protection.
    NoElements,
    /// Only frames were reported — windows, panes, custom regions — and nothing a user would act on.
    ///
    /// The shape of an Electron or Chromium window whose renderer accessibility is off: the frame is a
    /// real Win32 window with a real provider and everything inside it is a document tree UIA has not
    /// been asked to expose. `class_name` is the frame's own class, quoted so a report can name it.
    NoInteractiveContent {
        frames: usize,
        class_name: String,
        /// Whether the class name is one of the known web-content frames.
        ///
        /// A wording hint only. It does not decide an action, and it does not claim the application is
        /// inaccessible — it says "this looks like an embedded browser surface, so an empty tree is
        /// expected rather than mysterious".
        looks_like_web_content: bool,
    },
    /// The read was cut by [`Settings::max_nodes`], so the list is a sample, not the tree.
    Capped,
    /// The provider refused or could not be reached.
    ///
    /// Carries the reason, which is what a caller reports instead of retrying (R-14).
    Unreachable(String),
}

impl TreeExplanation {
    /// Whether anything in this explanation should stop unrelated work.
    ///
    /// The answer is never, and the method exists to keep it that way: `docs/desktop-control-plan.md`
    /// requires that a blocked read is not a blocked desktop, and the easiest way to break that rule is
    /// to let one explanation mean "give up on the session".
    pub fn is_session_blocking(&self) -> bool {
        false
    }
}

/// What a window exposes, in the coarse terms a capability report is written from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Probe {
    pub window: WindowId,
    /// Did UIA return an element for this window handle at all?
    pub has_root_element: bool,
    /// Class name of the frame, when it could be read — the cheapest available hint about an embedded
    /// web-content area, and nothing more.
    pub frame_class: Option<String>,
    pub control_count: usize,
    pub kinds: Vec<ControlKind>,
    /// Patterns any control in the tree supported.
    pub patterns: Vec<Pattern>,
    pub explanation: TreeExplanation,
    /// Whether the accessibility pipeline itself (COM plus `UIAutomationCore`) could be initialised on
    /// this thread. `false` here is a different failure from an empty tree, and the diagnostics say so.
    pub automation_available: bool,
    pub observed_at_ms: u64,
}

impl Probe {
    /// Whether there is anything worth acting on.
    ///
    /// Not named `can_see`: the plan is explicit that a single desktop-wide visibility flag is the design
    /// mistake that lets one protected application disable unrelated work, and a name is part of a design.
    pub fn has_actionable_controls(&self) -> bool {
        self.control_count > 0
            && self
                .patterns
                .iter()
                .any(|pattern| !matches!(pattern, Pattern::Transform))
    }
}

/// Settings a read or an action is performed under.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// How long one UIA conversation may take before the worker running it is terminated with its tree.
    pub call_timeout: Duration,
    /// Node cap for a tree read.
    pub max_nodes: usize,
    /// Whether every control's `patterns` list is probed.
    ///
    /// Probing costs one COM round trip per pattern per control, so a large tree is read without it by
    /// default and the action path probes only the control it is about to touch. That trade is stated
    /// rather than buried, because it changes what a report can claim.
    pub probe_patterns: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(5),
            max_nodes: MAX_NODES,
            probe_patterns: false,
        }
    }
}

impl Settings {
    /// Reject an impossible bound before anything is attempted.
    pub fn validate(&self) -> WindowsResult<()> {
        if self.call_timeout.is_zero() {
            return Err(WindowsError::InvalidSetting {
                reason: "desktop.uia_timeout_secs must be at least 1".to_owned(),
            });
        }
        if self.max_nodes == 0 {
            return Err(WindowsError::InvalidSetting {
                reason: "desktop.max_tree_nodes must be at least 1".to_owned(),
            });
        }
        Ok(())
    }
}

/// The boundary D-017 asks for: everything above this crate talks to these five questions, and an
/// implementation can be replaced without touching a caller.
///
/// The implementation is the [`crate::uia`] backend, and it runs inside `jean-desktop-worker.exe` (see
/// [`crate::worker`]). Implementers must create whatever apartment they need per call and must hold no UIA
/// object between calls, because the process running them can be terminated at any deadline and is not
/// expected to be polite about it.
pub trait AccessibilityBackend: Send + Sync {
    /// A bounded walk of the tree rooted at one window.
    fn read_tree(&self, window: WindowId, settings: &Settings) -> WindowsResult<Tree>;

    /// Resolve a locator to the controls it names, in tree order.
    ///
    /// Returning several is a legitimate answer and the caller's problem to disambiguate; an
    /// implementation must not silently pick one.
    fn resolve(
        &self,
        window: WindowId,
        locator: &Locator,
        settings: &Settings,
    ) -> WindowsResult<Vec<Control>>;

    /// Coarse "what does this window expose", for the capability diagnostics.
    fn probe(&self, window: WindowId, settings: &Settings) -> WindowsResult<Probe>;

    /// Read the state patterns of the one control this locator names, without acting on it.
    ///
    /// This is the question [`Self::act`] answers about its witness and its own control, asked on its own.
    /// It exists because a keystroke has no pattern call to carry a witness inside it: injected input is
    /// proven by reading what the target now contains, and a tree listing cannot answer that — only the
    /// Value, Toggle, SelectionItem and ExpandCollapse properties can.
    fn read_state(
        &self,
        window: WindowId,
        locator: &Locator,
        settings: &Settings,
    ) -> WindowsResult<StateRead>;

    /// Re-resolve `locator`, then perform `action` on the single control it names.
    ///
    /// The re-resolution is not optional and not an optimisation to be skipped: it is the reason a stale
    /// plan cannot act on a control that no longer exists.
    fn act(
        &self,
        window: WindowId,
        locator: &Locator,
        action: &Action,
        witness: Option<&Locator>,
        settings: &Settings,
    ) -> WindowsResult<Effect>;
}

/// The real UIA backend: `uiautomation` over the system's `UIAutomationCore`.
///
/// This is what `jean-desktop-worker.exe` runs, on the worker's own thread, inside the process the parent
/// holds the job handle for. Calling it directly is not an option for a desktop operation:
/// `UIAutomation::new()` initialises a multithreaded apartment **on the calling thread**, and a provider
/// that never answers then parks that thread inside a cross-process `SendMessage` with no way to recall it
/// — the exact failure [`crate::worker`] exists to make impossible. The type is public because the worker
/// binary is a separate crate target and has to name it, not because anything above should bypass the
/// supervision.
pub struct UiaBackend;

/// Read the accessible tree of one window, in a worker the parent can kill.
///
/// The bound is the point: a provider that does not answer costs a **process**, which the parent
/// terminates with its tree, and nothing is left parked in this one.
pub fn read_tree(
    window: WindowId,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Tree> {
    settings.validate()?;
    run(
        window,
        "uia-read",
        settings.call_timeout,
        cancel,
        Body::ReadTree {
            window: window.hwnd,
            max_nodes: settings.max_nodes,
            probe_patterns: settings.probe_patterns,
        },
        |answer| match answer {
            ReplyBody::Tree(tree) => Some(tree),
            _ => None,
        },
    )
}

/// The UIA call every action begins with: what does this locator name *right now*.
pub fn resolve(
    window: WindowId,
    locator: Locator,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Vec<Control>> {
    settings.validate()?;
    run(
        window,
        "uia-resolve",
        settings.call_timeout,
        cancel,
        Body::Resolve {
            window: window.hwnd,
            locator,
            max_nodes: settings.max_nodes,
            probe_patterns: settings.probe_patterns,
        },
        |answer| match answer {
            ReplyBody::Controls(controls) => Some(controls),
            _ => None,
        },
    )
}

/// Coarse capability probe of one window's accessibility.
pub fn probe(
    window: WindowId,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Probe> {
    settings.validate()?;
    run(
        window,
        "uia-probe",
        settings.call_timeout,
        cancel,
        Body::Probe {
            window: window.hwnd,
            max_nodes: settings.max_nodes,
        },
        |answer| match answer {
            ReplyBody::Probe(probe) => Some(probe),
            _ => None,
        },
    )
}

/// Read the state of the one control a locator names, without acting on anything.
///
/// The same bound and the same worker lifecycle as every other entry point: a provider that does not
/// answer costs this read, and the caller still has to distinguish "the control is gone" from "the control
/// did not change".
pub fn read_state(
    window: WindowId,
    locator: Locator,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<StateRead> {
    settings.validate()?;
    run(
        window,
        "uia-state",
        settings.call_timeout,
        cancel,
        Body::ReadState {
            window: window.hwnd,
            locator,
            probe_patterns: settings.probe_patterns,
            max_nodes: settings.max_nodes,
        },
        |answer| match answer {
            ReplyBody::State(state) => Some(state),
            _ => None,
        },
    )
}

/// Re-resolve a locator and perform one action, in a worker the parent can kill.
///
/// Note what terminating the worker cannot promise: it may have been killed *after* the pattern call had
/// already been delivered. An uncertain action is what `jean_core::desktop` records rather than retrying,
/// and [`WindowsError::ProviderTimeout`] on this entry point is the signal for it.
pub fn act(
    window: WindowId,
    locator: Locator,
    action: Action,
    witness: Option<Locator>,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Effect> {
    settings.validate()?;
    run(
        window,
        "uia-act",
        settings.call_timeout,
        cancel,
        Body::Act {
            window: window.hwnd,
            locator,
            action,
            witness,
            probe_patterns: settings.probe_patterns,
            max_nodes: settings.max_nodes,
        },
        |answer| match answer {
            ReplyBody::Effect(effect) => Some(*effect),
            _ => None,
        },
    )
}

/// One UIA conversation with a worker process, and the error vocabulary every entry point shares.
///
/// The mapping is written once because the *error* a caller gets for an abandoned call must not depend on
/// which call it was — a timeout on a read and a timeout on an action are the same observation about the
/// provider, and differ only in what the caller may safely do next, which is not this function's business.
///
/// A [`Stop::TimedOut`] and a [`Stop::Cancelled`] both mean the worker has already been terminated with
/// its tree, so unlike the thread design there is nothing left running behind the error.
fn run<T>(
    window: WindowId,
    label: &'static str,
    budget: Duration,
    cancel: Option<&AtomicBool>,
    body: Body,
    expect: impl FnOnce(ReplyBody) -> Option<T>,
) -> WindowsResult<T> {
    let answer = match worker::one(&body, budget, cancel) {
        Ok(answer) => answer,
        Err(stop) => {
            return Err(match stop {
                Stop::TimedOut { waited } => WindowsError::ProviderTimeout {
                    hwnd: window.hwnd,
                    budget_secs: budget.as_secs().max(1),
                    waited_ms: waited.as_millis() as u64,
                },
                Stop::Cancelled { waited } => WindowsError::Cancelled {
                    budget_secs: budget.as_secs().max(1),
                    waited_ms: waited.as_millis() as u64,
                },
                Stop::Lost { reason } | Stop::Unavailable { reason } => {
                    WindowsError::AccessibilityUnavailable {
                        reason: format!("{label}: {reason}"),
                    }
                }
            });
        }
    };
    // A failure the worker *reported* is the desktop's own answer, and arrives with the variant the
    // in-process call would have produced: an ambiguous locator is still `Ambiguous`, across a pipe.
    let answer = answer.into_result()?;
    let tag = answer.tag();
    expect(answer).ok_or_else(|| WindowsError::AccessibilityUnavailable {
        reason: format!(
            "the worker answered {label} with a {tag} rather than the shape this call returns"
        ),
    })
}

/// Truncate a provider-supplied string to [`MAX_NAME_CHARS`] and say whether it had to be cut.
pub(crate) fn bound_name(text: &str) -> (String, bool) {
    let count = text.chars().count();
    if count <= MAX_NAME_CHARS {
        (text.to_owned(), false)
    } else {
        (text.chars().take(MAX_NAME_CHARS).collect(), true)
    }
}

/// Which explanation the shape of a control list supports.
///
/// The rule is about *interactivity*, not count: a window that reports three panes and nothing else has
/// exposed a frame, and saying "no controls" would be a different claim from the true one.
pub(crate) fn explain(controls: &[Control], truncated: bool) -> TreeExplanation {
    if truncated {
        return TreeExplanation::Capped;
    }
    if controls.is_empty() {
        return TreeExplanation::NoElements;
    }
    let interactive = controls.iter().filter(|c| !c.kind.is_frame_like()).count();
    if interactive > 0 {
        return TreeExplanation::Populated;
    }
    let class_name = controls
        .iter()
        .find(|control| !control.class_name.is_empty())
        .map(|control| control.class_name.clone())
        .unwrap_or_default();
    TreeExplanation::NoInteractiveContent {
        frames: controls.len(),
        looks_like_web_content: looks_like_web_content(&class_name),
        class_name,
    }
}

/// Whether a class name is one of the known embedded-browser frames.
///
/// `Chrome_WidgetWin_1` is what every Chromium-derived shell hands to `RegisterClass`, and the others are
/// the long-standing variants. This is a wording aid for a diagnosis, and nothing downstream may treat it
/// as a fact about the application's accessibility: the check exists so a report can say *this looks
/// like*, which is the whole of what the evidence supports.
pub(crate) fn looks_like_web_content(class: &str) -> bool {
    let lowered = class.to_ascii_lowercase();
    lowered.contains("chrome_widgetwin")
        || lowered.contains("chrome_render_widget_host")
        || lowered.contains("electron")
        || lowered.contains("sun_awt_frame")
        || lowered.contains("wxwindowclass")
}
