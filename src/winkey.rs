//! Keeps the Win key from opening Windows' Start menu. A lone Win press opens
//! Spotlight instead; Win+<key> shortcuts (Win+E, Win+D, Win+Shift+S, ...) keep
//! working.
//!
//! Start opens when Win is released with no other key pressed in between, so on
//! a lone release we inject an unassigned "mask" key first (the technique
//! AutoHotkey uses). The low-level hook lives on its own thread: Windows drops
//! hooks whose thread doesn't answer in time, and the UI thread is sometimes busy.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the target window on a lone Win press.
pub const WM_WIN_KEY: u32 = WM_APP + 3;

/// Unassigned virtual key: pressing it does nothing but counts as "another key".
const MASK_KEY: u16 = 0xE8;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static OTHER_KEY: AtomicBool = AtomicBool::new(false);

pub fn start(target: HWND) {
    TARGET.store(target.0 as isize, Ordering::SeqCst);
    std::thread::spawn(|| unsafe {
        let Ok(module) = GetModuleHandleW(None) else { return };
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0) else { return };
        // The hook is called through this thread's message loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let message = wparam.0 as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let up = message == WM_KEYUP || message == WM_SYSKEYUP;
        let is_win = info.vkCode == VK_LWIN.0 as u32 || info.vkCode == VK_RWIN.0 as u32;
        let is_mask = info.vkCode == MASK_KEY as u32 && info.flags.0 & LLKHF_INJECTED.0 != 0;
        if is_win && down {
            // Auto-repeat sends more downs; only the first one starts a press.
            if !WIN_DOWN.swap(true, Ordering::SeqCst) {
                OTHER_KEY.store(false, Ordering::SeqCst);
            }
        } else if is_win && up {
            WIN_DOWN.store(false, Ordering::SeqCst);
            if !OTHER_KEY.load(Ordering::SeqCst) {
                mask();
                let target = HWND(TARGET.load(Ordering::SeqCst) as *mut _);
                let _ = PostMessageW(target, WM_WIN_KEY, WPARAM(0), LPARAM(0));
            }
        } else if down && !is_mask && WIN_DOWN.load(Ordering::SeqCst) {
            OTHER_KEY.store(true, Ordering::SeqCst);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Taps the mask key while Win is still down, so Windows sees a Win+<key> combo.
unsafe fn mask() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(MASK_KEY), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}
