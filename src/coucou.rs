//! Keeps Coucou's notch bubble at the very top of the screen, inside the bar's notch.
//! Coucou docks the bubble to the top of the work area, which our AppBar pushes down.

use std::cell::Cell;

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::tasks;

thread_local! {
    static BUBBLE: Cell<(HWND, HWINEVENTHOOK)> = Cell::new((HWND::default(), HWINEVENTHOOK::default()));
}

/// Finds the bubble if needed and moves it back to the top edge. Cheap enough to
/// call on every refresh; the location hook handles moves in between.
pub unsafe fn pin() {
    let (mut bubble, _) = BUBBLE.get();
    if !IsWindow(bubble).as_bool() {
        release();
        bubble = find();
        if bubble.is_invalid() {
            return;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(bubble, Some(&mut pid));
        // Scoped to Coucou's process: location changes are far too noisy system-wide.
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(on_location_change),
            pid,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        BUBBLE.set((bubble, hook));
    }
    move_to_top(bubble);
}

pub unsafe fn release() {
    let (_, hook) = BUBBLE.take();
    if !hook.is_invalid() {
        let _ = UnhookWinEvent(hook);
    }
}

unsafe fn move_to_top(hwnd: HWND) {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_ok() && rect.top != 0 {
        let _ = SetWindowPos(hwnd, None, rect.left, 0, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

unsafe extern "system" fn on_location_change(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object == OBJID_WINDOW.0 && hwnd == BUBBLE.get().0 {
        move_to_top(hwnd);
    }
}

unsafe fn find() -> HWND {
    let mut found = HWND::default();
    let _ = EnumWindows(Some(find_proc), LPARAM(&mut found as *mut HWND as isize));
    found
}

/// Coucou has several click-through overlays with the same class and title;
/// the bubble is the one centered horizontally near the top of the screen.
unsafe extern "system" fn find_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let centered = (rect.left + rect.right - GetSystemMetrics(SM_CXSCREEN)).abs() <= 2;
    let near_top = rect.top < GetSystemMetrics(SM_CYSCREEN) / 4;
    let is_bubble = ex & WS_EX_TRANSPARENT.0 != 0
        && centered
        && near_top
        && tasks::class_name(hwnd) == "Tauri Window"
        && tasks::title(hwnd).as_deref() == Some("Coucou");
    if is_bubble {
        *(lparam.0 as *mut HWND) = hwnd;
    }
    (!is_bubble).into()
}
