//! Discovers top-level windows that belong on the dock, and switches between them.

use std::ffi::c_void;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone)]
pub struct Task {
    pub hwnd: HWND,
    pub title: String,
    /// Full path of the owning process's executable (empty if inaccessible).
    pub exe: String,
    /// Explicit AppUserModelID, set by Store apps and many modern desktop apps.
    pub aumid: Option<String>,
}

/// Returns dock-eligible windows in Z-order (most recently active first).
pub fn collect() -> Vec<Task> {
    let mut list: Vec<Task> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut list as *mut Vec<Task> as isize));
    }
    list
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<Task>);
    if is_task(hwnd) {
        if let Some(title) = title(hwnd) {
            list.push(Task { hwnd, title, exe: exe_path(hwnd), aumid: aumid(hwnd) });
        }
    }
    true.into()
}

/// Same rules Explorer roughly uses: visible, not a tool window, not owned
/// (unless WS_EX_APPWINDOW), and not cloaked (other virtual desktops, suspended UWP).
unsafe fn is_task(hwnd: HWND) -> bool {
    if !IsWindowVisible(hwnd).as_bool() {
        return false;
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    // Click-through overlays (notification bubbles, HUDs) aren't something to switch to.
    let overlay = WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0;
    if ex & overlay == overlay {
        return false;
    }
    let owned = GetWindow(hwnd, GW_OWNER).map(|h| !h.is_invalid()).unwrap_or(false);
    if owned && ex & WS_EX_APPWINDOW.0 == 0 {
        return false;
    }
    if is_cloaked(hwnd) {
        return false;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return false;
    }
    class_name(hwnd) != "Progman"
}

unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut c_void,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked != 0
}

pub unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

pub unsafe fn title(hwnd: HWND) -> Option<String> {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut buf);
    Some(String::from_utf16_lossy(&buf[..copied as usize]))
}

pub unsafe fn exe_path(hwnd: HWND) -> String {
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
    let _ = CloseHandle(process);
    if ok {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

unsafe fn aumid(hwnd: HWND) -> Option<String> {
    let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
    let id = store.GetValue(&PKEY_AppUserModel_ID).ok()?.to_string();
    (!id.is_empty()).then_some(id)
}

/// Dock click on an app's only window: restore, focus, or minimize if already in front.
pub unsafe fn activate(hwnd: HWND) {
    if !IsIconic(hwnd).as_bool() && GetForegroundWindow() == hwnd {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    } else {
        focus(hwnd);
    }
}

/// Brings the window to the front, restoring it if minimized.
pub unsafe fn focus(hwnd: HWND) {
    if IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    let _ = SetForegroundWindow(hwnd);
}
