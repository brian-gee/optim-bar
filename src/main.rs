#![windows_subsystem = "windows"]

mod bar;
mod calendar;
mod config;
mod flyout;
mod json;
mod statspop;
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

        // Weather + airing advisor: background fetch, toast on good windows.
        // Runs only when the user has put coordinates in their config.
        if let Some(wcfg) = weather::read_cfg(&config::load()) {
            toast::ensure_registered();
            weather::spawn(wcfg, |h| {
                toast::show(
                    "Good time to air out the apartment",
                    &format!(
                        "{:.0} km/h from {} \u{b7} {:.0}\u{b0}F \u{b7} {:.0}% humidity \u{b7} score {}",
                        h.wind_kmh,
                        weather::compass(h.wind_dir),
                        h.temp_c * 9.0 / 5.0 + 32.0,
                        h.humidity,
                        h.score
                    ),
                );
            });
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
