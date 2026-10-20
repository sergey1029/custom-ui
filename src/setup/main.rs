//! Custom UI's installer and uninstaller, one exe with both programs inside.
//!
//! `CustomUI-Setup.exe`              — window with options, then installs
//! `CustomUI-Setup.exe /S`           — silent install (autostart on, starts the shell)
//! `uninstall.exe /uninstall [/S]`   — removes everything (the installer copies itself there)
//!
//! Everything goes into the user's profile, no admin rights needed:
//! %LOCALAPPDATA%\Programs\CustomUI, Start menu shortcuts, and HKCU entries for
//! "Apps & features" and autostart.

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

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::{w, Interface, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::*;

use gfx::{rgb, Canvas, COLOR_DIM, COLOR_TEXT};

const SHELL_EXE: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/custom-ui.exe"));
const SETTINGS_EXE: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/custom-ui-settings.exe"));
const VERSION: &str = env!("CARGO_PKG_VERSION");

const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\CustomUI";
/// No exe handle may stay open in the folder while it's deleted, so the
/// uninstaller hands that last step to cmd after a short pause.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const WIDTH: f32 = 540.0;
const HEIGHT: f32 = 330.0;
const PAD: f32 = 28.0;
const ICON: f32 = 88.0;
const TOGGLE_W: f32 = 40.0;
const TOGGLE_H: f32 = 24.0;
const ROW: f32 = 40.0;
const BUTTON_W: f32 = 150.0;
const BUTTON_H: f32 = 30.0;

const COLOR_WINDOW: COLORREF = rgb(32, 32, 36);
const COLOR_CONTROL: COLORREF = rgb(72, 72, 78);
const COLOR_CONTROL_HOVER: COLORREF = rgb(88, 88, 96);
const COLOR_TOGGLE_OFF: COLORREF = rgb(80, 80, 88);
const COLOR_ERROR: COLORREF = rgb(255, 105, 97);

#[derive(Clone, PartialEq)]
enum Stage {
    Options,
    Working,
    Done,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Autostart,
    Launch,
    Primary,
    Secondary,
}

struct State {
    hwnd: HWND,
    stage: Stage,
    autostart: bool,
    launch: bool,
    targets: Vec<(RECT, Target)>,
    hover: Option<Target>,
    fonts: [HFONT; 3],
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        hwnd: HWND::default(),
        stage: Stage::Options,
        autostart: true,
        launch: true,
        targets: Vec::new(),
        hover: None,
        fonts: [HFONT::default(); 3],
    });
}

const WM_DONE: u32 = WM_APP + 1;
/// MAKEINTRESOURCE(1): the icon from assets/setup.rc. A resource id, not a pointer.
#[allow(clippy::manual_dangling_ptr)]
const APP_ICON: PCWSTR = PCWSTR(1 as *const u16);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).map(|a| a.to_lowercase()).collect();
    let silent = args.iter().any(|a| a == "/s");
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    if args.iter().any(|a| a == "/uninstall") {
        uninstall(silent);
        return Ok(());
    }
    if silent {
        let _ = install(true, true);
        return Ok(());
    }
    unsafe { run_window() }
}

fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Programs").join("CustomUI")
}

fn start_menu_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("Microsoft\\Windows\\Start Menu\\Programs\\Custom UI"))
}

fn installed() -> bool {
    install_dir().join("custom-ui.exe").exists()
}

// ---- stopping running copies ----

/// Asks the window's process to close and waits for it to exit.
fn close_and_wait(class: PCWSTR, timeout: Duration) {
    unsafe {
        let Ok(window) = FindWindowW(class, None) else { return };
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok();
        let _ = PostMessageW(window, WM_CLOSE, WPARAM(0), LPARAM(0));
        if let Some(process) = process {
            WaitForSingleObject(process, timeout.as_millis() as u32);
            let _ = CloseHandle(process);
        }
    }
}

/// The shell closes itself properly (bringing Windows' taskbar back); anything
/// still running after that is ended so its exe can be replaced.
fn stop_running() {
    close_and_wait(w!("CustomUI.MenuBar"), Duration::from_secs(8));
    close_and_wait(w!("CustomUI.Settings"), Duration::from_secs(3));
    for exe in ["custom-ui.exe", "custom-ui-settings.exe"] {
        let _ = std::process::Command::new("taskkill").args(["/f", "/im", exe]).creation_flags(CREATE_NO_WINDOW).output();
    }
}

// ---- registry ----

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

unsafe fn set_string(key: HKEY, name: &str, value: &str) {
    let data = wide(value);
    let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
    let _ = RegSetValueExW(key, &HSTRING::from(name), 0, REG_SZ, Some(bytes));
}

unsafe fn set_dword(key: HKEY, name: &str, value: u32) {
    let _ = RegSetValueExW(key, &HSTRING::from(name), 0, REG_DWORD, Some(&value.to_le_bytes()));
}

/// The "Apps & features" entry.
fn register_uninstaller(dir: &Path, size_kb: u32) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY), &mut key).is_err() {
            return;
        }
        let uninstaller = dir.join("uninstall.exe");
        set_string(key, "DisplayName", "Custom UI");
        set_string(key, "DisplayVersion", VERSION);
        set_string(key, "Publisher", "Custom UI");
        set_string(key, "DisplayIcon", &dir.join("custom-ui.exe").to_string_lossy());
        set_string(key, "InstallLocation", &dir.to_string_lossy());
        set_string(key, "UninstallString", &format!("\"{}\" /uninstall", uninstaller.display()));
        set_string(key, "QuietUninstallString", &format!("\"{}\" /uninstall /S", uninstaller.display()));
        set_dword(key, "NoModify", 1);
        set_dword(key, "NoRepair", 1);
        set_dword(key, "EstimatedSize", size_kb);
        let _ = RegCloseKey(key);
    }
}

// ---- shortcuts ----

unsafe fn shortcut(link: &Path, target: &Path, description: &str) -> Result<()> {
    let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
    shell_link.SetPath(&HSTRING::from(target.as_os_str()))?;
    if let Some(dir) = target.parent() {
        shell_link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
    }
    shell_link.SetDescription(&HSTRING::from(description))?;
    shell_link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
    shell_link.cast::<IPersistFile>()?.Save(&HSTRING::from(link.as_os_str()), true)
}

// ---- install / uninstall ----

fn install(autostart: bool, launch: bool) -> std::result::Result<(), String> {
    stop_running();
    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать папку {}: {e}", dir.display()))?;
    let shell = dir.join("custom-ui.exe");
    let settings = dir.join("custom-ui-settings.exe");
    let write = |path: &Path, bytes: &[u8]| std::fs::write(path, bytes).map_err(|e| format!("Не удалось записать {}: {e}", path.display()));
    write(&shell, SHELL_EXE)?;
    write(&settings, SETTINGS_EXE)?;
    // The uninstaller is this very exe.
    if let Ok(me) = std::env::current_exe() {
        let target = dir.join("uninstall.exe");
        if me != target {
            std::fs::copy(&me, &target).map_err(|e| format!("Не удалось записать {}: {e}", target.display()))?;
        }
    }

    if let Some(menu) = start_menu_dir() {
        let _ = std::fs::create_dir_all(&menu);
        unsafe {
            let _ = shortcut(&menu.join("Custom UI.lnk"), &shell, "Оболочка в стиле macOS");
            let _ = shortcut(&menu.join("Настройки Custom UI.lnk"), &settings, "Цвета, Dock, строка меню и остальное");
        }
    }
    let size_kb = ((SHELL_EXE.len() + SETTINGS_EXE.len() * 2) / 1024) as u32;
    register_uninstaller(&dir, size_kb);

    config::store_flag("autostart", autostart);
    config::apply_autostart(&shell);
    if launch {
        std::process::Command::new(&shell).current_dir(&dir).spawn().map_err(|e| format!("Не удалось запустить оболочку: {e}"))?;
    }
    Ok(())
}

unsafe fn ask(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    MessageBoxW(None, &HSTRING::from(text), w!("Custom UI"), flags | MB_SETFOREGROUND)
}

fn uninstall(silent: bool) {
    unsafe {
        if !silent && ask("Удалить Custom UI? Панель задач и рабочий стол Windows вернутся как были.", MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
        stop_running();
        let dir = install_dir();
        // Autostart off and gone.
        let mut run = HKEY::default();
        if RegCreateKeyW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"), &mut run).is_ok() {
            let _ = RegDeleteValueW(run, w!("CustomUI"));
            let _ = RegCloseKey(run);
        }
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
        if let Some(menu) = start_menu_dir() {
            let _ = std::fs::remove_dir_all(menu);
        }
        let keep_settings = silent || ask("Сохранить настройки, закреплённые в Dock программы и полки? Они пригодятся, если установить Custom UI снова.", MB_YESNO | MB_ICONQUESTION) == IDYES;
        if !keep_settings {
            if let Some(data) = config::file().and_then(|f| f.parent().map(Path::to_path_buf)) {
                let _ = std::fs::remove_dir_all(data);
            }
        }
        for exe in ["custom-ui.exe", "custom-ui-settings.exe"] {
            let _ = std::fs::remove_file(dir.join(exe));
        }
        // uninstall.exe is running: let cmd remove it and the folder once we're gone.
        let _ = std::process::Command::new("cmd.exe")
            .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display()))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        if !silent {
            ask("Custom UI удалён.", MB_OK | MB_ICONINFORMATION);
        }
    }
}

// ---- window ----

unsafe fn run_window() -> Result<()> {
    let instance = GetModuleHandleW(None)?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hIcon: LoadIconW(instance, APP_ICON).unwrap_or_default(),
        lpszClassName: w!("CustomUI.Setup"),
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), w!("CustomUI.Setup"), w!("Установка Custom UI"), style, 0, 0, 0, 0, None, None, instance, None)?;
    STATE.with_borrow_mut(|s| s.hwnd = hwnd);
    let dark: BOOL = TRUE;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const c_void, 4);
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &COLOR_WINDOW as *const _ as *const c_void, 4);

    let dpi = scale(hwnd);
    let mut r = RECT { left: 0, top: 0, right: (WIDTH * dpi) as i32, bottom: (HEIGHT * dpi) as i32 };
    let _ = AdjustWindowRectEx(&mut r, style, false, WINDOW_EX_STYLE(0));
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let _ = SetWindowPos(hwnd, None, (GetSystemMetrics(SM_CXSCREEN) - w) / 2, (GetSystemMetrics(SM_CYSCREEN) - h) / 2, w, h, SWP_NOZORDER);
    let size = |v: f32| -(v * dpi).round() as i32;
    let fonts = [
        gfx::create_font(size(13.0), FW_NORMAL, gfx::ui_font()),
        gfx::create_font(size(24.0), FW_BOLD, gfx::DISPLAY_FONT),
        gfx::create_font(size(12.0), FW_NORMAL, gfx::ui_font()),
    ];
    STATE.with_borrow_mut(|s| s.fonts = fonts);
    let _ = ShowWindow(hwnd, SW_SHOW);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    Ok(())
}

fn scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd).max(96) as f32 / 96.0 }
}

fn point(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 }
}

fn target_at(p: POINT) -> Option<Target> {
    STATE.with_borrow(|s| s.targets.iter().find(|(r, _)| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom).map(|(_, t)| *t))
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
        WM_MOUSEMOVE => {
            let hover = target_at(point(lparam));
            if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
        WM_LBUTTONDOWN => on_click(hwnd, point(lparam)),
        WM_DONE => {
            let result = Box::from_raw(lparam.0 as *mut std::result::Result<(), String>);
            STATE.with_borrow_mut(|s| s.stage = match *result {
                Ok(()) => Stage::Done,
                Err(e) => Stage::Failed(e),
            });
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_CLOSE => {
            if STATE.with_borrow(|s| s.stage != Stage::Working) {
                let _ = DestroyWindow(hwnd);
            }
        }
        WM_DESTROY => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

unsafe fn on_click(hwnd: HWND, p: POINT) {
    let Some(target) = target_at(p) else { return };
    let stage = STATE.with_borrow(|s| s.stage.clone());
    match (target, stage) {
        (Target::Autostart, Stage::Options) => STATE.with_borrow_mut(|s| s.autostart = !s.autostart),
        (Target::Launch, Stage::Options) => STATE.with_borrow_mut(|s| s.launch = !s.launch),
        (Target::Primary, Stage::Options | Stage::Failed(_)) => {
            let (autostart, launch) = STATE.with_borrow_mut(|s| {
                s.stage = Stage::Working;
                (s.autostart, s.launch)
            });
            let notify = hwnd.0 as isize;
            std::thread::spawn(move || {
                let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let result = Box::new(install(autostart, launch));
                unsafe {
                    let _ = PostMessageW(HWND(notify as *mut _), WM_DONE, WPARAM(0), LPARAM(Box::into_raw(result) as isize));
                }
            });
        }
        (Target::Primary, Stage::Done) => {
            let _ = DestroyWindow(hwnd);
        }
        (Target::Secondary, Stage::Done) => {
            let _ = std::process::Command::new(install_dir().join("custom-ui-settings.exe")).spawn();
            let _ = DestroyWindow(hwnd);
        }
        (Target::Secondary, Stage::Options | Stage::Failed(_)) => {
            let _ = DestroyWindow(hwnd);
        }
        _ => return,
    }
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn paint(hwnd: HWND, window_dc: HDC) {
    let mut c = RECT::default();
    let _ = GetClientRect(hwnd, &mut c);
    let Some(mut canvas) = Canvas::new(c.right, c.bottom) else { return };
    let dpi = scale(hwnd);
    let px = |v: f32| (v * dpi).round() as i32;
    let dc = canvas.dc;
    let (stage, autostart, launch, hover, [normal, title, small]) = STATE.with_borrow(|s| (s.stage.clone(), s.autostart, s.launch, s.hover, s.fonts));
    let accent = gfx::accent();
    let mut targets = Vec::new();
    gfx::fill(dc, &c, COLOR_WINDOW);

    // Icon and title.
    let instance = GetModuleHandleW(None).unwrap_or_default();
    if let Ok(icon) = LoadImageW(instance, APP_ICON, IMAGE_ICON, px(ICON), px(ICON), LR_DEFAULTCOLOR) {
        let _ = DrawIconEx(dc, px(PAD), px(PAD), HICON(icon.0), px(ICON), px(ICON), 0, None, DI_NORMAL);
        let _ = DestroyIcon(HICON(icon.0));
    }
    let left = px(PAD + ICON + 22.0);
    let right = c.right - px(PAD);
    let old = SelectObject(dc, title);
    SetTextColor(dc, COLOR_TEXT);
    gfx::draw_text(dc, "Custom UI", &RECT { left, top: px(PAD), right, bottom: px(PAD + 34.0) }, DT_LEFT);
    SelectObject(dc, normal);
    SetTextColor(dc, COLOR_DIM);
    gfx::draw_text(dc, &format!("Версия {VERSION} · оболочка в стиле macOS для Windows 11"), &RECT { left, top: px(PAD + 36.0), right, bottom: px(PAD + 56.0) }, DT_LEFT | DT_END_ELLIPSIS);

    let mut y = px(PAD + 76.0);
    let body = |canvas: &mut Canvas, y: i32, text: &str, color: COLORREF| {
        let _ = canvas;
        SelectObject(dc, normal);
        SetTextColor(dc, color);
        gfx::draw_text_wrapped(dc, text, &RECT { left, top: y, right, bottom: y + px(44.0) });
    };
    match &stage {
        Stage::Options => {
            let text = if installed() { "Будет установлена новая версия поверх текущей. Настройки сохранятся." } else { "Строка меню, Dock, Spotlight, Launchpad и виджеты вместо панели задач Windows." };
            body(&mut canvas, y, text, COLOR_TEXT);
            y += px(52.0);
            for (target, label, on) in [(Target::Autostart, "Запускать при входе в Windows", autostart), (Target::Launch, "Запустить сразу после установки", launch)] {
                let row = RECT { left, top: y, right, bottom: y + px(ROW) };
                SelectObject(dc, normal);
                SetTextColor(dc, COLOR_TEXT);
                gfx::draw_text(dc, label, &RECT { right: right - px(TOGGLE_W + 12.0), ..row }, DT_LEFT);
                let mid = (row.top + row.bottom) / 2;
                let t = RECT { left: right - px(TOGGLE_W), top: mid - px(TOGGLE_H) / 2, right, bottom: mid - px(TOGGLE_H) / 2 + px(TOGGLE_H) };
                canvas.fill_shape(&t, TOGGLE_H * dpi / 2.0, if on { accent } else { COLOR_TOGGLE_OFF }, 255);
                let knob = px(TOGGLE_H - 4.0);
                let x = if on { t.right - px(2.0) - knob } else { t.left + px(2.0) };
                canvas.fill_shape(&RECT { left: x, top: t.top + px(2.0), right: x + knob, bottom: t.top + px(2.0) + knob }, knob as f32 / 2.0, rgb(255, 255, 255), 255);
                targets.push((row, target));
                y += px(ROW);
            }
            SelectObject(dc, small);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, &format!("Папка: {}", install_dir().display()), &RECT { left, top: y + px(6.0), right, bottom: y + px(24.0) }, DT_LEFT | DT_PATH_ELLIPSIS);
        }
        Stage::Working => body(&mut canvas, y, "Устанавливается…", COLOR_TEXT),
        Stage::Done => body(
            &mut canvas,
            y,
            if launch { "Готово! Оболочка запущена. Вернуться к обычному Windows можно через значок луны ▸ «Вернуться к Windows»." } else { "Готово! Custom UI есть в меню «Пуск»." },
            COLOR_TEXT,
        ),
        Stage::Failed(error) => body(&mut canvas, y, error, COLOR_ERROR),
    }

    // Buttons, bottom right.
    let (primary, secondary) = match &stage {
        Stage::Options => (Some(if installed() { "Обновить" } else { "Установить" }), Some("Отмена")),
        Stage::Working => (None, None),
        Stage::Done => (Some("Закрыть"), Some("Открыть настройки")),
        Stage::Failed(_) => (Some("Повторить"), Some("Закрыть")),
    };
    let bottom = c.bottom - px(PAD);
    let primary_rect = RECT { left: right - px(BUTTON_W), top: bottom - px(BUTTON_H), right, bottom };
    let secondary_rect = RECT { left: primary_rect.left - px(10.0 + BUTTON_W), right: primary_rect.left - px(10.0), ..primary_rect };
    SelectObject(dc, normal);
    SetTextColor(dc, COLOR_TEXT);
    for (label, rect, target, color) in [(primary, primary_rect, Target::Primary, accent), (secondary, secondary_rect, Target::Secondary, COLOR_CONTROL)] {
        let Some(label) = label else { continue };
        let color = if color == COLOR_CONTROL && hover == Some(target) { COLOR_CONTROL_HOVER } else { color };
        canvas.fill_shape(&rect, 6.0 * dpi, color, 255);
        gfx::draw_text(dc, label, &rect, DT_CENTER);
        targets.push((rect, target));
    }
    SelectObject(dc, old);
    let _ = BitBlt(window_dc, 0, 0, c.right, c.bottom, dc, 0, 0, SRCCOPY);
    STATE.with_borrow_mut(|s| s.targets = targets);
}
