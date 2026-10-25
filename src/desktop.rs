//! Desktop widgets (weather, news) lying on the wallpaper, under every normal
//! window. Each widget is its own small layered window kept just above
//! Explorer's desktop in the Z-order. They can be dragged anywhere; positions
//! are saved to %APPDATA%\CustomUI\desktop.tsv.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{w, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::macmenu::{Anchor, Menu};
use crate::{apps, config, spotlight};
use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use crate::net::{self, Location};

const WEATHER_W: f32 = 330.0;
const WEATHER_H: f32 = 158.0;
const NEWS_W: f32 = 330.0;
const NEWS_HEADER: f32 = 40.0;
const NEWS_ROW: f32 = 46.0;
const WIDGET_RADIUS: f32 = 22.0;
const MARGIN: f32 = 24.0;
const SPACING: f32 = 16.0;
const DRAG_THRESHOLD: i32 = 4;
const GLASS_ALPHA: u32 = 205;

const TIMER_SINK: usize = 1;

const COLOR_GLASS: COLORREF = rgb(24, 24, 28);
const COLOR_ROW_HOVER: COLORREF = rgb(52, 52, 60);
const COLOR_SKY_TEXT: COLORREF = rgb(222, 232, 248);
/// Never drawn: makes `shape_alpha` treat every pixel as opaque.
const COLOR_NONE: COLORREF = rgb(1, 2, 3);

const CMD_REMOVE: usize = 1;
const CMD_REFRESH: usize = 2;

const WEEKDAYS: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];

#[derive(Clone, Copy, PartialEq)]
pub enum Widget {
    Weather,
    News,
}

impl Widget {
    /// Its on/off key in the settings.
    fn setting(self) -> &'static str {
        match self {
            Widget::Weather => "widget.weather",
            Widget::News => "widget.news",
        }
    }
}

struct Item {
    widget: Widget,
    /// Window position in screen pixels.
    x: i32,
    y: i32,
    hwnd: HWND,
    /// Hovered headline (news widget).
    hover: Option<usize>,
}

struct Press {
    hwnd: HWND,
    start: POINT,
    origin: POINT,
    dragging: bool,
}

#[derive(Default)]
struct Fonts {
    small: HFONT,
    normal: HFONT,
    bold: HFONT,
    temperature: HFONT,
    symbol: HFONT,
}

#[derive(Default)]
struct State {
    controller: HWND,
    items: Vec<Item>,
    location: Option<Location>,
    feed: String,
    press: Option<Press>,
    tracking: Option<HWND>,
    fonts: Fonts,
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
        let controller = gfx::create_window(w!("CustomUI.Desktop"), Some(controller_proc), WS_EX_TOOLWINDOW)?;
        let size = |v: f32| -px(v);
        let fonts = Fonts {
            small: gfx::create_font(size(12.0), FW_NORMAL, gfx::ui_font()),
            normal: gfx::create_font(size(13.0), FW_NORMAL, gfx::ui_font()),
            bold: gfx::create_font(size(14.0), FW_SEMIBOLD, gfx::ui_font()),
            temperature: gfx::create_font(size(46.0), FW_LIGHT, gfx::DISPLAY_FONT),
            symbol: gfx::create_font(size(17.0), FW_NORMAL, w!("Segoe UI Symbol")),
        };
        let (mut items, location, feed) = load();
        // The settings decide which widgets exist; the file only remembers where they sit.
        items.retain(|i| config::flag(i.widget.setting()));
        let missing: Vec<Widget> = [Widget::Weather, Widget::News]
            .into_iter()
            .filter(|w| config::flag(w.setting()) && !items.iter().any(|i| i.widget == *w))
            .collect();
        let feed = Some(config::text("news.feed")).filter(|f| !f.trim().is_empty()).unwrap_or(feed);
        STATE.with_borrow_mut(|s| {
            s.controller = controller;
            s.fonts = fonts;
            s.items = items;
            s.location = location.clone();
            s.feed = feed.clone();
        });
        let count = STATE.with_borrow(|s| s.items.len());
        for index in 0..count {
            spawn(index)?;
        }
        for widget in missing {
            add_widget(widget);
        }
        SetTimer(controller, TIMER_SINK, 1000, None);
        net::start(controller, location, feed, config::text("weather.city"));
    }
    Ok(())
}

pub fn destroy() {
    let windows: Vec<HWND> = STATE.with_borrow_mut(|s| {
        let mut list: Vec<HWND> = s.items.drain(..).map(|i| i.hwnd).collect();
        list.push(s.controller);
        list
    });
    unsafe {
        for hwnd in windows.into_iter().filter(|h| !h.is_invalid()) {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Creates the window for item `index` and shows it.
unsafe fn spawn(index: usize) -> Result<()> {
    let hwnd = gfx::create_window(
        w!("CustomUI.DesktopItem"),
        Some(item_proc),
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
    )?;
    STATE.with_borrow_mut(|s| s.items[index].hwnd = hwnd);
    render_item(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    sink(hwnd);
    Ok(())
}

pub fn has_widget(widget: Widget) -> bool {
    STATE.with_borrow(|s| s.items.iter().any(|i| i.widget == widget))
}

pub fn toggle_widget(widget: Widget) {
    let existing = STATE.with_borrow(|s| s.items.iter().find(|i| i.widget == widget).map(|i| i.hwnd));
    unsafe {
        match existing {
            Some(hwnd) => remove_item(hwnd),
            None => {
                add_widget(widget);
                config::store_flag(widget.setting(), true);
                net::refresh();
            }
        }
    }
}

fn add_widget(widget: Widget) {
    let (x, y) = free_spot(widget);
    let index = STATE.with_borrow_mut(|s| {
        s.items.push(Item { widget, x, y, hwnd: HWND::default(), hover: None });
        s.items.len() - 1
    });
    let _ = unsafe { spawn(index) };
    STATE.with_borrow(save);
}

unsafe fn remove_item(hwnd: HWND) {
    let widget = STATE.with_borrow_mut(|s| {
        let widget = s.items.iter().find(|i| i.hwnd == hwnd).map(|i| i.widget);
        s.items.retain(|i| i.hwnd != hwnd);
        save(s);
        widget
    });
    if let Some(widget) = widget {
        config::store_flag(widget.setting(), false);
    }
    let _ = DestroyWindow(hwnd);
}

fn work_area() -> RECT {
    let mut work = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    work
}

/// Top-right corner, below any widget already there.
fn free_spot(widget: Widget) -> (i32, i32) {
    let work = work_area();
    let (width, _) = widget_size(widget);
    let x = work.right - px(MARGIN) - width;
    let bottom = STATE.with_borrow(|s| s.items.iter().map(|i| i.y + widget_size(i.widget).1).max());
    (x, bottom.map_or(work.top + px(MARGIN), |b| b + px(SPACING)))
}

// ---- keeping windows on the desktop ----

/// The window that hosts the desktop icons (Progman, or a WorkerW).
pub unsafe fn desktop_host() -> HWND {
    unsafe extern "system" fn find(hwnd: HWND, lparam: LPARAM) -> BOOL {
        if FindWindowExW(hwnd, None, w!("SHELLDLL_DefView"), None).is_ok() {
            *(lparam.0 as *mut HWND) = hwnd;
            return false.into();
        }
        true.into()
    }
    let mut host = HWND::default();
    let _ = EnumWindows(Some(find), LPARAM(&mut host as *mut HWND as isize));
    host
}

/// Where to insert a window so it sits directly above the desktop host;
/// `None` if it already does.
pub unsafe fn insert_point(hwnd: HWND) -> Option<HWND> {
    let host = desktop_host();
    if host.is_invalid() {
        return None;
    }
    match GetWindow(host, GW_HWNDPREV) {
        Ok(above) if above == hwnd => None,
        Ok(above) => Some(above),
        Err(_) => Some(HWND_TOP),
    }
}

/// True when `hwnd` is somewhere above the desktop host (i.e. visible on it).
pub unsafe fn above_desktop(hwnd: HWND, host: HWND) -> bool {
    let mut next = GetWindow(hwnd, GW_HWNDNEXT);
    while let Ok(window) = next {
        if window == host {
            return true;
        }
        next = GetWindow(window, GW_HWNDNEXT);
    }
    false
}

pub unsafe fn sink(hwnd: HWND) {
    if let Some(after) = insert_point(hwnd) {
        let _ = SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
    }
}

// ---- persistence ----

fn file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("CustomUI").join("desktop.tsv"))
}

/// Tab-separated lines: `weather x y`, `news x y`, `location lat lon city`,
/// `feed url`. Without a file: weather and news, top right.
fn load() -> (Vec<Item>, Option<Location>, String) {
    let mut feed = net::DEFAULT_FEED.to_owned();
    let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else {
        let work = work_area();
        let (w, wh) = widget_size(Widget::Weather);
        let x = work.right - px(MARGIN) - w;
        let y = work.top + px(MARGIN);
        let item = |widget, y| Item { widget, x, y, hwnd: HWND::default(), hover: None };
        let items = vec![item(Widget::Weather, y), item(Widget::News, y + wh + px(SPACING))];
        return (items, None, feed);
    };
    let mut items = Vec::new();
    let mut location = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let number = |i: usize| fields.get(i).and_then(|f| f.parse::<i32>().ok()).unwrap_or(0);
        let item = |widget| Item { widget, x: number(1), y: number(2), hwnd: HWND::default(), hover: None };
        match fields.first().copied() {
            Some("weather") => items.push(item(Widget::Weather)),
            Some("news") => items.push(item(Widget::News)),
            Some("location") => {
                let float = |i: usize| fields.get(i).and_then(|f| f.parse::<f64>().ok());
                if let (Some(lat), Some(lon)) = (float(1), float(2)) {
                    location = Some(Location { lat, lon, city: fields.get(3).copied().unwrap_or("").to_owned() });
                }
            }
            Some("feed") => {
                if let Some(url) = fields.get(1).filter(|u| !u.is_empty()) {
                    feed = (*url).to_owned();
                }
            }
            // Anything else (like the stacks of older versions) is dropped.
            _ => {}
        }
    }
    (items, location, feed)
}

fn save(s: &State) {
    let Some(path) = file() else { return };
    let mut text = String::new();
    for item in &s.items {
        let name = match item.widget {
            Widget::Weather => "weather",
            Widget::News => "news",
        };
        text += &format!("{name}\t{}\t{}\n", item.x, item.y);
    }
    if let Some(l) = &s.location {
        text += &format!("location\t{}\t{}\t{}\n", l.lat, l.lon, l.city.replace(['\t', '\n'], " "));
    }
    text += &format!("feed\t{}\n", s.feed);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

// ---- drawing ----

/// Headlines the news widget shows, as set.
fn news_rows() -> usize {
    config::int("news.rows") as usize
}

fn widget_size(widget: Widget) -> (i32, i32) {
    let (w, h) = match widget {
        Widget::Weather => (WEATHER_W, WEATHER_H),
        Widget::News => (NEWS_W, NEWS_HEADER + NEWS_ROW * news_rows() as f32 + 12.0),
    };
    (px(w), px(h))
}

unsafe fn render_item(hwnd: HWND) {
    STATE.with_borrow(|s| {
        let Some(item) = s.items.iter().find(|i| i.hwnd == hwnd) else { return };
        let (w, h) = widget_size(item.widget);
        let Some(mut canvas) = Canvas::new(w, h) else { return };
        match item.widget {
            Widget::Weather => draw_weather(&mut canvas, &s.fonts, net::weather().as_ref()),
            Widget::News => draw_news(&mut canvas, &s.fonts, net::news().as_ref(), item.hover),
        }
        canvas.present(hwnd, item.x, item.y, 255);
    });
}

fn render_widgets() {
    let windows: Vec<HWND> = STATE.with_borrow(|s| s.items.iter().map(|i| i.hwnd).collect());
    for hwnd in windows {
        unsafe { render_item(hwnd) };
    }
}

fn sky(weather: Option<&net::Weather>) -> (COLORREF, COLORREF) {
    match weather {
        None => (rgb(110, 120, 140), rgb(70, 78, 96)),
        Some(w) if !w.is_day => (rgb(38, 50, 98), rgb(14, 18, 46)),
        Some(w) => match w.code {
            2 | 3 | 45 | 48 => (rgb(118, 136, 160), rgb(72, 88, 112)),
            51..=67 | 80..=82 | 95..=99 => (rgb(88, 106, 134), rgb(46, 58, 84)),
            71..=77 | 85 | 86 => (rgb(150, 170, 198), rgb(96, 116, 148)),
            _ => (rgb(76, 152, 234), rgb(32, 96, 192)),
        },
    }
}

fn degrees(value: f64) -> String {
    // Avoid "-0°".
    let rounded = value.round();
    format!("{}°", if rounded == 0.0 { 0.0 } else { rounded })
}

/// macOS-style weather widget: sky gradient, city, big temperature, conditions,
/// and a five-day forecast on the right.
unsafe fn draw_weather(canvas: &mut Canvas, fonts: &Fonts, weather: Option<&net::Weather>) {
    let dc = canvas.dc;
    let client = RECT { left: 0, top: 0, right: canvas.width, bottom: canvas.height };
    let (top, bottom) = sky(weather);
    gfx::fill_gradient(dc, &client, top, bottom);
    SetTextColor(dc, COLOR_TEXT);

    match weather {
        None => {
            SelectObject(dc, fonts.normal);
            gfx::draw_text(dc, "Погода загружается…", &client, DT_CENTER);
        }
        Some(w) => {
            let left = px(18.0);
            let column = canvas.width / 2 - px(6.0);
            let row = |y: f32, h: f32| RECT { left, top: px(y), right: column, bottom: px(y + h) };
            SelectObject(dc, fonts.bold);
            gfx::draw_text(dc, &w.city, &row(14.0, 20.0), DT_LEFT | DT_END_ELLIPSIS);
            SelectObject(dc, fonts.temperature);
            gfx::draw_text(dc, &degrees(w.temperature), &row(34.0, 58.0), DT_LEFT);
            let (description, _) = net::describe(w.code, w.is_day);
            SelectObject(dc, fonts.normal);
            gfx::draw_text(dc, description, &row(96.0, 20.0), DT_LEFT | DT_END_ELLIPSIS);
            SelectObject(dc, fonts.small);
            SetTextColor(dc, COLOR_SKY_TEXT);
            gfx::draw_text(dc, &format!("Макс. {}, мин. {}", degrees(w.high), degrees(w.low)), &row(118.0, 18.0), DT_LEFT | DT_END_ELLIPSIS);

            let right = canvas.width - px(16.0);
            for (i, day) in w.days.iter().take(5).enumerate() {
                let y = px(16.0) + i as i32 * px(26.0);
                let line = RECT { left: column + px(10.0), top: y, right, bottom: y + px(24.0) };
                SelectObject(dc, fonts.normal);
                SetTextColor(dc, COLOR_TEXT);
                let name = if i == 0 { "Сегодня" } else { WEEKDAYS[day.weekday as usize % 7] };
                gfx::draw_text(dc, name, &line, DT_LEFT);
                SelectObject(dc, fonts.symbol);
                let (_, glyph) = net::describe(day.code, true);
                gfx::draw_text(dc, glyph, &RECT { left: line.left + px(62.0), right: line.left + px(88.0), ..line }, DT_CENTER);
                SelectObject(dc, fonts.small);
                SetTextColor(dc, COLOR_SKY_TEXT);
                gfx::draw_text(dc, &format!("{} / {}", degrees(day.high), degrees(day.low)), &line, DT_RIGHT);
            }
        }
    }
    canvas.shape_alpha(COLOR_NONE, 255, &[(client, WIDGET_RADIUS * scale())]);
}

fn news_row(index: usize, width: i32) -> RECT {
    let top = px(NEWS_HEADER) + index as i32 * px(NEWS_ROW);
    RECT { left: px(8.0), top, right: width - px(8.0), bottom: top + px(NEWS_ROW) - px(2.0) }
}

/// Dark glass card: header with the source, a few headlines with their time.
unsafe fn draw_news(canvas: &mut Canvas, fonts: &Fonts, news: Option<&net::News>, hover: Option<usize>) {
    let dc = canvas.dc;
    let client = RECT { left: 0, top: 0, right: canvas.width, bottom: canvas.height };
    gfx::fill(dc, &client, COLOR_GLASS);
    let header = RECT { left: px(18.0), top: px(12.0), right: canvas.width - px(18.0), bottom: px(36.0) };
    SelectObject(dc, fonts.bold);
    SetTextColor(dc, COLOR_TEXT);
    gfx::draw_text(dc, "Новости", &header, DT_LEFT);

    match news {
        None => {
            SelectObject(dc, fonts.normal);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, "Загружаю новости…", &client, DT_CENTER);
        }
        Some(news) => {
            SelectObject(dc, fonts.small);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, &news.source, &header, DT_RIGHT);
            for (i, headline) in news.items.iter().take(news_rows()).enumerate() {
                let r = news_row(i, canvas.width);
                if hover == Some(i) {
                    gfx::fill_round(dc, &r, COLOR_ROW_HOVER, px(24.0));
                }
                SelectObject(dc, fonts.small);
                SetTextColor(dc, COLOR_DIM);
                gfx::draw_text(dc, &headline.time, &RECT { left: r.left + px(10.0), top: r.top + px(4.0), right: r.left + px(50.0), bottom: r.top + px(20.0) }, DT_LEFT);
                SelectObject(dc, fonts.normal);
                SetTextColor(dc, COLOR_TEXT);
                gfx::draw_text_wrapped(dc, &headline.title, &RECT { left: r.left + px(52.0), top: r.top + px(4.0), right: r.right - px(10.0), bottom: r.bottom - px(4.0) });
            }
        }
    }
    canvas.shape_alpha(COLOR_GLASS, GLASS_ALPHA, &[(client, WIDGET_RADIUS * scale())]);
}

// ---- input ----

fn screen_cursor() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_SINK => {
            // "Show desktop" and Explorer restarts can leave widgets under the desktop.
            let host = desktop_host();
            let windows: Vec<HWND> = STATE.with_borrow(|s| s.items.iter().map(|i| i.hwnd).collect());
            for item in windows {
                if !host.is_invalid() && !above_desktop(item, host) {
                    sink(item);
                }
            }
        }
        net::WM_NET_UPDATE => {
            if let Some(found) = net::discovered_location() {
                STATE.with_borrow_mut(|s| {
                    s.location = Some(found);
                    save(s);
                });
            }
            render_widgets();
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

unsafe extern "system" fn item_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let local = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_WINDOWPOSCHANGING => {
            // Stay just above the desktop whatever tries to raise us.
            let pos = &mut *(lparam.0 as *mut WINDOWPOS);
            if pos.flags.0 & SWP_NOZORDER.0 == 0 {
                match insert_point(hwnd) {
                    Some(after) => pos.hwndInsertAfter = after,
                    None => pos.flags |= SWP_NOZORDER,
                }
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_LBUTTONDOWN => {
            let origin = STATE.with_borrow(|s| s.items.iter().find(|i| i.hwnd == hwnd).map(|i| POINT { x: i.x, y: i.y }));
            if let Some(origin) = origin {
                STATE.with_borrow_mut(|s| s.press = Some(Press { hwnd, start: screen_cursor(), origin, dragging: false }));
                SetCapture(hwnd);
            }
        }
        WM_MOUSEMOVE => on_item_move(hwnd, local()),
        WM_LBUTTONUP => {
            let press = STATE.with_borrow_mut(|s| s.press.take());
            let _ = ReleaseCapture();
            match press {
                Some(p) if p.dragging => STATE.with_borrow(save),
                Some(_) => on_item_click(hwnd, local()),
                None => {}
            }
        }
        WM_CAPTURECHANGED => {
            if STATE.with_borrow_mut(|s| s.press.take()).is_some_and(|p| p.dragging) {
                STATE.with_borrow(save);
            }
        }
        WM_RBUTTONUP => {
            let _ = item_menu(hwnd);
        }
        gfx::WM_MOUSELEAVE => {
            let changed = STATE.with_borrow_mut(|s| {
                s.tracking = None;
                s.items.iter_mut().find(|i| i.hwnd == hwnd).is_some_and(|i| i.hover.take().is_some())
            });
            if changed {
                render_item(hwnd);
            }
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

unsafe fn on_item_move(hwnd: HWND, local: POINT) {
    let cursor = screen_cursor();
    let moved = STATE.with_borrow_mut(|s| {
        let press = s.press.as_mut().filter(|p| p.hwnd == hwnd)?;
        let (dx, dy) = (cursor.x - press.start.x, cursor.y - press.start.y);
        if !press.dragging && dx.abs() <= DRAG_THRESHOLD && dy.abs() <= DRAG_THRESHOLD {
            return None;
        }
        press.dragging = true;
        let (x, y) = (press.origin.x + dx, press.origin.y + dy);
        let item = s.items.iter_mut().find(|i| i.hwnd == hwnd)?;
        (item.x, item.y) = (x, y);
        Some((x, y))
    });
    if let Some((x, y)) = moved {
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        return;
    }

    // Hover feedback for headlines.
    if STATE.with_borrow(|s| s.tracking != Some(hwnd)) {
        STATE.with_borrow_mut(|s| s.tracking = Some(hwnd));
        let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
        let _ = TrackMouseEvent(&mut tme);
    }
    let changed = STATE.with_borrow_mut(|s| {
        let Some(item) = s.items.iter_mut().find(|i| i.hwnd == hwnd) else { return false };
        let hover = match item.widget {
            Widget::News => headline_at(widget_size(item.widget).0, local),
            Widget::Weather => None,
        };
        std::mem::replace(&mut item.hover, hover) != hover
    });
    if changed {
        render_item(hwnd);
    }
}

fn headline_at(width: i32, p: POINT) -> Option<usize> {
    let count = net::news().map_or(0, |n| n.items.len().min(news_rows()));
    (0..count).find(|&i| {
        let r = news_row(i, width);
        p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    })
}

unsafe fn on_item_click(hwnd: HWND, local: POINT) {
    let Some(widget) = STATE.with_borrow(|s| s.items.iter().find(|i| i.hwnd == hwnd).map(|i| i.widget)) else { return };
    match widget {
        Widget::Weather => {
            let city = net::weather().map(|w| w.city).unwrap_or_default();
            apps::open(&spotlight::web_search_url(&format!("погода {city}")));
        }
        Widget::News => {
            let link = headline_at(widget_size(widget).0, local).and_then(|i| net::news().and_then(|n| n.items.get(i).map(|h| h.link.clone())));
            if let Some(link) = link.filter(|l| !l.is_empty()) {
                apps::open(&link);
            }
        }
    }
}

unsafe fn item_menu(hwnd: HWND) -> Result<()> {
    let mut menu = Menu::new();
    menu.item(CMD_REFRESH, "Обновить").separator().item(CMD_REMOVE, "Удалить виджет");
    let cmd = menu.show(Anchor::At(screen_cursor()));
    match cmd {
        CMD_REMOVE => remove_item(hwnd),
        CMD_REFRESH => net::refresh(),
        _ => {}
    }
    Ok(())
}
