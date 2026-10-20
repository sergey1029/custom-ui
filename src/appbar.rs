//! Registers shell AppBars so maximized windows don't overlap the menu bar or the dock.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, WM_APP};

/// Message the system sends us for AppBar notifications (ABN_*).
pub const WM_APPBAR_CALLBACK: u32 = WM_APP + 1;

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        ..Default::default()
    }
}

pub fn register(hwnd: HWND) {
    let mut abd = data(hwnd);
    abd.uCallbackMessage = WM_APPBAR_CALLBACK;
    unsafe { SHAppBarMessage(ABM_NEW, &mut abd) };
}

pub fn unregister(hwnd: HWND) {
    let mut abd = data(hwnd);
    unsafe { SHAppBarMessage(ABM_REMOVE, &mut abd) };
}

/// Reserves a strip of `thickness` px along the top or bottom edge (`ABE_TOP` /
/// `ABE_BOTTOM`) of the primary monitor and returns it. Positioning the window
/// is up to the caller, since it may be taller than what it reserves.
pub fn reserve(hwnd: HWND, edge: u32, thickness: i32) -> RECT {
    let mut abd = data(hwnd);
    abd.uEdge = edge;
    let bottom = edge == ABE_BOTTOM;
    unsafe {
        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        abd.rc = if bottom {
            RECT { left: 0, top: height - thickness, right: width, bottom: height }
        } else {
            RECT { left: 0, top: 0, right: width, bottom: thickness }
        };

        // The system may shift the rect if other AppBars already occupy the edge.
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        if bottom {
            abd.rc.top = abd.rc.bottom - thickness;
        } else {
            abd.rc.bottom = abd.rc.top + thickness;
        }
        SHAppBarMessage(ABM_SETPOS, &mut abd);
    }
    abd.rc
}
