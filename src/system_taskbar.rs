//! Hides Windows' own taskbar while the shell runs (unless turned off in the
//! settings) and brings it back on exit.
//! It's switched to auto-hide (so it stops reserving screen space) and hidden;
//! the previous auto-hide setting is restored afterwards.
//!
//! Our tray host also has the class `Shell_TrayWnd`, so Explorer's windows are
//! told apart by process.

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Taskbar state from before we touched it; -1 while we haven't.
static SAVED_STATE: AtomicIsize = AtomicIsize::new(-1);
/// Explorer's tray window as of the last `hide`, to recognize Explorer restarts.
static KNOWN_TRAY: AtomicIsize = AtomicIsize::new(0);

pub fn hide() {
    if !crate::config::flag("taskbar.hide") {
        return;
    }
    unsafe {
        let Some(tray) = explorer_tray() else { return };
        KNOWN_TRAY.store(tray.0 as isize, Ordering::SeqCst);
        let mut abd = data(tray);
        let state = SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32;
        if SAVED_STATE.load(Ordering::SeqCst) < 0 {
            SAVED_STATE.store(state as isize, Ordering::SeqCst);
        }
        // Switching the state makes Explorer re-show the taskbar asynchronously,
        // so only do it when needed; `enforce` catches the re-show either way.
        if state & ABS_AUTOHIDE == 0 {
            abd.lParam = LPARAM((state | ABS_AUTOHIDE) as isize);
            SHAppBarMessage(ABM_SETSTATE, &mut abd);
        }
        enforce();
    }
}

/// Hides the taskbar again if Explorer brought it back. Cheap; called periodically.
pub fn enforce() {
    if SAVED_STATE.load(Ordering::SeqCst) < 0 {
        return;
    }
    unsafe {
        for hwnd in trays() {
            if IsWindowVisible(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

/// Safe to call more than once (e.g. from the panic hook and on normal exit).
pub fn restore() {
    let state = SAVED_STATE.swap(-1, Ordering::SeqCst);
    if state < 0 {
        return;
    }
    unsafe {
        if let Some(tray) = explorer_tray() {
            let mut abd = data(tray);
            abd.lParam = LPARAM(state);
            SHAppBarMessage(ABM_SETSTATE, &mut abd);
        }
        for hwnd in trays() {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }
    }
}

/// True when Explorer's tray is a different window than when we last hid it,
/// i.e. a "TaskbarCreated" broadcast came from an Explorer restart, not from us.
pub fn explorer_restarted() -> bool {
    explorer_tray().is_some_and(|tray| tray.0 as isize != KNOWN_TRAY.load(Ordering::SeqCst))
}

/// Explorer's primary taskbar window.
pub fn explorer_tray() -> Option<HWND> {
    unsafe { trays().into_iter().find(|&hwnd| crate::tasks::class_name(hwnd) == "Shell_TrayWnd") }
}

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        ..Default::default()
    }
}

/// Explorer's primary taskbar plus the ones on other monitors.
unsafe fn trays() -> Vec<HWND> {
    let mut list = Vec::new();
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut list as *mut Vec<HWND> as isize));
    list
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let class = crate::tasks::class_name(hwnd);
    if class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd" {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != GetCurrentProcessId() {
            (*(lparam.0 as *mut Vec<HWND>)).push(hwnd);
        }
    }
    true.into()
}
