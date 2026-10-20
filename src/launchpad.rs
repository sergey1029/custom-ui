//! All apps, full screen, over a blurred and dimmed copy of the screen. Two
//! looks, switchable from the right-click menu and remembered:
//! - like macOS Launchpad: pages of a 7×5 grid of large icons with names,
//!   page dots below, a search field on top; the wheel or arrows turn pages;
//! - like the Apple Watch app grid: a honeycomb of round icons that scrolls
//!   vertically.
//!
//! No motion beyond that. Typing filters either one. Opened from the dock's
//! round launcher button.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::macmenu::{Anchor, Menu};
use crate::apps::{self, App};
use crate::dock::{self, Pin};
use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use crate::{config, icons};

// Grid (macOS) style.
const GRID_COLUMNS: usize = 7;
const GRID_ROWS: usize = 5;
const GRID_ICON: f32 = 80.0;
/// Side margins as a share of the screen width, like Launchpad's.
const GRID_SIDE: f32 = 0.12;
const GRID_TOP: f32 = 110.0;
const GRID_BOTTOM: f32 = 96.0;
const DOT: f32 = 8.0;
const DOT_GAP: f32 = 12.0;

// Honeycomb (Apple Watch) style.
const DISC: f32 = 84.0;
const DISC_ICON: f32 = 52.0;
/// Center-to-center distance within a row; rows are √3/2 of it apart, like hexagons.
const STEP: f32 = 104.0;
const SIDE_MARGIN: f32 = 80.0;
const TOP: f32 = 96.0;
const BOTTOM: f32 = 64.0;
const LABEL_H: f32 = 24.0;

const SEARCH_W: f32 = 260.0;
const SEARCH_H: f32 = 34.0;
/// How much brightness the blurred backdrop keeps.
const BACKDROP_DARKEN: f32 = 0.5;
/// Threads extracting icons in the background.
const LOADERS: usize = 4;

/// Posted by the loader threads when icons are ready.
const WM_ICONS_READY: u32 = WM_APP + 7;

const TIMER_REPAINT: usize = 1;
const TIMER_WARMUP: usize = 2;
const TIMER_SLIDE: usize = 3;
/// Share of the remaining distance a page slide covers per frame (~250 ms overall).
const SLIDE_EASE: f32 = 0.22;
/// A drag this far (share of the screen width) turns the page on release.
const FLIP_DISTANCE: f32 = 0.12;
const DRAG_THRESHOLD: i32 = 6;
/// Touchpad swipes snap to a page after this much quiet.
const SWIPE_SETTLE: Duration = Duration::from_millis(140);

const COLOR_FALLBACK: COLORREF = rgb(18, 18, 22);
const COLOR_DISC: COLORREF = rgb(42, 42, 50);
const COLOR_DISC_HOVER: COLORREF = rgb(72, 72, 86);
const COLOR_FIELD: COLORREF = rgb(255, 255, 255);
const COLOR_HOVER: COLORREF = rgb(255, 255, 255);
const COLOR_LABEL: COLORREF = rgb(36, 36, 44);
const COLOR_DOT: COLORREF = rgb(255, 255, 255);

const CMD_OPEN: usize = 1;
const CMD_PIN: usize = 2;
const CMD_GRID: usize = 3;
const CMD_HONEYCOMB: usize = 4;

#[derive(Clone, Copy, PartialEq, Default)]
enum Style {
    #[default]
    Grid,
    Honeycomb,
}

impl Style {
    fn icon(self) -> f32 {
        match self {
            Style::Grid => GRID_ICON,
            Style::Honeycomb => DISC_ICON,
        }
    }
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    visible: bool,
    style: Style,
    apps: Vec<App>,
    query: String,
    /// Indices into `apps` matching the query, in grid order.
    shown: Vec<usize>,
    /// Grid style: the page being shown (or slid to)...
    page: usize,
    /// ...and where the pages actually are, in pages: it eases toward `page`
    /// and follows drags and touchpad swipes in between.
    offset: f32,
    /// Honeycomb style: vertical scroll.
    scroll: f32,
    /// Wheel movement not yet turned into a page flip (touchpads send small steps).
    wheel: i32,
    /// A press that may become a sideways drag: start x, offset then, moved yet.
    drag: Option<(i32, f32, bool)>,
    /// Last horizontal touchpad scroll; pages snap once it stops.
    swiped: Option<Instant>,
    /// Position in `shown`.
    hover: Option<usize>,
    /// The blurred screen captured when opening.
    backdrop: Vec<u32>,
    /// Icons handed to the loader threads already.
    requested: HashSet<(String, i32)>,
    repaint_pending: bool,
    previous: HWND,
    font: HFONT,
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
        let hwnd = gfx::create_window(w!("CustomUI.Launchpad"), Some(wndproc), WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED)?;
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.style = load_style();
            s.font = gfx::create_font(-px(13.0), FW_NORMAL, gfx::ui_font());
            s.title = gfx::create_font(-px(16.0), FW_SEMIBOLD, gfx::ui_font());
            s.glyphs = gfx::create_font(-px(13.0), FW_NORMAL, w!("Segoe Fluent Icons"));
        });
        start_loaders(hwnd);
        // Not right away: the shell itself is still starting up.
        SetTimer(hwnd, TIMER_WARMUP, 4000, None);
    }
    Ok(())
}

pub fn destroy() {
    let hwnd = STATE.with_borrow_mut(|s| std::mem::take(&mut s.hwnd));
    if !hwnd.is_invalid() {
        unsafe {
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
    let apps = apps::all();
    let area = screen();
    // Captured before we cover the screen.
    let backdrop = unsafe { gfx::blurred_screen(area.right - area.left, area.bottom - area.top, BACKDROP_DARKEN) }.unwrap_or_default();
    let hwnd = STATE.with_borrow_mut(|s| {
        s.apps = apps;
        s.backdrop = backdrop;
        s.query.clear();
        s.visible = true;
        filter(s);
        s.hwnd
    });
    request_shown_icons();
    unsafe {
        render();
        let _ = ShowWindow(hwnd, SW_SHOW);
        let previous = gfx::force_foreground(hwnd);
        STATE.with_borrow_mut(|s| s.previous = previous);
    }
}

fn hide() {
    let hwnd = STATE.with_borrow_mut(|s| {
        s.backdrop = Vec::new();
        if std::mem::replace(&mut s.visible, false) { s.hwnd } else { HWND::default() }
    });
    if !hwnd.is_invalid() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Matches by name; names or words starting with the query come first.
fn filter(s: &mut State) {
    let query = s.query.trim().to_lowercase();
    if query.is_empty() {
        s.shown = (0..s.apps.len()).collect();
    } else {
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
        s.shown = hits.into_iter().map(|(_, i)| i).collect();
    }
    s.scroll = 0.0;
    s.page = 0;
    s.offset = 0.0;
    s.wheel = 0;
    s.drag = None;
    s.hover = None;
}

fn screen() -> RECT {
    unsafe { RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) } }
}

// ---- grid (macOS) layout ----

const PER_PAGE: usize = GRID_COLUMNS * GRID_ROWS;

fn pages(count: usize) -> usize {
    count.div_ceil(PER_PAGE).max(1)
}

/// Cells of `page`, moved `shift` px sideways: (position in `shown`, cell rect).
fn grid_cells(count: usize, page: usize, shift: i32) -> Vec<(usize, RECT)> {
    let area = screen();
    let width = (area.right - area.left) as f32;
    let side = width * GRID_SIDE;
    let cell_w = (width - side * 2.0) / GRID_COLUMNS as f32;
    let top = GRID_TOP * scale();
    let cell_h = ((area.bottom - area.top) as f32 - top - GRID_BOTTOM * scale()) / GRID_ROWS as f32;
    (page * PER_PAGE..((page + 1) * PER_PAGE).min(count))
        .map(|i| {
            let slot = i - page * PER_PAGE;
            let (col, row) = ((slot % GRID_COLUMNS) as f32, (slot / GRID_COLUMNS) as f32);
            let left = side + col * cell_w + shift as f32;
            let y = top + row * cell_h;
            (i, RECT { left: left as i32, top: y as i32, right: (left + cell_w) as i32, bottom: (y + cell_h) as i32 })
        })
        .collect()
}

fn dot_rects(count: usize) -> Vec<RECT> {
    let area = screen();
    let n = pages(count);
    if n < 2 {
        return Vec::new();
    }
    let (dot, gap) = (px(DOT), px(DOT_GAP));
    let total = n as i32 * dot + (n as i32 - 1) * gap;
    let left = (area.right - total) / 2;
    let y = area.bottom - px(GRID_BOTTOM / 2.0) - dot / 2;
    (0..n as i32).map(|i| RECT { left: left + i * (dot + gap), top: y, right: left + i * (dot + gap) + dot, bottom: y + dot }).collect()
}

/// The icon square within a grid cell.
fn grid_icon_rect(cell: &RECT) -> RECT {
    let icon = px(GRID_ICON);
    let left = cell.left + (cell.right - cell.left - icon) / 2;
    let top = cell.top + (cell.bottom - cell.top - icon - px(30.0)) / 2;
    RECT { left, top, right: left + icon, bottom: top + icon }
}

// ---- honeycomb (Apple Watch) layout ----

/// Rows alternate between `wide` and `wide - 1` cells, so every other row sits
/// half a step over. Centers of every cell (scrolled), and the content height.
fn honeycomb(count: usize, scroll: f32) -> (Vec<POINT>, i32) {
    let area = screen();
    let step = STEP * scale();
    let row_step = step * 0.866;
    let usable = (area.right - area.left) as f32 - SIDE_MARGIN * 2.0 * scale();
    let wide = ((usable / step) as usize).max(2);
    let mid = (area.left + area.right) as f32 / 2.0;
    let mut centers = Vec::with_capacity(count);
    let (mut row, mut placed) = (0usize, 0usize);
    while placed < count {
        let full = if row % 2 == 0 { wide } else { wide - 1 };
        let in_row = full.min(count - placed);
        // Partial last rows stay aligned to the honeycomb rather than re-centering.
        let start = mid - (full as f32 - 1.0) * step / 2.0;
        let y = TOP * scale() + DISC * scale() / 2.0 + row as f32 * row_step - scroll * scale();
        for i in 0..in_row {
            centers.push(POINT { x: (start + i as f32 * step).round() as i32, y: y.round() as i32 });
        }
        placed += in_row;
        row += 1;
    }
    let content = (TOP * scale() + DISC * scale() + row.saturating_sub(1) as f32 * row_step + BOTTOM * scale()).round() as i32;
    (centers, content)
}

fn max_scroll(count: usize) -> f32 {
    let height = (screen().bottom - screen().top) as f32;
    ((honeycomb(count, 0.0).1 as f32 - height) / scale()).max(0.0)
}

fn disc_rect(center: POINT, diameter: i32) -> RECT {
    RECT { left: center.x - diameter / 2, top: center.y - diameter / 2, right: center.x + diameter / 2, bottom: center.y + diameter / 2 }
}

// ---- hit testing ----

/// Position in `shown` under a point.
fn app_at(s: &State, p: POINT) -> Option<usize> {
    let inside = |r: &RECT| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
    match s.style {
        // Nothing is clickable while pages are sliding.
        Style::Grid if (s.offset - s.page as f32).abs() > 0.01 => None,
        Style::Grid => grid_cells(s.shown.len(), s.page, 0).into_iter().find(|(_, cell)| inside(&grid_icon_rect(cell)) || inside(cell)).map(|(i, _)| i),
        Style::Honeycomb => {
            let radius = (DISC * scale() / 2.0) as i32;
            honeycomb(s.shown.len(), s.scroll).0.iter().position(|c| {
                let (dx, dy) = (p.x - c.x, p.y - c.y);
                dx * dx + dy * dy <= radius * radius
            })
        }
    }
}

fn dot_at(s: &State, p: POINT) -> Option<usize> {
    if s.style != Style::Grid {
        return None;
    }
    // Generous hit area around the small dots.
    let pad = px(6.0);
    dot_rects(s.shown.len()).iter().position(|r| p.x >= r.left - pad && p.x < r.right + pad && p.y >= r.top - pad && p.y < r.bottom + pad)
}

// ---- drawing ----

/// The app's icon, if loaded (they stream in from the loader threads).
fn icon_of(s: &State, position: usize) -> Option<std::rc::Rc<icons::Icon>> {
    icons::peek(&s.apps[s.shown[position]].parsing_name(), px(s.style.icon())).flatten()
}

unsafe fn render() {
    let area = screen();
    let (width, height) = (area.right - area.left, area.bottom - area.top);
    let Some(mut canvas) = Canvas::new(width, height) else { return };
    let dc = canvas.dc;
    let client = RECT { left: 0, top: 0, right: width, bottom: height };

    let hwnd = STATE.with_borrow(|s| {
        if s.backdrop.len() == (width * height) as usize {
            canvas.pixels().copy_from_slice(&s.backdrop);
        } else {
            gfx::fill(dc, &client, COLOR_FALLBACK);
        }
        match s.style {
            Style::Grid => draw_grid(&mut canvas, s, &client),
            Style::Honeycomb => draw_honeycomb(&mut canvas, s, &client),
        }
        s.hwnd
    });
    canvas.present(hwnd, area.left, area.top, 255);
}

/// GDI leaves alpha at zero where it drew; the backdrop is opaque everywhere.
fn make_opaque(canvas: &mut Canvas) {
    for p in canvas.pixels() {
        *p |= 0xFF00_0000;
    }
}

unsafe fn draw_search(canvas: &mut Canvas, s: &State, field: &RECT) {
    let dc = canvas.dc;
    // A frosted pill: the backdrop shows through, lightened.
    canvas.fill_shape(field, (field.bottom - field.top) as f32 / 2.0, COLOR_FIELD, 46);
    let text = RECT { left: field.left + px(34.0), right: field.right - px(14.0), ..*field };
    SelectObject(dc, s.glyphs);
    SetTextColor(dc, COLOR_DIM);
    gfx::draw_text(dc, "\u{E721}", &RECT { left: field.left + px(12.0), right: text.left, ..*field }, DT_LEFT);
    SelectObject(dc, s.font);
    if s.query.is_empty() {
        gfx::draw_text(dc, "Поиск", &text, DT_LEFT);
    } else {
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, &s.query, &text, DT_LEFT | DT_END_ELLIPSIS);
    }
}

unsafe fn draw_grid(canvas: &mut Canvas, s: &State, client: &RECT) {
    let dc = canvas.dc;
    let width = client.right;
    let field = RECT { left: (width - px(SEARCH_W)) / 2, top: px(36.0), right: (width + px(SEARCH_W)) / 2, bottom: px(36.0 + SEARCH_H) };
    draw_search(canvas, s, &field);

    // The page(s) under the current offset, each shifted by its distance from it.
    let count = pages(s.shown.len());
    let first = s.offset.floor().max(0.0) as usize;
    let cells: Vec<(usize, RECT)> = (first..(first + 2).min(count))
        .flat_map(|page| grid_cells(s.shown.len(), page, ((page as f32 - s.offset) * width as f32).round() as i32))
        .filter(|(_, cell)| cell.right > 0 && cell.left < width)
        .collect();
    let icon = px(GRID_ICON);
    for (i, cell) in &cells {
        let square = grid_icon_rect(cell);
        if s.hover == Some(*i) {
            let glow = RECT { left: square.left - px(10.0), top: square.top - px(10.0), right: square.right + px(10.0), bottom: square.bottom + px(10.0) };
            canvas.fill_shape(&glow, px(22.0) as f32, COLOR_HOVER, 30);
        }
        SelectObject(dc, s.font);
        SetTextColor(dc, COLOR_TEXT);
        let label = RECT { left: cell.left + px(6.0), top: square.bottom + px(8.0), right: cell.right - px(6.0), bottom: square.bottom + px(28.0) };
        gfx::draw_text(dc, &s.apps[s.shown[*i]].name, &label, DT_CENTER | DT_END_ELLIPSIS);
    }
    if s.shown.is_empty() {
        SetTextColor(dc, COLOR_DIM);
        gfx::draw_text(dc, "Ничего не найдено", &RECT { top: field.bottom + px(60.0), bottom: field.bottom + px(90.0), ..*client }, DT_CENTER);
    }
    make_opaque(canvas);

    for (i, cell) in &cells {
        if let Some(image) = icon_of(s, *i) {
            let square = grid_icon_rect(cell);
            canvas.draw_icon(&image, square.left, square.top, icon, None, 1.0);
        }
    }
    let current = s.offset.round().clamp(0.0, (count - 1) as f32) as usize;
    for (page, dot) in dot_rects(s.shown.len()).iter().enumerate() {
        let alpha = if page == current { 235 } else { 90 };
        canvas.fill_shape(dot, px(DOT) as f32 / 2.0, COLOR_DOT, alpha);
    }
}

unsafe fn draw_honeycomb(canvas: &mut Canvas, s: &State, client: &RECT) {
    let dc = canvas.dc;
    let width = client.right;
    let (centers, _) = honeycomb(s.shown.len(), s.scroll);

    // Header: a search field while typing, otherwise the count (it scrolls with the grid).
    let field = RECT { left: (width - px(SEARCH_W)) / 2, top: px(28.0), right: (width + px(SEARCH_W)) / 2, bottom: px(28.0 + SEARCH_H) };
    if s.query.is_empty() {
        let shift = px(s.scroll);
        let title = RECT { top: field.top - shift, bottom: field.bottom - shift, left: 0, right: width };
        SelectObject(dc, s.title);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, &format!("Все приложения · {}", s.apps.len()), &title, DT_CENTER);
    } else {
        draw_search(canvas, s, &field);
        if s.shown.is_empty() {
            SelectObject(dc, s.font);
            SetTextColor(dc, COLOR_DIM);
            gfx::draw_text(dc, "Ничего не найдено", &RECT { top: field.bottom + px(40.0), bottom: field.bottom + px(70.0), ..*client }, DT_CENTER);
        }
    }

    // The hovered app's name under its disc.
    if let Some(i) = s.hover.filter(|&i| i < centers.len()) {
        let center = centers[i];
        let name = &s.apps[s.shown[i]].name;
        SelectObject(dc, s.font);
        let w = (gfx::text_width(dc, name) + px(24.0)).min(px(STEP * 2.0));
        let top = center.y + px(DISC / 2.0 + 6.0);
        let label = RECT { left: center.x - w / 2, top, right: center.x + w / 2, bottom: top + px(LABEL_H) };
        gfx::fill_round(dc, &label, COLOR_LABEL, label.bottom - label.top);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, name, &RECT { left: label.left + px(10.0), right: label.right - px(10.0), ..label }, DT_CENTER | DT_END_ELLIPSIS);
    }
    make_opaque(canvas);

    let height = client.bottom;
    let (disc, icon) = (px(DISC), px(DISC_ICON));
    for (i, center) in centers.iter().enumerate() {
        if center.y + disc / 2 < 0 || center.y - disc / 2 > height {
            continue;
        }
        let color = if s.hover == Some(i) { COLOR_DISC_HOVER } else { COLOR_DISC };
        canvas.fill_shape(&disc_rect(*center, disc), disc as f32 / 2.0, color, 235);
        if let Some(image) = icon_of(s, i) {
            canvas.draw_icon(&image, center.x - icon / 2, center.y - icon / 2, icon, None, 1.0);
        }
    }
}

// ---- background icon loading ----
//
// ~170 shell icons take a second or two to extract one by one, so a few
// worker threads (each its own STA) produce the pixels and the UI thread only
// caches them.

type Job = (String, i32);
type Loaded = (String, i32, Option<(i32, Vec<u32>)>);

struct Loader {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    done: Mutex<Vec<Loaded>>,
}

static LOADER: OnceLock<Arc<Loader>> = OnceLock::new();

fn start_loaders(notify: HWND) {
    let loader = Arc::new(Loader { jobs: Mutex::new(VecDeque::new()), wake: Condvar::new(), done: Mutex::new(Vec::new()) });
    let _ = LOADER.set(loader.clone());
    let notify = notify.0 as isize;
    for _ in 0..LOADERS {
        let loader = loader.clone();
        std::thread::spawn(move || {
            unsafe {
                // STA like the UI thread: in an MTA the shell hands out generic icons for many apps.
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            loop {
                let (name, size) = {
                    let mut jobs = loader.jobs.lock().unwrap();
                    loop {
                        if let Some(job) = jobs.pop_front() {
                            break job;
                        }
                        jobs = loader.wake.wait(jobs).unwrap();
                    }
                };
                let pixels = icons::load_pixels(&name, size);
                loader.done.lock().unwrap().push((name, size, pixels));
                unsafe {
                    let _ = PostMessageW(HWND(notify as *mut _), WM_ICONS_READY, WPARAM(0), LPARAM(0));
                }
            }
        });
    }
}

/// Queues icons that aren't cached or queued yet, in the given order.
fn request_icons(names: impl IntoIterator<Item = String>, size: i32) {
    let Some(loader) = LOADER.get() else { return };
    let fresh: Vec<Job> = STATE.with_borrow_mut(|s| {
        names
            .into_iter()
            .filter(|n| icons::peek(n, size).is_none() && s.requested.insert((n.clone(), size)))
            .map(|n| (n, size))
            .collect()
    });
    if fresh.is_empty() {
        return;
    }
    loader.jobs.lock().unwrap().extend(fresh);
    loader.wake.notify_all();
}

/// The current list in order, so what's on screen first loads first.
fn request_shown_icons() {
    let (names, size) = STATE.with_borrow(|s| {
        let start = if s.style == Style::Grid { s.page * PER_PAGE } else { 0 };
        let names: Vec<String> = s.shown.iter().skip(start).chain(s.shown.iter().take(start)).map(|&i| s.apps[i].parsing_name()).collect();
        (names, px(s.style.icon()))
    });
    request_icons(names, size);
}

/// Caches finished icons; repaints at most every few frames while they stream in.
unsafe fn take_icons(hwnd: HWND) {
    let Some(loader) = LOADER.get() else { return };
    let done = std::mem::take(&mut *loader.done.lock().unwrap());
    for (name, size, pixels) in done {
        icons::insert(&name, size, pixels);
    }
    if STATE.with_borrow_mut(|s| s.visible && !std::mem::replace(&mut s.repaint_pending, true)) {
        SetTimer(hwnd, TIMER_REPAINT, 30, None);
    }
}

// ---- input ----

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_ICONS_READY => take_icons(hwnd),
        WM_TIMER if wparam.0 == TIMER_REPAINT => {
            let _ = KillTimer(hwnd, TIMER_REPAINT);
            STATE.with_borrow_mut(|s| s.repaint_pending = false);
            if STATE.with_borrow(|s| s.visible) {
                render();
            }
        }
        WM_TIMER if wparam.0 == TIMER_WARMUP => {
            // Fill the cache in the background so the first opening is instant.
            let _ = KillTimer(hwnd, TIMER_WARMUP);
            let size = STATE.with_borrow(|s| px(s.style.icon()));
            request_icons(apps::all().iter().map(App::parsing_name), size);
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => hide(),
        WM_KEYDOWN => on_key(VIRTUAL_KEY(wparam.0 as u16)),
        WM_CHAR => on_char(wparam.0 as u32),
        WM_TIMER if wparam.0 == TIMER_SLIDE => slide_tick(hwnd),
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16 & 0xFFFF) as i16 as i32;
            on_wheel(-delta);
        }
        // Touchpad swipes (and tilt wheels): pages follow, then snap.
        WM_MOUSEHWHEEL => {
            let delta = (wparam.0 >> 16 & 0xFFFF) as i16 as i32;
            on_swipe(hwnd, delta);
        }
        WM_LBUTTONDOWN => {
            if STATE.with_borrow(|s| s.style == Style::Grid) {
                let x = point().x;
                STATE.with_borrow_mut(|s| s.drag = Some((x, s.offset, false)));
                SetCapture(hwnd);
            }
        }
        WM_MOUSEMOVE => {
            if on_drag(point().x) {
                return LRESULT(0);
            }
            let hover = STATE.with_borrow(|s| app_at(s, point()));
            if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
                render();
            }
        }
        WM_LBUTTONUP => {
            let drag = STATE.with_borrow_mut(|s| s.drag.take());
            let _ = ReleaseCapture();
            if let Some((_, start, true)) = drag {
                finish_drag(start);
                return LRESULT(0);
            }
            let p = point();
            let (app, dot) = STATE.with_borrow(|s| (app_at(s, p), dot_at(s, p)));
            match (app, dot) {
                (Some(i), _) => launch(i),
                (None, Some(page)) => go_to_page(page),
                // Like Launchpad: a click on the background closes it.
                (None, None) => {
                    hide();
                    gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
                }
            }
        }
        WM_CAPTURECHANGED => {
            if let Some((_, start, true)) = STATE.with_borrow_mut(|s| s.drag.take()) {
                finish_drag(start);
            }
        }
        WM_RBUTTONUP => {
            let app = STATE.with_borrow(|s| app_at(s, point()));
            let _ = menu(app);
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

/// `forward` in wheel units (120 per notch).
fn on_wheel(forward: i32) {
    match STATE.with_borrow(|s| s.style) {
        Style::Honeycomb => scroll_by(forward as f32 / 120.0 * STEP),
        Style::Grid => {
            // One page per notch; small touchpad steps add up first.
            let flip = STATE.with_borrow_mut(|s| {
                s.wheel += forward;
                let flip = s.wheel / 120;
                s.wheel %= 120;
                flip
            });
            if flip != 0 {
                let page = STATE.with_borrow(|s| s.page as i32 + flip.signum());
                go_to_page(page.max(0) as usize);
            }
        }
    }
}

/// Slides to `page` (clamped to the existing ones).
fn go_to_page(page: usize) {
    let (hwnd, changed) = STATE.with_borrow_mut(|s| {
        let page = page.min(pages(s.shown.len()) - 1);
        s.hover = None;
        (s.hwnd, std::mem::replace(&mut s.page, page) != page)
    });
    if changed {
        request_shown_icons();
    }
    unsafe {
        SetTimer(hwnd, TIMER_SLIDE, 15, None);
    }
}

/// Eases the pages toward `page`, and snaps a finished touchpad swipe.
unsafe fn slide_tick(hwnd: HWND) {
    let swipe_over = STATE.with_borrow(|s| s.drag.is_none() && s.swiped.is_some_and(|t| t.elapsed() >= SWIPE_SETTLE));
    if swipe_over {
        STATE.with_borrow_mut(|s| s.swiped = None);
        let target = STATE.with_borrow(|s| flip_target(s, s.page as f32));
        go_to_page(target);
    }
    let settled = STATE.with_borrow_mut(|s| {
        if s.drag.is_some() || s.swiped.is_some() {
            return false;
        }
        let distance = s.page as f32 - s.offset;
        if distance.abs() < 0.002 {
            s.offset = s.page as f32;
            true
        } else {
            s.offset += distance * SLIDE_EASE;
            false
        }
    });
    render();
    if settled {
        let _ = KillTimer(hwnd, TIMER_SLIDE);
    }
}

/// The page to settle on after the pages were pulled from `from`: the next one
/// in the pulled direction once the pull passed FLIP_DISTANCE, else back.
fn flip_target(s: &State, from: f32) -> usize {
    let pulled = s.offset - from;
    let base = from.round() as i32;
    let target = if pulled > FLIP_DISTANCE {
        base + 1
    } else if pulled < -FLIP_DISTANCE {
        base - 1
    } else {
        base
    };
    target.clamp(0, pages(s.shown.len()) as i32 - 1) as usize
}

/// Past the first or last page the pages only follow a third of the way.
fn rubber_band(offset: f32, last: f32) -> f32 {
    if offset < 0.0 {
        offset / 3.0
    } else if offset > last {
        last + (offset - last) / 3.0
    } else {
        offset
    }
}

/// Mouse move during a press: drags the pages sideways once it moved enough.
/// True while dragging.
fn on_drag(x: i32) -> bool {
    let width = (screen().right - screen().left) as f32;
    let dragging = STATE.with_borrow_mut(|s| {
        let Some((start_x, start_offset, moved)) = s.drag.as_mut() else { return false };
        let dx = x - *start_x;
        if !*moved && dx.abs() <= DRAG_THRESHOLD {
            return false;
        }
        *moved = true;
        let last = (pages(s.shown.len()) - 1) as f32;
        let start_offset = *start_offset;
        s.offset = rubber_band(start_offset - dx as f32 / width, last);
        s.hover = None;
        true
    });
    if dragging {
        unsafe { render() };
    }
    dragging
}

fn finish_drag(start: f32) {
    let target = STATE.with_borrow(|s| flip_target(s, start));
    go_to_page(target);
}

/// Horizontal touchpad scroll, 120 units ≈ a fifth of a page.
unsafe fn on_swipe(hwnd: HWND, delta: i32) {
    if STATE.with_borrow(|s| s.style != Style::Grid) {
        return;
    }
    STATE.with_borrow_mut(|s| {
        let last = (pages(s.shown.len()) - 1) as f32;
        let step = delta as f32 / 600.0;
        // Same resistance past the ends as a drag, applied per step.
        let outside = s.offset + step < 0.0 || s.offset + step > last;
        s.offset = (s.offset + if outside { step / 3.0 } else { step }).clamp(-0.3, last + 0.3);
        s.swiped = Some(Instant::now());
        s.hover = None;
    });
    SetTimer(hwnd, TIMER_SLIDE, 15, None);
}

fn scroll_by(delta: f32) {
    STATE.with_borrow_mut(|s| {
        s.scroll = (s.scroll + delta).clamp(0.0, max_scroll(s.shown.len()));
        s.hover = None;
    });
    unsafe { render() };
}

fn on_key(key: VIRTUAL_KEY) {
    let style = STATE.with_borrow(|s| s.style);
    let page = STATE.with_borrow(|s| s.page);
    match key {
        VK_ESCAPE => {
            let had_query = STATE.with_borrow_mut(|s| {
                let had = !s.query.is_empty();
                if had {
                    s.query.clear();
                    filter(s);
                }
                had
            });
            if had_query {
                unsafe { render() };
            } else {
                hide();
                unsafe { gfx::restore_foreground(STATE.with_borrow(|s| s.previous)) };
            }
        }
        VK_RETURN => {
            if !STATE.with_borrow(|s| s.shown.is_empty()) {
                launch(0);
            }
        }
        VK_LEFT | VK_PRIOR if style == Style::Grid => go_to_page(page.saturating_sub(1)),
        VK_RIGHT | VK_NEXT if style == Style::Grid => go_to_page(page + 1),
        VK_HOME if style == Style::Grid => go_to_page(0),
        VK_END if style == Style::Grid => go_to_page(usize::MAX),
        VK_PRIOR => scroll_by(-STEP * 4.0),
        VK_NEXT => scroll_by(STEP * 4.0),
        VK_HOME => scroll_by(-f32::MAX / 4.0),
        VK_END => scroll_by(f32::MAX / 4.0),
        _ => {}
    }
}

fn on_char(code: u32) {
    let changed = STATE.with_borrow_mut(|s| {
        match code {
            0x08 => {
                s.query.pop();
            }
            0x7F => s.query.clear(),
            _ => match char::from_u32(code) {
                Some(c) if !c.is_control() => s.query.push(c),
                _ => return false,
            },
        }
        filter(s);
        true
    });
    if changed {
        request_shown_icons();
        unsafe { render() };
    }
}

/// `i` is a position in `shown`.
fn launch(i: usize) {
    let target = STATE.with_borrow(|s| s.shown.get(i).map(|&a| s.apps[a].parsing_name()));
    if let Some(target) = target {
        hide();
        apps::open(&target);
    }
}

unsafe fn menu(app: Option<usize>) -> Result<()> {
    let pin = app.and_then(|i| {
        STATE.with_borrow(|s| {
            let app = &s.apps[*s.shown.get(i)?];
            Some(Pin { name: app.name.clone(), launch: app.parsing_name(), aumid: Some(app.id.clone()), path: app.target.clone() })
        })
    });
    let style = STATE.with_borrow(|s| s.style);
    let mut menu = Menu::new();
    if let Some(pin) = &pin {
        menu.item(CMD_OPEN, "Открыть").check(CMD_PIN, "Оставить в Доке", dock::is_pinned(&pin.launch)).separator();
    }
    let mut look = Menu::new();
    look.check(CMD_GRID, "Сетка как в macOS", style == Style::Grid).check(CMD_HONEYCOMB, "Соты как в Apple Watch", style == Style::Honeycomb);
    menu.submenu("Вид", look);
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let cmd = menu.show(Anchor::At(cursor));
    match (cmd, app, pin) {
        (CMD_OPEN, Some(i), _) => launch(i),
        (CMD_PIN, _, Some(pin)) => dock::toggle_pin(pin),
        (CMD_GRID, ..) => set_style(Style::Grid),
        (CMD_HONEYCOMB, ..) => set_style(Style::Honeycomb),
        _ => {}
    }
    Ok(())
}

fn set_style(style: Style) {
    STATE.with_borrow_mut(|s| {
        s.style = style;
        s.page = 0;
        s.offset = 0.0;
        s.scroll = 0.0;
        s.hover = None;
    });
    save_style(style);
    request_shown_icons();
    unsafe { render() };
}

fn load_style() -> Style {
    match config::text("launchpad.style").as_str() {
        "honeycomb" => Style::Honeycomb,
        _ => Style::Grid,
    }
}

fn save_style(style: Style) {
    let name = match style {
        Style::Grid => "grid",
        Style::Honeycomb => "honeycomb",
    };
    config::store("launchpad.style", name);
}
