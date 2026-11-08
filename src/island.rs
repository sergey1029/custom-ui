//! Dynamic Island (the user's own WPF app, a black "pill" at the top center) as the
//! bar's notch. The bar keeps its middle free while the island runs, stays just
//! below it in Z-order, and tells it through a window property whether to dock:
//! the island then sits flush with the top edge at the bar's height, square-topped
//! like a MacBook notch, and grows out of the bar when it expands.

use std::cell::Cell;

use windows::core::w;
use windows::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{config, tasks};

/// Window property on the bar that the island reads; nonzero = dock into the bar.
const DOCK_PROP: windows::core::PCWSTR = w!("CustomUI.IslandDock");

thread_local! {
    static ISLAND: Cell<HWND> = Cell::new(HWND::default());
}

/// The middle of the bar is set to Dynamic Island (not Coucou or nothing).
pub fn docking() -> bool {
    config::text("menubar.island") == "dynamic"
}

/// Whether the island is running (as of the last [`sync`]).
pub fn present() -> bool {
    !ISLAND.get().is_invalid()
}

/// Finds the island, publishes the dock flag and keeps the bar under it.
/// Cheap; called every second and on window events. Returns true when the island
/// appeared or went away, so the bar can lay itself out again.
pub unsafe fn sync(bar: HWND) -> bool {
    let dock = docking();
    let flag = HANDLE(dock as usize as *mut _);
    if GetPropW(bar, DOCK_PROP) != flag {
        let _ = SetPropW(bar, DOCK_PROP, flag);
    }

    let old = ISLAND.get();
    if !old.is_invalid() && IsWindow(old).as_bool() {
        return false;
    }
    let found = find();
    ISLAND.set(found);
    if !found.is_invalid() && dock {
        stay_below(bar);
    }
    found != old
}

/// Puts the bar directly under the island, both staying topmost: the bar's glass
/// is continuous, the island has to be the one on top.
pub unsafe fn stay_below(bar: HWND) {
    let island = ISLAND.get();
    if !island.is_invalid() && docking() {
        let _ = SetWindowPos(bar, island, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

pub unsafe fn release(bar: HWND) {
    let _ = RemovePropW(bar, DOCK_PROP);
    ISLAND.set(HWND::default());
}

unsafe fn find() -> HWND {
    let mut found = HWND::default();
    let _ = EnumWindows(Some(find_proc), LPARAM(&mut found as *mut HWND as isize));
    found
}

/// WPF names its window classes "HwndWrapper[<assembly>;;<guid>]".
unsafe extern "system" fn find_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let is_island = tasks::class_name(hwnd).starts_with("HwndWrapper[DynamicIsland;")
        && tasks::title(hwnd).as_deref() == Some("Dynamic Island")
        && IsWindowVisible(hwnd).as_bool();
    if is_island {
        *(lparam.0 as *mut HWND) = hwnd;
    }
    (!is_island).into()
}
