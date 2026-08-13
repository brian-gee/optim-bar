#![windows_subsystem = "windows"]

mod bar;
mod config;
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
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostMessageW, TranslateMessage, MSG,
};

use bar::{Bar, WM_APP_RELOAD};

/// Watches the config directory; posts WM_APP_RELOAD on changes (debounced).
fn watch_config(hwnd_val: isize) {
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
            let _ = PostMessageW(
                Some(HWND(hwnd_val as *mut _)),
                WM_APP_RELOAD,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn main() -> Result<()> {
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("optim_bar_single_instance_mutex"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        let bar = Bar::create()?;
        let hwnd_val = bar.hwnd_val();
        std::thread::spawn(move || watch_config(hwnd_val));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
