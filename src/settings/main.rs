//! "Настройки Custom UI": a System Settings–style window for config.ini. Its
//! pages are built from `config::SETTINGS`, so a setting added there shows up
//! here by itself. «Применить» saves and restarts the shell.

#![windows_subsystem = "windows"]

#[allow(dead_code)]
#[path = "../config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../gfx.rs"]
mod gfx;
#[allow(dead_code)]
#[path = "../icons.rs"]
mod icons;
#[allow(dead_code)]
#[path = "../macmenu.rs"]
mod macmenu;

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use config::{Kind, Setting, SECTIONS, SETTINGS};
use gfx::{rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use macmenu::{Anchor, Menu};

const WIDTH: f32 = 900.0;
const HEIGHT: f32 = 640.0;
const SIDEBAR: f32 = 232.0;
const SIDE_ROW: f32 = 34.0;
const SIDE_ICON: f32 = 22.0;
const PAD: f32 = 28.0;
const TITLE_H: f32 = 56.0;
const ROW: f32 = 46.0;
const ROW_HINT: f32 = 60.0;
const CARD_RADIUS: f32 = 12.0;
const FOOTER: f32 = 60.0;
const TOGGLE_W: f32 = 40.0;
const TOGGLE_H: f32 = 24.0;
const SLIDER_W: f32 = 190.0;
const VALUE_W: f32 = 64.0;
const SWATCH: f32 = 22.0;
const SWATCH_GAP: f32 = 8.0;
const CHOICE_W: f32 = 230.0;
const FIELD_W: f32 = 280.0;
const CONTROL_H: f32 = 28.0;
const BUTTON_W: f32 = 132.0;

const COLOR_SIDEBAR: COLORREF = rgb(40, 40, 44);
const COLOR_CONTENT: COLORREF = rgb(29, 29, 32);
const COLOR_CARD: COLORREF = rgb(44, 44, 48);
const COLOR_DIVIDER: COLORREF = rgb(58, 58, 64);
const COLOR_CONTROL: COLORREF = rgb(72, 72, 78);
const COLOR_CONTROL_HOVER: COLORREF = rgb(88, 88, 96);
const COLOR_FIELD: COLORREF = rgb(56, 56, 62);
const COLOR_TOGGLE_OFF: COLORREF = rgb(80, 80, 88);
const COLOR_SIDE_SELECTED: COLORREF = rgb(66, 66, 74);

/// macOS's accent choices, plus graphite.
const ACCENTS: [&str; 8] = ["0062E1", "A550A7", "F74F9E", "FF5257", "F7821B", "FFC600", "62BA46", "8C8C8C"];
/// Sidebar icon glyph (Segoe Fluent Icons) and tile color per section.
const SECTION_ICONS: [(&str, COLORREF); 6] = [
    ("\u{E713}", rgb(142, 142, 147)),
    ("\u{E790}", rgb(175, 82, 222)),
    ("\u{E8A0}", rgb(10, 132, 255)),
    ("\u{E7C4}", rgb(48, 176, 199)),
    ("\u{E7F4}", rgb(52, 199, 89)),
    ("\u{E721}", rgb(255, 149, 0)),
];

const WM_SHELL_RESTARTED: u32 = WM_APP + 1;
/// MAKEINTRESOURCE(1): the icon from assets/settings.rc. A resource id, not a pointer.
#[allow(clippy::manual_dangling_ptr)]
const APP_ICON: PCWSTR = PCWSTR(1 as *const u16);
const EN_CHANGE: u32 = 0x0300;
const ES_AUTOHSCROLL: u32 = 0x0080;

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Section(usize),
    Toggle(&'static str),
    Slider(&'static str, RECT),
    Swatch(&'static str, &'static str),
    CustomColor(&'static str),
    Choice(&'static str, RECT),
    Reset,
    Apply,
    ShellPower,
}

#[derive(Default)]
struct Fonts {
    normal: HFONT,
    small: HFONT,
    bold: HFONT,
    title: HFONT,
    glyphs: HFONT,
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    section: usize,
    scroll: i32,
    content_height: i32,
    targets: Vec<(RECT, Target)>,
    hover: Option<Target>,
    dragging: Option<&'static str>,
    /// Text fields on the current page: setting key and EDIT control.
    edits: Vec<(&'static str, HWND)>,
    field_brush: HBRUSH,
    fonts: Fonts,
    dirty: bool,
    status: String,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn main() -> Result<()> {
    unsafe {
        // Already open: bring that window forward instead.
        if let Ok(existing) = FindWindowW(w!("CustomUI.Settings"), None) {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetForegroundWindow(existing);
            return Ok(());
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        gfx::enable_dark_menus();
    }
    config::load();
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: LoadIconW(instance, APP_ICON).unwrap_or_default(),
            lpszClassName: w!("CustomUI.Settings"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("CustomUI.Settings"),
            w!("Настройки Custom UI"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?;
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.field_brush = CreateSolidBrush(COLOR_FIELD);
            s.status = shell_status();
        });
        let dark: BOOL = TRUE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const c_void, 4);
        let caption = COLOR_SIDEBAR;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as *const c_void, 4);
        update_fonts(hwnd);
        resize(hwnd, None);
        rebuild_edits();
        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd).max(96) as f32 / 96.0 }
}

/// Sizes the window for its DPI: centered on first show, or into `suggested` on a DPI change.
unsafe fn resize(hwnd: HWND, suggested: Option<RECT>) {
    let dpi = scale(hwnd);
    let mut r = RECT { left: 0, top: 0, right: (WIDTH * dpi) as i32, bottom: (HEIGHT * dpi) as i32 };
    let _ = AdjustWindowRectEx(&mut r, WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, false, WINDOW_EX_STYLE(0));
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let (x, y) = match suggested {
        Some(s) => (s.left, s.top),
        None => ((GetSystemMetrics(SM_CXSCREEN) - w) / 2, (GetSystemMetrics(SM_CYSCREEN) - h) / 2),
    };
    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
}

unsafe fn update_fonts(hwnd: HWND) {
    let dpi = scale(hwnd);
    let size = |v: f32| -(v * dpi).round() as i32;
    let fonts = Fonts {
        normal: gfx::create_font(size(13.0), FW_NORMAL, gfx::ui_font()),
        small: gfx::create_font(size(11.5), FW_NORMAL, gfx::ui_font()),
        bold: gfx::create_font(size(13.0), FW_SEMIBOLD, gfx::ui_font()),
        title: gfx::create_font(size(20.0), FW_BOLD, gfx::DISPLAY_FONT),
        glyphs: gfx::create_font(size(13.0), FW_NORMAL, w!("Segoe Fluent Icons")),
    };
    let (old, edits) = STATE.with_borrow_mut(|s| (std::mem::replace(&mut s.fonts, fonts), s.edits.clone()));
    for (_, edit) in edits {
        set_edit_font(edit);
    }
    for f in [old.normal, old.small, old.bold, old.title, old.glyphs].into_iter().filter(|f| !f.is_invalid()) {
        let _ = DeleteObject(f);
    }
}

unsafe fn set_edit_font(edit: HWND) {
    let font = STATE.with_borrow(|s| s.fonts.normal);
    SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
}

fn section_settings(section: usize) -> impl Iterator<Item = &'static Setting> {
    SETTINGS.iter().filter(move |s| s.section == SECTIONS[section].0)
}

/// Text fields are real EDIT controls (caret, selection, IME and clipboard for free).
unsafe fn rebuild_edits() {
    let (hwnd, section, old) = STATE.with_borrow_mut(|s| (s.hwnd, s.section, std::mem::take(&mut s.edits)));
    for (_, edit) in old {
        let _ = DestroyWindow(edit);
    }
    let mut edits = Vec::new();
    for setting in section_settings(section).filter(|s| matches!(s.kind, Kind::Text)) {
        let Ok(edit) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            &windows::core::HSTRING::from(config::text(setting.key)),
            WS_CHILD | WINDOW_STYLE(ES_AUTOHSCROLL),
            0,
            0,
            0,
            0,
            hwnd,
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        ) else {
            continue;
        };
        set_edit_font(edit);
        edits.push((setting.key, edit));
    }
    STATE.with_borrow_mut(|s| s.edits = edits);
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn edit_text(edit: HWND) -> String {
    let mut buffer = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
    let n = GetWindowTextW(edit, &mut buffer);
    String::from_utf16_lossy(&buffer[..n.max(0) as usize])
}

fn shell_exe() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("custom-ui.exe"))
}

fn shell_window() -> Option<HWND> {
    unsafe { FindWindowW(w!("CustomUI.MenuBar"), None).ok() }
}

fn shell_status() -> String {
    if shell_window().is_some() {
        "Оболочка запущена".to_owned()
    } else {
        "Оболочка не запущена — сейчас обычный интерфейс Windows".to_owned()
    }
}

/// Closes the running shell (it restores Windows' taskbar on the way out) and
/// waits for it to be gone.
fn stop_shell() {
    let Some(bar) = shell_window() else { return };
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(bar, Some(&mut pid));
        let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok();
        let _ = PostMessageW(bar, WM_CLOSE, WPARAM(0), LPARAM(0));
        if let Some(process) = process {
            WaitForSingleObject(process, 8000);
            let _ = CloseHandle(process);
        }
    }
}

fn start_shell() {
    if let Some(exe) = shell_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// Off the UI thread: waiting for the old shell can take a few seconds.
fn restart_shell(hwnd: HWND, start: bool) {
    let notify = hwnd.0 as isize;
    std::thread::spawn(move || {
        stop_shell();
        if start {
            start_shell();
            // Give it a moment to put its menu bar up before reporting.
            for _ in 0..30 {
                if shell_window().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        unsafe {
            let _ = PostMessageW(HWND(notify as *mut _), WM_SHELL_RESTARTED, WPARAM(0), LPARAM(0));
        }
    });
}

unsafe fn apply(hwnd: HWND) {
    let edits = STATE.with_borrow(|s| s.edits.clone());
    for (key, edit) in edits {
        config::set(key, edit_text(edit).trim());
    }
    config::save();
    if let Some(exe) = shell_exe() {
        config::apply_autostart(&exe);
    }
    STATE.with_borrow_mut(|s| {
        s.dirty = false;
        s.status = "Перезапуск оболочки…".to_owned();
    });
    restart_shell(hwnd, true);
}

fn changed(hwnd: HWND) {
    STATE.with_borrow_mut(|s| s.dirty = true);
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            paint(hwnd, dc);
            let _ = EndPaint(hwnd, &ps);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_LBUTTONDOWN => on_down(hwnd, point(lparam)),
        WM_MOUSEMOVE => on_move(hwnd, point(lparam)),
        WM_LBUTTONUP => {
            if STATE.with_borrow_mut(|s| s.dragging.take()).is_some() {
                let _ = ReleaseCapture();
            }
        }
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| s.hover = None);
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16 & 0xFFFF) as i16 as i32;
            let step = (ROW * scale(hwnd)) as i32;
            let view = view_height(hwnd);
            STATE.with_borrow_mut(|s| {
                let max = (s.content_height - view).max(0);
                s.scroll = (s.scroll - delta * step / 120).clamp(0, max);
            });
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_COMMAND => {
            if (wparam.0 >> 16) as u32 == EN_CHANGE {
                let edit = HWND(lparam.0 as *mut _);
                let key = STATE.with_borrow(|s| s.edits.iter().find(|(_, e)| *e == edit).map(|(k, _)| *k));
                if let Some(key) = key {
                    config::set(key, edit_text(edit).trim());
                    STATE.with_borrow_mut(|s| s.dirty = true);
                    // Only the footer changes; leave the field itself alone while typing.
                    let footer = footer_rect(hwnd);
                    let _ = InvalidateRect(hwnd, Some(&footer), false);
                }
            }
        }
        WM_CTLCOLOREDIT => {
            let dc = HDC(wparam.0 as *mut _);
            SetTextColor(dc, COLOR_TEXT);
            SetBkColor(dc, COLOR_FIELD);
            return LRESULT(STATE.with_borrow(|s| s.field_brush.0 as isize));
        }
        WM_SHELL_RESTARTED => {
            STATE.with_borrow_mut(|s| s.status = shell_status());
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_DPICHANGED => {
            let suggested = *(lparam.0 as *const RECT);
            update_fonts(hwnd);
            resize(hwnd, Some(suggested));
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_CLOSE => {
            let dirty = STATE.with_borrow(|s| s.dirty);
            if dirty {
                let answer = MessageBoxW(
                    hwnd,
                    w!("Применить изменения перед закрытием?"),
                    w!("Настройки Custom UI"),
                    MB_YESNOCANCEL | MB_ICONQUESTION,
                );
                if answer == IDCANCEL {
                    return LRESULT(0);
                }
                if answer == IDYES {
                    apply(hwnd);
                    // The restart runs on its own; it doesn't need this window.
                }
            }
            let _ = DestroyWindow(hwnd);
        }
        WM_DESTROY => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

fn point(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 }
}

fn inside(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

unsafe fn client(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    r
}

unsafe fn footer_rect(hwnd: HWND) -> RECT {
    let c = client(hwnd);
    let dpi = scale(hwnd);
    RECT { left: (SIDEBAR * dpi) as i32, top: c.bottom - (FOOTER * dpi) as i32, ..c }
}

/// Height of the scrolling part of the page.
unsafe fn view_height(hwnd: HWND) -> i32 {
    let c = client(hwnd);
    c.bottom - (FOOTER * scale(hwnd)) as i32
}

fn target_at(p: POINT) -> Option<Target> {
    STATE.with_borrow(|s| s.targets.iter().rev().find(|(r, _)| inside(r, p)).map(|(_, t)| *t))
}

unsafe fn on_move(hwnd: HWND, p: POINT) {
    if let Some(key) = STATE.with_borrow(|s| s.dragging) {
        let track = STATE.with_borrow(|s| {
            s.targets.iter().find_map(|(_, t)| match t {
                Target::Slider(k, track) if *k == key => Some(*track),
                _ => None,
            })
        });
        if let Some(track) = track {
            set_slider(hwnd, key, &track, p.x);
        }
        return;
    }
    let hover = target_at(p);
    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
    let _ = TrackMouseEvent(&mut tme);
    if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe fn set_slider(hwnd: HWND, key: &'static str, track: &RECT, x: i32) {
    let Some(Kind::Int { min, max, step, .. }) = config::find(key).map(|s| &s.kind) else { return };
    let t = ((x - track.left) as f32 / (track.right - track.left).max(1) as f32).clamp(0.0, 1.0);
    let raw = *min as f32 + t * (*max - *min) as f32;
    let value = (min + (((raw - *min as f32) / *step as f32).round() as i32) * step).clamp(*min, *max);
    if value != config::int(key) {
        config::set(key, value.to_string());
        changed(hwnd);
    }
}

unsafe fn on_down(hwnd: HWND, p: POINT) {
    let Some(target) = target_at(p) else { return };
    match target {
        Target::Section(i) => {
            STATE.with_borrow_mut(|s| {
                s.section = i;
                s.scroll = 0;
            });
            rebuild_edits();
        }
        Target::Toggle(key) => {
            config::set_flag(key, !config::flag(key));
            changed(hwnd);
        }
        Target::Slider(key, track) => {
            STATE.with_borrow_mut(|s| s.dragging = Some(key));
            SetCapture(hwnd);
            set_slider(hwnd, key, &track, p.x);
        }
        Target::Swatch(key, hex) => {
            config::set(key, hex);
            changed(hwnd);
        }
        Target::CustomColor(key) => {
            let (r, g, b) = config::color(key);
            let mut custom = [COLORREF(0xFFFFFF); 16];
            let mut cc = CHOOSECOLORW {
                lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
                hwndOwner: hwnd,
                rgbResult: rgb(r, g, b),
                lpCustColors: custom.as_mut_ptr(),
                Flags: CC_FULLOPEN | CC_RGBINIT,
                ..Default::default()
            };
            if ChooseColorW(&mut cc).as_bool() {
                let c = cc.rgbResult.0;
                config::set(key, format!("{:02X}{:02X}{:02X}", c & 0xFF, c >> 8 & 0xFF, c >> 16 & 0xFF));
                changed(hwnd);
            }
        }
        Target::Choice(key, button) => {
            let Some(Kind::Choice(options)) = config::find(key).map(|s| &s.kind) else { return };
            let current = config::text(key);
            let mut menu = Menu::new();
            for (i, (value, label)) in options.iter().enumerate() {
                menu.check(i + 1, *label, *value == current);
            }
            let mut on_screen = button;
            let _ = MapWindowPoints(hwnd, None, std::slice::from_raw_parts_mut(&mut on_screen as *mut RECT as *mut POINT, 2));
            let chosen = menu.show(Anchor::Below(on_screen));
            if let Some((value, _)) = chosen.checked_sub(1).and_then(|i| options.get(i)) {
                config::set(key, *value);
                changed(hwnd);
            }
        }
        Target::Reset => {
            let section = STATE.with_borrow(|s| s.section);
            for setting in section_settings(section) {
                config::reset(setting.key);
            }
            rebuild_edits();
            changed(hwnd);
        }
        Target::Apply => apply(hwnd),
        Target::ShellPower => {
            let running = shell_window().is_some();
            STATE.with_borrow_mut(|s| s.status = if running { "Возврат к Windows…" } else { "Запуск оболочки…" }.to_owned());
            let _ = InvalidateRect(hwnd, None, false);
            restart_shell(hwnd, !running);
        }
    }
}

// ---- drawing ----

unsafe fn paint(hwnd: HWND, window_dc: HDC) {
    let c = client(hwnd);
    let Some(mut canvas) = Canvas::new(c.right, c.bottom) else { return };
    let dpi = scale(hwnd);
    let px = |v: f32| (v * dpi).round() as i32;
    let dc = canvas.dc;
    let mut targets = Vec::new();
    let mut edit_spots = Vec::new();

    let state = STATE.with_borrow(|s| (s.section, s.scroll, s.hover, s.dirty, s.status.clone()));
    let (section, scroll, hover, dirty, status) = state;
    let fonts = STATE.with_borrow(|s| (s.fonts.normal, s.fonts.small, s.fonts.bold, s.fonts.title, s.fonts.glyphs));
    let (normal, small, bold, title, glyphs) = fonts;
    let accent = gfx::accent();

    // Sidebar.
    let sidebar = RECT { right: px(SIDEBAR), ..c };
    gfx::fill(dc, &sidebar, COLOR_SIDEBAR);
    let content = RECT { left: sidebar.right, ..c };
    gfx::fill(dc, &content, COLOR_CONTENT);

    let old_font = SelectObject(dc, bold);
    SetTextColor(dc, COLOR_TEXT);
    let mut y = px(18.0);
    gfx::draw_text(dc, "Custom UI", &RECT { left: px(20.0), top: y, right: sidebar.right - px(12.0), bottom: y + px(22.0) }, DT_LEFT);
    SelectObject(dc, small);
    SetTextColor(dc, COLOR_DIM);
    gfx::draw_text(
        dc,
        concat!("Версия ", env!("CARGO_PKG_VERSION")),
        &RECT { left: px(20.0), top: y + px(20.0), right: sidebar.right - px(12.0), bottom: y + px(38.0) },
        DT_LEFT,
    );
    y += px(54.0);
    for (i, (_, name)) in SECTIONS.iter().enumerate() {
        let row = RECT { left: px(10.0), top: y, right: sidebar.right - px(10.0), bottom: y + px(SIDE_ROW) };
        if i == section {
            canvas.fill_shape(&row, 7.0 * dpi, COLOR_SIDE_SELECTED, 255);
        } else if hover == Some(Target::Section(i)) {
            canvas.fill_shape(&row, 7.0 * dpi, COLOR_SIDE_SELECTED, 120);
        }
        let (glyph, tile) = SECTION_ICONS[i];
        let icon = RECT { left: row.left + px(8.0), top: row.top + (px(SIDE_ROW) - px(SIDE_ICON)) / 2, right: 0, bottom: 0 };
        let icon = RECT { right: icon.left + px(SIDE_ICON), bottom: icon.top + px(SIDE_ICON), ..icon };
        canvas.fill_shape(&icon, 5.5 * dpi, tile, 255);
        SelectObject(dc, glyphs);
        SetTextColor(dc, rgb(255, 255, 255));
        gfx::draw_text(dc, glyph, &icon, DT_CENTER);
        SelectObject(dc, normal);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, name, &RECT { left: icon.right + px(10.0), right: row.right - px(6.0), ..row }, DT_LEFT | DT_END_ELLIPSIS);
        targets.push((row, Target::Section(i)));
        y += px(SIDE_ROW) + px(2.0);
    }

    // Page: title, then one card with a row per setting.
    let view_bottom = c.bottom - px(FOOTER);
    let left = content.left + px(PAD);
    let right = content.right - px(PAD);
    let mut y = px(PAD) - scroll;
    SelectObject(dc, title);
    SetTextColor(dc, COLOR_TEXT);
    gfx::draw_text(dc, SECTIONS[section].1, &RECT { left, top: y, right, bottom: y + px(TITLE_H) - px(16.0) }, DT_LEFT);
    y += px(TITLE_H);

    let settings: Vec<&Setting> = section_settings(section).collect();
    let heights: Vec<i32> = settings.iter().map(|s| px(if s.hint.is_empty() { ROW } else { ROW_HINT })).collect();
    let card = RECT { left, top: y, right, bottom: y + heights.iter().sum::<i32>() };
    canvas.fill_shape(&card, CARD_RADIUS * dpi, COLOR_CARD, 255);
    canvas.stroke_shape(&card, CARD_RADIUS * dpi, rgb(255, 255, 255), 14);
    for (n, (setting, h)) in settings.iter().zip(&heights).enumerate() {
        let row = RECT { left: card.left + px(16.0), top: y, right: card.right - px(16.0), bottom: y + h };
        if n > 0 {
            gfx::fill(dc, &RECT { top: row.top, bottom: row.top + 1.max(px(0.5)), ..row }, COLOR_DIVIDER);
        }
        let control_w = match &setting.kind {
            Kind::Bool => TOGGLE_W,
            Kind::Int { .. } => SLIDER_W + VALUE_W,
            Kind::Color => (ACCENTS.len() + 1) as f32 * (SWATCH + SWATCH_GAP),
            Kind::Choice(_) => CHOICE_W,
            Kind::Text => FIELD_W,
        };
        let label_right = row.right - px(control_w + 20.0);
        SelectObject(dc, normal);
        SetTextColor(dc, COLOR_TEXT);
        if setting.hint.is_empty() {
            gfx::draw_text(dc, setting.label, &RECT { right: label_right, ..row }, DT_LEFT | DT_END_ELLIPSIS);
        } else {
            let mid = row.top + h / 2;
            gfx::draw_text(dc, setting.label, &RECT { top: mid - px(20.0), bottom: mid + px(1.0), right: label_right, ..row }, DT_LEFT | DT_END_ELLIPSIS);
            SelectObject(dc, small);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, setting.hint, &RECT { top: mid + px(1.0), bottom: mid + px(19.0), right: label_right, ..row }, DT_LEFT | DT_END_ELLIPSIS);
        }
        let mid = row.top + h / 2;
        let control = |width: f32, height: f32| RECT { left: row.right - px(width), top: mid - px(height) / 2, right: row.right, bottom: mid - px(height) / 2 + px(height) };
        let visible = row.bottom > 0 && row.top < view_bottom;
        match &setting.kind {
            Kind::Bool => {
                let r = control(TOGGLE_W, TOGGLE_H);
                let on = config::flag(setting.key);
                canvas.fill_shape(&r, TOGGLE_H * dpi / 2.0, if on { accent } else { COLOR_TOGGLE_OFF }, 255);
                let knob = px(TOGGLE_H - 4.0);
                let x = if on { r.right - px(2.0) - knob } else { r.left + px(2.0) };
                let k = RECT { left: x, top: r.top + px(2.0), right: x + knob, bottom: r.top + px(2.0) + knob };
                canvas.fill_shape(&k, knob as f32 / 2.0, rgb(255, 255, 255), 255);
                if visible {
                    targets.push((r, Target::Toggle(setting.key)));
                }
            }
            Kind::Int { min, max, unit, .. } => {
                let value = config::int(setting.key);
                let box_ = control(SLIDER_W + VALUE_W, 22.0);
                let track = RECT { left: box_.left + px(10.0), right: box_.left + px(SLIDER_W - 10.0), top: mid - px(2.0), bottom: mid + px(2.0) };
                let t = (value - min) as f32 / (max - min).max(1) as f32;
                let x = track.left + ((track.right - track.left) as f32 * t).round() as i32;
                canvas.fill_shape(&track, 2.0 * dpi, COLOR_TOGGLE_OFF, 255);
                canvas.fill_shape(&RECT { right: x, ..track }, 2.0 * dpi, accent, 255);
                let knob = px(18.0);
                let k = RECT { left: x - knob / 2, top: mid - knob / 2, right: x - knob / 2 + knob, bottom: mid - knob / 2 + knob };
                canvas.fill_shape(&k, knob as f32 / 2.0, rgb(255, 255, 255), 255);
                SelectObject(dc, normal);
                SetTextColor(dc, COLOR_DIM);
                gfx::draw_text(dc, &format!("{value}{unit}"), &RECT { left: box_.right - px(VALUE_W), ..box_ }, DT_RIGHT);
                if visible {
                    let hit = RECT { left: track.left - px(10.0), right: track.right + px(10.0), top: box_.top, bottom: box_.bottom };
                    targets.push((hit, Target::Slider(setting.key, track)));
                }
            }
            Kind::Color => {
                let current = config::text(setting.key).trim_start_matches('#').to_uppercase();
                let count = ACCENTS.len() as f32 + 1.0;
                let strip = control(count * SWATCH + (count - 1.0) * SWATCH_GAP, SWATCH);
                let mut x = strip.left;
                let custom = !ACCENTS.contains(&current.as_str());
                for hex in ACCENTS.iter().map(|h| Some(*h)).chain([None]) {
                    let r = RECT { left: x, top: strip.top, right: x + px(SWATCH), bottom: strip.bottom };
                    let selected = match hex {
                        Some(h) => h == current,
                        None => custom,
                    };
                    let color = match hex {
                        Some(h) => config::parse_color(h).map(|(r, g, b)| rgb(r, g, b)).unwrap_or(accent),
                        None => if custom { accent } else { COLOR_CONTROL },
                    };
                    canvas.fill_shape(&r, SWATCH * dpi / 2.0, color, 255);
                    if selected {
                        let dot = gfx::centered(&r, px(8.0));
                        canvas.fill_shape(&dot, 4.0 * dpi, rgb(255, 255, 255), 255);
                    }
                    if hex.is_none() && !custom {
                        SelectObject(dc, glyphs);
                        SetTextColor(dc, COLOR_TEXT);
                        gfx::draw_text(dc, "\u{E710}", &r, DT_CENTER);
                    }
                    if visible {
                        targets.push((r, hex.map_or(Target::CustomColor(setting.key), |h| Target::Swatch(setting.key, h))));
                    }
                    x += px(SWATCH + SWATCH_GAP);
                }
            }
            Kind::Choice(options) => {
                let r = control(CHOICE_W, CONTROL_H);
                let current = config::text(setting.key);
                let label = options.iter().find(|(v, _)| *v == current).map_or(current.as_str(), |(_, l)| l);
                let hot = matches!(hover, Some(Target::Choice(k, _)) if k == setting.key);
                canvas.fill_shape(&r, 6.0 * dpi, if hot { COLOR_CONTROL_HOVER } else { COLOR_CONTROL }, 255);
                SelectObject(dc, normal);
                SetTextColor(dc, COLOR_TEXT);
                gfx::draw_text(dc, label, &RECT { left: r.left + px(10.0), right: r.right - px(28.0), ..r }, DT_LEFT | DT_END_ELLIPSIS);
                SelectObject(dc, glyphs);
                gfx::draw_text(dc, "\u{E70D}", &RECT { left: r.right - px(26.0), right: r.right - px(8.0), ..r }, DT_CENTER);
                if visible {
                    targets.push((r, Target::Choice(setting.key, r)));
                }
            }
            Kind::Text => {
                let r = control(FIELD_W, CONTROL_H);
                canvas.fill_shape(&r, 6.0 * dpi, COLOR_FIELD, 255);
                let inner = RECT { left: r.left + px(8.0), top: mid - px(9.0), right: r.right - px(8.0), bottom: mid + px(9.0) };
                edit_spots.push((setting.key, inner, r.top >= 0 && r.bottom <= view_bottom));
            }
        }
        y += h;
    }
    let content_height = card.bottom + scroll + px(PAD);

    // Footer: shell status and power on the left, reset and apply on the right.
    let footer = RECT { left: content.left, top: view_bottom, ..c };
    gfx::fill(dc, &footer, COLOR_CONTENT);
    gfx::fill(dc, &RECT { bottom: footer.top + 1, ..footer }, COLOR_DIVIDER);
    let mid = (footer.top + footer.bottom) / 2;
    let button = |right: i32, width: i32| RECT { left: right - width, top: mid - px(14.0), right, bottom: mid + px(14.0) };
    let apply = button(footer.right - px(PAD), px(BUTTON_W));
    let reset = button(apply.left - px(10.0), px(BUTTON_W));
    let running = shell_window().is_some();
    let power = RECT { left: footer.left + px(PAD), right: footer.left + px(PAD) + px(BUTTON_W + 40.0), ..apply };
    let apply_color = if dirty { accent } else { COLOR_CONTROL };
    let draw_button = |canvas: &mut Canvas, r: &RECT, target: Target, base: COLORREF| {
        let color = if hover == Some(target) && base == COLOR_CONTROL { COLOR_CONTROL_HOVER } else { base };
        canvas.fill_shape(r, 6.0 * dpi, color, 255);
    };
    draw_button(&mut canvas, &apply, Target::Apply, apply_color);
    draw_button(&mut canvas, &reset, Target::Reset, COLOR_CONTROL);
    draw_button(&mut canvas, &power, Target::ShellPower, COLOR_CONTROL);
    SelectObject(dc, normal);
    SetTextColor(dc, COLOR_TEXT);
    gfx::draw_text(dc, "Применить", &apply, DT_CENTER);
    gfx::draw_text(dc, "Сбросить раздел", &reset, DT_CENTER);
    gfx::draw_text(dc, if running { "Вернуться к Windows" } else { "Запустить оболочку" }, &power, DT_CENTER);
    SelectObject(dc, small);
    SetTextColor(dc, COLOR_DIM);
    let note = if dirty { "Есть неприменённые изменения".to_owned() } else { status };
    gfx::draw_text(dc, &note, &RECT { left: power.right + px(14.0), right: reset.left - px(10.0), ..footer }, DT_LEFT | DT_END_ELLIPSIS | DT_WORDBREAK);
    targets.push((apply, Target::Apply));
    targets.push((reset, Target::Reset));
    targets.push((power, Target::ShellPower));
    SelectObject(dc, old_font);

    let _ = BitBlt(window_dc, 0, 0, c.right, c.bottom, dc, 0, 0, SRCCOPY);
    drop(canvas);

    let edits = STATE.with_borrow_mut(|s| {
        s.targets = targets;
        s.content_height = content_height;
        s.edits.clone()
    });
    for (key, spot, shown) in edit_spots {
        if let Some((_, edit)) = edits.iter().find(|(k, _)| *k == key) {
            let _ = SetWindowPos(*edit, None, spot.left, spot.top, spot.right - spot.left, spot.bottom - spot.top, SWP_NOZORDER | SWP_NOACTIVATE);
            let _ = ShowWindow(*edit, if shown { SW_SHOWNA } else { SW_HIDE });
        }
    }
}
