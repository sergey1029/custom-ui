//! Settings shared by the shell and the settings app, in
//! %APPDATA%\CustomUI\config.ini as `key = value` lines. The table below is the
//! single list of what can be configured: the settings app builds its pages
//! from it, the shell reads values by key (unknown keys fall back to defaults).
//! The shell reads the file at startup; the settings app restarts it to apply.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub enum Kind {
    Bool,
    /// Inclusive range, step, unit suffix for display.
    Int { min: i32, max: i32, step: i32, unit: &'static str },
    /// RGB hex like "0062E1".
    Color,
    /// (value, label) pairs.
    Choice(&'static [(&'static str, &'static str)]),
    Text,
}

pub struct Setting {
    pub key: &'static str,
    pub section: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub kind: Kind,
    pub default: &'static str,
}

pub const SECTIONS: [(&str, &str); 6] = [
    ("general", "Основные"),
    ("appearance", "Оформление"),
    ("menubar", "Строка меню"),
    ("dock", "Dock"),
    ("desktop", "Рабочий стол"),
    ("search", "Поиск и Launchpad"),
];

const fn setting(key: &'static str, section: &'static str, label: &'static str, hint: &'static str, kind: Kind, default: &'static str) -> Setting {
    Setting { key, section, label, hint, kind, default }
}

const fn range(min: i32, max: i32, step: i32, unit: &'static str) -> Kind {
    Kind::Int { min, max, step, unit }
}

pub const SETTINGS: &[Setting] = &[
    // General
    setting("autostart", "general", "Запускать при входе в Windows", "Оболочка стартует сама после входа в систему", Kind::Bool, "1"),
    setting("taskbar.hide", "general", "Скрывать панель задач Windows", "Пока работает оболочка; возвращается при выходе", Kind::Bool, "1"),
    setting("winkey.spotlight", "general", "Клавиша Win открывает Spotlight", "Вместо меню «Пуск» Windows; сочетания Win+… работают как обычно", Kind::Bool, "1"),
    setting("hotkey.alt_space", "general", "Alt + Пробел открывает Spotlight", "", Kind::Bool, "1"),
    setting("tray.enabled", "general", "Значки трея в строке меню", "Значки программ (Discord, Steam…) под стрелкой справа", Kind::Bool, "1"),
    // Appearance
    setting("accent", "appearance", "Цвет акцента", "Подсветка в меню, выбранные элементы, включённые переключатели", Kind::Color, "0062E1"),
    setting("glass.darkness", "appearance", "Затемнение стекла", "Насколько тёмными выглядят строка меню, Dock и панели", range(20, 85, 5, "%"), "55"),
    setting("glass.saturation", "appearance", "Насыщенность стекла", "Яркость цветов, просвечивающих сквозь стекло", range(100, 200, 10, "%"), "150"),
    setting(
        "font",
        "appearance",
        "Шрифт интерфейса",
        "",
        Kind::Choice(&[("Segoe UI Variable Text", "Segoe UI Variable"), ("Segoe UI", "Segoe UI"), ("Arial", "Arial"), ("Bahnschrift", "Bahnschrift")]),
        "Segoe UI Variable Text",
    ),
    // Menu bar
    setting("menubar.height", "menubar", "Высота строки меню", "", range(24, 36, 1, " px"), "28"),
    setting("menubar.notch", "menubar", "Вырез посередине", "Место под пузырь Coucou, как «чёлка» MacBook", Kind::Bool, "1"),
    setting("menubar.notch_width", "menubar", "Ширина выреза", "", range(160, 420, 4, " px"), "304"),
    setting("menubar.app_menus", "menubar", "Меню приложения", "«Файл», «Правка», «Вид»… рядом с названием программы", Kind::Bool, "1"),
    setting("menubar.now_playing", "menubar", "Играющий трек", "Название трека справа от выреза; клик — пауза", Kind::Bool, "1"),
    setting("menubar.layout", "menubar", "Раскладка клавиатуры", "", Kind::Bool, "1"),
    setting("menubar.volume", "menubar", "Громкость", "", Kind::Bool, "1"),
    setting("menubar.network", "menubar", "Сеть", "", Kind::Bool, "1"),
    setting("menubar.battery", "menubar", "Аккумулятор", "Только на устройствах с батареей", Kind::Bool, "1"),
    setting("menubar.spotlight", "menubar", "Значок Spotlight", "", Kind::Bool, "1"),
    setting("menubar.control_center", "menubar", "Значок Пункта управления", "", Kind::Bool, "1"),
    setting("menubar.date", "menubar", "Дата рядом со временем", "", Kind::Bool, "1"),
    // Dock
    setting("dock.enabled", "dock", "Показывать Dock", "", Kind::Bool, "1"),
    setting("dock.icon_size", "dock", "Размер значков", "", range(32, 80, 2, " px"), "48"),
    setting("dock.magnify", "dock", "Увеличение под курсором", "", Kind::Bool, "1"),
    setting("dock.magnify_max", "dock", "Сила увеличения", "", range(110, 250, 10, "%"), "160"),
    setting("dock.indicators", "dock", "Точки под открытыми программами", "", Kind::Bool, "1"),
    setting("dock.trash", "dock", "Корзина в Dock", "", Kind::Bool, "1"),
    // Desktop
    setting("shelves.enabled", "desktop", "Полки рабочего стола", "Файлы со стола — в боковой панели по стрелке слева; значки Проводника скрыты", Kind::Bool, "1"),
    setting("widget.weather", "desktop", "Виджет погоды", "", Kind::Bool, "1"),
    setting("weather.city", "desktop", "Город для погоды", "Пусто — по IP или как уже сохранено", Kind::Text, ""),
    setting("widget.news", "desktop", "Виджет новостей", "", Kind::Bool, "1"),
    setting("news.feed", "desktop", "Лента новостей (RSS)", "Адрес любой RSS-ленты", Kind::Text, "https://lenta.ru/rss/news"),
    // Search & Launchpad
    setting("spotlight.files", "search", "Искать файлы в Spotlight", "По индексу Windows Search в папке пользователя", Kind::Bool, "1"),
    setting(
        "spotlight.web",
        "search",
        "Поиск в интернете",
        "Последняя строка в результатах Spotlight",
        Kind::Choice(&[("google", "Google"), ("yandex", "Яндекс"), ("bing", "Bing"), ("duckduckgo", "DuckDuckGo")]),
        "google",
    ),
    setting(
        "launchpad.style",
        "search",
        "Вид Launchpad",
        "",
        Kind::Choice(&[("grid", "Сетка как в macOS"), ("honeycomb", "Соты как в Apple Watch")]),
        "grid",
    ),
];

pub fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

static VALUES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub fn file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("CustomUI").join("config.ini"))
}

fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// (Re)reads the file.
pub fn load() {
    let values = file().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| parse(&t)).unwrap_or_default();
    *VALUES.lock().unwrap() = Some(values);
}

/// Writes every setting (defaults included, so the file documents itself).
pub fn save() {
    let Some(path) = file() else { return };
    let values = VALUES.lock().unwrap().clone().unwrap_or_default();
    let mut text = String::from("# Custom UI — настройки. Меняются в программе «Настройки Custom UI».\n");
    for s in SETTINGS {
        let value = values.get(s.key).map(String::as_str).unwrap_or(s.default);
        text += &format!("{} = {}\n", s.key, value);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

/// The value as stored, or the setting's default.
pub fn text(key: &str) -> String {
    let mut guard = VALUES.lock().unwrap();
    if guard.is_none() {
        drop(guard);
        load();
        guard = VALUES.lock().unwrap();
    }
    let default = find(key).map(|s| s.default);
    debug_assert!(default.is_some(), "unknown setting {key}");
    guard.as_ref().and_then(|v| v.get(key).cloned()).unwrap_or_else(|| default.unwrap_or("").to_owned())
}

pub fn flag(key: &str) -> bool {
    matches!(text(key).as_str(), "1" | "true" | "yes" | "on")
}

/// Clamped to the setting's range.
pub fn int(key: &str) -> i32 {
    let value = text(key).parse::<i32>().ok();
    match find(key).map(|s| &s.kind) {
        Some(Kind::Int { min, max, .. }) => value.unwrap_or_else(|| find(key).and_then(|s| s.default.parse().ok()).unwrap_or(*min)).clamp(*min, *max),
        _ => value.unwrap_or(0),
    }
}

/// (r, g, b).
pub fn color(key: &str) -> (u8, u8, u8) {
    parse_color(&text(key)).or_else(|| find(key).and_then(|s| parse_color(s.default))).unwrap_or((0, 98, 225))
}

pub fn parse_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

pub fn set(key: &str, value: impl Into<String>) {
    let mut guard = VALUES.lock().unwrap();
    guard.get_or_insert_with(HashMap::new).insert(key.to_owned(), value.into());
}

pub fn set_flag(key: &str, on: bool) {
    set(key, if on { "1" } else { "0" });
}

/// Changes one flag in the file right away (for toggles inside the shell),
/// keeping whatever else the file says now.
pub fn store(key: &str, value: &str) {
    load();
    set(key, value);
    save();
}

pub fn store_flag(key: &str, on: bool) {
    store(key, if on { "1" } else { "0" });
}

const RUN_KEY: windows::core::PCWSTR = windows::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: windows::core::PCWSTR = windows::core::w!("CustomUI");

/// Makes Windows start `shell` at sign-in, or not, as the "autostart" setting says.
pub fn apply_autostart(shell: &std::path::Path) {
    use windows::Win32::System::Registry::*;
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyW(HKEY_CURRENT_USER, RUN_KEY, &mut key).is_err() {
            return;
        }
        if flag("autostart") {
            let command: Vec<u16> = format!("\"{}\"", shell.display()).encode_utf16().chain([0]).collect();
            let bytes = std::slice::from_raw_parts(command.as_ptr() as *const u8, command.len() * 2);
            let _ = RegSetValueExW(key, RUN_VALUE, 0, REG_SZ, Some(bytes));
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

/// Resets one setting to its default.
pub fn reset(key: &str) {
    if let Some(values) = VALUES.lock().unwrap().as_mut() {
        values.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        for s in SETTINGS {
            assert!(SECTIONS.iter().any(|(id, _)| *id == s.section), "{} has unknown section", s.key);
            match &s.kind {
                Kind::Bool => assert!(s.default == "0" || s.default == "1", "{}", s.key),
                Kind::Int { min, max, .. } => {
                    let v: i32 = s.default.parse().unwrap();
                    assert!(v >= *min && v <= *max, "{}", s.key);
                }
                Kind::Color => assert!(parse_color(s.default).is_some(), "{}", s.key),
                Kind::Choice(options) => assert!(options.iter().any(|(v, _)| *v == s.default), "{}", s.key),
                Kind::Text => {}
            }
        }
        let mut keys: Vec<_> = SETTINGS.iter().map(|s| s.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), SETTINGS.len(), "duplicate keys");
    }

    #[test]
    fn parsing() {
        let values = parse("# comment\na = 1\nurl = https://x.ru/?q=a=b\n bad line\n");
        assert_eq!(values.get("a").map(String::as_str), Some("1"));
        assert_eq!(values.get("url").map(String::as_str), Some("https://x.ru/?q=a=b"));
        assert_eq!(parse_color("#0062E1"), Some((0, 98, 225)));
        assert_eq!(parse_color("xyz"), None);
    }
}
