//! Phase 6 tray shell. Opens a tray icon and menu with a Win32 message pump.
//! Task board opens `jean status` in a persistent readable console view; Quit exits.
//! Reads no store directly: delegates to the CLI so policy/audit stay in
//! the supervised path. Must run beside jean.exe (same data dir default).

use anyhow::Result;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::path::Path;
use std::process::{Child, Command};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const OPEN_TASK_BOARD_ID: &str = "open-task-board";
const QUIT_ID: &str = "quit";

fn main() -> Result<()> {
    let tray = build_tray(menu())?;
    let mut owned_children = Vec::new();

    loop {
        if !pump_win32_messages() {
            break;
        }
        if !dispatch_pending_events(&mut owned_children, open_status) {
            break;
        }

        reap_exited(&mut owned_children);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    // Drop the native icon/menu before leaving the event loop. Only terminate
    // direct children created by this tray process; never search for or kill
    // unrelated `jean.exe` processes.
    drop(tray);
    stop_owned_children(owned_children);
    Ok(())
}

fn tray_icon() -> Result<Icon> {
    const SIZE: u32 = 32;
    let center = (SIZE as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let pixel = if dist <= 14.5 && dist >= 10.0 {
                // Cyan reactor ring
                [0x56, 0xe3, 0xd8, 0xff]
            } else if dist <= 4.0 {
                // Inner core
                [0xa9, 0x9a, 0xff, 0xff]
            } else if dist < 14.5 {
                // Dark slate disc
                [0x18, 0x1c, 0x2c, 0xf2]
            } else {
                [0x00, 0x00, 0x00, 0x00]
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    Ok(Icon::from_rgba(rgba, SIZE, SIZE)?)
}

fn build_tray(menu: Menu) -> Result<TrayIcon> {
    Ok(TrayIconBuilder::new()
        .with_tooltip("J.E.A.N. — tray")
        .with_icon(tray_icon()?)
        .with_menu(Box::new(menu))
        .build()?)
}

fn pump_win32_messages() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
        };
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return false;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    true
}

fn handle_menu_action(
    id: &str,
    owned_children: &mut Vec<Child>,
    spawn_status: &impl Fn() -> Option<Child>,
) -> bool {
    match id {
        OPEN_TASK_BOARD_ID => {
            if let Some(child) = spawn_status() {
                owned_children.push(child);
            }
            true
        }
        QUIT_ID => false,
        _ => true,
    }
}

fn dispatch_pending_events(
    owned_children: &mut Vec<Child>,
    spawn_status: impl Fn() -> Option<Child>,
) -> bool {
    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            if let Some(child) = spawn_status() {
                owned_children.push(child);
            }
        }
    }

    while let Ok(event) = MenuEvent::receiver().try_recv() {
        if !handle_menu_action(event.id().as_ref(), owned_children, &spawn_status) {
            return false;
        }
    }

    true
}

fn menu() -> Menu {
    let m = Menu::new();
    let _ = m.append(&MenuItem::with_id(
        OPEN_TASK_BOARD_ID,
        "Open task board (jean status)",
        true,
        None,
    ));
    let _ = m.append(&PredefinedMenuItem::separator());
    let _ = m.append(&MenuItem::with_id(QUIT_ID, "Quit", true, None));
    m
}

fn status_command(jean: &Path) -> Command {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "$Host.UI.RawUI.WindowTitle = 'J.E.A.N. — Task Status'; \
             & $env:JEAN_TRAY_STATUS_EXE status; \
             Write-Host ''; \
             Write-Host 'Press Enter or close this window when done.'; \
             [void][Console]::ReadLine()",
        ])
        .env("JEAN_TRAY_STATUS_EXE", jean);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        command.creation_flags(CREATE_NEW_CONSOLE);
    }

    command
}

fn open_status() -> Option<Child> {
    let jean = std::env::current_exe().ok()?.parent()?.join("jean.exe");
    status_command(&jean).spawn().ok()
}

fn reap_exited(children: &mut Vec<Child>) {
    children.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) | Err(_) => true,
    });
}

fn stop_owned_children(mut children: Vec<Child>) {
    for child in &mut children {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
    for child in &mut children {
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn menu_items_have_stable_dispatch_ids() {
        let menu = menu();
        let items = menu.items();
        let ids: Vec<_> = items
            .iter()
            .filter(|item| matches!(item, muda::MenuItemKind::MenuItem(_)))
            .map(|item| item.id().as_ref().to_owned())
            .collect();
        assert_eq!(ids, [OPEN_TASK_BOARD_ID, QUIT_ID]);
    }

    #[test]
    fn tray_icon_builds_pumps_win32_messages_and_dispatches_menu_actions() {
        let tray = build_tray(menu()).expect("tray icon with valid RGBA icon must build on Windows");
        assert!(pump_win32_messages());

        let mut children = Vec::new();
        let opened = Cell::new(0usize);
        let cont = handle_menu_action(OPEN_TASK_BOARD_ID, &mut children, &|| {
            opened.set(opened.get() + 1);
            None
        });
        assert!(cont);
        assert_eq!(opened.get(), 1);

        let cont_quit = handle_menu_action(QUIT_ID, &mut children, &|| None);
        assert!(!cont_quit);

        drop(tray);
        assert!(pump_win32_messages());
    }

    #[test]
    fn status_command_keeps_status_view_readable() {
        let fake_jean = Path::new(r"C:\Program Files\JEAN\jean.exe");
        let cmd = status_command(fake_jean);
        assert_eq!(cmd.get_program(), "powershell.exe");
        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg.contains("ReadLine")));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(
            std::ffi::OsStr::new("JEAN_TRAY_STATUS_EXE"),
            Some(fake_jean.as_os_str())
        )));
    }
}
