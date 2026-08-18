#![windows_subsystem = "windows"]

mod air;
mod bar;
mod calendar;
mod config;
mod flyout;
mod http;
mod json;
mod statspop;
mod memguard;
mod switcher;
mod toast;
mod tray;
mod weather;
mod widgets;

use windows::core::{w, Result};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG,
    WM_NULL,
};

use bar::REBUILD;

/// Watches the config directory; flags a rebuild and wakes the main loop.
fn watch_config(main_tid: u32) {
    unsafe {
        let dir = config::path()
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let path16: Vec<u16> = dir.encode_utf16().chain(std::iter::once(0)).collect();
        let Ok(handle) = CreateFileW(
            windows::core::PCWSTR(path16.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) else {
            return;
        };
        loop {
            let mut buf = [0u8; 2048];
            let mut returned = 0u32;
            if ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr() as _,
                buf.len() as u32,
                false,
                FILE_NOTIFY_CHANGE_LAST_WRITE,
                Some(&mut returned),
                None,
                None,
            )
            .is_err()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            REBUILD.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = PostThreadMessageW(main_tid, WM_NULL, WPARAM(0), LPARAM(0));
        }
    }
}

/// Adds or removes the HKCU Run entry that starts the bar at logon.
fn set_autostart(enable: bool) -> Result<()> {
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
    };
    unsafe {
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let name = w!("optim-bar");
        if enable {
            let exe = std::env::current_exe().map_err(|_| windows::core::Error::empty())?;
            let path16: Vec<u16> = exe
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey,
                name,
                REG_SZ.0,
                Some(path16.as_ptr() as _),
                (path16.len() * 2) as u32,
            )
            .ok()?;
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey, name).ok()?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--install-autostart") {
        return set_autostart(true);
    }
    if std::env::args().any(|a| a == "--uninstall-autostart") {
        return set_autostart(false);
    }
    if std::env::args().any(|a| a == "--restore-tray") {
        // Escape hatch: un-hide explorer's taskbar, hand tray icons back,
        // and give monitors their full work areas back.
        tray::restore_explorer_tray();
        bar::restore_work_areas();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--quit") {
        // Shutdown for callers that have no bar to right-click: the launcher's
        // restart entry, or a terminal after `cargo build`. Returns only once
        // the old process is gone, so a start can follow immediately.
        bar::request_quit();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--refresh-tray") {
        // Repair hatch for missing tray icons: broadcasts TaskbarCreated so
        // every app re-registers with whichever tray is on top — the running
        // optim-bar. Cheaper than restarting the bar, and the fix for icons
        // that went to explorer's tray during a shell restart.
        tray::rebroadcast();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--list-tray") {
        // Diagnostic for a missing icon: shows what the *running* host
        // actually collected, so "registered somewhere else" stops looking
        // identical to "this app never had a tray icon".
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
        match tray::request_dump() {
            Ok(text) => print!("{text}"),
            Err(tray::DumpError::NoHost) => {
                println!("no running optim-bar is hosting the tray")
            }
            Err(tray::DumpError::NoAnswer) => println!(
                "the running optim-bar ignored the request — it predates --list-tray.\n\
                 Restart it to pick up this build, then try again."
            ),
        }
        return Ok(());
    }
    if std::env::args().any(|a| a == "--check-config") {
        // GUI subsystem, so borrow the launching terminal's console for output.
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
        config::check();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--mem-top") {
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
        memguard::dump_top(10);
        return Ok(());
    }
    if std::env::args().any(|a| a == "--list-windows") {
        // Diagnostic: the exact list Alt+Tab would show, and where each window
        // lives. Off-workspace rows only appear once komorebi's registry is
        // seeded, which is precisely what tends to be wrong when one goes missing.
        // GUI subsystem, so borrow the launching terminal's console for output.
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
        widgets::komorebi::seed_registry();
        for row in switcher::window_list() {
            let place = match &row.offscreen {
                Some((name, mon, ws)) => format!("ws {name} (monitor {mon}, index {ws})"),
                None => "on screen".to_string(),
            };
            println!("{:>10}  {place:<32}  {}", row.hwnd, row.title);
        }
        return Ok(());
    }
    if std::env::args().any(|a| a == "--version") {
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};
        let text: Vec<u16> = concat!("optim-bar ", env!("CARGO_PKG_VERSION"))
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            MessageBoxW(
                None,
                windows::core::PCWSTR(text.as_ptr()),
                w!("optim-bar"),
                MB_OK,
            );
        }
        return Ok(());
    }
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("optim_bar_single_instance_mutex"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        tray::ensure_host();

        // Alt+Tab replacement. Owns a dedicated thread so its keyboard hook
        // never waits on this loop's rendering — a low-level hook that misses
        // LowLevelHooksTimeout gets silently unhooked by Windows.
        switcher::install(&config::load());

        // Early warning on commit charge, which is what actually wedges the
        // machine — and it climbs for hours before anything feels wrong.
        toast::ensure_registered();
        memguard::install(&config::load());

        // Weather + airing advisor: background fetch, toast on good windows.
        // Runs only when the user has put coordinates in their config.
        if let Some(wcfg) = weather::read_cfg(&config::load()) {
            toast::ensure_registered();
            weather::spawn(wcfg, |h| {
                toast::show(
                    "Good time to air out the apartment",
                    &format!(
                        "{:.0} mph from {} \u{b7} {:.1}\u{b0}F \u{b7} {:.0}% humidity \u{b7} score {}",
                        weather::to_mph(h.wind_kmh),
                        weather::compass(h.wind_dir),
                        weather::to_f(h.temp_c),
                        h.humidity,
                        h.score
                    ),
                );
            });
        }

        // Indoor air monitor on the LAN; also only when configured.
        if let Some(acfg) = air::read_cfg(&config::load()) {
            air::spawn(acfg);
        }

        let mut bars = bar::create_all();
        let main_tid = GetCurrentThreadId();
        std::thread::spawn(move || watch_config(main_tid));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if REBUILD.swap(false, std::sync::atomic::Ordering::Relaxed) {
                for b in &bars {
                    let _ = DestroyWindow(HWND(b.hwnd_val() as *mut _));
                }
                // Give monitors a beat to settle after topology changes.
                std::thread::sleep(std::time::Duration::from_millis(300));
                bars = bar::create_all();
            }
        }
        // Graceful exit (bar menu's Exit): un-reserve work areas, then
        // hand the tray back to explorer.
        for b in &bars {
            let _ = DestroyWindow(HWND(b.hwnd_val() as *mut _));
        }
        drop(bars);
        tray::restore_explorer_tray();
    }
    Ok(())
}
