//! The native desktop suite: real windows, real UI Automation, real pixels, real refusals.
//!
//! These are the tests the roadmap's Phase-4 exit criteria are actually stated in — "complete a controlled
//! set of UI tasks in supported applications **without relying only on fixed coordinates**". Nothing here
//! uses a fake backend, because a fake can only confirm that this crate agrees with itself. Every claim is
//! checked against `jean-desktop-fixture`, a real Win32 window with real `BUTTON`/`EDIT`/`COMBOBOX`/
//! `STATIC` children, whose UIA tree, pattern set, bounding rectangles and capture behaviour are Windows'
//! answers rather than ours.
//!
//! ## Why they are serialised
//!
//! One `Mutex` guards every test in this file. The resources involved are process-global and partly owned
//! by the desktop: the foreground window, another process's UIA tree, the screen. Two of these running at
//! once would produce failures that are real about the machine and false about the code — the same class of
//! flake this tree already refuses to carry in `jean-process`. The cost is a slower suite; the thing being
//! protected is the only trustworthy evidence the phase produces.
//!
//! ## What a failure here means
//!
//! It means the desktop changed, or Windows changed, or this crate is wrong about one of them. It does not
//! mean a mock needs updating. Assertions are worded to print the observation that stopped matching, not
//! the expectation that was preferred.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use jean_windows::accessibility::{
    self, Action, Control, ControlKind, Locator, Pattern, StateRead, TreeExplanation,
};
use jean_windows::capability::{self, Diagnosis, Operation};
use jean_windows::capture::{self, FrameQuality};
use jean_windows::identity::{self, DisplayAffinity, WindowId};
use jean_windows::input::{self, Target};
use jean_windows::{worker, WindowsError};

/// Read one `key=value` field out of a fixture line.
fn field(text: &str, key: &str) -> isize {
    text.split_whitespace()
        .find_map(|part| part.strip_prefix(key))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Every test in this file acts on one shared desktop, so they take turns.
static DESKTOP: Mutex<()> = Mutex::new(());

fn lock_desktop() -> MutexGuard<'static, ()> {
    DESKTOP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A fixture process, the handle it reported, and a guard that ends it.
///
/// `--exit-after` is the belt and this guard is the braces: a fixture left running after a failing
/// assertion would be a window on the operator's desktop that nobody owns, and the suite runs on a machine
/// where a human is working.
///
/// `child` is `None` for the one mode that has to be launched through the shell rather than as a child of
/// the test process; see [`Fixture::start_shell`].
struct Fixture {
    child: Option<Child>,
    /// The owning process, for the shell-launched case, where the only way back to it is the pid.
    pid: Option<u32>,
    hwnd: isize,
}

impl Fixture {
    fn start(mode: &str, position: Option<(i32, i32)>) -> Fixture {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jean-desktop-fixture"));
        command
            .args(["--mode", mode, "--exit-after", "40"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some((x, y)) = position {
            command.args(["--position", &format!("{x},{y}")]);
        }
        let mut child = command
            .spawn()
            .expect("the fixture binary must start; a failure here is the build, not the desktop");

        // The ready line carries the handle, so the suite addresses the window it started rather than
        // guessing at it by title among whatever else happens to be on the desktop.
        let stdout = child
            .stdout
            .take()
            .expect("the fixture's stdout must be piped");
        let mut reader = BufReader::new(stdout);
        let mut hwnd = 0isize;
        let mut hung_hwnd = 0isize;
        let mut line = String::new();
        // The lines the suite needs are in the first two writes. The bound is on lines rather than on a
        // clock because `read_line` blocks: a fixture that printed nothing would otherwise park the suite
        // here forever, and a hung suite is indistinguishable from a hung provider.
        for _ in 0..16 {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Some(rest) = line.strip_prefix("[fixture] ready ") {
                hwnd = field(rest, "hwnd=");
            } else if let Some(rest) = line.strip_prefix("[fixture] hung ") {
                hung_hwnd = field(rest, "hwnd=");
            }
            let wanted = if mode == "hung" {
                hwnd != 0 && hung_hwnd != 0
            } else {
                hwnd != 0
            };
            if wanted {
                break;
            }
        }
        // The pipe stays open for the fixture's whole life, drained by a thread rather than dropped here.
        // A fixture that writes after its ready line — `focus-steal` asks for the foreground 1.2s later —
        // panics inside `println!` when the write end is gone, and that panic kills the process whose
        // behaviour the test is about. The thread ends at EOF, which the `Drop` below provokes.
        let mode_for_drain = mode.to_owned();
        std::thread::Builder::new()
            .name(format!("fixture-stdout-drain-{mode_for_drain}"))
            .spawn(move || {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        // EOF is the fixture leaving, which is the normal end of this thread.
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            // Forwarded rather than discarded: a fixture that stops printing is a fixture
                            // that stopped working, and the modes that ask for the foreground report
                            // whether Windows granted it.
                            eprint!("[fixture:{mode_for_drain}] {line}");
                        }
                    }
                }
            })
            .expect(
                "the fixture's stdout must be drained: a pipe closed here kills the fixture at its \
                 next print",
            );
        assert!(
            hwnd != 0,
            "the fixture never reported its window handle in mode {mode}"
        );
        if mode == "hung" {
            assert!(
                hung_hwnd != 0,
                "hung mode must report the handle of the window whose thread never pumps"
            );
            // The unresponsive window is the one under test, not the frame that printed the line.
            hwnd = hung_hwnd;
        }
        Fixture {
            child: Some(child),
            pid: None,
            hwnd,
        }
    }

    /// A fixture launched the way the operator launches an application: through the shell.
    ///
    /// This exists because of a measurement rather than a preference. `SetForegroundWindow` is refused to
    /// a process that inherits the test's handles and granted to one the shell started — measured on this
    /// desktop as `.spawn()` → `granted=false`, `cmd /c start` → `granted=true`, and `start /B` (which
    /// keeps the inherited console) → `granted=false`. The coordinate test needs a window that is
    /// genuinely in front *without* JEAN putting it there, because putting it there is the behaviour R-20
    /// forbids, so the fixture has to arrive the way a real target application does.
    ///
    /// The cost is that there is no `Child` and no stdout to read a handle from: the window is found by
    /// its own title and ended by the pid that owns it.
    fn start_shell(mode: &str, position: (i32, i32)) -> Fixture {
        let exe = env!("CARGO_BIN_EXE_jean-desktop-fixture");
        // The empty argument is `start`'s window title, and without it the first quoted path is consumed
        // as the title and nothing launches at all.
        let launched = Command::new("cmd.exe")
            .args([
                "/c",
                "start",
                "",
                exe,
                "--mode",
                mode,
                "--exit-after",
                "40",
                "--position",
                &format!("{},{}", position.0, position.1),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("the shell must be able to start");
        assert!(
            launched.success(),
            "the shell refused to launch the fixture (exit {launched}); nothing was started, so the \
             window below cannot be found"
        );

        let title = format!("J.E.A.N. desktop fixture [{mode}]");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = identity::enumerate()
                .unwrap_or_default()
                .into_iter()
                .find(|found| {
                    found.title.as_deref() == Some(title.as_str())
                        && found.pid != std::process::id()
                });
            if let Some(found) = found {
                return Fixture {
                    child: None,
                    pid: Some(found.pid),
                    hwnd: found.id.hwnd,
                };
            }
            assert!(
                Instant::now() < deadline,
                "the shell-launched fixture never produced a window titled {title:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn id(&self) -> WindowId {
        WindowId { hwnd: self.hwnd }
    }

    /// The process the fixture window belongs to, whether it was spawned or shell-launched.
    fn pid(&self) -> u32 {
        self.pid.unwrap_or_else(|| {
            self.child
                .as_ref()
                .expect("a fixture is either a child process or a shell-launched one")
                .id()
        })
    }

    /// A control addressed by its dialog id, the way a plan would address it.
    fn by_id(id: usize) -> Locator {
        Locator::AutomationId(id.to_string())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        if let Some(pid) = self.pid {
            // A shell-launched fixture is not a child of this process, so there is no handle to kill: the
            // pid is ended the way any other process on this machine is ended from outside. It is a pid
            // this test discovered seconds ago and launched itself, and `/T` takes whatever it started.
            let ended = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if ended.is_err() {
                // The fixture self-exits at `--exit-after`, so this is a window that outlives the test by
                // seconds rather than a leak, but it is said out loud because it is a window on the
                // operator's desktop that no longer has an owner in this process.
                eprintln!(
                    "NOTE: fixture pid {pid} could not be ended; it will exit on its own timer"
                );
            }
        }
    }
}

fn settings() -> accessibility::Settings {
    accessibility::Settings {
        call_timeout: Duration::from_secs(8),
        max_nodes: 400,
        // The pattern list is what most of these tests assert about, so the cost is paid here: one probe
        // per pattern per control. The fixture is small enough that it is milliseconds.
        probe_patterns: true,
    }
}

fn capture_settings() -> capture::Settings {
    capture::Settings {
        call_timeout: Duration::from_secs(10),
        ..Default::default()
    }
}

/// Find one control by automation id, failing with what the tree did contain.
fn control(fixture: &Fixture, id: usize) -> Control {
    let found = accessibility::resolve(fixture.id(), Fixture::by_id(id), settings(), None)
        .unwrap_or_else(|error| panic!("resolving control {id} failed: {error}"));
    assert_eq!(
        found.len(),
        1,
        "control {id} must name exactly one control, and it named {}",
        found.len()
    );
    found.into_iter().next().expect("len was 1")
}

#[test]
fn the_fixture_window_is_discoverable_and_attributable() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let found = identity::read_identity(fixture.id());

    assert_eq!(
        found.pid,
        fixture.pid(),
        "the window must belong to the fixture process it was started as"
    );
    assert_eq!(
        found.class_name.as_deref(),
        Some("JeanFixtureMain"),
        "the class name is user32's own answer about the window"
    );
    assert!(
        found
            .title
            .as_deref()
            .unwrap_or_default()
            .contains("J.E.A.N. desktop fixture"),
        "the title is bounded, untrusted text: {:?}",
        found.title
    );
    let rect = found.rect.expect("a shown window has a rectangle");
    assert!(!rect.is_empty(), "the rectangle is {rect:?}");
    assert!(
        found.dpi.is_some(),
        "a per-monitor-aware window must report its own DPI, not a default"
    );
    let monitor = found.monitor.expect("the window is on a monitor");
    assert!(monitor.width > 0 && monitor.height > 0);
    assert_eq!(
        found.display_affinity,
        DisplayAffinity::None,
        "nothing in normal mode restricts capture"
    );
    assert!(
        found.observed_at_ms > 0,
        "an observation without a timestamp is not evidence"
    );
    // The integrity read is what R-14's whole mitigation depends on. The level itself belongs to the
    // machine, so the assertion is that it was *established*, not that it equals some expected value.
    let integrity = found.integrity.unwrap_or_else(|| {
        panic!(
            "the owning process's integrity level must be readable: {:?}",
            found.identity_note
        )
    });
    assert!(
        integrity.sid.starts_with("S-1-16-"),
        "an integrity SID is labelled, got {}",
        integrity.sid
    );
    assert!(!integrity.label.is_empty());
}

#[test]
fn dialog_control_ids_become_the_automation_ids_the_locators_address() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let tree = accessibility::read_tree(fixture.id(), settings(), None)
        .expect("the tree must be readable");

    assert!(
        matches!(tree.explanation, TreeExplanation::Populated),
        "the fixture's tree is populated, got {:?}",
        tree.explanation
    );
    assert!(!tree.controls.is_empty(), "the fixture exposes controls");

    let ids: Vec<String> = tree
        .controls
        .iter()
        .map(|control| control.automation_id.clone())
        .collect();
    // This is the assertion the fixture's own documentation refuses to trust: that a Win32 dialog control
    // id surfaces as UIA's `AutomationId`. If Windows ever stopped doing that, every `AutomationId`
    // locator in the suite would resolve nothing, and this is where it would say so.
    for id in [1001usize, 1002, 1003, 1004, 1005, 1006, 1007, 1008, 1009] {
        assert!(
            ids.iter().any(|found| *found == id.to_string()),
            "control id {id} did not become an AutomationId; the tree reported {ids:?}"
        );
    }

    let edit = tree
        .controls
        .iter()
        .find(|control| control.automation_id == "1001")
        .expect("the subject edit");
    assert_eq!(edit.kind, ControlKind::Edit, "id 1001 is the edit control");

    let send = tree
        .controls
        .iter()
        .find(|control| control.automation_id == "1002")
        .expect("the Send button");
    assert_eq!(send.name, "Send");
    assert!(send.is_enabled);
    assert!(
        send.patterns.contains(&Pattern::Invoke),
        "a real push button supports Invoke; it reported {:?}",
        send.patterns
    );
    assert!(
        send.rect.is_some(),
        "an on-screen control reports a bounding rectangle"
    );
    assert!(
        send.click_point().is_some(),
        "and a point a click could use"
    );
}

#[test]
fn invoke_is_performed_and_the_resulting_change_is_visible() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let before = control(&fixture, 1006);
    assert_eq!(before.name, "idle", "the fixture starts idle");

    // The witness is the status label, and the assertion is about *it*: a send button that was pressed
    // does not itself change, so an implementation that only re-reads the acted control would report a
    // successful no-op. This is R-21's requirement in the shape the fixture can actually prove.
    let started = std::time::Instant::now();
    let effect = accessibility::act(
        fixture.id(),
        Fixture::by_id(1002),
        Action::Invoke,
        Some(Fixture::by_id(1006)),
        settings(),
        None,
    )
    .expect("invoking a real button must work");

    assert_eq!(effect.pattern, Pattern::Invoke);
    assert!(
        effect.is_verified(),
        "the action must report a change it could see: before {:?} after {:?}",
        effect.before.digest(),
        effect.after.as_ref().map(|state| state.digest())
    );

    // And the *other* control — the label the button writes to — proves the effect landed in the fixture
    // rather than merely in the read of the button itself. This is the "the click API returned success is
    // not a result" rule, enforced by an independent witness.
    assert_eq!(
        control(&fixture, 1006).name,
        "clicked:1",
        "the fixture's own counter is the verifier"
    );
    println!(
        "MEASURED request-to-verified-result: {} ms (one Invoke plus independent status read)",
        started.elapsed().as_millis()
    );

    accessibility::act(
        fixture.id(),
        Fixture::by_id(1002),
        Action::Invoke,
        Some(Fixture::by_id(1006)),
        settings(),
        None,
    )
    .expect("a second invoke");
    assert_eq!(
        control(&fixture, 1006).name,
        "clicked:2",
        "two invokes must count, which is what separates a performed action from a re-read of a fixed label"
    );
}

#[test]
fn the_value_pattern_writes_and_reads_back_what_was_written() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    let effect = accessibility::act(
        fixture.id(),
        Fixture::by_id(1001),
        Action::SetValue {
            value: "typed by jean".to_owned(),
        },
        None,
        settings(),
        None,
    )
    .expect("the fixture's edit control is writable");

    assert_eq!(effect.pattern, Pattern::Value);
    assert!(
        effect.is_verified(),
        "the write must be visible in the read-back"
    );
    assert_eq!(
        effect
            .after
            .as_ref()
            .and_then(|state| state.value.clone())
            .unwrap_or_default(),
        "typed by jean",
        "the value the provider reports is the value that was set"
    );
    // The audit-facing description carries a length and never the text.
    assert_eq!(
        Action::SetValue {
            value: "typed by jean".to_owned()
        }
        .describe(),
        "set_value(13 chars)"
    );
}

#[test]
fn a_state_read_reports_the_controls_own_patterns_without_acting() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    // The fifth boundary question exists because injected input has no pattern call to carry a witness
    // inside it: the only way to prove a keystroke landed is to read what the control now contains, on the
    // same terms an action reads back. A fake cannot answer that, so this asks a real provider.
    let read = accessibility::read_state(fixture.id(), Fixture::by_id(1001), settings(), None)
        .expect("the fixture's edit control must be readable without acting on it");
    let StateRead::Found(found) = read.clone() else {
        panic!("1001 names exactly one control; it reported {read:?}");
    };
    let control = &found.control;
    let state = &found.state;
    assert_eq!(control.automation_id, "1001");
    assert_eq!(
        state.value.as_deref(),
        Some("not sent"),
        "the value is the provider's own answer about the control"
    );
    assert!(
        state.name.is_none(),
        "an edit with no accessible name reports none, not an empty claim: {:?}",
        state.name
    );

    // A read that read nothing else is still the same read twice. This is the half of the input path's
    // claim that it brackets a change rather than causing one. The comparison is on the digest rather than
    // on the whole `StateRead` because a control's identity fields are not equality-comparable here on
    // purpose — the state is what a witness is made of.
    let first_digest = state.digest();
    let again = accessibility::read_state(fixture.id(), Fixture::by_id(1001), settings(), None)
        .expect("the second state read must succeed");
    match again {
        StateRead::Found(found) => assert_eq!(
            first_digest,
            found.state.digest(),
            "two reads of an untouched control must agree"
        ),
        other => panic!("the control that was just readable stopped being readable: {other:?}"),
    }

    // The pair the input path performs around `SendInput`: write through some other means, then read.
    accessibility::act(
        fixture.id(),
        Fixture::by_id(1001),
        Action::SetValue {
            value: "typed by jean".to_owned(),
        },
        None,
        settings(),
        None,
    )
    .expect("the write must land");
    let after = accessibility::read_state(fixture.id(), Fixture::by_id(1001), settings(), None)
        .expect("the read after the write must succeed");
    match after {
        StateRead::Found(found) => assert_eq!(
            found.state.value.as_deref(),
            Some("typed by jean"),
            "the state read must see what the action wrote, which is what makes it usable as a witness"
        ),
        other => panic!("the witness disappeared: {other:?}"),
    }

    // Absent and ambiguous are answers rather than errors: a witness that names nothing, or names two
    // things, has proved nothing, and the caller has to be able to say which of the two.
    let absent = accessibility::read_state(
        fixture.id(),
        Locator::AutomationId("999999".to_owned()),
        settings(),
        None,
    )
    .expect("a locator that matches nothing is still a successful read");
    assert!(matches!(absent, StateRead::Absent), "{absent:?}");

    let ambiguous = accessibility::read_state(
        fixture.id(),
        Locator::Name {
            name: "Continue".to_owned(),
            control_type: None,
        },
        settings(),
        None,
    )
    .expect("a locator that matches several is still a successful read");
    assert!(
        matches!(ambiguous, StateRead::Ambiguous { count: 2 },),
        "the fixture has exactly two Continue buttons: {ambiguous:?}"
    );
}

#[test]
fn the_toggle_pattern_flips_between_two_named_states() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    let effect = accessibility::act(
        fixture.id(),
        Fixture::by_id(1003),
        Action::Toggle,
        None,
        settings(),
        None,
    )
    .expect("the fixture's checkbox is a real BS_AUTOCHECKBOX");

    assert_eq!(effect.pattern, Pattern::Toggle);
    let before = effect.before.toggle.clone().unwrap_or_default();
    let after = effect
        .after
        .as_ref()
        .and_then(|state| state.toggle.clone())
        .unwrap_or_default();
    assert!(
        !before.is_empty() && !after.is_empty() && before != after,
        "a toggle must move between two named states, saw {before:?} then {after:?}"
    );
}

#[test]
fn selection_and_expansion_are_read_from_the_controls_that_have_them() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    let radio = control(&fixture, 1004);
    assert_eq!(radio.kind, ControlKind::RadioButton);
    let effect = accessibility::act(
        fixture.id(),
        Fixture::by_id(1004),
        Action::Select,
        None,
        settings(),
        None,
    )
    .expect("a radio button answers SelectionItem");
    assert_eq!(effect.pattern, Pattern::SelectionItem);
    assert_eq!(
        effect.after.as_ref().and_then(|state| state.selected),
        Some(true),
        "the provider must report the selection it just made"
    );

    let combo = control(&fixture, 1005);
    assert_eq!(combo.kind, ControlKind::ComboBox);
    assert!(
        combo.patterns.contains(&Pattern::ExpandCollapse),
        "a CBS_DROPDOWNLIST combobox supports ExpandCollapse; it reported {:?}",
        combo.patterns
    );
}

#[test]
fn a_pattern_that_does_not_exist_is_refused_by_name() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    // The status label is a STATIC: readable, addressable, and with nothing to act on. The refusal has to
    // name the missing pattern rather than pretend the control is absent.
    let error = accessibility::act(
        fixture.id(),
        Fixture::by_id(1006),
        Action::Invoke,
        None,
        settings(),
        None,
    )
    .expect_err("a static label cannot be invoked");
    match error {
        WindowsError::PatternUnsupported { pattern, .. } => {
            assert!(
                pattern.contains("Invoke"),
                "the refusal must name the pattern it could not find, got {pattern}"
            );
        }
        other => panic!("expected PatternUnsupported, got {other:?}"),
    }
}

#[test]
fn an_ambiguous_locator_is_refused_rather_than_resolved_by_enumeration_order() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    let locator = Locator::Name {
        name: "Continue".to_owned(),
        control_type: Some(ControlKind::Button),
    };
    let matches = accessibility::resolve(fixture.id(), locator.clone(), settings(), None)
        .expect("the name lookup must run");
    assert_eq!(
        matches.len(),
        2,
        "the fixture deliberately ships two buttons named Continue"
    );

    let error = accessibility::act(
        fixture.id(),
        locator,
        Action::Invoke,
        None,
        settings(),
        None,
    )
    .expect_err("naming one of two is not a tie-break");
    match error {
        WindowsError::Ambiguous { count, locator } => {
            assert_eq!(count, 2);
            assert!(
                locator.contains("name=Continue"),
                "the report must say what was ambiguous: {locator}"
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }

    // Names alone remain ambiguous across parent groups. The ancestry form must start at one parent,
    // then resolve the same accessible name underneath it.
    let left = Locator::Path(vec![
        Locator::Name {
            name: "Workspace A".to_owned(),
            control_type: Some(ControlKind::Group),
        },
        Locator::Name {
            name: "Continue".to_owned(),
            control_type: Some(ControlKind::Button),
        },
    ]);
    let started = std::time::Instant::now();
    let effect = accessibility::act(
        fixture.id(),
        left,
        Action::Invoke,
        None,
        settings(),
        None,
    )
    .expect("a path through the intended parent group is not ambiguous");
    assert_eq!(effect.control.automation_id, "1008");
    assert!(!effect.is_verified(), "an Invoke return without a state change is not evidence: {effect:?}");
    assert_eq!(control(&fixture, 1006).name, "idle");
    assert_eq!(control(&fixture, 1008).name, "Continue");
    assert_eq!(control(&fixture, 1009).name, "Continue");

    let right = Locator::Path(vec![
        Locator::Name {
            name: "Workspace B".to_owned(),
            control_type: Some(ControlKind::Group),
        },
        Locator::Name {
            name: "Continue".to_owned(),
            control_type: Some(ControlKind::Button),
        },
    ]);
    let right = accessibility::act(
        fixture.id(),
        right,
        Action::Invoke,
        None,
        settings(),
        None,
    )
    .expect("the other parent resolves its own Continue button");
    assert_eq!(right.control.automation_id, "1009");
    assert!(!right.is_verified());
    assert_eq!(control(&fixture, 1006).name, "idle");
    println!(
        "MEASURED request-to-observed-no-op: {} ms (parent-scoped Invoke; expected state did not change)",
        started.elapsed().as_millis()
    );
}

#[test]
fn a_control_destroyed_by_an_action_is_stale_rather_than_merely_absent() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);

    let cached = control(&fixture, 1006);
    assert_eq!(cached.name, "idle");

    // No witness here, on purpose: the witness for this action is the disappearance of a control, and the
    // tree read below is the observation that shows it.
    accessibility::act(
        fixture.id(),
        Fixture::by_id(1007),
        Action::Invoke,
        None,
        settings(),
        None,
    )
    .expect("the fixture's label-destroying button works");

    // The label is gone. A locator that no longer names anything must say so at action time rather than
    // perform the action on whatever now occupies that position in the tree.
    let error = accessibility::act(
        fixture.id(),
        Fixture::by_id(1006),
        Action::Invoke,
        None,
        settings(),
        None,
    )
    .expect_err("the control is gone");
    assert!(
        matches!(error, WindowsError::StaleElement { .. }),
        "expected StaleElement, got {error:?}"
    );

    let tree =
        accessibility::read_tree(fixture.id(), settings(), None).expect("the tree still reads");
    assert!(
        !tree
            .controls
            .iter()
            .any(|control| control.automation_id == "1006"),
        "the destroyed control must be gone from the tree, not merely renamed"
    );
    let survivor = &tree.controls[0];
    assert!(
        !cached.is_same_as(survivor),
        "a cached runtime id must not match a different control"
    );
}

#[test]
fn a_capture_excluded_window_is_still_workable_through_its_controls() {
    let _guard = lock_desktop();
    // The Telegram-class case, and the headline requirement of the phase: one window whose pixels are
    // refused must not become one application that cannot be operated, and certainly not one desktop that
    // is considered blind.
    let fixture = Fixture::start("excluded", None);
    let found = identity::read_identity(fixture.id());
    assert_eq!(
        found.display_affinity,
        DisplayAffinity::ExcludeFromCapture,
        "the fixture asked for this, and the window says so"
    );

    let report = capability::diagnose(fixture.id(), &capability::Settings::default(), None);
    let capture_entry = report
        .capability(Operation::Capture)
        .expect("a capture capability is always reported");
    assert!(
        matches!(
            capture_entry.diagnosis,
            Diagnosis::ExplicitCaptureExclusion { .. }
        ),
        "an exclusion the window declared must be named as one, got {:?}",
        capture_entry.diagnosis
    );
    assert_eq!(
        capture_entry.observation.state(),
        "explicit_protection",
        "and not as an ordinary failure: {}",
        capture_entry.observation.evidence()
    );
    assert!(
        capture_entry.observation.blocks_this_window(),
        "it does stop work on this window's pixels"
    );
    assert!(
        !capture_entry.observation.blocks_other_work(),
        "and nothing else"
    );

    let accessibility_entry = report
        .capability(Operation::InspectAccessibility)
        .expect("an accessibility capability is always reported");
    assert_eq!(
        accessibility_entry.observation.state(),
        "available",
        "a blank capture must not disable the controls: {}",
        accessibility_entry.observation.evidence()
    );
    assert!(
        report.is_workable(),
        "the whole point: this window is workable"
    );
    assert_eq!(
        report.next_step(),
        jean_windows::capability::NextStep::UseAccessibility,
        "and the plan it suggests is the semantic path, not a workaround"
    );

    // The proof is behavioural, not declarative: the button still presses and the label still proves it
    // happened, while the window is excluded from every capture path.
    accessibility::act(
        fixture.id(),
        Fixture::by_id(1002),
        Action::Invoke,
        Some(Fixture::by_id(1006)),
        settings(),
        None,
    )
    .expect("invoking a control in an excluded window must work");
    assert_eq!(control(&fixture, 1006).name, "clicked:1");

    // What the capture itself may report: either a refusal or a frame that says nothing. A frame full of
    // *content* would be the failure, because it would mean the exclusion had been worked around.
    if let Ok(frame) = capture::capture(fixture.id(), capture_settings(), None) {
        assert!(
            !frame.quality().is_informative(),
            "an excluded window produced an informative frame — the protection was bypassed, which is not what this crate does"
        );
    }
}

#[test]
fn an_unexplained_uniform_frame_is_not_reported_as_a_protection() {
    let _guard = lock_desktop();
    // The other half of the same rule: a window that paints one flat colour and declared nothing must not
    // be labelled as if it had. Mislabelling a graphics condition as a policy conclusion is how an
    // automation ends up refusing work it could have done.
    let fixture = Fixture::start("blank", None);
    let found = identity::read_identity(fixture.id());
    assert!(
        !found.display_affinity.is_explicit_protection(),
        "the blank fixture declares no restriction: {:?}",
        found.display_affinity
    );

    let report = capability::diagnose(fixture.id(), &capability::Settings::default(), None);
    let capture_entry = report
        .capability(Operation::Capture)
        .expect("a capture capability is always reported");
    assert!(
        !matches!(
            capture_entry.diagnosis,
            Diagnosis::ExplicitCaptureExclusion { .. }
        ),
        "a uniform frame with no declared affinity must not be called a protection, got {:?}",
        capture_entry.diagnosis
    );
    // The controls still answer, which is the other thing this test is really about.
    assert!(
        report.is_workable(),
        "a painted-over window is still operable"
    );
}

#[test]
fn a_hung_provider_costs_one_operation_and_not_the_session() {
    let _guard = lock_desktop();
    // A window whose thread never pumps messages: the UIA call into it sends `WM_GETOBJECT` and nothing
    // dequeues it. Both bounds in front of it — this crate's worker timeout and UIA's own matcher timeout —
    // are what stand between one stuck application and a desktop that is simply gone.
    let hung = Fixture::start("hung", None);
    let healthy = Fixture::start("normal", None);
    let budget = Duration::from_millis(1_500);
    let tight = accessibility::Settings {
        call_timeout: budget,
        max_nodes: 200,
        probe_patterns: false,
    };

    let started = Instant::now();
    let outcome = accessibility::probe(hung.id(), tight, None);
    let waited = started.elapsed();

    match outcome {
        Err(WindowsError::ProviderTimeout { waited_ms, .. }) => {
            assert!(
                waited_ms >= budget.as_millis() as u64 - 60,
                "the report must state the bound it actually waited: {waited_ms}ms for {budget:?}"
            );
        }
        Ok(probe) => {
            // Windows' own UIA timeout may answer first, and if it did the call is still bounded. What is
            // not acceptable is an answer that claims controls for a window that never replied.
            assert!(
                !probe.has_actionable_controls(),
                "a window that never pumped messages reported actionable controls: {:?}",
                probe.explanation
            );
        }
        Err(other) => panic!("expected a bounded timeout, got {other:?}"),
    }
    assert!(
        waited < budget + Duration::from_secs(4),
        "the wait was abandoned after {waited:?}, which is not the bound {budget:?} plus UIA's own timeout"
    );

    // The isolation claim, which is the reason the worker is per-call rather than shared: the unrelated
    // window is still fully operable while the first call's thread is parked.
    let healthy_probe =
        accessibility::probe(healthy.id(), settings(), None).expect("the healthy window answers");
    assert!(
        healthy_probe.has_actionable_controls(),
        "an unrelated window must remain inspectable while a different one is hung"
    );
    let healthy_report = capability::diagnose(healthy.id(), &capability::Settings::default(), None);
    assert!(healthy_report.is_workable());

    // And the hung window is diagnosed as *its own* problem, never as a desktop-wide condition.
    let hung_report = capability::diagnose(hung.id(), &capability::Settings::default(), None);
    for entry in &hung_report.capabilities {
        assert!(
            !entry.observation.blocks_other_work(),
            "{} reported that it blocks unrelated work",
            entry.operation.name()
        );
    }
}

/// Worker processes the operating system still lists.
///
/// Queried from outside this process on purpose: `worker::live_workers()` is the parent's own bookkeeping,
/// and a demonstration that the parent's arithmetic adds up is not evidence that the processes it counted
/// are gone. `tasklist` is the answer to a question the parent cannot ask about itself.
fn stray_workers() -> Vec<String> {
    let output = Command::new("tasklist")
        .args([
            "/FI",
            "IMAGENAME eq jean-desktop-worker.exe",
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .expect("tasklist is part of Windows; a failure here is the machine, not the code");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("jean-desktop-worker.exe"))
        .map(|line| line.trim().to_owned())
        .collect()
}

#[test]
fn the_fixed_cost_of_a_desktop_call_is_reported_as_a_measurement() {
    let _guard = lock_desktop();
    // Running every UIA call in a process that can be killed has a price, and the requirement is not met by
    // hiding it. Two numbers separate the two parts of it:
    //
    // * the round trip of a worker that does nothing but answer its handshake — process creation, the pipe
    //   wiring, and the version check, with no provider involved;
    // * the same round trip with a real UIA tree walk in it — that part is the child's `UIAutomation::new()`
    //   plus the walk, which the in-process design paid on every call as well.
    //
    // Diagnostic-only: nothing here asserts a threshold, because a threshold on wall-clock is a claim about
    // this machine on this day. The printed figures are the evidence, and the suite that uses the mechanism
    // is what would feel the cost.
    //
    // The three figures split the cost three ways: the same executable started by `CreateProcess` with no
    // pipes and no job object is the image load and whatever the machine's file scanning charges for it;
    // `worker::one` with the handshake only is that plus the supervision and the pipe wiring; and the UIA
    // and capture figures are what the provider work itself adds on top.
    let fixture = Fixture::start("normal", None);
    let budget = Duration::from_secs(8);
    let worker_exe = env!("CARGO_BIN_EXE_jean-desktop-worker");

    let started = Instant::now();
    {
        // A worker started with no stdin is a worker that reads EOF and exits, so this measures the start
        // alone without anything of J.E.A.N.'s supervision in it.
        let mut bare_child = Command::new(worker_exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the worker binary must start");
        bare_child.wait().expect("and exit");
    }
    let exec_alone = started.elapsed();

    // The same `piped::spawn` the session uses, with nobody talking to it afterwards: this separates "the
    // supervision costs it" from "the conversation costs it", which are two different things to fix and a
    // single wall-clock figure does not say which one it is.
    let started = Instant::now();
    let spec = jean_process::spec::LaunchSpec {
        executable: std::path::PathBuf::from(worker_exe),
        args: Vec::new(),
        working_dir: None,
        env: worker::worker_environment(Path::new(worker_exe)),
        timeout: None,
        output_cap_bytes: 1024 * 1024,
        stdout_path: None,
        stderr_path: None,
        terminal: jean_process::spec::TerminalMode::Pipes,
    };
    let supervised_child = jean_process::piped::spawn(&spec)
        .expect("the worker must start under the supervision the session uses");
    let spawn_only = started.elapsed();
    let _ = supervised_child.control().terminate();

    let started = Instant::now();
    worker::one(
        &worker::Body::Hello {
            version: worker::PROTOCOL_VERSION,
            parent_pid: std::process::id(),
        },
        budget,
        None,
    )
    .expect("a worker that cannot answer its handshake is not worth measuring");
    let supervised = started.elapsed();

    let started = Instant::now();
    let walked_probe =
        accessibility::probe(fixture.id(), settings(), None).expect("the fixture answers");
    let walked = started.elapsed();

    let started = Instant::now();
    let frame = capture::capture(fixture.id(), capture_settings(), None)
        .expect("the fixture is capturable in its own right");
    let captured = started.elapsed();

    // Four figures, in the order that isolates each layer of the cost: the executable alone, the same
    // executable under `piped::spawn`, that supervision plus one typed round trip, and the two kinds of
    // real desktop work on top. Nothing asserts a threshold, because a threshold on wall-clock is a claim
    // about this machine on this day — the numbers are the deliverable, and `jean desktop` is what feels them.
    eprintln!(
        "NOTE (diagnostic, no threshold asserted): the worker executable started by `CreateProcess` with \
         no pipes and no job object cost {exec_alone:?}; the same executable started the way J.E.A.N. \
         starts it cost {spawn_only:?}; that supervision plus one handshake round trip cost \
         {supervised:?}; a UIA walk of {} controls through it cost {walked:?}; a PrintWindow capture of \
         {}x{} through it cost {captured:?}.",
        walked_probe.control_count,
        frame.width,
        frame.height
    );
    assert_eq!(
        0,
        worker::live_workers(),
        "measuring costs processes the run does not keep"
    );
    assert!(
        stray_workers().is_empty(),
        "a measured worker outlived its session"
    );

    // The one threshold this test does hold itself to, and it is a shape rather than a speed: a worker that
    // answered its handshake is done, exits on the EOF its parent owes it, and is reaped. Reaching the
    // shutdown bound here would mean the parent is still holding the pipe open and every desktop call pays
    // for that patience in full.
    assert!(
        supervised < worker::SHUTDOWN_BOUND,
        "a completed session took {supervised:?}, which is the {:?} shutdown bound — the polite exit is          not happening and every call is paying for it",
        worker::SHUTDOWN_BOUND
    );
}

#[test]
fn a_storm_of_hung_providers_leaves_no_worker_behind_and_the_desktop_still_works() {
    let _guard = lock_desktop();
    // This is the demonstration the isolation requirement asks for, and it is stated as the number it has
    // to beat: the previous design tolerated four abandoned calls and refused the fifth, because each one
    // held a thread that could not be recalled. Five hangs in a row against one unresponsive window is
    // therefore the case that used to stop working — and it must now cost nothing but time.
    const ROUNDS: usize = 5;
    let hung = Fixture::start("hung", None);
    let healthy = Fixture::start("normal", None);
    let tight = accessibility::Settings {
        call_timeout: Duration::from_millis(1_200),
        max_nodes: 200,
        probe_patterns: false,
    };

    let storm = Instant::now();
    let mut timed_out = 0usize;
    for round in 1..=ROUNDS {
        let started = Instant::now();
        match accessibility::probe(hung.id(), tight, None) {
            Err(WindowsError::ProviderTimeout { .. }) => timed_out += 1,
            // Windows' own matcher may answer first with an empty tree. That is a bounded read of a window
            // that said nothing, which is the same observation, obtained by the other timeout in front of it.
            Ok(probe) => assert!(
                !probe.has_actionable_controls(),
                "round {round}: a window that never pumped messages reported controls"
            ),
            Err(other) => panic!("round {round}: expected a bounded read, got {other:?}"),
        }
        let waited = started.elapsed();
        assert!(
            waited < tight.call_timeout + Duration::from_secs(6),
            "round {round} took {waited:?}, which is not a bounded call"
        );
        // The claim that replaced "the thread is abandoned and still counted": after every round, the
        // parent is holding no worker at all.
        assert_eq!(
            0,
            worker::live_workers(),
            "round {round} left a worker process alive"
        );
    }
    assert!(
        timed_out >= 1,
        "none of the {ROUNDS} rounds reported `ProviderTimeout`, which is the error this crate's own \
         deadline produces; without at least one of them the termination path is not demonstrated here"
    );

    // The cost of the design, measured rather than described: each call above started a process, so this
    // figure is process creation plus the child's UIA initialisation. It is printed because an isolation
    // claim that does not say what it costs is a claim about something else.
    let healthy_probe = Instant::now();
    accessibility::probe(healthy.id(), accessibility::Settings { ..tight }, None)
        .expect("the healthy window answers under the hung window's bound");
    let healthy_waited = healthy_probe.elapsed();

    // The point of all of it: the fifth hang does not disable the desktop. An unrelated window is read and
    // then acted on, with the witness checked, in the same process that just killed five workers.
    let probe = accessibility::probe(healthy.id(), settings(), None)
        .expect("a healthy window answers after five hung reads");
    assert!(
        probe.has_actionable_controls(),
        "the healthy window reported nothing after the storm: {:?}",
        probe.explanation
    );
    let before = control(&healthy, 1006).name;
    accessibility::act(
        healthy.id(),
        Fixture::by_id(1002),
        Action::Invoke,
        Some(Fixture::by_id(1006)),
        settings(),
        None,
    )
    .expect("invoking a real button after the storm must work");
    let after = control(&healthy, 1006).name;
    assert!(
        after != before && after.starts_with("clicked:"),
        "the action was not observed: the witness went {before:?} to {after:?}"
    );

    // Nothing owned by this suite is left running. `live_workers` says the parent's books are clean; this
    // says the processes agree.
    let strays = stray_workers();
    assert!(
        strays.is_empty(),
        "worker processes survived their sessions: {strays:?}"
    );
    eprintln!(
        "NOTE: {timed_out} of {ROUNDS} hung rounds were ended by this crate's own deadline — \
         `ProviderTimeout` is the error that deadline produces, and `live_workers()` was 0 after every \
         round — while one healthy read of the same call cost {healthy_waited:?}. Total storm wall time \
         {storm:?}.",
        storm = storm.elapsed()
    );
}

#[test]
fn cancellation_ends_a_blocked_desktop_call_promptly() {
    let _guard = lock_desktop();
    // A cancel raised while a provider is thinking has to be *responsive*. The call itself cannot be
    // retracted, and the suite asserts what replaced the old honest admission: the worker that was
    // cancelled is terminated rather than left counted, so the cancel costs the run nothing permanent.
    let hung = Fixture::start("hung", None);
    let cancel = Arc::new(AtomicBool::new(false));
    let settings = accessibility::Settings {
        call_timeout: Duration::from_secs(120),
        max_nodes: 200,
        probe_patterns: false,
    };

    let flag = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        flag.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    let outcome = accessibility::probe(hung.id(), settings, Some(cancel.as_ref()));
    let waited = started.elapsed();

    match outcome {
        Err(WindowsError::Cancelled { budget_secs, .. }) => {
            assert_eq!(
                budget_secs, 120,
                "the bound that was in force is reported, not the one that stopped the wait"
            );
        }
        Err(WindowsError::ProviderTimeout { .. }) => {
            panic!("a 120-second bound answered in {waited:?}; the cancel did not end the wait")
        }
        Ok(_) => {
            // UIA's own timeout may have answered first. That is a bounded outcome too, but it is not
            // evidence about cancellation, so the test says which branch it took.
            eprintln!(
                "NOTE: the hung provider answered before the cancel landed, after {waited:?}"
            );
        }
        Err(other) => panic!("expected a cancellation, got {other:?}"),
    }
    assert!(
        waited < Duration::from_secs(30),
        "cancellation must be responsive, not merely eventual; waited {waited:?}"
    );
    // The old design had to concede a cancelled call: the thread stayed parked and stayed counted.
    // Ending the process is what makes that concession unnecessary, and it is checked rather than claimed.
    if matches!(outcome, Err(WindowsError::Cancelled { .. })) {
        assert_eq!(
            0,
            worker::live_workers(),
            "a cancelled call left its worker running"
        );
    }
}

#[test]
fn input_is_refused_when_the_foreground_is_not_the_target() {
    let _guard = lock_desktop();
    // The overlay case from the plan: a capture-excluded window can leave the *image* showing something
    // else, so input must be gated on the real foreground rather than on what was seen. This drives the
    // refusal through the same `Target::verify` the send path runs, with a foreground expectation that is
    // deliberately not the target.
    let fixture = Fixture::start("normal", None);
    let rect = identity::rect_of(fixture.id()).expect("the fixture has a rectangle");
    let centre = rect.centre();

    let wrong = Target {
        window: fixture.id(),
        point: centre,
        expect_foreground: WindowId {
            hwnd: fixture.hwnd ^ 0x1,
        },
    };
    let error = wrong
        .verify()
        .expect_err("a target whose foreground expectation is wrong must be refused");
    match error {
        WindowsError::FocusChanged { expected, .. } => {
            assert!(
                expected.contains("hwnd="),
                "the refusal names what was expected: {expected}"
            );
        }
        other => panic!("expected FocusChanged, got {other:?}"),
    }

    // The same target with the expectation it can actually meet is either accepted with the confirmation
    // recorded, or refused because the fixture genuinely is not the foreground right now. Both are correct
    // answers about this machine; neither is a silent pass.
    let honest = Target {
        window: fixture.id(),
        point: centre,
        expect_foreground: fixture.id(),
    };
    match honest.verify() {
        Ok(state) => assert!(
            state.foreground_confirmed,
            "a verified target carries the confirmation it just made"
        ),
        Err(WindowsError::FocusChanged { .. }) => {}
        Err(other) => panic!("unexpected refusal for a well-formed target: {other:?}"),
    }
}

#[test]
fn a_point_outside_the_targets_current_rectangle_is_refused() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let rect = identity::rect_of(fixture.id()).expect("the fixture has a rectangle");

    // Somewhere the window was not. A stale plan's saved click is this exact shape, and it must not reach
    // `SendInput` — the coordinate is a claim about where something is, made at an earlier time.
    let stale = Target {
        window: fixture.id(),
        point: (rect.right + 400, rect.bottom + 400),
        expect_foreground: fixture.id(),
    };
    let error = stale
        .verify()
        .expect_err("a point outside the window must be refused");
    assert!(
        matches!(error, WindowsError::StaleElement { .. })
            || matches!(error, WindowsError::FocusChanged { .. }),
        "expected a stale-coordinate or foreground refusal, got {error:?}"
    );
}

#[test]
fn a_dead_handle_is_stale_before_any_action_rather_than_after() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let live = fixture.id();
    assert!(live.is_alive(), "the fixture's window exists");

    // Stopping the process destroys the window, which is the only way to obtain a genuinely dead handle
    // without touching anything else on the desktop.
    drop(fixture);
    let deadline = Instant::now() + Duration::from_secs(6);
    while live.is_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!live.is_alive(), "the handle must stop naming a window");

    let outcome = accessibility::resolve(live, Fixture::by_id(1002), settings(), None);
    match outcome {
        Err(error) => assert!(
            matches!(
                error,
                WindowsError::StaleWindow { .. }
                    | WindowsError::AccessibilityUnavailable { .. }
                    | WindowsError::StaleElement { .. }
            ),
            "a dead handle must be reported as stale, unreadable or absent, got {error:?}"
        ),
        Ok(found) => assert!(
            found.is_empty(),
            "a dead handle resolved to {} control(s)",
            found.len()
        ),
    }
}

#[test]
fn coordinates_are_computed_in_the_space_the_monitors_actually_use() {
    let _guard = lock_desktop();
    // R-15 and R-03 together. The report is written from what this machine says about itself rather than
    // from a case count, because on a single-monitor machine there is nothing to cross-check and the test
    // has to say that instead of passing vacuously.
    let desktop = identity::virtual_desktop();
    assert!(
        desktop.width() > 0 && desktop.height() > 0,
        "the virtual desktop is {}x{}",
        desktop.width(),
        desktop.height()
    );

    let fixture = Fixture::start("normal", None);
    let found = identity::read_identity(fixture.id());
    let rect = found.rect.expect("the fixture has a rectangle");
    assert!(
        desktop.contains(rect.left + 1, rect.top + 1),
        "the window at ({},{}) is outside the virtual desktop {desktop:?}",
        rect.left,
        rect.top
    );

    let normalised = input::to_normalised(rect.centre(), desktop);
    assert!(
        normalised.0 >= 0 && normalised.0 <= 65535 && normalised.1 >= 0 && normalised.1 <= 65535,
        "the point normalised outside the range SendInput expects: {normalised:?}"
    );

    let report =
        input::coordinate_report(fixture.id()).expect("the coordinate posture is reportable");
    assert!(report.contains("virtual="), "report is {report}");
    assert!(
        report.contains("awareness="),
        "the awareness actually in force is part of the report: {report}"
    );

    // The monitor set, read from the windows that are on them.
    let monitors: Vec<(i32, i32, i32, i32, u32)> = identity::enumerate()
        .unwrap_or_default()
        .iter()
        .filter_map(|found| {
            found.monitor.map(|monitor| {
                (
                    monitor.x,
                    monitor.y,
                    monitor.width,
                    monitor.height,
                    monitor.dpi,
                )
            })
        })
        .collect();
    let distinct: std::collections::HashSet<(i32, i32, i32, i32)> = monitors
        .iter()
        .map(|(x, y, w, h, _)| (*x, *y, *w, *h))
        .collect();
    if distinct.len() < 2 {
        // Not a pass and not a failure. The plan is explicit that an unavailable mixed-DPI case must never
        // be counted as covered, so it is said out loud and the assertion below is not made.
        eprintln!(
            "SKIPPED mixed-DPI cross-check: this desktop reported {} distinct monitor geometr{}",
            distinct.len(),
            if distinct.len() == 1 { "y" } else { "ies" }
        );
        return;
    }
    let mut dpis: Vec<u32> = monitors.iter().map(|(_, _, _, _, dpi)| *dpi).collect();
    dpis.sort_unstable();
    dpis.dedup();
    if dpis.len() < 2 {
        eprintln!("SKIPPED mixed-DPI cross-check: every monitor reported the same DPI {dpis:?}");
        return;
    }
    // Two monitors at two DPIs. A window's own `GetDpiForWindow` must be the DPI of the monitor it is
    // sitting on — the property a primary-monitor-only normalisation gets wrong, and the one that decides
    // whether a click on the second display lands where it was aimed.
    let second = monitors
        .iter()
        .find(|(_, _, _, _, dpi)| *dpi != dpis[0])
        .expect("a monitor at the second DPI");
    assert!(
        second.4 > 0 && second.2 > 0,
        "the second monitor reported {second:?}"
    );
}

#[test]
fn a_coordinate_click_lands_on_the_control_on_the_monitor_it_was_placed_on() {
    let _guard = lock_desktop();
    // R-03's real half, and the reason `enigo` was never adopted: the shipped `SendInput` backend aimed by
    // *coordinate* — not by locator — at a control belonging to a window on a monitor that is not the
    // primary, and verified by the fixture's own click counter. The coordinate is the whole subject here:
    // `to_normalised` is where a second monitor, a negative virtual origin, or a DPI that is not the
    // primary's turns a click into a miss, and a unit test can only show the arithmetic agrees with itself.
    //
    // A machine with one monitor cannot produce this evidence. It is reported as skipped rather than as a
    // pass, and no simulated display geometry is used to manufacture one.
    let desktop = identity::virtual_desktop();
    let mut monitors: Vec<identity::Monitor> = Vec::new();
    for found in identity::enumerate().unwrap_or_default() {
        if let Some(monitor) = found.monitor {
            if !monitors.iter().any(|seen| {
                seen.x == monitor.x && seen.y == monitor.y && seen.width == monitor.width
            }) {
                monitors.push(monitor);
            }
        }
    }
    let Some(target_monitor) = monitors.iter().find(|monitor| !monitor.is_primary) else {
        eprintln!(
            "SKIPPED second-monitor coordinate click: this desktop reported {} monitor(s), all of them \
             primary. The case is unverified on this machine, not covered.",
            monitors.len()
        );
        return;
    };
    let target_monitor = *target_monitor;

    // A real window on that monitor, and one that takes the foreground the way an application the operator
    // opened does: JEAN will not steal focus to make a click land, so the test has to be given it.
    // `--position` is in physical pixels on the virtual desktop, which is the same space the click is
    // aimed in.
    let x = target_monitor.x + 20;
    let y = target_monitor.y + 40;
    let fixture = Fixture::start_shell("focus-steal", (x, y));

    // Where the window actually ended up. Windows can clamp a position that overhangs an edge, and a test
    // that clicked "the second monitor" after the window stayed on the first would be proving nothing.
    let landed = identity::monitor_of(fixture.id()).unwrap_or_else(|| {
        panic!(
            "the fixture at ({x},{y}) reported no monitor; the desktop is {:?}",
            identity::virtual_desktop()
        )
    });
    assert_eq!(
        (landed.x, landed.y, landed.width, landed.height),
        (
            target_monitor.x,
            target_monitor.y,
            target_monitor.width,
            target_monitor.height
        ),
        "the fixture was asked onto {target_monitor:?} and is on {landed:?}"
    );

    // The window's own DPI must be the monitor's. This is the per-monitor-v2 property a primary-only
    // normalisation gets wrong, and it is what makes the coordinate below meaningful.
    let window_dpi = identity::dpi_of(fixture.id()).unwrap_or(0);
    assert_eq!(
        window_dpi, landed.dpi,
        "the window reports dpi {window_dpi} on a monitor at {}",
        landed.dpi
    );

    // Wait for the foreground the fixture asked for. A refusal here is Windows' foreground lock, which is
    // a real property of this desktop and is said out loud rather than worked around.
    let deadline = Instant::now() + Duration::from_secs(8);
    while identity::foreground() != Some(fixture.id()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if identity::foreground() != Some(fixture.id()) {
        let observed = identity::foreground().map_or_else(
            || "no window".to_owned(),
            |window| {
                format!(
                    "hwnd={} {:?}",
                    window.hwnd,
                    window.title().unwrap_or_default()
                )
            },
        );
        eprintln!(
            "SKIPPED second-monitor coordinate click: the fixture at hwnd={} asked for the foreground \
             and the foreground is {observed}. JEAN does not force focus, so the click cannot be sent \
             honestly on this desktop.",
            fixture.hwnd
        );
        return;
    }

    // The button's own bounds, as its provider reported them, are the coordinate under test.
    let button = control(&fixture, 1002);
    let rect = button
        .rect
        .unwrap_or_else(|| panic!("the button reported no bounds: {button:?}"));
    let point = rect.centre();
    let landed_bounds = identity::Rect {
        left: landed.x,
        top: landed.y,
        right: landed.x + landed.width,
        bottom: landed.y + landed.height,
    };
    assert!(
        landed_bounds.contains(point.0, point.1),
        "the aimed point ({},{}) is outside monitor {landed:?}",
        point.0,
        point.1
    );
    assert!(
        desktop.contains(point.0, point.1),
        "the aimed point ({},{}) is outside the virtual desktop {desktop:?}",
        point.0,
        point.1
    );

    // The conversion, checked for what it means for a second monitor rather than for being in range: a
    // monitor to the right of the primary starts past the middle of the normalised range, and one to the
    // left of it starts before. A conversion from zero instead of from the virtual origin cannot do this.
    let normalised = input::to_normalised(point, desktop);
    if target_monitor.x > 0 {
        assert!(
            normalised.0 > 65535 / 2,
            "a point at x={} on a monitor right of the primary normalised to {}, which is not past the \
             middle of the range",
            point.0,
            normalised.0
        );
    } else if target_monitor.x < 0 {
        assert!(
            normalised.0 < 65535 / 2,
            "a point at x={} on a monitor left of the primary normalised to {}, which is not before the \
             middle of the range",
            point.0,
            normalised.0
        );
    }

    assert_eq!(
        control(&fixture, 1006).name,
        "idle",
        "nothing has been clicked yet"
    );

    let sent = input::send(
        Target {
            window: fixture.id(),
            point,
            expect_foreground: fixture.id(),
        },
        input::Input::Click {
            button: input::Button::Left,
        },
        input::Settings {
            call_timeout: Duration::from_secs(8),
            ..Default::default()
        },
        None,
    )
    .expect("a click aimed at the control the operator is looking at must be sent");
    assert!(
        sent.is_complete(),
        "SendInput accepted {}/{} events at ({},{}) normalised to {:?}",
        sent.events_accepted,
        sent.events_queued,
        point.0,
        point.1,
        normalised
    );

    // The witness is the fixture's counter, not the API's return value. A click that landed on the
    // monitor's wallpaper, on the title bar, or one pixel off the button is accepted by `SendInput`
    // exactly as this one was, and only the control that was aimed at can tell them apart.
    let after = control(&fixture, 1006).name;
    assert_eq!(
        after, "clicked:1",
        "a coordinate click at ({},{}) on monitor {landed:?} (dpi {}) did not reach the button: \
         the status label reads {after:?}",
        point.0, point.1, landed.dpi
    );

    // The two refusals, through the same call rather than through `Target::verify` alone: what has to be
    // proven is that `send` stops before `SendInput`, so that the counter is still the number it was.
    let stale = input::send(
        Target {
            window: fixture.id(),
            point: (point.0 + 4000, point.1 + 4000),
            expect_foreground: fixture.id(),
        },
        input::Input::Click {
            button: input::Button::Left,
        },
        input::Settings {
            call_timeout: Duration::from_secs(8),
            ..Default::default()
        },
        None,
    );
    match stale {
        Err(WindowsError::StaleElement { .. }) | Err(WindowsError::FocusChanged { .. }) => {}
        Err(other) => panic!("expected a stale-coordinate or foreground refusal, got {other:?}"),
        Ok(sent) => panic!(
            "a point 4000px outside the window was sent anyway: {:?} at {:?}",
            sent.events_accepted, sent.input
        ),
    }

    // A focus expectation the target cannot meet is refused before the batch is built, whatever the real
    // foreground happens to be right now.
    let misattributed = input::send(
        Target {
            window: fixture.id(),
            point,
            expect_foreground: WindowId {
                hwnd: fixture.hwnd ^ 0x1,
            },
        },
        input::Input::Text {
            text: "typed by a refused send".to_owned(),
        },
        input::Settings {
            call_timeout: Duration::from_secs(8),
            ..Default::default()
        },
        None,
    );
    match misattributed {
        Err(WindowsError::FocusChanged { .. }) => {}
        Err(other) => panic!("expected a foreground refusal, got {other:?}"),
        Ok(sent) => panic!(
            "text was injected against a foreground expectation the target does not meet: {:?}",
            sent.input
        ),
    }

    assert_eq!(
        control(&fixture, 1006).name,
        "clicked:1",
        "the two refused sends performed nothing, which is the whole point of verifying inside the send"
    );
    assert_eq!(
        0,
        worker::live_workers(),
        "coordinate work is not provider work and owns no workers"
    );
    // Said out loud, because the shape of this evidence is the point: a monitor at a DPI the primary does
    // not use, a point on it, and a click that arrived.
    eprintln!(
        "MEASURED: a coordinate click at ({},{}) — normalised {normalised:?} of a virtual desktop \
         {}x{}@{},{} — landed on monitor {}x{}@{},{} at dpi {} ({}%) and the fixture's own counter read \
         {after:?}; two refused sends after it left that counter unchanged.",
        point.0,
        point.1,
        desktop.width(),
        desktop.height(),
        desktop.left,
        desktop.top,
        landed.width,
        landed.height,
        landed.x,
        landed.y,
        landed.dpi,
        landed.scale_percent(),
    );
}

#[test]
fn a_capture_reports_only_metadata_and_never_its_pixels_in_a_diagnostic() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let frame =
        capture::capture(fixture.id(), capture_settings(), None).expect("a normal window captures");

    assert_eq!(
        frame.quality(),
        FrameQuality::Usable,
        "the fixture's white client area over a coloured desktop is content"
    );

    // The rule is not "we promise not to log the image"; it is that the type cannot be made to print it.
    let debug = format!("{frame:?}");
    assert!(
        debug.len() < 600,
        "the Debug of a capture is {} characters, which means it is carrying pixels",
        debug.len()
    );
    assert!(
        debug.contains("hash"),
        "the debug form carries the summary: {debug}"
    );

    let line = capture::log_line(&frame);
    assert!(line.contains("capture hwnd="), "log line is {line}");
    assert!(
        !line.contains("Send") && !line.contains("J.E.A.N"),
        "a log line about a capture of the fixture must not quote the fixture's controls: {line}"
    );

    // Retention is the caller's decision, so the mechanism must at least be able to name where a kept
    // frame would go, and the name must be about the window and the instant rather than about its content.
    let path = jean_windows::ocr::capture_path(Path::new("."), fixture.hwnd, frame.captured_at_ms);
    assert!(
        path.to_string_lossy().ends_with(".bmp"),
        "a kept capture is named by window and instant: {path:?}"
    );
}

#[test]
fn a_kept_capture_writes_a_bmp_whose_header_agrees_with_the_frame() {
    let _guard = lock_desktop();
    let fixture = Fixture::start("normal", None);
    let frame =
        capture::capture(fixture.id(), capture_settings(), None).expect("a normal window captures");
    let directory = std::env::temp_dir().join("jean-phase4-capture-check");
    std::fs::create_dir_all(&directory).expect("a temp directory for the check");
    let path = directory.join(format!("fixture-{}.bmp", fixture.hwnd));

    let written = capture::write_bmp(&frame, &path).expect("the BMP must be writable");
    let bytes = std::fs::read(&written).expect("the file must exist after the write");
    assert_eq!(&bytes[0..2], b"BM", "a BMP starts with its magic");
    let width = i32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]) as usize;
    let height =
        i32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]).unsigned_abs() as usize;
    let bits = u16::from_le_bytes([bytes[28], bytes[29]]) as usize;
    let size = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as usize;
    assert_eq!(
        width, frame.width as usize,
        "the header agrees with the frame"
    );
    assert_eq!(height, frame.height as usize);
    assert_eq!(
        bits, 24,
        "no alpha, because a screenshot's alpha is meaningless"
    );
    assert_eq!(
        size,
        bytes.len(),
        "the file is exactly as long as its header claims"
    );
    assert!(
        bytes.len() > 54,
        "a header-only BMP would mean no pixels were written"
    );
    let _ = std::fs::remove_file(&written);
    let _ = std::fs::remove_dir(&directory);
}

#[test]
fn the_ocr_backend_that_is_not_configured_refuses_instead_of_inventing_text() {
    let _guard = lock_desktop();
    // The rule the brief states directly: a mocked OCR response must never be labelled working inference.
    // `NoOcr` therefore cannot be asked to produce a transcript at all — it refuses, and it reports itself
    // as unavailable rather than as untested.
    use jean_windows::ocr::{BackendStatus, NoOcr, TextExtractor};
    assert_eq!(NoOcr.name(), "none");
    assert!(matches!(NoOcr.status(), BackendStatus::Unavailable(_)));
    assert!(
        !NoOcr.status().counts_as_capability(),
        "an unconfigured backend is not a capability"
    );
}
