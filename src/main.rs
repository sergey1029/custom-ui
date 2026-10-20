#![windows_subsystem = "windows"]

mod appbar;
mod appmenu;
mod apps;
mod calendar;
// The settings table and its helpers are shared with the settings app, which uses more of them.
#[allow(dead_code)]
mod config;
mod controlcenter;
mod coucou;
mod desktop;
mod dock;
mod filesearch;
mod gfx;
mod icons;
mod launchpad;
mod macmenu;
mod media;
mod menubar;
mod net;
mod shelves;
mod spotlight;
mod status;
mod system_taskbar;
mod tasks;
mod tray;
mod winkey;

use windows::core::{w, Result};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

fn main() -> Result<()> {
    // One shell at a time: a second copy would fight the first over the screen edges.
    let _instance = unsafe { CreateMutexW(None, false, w!("CustomUI.Shell")) }?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Shell items and icon extraction want an STA on the UI thread.
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        gfx::enable_dark_menus();
    }
    config::load();
    if let Ok(exe) = std::env::current_exe() {
        config::apply_autostart(&exe);
    }

    // Never leave the user without a taskbar, even if we crash: panics, and
    // native faults (access violations) that never become panics.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        system_taskbar::restore();
        shelves::restore_explorer_icons();
        default_hook(info);
    }));
    unsafe {
        SetUnhandledExceptionFilter(Some(on_crash));
    }

    system_taskbar::hide();
    let result = run();
    shelves::destroy();
    desktop::destroy();
    calendar::destroy();
    controlcenter::destroy();
    tray::destroy();
    dock::destroy();
    launchpad::destroy();
    spotlight::destroy();
    system_taskbar::restore();
    result
}

unsafe extern "system" fn on_crash(_info: *const EXCEPTION_POINTERS) -> i32 {
    system_taskbar::restore();
    shelves::restore_explorer_icons();
    0 // EXCEPTION_CONTINUE_SEARCH: let Windows crash the process as usual
}

/// Runs until the menu bar is closed ("Вернуться к Windows").
fn run() -> Result<()> {
    spotlight::create()?;
    launchpad::create()?;
    // The dock first: the menu bar feeds it the window list as soon as it exists.
    dock::create()?;
    menubar::create()?;
    // Last: from now on our AppBar calls go through it (and on to Explorer).
    if config::flag("tray.enabled") {
        tray::create()?;
    }
    controlcenter::create()?;
    calendar::create()?;
    desktop::create()?;
    shelves::create()?;
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
