//! The menus on the left of the menu bar, as on a Mac:
//! - the system menu (where the Apple menu sits): about, settings, store, force
//!   quit, sleep / restart / shut down, lock, log out, and the shell's options;
//! - the app menu under the front app's bold name: hide, hide others, quit;
//! - the app's own menus. Classic Win32 programs have a real menu bar, which is
//!   read and shown here (choosing an item sends it the same WM_COMMAND its own
//!   menu would). Other programs get a standard File / Edit / View / Window /
//!   Help set that works through the usual keyboard shortcuts.

use windows::core::PWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::{ExitWindowsEx, LockWorkStation, EWX_LOGOFF, SHUTDOWN_REASON};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::desktop::{self, Widget};
use crate::macmenu::{split_label, Anchor, Menu};
use crate::{apps, shelves, tasks};

/// What a menu bar title opens.
#[derive(Clone, PartialEq)]
pub enum Source {
    System,
    App,
    /// Position of a top-level item in the window's own menu bar.
    Native(u32),
    Generic(usize),
}

pub struct Title {
    pub label: String,
    pub source: Source,
}

const GENERIC: [&str; 5] = ["Файл", "Правка", "Вид", "Окно", "Справка"];

/// The titles after the app name for `window`: its own menus if it has a
/// classic menu bar, the standard set otherwise.
pub fn titles(window: HWND) -> Vec<Title> {
    let native = unsafe { native_titles(window) };
    if !native.is_empty() {
        return native;
    }
    GENERIC.iter().enumerate().map(|(i, label)| Title { label: (*label).to_owned(), source: Source::Generic(i) }).collect()
}

unsafe fn native_titles(window: HWND) -> Vec<Title> {
    if window.is_invalid() {
        return Vec::new();
    }
    let menu = GetMenu(window);
    if menu.is_invalid() || !IsMenu(menu).as_bool() {
        return Vec::new();
    }
    (0..GetMenuItemCount(menu).max(0) as u32)
        .filter_map(|i| {
            let (label, _, _, _, submenu) = item_info(menu, i)?;
            // Only real drop-down menus; bare commands on a menu bar are rare and odd.
            (!submenu.is_invalid() && !label.is_empty()).then(|| Title { label: split_label(&label).0, source: Source::Native(i) })
        })
        .collect()
}

/// Text, id, enabled, checked, submenu of a menu item (`None` for separators).
unsafe fn item_info(menu: HMENU, position: u32) -> Option<(String, u32, bool, bool, HMENU)> {
    let mut buf = [0u16; 256];
    let mut info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_ID | MIIM_STATE | MIIM_SUBMENU | MIIM_FTYPE,
        dwTypeData: PWSTR(buf.as_mut_ptr()),
        cch: buf.len() as u32 - 1,
        ..Default::default()
    };
    GetMenuItemInfoW(menu, position, true, &mut info).ok()?;
    if info.fType.0 & MFT_SEPARATOR.0 != 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..info.cch.min(255) as usize]);
    let enabled = info.fState.0 & (MFS_DISABLED.0 | MFS_GRAYED.0) == 0;
    let checked = info.fState.0 & MFS_CHECKED.0 != 0;
    Some((text, info.wID, enabled, checked, info.hSubMenu))
}

/// Builds our menu from a native one, numbering items from `ids.len() + 1` and
/// remembering their native ids. Lets the program refresh the states first, as
/// it would right before showing its own menu.
unsafe fn convert(owner: HWND, menu: HMENU, position: u32, ids: &mut Vec<u32>, depth: u32) -> Menu {
    let mut result = 0usize;
    let _ = SendMessageTimeoutW(owner, WM_INITMENUPOPUP, WPARAM(menu.0 as usize), LPARAM(position as isize), SMTO_ABORTIFHUNG, 300, Some(&mut result));
    let mut out = Menu::new();
    for i in 0..GetMenuItemCount(menu).max(0) as u32 {
        match item_info(menu, i) {
            None => {
                out.separator();
            }
            Some((label, id, enabled, checked, submenu)) => {
                let (text, shortcut) = split_label(&label);
                if !submenu.is_invalid() {
                    if depth < 3 {
                        out.submenu(text, convert(owner, submenu, i, ids, depth + 1));
                    }
                } else if !text.is_empty() {
                    ids.push(id);
                    out.item_full(ids.len(), text, &shortcut, enabled, checked);
                }
            }
        }
    }
    out
}

// ---- the standard menus ----

#[derive(Clone, Copy)]
enum Action {
    Keys(VIRTUAL_KEY, bool),
    Minimize,
    Zoom,
    Close,
    MinimizeOthers,
}

/// (label, shortcut shown, action) per standard menu; `None` is a separator.
fn generic(index: usize) -> Vec<Option<(&'static str, &'static str, Action)>> {
    let ctrl = |key: u8| Action::Keys(VIRTUAL_KEY(key as u16), true);
    let key = |vk: VIRTUAL_KEY| Action::Keys(vk, false);
    match index {
        0 => vec![
            Some(("Новое окно", "Ctrl+N", ctrl(b'N'))),
            Some(("Открыть…", "Ctrl+O", ctrl(b'O'))),
            None,
            Some(("Сохранить", "Ctrl+S", ctrl(b'S'))),
            Some(("Печать…", "Ctrl+P", ctrl(b'P'))),
            None,
            Some(("Закрыть окно", "Ctrl+W", ctrl(b'W'))),
        ],
        1 => vec![
            Some(("Отменить", "Ctrl+Z", ctrl(b'Z'))),
            Some(("Повторить", "Ctrl+Y", ctrl(b'Y'))),
            None,
            Some(("Вырезать", "Ctrl+X", ctrl(b'X'))),
            Some(("Скопировать", "Ctrl+C", ctrl(b'C'))),
            Some(("Вставить", "Ctrl+V", ctrl(b'V'))),
            Some(("Выбрать все", "Ctrl+A", ctrl(b'A'))),
            None,
            Some(("Найти…", "Ctrl+F", ctrl(b'F'))),
        ],
        2 => vec![
            Some(("Обновить", "F5", key(VK_F5))),
            None,
            Some(("Увеличить", "Ctrl++", Action::Keys(VK_OEM_PLUS, true))),
            Some(("Уменьшить", "Ctrl+-", Action::Keys(VK_OEM_MINUS, true))),
            Some(("Фактический размер", "Ctrl+0", ctrl(b'0'))),
            None,
            Some(("Во весь экран", "F11", key(VK_F11))),
        ],
        3 => vec![
            Some(("Свернуть", "", Action::Minimize)),
            Some(("Масштаб", "", Action::Zoom)),
            None,
            Some(("Скрыть остальные", "", Action::MinimizeOthers)),
            None,
            Some(("Закрыть", "Alt+F4", Action::Close)),
        ],
        _ => vec![Some(("Справка", "F1", key(VK_F1)))],
    }
}

/// Presses `key` (with Ctrl) in the app in front: our menus never take the focus.
unsafe fn press(key: VIRTUAL_KEY, ctrl: bool) {
    let event = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, time: 0, dwExtraInfo: 0 },
        },
    };
    let mut inputs = Vec::new();
    if ctrl {
        inputs.push(event(VK_CONTROL, false));
    }
    inputs.push(event(key, false));
    inputs.push(event(key, true));
    if ctrl {
        inputs.push(event(VK_CONTROL, true));
    }
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

unsafe fn perform(window: HWND, action: Action) {
    match action {
        Action::Keys(key, ctrl) => {
            let _ = SetForegroundWindow(window);
            press(key, ctrl);
        }
        Action::Minimize => {
            let _ = ShowWindow(window, SW_MINIMIZE);
        }
        Action::Zoom => {
            let _ = ShowWindow(window, if IsZoomed(window).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
        }
        Action::Close => {
            let _ = PostMessageW(window, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        Action::MinimizeOthers => {
            for task in tasks::collect().into_iter().filter(|t| t.hwnd != window) {
                let _ = ShowWindow(task.hwnd, SW_MINIMIZE);
            }
        }
    }
}

// ---- the system menu ----

const SYS_ABOUT: usize = 1;
const SYS_SETTINGS: usize = 2;
const SYS_STORE: usize = 3;
const SYS_FORCE_QUIT: usize = 4;
const SYS_SLEEP: usize = 5;
const SYS_RESTART: usize = 6;
const SYS_SHUTDOWN: usize = 7;
const SYS_LOCK: usize = 8;
const SYS_LOGOUT: usize = 9;
const SYS_SHELVES: usize = 10;
const SYS_WEATHER: usize = 11;
const SYS_NEWS: usize = 12;
const SYS_QUIT_SHELL: usize = 13;
const SYS_SHELL_SETTINGS: usize = 14;

/// Asks before anything that ends the session.
unsafe fn confirm(question: &str, title: &str) -> bool {
    let (question, title) = (windows::core::HSTRING::from(question), windows::core::HSTRING::from(title));
    MessageBoxW(None, &question, &title, MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND) == IDYES
}

/// Returns true when the shell itself should quit.
unsafe fn system_menu(anchor: RECT) -> bool {
    let mut shell = Menu::new();
    shell
        .check(SYS_SHELVES, "Полки рабочего стола", shelves::is_enabled())
        .check(SYS_WEATHER, "Виджет погоды", desktop::has_widget(Widget::Weather))
        .check(SYS_NEWS, "Виджет новостей", desktop::has_widget(Widget::News))
        .separator()
        .item(SYS_SHELL_SETTINGS, "Настройки Custom UI…");
    let mut menu = Menu::new();
    menu.item(SYS_ABOUT, "Об этом компьютере")
        .separator()
        .item(SYS_SETTINGS, "Системные настройки…")
        .item(SYS_STORE, "Магазин приложений…")
        .separator()
        .item_full(SYS_FORCE_QUIT, "Завершить принудительно…", "Ctrl+Shift+Esc", true, false)
        .separator()
        .item(SYS_SLEEP, "Режим сна")
        .item(SYS_RESTART, "Перезагрузить…")
        .item(SYS_SHUTDOWN, "Выключить…")
        .separator()
        .item_full(SYS_LOCK, "Заблокировать экран", "Win+L", true, false)
        .item(SYS_LOGOUT, "Завершить сеанс…")
        .separator()
        .submenu("Оболочка", shell)
        .item(SYS_QUIT_SHELL, "Вернуться к Windows");
    match menu.show(Anchor::Below(anchor)) {
        SYS_ABOUT => apps::open("ms-settings:about"),
        SYS_SETTINGS => apps::open("ms-settings:"),
        SYS_STORE => apps::open("ms-windows-store:"),
        SYS_FORCE_QUIT => apps::open("taskmgr.exe"),
        SYS_SLEEP => {
            let _ = SetSuspendState(false, false, false);
        }
        SYS_RESTART if confirm("Перезагрузить компьютер сейчас?", "Перезагрузка") => apps::run("shutdown.exe", "/r /t 0"),
        SYS_SHUTDOWN if confirm("Выключить компьютер сейчас?", "Выключение") => apps::run("shutdown.exe", "/s /t 0"),
        SYS_LOCK => {
            let _ = LockWorkStation();
        }
        SYS_LOGOUT if confirm("Завершить сеанс? Несохранённые данные в программах будут потеряны.", "Завершение сеанса") => {
            let _ = ExitWindowsEx(EWX_LOGOFF, SHUTDOWN_REASON(0));
        }
        SYS_SHELVES => shelves::set_enabled(!shelves::is_enabled()),
        SYS_WEATHER => desktop::toggle_widget(Widget::Weather),
        SYS_NEWS => desktop::toggle_widget(Widget::News),
        SYS_SHELL_SETTINGS => open_settings(),
        // Exits the shell: Explorer's taskbar and desktop icons come back.
        SYS_QUIT_SHELL => return true,
        _ => {}
    }
    false
}

/// The settings app lives next to the shell's exe.
pub fn open_settings() {
    if let Some(exe) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("custom-ui-settings.exe"))) {
        apps::open(&exe.to_string_lossy());
    }
}

/// Opens the menu for a title hanging under `anchor` (screen coordinates), for
/// the app whose window is `window` and whose name is `app`. Returns true when
/// the user chose to quit the shell.
pub fn open(source: &Source, anchor: RECT, window: HWND, app: &str) -> bool {
    unsafe {
        match source {
            Source::System => return system_menu(anchor),
            Source::App => {
                let mut menu = Menu::new();
                menu.item(1, format!("Скрыть «{app}»"))
                    .item(2, "Скрыть остальные")
                    .separator()
                    .item_full(3, format!("Завершить «{app}»"), "Alt+F4", !window.is_invalid(), false);
                match menu.show(Anchor::Below(anchor)) {
                    1 => perform(window, Action::Minimize),
                    2 => perform(window, Action::MinimizeOthers),
                    3 => perform(window, Action::Close),
                    _ => {}
                }
            }
            Source::Native(position) => {
                let bar = GetMenu(window);
                if bar.is_invalid() {
                    return false;
                }
                let _ = SendMessageTimeoutW(window, WM_INITMENU, WPARAM(bar.0 as usize), LPARAM(0), SMTO_ABORTIFHUNG, 300, None);
                let Some((_, _, _, _, submenu)) = item_info(bar, *position) else { return false };
                let mut ids = Vec::new();
                let menu = convert(window, submenu, *position, &mut ids, 0);
                let chosen = menu.show(Anchor::Below(anchor));
                if let Some(&id) = chosen.checked_sub(1).and_then(|i| ids.get(i)) {
                    let _ = PostMessageW(window, WM_COMMAND, WPARAM(id as usize & 0xFFFF), LPARAM(0));
                }
            }
            Source::Generic(index) => {
                let entries = generic(*index);
                let mut menu = Menu::new();
                for (i, entry) in entries.iter().enumerate() {
                    match entry {
                        Some((label, shortcut, _)) => {
                            menu.item_full(i + 1, *label, shortcut, !window.is_invalid(), false);
                        }
                        None => {
                            menu.separator();
                        }
                    }
                }
                let chosen = menu.show(Anchor::Below(anchor));
                if let Some(Some((_, _, action))) = chosen.checked_sub(1).and_then(|i| entries.get(i)) {
                    perform(window, *action);
                }
            }
        }
    }
    false
}
