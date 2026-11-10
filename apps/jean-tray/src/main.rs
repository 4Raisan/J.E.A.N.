//! Phase 6 tray shell. Opens no windows, only a tray icon and menu.
//! Task board opens `jean status` in a new console; Quit exits.
//! Reads no store directly: delegates to the CLI so policy/audit stay in
//! the supervised path. Must run beside jean.exe (same data dir default).

use anyhow::Result;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::process::{Child, Command};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

const OPEN_TASK_BOARD_ID: &str = "open-task-board";
const QUIT_ID: &str = "quit";

fn main() -> Result<()> {
    let tray = TrayIconBuilder::new()
        .with_tooltip("J.E.A.N. — tray")
        .with_menu(Box::new(menu()))
        .build()?;
    let mut owned_children = Vec::new();

    loop {
        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::Click { .. }) {
                if let Some(child) = open_status() {
                    owned_children.push(child);
                }
            }
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id().as_ref() {
                OPEN_TASK_BOARD_ID => {
                    if let Some(child) = open_status() {
                        owned_children.push(child);
                    }
                }
                QUIT_ID => break,
                _ => {}
            }
        }

        reap_exited(&mut owned_children);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Drop the native icon/menu before leaving the event loop. Only terminate
    // direct children created by this tray process; never search for or kill
    // unrelated `jean.exe` processes.
    drop(tray);
    stop_owned_children(owned_children);
    Ok(())
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

fn open_status() -> Option<Child> {
    let jean = std::env::current_exe().ok()?.parent()?.join("jean.exe");
    let mut command = Command::new(jean);
    command.arg("status");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        command.creation_flags(CREATE_NEW_CONSOLE);
    }

    command.spawn().ok()
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
}
