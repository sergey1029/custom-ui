//! Spotlight-style search, opened with the Win key, Alt+Space (or Ctrl+Alt+Space
//! if that's taken): a rounded bar in the middle of the screen that grows a
//! result list as you type. It finds apps (the same list as Windows 11's "All
//! apps", Store apps included) and indexed files, evaluates arithmetic, and
//! offers a web search.

use std::cell::RefCell;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::{w, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::macmenu::{Anchor, Menu};
use crate::apps::{self, App};
use crate::dock::{self, Pin};
use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use crate::{config, filesearch, icons, winkey};

const WIDTH: f32 = 680.0;
/// The bar's top edge sits this far down the work area, like Spotlight's.
const TOP: f32 = 0.22;
const SEARCH_H: f32 = 52.0;
const PAD: f32 = 6.0;
const LIST_PAD: f32 = 8.0;
const RADIUS: f32 = 24.0;
const ROW_H: f32 = 48.0;
const HEADER_H: f32 = 30.0;
const ROW_RADIUS: f32 = 12.0;
const ICON: f32 = 32.0;
/// Most visible list height (unscaled px).
const MAX_RESULTS_H: f32 = ROW_H * 7.0;
const QUERY_FONT: f32 = 21.0;
const FONT_SIZE: f32 = 15.0;
const SMALL_FONT: f32 = 12.5;
const SCROLL_STEP: f32 = ROW_H * 3.0;
const PAGE_ROWS: isize = 6;
const OPEN_ANIM: Duration = Duration::from_millis(140);
const SLIDE: f32 = 10.0;

const TIMER_ANIM: usize = 1;
const HOTKEY_ID: i32 = 1;
const CF_UNICODETEXT: u32 = 13;

const COLOR_PANEL: COLORREF = rgb(30, 30, 34);
const COLOR_DIVIDER: COLORREF = rgb(58, 58, 66);
const COLOR_SELECTED_DIM: COLORREF = rgb(200, 214, 250);
const COLOR_SCROLLBAR: COLORREF = rgb(88, 88, 98);

const CMD_OPEN: usize = 1;
const CMD_PIN: usize = 2;
const CMD_REVEAL: usize = 3;

enum Row {
    Header(String),
    App(usize),
    /// Formatted result of the query as arithmetic.
    Calc(String),
    /// Index into `State::files`.
    File(usize),
    Web(String),
}

impl Row {
    fn height(&self) -> f32 {
        match self {
            Row::Header(_) => HEADER_H,
            _ => ROW_H,
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self, Row::Header(_))
    }
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    visible: bool,
    hotkey: &'static str,
    /// Index search results for the current query, and that query's id.
    files: Vec<filesearch::Found>,
    search_id: u64,
    apps: Vec<App>,
    query: String,
    rows: Vec<Row>,
    /// Row offsets and total content height, in unscaled pixels like `scroll`.
    tops: Vec<f32>,
    content: f32,
    scroll: f32,
    selected: Option<usize>,
    hover: Option<usize>,
    tracking_mouse: bool,
    opened: Option<Instant>,
    origin: POINT,
    /// Focus goes back here when dismissed with Esc.
    previous: HWND,
    /// Frosted glass: the screen under the panel, captured when it opened.
    backdrop: Vec<u32>,
    query_font: HFONT,
    font: HFONT,
    bold_font: HFONT,
    small_font: HFONT,
    glyph_font: HFONT,
    big_glyph_font: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn create() -> Result<HWND> {
    unsafe {
        let hwnd = gfx::create_window(
            w!("CustomUI.Spotlight"),
            Some(wndproc),
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        )?;
        // Alt+Space is what PowerToys Run uses; fall back if something already owns it.
        let hotkey = if !config::flag("hotkey.alt_space") {
            ""
        } else if RegisterHotKey(hwnd, HOTKEY_ID, MOD_ALT | MOD_NOREPEAT, VK_SPACE.0 as u32).is_ok() {
            "Alt + Пробел"
        } else if RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_SPACE.0 as u32).is_ok() {
            "Ctrl + Alt + Пробел"
        } else {
            ""
        };
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.hotkey = hotkey;
        });
        update_fonts(hwnd);
        filesearch::start(hwnd);
        // A lone Win press opens us instead of Windows' Start.
        if config::flag("winkey.spotlight") {
            winkey::start(hwnd);
        }
        Ok(hwnd)
    }
}

pub fn destroy() {
    let hwnd = STATE.with_borrow(|s| s.hwnd);
    if !hwnd.is_invalid() {
        unsafe {
            let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
            let _ = DestroyWindow(hwnd);
        }
    }
}

pub fn toggle() {
    if STATE.with_borrow(|s| s.visible) {
        hide();
    } else {
        show();
    }
}

fn show() {
    let hwnd = STATE.with_borrow(|s| s.hwnd);
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        // Re-read every time so freshly installed apps show up.
        let apps = apps::all();
        let dpi = scale(hwnd);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let width = (WIDTH * dpi).round() as i32;
        let origin = POINT {
            x: work.left + (work.right - work.left - width) / 2,
            y: work.top + ((work.bottom - work.top) as f32 * TOP) as i32,
        };
        // Frosted glass: what's under the panel at its tallest, captured before it shows.
        let tallest = ((PAD + SEARCH_H + PAD + 1.0 + LIST_PAD * 2.0 + MAX_RESULTS_H) * dpi).round() as i32;
        let backdrop = gfx::frosted(RECT { left: origin.x, top: origin.y, right: origin.x + width, bottom: origin.y + tallest }).unwrap_or_default();

        STATE.with_borrow_mut(|s| {
            s.backdrop = backdrop;
            s.apps = apps;
            s.query.clear();
            query_changed(s);
            rebuild(s);
            s.visible = true;
            s.opened = Some(Instant::now());
            s.origin = origin;
        });
        render(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let previous = gfx::force_foreground(hwnd);
        STATE.with_borrow_mut(|s| s.previous = previous);
        SetTimer(hwnd, TIMER_ANIM, 15, None);
    }
}

pub fn hide() {
    let hwnd = STATE.with_borrow_mut(|s| {
        let was_visible = std::mem::replace(&mut s.visible, false);
        s.tracking_mouse = false;
        if was_visible { s.hwnd } else { HWND::default() }
    });
    if !hwnd.is_invalid() {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_ANIM);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Search results: the arithmetic result first, then matching apps (names or
/// words starting with the query first), then files, then a web search. With
/// an empty query the list is empty and only the bar shows.
fn rebuild(s: &mut State) {
    s.rows.clear();
    let query = s.query.trim().to_lowercase();
    if !query.is_empty() {
        if let Some(value) = calc::evaluate(&query) {
            s.rows.push(Row::Calc(calc::format(value)));
        }
        let mut hits: Vec<(bool, usize)> = s
            .apps
            .iter()
            .enumerate()
            .filter_map(|(i, app)| {
                let name = app.name.to_lowercase();
                let starts = name.split_whitespace().any(|word| word.starts_with(&query));
                name.contains(&query).then_some((!starts, i))
            })
            .collect();
        hits.sort();
        s.rows.extend(hits.into_iter().map(|(_, i)| Row::App(i)));
        if !s.files.is_empty() {
            s.rows.push(Row::Header("Файлы и папки".to_owned()));
            s.rows.extend((0..s.files.len()).map(Row::File));
        }
        s.rows.push(Row::Web(s.query.trim().to_owned()));
    }

    s.tops.clear();
    let mut y = 0.0;
    for row in &s.rows {
        s.tops.push(y);
        y += row.height();
    }
    s.content = y;
    s.scroll = 0.0;
    s.hover = None;
    s.selected = s.rows.iter().position(Row::selectable);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => toggle(),
        winkey::WM_WIN_KEY => toggle(),
        filesearch::WM_FILES_FOUND => on_files_found(hwnd, wparam.0 as u64),
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            let done = STATE.with_borrow(|s| s.opened.is_none_or(|t| t.elapsed() >= OPEN_ANIM));
            if done {
                let _ = KillTimer(hwnd, TIMER_ANIM);
            }
            render(hwnd);
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => hide(),
        WM_KEYDOWN => on_key(hwnd, VIRTUAL_KEY(wparam.0 as u16)),
        WM_CHAR => on_char(hwnd, wparam.0 as u32),
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16 & 0xFFFF) as i16 as f32 / 120.0;
            scroll_by(hwnd, -delta * SCROLL_STEP);
        }
        WM_MOUSEMOVE => on_mouse_move(hwnd, point()),
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.hover = None;
                s.tracking_mouse = false;
            });
            render(hwnd);
        }
        WM_LBUTTONUP => {
            if let Some(row) = row_at(hwnd, point()) {
                STATE.with_borrow_mut(|s| s.selected = Some(row));
                activate_selected(hwnd);
            }
        }
        WM_RBUTTONUP => {
            if let Some(row) = row_at(hwnd, point()) {
                let _ = show_menu(hwnd, row);
            }
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

fn scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

unsafe fn update_fonts(hwnd: HWND) {
    let size = |v: f32| -(v * scale(hwnd)) as i32;
    let fonts = [
        gfx::create_font(size(QUERY_FONT), FW_NORMAL, gfx::ui_font()),
        gfx::create_font(size(FONT_SIZE), FW_NORMAL, gfx::ui_font()),
        gfx::create_font(size(SMALL_FONT), FW_SEMIBOLD, gfx::ui_font()),
        gfx::create_font(size(SMALL_FONT), FW_NORMAL, gfx::ui_font()),
        gfx::create_font(size(QUERY_FONT), FW_NORMAL, w!("Segoe Fluent Icons")),
        gfx::create_font(size(22.0), FW_NORMAL, w!("Segoe Fluent Icons")),
    ];
    let old = STATE.with_borrow_mut(|s| {
        [
            std::mem::replace(&mut s.query_font, fonts[0]),
            std::mem::replace(&mut s.font, fonts[1]),
            std::mem::replace(&mut s.bold_font, fonts[2]),
            std::mem::replace(&mut s.small_font, fonts[3]),
            std::mem::replace(&mut s.glyph_font, fonts[4]),
            std::mem::replace(&mut s.big_glyph_font, fonts[5]),
        ]
    });
    for f in old.into_iter().filter(|f| !f.is_invalid()) {
        let _ = DeleteObject(f);
    }
}

/// Visible list height (unscaled): the content, capped; 0 for the bare bar.
fn list_height(s: &State) -> f32 {
    if s.rows.is_empty() {
        return 0.0;
    }
    s.content.min(MAX_RESULTS_H)
}

struct Layout {
    search: RECT,
    /// Empty (zero height) while there are no results.
    list: RECT,
    height: i32,
}

/// The bar alone is a pill; with results it grows a divider and a list below.
fn layout(s: &State, dpi: f32) -> Layout {
    let px = |v: f32| (v * dpi).round() as i32;
    let width = px(WIDTH);
    let search = RECT { left: px(PAD), top: px(PAD), right: width - px(PAD), bottom: px(PAD + SEARCH_H) };
    let list_h = list_height(s);
    if list_h == 0.0 {
        return Layout { search, list: RECT { top: search.bottom, bottom: search.bottom, ..search }, height: search.bottom + px(PAD) };
    }
    let top = search.bottom + px(PAD + 1.0 + LIST_PAD);
    let list = RECT { left: px(LIST_PAD), top, right: width - px(LIST_PAD), bottom: top + px(list_h) };
    Layout { search, list, height: list.bottom + px(LIST_PAD) }
}

fn row_rect(s: &State, i: usize, list: &RECT, dpi: f32) -> RECT {
    let top = list.top + ((s.tops[i] - s.scroll) * dpi).round() as i32;
    RECT {
        left: list.left,
        top,
        right: list.right - (10.0 * dpi) as i32,
        bottom: top + (s.rows[i].height() * dpi).round() as i32,
    }
}

unsafe fn row_at(hwnd: HWND, p: POINT) -> Option<usize> {
    let dpi = scale(hwnd);
    STATE.with_borrow(|s| {
        let l = layout(s, dpi);
        if p.y < l.list.top || p.y >= l.list.bottom || p.x < l.list.left || p.x >= l.list.right {
            return None;
        }
        let y = (p.y - l.list.top) as f32 / dpi + s.scroll;
        (0..s.rows.len())
            .find(|&i| y >= s.tops[i] && y < s.tops[i] + s.rows[i].height())
            .filter(|&i| s.rows[i].selectable())
    })
}

/// Layered windows don't get WM_PAINT; the canvas size also sets the window size,
/// which follows the number of results.
unsafe fn render(hwnd: HWND) {
    let dpi = scale(hwnd);
    let (origin, progress, height) = STATE.with_borrow(|s| {
        let t = s.opened.map_or(1.0, |t| t.elapsed().as_secs_f32() / OPEN_ANIM.as_secs_f32());
        (s.origin, t.min(1.0), layout(s, dpi).height)
    });
    let Some(mut canvas) = Canvas::new((WIDTH * dpi).round() as i32, height) else { return };
    let ease = 1.0 - (1.0 - progress).powi(3);

    STATE.with_borrow(|s| draw(&mut canvas, s, dpi));
    let y = origin.y - (SLIDE * dpi * (1.0 - ease)) as i32;
    canvas.present(hwnd, origin.x, y, (255.0 * ease) as u8);
}

unsafe fn draw(canvas: &mut Canvas, s: &State, dpi: f32) {
    let px = |v: f32| (v * dpi) as i32;
    let dc = canvas.dc;
    let l = layout(s, dpi);
    let client = RECT { left: 0, top: 0, right: canvas.width, bottom: canvas.height };

    canvas.backdrop(&s.backdrop, canvas.width, COLOR_PANEL);

    // The search line: magnifier, query (or placeholder) and caret.
    SelectObject(dc, s.glyph_font);
    SetTextColor(dc, COLOR_DIM);
    let glyph = RECT { left: l.search.left + px(16.0), right: l.search.left + px(52.0), ..l.search };
    gfx::draw_text(dc, "\u{E721}", &glyph, DT_LEFT);
    SelectObject(dc, s.query_font);
    let text = RECT { left: l.search.left + px(56.0), right: l.search.right - px(20.0), ..l.search };
    if s.query.is_empty() {
        let hint = "Поиск Spotlight";
        gfx::draw_text(dc, hint, &text, DT_LEFT);
        if !s.hotkey.is_empty() {
            SelectObject(dc, s.small_font);
            gfx::draw_text(dc, s.hotkey, &text, DT_RIGHT);
            SelectObject(dc, s.query_font);
        }
    } else {
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, &s.query, &text, DT_LEFT | DT_END_ELLIPSIS);
    }
    let caret_x = (text.left + gfx::text_width(dc, &s.query)).min(text.right);
    let caret_h = px(26.0);
    let caret_top = l.search.top + (l.search.bottom - l.search.top - caret_h) / 2;
    let caret = RECT { left: caret_x + 1, top: caret_top, right: caret_x + 1 + px(1.5).max(1), bottom: caret_top + caret_h };
    gfx::fill(dc, &caret, COLOR_TEXT);

    let mut icon_spots = Vec::new();
    if l.list.bottom > l.list.top {
        let divider = RECT { left: 0, top: l.search.bottom + px(PAD), right: client.right, bottom: l.search.bottom + px(PAD) + px(1.0).max(1) };
        gfx::fill(dc, &divider, COLOR_DIVIDER);

        let saved = SaveDC(dc);
        IntersectClipRect(dc, l.list.left, l.list.top, l.list.right, l.list.bottom);
        let icon = px(ICON);
        for i in 0..s.rows.len() {
            let r = row_rect(s, i, &l.list, dpi);
            if r.bottom <= l.list.top || r.top >= l.list.bottom {
                continue;
            }
            let row = &s.rows[i];
            if let Row::Header(letter) = row {
                SelectObject(dc, s.bold_font);
                SetTextColor(dc, COLOR_DIM);
                gfx::draw_text(dc, letter, &RECT { left: r.left + px(14.0), ..r }, DT_LEFT);
                continue;
            }
            let selected = s.selected == Some(i);
            if selected {
                gfx::fill_round(dc, &r, gfx::accent(), px(ROW_RADIUS * 2.0));
            } else if s.hover == Some(i) {
                // A translucent white wash over the glass, kept inside the list.
                let visible = RECT { top: r.top.max(l.list.top), bottom: r.bottom.min(l.list.bottom), ..r };
                canvas.fill_shape(&visible, ROW_RADIUS * dpi, COLOR_TEXT, 26);
            }
            let square = RECT { left: r.left + px(10.0), top: r.top + (r.bottom - r.top - icon) / 2, right: r.left + px(10.0) + icon, bottom: 0 };
            let square = RECT { bottom: square.top + icon, ..square };
            let (title, kind) = match row {
                Row::App(app) => {
                    icon_spots.push((s.apps[*app].parsing_name(), square.left, square.top));
                    (s.apps[*app].name.clone(), "Приложение".to_owned())
                }
                Row::File(file) => {
                    let found = &s.files[*file];
                    icon_spots.push((found.path.clone(), square.left, square.top));
                    (found.name.clone(), file_kind(found))
                }
                Row::Calc(result) => {
                    draw_glyph(dc, s, "\u{E8EF}", &square);
                    (format!("= {result}"), "Калькулятор · Enter — копировать".to_owned())
                }
                Row::Web(query) => {
                    draw_glyph(dc, s, "\u{E774}", &square);
                    (format!("Искать «{query}» в интернете"), web_engine().0.to_owned())
                }
                Row::Header(_) => unreachable!(),
            };
            SelectObject(dc, s.small_font);
            SetTextColor(dc, if selected { COLOR_SELECTED_DIM } else { COLOR_DIM });
            let kind_w = gfx::text_width(dc, &kind);
            let kind_rect = RECT { left: r.right - px(14.0) - kind_w, right: r.right - px(14.0), ..r };
            gfx::draw_text(dc, &kind, &kind_rect, DT_RIGHT);
            SelectObject(dc, s.font);
            SetTextColor(dc, COLOR_TEXT);
            let name = RECT { left: square.right + px(14.0), right: kind_rect.left - px(12.0), ..r };
            gfx::draw_text(dc, &title, &name, DT_LEFT | DT_END_ELLIPSIS);
        }

        // Thin scroll indicator when the list overflows.
        let view = (l.list.bottom - l.list.top) as f32;
        let content = s.content * dpi;
        if content > view + 1.0 {
            let thumb = (view * view / content).max(px(24.0) as f32);
            let top = l.list.top as f32 + s.scroll * dpi / (content - view) * (view - thumb);
            let bar = RECT { left: l.list.right - px(5.0), top: top as i32, right: l.list.right - px(2.0), bottom: (top + thumb) as i32 };
            gfx::fill_round(dc, &bar, COLOR_SCROLLBAR, px(3.0));
        }
        let _ = RestoreDC(dc, saved);
    }

    let radius = if l.list.bottom > l.list.top { RADIUS * dpi } else { canvas.height as f32 / 2.0 };
    canvas.finish_panel(radius);

    let icon = px(ICON);
    for (source, x, y) in icon_spots {
        if let Some(image) = icons::get(&source, icon) {
            canvas.draw_icon(&image, x, y, icon, Some(&l.list), 1.0);
        }
    }
}

/// "Папка", or the folder the file is in.
fn file_kind(found: &filesearch::Found) -> String {
    if found.folder {
        return "Папка".to_owned();
    }
    Path::new(&found.path)
        .parent()
        .and_then(|p| p.file_name())
        .map_or_else(|| "Файл".to_owned(), |dir| format!("в «{}»", dir.to_string_lossy()))
}

/// Every edit of the query starts a new index search; old results are dropped.
fn query_changed(s: &mut State) {
    s.search_id += 1;
    s.files.clear();
    let query = s.query.trim();
    if query.chars().count() >= 2 && config::flag("spotlight.files") {
        filesearch::request(s.search_id, query);
    }
}

unsafe fn on_files_found(hwnd: HWND, id: u64) {
    let Some(found) = filesearch::results(id) else { return };
    let changed = STATE.with_borrow_mut(|s| {
        if id != s.search_id || !s.visible {
            return false;
        }
        // Results arrive while typing; keep the user's selection if they moved it.
        let first = s.rows.iter().position(Row::selectable);
        let kept = s.selected.filter(|&sel| Some(sel) != first);
        s.files = found;
        rebuild(s);
        if let Some(sel) = kept.filter(|&sel| s.rows.get(sel).is_some_and(Row::selectable)) {
            s.selected = Some(sel);
        }
        true
    });
    if changed {
        render(hwnd);
    }
}

unsafe fn draw_glyph(dc: HDC, s: &State, glyph: &str, square: &RECT) {
    SelectObject(dc, s.big_glyph_font);
    SetTextColor(dc, COLOR_TEXT);
    gfx::draw_text(dc, glyph, square, DT_CENTER);
}

unsafe fn on_key(hwnd: HWND, key: VIRTUAL_KEY) {
    match key {
        VK_ESCAPE => {
            let cleared = STATE.with_borrow_mut(|s| {
                let had_query = !s.query.is_empty();
                if had_query {
                    s.query.clear();
                    query_changed(s);
                    rebuild(s);
                }
                had_query
            });
            if !cleared {
                hide();
                gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
                return;
            }
        }
        VK_RETURN => return activate_selected(hwnd),
        VK_UP => move_selection(-1),
        VK_DOWN => move_selection(1),
        VK_PRIOR => move_selection(-PAGE_ROWS),
        VK_NEXT => move_selection(PAGE_ROWS),
        _ => return,
    }
    render(hwnd);
}

unsafe fn on_char(hwnd: HWND, code: u32) {
    let changed = STATE.with_borrow_mut(|s| {
        match code {
            0x08 => {
                s.query.pop();
            }
            // Ctrl+Backspace
            0x7F => s.query.clear(),
            _ => match char::from_u32(code) {
                Some(c) if !c.is_control() => s.query.push(c),
                _ => return false,
            },
        }
        query_changed(s);
        rebuild(s);
        true
    });
    if changed {
        render(hwnd);
    }
}

fn move_selection(delta: isize) {
    STATE.with_borrow_mut(|s| {
        let selectable: Vec<usize> = (0..s.rows.len()).filter(|&i| s.rows[i].selectable()).collect();
        if selectable.is_empty() {
            return;
        }
        let current = s.selected.and_then(|sel| selectable.iter().position(|&i| i == sel)).unwrap_or(0);
        let next = (current as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
        let row = selectable[next];
        s.selected = Some(row);
        // Keep the selection in view; at the top, also reveal its letter header.
        let view = list_height(s);
        let top = if next == 0 { 0.0 } else { s.tops[row] };
        let bottom = s.tops[row] + s.rows[row].height();
        if top < s.scroll {
            s.scroll = top;
        } else if bottom > s.scroll + view {
            s.scroll = bottom - view;
        }
    });
}

unsafe fn scroll_by(hwnd: HWND, delta: f32) {
    STATE.with_borrow_mut(|s| {
        let view = list_height(s);
        s.scroll = (s.scroll + delta).clamp(0.0, (s.content - view).max(0.0));
    });
    render(hwnd);
}

unsafe fn on_mouse_move(hwnd: HWND, p: POINT) {
    let start_tracking = STATE.with_borrow_mut(|s| !std::mem::replace(&mut s.tracking_mouse, true));
    if start_tracking {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }
    let row = row_at(hwnd, p);
    let changed = STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, row) != row);
    if changed {
        render(hwnd);
    }
}

enum Action {
    Open(String),
    Copy(String),
}

unsafe fn activate_selected(hwnd: HWND) {
    let action = STATE.with_borrow(|s| match s.selected.map(|i| &s.rows[i]) {
        Some(Row::App(app)) => Some(Action::Open(s.apps[*app].parsing_name())),
        Some(Row::File(file)) => Some(Action::Open(s.files[*file].path.clone())),
        Some(Row::Calc(result)) => Some(Action::Copy(result.clone())),
        Some(Row::Web(query)) => Some(Action::Open(web_search_url(query))),
        _ => None,
    });
    match action {
        Some(Action::Open(target)) => {
            hide();
            apps::open(&target);
        }
        Some(Action::Copy(text)) => {
            copy_to_clipboard(hwnd, &text);
            hide();
        }
        None => {}
    }
}

/// The configured search engine's name and query URL prefix.
fn web_engine() -> (&'static str, &'static str) {
    match config::text("spotlight.web").as_str() {
        "yandex" => ("Яндекс", "https://yandex.ru/search/?text="),
        "bing" => ("Bing", "https://www.bing.com/search?q="),
        "duckduckgo" => ("DuckDuckGo", "https://duckduckgo.com/?q="),
        _ => ("Google", "https://www.google.com/search?q="),
    }
}

/// Searches the web for `query` with the configured engine.
pub fn web_search_url(query: &str) -> String {
    format!("{}{}", web_engine().1, url_encode(query))
}

fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_owned(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

unsafe fn copy_to_clipboard(hwnd: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    if OpenClipboard(hwnd).is_err() {
        return;
    }
    let _ = EmptyClipboard();
    if let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
        let target = GlobalLock(memory) as *mut u16;
        if !target.is_null() {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
            let _ = GlobalUnlock(memory);
            // On success the clipboard owns the memory.
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(memory.0));
        }
    }
    let _ = CloseClipboard();
}

unsafe fn show_menu(hwnd: HWND, row: usize) -> Result<()> {
    // Apps can be pinned; apps with an exe and found files can be revealed.
    let Some((pin, target)) = STATE.with_borrow(|s| match &s.rows[row] {
        Row::App(i) => {
            let app = &s.apps[*i];
            let pin = Pin { name: app.name.clone(), launch: app.parsing_name(), aumid: Some(app.id.clone()), path: app.target.clone() };
            Some((Some(pin), app.target.clone().filter(|t| Path::new(t).exists())))
        }
        Row::File(i) => Some((None, Some(s.files[*i].path.clone()))),
        _ => None,
    }) else {
        return Ok(());
    };

    let mut menu = Menu::new();
    menu.item(CMD_OPEN, "Открыть");
    if let Some(pin) = &pin {
        menu.check(CMD_PIN, "Оставить в Доке", dock::is_pinned(&pin.launch));
    }
    if target.is_some() {
        menu.item(CMD_REVEAL, "Показать в Проводнике");
    }
    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor)?;
    let cmd = menu.show(Anchor::At(cursor));

    match cmd {
        CMD_OPEN => {
            STATE.with_borrow_mut(|s| s.selected = Some(row));
            activate_selected(hwnd);
        }
        CMD_PIN => {
            if let Some(pin) = pin {
                dock::toggle_pin(pin);
                render(hwnd);
            }
        }
        CMD_REVEAL => {
            if let Some(target) = target {
                hide();
                apps::reveal(&target);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Spotlight-style inline calculator: + - * / ^, parentheses, unary minus,
/// '.' or ',' as decimal separator, '×' '÷' ':' accepted as operators.
mod calc {
    pub fn evaluate(input: &str) -> Option<f64> {
        let chars: Vec<char> = input.chars().filter(|c| !c.is_whitespace()).collect();
        // A lone number isn't a calculation.
        if !chars.iter().any(|c| "+-*/^×÷:(".contains(*c)) || !chars.iter().any(char::is_ascii_digit) {
            return None;
        }
        let mut parser = Parser { chars, pos: 0 };
        let value = parser.expr()?;
        (parser.pos == parser.chars.len() && value.is_finite()).then_some(value)
    }

    /// Integers without a fraction, otherwise up to 10 decimals; comma separator.
    pub fn format(value: f64) -> String {
        if value.fract() == 0.0 && value.abs() < 1e15 {
            return format!("{value:.0}");
        }
        let text = format!("{value:.10}");
        text.trim_end_matches('0').trim_end_matches('.').replace('.', ",")
    }

    struct Parser {
        chars: Vec<char>,
        pos: usize,
    }

    impl Parser {
        fn peek(&self) -> Option<char> {
            self.chars.get(self.pos).copied()
        }

        fn eat(&mut self, options: &str) -> Option<char> {
            let c = self.peek().filter(|c| options.contains(*c))?;
            self.pos += 1;
            Some(c)
        }

        fn expr(&mut self) -> Option<f64> {
            let mut value = self.term()?;
            while let Some(op) = self.eat("+-") {
                let rhs = self.term()?;
                value = if op == '+' { value + rhs } else { value - rhs };
            }
            Some(value)
        }

        fn term(&mut self) -> Option<f64> {
            let mut value = self.power()?;
            while let Some(op) = self.eat("*/×÷:") {
                let rhs = self.power()?;
                value = if op == '*' || op == '×' { value * rhs } else { value / rhs };
            }
            Some(value)
        }

        /// Right-associative, binds tighter than unary minus on its left: -2^2 = -4.
        fn power(&mut self) -> Option<f64> {
            if self.eat("-").is_some() {
                return Some(-self.power()?);
            }
            let base = self.primary()?;
            if self.eat("^").is_some() {
                return Some(base.powf(self.power()?));
            }
            Some(base)
        }

        fn primary(&mut self) -> Option<f64> {
            if self.eat("(").is_some() {
                let value = self.expr()?;
                self.eat(")")?;
                return Some(value);
            }
            let start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '.' || c == ',') {
                self.pos += 1;
            }
            let number: String = self.chars[start..self.pos].iter().map(|&c| if c == ',' { '.' } else { c }).collect();
            number.parse().ok()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{evaluate, format};

        #[test]
        fn arithmetic() {
            assert_eq!(evaluate("2+2*3"), Some(8.0));
            assert_eq!(evaluate("(2+2)*3"), Some(12.0));
            assert_eq!(evaluate("2^3^2"), Some(512.0));
            assert_eq!(evaluate("-2^2"), Some(-4.0));
            assert_eq!(evaluate("10 / 4"), Some(2.5));
            assert_eq!(evaluate("1,5×2"), Some(3.0));
        }

        #[test]
        fn not_calculations() {
            assert_eq!(evaluate("42"), None);
            assert_eq!(evaluate("discord"), None);
            assert_eq!(evaluate("2+"), None);
            assert_eq!(evaluate("1/0"), None);
            assert_eq!(evaluate("c++"), None);
        }

        #[test]
        fn formatting() {
            assert_eq!(format(8.0), "8");
            assert_eq!(format(2.5), "2,5");
            assert_eq!(format(1.0 / 3.0), "0,3333333333");
        }
    }
}
