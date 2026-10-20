//! macOS-style menus for everything the shell shows itself (the menu bar, the
//! dock, Spotlight, shelves, widgets): rounded frosted panels, a blue rounded
//! highlight, checkmarks on the left, shortcuts and submenu arrows on the right,
//! cascading submenus.
//!
//! `Menu::show` works like TrackPopupMenu: it runs its own message loop until
//! something is chosen (its id is returned) or the menu is dismissed (0). The
//! menu windows never take the focus, so the app in front stays in front, and
//! commands that send it keystrokes reach it.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, rgb, Canvas, COLOR_TEXT};

const PAD_V: f32 = 5.0;
const ITEM_H: f32 = 24.0;
const SEPARATOR_H: f32 = 9.0;
const INSET: f32 = 5.0;
const TEXT_LEFT: f32 = 24.0;
const TEXT_RIGHT: f32 = 14.0;
const SHORTCUT_GAP: f32 = 28.0;
const ARROW_W: f32 = 16.0;
const MIN_W: f32 = 180.0;
const RADIUS: f32 = 10.0;
const HIGHLIGHT_RADIUS: f32 = 5.0;
const FONT_SIZE: f32 = 13.0;
/// How often the open menu checks for outside clicks, Esc and focus changes.
const POLL_MS: u32 = 25;

const TIMER_POLL: usize = 1;

const COLOR_DISABLED: COLORREF = rgb(118, 118, 126);
const COLOR_SHORTCUT: COLORREF = rgb(156, 156, 164);
const COLOR_SEPARATOR: COLORREF = rgb(78, 78, 86);
const COLOR_EDGE: COLORREF = rgb(255, 255, 255);

#[derive(Clone)]
enum Entry {
    Item { id: usize, text: String, shortcut: String, enabled: bool, checked: bool },
    Separator,
    Submenu { text: String, menu: Menu, enabled: bool },
}

impl Entry {
    fn height(&self) -> f32 {
        match self {
            Entry::Separator => SEPARATOR_H,
            _ => ITEM_H,
        }
    }

    fn selectable(&self) -> bool {
        match self {
            Entry::Item { enabled, .. } | Entry::Submenu { enabled, .. } => *enabled,
            Entry::Separator => false,
        }
    }
}

#[derive(Clone, Default)]
pub struct Menu {
    entries: Vec<Entry>,
}

/// Where a menu opens.
pub enum Anchor {
    /// Top-left corner at a point (context menus at the cursor).
    At(POINT),
    /// Hanging below a rect, left-aligned (menu bar titles).
    Below(RECT),
    /// Centered above a point (dock icons).
    Above(POINT),
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn item(&mut self, id: usize, text: impl Into<String>) -> &mut Self {
        self.item_full(id, text, "", true, false)
    }

    pub fn item_full(&mut self, id: usize, text: impl Into<String>, shortcut: &str, enabled: bool, checked: bool) -> &mut Self {
        self.entries.push(Entry::Item { id, text: text.into(), shortcut: shortcut.to_owned(), enabled, checked });
        self
    }

    pub fn check(&mut self, id: usize, text: impl Into<String>, checked: bool) -> &mut Self {
        self.item_full(id, text, "", true, checked)
    }

    /// Ignored at the start and after another separator, like macOS.
    pub fn separator(&mut self) -> &mut Self {
        if !matches!(self.entries.last(), None | Some(Entry::Separator)) {
            self.entries.push(Entry::Separator);
        }
        self
    }

    pub fn submenu(&mut self, text: impl Into<String>, menu: Menu) -> &mut Self {
        let enabled = !menu.entries.is_empty();
        self.entries.push(Entry::Submenu { text: text.into(), menu, enabled });
        self
    }

    /// Shows the menu and returns the chosen item's id, or 0 if dismissed.
    pub fn show(&self, anchor: Anchor) -> usize {
        let mut menu = self.clone();
        if matches!(menu.entries.last(), Some(Entry::Separator)) {
            menu.entries.pop();
        }
        if menu.entries.is_empty() {
            return 0;
        }
        unsafe { run(menu, anchor) }
    }
}

struct Level {
    hwnd: HWND,
    menu: Menu,
    /// On screen.
    rect: RECT,
    hover: Option<usize>,
    backdrop: Vec<u32>,
    /// Which entry of the parent level this one hangs from.
    parent_entry: Option<usize>,
}

thread_local! {
    static FONT: RefCell<HFONT> = const { RefCell::new(HFONT(std::ptr::null_mut())) };
    static GLYPHS: RefCell<HFONT> = const { RefCell::new(HFONT(std::ptr::null_mut())) };
}

fn scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

fn px(v: f32) -> i32 {
    (v * scale()).round() as i32
}

unsafe fn fonts() -> (HFONT, HFONT) {
    let font = FONT.with_borrow_mut(|f| {
        if f.is_invalid() {
            *f = gfx::create_font(-px(FONT_SIZE), FW_NORMAL, gfx::ui_font());
        }
        *f
    });
    let glyphs = GLYPHS.with_borrow_mut(|f| {
        if f.is_invalid() {
            *f = gfx::create_font(-px(10.0), FW_NORMAL, w!("Segoe Fluent Icons"));
        }
        *f
    });
    (font, glyphs)
}

/// Width and height for a menu's entries.
unsafe fn measure(menu: &Menu) -> (i32, i32) {
    let (font, _) = fonts();
    let dc = CreateCompatibleDC(None);
    let old = SelectObject(dc, font);
    let mut widest = 0;
    for entry in &menu.entries {
        let w = match entry {
            Entry::Item { text, shortcut, .. } => {
                let shortcut_w = if shortcut.is_empty() { 0 } else { gfx::text_width(dc, shortcut) + px(SHORTCUT_GAP) };
                gfx::text_width(dc, text) + shortcut_w
            }
            Entry::Submenu { text, .. } => gfx::text_width(dc, text) + px(SHORTCUT_GAP + ARROW_W),
            Entry::Separator => 0,
        };
        widest = widest.max(w);
    }
    SelectObject(dc, old);
    let _ = DeleteDC(dc);
    let width = (widest + px(TEXT_LEFT + TEXT_RIGHT)).max(px(MIN_W));
    let height = px(PAD_V * 2.0 + menu.entries.iter().map(Entry::height).sum::<f32>());
    (width, height)
}

/// Entry rects in menu coordinates.
fn entry_rects(menu: &Menu, width: i32) -> Vec<RECT> {
    let mut y = PAD_V;
    menu.entries
        .iter()
        .map(|entry| {
            let top = px(y);
            y += entry.height();
            RECT { left: 0, top, right: width, bottom: px(y) }
        })
        .collect()
}

fn work_area() -> RECT {
    let mut work = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        // Menus may also cover the menu bar and dock areas.
        work = RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN).max(work.bottom) };
    }
    work
}

/// Keeps `rect` on screen, moving it rather than shrinking it.
fn fit(mut rect: RECT) -> RECT {
    let area = work_area();
    let margin = px(4.0);
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if rect.right > area.right - margin {
        rect.left = area.right - margin - w;
    }
    if rect.bottom > area.bottom - margin {
        rect.top = area.bottom - margin - h;
    }
    rect.left = rect.left.max(area.left + margin);
    rect.top = rect.top.max(area.top + margin);
    RECT { left: rect.left, top: rect.top, right: rect.left + w, bottom: rect.top + h }
}

unsafe fn open_level(levels: &mut Vec<Level>, menu: Menu, rect: RECT, parent_entry: Option<usize>) {
    let Ok(hwnd) = gfx::create_window(w!("CustomUI.Menu"), Some(menu_proc), WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED) else {
        return;
    };
    // Captured before the menu covers it.
    let backdrop = gfx::frosted(rect).unwrap_or_default();
    levels.push(Level { hwnd, menu, rect, hover: None, backdrop, parent_entry });
    render(levels.last().unwrap());
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
}

unsafe fn close_levels_after(levels: &mut Vec<Level>, keep: usize) {
    while levels.len() > keep {
        if let Some(level) = levels.pop() {
            let _ = DestroyWindow(level.hwnd);
        }
    }
}

unsafe fn run(menu: Menu, anchor: Anchor) -> usize {
    let (width, height) = measure(&menu);
    let rect = match anchor {
        Anchor::At(p) => RECT { left: p.x, top: p.y, right: p.x + width, bottom: p.y + height },
        Anchor::Below(r) => RECT { left: r.left, top: r.bottom + px(2.0), right: r.left + width, bottom: r.bottom + px(2.0) + height },
        Anchor::Above(p) => RECT { left: p.x - width / 2, top: p.y - height, right: p.x + width / 2 + width % 2, bottom: p.y },
    };
    let mut levels = Vec::new();
    open_level(&mut levels, menu, fit(rect), None);
    let Some(main) = levels.first().map(|l| l.hwnd) else { return 0 };
    SetTimer(main, TIMER_POLL, POLL_MS, None);
    let foreground = GetForegroundWindow();

    let mut result = 0;
    let mut msg = MSG::default();
    loop {
        let got = GetMessageW(&mut msg, None, 0, 0);
        if got.0 == 0 || got.0 == -1 {
            // WM_QUIT: hand it back to the main loop.
            PostQuitMessage(msg.wParam.0 as i32);
            break;
        }
        let ours = levels.iter().any(|l| l.hwnd == msg.hwnd);
        match msg.message {
            WM_MOUSEMOVE if ours => on_move(&mut levels),
            WM_LBUTTONUP | WM_RBUTTONUP if ours => {
                if let Some(id) = chosen(&levels) {
                    result = id;
                    break;
                }
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_LBUTTONDBLCLK if ours => {}
            WM_TIMER if msg.hwnd == main && msg.wParam.0 == TIMER_POLL => {
                let cursor = cursor();
                let outside = !levels.iter().any(|l| inside(&l.rect, cursor));
                let pressed = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].iter().any(|k| GetAsyncKeyState(k.0 as i32) as u16 & 0x8000 != 0);
                let escape = GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000 != 0;
                if escape || (pressed && outside) || GetForegroundWindow() != foreground {
                    break;
                }
                if outside {
                    // The pointer left: drop the deepest highlight (a parent's
                    // highlight stays, it marks the open submenu).
                    let last = levels.len() - 1;
                    if levels[last].hover.take().is_some() {
                        render(&levels[last]);
                    }
                }
            }
            _ => {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    let _ = KillTimer(main, TIMER_POLL);
    close_levels_after(&mut levels, 0);
    result
}

fn cursor() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

fn inside(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// (level, entry) under the cursor; the deepest level wins where they overlap.
fn entry_at(levels: &[Level]) -> Option<(usize, Option<usize>)> {
    let p = cursor();
    let (index, level) = levels.iter().enumerate().rev().find(|(_, l)| inside(&l.rect, p))?;
    let local = POINT { x: p.x - level.rect.left, y: p.y - level.rect.top };
    let rects = entry_rects(&level.menu, level.rect.right - level.rect.left);
    let entry = rects.iter().position(|r| inside(r, local)).filter(|&e| level.menu.entries[e].selectable());
    Some((index, entry))
}

unsafe fn on_move(levels: &mut Vec<Level>) {
    let Some((index, entry)) = entry_at(levels) else { return };
    if levels[index].hover != entry {
        levels[index].hover = entry;
        render(&levels[index]);
    }
    // Hovering an entry decides which submenu (if any) hangs off this level.
    let wants = entry.filter(|&e| matches!(levels[index].menu.entries[e], Entry::Submenu { .. }));
    let has = levels.get(index + 1).and_then(|l| l.parent_entry);
    if wants == has {
        return;
    }
    if entry.is_none() && has.is_some() {
        // Between items (or on a separator): keep the open submenu.
        return;
    }
    close_levels_after(levels, index + 1);
    if let Some(e) = wants {
        let Entry::Submenu { menu, .. } = &levels[index].menu.entries[e] else { return };
        let menu = menu.clone();
        let parent = levels[index].rect;
        let item = entry_rects(&levels[index].menu, parent.right - parent.left)[e];
        let (width, height) = measure(&menu);
        let top = parent.top + item.top - px(PAD_V);
        let mut rect = RECT { left: parent.right - px(4.0), top, right: parent.right - px(4.0) + width, bottom: top + height };
        if rect.right > work_area().right {
            // No room on the right: open to the left like macOS does.
            rect = RECT { left: parent.left - width + px(4.0), right: parent.left + px(4.0), ..rect };
        }
        open_level(levels, menu, fit(rect), Some(e));
    }
}

fn chosen(levels: &[Level]) -> Option<usize> {
    let (index, entry) = entry_at(levels)?;
    match levels[index].menu.entries.get(entry?)? {
        Entry::Item { id, enabled: true, .. } => Some(*id),
        _ => None,
    }
}

unsafe fn render(level: &Level) {
    let (width, height) = (level.rect.right - level.rect.left, level.rect.bottom - level.rect.top);
    let Some(mut canvas) = Canvas::new(width, height) else { return };
    let dc = canvas.dc;
    let client = RECT { left: 0, top: 0, right: width, bottom: height };
    if level.backdrop.len() == (width * height) as usize {
        canvas.fill_from(&level.backdrop, width, 0, 0);
    } else {
        gfx::fill(dc, &client, rgb(36, 36, 40));
    }

    let (font, glyphs) = fonts();
    let rects = entry_rects(&level.menu, width);
    for (i, (entry, r)) in level.menu.entries.iter().zip(&rects).enumerate() {
        let highlighted = level.hover == Some(i);
        if let Entry::Separator = entry {
            let y = (r.top + r.bottom) / 2;
            gfx::fill(dc, &RECT { left: r.left + px(INSET + 5.0), top: y, right: r.right - px(INSET + 5.0), bottom: y + px(1.0).max(1) }, COLOR_SEPARATOR);
            continue;
        }
        if highlighted {
            let bar = RECT { left: r.left + px(INSET), top: r.top, right: r.right - px(INSET), bottom: r.bottom };
            gfx::fill_round(dc, &bar, gfx::accent(), px(HIGHLIGHT_RADIUS * 2.0));
        }
        let enabled = entry.selectable();
        let color = if !enabled { COLOR_DISABLED } else { COLOR_TEXT };
        let text_rect = RECT { left: r.left + px(TEXT_LEFT), right: r.right - px(TEXT_RIGHT), ..*r };
        SelectObject(dc, font);
        SetTextColor(dc, color);
        match entry {
            Entry::Item { text, shortcut, checked, .. } => {
                if *checked {
                    gfx::draw_text(dc, "\u{2713}", &RECT { left: r.left + px(INSET + 4.0), right: text_rect.left, ..*r }, DT_LEFT);
                }
                gfx::draw_text(dc, text, &text_rect, DT_LEFT | DT_END_ELLIPSIS);
                if !shortcut.is_empty() {
                    SetTextColor(dc, if highlighted { COLOR_TEXT } else if enabled { COLOR_SHORTCUT } else { COLOR_DISABLED });
                    gfx::draw_text(dc, shortcut, &text_rect, DT_RIGHT);
                }
            }
            Entry::Submenu { text, .. } => {
                gfx::draw_text(dc, text, &text_rect, DT_LEFT | DT_END_ELLIPSIS);
                SelectObject(dc, glyphs);
                gfx::draw_text(dc, "\u{E76C}", &text_rect, DT_RIGHT);
            }
            Entry::Separator => {}
        }
    }

    let radius = RADIUS * scale();
    canvas.mask(&[(client, radius)]);
    canvas.stroke_shape(&client, radius, COLOR_EDGE, 34);
    canvas.present(level.hwnd, level.rect.left, level.rect.top, 255);
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Strips Win32 menu markup: "&File" → "File", "Save\tCtrl+S" → ("Save", "Ctrl+S").
pub fn split_label(raw: &str) -> (String, String) {
    let (text, shortcut) = raw.split_once('\t').unwrap_or((raw, ""));
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            // "&&" is a literal ampersand; a single one marks the mnemonic.
            if chars.peek() == Some(&'&') {
                clean.push('&');
                chars.next();
            }
        } else {
            clean.push(c);
        }
    }
    (clean, shortcut.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::split_label;

    #[test]
    fn labels() {
        assert_eq!(split_label("&File"), ("File".to_owned(), String::new()));
        assert_eq!(split_label("&Save\tCtrl+S"), ("Save".to_owned(), "Ctrl+S".to_owned()));
        assert_eq!(split_label("Fish && Chips"), ("Fish & Chips".to_owned(), String::new()));
    }
}
