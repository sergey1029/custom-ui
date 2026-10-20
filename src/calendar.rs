//! Calendar popup under the menu bar clock: today's full date and a month grid
//! (Monday first) that ‹ › or the mouse wheel page through; clicking the month
//! title jumps back to today.

use std::cell::RefCell;

use windows::core::{w, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};

const PAD: f32 = 16.0;
const CELL: f32 = 38.0;
const DATE_H: f32 = 30.0;
const HEADER_H: f32 = 36.0;
const WEEKDAYS_H: f32 = 26.0;
const ARROW: f32 = 30.0;
const RADIUS: f32 = 20.0;
const OFFSET: f32 = 6.0;
const FONT_SIZE: f32 = 13.0;
const TITLE_FONT: f32 = 15.0;

const COLOR_PANEL: COLORREF = rgb(30, 30, 34);
const COLOR_WEEKEND: COLORREF = rgb(255, 128, 118);
const COLOR_OTHER_MONTH: COLORREF = rgb(86, 86, 96);
const COLOR_DIVIDER: COLORREF = rgb(58, 58, 66);

const MONTHS: [&str; 12] = [
    "Январь", "Февраль", "Март", "Апрель", "Май", "Июнь", "Июль", "Август", "Сентябрь", "Октябрь", "Ноябрь", "Декабрь",
];
const MONTHS_GENITIVE: [&str; 12] = [
    "января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря",
];
const WEEKDAYS: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];
const WEEKDAYS_FULL: [&str; 7] = ["Понедельник", "Вторник", "Среда", "Четверг", "Пятница", "Суббота", "Воскресенье"];

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Previous,
    Next,
    Title,
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    visible: bool,
    previous: HWND,
    /// Frosted glass: the screen under the panel, captured when it opened.
    backdrop: Vec<u32>,
    anchor: RECT,
    /// The month on screen.
    year: i32,
    month: u32,
    hover: Option<Part>,
    tracking_mouse: bool,
    font: HFONT,
    bold_font: HFONT,
    title_font: HFONT,
    glyph_font: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn create() -> Result<()> {
    unsafe {
        let hwnd = gfx::create_window(w!("CustomUI.Calendar"), Some(wndproc), WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED)?;
        let size = |v: f32| -(v * scale()) as i32;
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.font = gfx::create_font(size(FONT_SIZE), FW_NORMAL, gfx::ui_font());
            s.bold_font = gfx::create_font(size(FONT_SIZE), FW_SEMIBOLD, gfx::ui_font());
            s.title_font = gfx::create_font(size(TITLE_FONT), FW_SEMIBOLD, gfx::ui_font());
            s.glyph_font = gfx::create_font(size(12.0), FW_NORMAL, w!("Segoe Fluent Icons"));
        });
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

/// Opens under `anchor` (the clock, in screen coordinates) on the current month, or closes.
pub fn toggle(anchor: RECT) {
    if STATE.with_borrow(|s| s.visible) {
        return hide();
    }
    let (year, month, _) = today();
    let hwnd = STATE.with_borrow_mut(|s| {
        s.anchor = anchor;
        s.year = year;
        s.month = month;
        s.visible = true;
        s.hover = None;
        s.hwnd
    });
    unsafe {
        // Frosted glass: what's under the panel, captured before it shows.
        let dpi = scale();
        let l = layout(dpi);
        let at = position(&anchor, l.width, dpi);
        let backdrop = gfx::frosted(RECT { left: at.x, top: at.y, right: at.x + l.width, bottom: at.y + l.height }).unwrap_or_default();
        STATE.with_borrow_mut(|s| s.backdrop = backdrop);
        render(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let previous = gfx::force_foreground(hwnd);
        STATE.with_borrow_mut(|s| s.previous = previous);
    }
}

fn hide() {
    let hwnd = STATE.with_borrow_mut(|s| {
        s.tracking_mouse = false;
        if std::mem::replace(&mut s.visible, false) { s.hwnd } else { HWND::default() }
    });
    if !hwnd.is_invalid() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

fn today() -> (i32, u32, u32) {
    let t = unsafe { GetLocalTime() };
    (t.wYear as i32, t.wMonth as u32, t.wDay as u32)
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 0 = Monday … 6 = Sunday (Sakamoto's method).
pub fn weekday(year: i32, month: u32, day: u32) -> u32 {
    const OFFSETS: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let sunday_first = (y + y / 4 - y / 100 + y / 400 + OFFSETS[month as usize - 1] + day as i32).rem_euclid(7);
    ((sunday_first + 6) % 7) as u32
}

fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let index = year * 12 + month as i32 - 1 + delta;
    (index.div_euclid(12), (index.rem_euclid(12) + 1) as u32)
}

/// The 6×7 grid for a month: (year, month, day) per cell, padded with the
/// neighbouring months' days.
fn grid(year: i32, month: u32) -> Vec<(i32, u32, u32)> {
    let lead = weekday(year, month, 1);
    let (prev_year, prev_month) = shift_month(year, month, -1);
    let (next_year, next_month) = shift_month(year, month, 1);
    let prev_days = days_in_month(prev_year, prev_month);
    let days = days_in_month(year, month);
    (0..42)
        .map(|cell| {
            let n = cell - lead as i32 + 1;
            if n < 1 {
                (prev_year, prev_month, (prev_days as i32 + n) as u32)
            } else if n as u32 > days {
                (next_year, next_month, n as u32 - days)
            } else {
                (year, month, n as u32)
            }
        })
        .collect()
}

struct Layout {
    width: i32,
    height: i32,
    date: RECT,
    title: RECT,
    previous: RECT,
    next: RECT,
    weekdays: RECT,
    /// Top-left of the day grid.
    grid: POINT,
}

fn layout(dpi: f32) -> Layout {
    let px = |v: f32| (v * dpi).round() as i32;
    let width = PAD * 2.0 + CELL * 7.0;
    let date = RECT { left: px(PAD + 4.0), top: px(PAD - 2.0), right: px(width - PAD), bottom: px(PAD - 2.0 + DATE_H) };
    let header_top = PAD + DATE_H + 10.0;
    let next = RECT { left: px(width - PAD - ARROW), top: px(header_top + 3.0), right: px(width - PAD), bottom: px(header_top + 3.0 + ARROW) };
    let previous = RECT { left: next.left - px(ARROW + 4.0), right: next.left - px(4.0), ..next };
    let title = RECT { left: px(PAD + 4.0), top: px(header_top), right: previous.left - px(8.0), bottom: px(header_top + HEADER_H) };
    let weekdays_top = header_top + HEADER_H + 4.0;
    let weekdays = RECT { left: px(PAD), top: px(weekdays_top), right: px(width - PAD), bottom: px(weekdays_top + WEEKDAYS_H) };
    let grid_top = weekdays_top + WEEKDAYS_H;
    Layout {
        width: px(width),
        height: px(grid_top + CELL * 6.0 + PAD - 4.0),
        date,
        title,
        previous,
        next,
        weekdays,
        grid: POINT { x: px(PAD), y: px(grid_top) },
    }
}

unsafe fn render(hwnd: HWND) {
    let dpi = scale();
    let px = |v: f32| (v * dpi).round() as i32;
    let l = layout(dpi);
    let Some(mut canvas) = Canvas::new(l.width, l.height) else { return };
    let dc = canvas.dc;
    let (today_year, today_month, today_day) = today();

    let anchor = STATE.with_borrow(|s| {
        canvas.backdrop(&s.backdrop, l.width, COLOR_PANEL);

        // Today, spelled out.
        let weekday_index = weekday(today_year, today_month, today_day) as usize;
        let date = format!(
            "{}, {} {} {}",
            WEEKDAYS_FULL[weekday_index],
            today_day,
            MONTHS_GENITIVE[today_month as usize - 1],
            today_year
        );
        SelectObject(dc, s.title_font);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, &date, &l.date, DT_LEFT | DT_END_ELLIPSIS);
        let divider_y = l.date.bottom + px(6.0);
        gfx::fill(dc, &RECT { left: px(PAD), top: divider_y, right: l.width - px(PAD), bottom: divider_y + px(1.0).max(1) }, COLOR_DIVIDER);

        // Month title and arrows.
        let viewing_today = s.year == today_year && s.month == today_month;
        if s.hover == Some(Part::Title) && !viewing_today {
            canvas.fill_shape(&RECT { right: l.title.left + px(150.0), left: l.title.left - px(6.0), ..l.title }, 8.0 * dpi, COLOR_TEXT, 30);
        }
        SelectObject(dc, s.bold_font);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, &format!("{} {}", MONTHS[s.month as usize - 1], s.year), &l.title, DT_LEFT);
        SelectObject(dc, s.glyph_font);
        for (part, r, glyph) in [(Part::Previous, l.previous, "\u{E76B}"), (Part::Next, l.next, "\u{E76C}")] {
            if s.hover == Some(part) {
                canvas.fill_shape(&r, (r.right - r.left) as f32 / 2.0, COLOR_TEXT, 30);
            }
            gfx::draw_text(dc, glyph, &r, DT_CENTER);
        }

        // Weekday names, weekends tinted.
        SelectObject(dc, s.font);
        let cell = px(CELL);
        for (i, name) in WEEKDAYS.iter().enumerate() {
            let r = RECT { left: l.grid.x + cell * i as i32, right: l.grid.x + cell * (i as i32 + 1), ..l.weekdays };
            SetTextColor(dc, if i >= 5 { COLOR_WEEKEND } else { COLOR_DIM });
            gfx::draw_text(dc, name, &r, DT_CENTER);
        }

        // Days.
        for (i, (year, month, day)) in grid(s.year, s.month).into_iter().enumerate() {
            let (col, row) = ((i % 7) as i32, (i / 7) as i32);
            let r = RECT {
                left: l.grid.x + cell * col,
                top: l.grid.y + cell * row,
                right: l.grid.x + cell * (col + 1),
                bottom: l.grid.y + cell * (row + 1),
            };
            let is_today = (year, month, day) == (today_year, today_month, today_day);
            let in_month = month == s.month;
            if is_today {
                gfx::fill_round(dc, &gfx::centered(&r, px(32.0)), gfx::accent(), px(32.0));
            }
            SelectObject(dc, if is_today { s.bold_font } else { s.font });
            SetTextColor(dc, match () {
                _ if is_today => COLOR_TEXT,
                _ if !in_month => COLOR_OTHER_MONTH,
                _ if col >= 5 => COLOR_WEEKEND,
                _ => COLOR_TEXT,
            });
            gfx::draw_text(dc, &day.to_string(), &r, DT_CENTER);
        }
        s.anchor
    });

    canvas.finish_panel(RADIUS * dpi);
    let at = position(&anchor, l.width, dpi);
    canvas.present(hwnd, at.x, at.y, 255);
}

/// Right-aligned with the clock, kept on screen.
unsafe fn position(anchor: &RECT, width: i32, dpi: f32) -> POINT {
    let margin = (8.0 * dpi).round() as i32;
    let screen = GetSystemMetrics(SM_CXSCREEN);
    POINT { x: (anchor.right - width).clamp(margin, screen - width - margin), y: anchor.bottom + (OFFSET * dpi).round() as i32 }
}

fn part_at(p: POINT) -> Option<Part> {
    let l = layout(scale());
    let inside = |r: &RECT| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
    [(Part::Previous, l.previous), (Part::Next, l.next), (Part::Title, l.title)]
        .into_iter()
        .find(|(_, r)| inside(r))
        .map(|(part, _)| part)
}

unsafe fn page(hwnd: HWND, delta: i32) {
    STATE.with_borrow_mut(|s| (s.year, s.month) = shift_month(s.year, s.month, delta));
    render(hwnd);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => hide(),
        WM_KEYDOWN => match wparam.0 {
            0x1B => {
                hide();
                gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
            }
            0x25 | 0x21 => page(hwnd, -1), // Left, PageUp
            0x27 | 0x22 => page(hwnd, 1),  // Right, PageDown
            _ => {}
        },
        WM_MOUSEWHEEL => {
            let notches = (wparam.0 >> 16 & 0xFFFF) as i16;
            page(hwnd, if notches > 0 { -1 } else { 1 });
        }
        WM_MOUSEMOVE => {
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
            let hover = part_at(point());
            if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
                render(hwnd);
            }
        }
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.hover = None;
                s.tracking_mouse = false;
            });
            render(hwnd);
        }
        WM_LBUTTONUP => match part_at(point()) {
            Some(Part::Previous) => page(hwnd, -1),
            Some(Part::Next) => page(hwnd, 1),
            Some(Part::Title) => {
                let (year, month, _) = today();
                STATE.with_borrow_mut(|s| (s.year, s.month) = (year, month));
                render(hwnd);
            }
            None => {}
        },
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::{days_in_month, grid, weekday};

    #[test]
    fn weekdays() {
        assert_eq!(weekday(2026, 10, 20), 1); // Tuesday
        assert_eq!(weekday(2024, 2, 29), 3); // Thursday
        assert_eq!(weekday(2000, 1, 1), 5); // Saturday
    }

    #[test]
    fn month_grid() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2100, 2), 28);
        // October 2026 starts on a Thursday: Mon–Wed are 28–30 September.
        let cells = grid(2026, 10);
        assert_eq!(cells[0], (2026, 9, 28));
        assert_eq!(cells[3], (2026, 10, 1));
        assert_eq!(cells[41], (2026, 11, 8));
        // January pads with December of the previous year.
        assert_eq!(grid(2027, 1)[0], (2026, 12, 28));
    }
}
