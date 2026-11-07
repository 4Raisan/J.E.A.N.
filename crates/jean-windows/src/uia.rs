//! The `uiautomation`-backed implementation of the accessibility boundary.
//!
//! This is the only module in the crate that names a UIA type. That containment is the point of D-017:
//! when the wrapper is swapped — for a raw `IUIAutomation` implementation, or for a version that changed
//! under us — this file is what moves, and `jean_core::desktop` does not notice.
//!
//! Three rules hold here, and each of them is a rule about the *worker thread* rather than about UIA:
//!
//! * **Nothing is cached.** A `UIAutomation` instance and every element it produced are created inside one
//!   call and dropped at the end of it. `UIAutomation::new()` initialises a multithreaded apartment on the
//!   thread that calls it, so an instance held across calls would tie the crate to whichever thread
//!   created it first — and this code runs in `jean-desktop-worker.exe`, whose whole purpose (D-051) is
//!   that a call which does not answer is ended by killing the *process*. An instance that outlived the
//!   call would outlive the kill, and a COM apartment interrupted mid-conversation is not something the
//!   next call can trust: the apartment, the providers it connected to and any cached element all die with
//!   the process, which is exactly why nothing may be carried between calls.
//! * **Every pattern is probed, never inferred from the control type.** A button with no Invoke pattern and
//!   an edit with a read-only Value pattern are both real, and the only way to know is to ask the provider
//!   on this control.
//! * **A read that could not be made is `None`, not a default.** The rectangle UIA did not report is not
//!   `(0, 0, 0, 0)`, because `(0, 0, 0, 0)` is a place a click could land.

use uiautomation::types::{Handle, PropertyConditionFlags, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};

use crate::accessibility::{
    bound_name, explain, AccessibilityBackend, Action, Control, ControlKind, ControlState, Effect,
    Locator, Pattern, Probe, Readback, Settings, StateRead, Tree, TreeExplanation, UiaBackend,
    Witness,
};
use crate::error::{WindowsError, WindowsResult};
use crate::identity::{foreground, now_ms, Rect, WindowId};

/// The patterns probed, in the order they are asked for.
///
/// `Transform` is probed but is not an action this module performs: it moves or resizes a window, which
/// `docs/desktop-control-plan.md` classifies as optional workspace management and not as a way to reach
/// content, so it is reported as available and never used as a fallback for something else.
const PROBED: &[Pattern] = &[
    Pattern::Invoke,
    Pattern::Value,
    Pattern::Toggle,
    Pattern::SelectionItem,
    Pattern::Selection,
    Pattern::ExpandCollapse,
    Pattern::ScrollItem,
    Pattern::Transform,
];

/// The error vocabulary of the wrapper, translated once so no UIA type escapes this module.
///
/// `uiautomation` reports its own failures with its own strings. Those strings are the *evidence* a
/// report quotes — an `ERR_TIMEOUT` from the wrapper and a `COMException` from a provider are different
/// diagnoses — so they are carried verbatim rather than mapped to a code and lost.
fn uia_error(call: &str, error: uiautomation::Error) -> WindowsError {
    WindowsError::AccessibilityUnavailable {
        reason: format!("{call}: {error}"),
    }
}

/// Open a UIA session for one window on *this* thread.
///
/// Returns the automation instance and the element that stands for the window handle. The instance has to
/// outlive the walk, so both travel together.
struct Session {
    automation: UIAutomation,
    root: Option<UIElement>,
    /// Whether `UIAutomation::new()` succeeded. A failure here is a different fact from "this window has
    /// no controls", and the probe reports it as one.
    available: bool,
}

/// An automation instance is not constructible without COM, so a failed open has to say so rather than
/// carry one.
fn unavailable(reason: String) -> WindowsError {
    WindowsError::AccessibilityUnavailable { reason }
}

/// Open a session or explain why there is none.
fn open_session(window: WindowId) -> Result<Session, WindowsError> {
    match UIAutomation::new() {
        Ok(automation) => {
            let root = automation
                .element_from_handle(Handle::from(window.to_raw()))
                .ok();
            Ok(Session {
                automation,
                root,
                available: true,
            })
        }
        Err(error) => Err(unavailable(format!(
            "UI Automation could not be initialised on this thread: {error}"
        ))),
    }
}

/// The walk: every descendant of the window's own element, up to the caller's cap.
///
/// `TreeScope::Subtree` asks UIA for the element *and* its descendants in one call, which is the cheap
/// shape and the reason a tree read costs one conversation rather than one per node. The cost is that the
/// result is flat, so `depth` is reported as unknown here and computed only where a path is being walked.
fn descendants(
    session: &Session,
    root: &UIElement,
    max_nodes: usize,
) -> Result<(Vec<UIElement>, bool), WindowsError> {
    let condition = session
        .automation
        .create_true_condition()
        .map_err(|error| uia_error("create_true_condition", error))?;
    let found = root
        .find_all(TreeScope::Subtree, &condition)
        .map_err(|error| uia_error("FindAll(Subtree)", error))?;
    let truncated = found.len() > max_nodes;
    let mut kept = found;
    if truncated {
        kept.truncate(max_nodes);
    }
    Ok((kept, truncated))
}

/// One element into one observation.
///
/// Every property is read independently and a failed read leaves the field absent. That is more COM calls
/// than a cached-props read would be, and it is deliberate: `element_from_handle_build_cache` would ask
/// for the properties in one round trip but would also report whatever the provider *decided* to cache,
/// which is not the same as what it would have answered when asked.
fn to_control(element: &UIElement, depth: Option<usize>) -> Control {
    let (name, name_truncated) = match element.get_name() {
        Ok(name) => bound_name(&name),
        Err(_) => (String::new(), false),
    };
    let automation_id = element.get_automation_id().unwrap_or_default();
    let class_name = element.get_classname().unwrap_or_default();

    let kind = match element.get_control_type() {
        Ok(control_type) => {
            let localized = element
                .get_localized_control_type()
                .unwrap_or_else(|_| format!("{control_type:?}"));
            ControlKind::from_uia(control_type as i32, &localized)
        }
        Err(_) => ControlKind::Other {
            id: -1,
            localized: "unreadable".to_owned(),
        },
    };

    // UIA reports bounding rectangles in physical screen pixels for a per-monitor-aware process, which is
    // why R-15 has to be handled before this value means anything at all. A degenerate rectangle —
    // including the (0,0,0,0) an off-screen or not-yet-laid-out element reports — is stored as absent,
    // because the input path treats a present rectangle as a place it may click.
    let rect = element
        .get_bounding_rectangle()
        .ok()
        .map(|value| Rect {
            left: value.get_left(),
            top: value.get_top(),
            right: value.get_right(),
            bottom: value.get_bottom(),
        })
        .filter(|rect| !rect.is_empty());

    Control {
        automation_id,
        name,
        name_truncated,
        kind,
        class_name,
        is_enabled: element.is_enabled().unwrap_or(false),
        has_keyboard_focus: element.has_keyboard_focus().unwrap_or(false),
        is_password: element.is_password().unwrap_or(false),
        rect,
        runtime_id: element.get_runtime_id().unwrap_or_default(),
        patterns: Vec::new(),
        depth,
    }
}

/// Which of the probed patterns this element answered for.
fn probe_patterns(element: &UIElement) -> Vec<Pattern> {
    let mut out = Vec::new();
    for pattern in PROBED {
        let supported = match pattern {
            Pattern::Invoke => element
                .get_pattern::<uiautomation::patterns::UIInvokePattern>()
                .is_ok(),
            Pattern::Value => element
                .get_pattern::<uiautomation::patterns::UIValuePattern>()
                .is_ok(),
            Pattern::Toggle => element
                .get_pattern::<uiautomation::patterns::UITogglePattern>()
                .is_ok(),
            Pattern::SelectionItem => element
                .get_pattern::<uiautomation::patterns::UISelectionItemPattern>()
                .is_ok(),
            Pattern::Selection => element
                .get_pattern::<uiautomation::patterns::UISelectionPattern>()
                .is_ok(),
            Pattern::ExpandCollapse => element
                .get_pattern::<uiautomation::patterns::UIExpandCollapsePattern>()
                .is_ok(),
            Pattern::ScrollItem => element
                .get_pattern::<uiautomation::patterns::UIScrollItemPattern>()
                .is_ok(),
            Pattern::Transform => element
                .get_pattern::<uiautomation::patterns::UITransformPattern>()
                .is_ok(),
        };
        if supported {
            out.push(*pattern);
        }
    }
    out
}

/// What a control's own patterns say about its state.
///
/// The password case is handled here rather than at the reporting layer because this is the only place
/// that knows what was actually read: a redaction applied after the value reached a string is a redaction
/// that depends on nobody forgetting, and the plan requires private UI text never to enter a log at all.
fn read_state(element: &UIElement, control: &Control) -> Readback {
    let mut state = Readback {
        name: if control.name.is_empty() {
            None
        } else {
            Some(control.name.clone())
        },
        enabled: Some(control.is_enabled),
        ..Default::default()
    };
    if let Ok(pattern) = element.get_pattern::<uiautomation::patterns::UIValuePattern>() {
        if let Ok(value) = pattern.get_value() {
            if control.is_password {
                state.value_redacted = true;
                // The length is kept and the content is not: "the field is empty" and "the field has four
                // characters in it" are operationally different and neither is private.
                state.value = Some("".repeat(value.chars().count()));
            } else {
                state.value = Some(bound_name(&value).0);
            }
        }
    }
    if let Ok(pattern) = element.get_pattern::<uiautomation::patterns::UITogglePattern>() {
        state.toggle = pattern
            .get_toggle_state()
            .ok()
            .map(|state| format!("{state:?}"));
    }
    if let Ok(pattern) = element.get_pattern::<uiautomation::patterns::UISelectionItemPattern>() {
        state.selected = pattern.is_selected().ok();
    }
    if let Ok(pattern) = element.get_pattern::<uiautomation::patterns::UIExpandCollapsePattern>() {
        state.expanded = pattern.get_state().ok().map(|state| format!("{state:?}"));
    }
    state
}

/// Re-find the element a control came from, by its runtime id.
///
/// The alternative — keeping the `UIElement` from the read — is what this module cannot do: the element
/// belongs to the apartment of the thread that made it, and an action runs on a worker of its own. So the
/// control is *re-located* by identity, and a control that cannot be re-located is reported as stale
/// rather than acted on by name again.
fn relocate(
    session: &Session,
    root: &UIElement,
    control: &Control,
    max_nodes: usize,
) -> Result<UIElement, WindowsError> {
    let (found, _) = descendants(session, root, max_nodes)?;
    for element in &found {
        let candidate = to_control(element, None);
        if candidate.is_same_as(control)
            || (!control.automation_id.is_empty()
                && candidate.automation_id == control.automation_id
                && candidate.kind == control.kind)
        {
            return Ok(element.clone());
        }
    }
    Err(WindowsError::StaleElement {
        control: control.fingerprint(),
    })
}

/// Resolve a single step of a locator within one element's subtree.
fn resolve_in(
    session: &Session,
    scope: &UIElement,
    step: &Locator,
    max_nodes: usize,
) -> Result<Vec<UIElement>, WindowsError> {
    match step {
        Locator::AutomationId(id) => {
            let condition = session
                .automation
                .create_property_condition(
                    UIProperty::AutomationId,
                    Variant::from(id.as_str()),
                    Some(PropertyConditionFlags::IgnoreCase),
                )
                .map_err(|error| uia_error("create_property_condition(AutomationId)", error))?;
            let found = scope
                .find_all(TreeScope::Subtree, &condition)
                .map_err(|error| uia_error("FindAll(AutomationId)", error))?;
            Ok(capped(found, max_nodes))
        }
        Locator::Name { name, control_type } => {
            let condition = session
                .automation
                .create_property_condition(
                    UIProperty::Name,
                    Variant::from(name.as_str()),
                    Some(PropertyConditionFlags::IgnoreCase),
                )
                .map_err(|error| uia_error("create_property_condition(Name)", error))?;
            let found = scope
                .find_all(TreeScope::Subtree, &condition)
                .map_err(|error| uia_error("FindAll(Name)", error))?;
            let kept = capped(found, max_nodes);
            match control_type {
                None => Ok(kept),
                Some(want) => Ok(kept
                    .into_iter()
                    .filter(|element| {
                        element
                            .get_control_type()
                            .map(|kind| ControlKind::from_uia(kind as i32, "") == *want)
                            .unwrap_or(false)
                    })
                    .collect()),
            }
        }
        Locator::Index(index) => {
            let (all, _) = descendants_from(session, scope, max_nodes)?;
            Ok(all.into_iter().nth(*index).into_iter().collect())
        }
        // A nested path is flattened by the caller: `Path` steps are applied one scope at a time, so a
        // `Path` inside a `Path` is a caller error rather than a recursion this module has to define.
        Locator::Path(steps) => {
            let mut current = vec![scope.clone()];
            for step in steps {
                let mut next = Vec::new();
                for element in &current {
                    next.extend(resolve_in(session, element, step, max_nodes)?);
                }
                current = next;
            }
            Ok(current)
        }
    }
}

/// `descendants` with an arbitrary root instead of the window element.
fn descendants_from(
    session: &Session,
    scope: &UIElement,
    max_nodes: usize,
) -> Result<(Vec<UIElement>, bool), WindowsError> {
    descendants(session, scope, max_nodes)
}

fn capped(mut found: Vec<UIElement>, max_nodes: usize) -> Vec<UIElement> {
    if found.len() > max_nodes {
        found.truncate(max_nodes);
    }
    found
}

/// The depth of each element, when a path walk made it knowable.
///
/// A path resolution produces a per-step list, so the step number is the depth and is worth reporting;
/// the flat subtree read has no such number and reports `None`.
fn with_depth(controls: Vec<Control>, depth: usize) -> Vec<Control> {
    controls
        .into_iter()
        .map(|mut control| {
            control.depth = Some(depth);
            control
        })
        .collect()
}

impl AccessibilityBackend for UiaBackend {
    fn read_tree(&self, window: WindowId, settings: &Settings) -> WindowsResult<Tree> {
        let session = open_session(window)?;
        let Some(root) = session.root.clone() else {
            // No element for the handle. That is the R-16 case, and the report has to say *that* — not
            // "the application has no controls", which is a different claim than the evidence supports.
            return Ok(Tree {
                window,
                controls: Vec::new(),
                truncated: false,
                observed_at_ms: now_ms(),
                explanation: TreeExplanation::NoElements,
            });
        };
        let (found, truncated) = descendants(&session, &root, settings.max_nodes)?;
        let mut controls: Vec<Control> = found
            .iter()
            .map(|element| to_control(element, None))
            .collect();
        if settings.probe_patterns {
            for (control, element) in controls.iter_mut().zip(found.iter()) {
                control.patterns = probe_patterns(element);
            }
        }
        let explanation = explain(&controls, truncated);
        Ok(Tree {
            window,
            controls,
            truncated,
            observed_at_ms: now_ms(),
            explanation,
        })
    }

    fn resolve(
        &self,
        window: WindowId,
        locator: &Locator,
        settings: &Settings,
    ) -> WindowsResult<Vec<Control>> {
        let session = open_session(window)?;
        let Some(root) = session.root.clone() else {
            return Ok(Vec::new());
        };
        let found = resolve_in(&session, &root, locator, settings.max_nodes)?;
        let mut controls: Vec<Control> = found
            .iter()
            .map(|element| to_control(element, Some(0)))
            .collect();
        if settings.probe_patterns {
            for (control, element) in controls.iter_mut().zip(found.iter()) {
                control.patterns = probe_patterns(element);
            }
        }
        if locator.is_path() {
            controls = with_depth(controls, 1);
        }
        Ok(controls)
    }

    fn probe(&self, window: WindowId, settings: &Settings) -> WindowsResult<Probe> {
        let session = match open_session(window) {
            Ok(session) => session,
            Err(error) => {
                return Ok(Probe {
                    window,
                    has_root_element: false,
                    frame_class: None,
                    control_count: 0,
                    kinds: Vec::new(),
                    patterns: Vec::new(),
                    explanation: TreeExplanation::Unreachable(error.to_string()),
                    automation_available: false,
                    observed_at_ms: now_ms(),
                })
            }
        };
        let Some(root) = session.root.clone() else {
            return Ok(Probe {
                window,
                has_root_element: false,
                // Read from the window itself rather than from a UIA element, because there is no UIA
                // element: the class name is the one fact about this frame that is still obtainable, and
                // it is the fact that lets a report say "this looks like an embedded browser surface".
                frame_class: crate::identity::class_name_of(window),
                control_count: 0,
                kinds: Vec::new(),
                patterns: Vec::new(),
                explanation: TreeExplanation::NoElements,
                automation_available: session.available,
                observed_at_ms: now_ms(),
            });
        };

        let frame_class = root.get_classname().ok().filter(|class| !class.is_empty());
        let (found, truncated) = descendants(&session, &root, settings.max_nodes)?;
        let controls: Vec<Control> = found
            .iter()
            .map(|element| to_control(element, None))
            .collect();

        let mut kinds: Vec<ControlKind> = Vec::new();
        let mut patterns: Vec<Pattern> = Vec::new();
        for (control, element) in controls.iter().zip(found.iter()) {
            if !kinds.contains(&control.kind) {
                kinds.push(control.kind.clone());
            }
            for pattern in probe_patterns(element) {
                if !patterns.contains(&pattern) {
                    patterns.push(pattern);
                }
            }
        }

        Ok(Probe {
            window,
            has_root_element: true,
            frame_class,
            control_count: controls.len(),
            kinds,
            patterns,
            explanation: explain(&controls, truncated),
            automation_available: session.available,
            observed_at_ms: now_ms(),
        })
    }

    fn read_state(
        &self,
        window: WindowId,
        locator: &Locator,
        settings: &Settings,
    ) -> WindowsResult<StateRead> {
        let session = open_session(window)?;
        let Some(root) = session.root.clone() else {
            // Nothing to address. Not "the control did not change" — there is no element here to change.
            return Ok(StateRead::Absent);
        };
        let found = resolve_in(&session, &root, locator, settings.max_nodes)?;
        match found.len() {
            0 => Ok(StateRead::Absent),
            1 => {
                let element = &found[0];
                let mut control = to_control(element, None);
                control.patterns = probe_patterns(element);
                let state = read_state(element, &control);
                Ok(StateRead::Found(Box::new(ControlState { control, state })))
            }
            count => Ok(StateRead::Ambiguous { count }),
        }
    }

    fn act(
        &self,
        window: WindowId,
        locator: &Locator,
        action: &Action,
        witness: Option<&Locator>,
        settings: &Settings,
    ) -> WindowsResult<Effect> {
        let session = open_session(window)?;
        let root = session
            .root
            .clone()
            .ok_or_else(|| WindowsError::AccessibilityUnavailable {
                reason: format!(
                    "window {} reported no UI Automation element, so nothing could be addressed",
                    window.hwnd
                ),
            })?;

        // Re-resolution is the first thing this does and the last thing it can be relied on. A stale plan
        // is allowed to *name* a control; it is not allowed to touch one it found earlier.
        let found = resolve_in(&session, &root, locator, settings.max_nodes)?;
        match found.len() {
            0 => {
                return Err(WindowsError::StaleElement {
                    control: locator.describe(),
                })
            }
            1 => {}
            count => {
                return Err(WindowsError::Ambiguous {
                    locator: locator.describe(),
                    count,
                })
            }
        }
        let element = &found[0];
        let control = to_control(element, None);
        let mut control = control;
        control.patterns = probe_patterns(element);

        let before = read_state(element, &control);

        // The witness, read on the same thread and inside the same bound as the action it is meant to
        // prove. Reading it here rather than in the caller is what makes the pair atomic: a caller that
        // read the status line before `act` could be reading it before something else changed it, and a
        // caller that read it afterwards could be reading it after the next action of its own plan.
        let witness_before =
            witness.map(|locator| read_witness(&session, &root, locator, settings.max_nodes));

        let required = Pattern::required_by(action);
        if let Some(pattern) = required {
            if !control.patterns.contains(&pattern) {
                return Err(WindowsError::PatternUnsupported {
                    control: control.fingerprint(),
                    pattern: pattern.name().to_owned(),
                });
            }
        }

        // The pattern call itself. Errors here are *not* converted into an uncertain outcome: a refusal
        // from the provider before it accepted anything leaves the world unchanged, and the caller is
        // entitled to know the difference between that and a call that may have landed.
        perform(element, action, &control)?;

        // Verification. The element may be gone, the tree may have been rebuilt under the action, and
        // either case makes the outcome unknown rather than unchanged — which is the state that must
        // never be retried automatically.
        // The verification read. `relocate` is tried first because the tree may have been rebuilt under
        // the action and the element held across a cross-process call is not guaranteed to still name the
        // thing it named; if relocation fails, the original element is read anyway, because a provider
        // that answers the old pointer is still an answer.
        let after: Option<Readback> = match relocate(&session, &root, &control, settings.max_nodes)
        {
            Ok(fresh) => {
                let observed = to_control(&fresh, None);
                Some(read_state(&fresh, &observed))
            }
            Err(_) => Some(read_state(element, &control)),
        };

        // A digest that cannot differ is not a failure and not a success: some controls legitimately
        // report nothing observable, and calling that "nothing happened" would be its own kind of wrong.
        let observable = after
            .as_ref()
            .map(|state| state.digest() != "<nothing observable>")
            .unwrap_or(false);
        let uncertain = action.has_side_effects() && !observable;
        let uncertainty_reason = if uncertain {
            Some(
                "the control reported no observable state, so the action's outcome could not be seen"
                    .to_owned(),
            )
        } else {
            None
        };

        let changed = after
            .as_ref()
            .map(|after| after.digest() != before.digest())
            .unwrap_or(false);

        // The witness, read again now that the action has had its effect. Only the pair is evidence: a
        // single post-action read tells you what the control says today, which is what the pre-action read
        // already said.
        let witness_effect = match (witness, witness_before) {
            (Some(locator), Some((before, _))) => {
                // The pre-action digest is taken first, because `before` is moved into the record below
                // and the comparison has to be against the state the action started from.
                let before_digest = before.digest();
                let (after, unresolved) =
                    read_witness(&session, &root, locator, settings.max_nodes);
                Some(Witness {
                    locator: locator.clone(),
                    before,
                    // An unresolved witness stays `None` after, rather than becoming an empty readback:
                    // "the witness control is gone" and "the witness control says nothing" are different
                    // observations, and the first is usually the one that matters.
                    after: if unresolved {
                        None
                    } else {
                        Some(after.clone())
                    },
                    // The comparison is against the *pre-action* digest, captured above: an `after` that
                    // agrees with a later `after` proves nothing about the action at all.
                    changed: !unresolved && after.digest() != before_digest,
                    unresolved,
                })
            }
            _ => None,
        };

        Ok(Effect {
            control,
            action: action.clone(),
            // An action without a required pattern (`SetFocus`) is recorded as carrying no pattern, which
            // is reported as `Invoke`-shaped only because `Pattern` has to name something; `SetFocus` is
            // the one case, and the caller checks `action` before `pattern`.
            pattern: required.unwrap_or(Pattern::Invoke),
            before,
            after,
            changed,
            witness: witness_effect,
            uncertain,
            uncertainty_reason,
            foreground_after: foreground(),
        })
    }
}

/// Read one witness control's observable state.
///
/// Returns the readback and whether the locator failed to resolve. A witness that cannot be found is a
/// plan pointing at the wrong control, and that is reported rather than being smoothed into "nothing
/// changed" — the two have opposite meanings about what to do next.
fn read_witness(
    session: &Session,
    root: &UIElement,
    locator: &Locator,
    max_nodes: usize,
) -> (Readback, bool) {
    match resolve_in(session, root, locator, max_nodes) {
        Ok(found) if found.len() == 1 => {
            let element = &found[0];
            let control = to_control(element, None);
            (read_state(element, &control), false)
        }
        // Zero matches, or more than one and therefore not usable as evidence either: a caller that named
        // an ambiguous control as its proof has no proof, and saying so is the whole answer.
        Ok(_) => (Readback::default(), true),
        Err(_) => (Readback::default(), true),
    }
}

/// Perform the pattern call the action names.
///
/// Split out so the failure modes stay legible: everything here is "the provider refused", and nothing
/// here is "we could not tell".
fn perform(element: &UIElement, action: &Action, control: &Control) -> WindowsResult<()> {
    use uiautomation::patterns::{
        UIExpandCollapsePattern, UIInvokePattern, UIScrollItemPattern, UISelectionItemPattern,
        UITogglePattern, UIValuePattern,
    };
    let refused = |call: &str, error: uiautomation::Error| WindowsError::PatternUnsupported {
        control: control.fingerprint(),
        pattern: format!("{call} refused: {error}"),
    };
    match action {
        Action::Invoke => {
            let pattern = element
                .get_pattern::<UIInvokePattern>()
                .map_err(|error| refused("Invoke", error))?;
            pattern.invoke().map_err(|error| refused("Invoke", error))
        }
        Action::SetValue { value } => {
            let pattern = element
                .get_pattern::<UIValuePattern>()
                .map_err(|error| refused("Value", error))?;
            // A read-only Value pattern is a refusal with a name, and it is worth naming: "the control
            // would not take the text" is what the operator needs, not a provider error code.
            if pattern.is_readonly().unwrap_or(false) {
                return Err(WindowsError::PatternUnsupported {
                    control: control.fingerprint(),
                    pattern: "Value is read-only".to_owned(),
                });
            }
            pattern
                .set_value(value)
                .map_err(|error| refused("SetValue", error))
        }
        Action::Toggle => {
            let pattern = element
                .get_pattern::<UITogglePattern>()
                .map_err(|error| refused("Toggle", error))?;
            pattern.toggle().map_err(|error| refused("Toggle", error))
        }
        Action::Select | Action::AddToSelection | Action::RemoveFromSelection => {
            let pattern = element
                .get_pattern::<UISelectionItemPattern>()
                .map_err(|error| refused("SelectionItem", error))?;
            match action {
                Action::Select => pattern.select().map_err(|error| refused("Select", error)),
                Action::AddToSelection => pattern
                    .add_to_selection()
                    .map_err(|error| refused("AddToSelection", error)),
                _ => pattern
                    .remove_from_selection()
                    .map_err(|error| refused("RemoveFromSelection", error)),
            }
        }
        Action::Expand | Action::Collapse => {
            let pattern = element
                .get_pattern::<UIExpandCollapsePattern>()
                .map_err(|error| refused("ExpandCollapse", error))?;
            match action {
                Action::Expand => pattern.expand().map_err(|error| refused("Expand", error)),
                _ => pattern
                    .collapse()
                    .map_err(|error| refused("Collapse", error)),
            }
        }
        Action::ScrollIntoView => {
            let pattern = element
                .get_pattern::<UIScrollItemPattern>()
                .map_err(|error| refused("ScrollItem", error))?;
            pattern
                .scroll_into_view()
                .map_err(|error| refused("ScrollIntoView", error))
        }
        Action::SetFocus => element
            .set_focus()
            .map_err(|error| refused("SetFocus", error)),
    }
}
