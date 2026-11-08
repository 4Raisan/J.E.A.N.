//! Phase 6 tray shell. Opens no windows, only a tray icon and menu.
//! Task board opens `jean status` in a new console; Quit exits.
//! Reads no store directly: delegates to the CLI so policy/audit stay in
//! the supervised path. Must run beside jean.exe (same data dir default).

use anyhow::Result;
use muda::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

fn main() -> Result<()> {
    let tray = TrayIconBuilder::new()
        .with_tooltip("J.E.A.N. — tray")
        .with_menu(Box::new(menu()))
        .build()?;
    let _ = tray;
    loop {
        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click { .. } => open_status(),
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn menu() -> Menu {
    let m = Menu::new();
    let _ = m.append(&MenuItem::new("Open task board (jean status)", true, None));
    let _ = m.append(&PredefinedMenuItem::separator());
    let _ = m.append(&MenuItem::new("Quit", true, None));
    m
}

fn open_status() {
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("jean.exe")));
    if let Some(jean) = exe {
        let _ = std::process::Command::new("cmd").arg("/c").arg(jean).arg("status").spawn();
    }
}
