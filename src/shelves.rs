//! Smart shelves: the desktop's files, folders and shortcuts, sorted by kind
//! (games, apps, folders, documents, images, ...). They live in a panel that
//! slides in from the left edge when its small side tab is clicked, so the
//! desktop itself stays clean (Explorer's own icons are hidden meanwhile and
//! come back on exit or crash). Games are recognised from their launchers'
//! links and install paths. Shelves roll up to their header; items open on
//! double-click and can be dragged onto another shelf, which is remembered.
//! State lives in %APPDATA%\CustomUI\shelves.tsv.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::macmenu::{Anchor, Menu};
use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use crate::{apps, config, desktop, icons};

const GAMES: usize = 0;
const APPS: usize = 1;
const FOLDERS: usize = 2;
const DOCUMENTS: usize = 3;
const IMAGES: usize = 4;
const MEDIA: usize = 5;
const ARCHIVES: usize = 6;
const LINKS: usize = 7;
const OTHER: usize = 8;
const CATEGORIES: [(&str, &str); 9] = [
    ("Игры", "\u{E7FC}"),
    ("Приложения", "\u{E71D}"),
    ("Папки", "\u{E8B7}"),
    ("Документы", "\u{E8A5}"),
    ("Изображения", "\u{EB9F}"),
    ("Медиа", "\u{E8D6}"),
    ("Архивы", "\u{E7B8}"),
    ("Ссылки", "\u{E71B}"),
    ("Прочее", "\u{E712}"),
];

// A shelf (card) inside the panel.
const COLUMNS: usize = 4;
const CELL_W: f32 = 86.0;
const CELL_H: f32 = 92.0;
const SIDE_PAD: f32 = 12.0;
const HEADER_H: f32 = 40.0;
const BOTTOM_PAD: f32 = 8.0;
const ICON: f32 = 40.0;
const CARD_RADIUS: f32 = 16.0;
const CELL_RADIUS: f32 = 12.0;
const CARD_GAP: f32 = 10.0;

// The panel.
const PANEL_PAD: f32 = 12.0;
const TITLE_H: f32 = 50.0;
const PANEL_MARGIN: f32 = 8.0;
const PANEL_RADIUS: f32 = 22.0;
const PANEL_ALPHA: u32 = 238;
const SCROLL_STEP: f32 = CELL_H;
const SLIDE: Duration = Duration::from_millis(170);

// The side tab.
const TAB_W: f32 = 22.0;
const TAB_H: f32 = 86.0;
const TAB_IDLE: f32 = 7.0;
const TAB_HOVER: f32 = 20.0;

const DRAG_THRESHOLD: i32 = 4;
const TIMER_RESCAN: usize = 1;
const TIMER_SLIDE: usize = 2;
const RESCAN_MS: u32 = 2000;

const COLOR_PANEL: COLORREF = rgb(26, 26, 30);
const COLOR_CARD: COLORREF = rgb(38, 38, 44);
const COLOR_CARD_DROP: COLORREF = rgb(44, 58, 92);
const COLOR_HOVER: COLORREF = rgb(56, 56, 66);
const COLOR_SELECTED: COLORREF = rgb(44, 82, 170);
const COLOR_DIVIDER: COLORREF = rgb(62, 62, 72);
const COLOR_SCROLLBAR: COLORREF = rgb(100, 100, 112);
const COLOR_TAB: COLORREF = rgb(210, 210, 220);
const COLOR_TAB_IDLE: COLORREF = rgb(130, 130, 142);

const CMD_OPEN: usize = 1;
const CMD_REVEAL: usize = 2;
const CMD_PROPERTIES: usize = 3;
const CMD_DELETE: usize = 4;
const CMD_AUTO: usize = 5;
/// "Переместить на полку ▸" entries: CMD_MOVE + category.
const CMD_MOVE: usize = 100;

/// Set while Explorer's desktop icons are hidden by us, so the crash handler knows to bring them back.
static ICONS_HIDDEN: AtomicBool = AtomicBool::new(false);

struct Entry {
    path: String,
    name: String,
    category: usize,
}

struct Shelf {
    category: usize,
    /// Indices into `State::entries`, sorted by name.
    entries: Vec<usize>,
}

/// What a point in the panel is over.
#[derive(Clone, Copy, PartialEq)]
enum Spot {
    Close,
    Header(usize),
    Item(usize),
    /// Inside a shelf's card but not on an item (a drop target).
    Card(usize),
}

struct Press {
    start: POINT,
    item: usize,
    dragging: bool,
}

#[derive(Default)]
struct State {
    enabled: bool,
    controller: HWND,
    tab: HWND,
    panel: HWND,
    entries: Vec<Entry>,
    shelves: Vec<Shelf>,
    collapsed: HashMap<usize, bool>,
    /// Manual moves: lowercase path → category.
    assignments: HashMap<String, usize>,
    /// Classification is slow-ish (shortcut targets), so it's cached by path.
    classified: HashMap<String, usize>,
    /// The desktop listing last seen, to notice changes.
    listing: Vec<String>,
    open: bool,
    /// Slide animation: when it started and whether it's opening.
    slide: Option<(Instant, bool)>,
    scroll: f32,
    selected: Option<usize>,
    hover: Option<Spot>,
    drop_target: Option<usize>,
    press: Option<Press>,
    last_click: Option<(usize, Instant)>,
    tracking: bool,
    tab_hover: bool,
    tab_hidden: bool,
    previous: HWND,
    font: HFONT,
    bold: HFONT,
    title: HFONT,
    glyphs: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

fn px(v: f32) -> i32 {
    (v * scale()).round() as i32
}

pub fn create() -> Result<()> {
    unsafe {
        let controller = gfx::create_window(w!("CustomUI.Shelves"), Some(controller_proc), WS_EX_TOOLWINDOW)?;
        let tab = gfx::create_window(w!("CustomUI.ShelfTab"), Some(tab_proc), WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED)?;
        let panel = gfx::create_window(w!("CustomUI.ShelfPanel"), Some(panel_proc), WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED)?;
        let (_, collapsed, assignments) = load();
        let enabled = config::flag("shelves.enabled");
        STATE.with_borrow_mut(|s| {
            s.controller = controller;
            s.tab = tab;
            s.panel = panel;
            s.enabled = enabled;
            s.collapsed = collapsed;
            s.assignments = assignments;
            s.font = gfx::create_font(-px(12.0), FW_NORMAL, gfx::ui_font());
            s.bold = gfx::create_font(-px(13.0), FW_SEMIBOLD, gfx::ui_font());
            s.title = gfx::create_font(-px(15.0), FW_SEMIBOLD, gfx::ui_font());
            s.glyphs = gfx::create_font(-px(14.0), FW_NORMAL, w!("Segoe Fluent Icons"));
        });
        SetTimer(controller, TIMER_RESCAN, RESCAN_MS, None);
        if enabled {
            activate();
        }
    }
    Ok(())
}

pub fn destroy() {
    let windows = STATE.with_borrow(|s| [s.panel, s.tab, s.controller]);
    unsafe {
        for hwnd in windows.into_iter().filter(|h| !h.is_invalid()) {
            let _ = DestroyWindow(hwnd);
        }
    }
    restore_explorer_icons();
}

pub fn is_enabled() -> bool {
    STATE.with_borrow(|s| s.enabled)
}

pub fn set_enabled(enabled: bool) {
    STATE.with_borrow_mut(|s| s.enabled = enabled);
    config::store_flag("shelves.enabled", enabled);
    if enabled {
        activate();
    } else {
        close_panel(false);
        let tab = STATE.with_borrow_mut(|s| {
            s.listing.clear();
            s.tab
        });
        unsafe {
            let _ = ShowWindow(tab, SW_HIDE);
        }
        restore_explorer_icons();
    }
    STATE.with_borrow(save);
}

fn activate() {
    hide_explorer_icons();
    rescan(true);
    unsafe { render_tab() };
}

// ---- Explorer's icons ----

/// The desktop's SHELLDLL_DefView and its icon list view.
unsafe fn explorer_icons() -> Option<(HWND, HWND)> {
    let defview = FindWindowExW(desktop::desktop_host(), None, w!("SHELLDLL_DefView"), None).ok()?;
    let list = FindWindowExW(defview, None, w!("SysListView32"), None).ok()?;
    Some((defview, list))
}

/// Explorer's own "Вид ▸ Отображать значки рабочего стола" command.
const TOGGLE_DESKTOP_ICONS: usize = 0x7402;

fn hide_explorer_icons() {
    unsafe {
        if let Some((defview, list)) = explorer_icons() {
            if IsWindowVisible(list).as_bool() {
                let _ = PostMessageW(defview, WM_COMMAND, WPARAM(TOGGLE_DESKTOP_ICONS), LPARAM(0));
                ICONS_HIDDEN.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Safe from the crash handler; does nothing unless we hid them.
pub fn restore_explorer_icons() {
    if !ICONS_HIDDEN.swap(false, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if let Some((defview, list)) = explorer_icons() {
            if !IsWindowVisible(list).as_bool() {
                let _ = PostMessageW(defview, WM_COMMAND, WPARAM(TOGGLE_DESKTOP_ICONS), LPARAM(0));
            }
        }
    }
}

// ---- the desktop's contents ----

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

/// Visible items on the user's and the public desktop.
fn list_desktop() -> Vec<(String, bool)> {
    let mut items = Vec::new();
    for folder in [known_folder(&FOLDERID_Desktop), known_folder(&FOLDERID_PublicDesktop)].into_iter().flatten() {
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
            if meta.file_attributes() & HIDDEN_OR_SYSTEM != 0 {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name == "desktop.ini" || name.starts_with("~$") {
                continue;
            }
            items.push((entry.path().to_string_lossy().into_owned(), meta.is_dir()));
        }
    }
    items.sort();
    items
}

/// Stores, launchers, mod managers and emulators, matched anywhere in the path.
const GAME_MARKERS: [&str; 37] = [
    "\\steamapps\\", "\\steam\\", "epic games", "riot games", "battle.net", "ubisoft", "gog galaxy", "xboxgames",
    "\\ea games\\", "\\ea\\", "origin games", "\\games\\", "minecraft", "roblox", "osu!", "\\lutris\\",
    "prismlauncher", "multimc", "tlauncher", "curseforge", "overwolf", "thunderstore", "playnite", "heroic", "\\itch\\",
    "rpcs3", "pcsx2", "ppsspp", "yuzu", "ryujinx", "dolphin", "retroarch", "cemu", "duckstation", "xenia", "citra",
    "\\game\\",
];
const GAME_URLS: [&str; 7] = ["steam://", "com.epicgames.launcher://", "uplay://", "battlenet://", "origin://", "riotclient://", "roblox"];

fn extension(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn looks_like_game(path: &str) -> bool {
    let path = path.to_lowercase();
    // Executables named like games, e.g. "BetGame.exe" or Unreal's "X-Win64-Shipping.exe".
    let exe = Path::new(&path).file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    GAME_MARKERS.iter().any(|m| path.contains(m)) || exe.contains("game") || exe.ends_with("-shipping")
}

/// Category for a plain (non-shortcut) path.
fn classify_file(path: &str, is_dir: bool) -> usize {
    if is_dir {
        return FOLDERS;
    }
    match extension(path).as_str() {
        "exe" | "msi" | "bat" | "cmd" | "appref-ms" => {
            if looks_like_game(path) {
                GAMES
            } else {
                APPS
            }
        }
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "heic" | "svg" | "ico" | "tif" | "tiff" | "psd" => IMAGES,
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" => MEDIA,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt" | "ods" | "md" | "csv" | "json" | "xml" | "log" => DOCUMENTS,
        "zip" | "rar" | "7z" | "tar" | "gz" | "iso" | "cab" => ARCHIVES,
        _ => OTHER,
    }
}

/// Category for a desktop item. Shortcuts are judged by what they point to;
/// `url` is the URL= of an internet shortcut.
fn classify(path: &str, is_dir: bool, link_target: Option<&str>, url: Option<&str>) -> usize {
    if is_dir {
        return FOLDERS;
    }
    match extension(path).as_str() {
        "url" => match url.map(str::to_lowercase) {
            Some(u) if GAME_URLS.iter().any(|g| u.starts_with(g)) => GAMES,
            _ => LINKS,
        },
        "lnk" => match link_target {
            Some(target) if Path::new(target).is_dir() => FOLDERS,
            Some(target) => match classify_file(target, false) {
                OTHER => APPS,
                category => category,
            },
            // Shortcuts to Store apps and shell objects have no file target.
            None => APPS,
        },
        _ => classify_file(path, false),
    }
}

fn url_of(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| line.strip_prefix("URL=").map(|u| u.trim().to_owned()))
}

/// Explorer hides the extension of shortcuts; so do we.
fn display_name(path: &str) -> String {
    let p = Path::new(path);
    match extension(path).as_str() {
        "lnk" | "url" => p.file_stem(),
        _ => p.file_name(),
    }
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default()
}


/// Re-reads the desktop if it changed (or `force`) and re-sorts it.
fn rescan(force: bool) {
    if !STATE.with_borrow(|s| s.enabled) {
        return;
    }
    let listing = list_desktop();
    let paths: Vec<String> = listing.iter().map(|(p, _)| p.clone()).collect();
    if !force && STATE.with_borrow(|s| s.listing == paths) {
        return;
    }

    let cached = STATE.with_borrow(|s| s.classified.clone());
    let mut classified = HashMap::new();
    let entries: Vec<Entry> = listing
        .iter()
        .map(|(path, is_dir)| {
            let category = cached.get(path).copied().unwrap_or_else(|| {
                let target = (extension(path) == "lnk").then(|| apps::link_target(path)).flatten();
                let url = (extension(path) == "url").then(|| url_of(path)).flatten();
                classify(path, *is_dir, target.as_deref(), url.as_deref())
            });
            classified.insert(path.clone(), category);
            Entry { path: path.clone(), name: display_name(path), category }
        })
        .collect();

    STATE.with_borrow_mut(|s| {
        // Group into shelves (in category order), applying manual moves.
        let mut groups: Vec<Vec<usize>> = vec![Vec::new(); CATEGORIES.len()];
        for (i, entry) in entries.iter().enumerate() {
            let category = s.assignments.get(&entry.path.to_lowercase()).copied().unwrap_or(entry.category);
            groups[category].push(i);
        }
        for group in &mut groups {
            group.sort_by_cached_key(|&i| entries[i].name.to_lowercase());
        }
        s.shelves = groups
            .into_iter()
            .enumerate()
            .filter(|(_, g)| !g.is_empty())
            .map(|(category, entries)| Shelf { category, entries })
            .collect();
        s.entries = entries;
        s.classified = classified;
        s.listing = paths;
        s.selected = None;
        s.press = None;
        s.hover = None;
    });
    if STATE.with_borrow(|s| s.open) {
        unsafe { render_panel() };
    }
}

fn work_area() -> RECT {
    let mut work = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    work
}

// ---- the side tab ----

/// A slim pill on the left edge, centered vertically; wider with a chevron on hover.
unsafe fn render_tab() {
    let (tab, hover, hidden, enabled) = STATE.with_borrow(|s| (s.tab, s.tab_hover, s.tab_hidden, s.enabled));
    if !enabled || hidden {
        let _ = ShowWindow(tab, SW_HIDE);
        return;
    }
    let (width, height) = (px(TAB_W), px(TAB_H));
    let Some(mut canvas) = Canvas::new(width, height) else { return };
    let dc = canvas.dc;
    let visible = px(if hover { TAB_HOVER } else { TAB_IDLE });
    gfx::fill(dc, &RECT { left: 0, top: 0, right: visible, bottom: height }, COLOR_PANEL);
    if hover {
        STATE.with_borrow(|s| SelectObject(dc, s.glyphs));
        SetTextColor(dc, COLOR_TAB);
        gfx::draw_text(dc, "\u{E76C}", &RECT { left: 0, top: 0, right: visible, bottom: height }, DT_CENTER);
    } else {
        let mark = RECT { left: visible / 2 - 1, top: height / 2 - px(14.0), right: visible / 2 + 1, bottom: height / 2 + px(14.0) };
        gfx::fill(dc, &mark, COLOR_TAB_IDLE);
    }
    // Half of a pill sticking out of the screen edge: only the right side shows its rounding.
    let pill = RECT { left: -visible, top: 0, right: visible, bottom: height };
    canvas.shape_alpha(COLOR_PANEL, if hover { 235 } else { 150 }, &[(pill, visible as f32)]);
    let work = work_area();
    let y = work.top + (work.bottom - work.top - height) / 2;
    canvas.present(tab, work.left, y, 255);
    if !IsWindowVisible(tab).as_bool() {
        let _ = ShowWindow(tab, SW_SHOWNOACTIVATE);
    }
}

unsafe extern "system" fn tab_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            if !STATE.with_borrow(|s| s.tab_hover) {
                STATE.with_borrow_mut(|s| s.tab_hover = true);
                let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                let _ = TrackMouseEvent(&mut tme);
                render_tab();
            }
        }
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| s.tab_hover = false);
            render_tab();
        }
        WM_LBUTTONUP => toggle_panel(),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

/// No tab over fullscreen games, videos and presentations.
fn update_tab_visibility() {
    let busy = unsafe {
        matches!(SHQueryUserNotificationState(), Ok(state) if state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE)
    };
    if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.tab_hidden, busy) != busy) {
        unsafe { render_tab() };
    }
}

// ---- the panel ----

pub fn toggle_panel() {
    if STATE.with_borrow(|s| s.open) {
        close_panel(true);
    } else {
        open_panel();
    }
}

fn open_panel() {
    if !STATE.with_borrow(|s| s.enabled) {
        return;
    }
    rescan(false);
    let panel = STATE.with_borrow_mut(|s| {
        s.open = true;
        s.slide = Some((Instant::now(), true));
        s.hover = None;
        s.panel
    });
    unsafe {
        render_panel();
        let _ = ShowWindow(panel, SW_SHOW);
        let previous = gfx::force_foreground(panel);
        STATE.with_borrow_mut(|s| s.previous = previous);
        SetTimer(panel, TIMER_SLIDE, 15, None);
    }
}

/// Slides the panel out (or hides it at once when not `animated`).
fn close_panel(animated: bool) {
    let panel = STATE.with_borrow_mut(|s| {
        if !s.open {
            return None;
        }
        s.open = false;
        s.press = None;
        s.tracking = false;
        s.slide = animated.then(|| (Instant::now(), false));
        Some(s.panel)
    });
    let Some(panel) = panel else { return };
    unsafe {
        if animated {
            SetTimer(panel, TIMER_SLIDE, 15, None);
        } else {
            let _ = ShowWindow(panel, SW_HIDE);
        }
    }
}

fn card_width() -> i32 {
    px(SIDE_PAD * 2.0 + CELL_W * COLUMNS as f32)
}

fn panel_size() -> (i32, i32) {
    let work = work_area();
    (card_width() + px(PANEL_PAD * 2.0), work.bottom - work.top - px(PANEL_MARGIN * 2.0))
}

/// (entry, cell) pairs of one shelf.
type Cells = Vec<(usize, RECT)>;

/// Where everything is, in panel coordinates (shelves already scrolled).
struct Layout {
    close: RECT,
    content: RECT,
    /// Per shelf: card, header, and its items.
    cards: Vec<(RECT, RECT, Cells)>,
    /// Total unscrolled content height.
    total: i32,
}

fn layout(s: &State) -> Layout {
    let (width, height) = panel_size();
    let close = RECT { left: width - px(PANEL_PAD + 34.0), top: px(10.0), right: width - px(PANEL_PAD), bottom: px(TITLE_H - 6.0) };
    let content = RECT { left: 0, top: px(TITLE_H), right: width, bottom: height - px(PANEL_PAD) };
    let left = px(PANEL_PAD);
    let card_w = card_width();
    let scroll = px(s.scroll);
    let mut y = content.top;
    let mut cards = Vec::new();
    for shelf in &s.shelves {
        let collapsed = s.collapsed.get(&shelf.category).copied().unwrap_or(false);
        let rows = shelf.entries.len().div_ceil(COLUMNS) as f32;
        let body = if collapsed { 0.0 } else { CELL_H * rows + BOTTOM_PAD };
        let top = y - scroll;
        let card = RECT { left, top, right: left + card_w, bottom: top + px(HEADER_H + body) };
        let header = RECT { bottom: top + px(HEADER_H), ..card };
        let cells = if collapsed {
            Vec::new()
        } else {
            shelf
                .entries
                .iter()
                .enumerate()
                .map(|(slot, &entry)| {
                    let (col, row) = ((slot % COLUMNS) as f32, (slot / COLUMNS) as f32);
                    let cell_left = left + px(SIDE_PAD + col * CELL_W);
                    let cell_top = header.bottom + px(row * CELL_H);
                    (entry, RECT { left: cell_left, top: cell_top, right: cell_left + px(CELL_W), bottom: cell_top + px(CELL_H) })
                })
                .collect()
        };
        cards.push((card, header, cells));
        y += card.bottom - card.top + px(CARD_GAP);
    }
    Layout { close, content, cards, total: y - content.top }
}

fn max_scroll(s: &State) -> f32 {
    let l = layout(s);
    ((l.total - (l.content.bottom - l.content.top)) as f32 / scale()).max(0.0)
}

fn inside(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn spot_at(s: &State, p: POINT) -> Option<Spot> {
    let l = layout(s);
    if inside(&l.close, p) {
        return Some(Spot::Close);
    }
    if !inside(&l.content, p) {
        return None;
    }
    for (i, (card, header, cells)) in l.cards.iter().enumerate() {
        if inside(header, p) {
            return Some(Spot::Header(i));
        }
        if let Some((entry, _)) = cells.iter().find(|(_, r)| inside(r, p)) {
            return Some(Spot::Item(*entry));
        }
        if inside(card, p) {
            return Some(Spot::Card(i));
        }
    }
    None
}

/// The shelf index an item is on.
fn shelf_of(s: &State, entry: usize) -> Option<usize> {
    s.shelves.iter().position(|sh| sh.entries.contains(&entry))
}

unsafe fn render_panel() {
    let (width, height) = panel_size();
    let Some(mut canvas) = Canvas::new(width, height) else { return };
    let dc = canvas.dc;
    let client = RECT { left: 0, top: 0, right: width, bottom: height };

    let (panel, progress, opening) = STATE.with_borrow(|s| {
        let (t, opening) = s.slide.map_or((1.0, s.open), |(start, opening)| ((start.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.0), opening));
        (s.panel, t, opening)
    });

    STATE.with_borrow(|s| {
        let l = layout(s);
        gfx::fill(dc, &client, COLOR_PANEL);

        // Title row.
        SelectObject(dc, s.title);
        SetTextColor(dc, COLOR_TEXT);
        let title = RECT { left: px(PANEL_PAD + 6.0), top: 0, right: l.close.left, bottom: px(TITLE_H) };
        gfx::draw_text(dc, "Рабочий стол", &title, DT_LEFT);
        let title_w = gfx::text_width(dc, "Рабочий стол");
        SelectObject(dc, s.font);
        SetTextColor(dc, COLOR_DIM);
        gfx::draw_text(dc, &s.entries.len().to_string(), &RECT { left: title.left + title_w + px(8.0), ..title }, DT_LEFT);
        if s.hover == Some(Spot::Close) {
            gfx::fill_round(dc, &l.close, COLOR_HOVER, px(20.0));
        }
        SelectObject(dc, s.glyphs);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, "\u{E76B}", &l.close, DT_CENTER);

        let saved = SaveDC(dc);
        IntersectClipRect(dc, l.content.left, l.content.top, l.content.right, l.content.bottom);
        let icon = px(ICON);
        let dragged = s.press.as_ref().filter(|p| p.dragging).map(|p| p.item);
        let mut spots = Vec::new();
        for (i, (shelf, (card, header, cells))) in s.shelves.iter().zip(&l.cards).enumerate() {
            if card.bottom <= l.content.top || card.top >= l.content.bottom {
                continue;
            }
            let color = if s.drop_target == Some(i) { COLOR_CARD_DROP } else { COLOR_CARD };
            gfx::fill_round(dc, card, color, px(CARD_RADIUS * 2.0));

            // Header: glyph, name, count, roll-up chevron.
            let collapsed = cells.is_empty();
            let (name, glyph) = CATEGORIES[shelf.category];
            let text = RECT { left: header.left + px(14.0), right: header.right - px(40.0), ..*header };
            SelectObject(dc, s.glyphs);
            SetTextColor(dc, COLOR_TEXT);
            gfx::draw_text(dc, glyph, &RECT { right: text.left + px(20.0), ..text }, DT_LEFT);
            SelectObject(dc, s.bold);
            let label = RECT { left: text.left + px(26.0), ..text };
            gfx::draw_text(dc, name, &label, DT_LEFT);
            let name_w = gfx::text_width(dc, name);
            SelectObject(dc, s.font);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, &shelf.entries.len().to_string(), &RECT { left: label.left + name_w + px(8.0), ..text }, DT_LEFT);
            SelectObject(dc, s.glyphs);
            SetTextColor(dc, if s.hover == Some(Spot::Header(i)) { COLOR_TEXT } else { COLOR_DIM });
            let chevron = RECT { left: header.right - px(40.0), right: header.right - px(10.0), ..*header };
            gfx::draw_text(dc, if collapsed { "\u{E70D}" } else { "\u{E70E}" }, &chevron, DT_CENTER);
            if !collapsed {
                gfx::fill(dc, &RECT { left: card.left + px(12.0), top: header.bottom - 1, right: card.right - px(12.0), bottom: header.bottom }, COLOR_DIVIDER);
            }

            for (entry, r) in cells {
                if r.bottom <= l.content.top || r.top >= l.content.bottom {
                    continue;
                }
                let inner = RECT { left: r.left + px(3.0), top: r.top + px(2.0), right: r.right - px(3.0), bottom: r.bottom - px(2.0) };
                if s.selected == Some(*entry) {
                    gfx::fill_round(dc, &inner, COLOR_SELECTED, px(CELL_RADIUS * 2.0));
                } else if s.hover == Some(Spot::Item(*entry)) {
                    gfx::fill_round(dc, &inner, COLOR_HOVER, px(CELL_RADIUS * 2.0));
                }
                let e = &s.entries[*entry];
                let x = r.left + (r.right - r.left - icon) / 2;
                let y = r.top + px(8.0);
                spots.push((e.path.clone(), x, y, if dragged == Some(*entry) { 0.35 } else { 1.0 }));
                SelectObject(dc, s.font);
                SetTextColor(dc, COLOR_TEXT);
                let text = RECT { left: r.left + px(4.0), top: y + icon + px(4.0), right: r.right - px(4.0), bottom: r.bottom - px(2.0) };
                gfx::draw_text_wrapped_aligned(dc, &e.name, &text, DT_CENTER);
            }
        }
        if s.shelves.is_empty() {
            SelectObject(dc, s.font);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, "На рабочем столе пусто", &RECT { bottom: l.content.top + px(60.0), ..l.content }, DT_CENTER);
        }

        // Scroll indicator.
        let view = (l.content.bottom - l.content.top) as f32;
        if l.total as f32 > view {
            let thumb = (view * view / l.total as f32).max(px(24.0) as f32);
            let limit = max_scroll(s).max(1.0);
            let top = l.content.top as f32 + s.scroll / limit * (view - thumb);
            let bar = RECT { left: width - px(6.0), top: top as i32, right: width - px(3.0), bottom: (top + thumb) as i32 };
            gfx::fill_round(dc, &bar, COLOR_SCROLLBAR, px(3.0));
        }
        let _ = RestoreDC(dc, saved);

        canvas.shape_alpha(COLOR_PANEL, PANEL_ALPHA, &[(client, PANEL_RADIUS * scale())]);
        for (path, x, y, opacity) in spots {
            if let Some(image) = icons::get(&path, icon) {
                canvas.draw_icon(&image, x, y, icon, Some(&l.content), opacity);
            }
        }
    });

    // Slide in from (and out to) the left edge.
    let work = work_area();
    let ease = 1.0 - (1.0 - progress).powi(3);
    let shown = if opening { ease } else { 1.0 - ease };
    let rest = work.left + px(PANEL_MARGIN);
    let x = rest - ((1.0 - shown) * (width + px(PANEL_MARGIN)) as f32) as i32;
    canvas.present(panel, x, work.top + px(PANEL_MARGIN), (255.0 * shown.max(0.05)) as u8);
}

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_TIMER && wparam.0 == TIMER_RESCAN {
        rescan(false);
        update_tab_visibility();
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let local = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_TIMER if wparam.0 == TIMER_SLIDE => {
            let (done, open) = STATE.with_borrow(|s| (s.slide.is_none_or(|(start, _)| start.elapsed() >= SLIDE), s.open));
            render_panel();
            if done {
                let _ = KillTimer(hwnd, TIMER_SLIDE);
                STATE.with_borrow_mut(|s| s.slide = None);
                if !open {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => close_panel(true),
        WM_KEYDOWN if wparam.0 == 0x1B => {
            close_panel(true);
            gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
        }
        WM_MOUSEWHEEL => {
            let notches = (wparam.0 >> 16 & 0xFFFF) as i16 as f32 / 120.0;
            STATE.with_borrow_mut(|s| {
                s.scroll = (s.scroll - notches * SCROLL_STEP).clamp(0.0, max_scroll(s));
                s.hover = None;
            });
            render_panel();
        }
        WM_LBUTTONDOWN => {
            let p = local();
            let item = STATE.with_borrow_mut(|s| match spot_at(s, p) {
                Some(Spot::Item(i)) => {
                    s.selected = Some(i);
                    Some(i)
                }
                _ => None,
            });
            if let Some(item) = item {
                STATE.with_borrow_mut(|s| s.press = Some(Press { start: p, item, dragging: false }));
                SetCapture(hwnd);
            }
            render_panel();
        }
        WM_MOUSEMOVE => on_move(hwnd, local()),
        WM_LBUTTONUP => on_up(hwnd, local()),
        WM_CAPTURECHANGED => {
            if STATE.with_borrow_mut(|s| {
                s.drop_target = None;
                s.press.take()
            })
            .is_some()
            {
                render_panel();
            }
        }
        WM_RBUTTONUP => {
            let _ = context_menu(hwnd, local());
        }
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.tracking = false;
                s.hover = None;
            });
            render_panel();
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

unsafe fn on_move(hwnd: HWND, p: POINT) {
    if !STATE.with_borrow(|s| s.tracking) {
        STATE.with_borrow_mut(|s| s.tracking = true);
        let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
        let _ = TrackMouseEvent(&mut tme);
    }
    // Dragging an item: highlight the shelf it would land on.
    let dragging = STATE.with_borrow_mut(|s| {
        let Some(press) = s.press.as_mut() else { return false };
        if !press.dragging && (p.x - press.start.x).abs() <= DRAG_THRESHOLD && (p.y - press.start.y).abs() <= DRAG_THRESHOLD {
            return false;
        }
        press.dragging = true;
        let item = press.item;
        let target = match spot_at(s, p) {
            Some(Spot::Header(i) | Spot::Card(i)) => Some(i),
            Some(Spot::Item(other)) => shelf_of(s, other),
            _ => None,
        };
        s.drop_target = target.filter(|&t| Some(t) != shelf_of(s, item));
        true
    });
    if dragging {
        if let Ok(cursor) = LoadCursorW(None, IDC_SIZEALL) {
            SetCursor(cursor);
        }
        render_panel();
        return;
    }
    let hover = STATE.with_borrow(|s| spot_at(s, p));
    if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
        render_panel();
    }
}

unsafe fn on_up(hwnd: HWND, p: POINT) {
    let (press, target) = STATE.with_borrow_mut(|s| (s.press.take(), s.drop_target.take()));
    let _ = ReleaseCapture();
    match press {
        Some(press) if press.dragging => match target {
            Some(shelf) => {
                let category = STATE.with_borrow(|s| s.shelves[shelf].category);
                move_to(press.item, Some(category));
            }
            None => render_panel(),
        },
        Some(press) => {
            let double = STATE.with_borrow_mut(|s| {
                let double = s.last_click.is_some_and(|(i, t)| i == press.item && t.elapsed().as_millis() <= GetDoubleClickTime() as u128);
                s.last_click = if double { None } else { Some((press.item, Instant::now())) };
                double
            });
            if double {
                open(press.item);
            }
        }
        None => match STATE.with_borrow(|s| spot_at(s, p)) {
            Some(Spot::Close) => {
                close_panel(true);
                gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
            }
            Some(Spot::Header(i)) => toggle_collapsed(i),
            _ => {}
        },
    }
    let _ = hwnd;
}

fn toggle_collapsed(shelf: usize) {
    STATE.with_borrow_mut(|s| {
        let Some(category) = s.shelves.get(shelf).map(|sh| sh.category) else { return };
        let collapsed = s.collapsed.entry(category).or_insert(false);
        *collapsed = !*collapsed;
        s.scroll = s.scroll.min(max_scroll(s));
        save(s);
    });
    unsafe { render_panel() };
}

fn entry_path(index: usize) -> Option<String> {
    STATE.with_borrow(|s| s.entries.get(index).map(|e| e.path.clone()))
}

/// Opens an item; the panel closes by itself when the program takes the foreground.
fn open(index: usize) {
    if let Some(path) = entry_path(index) {
        apps::open(&path);
    }
}

/// Puts an entry on `category`'s shelf, or back on its automatic one (`None`).
fn move_to(index: usize, category: Option<usize>) {
    STATE.with_borrow_mut(|s| {
        let Some(entry) = s.entries.get(index) else { return };
        let key = entry.path.to_lowercase();
        match category {
            Some(c) if c != entry.category => {
                s.assignments.insert(key, c);
            }
            _ => {
                s.assignments.remove(&key);
            }
        }
        save(s);
    });
    rescan(true);
}

unsafe fn context_menu(hwnd: HWND, p: POINT) -> Result<()> {
    let (index, current, assigned) = STATE.with_borrow(|s| {
        let index = match spot_at(s, p) {
            Some(Spot::Item(i)) => Some(i),
            _ => None,
        };
        let current = index.and_then(|i| shelf_of(s, i)).map_or(OTHER, |sh| s.shelves[sh].category);
        let assigned = index.and_then(|i| s.entries.get(i)).is_some_and(|e| s.assignments.contains_key(&e.path.to_lowercase()));
        (index, current, assigned)
    });
    let Some(index) = index else { return Ok(()) };
    STATE.with_borrow_mut(|s| s.selected = Some(index));
    render_panel();

    let mut shelves = Menu::new();
    for (category, (name, _)) in CATEGORIES.iter().enumerate().filter(|(c, _)| *c != current) {
        shelves.item(CMD_MOVE + category, *name);
    }
    let mut menu = Menu::new();
    menu.item(CMD_OPEN, "Открыть").item(CMD_REVEAL, "Показать в Проводнике").separator().submenu("Переместить на полку", shelves);
    if assigned {
        menu.item(CMD_AUTO, "Вернуть на свою полку");
    }
    menu.separator().item(CMD_PROPERTIES, "Свойства").separator().item(CMD_DELETE, "Переместить в Корзину");
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let cmd = menu.show(Anchor::At(cursor));

    let path = entry_path(index);
    match cmd {
        CMD_OPEN => open(index),
        CMD_REVEAL => {
            if let Some(path) = &path {
                close_panel(false);
                apps::reveal(path);
            }
        }
        CMD_PROPERTIES => {
            if let Some(path) = &path {
                let path = HSTRING::from(path.as_str());
                let _ = SHObjectProperties(hwnd, SHOP_FILEPATH, PCWSTR(path.as_ptr()), PCWSTR::null());
            }
        }
        CMD_DELETE => {
            if let Some(path) = &path {
                recycle(hwnd, path);
                rescan(true);
            }
        }
        CMD_AUTO => move_to(index, None),
        cmd if (CMD_MOVE..CMD_MOVE + CATEGORIES.len()).contains(&cmd) => move_to(index, Some(cmd - CMD_MOVE)),
        _ => {}
    }
    Ok(())
}

/// To the Recycle Bin, with Explorer's usual confirmation.
unsafe fn recycle(hwnd: HWND, path: &str) {
    let from: Vec<u16> = path.encode_utf16().chain([0, 0]).collect();
    let mut op = SHFILEOPSTRUCTW {
        hwnd,
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: FOF_ALLOWUNDO.0 as u16,
        ..Default::default()
    };
    SHFileOperationW(&mut op);
}

// ---- persistence ----

fn file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("CustomUI").join("shelves.tsv"))
}

/// `enabled 0|1`, `collapsed <category>`, `assign <path> <category>`.
fn load() -> (bool, HashMap<usize, bool>, HashMap<String, usize>) {
    let (mut enabled, mut collapsed, mut assignments) = (true, HashMap::new(), HashMap::new());
    let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else {
        return (enabled, collapsed, assignments);
    };
    let category = |name: &str| CATEGORIES.iter().position(|(n, _)| *n == name);
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first().copied() {
            Some("enabled") => enabled = f.get(1) != Some(&"0"),
            Some("collapsed") => {
                if let Some(c) = f.get(1).and_then(|n| category(n)) {
                    collapsed.insert(c, true);
                }
            }
            // Older `shelf` lines (shelves placed on the desktop) don't apply to the panel.
            Some("assign") => {
                if let (Some(path), Some(c)) = (f.get(1), f.get(2).and_then(|n| category(n))) {
                    assignments.insert(path.to_lowercase(), c);
                }
            }
            _ => {}
        }
    }
    (enabled, collapsed, assignments)
}

fn save(s: &State) {
    let Some(path) = file() else { return };
    let mut text = format!("enabled\t{}\n", if s.enabled { 1 } else { 0 });
    let mut collapsed: Vec<usize> = s.collapsed.iter().filter(|(_, c)| **c).map(|(c, _)| *c).collect();
    collapsed.sort();
    for category in collapsed {
        text += &format!("collapsed\t{}\n", CATEGORIES[category].0);
    }
    for (file, category) in &s.assignments {
        text += &format!("assign\t{}\t{}\n", file, CATEGORIES[*category].0);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify(r"C:\Users\u\Desktop\Counter-Strike 2.url", false, None, Some("steam://rungameid/730")), GAMES);
        assert_eq!(classify(r"C:\Users\u\Desktop\Site.url", false, None, Some("https://example.com")), LINKS);
        assert_eq!(classify(r"C:\Users\u\Desktop\Discord.lnk", false, Some(r"C:\Users\u\AppData\Local\Discord\Update.exe"), None), APPS);
        assert_eq!(classify(r"C:\x\Witcher.lnk", false, Some(r"D:\SteamLibrary\steamapps\common\Witcher\witcher3.exe"), None), GAMES);
        assert_eq!(classify(r"C:\x\Steam.lnk", false, Some(r"C:\Program Files (x86)\Steam\steam.exe"), None), GAMES);
        assert_eq!(classify(r"C:\x\Calculator.lnk", false, None, None), APPS);
        assert_eq!(classify(r"C:\x\Backrooms.lnk", false, Some(r"C:\Users\u\Downloads\Backrooms\BetGame.exe"), None), GAMES);
        assert_eq!(classify(r"C:\x\RPCS3.lnk", false, Some(r"C:\Users\u\Desktop\ps3emu\rpcs3.exe"), None), GAMES);
        assert_eq!(classify(r"C:\x\PineconeMC.lnk", false, Some(r"C:\Users\u\AppData\Local\Programs\ElyPrismLauncher\elyprismlauncher.exe"), None), GAMES);
        assert_eq!(classify(r"C:\x\Spotify.lnk", false, Some(r"C:\Users\u\AppData\Roaming\Spotify\Spotify.exe"), None), APPS);
        assert_eq!(classify(r"C:\x\OBS.lnk", false, Some(r"C:\Program Files\obs-studio\bin\64bit\obs64.exe"), None), APPS);
        assert_eq!(classify(r"C:\x\отчёт.docx", false, None, None), DOCUMENTS);
        assert_eq!(classify(r"C:\x\photo.JPG", false, None, None), IMAGES);
        assert_eq!(classify(r"C:\x\backup.7z", false, None, None), ARCHIVES);
        assert_eq!(classify(r"C:\x\song.mp3", false, None, None), MEDIA);
        assert_eq!(classify(r"C:\x\Новая папка", true, None, None), FOLDERS);
        assert_eq!(classify(r"C:\x\thing.xyz", false, None, None), OTHER);
    }

    #[test]
    fn names() {
        assert_eq!(display_name(r"C:\x\Discord.lnk"), "Discord");
        assert_eq!(display_name(r"C:\x\report.pdf"), "report.pdf");
    }
}
