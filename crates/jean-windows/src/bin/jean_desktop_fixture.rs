//! The Phase-4 desktop fixture: a real Win32 window with real UIA providers.
//!
//! Every claim `jean-windows` makes about accessibility, capture and input has to be checked against
//! something that is genuinely on the desktop — a fake backend can only ever confirm that the code agrees
//! with itself. This binary is that something: standard `BUTTON`/`EDIT`/`COMBOBOX`/`STATIC` windows created
//! with fixed control ids, so the UIA tree, the pattern set, the bounding rectangles and the capture
//! behaviour are Windows' answers rather than ours.
//!
//! It is deliberately difficult in five ways, one per mode, because those are the five cases that make
//! desktop control unreliable:
//!
//! | `--mode` | What it reproduces | Why it is here |
//! |---|---|---|
//! | `normal` | an ordinary accessible window | the baseline every locator and pattern action is measured against |
//! | `excluded` | `WDA_EXCLUDEFROMCAPTURE` | a window whose *pixels* are refused while its *controls* still work — the Telegram-class case, and the reason a blank capture must not disable accessibility |
//! | `blank` | a window painted solid black | an unexplained blank frame, which must not be reported as the same fact as explicit protection |
//! | `hung` | a window whose owning thread never pumps messages | a provider that cannot answer: the UIA call never returns, so the only thing protecting the caller is its own bound |
//! | `focus-steal` | a window that takes the foreground after a moment | the state that must make typed input stop rather than land somewhere else |
//!
//! Two more scenarios live inside `normal`, because they are properties of the control set rather than of
//! the window: two push buttons sharing the accessible name `Continue` (ambiguity must be reported, not
//! silently resolved by whichever one enumeration reached first), and a button that destroys the status
//! label, so an element cached before it is provably stale afterwards.
//!
//! Nothing here reads user data, opens a socket, writes the registry or needs elevation. `--exit-after`
//! makes the process quit by itself, and every test that starts it does so inside a kill-on-close job
//! object, so a forgotten fixture cannot outlive the run that made it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, GetStockObject, BLACK_BRUSH, HBRUSH, WHITE_BRUSH,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
    PostQuitMessage, RegisterClassExW, SendMessageW, SetForegroundWindow, SetTimer,
    SetWindowDisplayAffinity, SetWindowTextW, ShowWindow, TranslateMessage, BN_CLICKED,
    BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING, CS_HREDRAW, CS_VREDRAW,
    HCURSOR, HMENU, MSG, SW_SHOW, WDA_EXCLUDEFROMCAPTURE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_COMMAND, WM_DESTROY, WM_TIMER, WNDCLASSEXW, WNDCLASS_STYLES, WS_CHILD, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE, WS_VSCROLL,
};

/// Window text of the main frame. The tests find their window by pid **and** this title.
const TITLE: &str = "J.E.A.N. desktop fixture";
const CLASS: &str = "JeanFixtureMain";
const HUNG_CLASS: &str = "JeanFixtureHung";
const HUNG_TITLE: &str = "J.E.A.N. fixture hung provider";

// The control ids are the identity the tests address. Windows' own provider for a standard control
// exposes the dialog control id as `AutomationId`, which is why the fixture is built out of real
// controls rather than owner-drawn ones — and why the suite *checks* that mapping instead of trusting
// this comment.
const ID_SUBJECT: usize = 1001;
const ID_SEND: usize = 1002;
const ID_TOGGLE: usize = 1003;
const ID_RADIO_A: usize = 1004;
const ID_COMBO: usize = 1005;
const ID_STATUS: usize = 1006;
const ID_DESTROY_STATUS: usize = 1007;
const ID_AMBIGUOUS_LEFT: usize = 1008;
const ID_AMBIGUOUS_RIGHT: usize = 1009;

/// The status label, and the click counter behind its text.
///
/// A bare `extern "system"` window procedure cannot carry state, so the fixture keeps the two things the
/// verifier reads in process-wide atomics. That is acceptable here and nowhere else: this process exists
/// to be one window with one state.
static STATUS: AtomicIsize = AtomicIsize::new(0);
static CLICKS: AtomicU32 = AtomicU32::new(0);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn status(text: &str) {
    let handle = HWND(STATUS.load(Ordering::SeqCst) as *mut c_void);
    if handle.0.is_null() {
        return;
    }
    let buffer = wide(text);
    // Every action the suite performs ends with a read of this window text, so it has to be the most
    // reliable thing in the tree — which is also why destroying it is a scenario of its own.
    let _ = unsafe { SetWindowTextW(handle, PCWSTR(buffer.as_ptr())) };
}

unsafe extern "system" fn wndproc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = wparam.0 & 0xffff;
            let code = wparam.0 >> 16;
            if code == BN_CLICKED as usize {
                match id {
                    ID_SEND => {
                        let n = CLICKS.fetch_add(1, Ordering::SeqCst) + 1;
                        status(&format!("clicked:{n}"));
                    }
                    ID_DESTROY_STATUS => {
                        // Stale by construction: whatever a caller cached three calls ago is now a dead
                        // handle, so reusing it has to fail honestly rather than act on nothing.
                        let stale = HWND(STATUS.swap(0, Ordering::SeqCst) as *mut c_void);
                        if !stale.0.is_null() {
                            let _ = DestroyWindow(stale);
                            status("status label removed");
                        }
                    }
                    ID_AMBIGUOUS_LEFT => status("ambiguous:left"),
                    ID_AMBIGUOUS_RIGHT => status("ambiguous:right"),
                    _ => {}
                }
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_TIMER => {
            // Timer 1 is the self-exit, timer 2 steals the foreground. Both are armed at start-up, and
            // only in the mode that asks for them.
            match wparam.0 {
                1 => {
                    let _ = DestroyWindow(window);
                }
                2 => {
                    let moved = unsafe { SetForegroundWindow(window) };
                    println!(
                        "[fixture] asked for the foreground, granted={}",
                        bool::from(moved)
                    );
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe extern "system" fn hung_wndproc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(window, message, wparam, lparam)
}

/// A window whose thread never pumps messages.
///
/// This is not a simulation of a slow application, it is one: a UIA call into this window cross-process
/// sends `WM_GETOBJECT` and nothing ever dequeues it. Both bounds in front of it — UIA's own matcher
/// timeout and `jean-windows`' worker timeout — are what stand between a hung provider and a desktop that
/// is simply gone for the rest of the session.
fn start_hung_provider() {
    let worker = std::thread::Builder::new()
        .name("jean-fixture-hung".to_owned())
        .spawn(|| unsafe {
            let class = wide(HUNG_CLASS);
            let name = wide(HUNG_TITLE);
            let instance = HINSTANCE(std::ptr::null_mut());
            let wndclass = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: WNDCLASS_STYLES(CS_HREDRAW.0 | CS_VREDRAW.0),
                lpfnWndProc: Some(hung_wndproc),
                hInstance: instance,
                hCursor: HCURSOR(std::ptr::null_mut()),
                hbrBackground: CreateSolidBrush(COLORREF(0x0020_2020)),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&wndclass) == 0 {
                println!("[fixture] hung provider: class registration failed");
                return;
            }
            let Ok(handle) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                900,
                120,
                320,
                200,
                None,
                None,
                Some(instance),
                None,
            ) else {
                println!("[fixture] hung provider: window creation failed");
                return;
            };
            let _ = ShowWindow(handle, SW_SHOW);
            println!("[fixture] hung hwnd={}", handle.0 as isize,);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            // And no message loop, ever.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    // Deliberately not joined: parking the thread *is* the mode. The process dies with the job object.
    let _ = worker;
}

// The argument list is `CreateWindowExW`'s own shape for one child control, and a struct for it would
// rename the Win32 parameters to nothing without removing one of them.
#[allow(clippy::too_many_arguments)]
fn child(
    parent: HWND,
    class: &str,
    text: &str,
    style: WINDOW_STYLE,
    id: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<HWND> {
    let class = wide(class);
    let text = wide(text);
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            width,
            height,
            Some(parent),
            // For a child control the `hmenu` parameter is its control id, which is where the
            // `AutomationId` the tests address comes from.
            Some(HMENU(id as *mut c_void)),
            None,
            None,
        )
        .ok()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut mode = String::from("normal");
    let mut exit_after = 180u64;
    // The default position is on the primary monitor. A second monitor at a different DPI is the case
    // R-15 and R-03 are about, and it can only be tested by putting a real window there — which is why
    // the position is an argument and not a constant.
    let mut position = (140, 120);
    let mut index = 1;
    while index + 1 < args.len() {
        match (args[index].as_str(), args[index + 1].as_str()) {
            ("--mode", value) => {
                mode = value.to_owned();
                index += 2;
            }
            ("--exit-after", value) => {
                exit_after = value.parse().unwrap_or(180);
                index += 2;
            }
            ("--position", value) => {
                let (x, y) = value
                    .split_once(',')
                    .map(|(x, y)| (x.parse().unwrap_or(140), y.parse().unwrap_or(120)))
                    .unwrap_or((140, 120));
                position = (x, y);
                index += 2;
            }
            _ => index += 1,
        }
    }

    // Before any window exists, which is the only time the choice can be made. The fixture is
    // per-monitor-v2 so its geometry is in physical pixels and agrees with what a DPI-aware JEAN
    // computes; a fixture that lied about its DPI would make every coordinate test meaningless.
    let aware =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let instance = HINSTANCE(std::ptr::null_mut());
    let class = wide(CLASS);
    let title = wide(&format!("{TITLE} [{mode}]"));
    // `blank` gets a black class background rather than custom paint handling: the point of the mode is
    // that the pixels are legitimately uninformative, not that the fixture is a good painter.
    let background = if mode == "blank" {
        HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0)
    } else {
        HBRUSH(unsafe { GetStockObject(WHITE_BRUSH) }.0)
    };
    let wndclass = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(CS_HREDRAW.0 | CS_VREDRAW.0),
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        hCursor: HCURSOR(std::ptr::null_mut()),
        hbrBackground: background,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wndclass) } == 0 {
        eprintln!("[fixture] failed to register its window class");
        std::process::exit(2);
    }

    let main = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            position.0,
            position.1,
            640,
            360,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(handle) => handle,
        Err(_) => {
            eprintln!("[fixture] failed to create its window");
            std::process::exit(2);
        }
    };

    let _ = child(
        main,
        "EDIT",
        "not sent",
        WINDOW_STYLE(0),
        ID_SUBJECT,
        20,
        20,
        360,
        28,
    );
    let _ = child(
        main,
        "BUTTON",
        "Send",
        WINDOW_STYLE(0),
        ID_SEND,
        400,
        18,
        120,
        30,
    );
    let _ = child(
        main,
        "BUTTON",
        "enabled",
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
        ID_TOGGLE,
        20,
        60,
        160,
        24,
    );
    let _ = child(
        main,
        "BUTTON",
        "option a",
        WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
        ID_RADIO_A,
        20,
        92,
        160,
        24,
    );
    let combo = child(
        main,
        "COMBOBOX",
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        ID_COMBO,
        220,
        92,
        180,
        140,
    );
    let status_handle = unsafe { GetDlgItem(Some(main), ID_STATUS as i32) }
        .ok()
        .unwrap_or(HWND(std::ptr::null_mut()));
    let _ = status_handle;
    let _ = child(
        main,
        "STATIC",
        "idle",
        WINDOW_STYLE(0),
        ID_STATUS,
        20,
        130,
        360,
        24,
    );
    if let Ok(handle) = unsafe { GetDlgItem(Some(main), ID_STATUS as i32) } {
        STATUS.store(handle.0 as isize, Ordering::SeqCst);
    }
    let _ = child(
        main,
        "BUTTON",
        "Continue",
        WINDOW_STYLE(0),
        ID_AMBIGUOUS_LEFT,
        20,
        170,
        120,
        30,
    );
    let _ = child(
        main,
        "BUTTON",
        "Continue",
        WINDOW_STYLE(0),
        ID_AMBIGUOUS_RIGHT,
        150,
        170,
        120,
        30,
    );
    let _ = child(
        main,
        "BUTTON",
        "Remove the status label",
        WINDOW_STYLE(0),
        ID_DESTROY_STATUS,
        20,
        210,
        220,
        30,
    );

    if let Some(combo) = combo {
        for item in ["alpha", "beta", "gamma"] {
            let buffer = wide(item);
            unsafe {
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    Some(WPARAM(0)),
                    Some(LPARAM(buffer.as_ptr() as isize)),
                )
            };
        }
    }

    if mode == "excluded" {
        let _ = unsafe { SetWindowDisplayAffinity(main, WDA_EXCLUDEFROMCAPTURE) };
    }

    let _ = unsafe { ShowWindow(main, SW_SHOW) };
    if mode == "hung" {
        start_hung_provider();
    }
    unsafe {
        SetTimer(Some(main), 1, (exit_after.min(600) * 1000) as u32, None);
        if mode == "focus-steal" {
            SetTimer(Some(main), 2, 1200, None);
        }
    }

    println!(
        "[fixture] ready pid={} hwnd={} mode={} dpi_aware={}",
        std::process::id(),
        main.0 as isize,
        mode,
        aware.is_ok()
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let mut message = MSG::default();
    while bool::from(unsafe { GetMessageW(&mut message, None, 0, 0) }) {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
