//! Notification area ("tray") host. Programs add their icons with
//! Shell_NotifyIcon, which shell32 delivers as WM_COPYDATA to the first window
//! of class `Shell_TrayWnd` it finds, so we create one and keep it above
//! Explorer's in the Z-order (the same trick RetroBar uses). Everything is also
//! forwarded to Explorer's real tray so it stays in sync (balloon toasts, and
//! a complete tray again once we exit); AppBar requests, which travel the same
//! way, are only forwarded. The icons are shown in a popup under the chevron
//! in the menu bar, and clicks go back to their programs the way the real
//! taskbar sends them.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use windows::core::{w, Result, GUID};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use crate::icons::{self, Icon};
use crate::{menubar, system_taskbar};

const COPYDATA_APPBAR: usize = 0;
const COPYDATA_NOTIFYICON: usize = 1;
const COPYDATA_ICON_RECT: usize = 3;
const TRAY_SIGNATURE: u32 = 0x3475_3423;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIF_TIP: u32 = 0x04;
const NIF_STATE: u32 = 0x08;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x01;
const NOTIFYICON_VERSION_4: u32 = 4;
const NIN_SELECT: u32 = WM_USER;

const ICON: f32 = 16.0;
const CELL: f32 = 40.0;
const COLUMNS: usize = 6;
const PAD: f32 = 10.0;
const FOOTER_H: f32 = 30.0;
const RADIUS: f32 = 16.0;
const CELL_RADIUS: f32 = 10.0;
const GAP: f32 = 6.0;
const FONT_SIZE: f32 = 12.5;

const COLOR_PANEL: COLORREF = rgb(30, 30, 34);

/// NOTIFYICONDATAW as shell32 marshals it to the tray: handles are 32-bit so
/// 32- and 64-bit programs send the same layout.
#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconData32 {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback_message: u32,
    icon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TrayData {
    signature: u32,
    message: u32,
    nid: NotifyIconData32,
}

/// What Shell_NotifyIconGetRect sends.
#[repr(C)]
#[derive(Clone, Copy)]
struct IconIdentifier32 {
    magic: u32,
    message: u32,
    cb_size: u32,
    padding: u32,
    hwnd: u32,
    uid: u32,
    guid: GUID,
}

/// 32-bit handle values are sign-extended to pointers.
fn handle(value: u32) -> *mut std::ffi::c_void {
    value as i32 as isize as *mut std::ffi::c_void
}

/// Reads a struct out of WM_COPYDATA bytes; shorter (older) senders leave the tail zeroed.
unsafe fn read<T: Copy>(data: &COPYDATASTRUCT) -> Option<T> {
    if data.lpData.is_null() || data.cbData < 8 {
        return None;
    }
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let len = (data.cbData as usize).min(std::mem::size_of::<T>());
    std::ptr::copy_nonoverlapping(data.lpData as *const u8, value.as_mut_ptr() as *mut u8, len);
    Some(value.assume_init())
}

fn utf16(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

struct TrayIcon {
    owner: HWND,
    uid: u32,
    guid: Option<GUID>,
    callback: u32,
    version: u32,
    tip: String,
    hidden: bool,
    /// Our own copy; the program may destroy its handle right after sending it.
    hicon: HICON,
    image: Option<Rc<Icon>>,
}

impl TrayIcon {
    fn is(&self, nid: &NotifyIconData32) -> bool {
        match (self.guid, nid.flags & NIF_GUID != 0) {
            (Some(guid), true) => guid == nid.guid,
            _ => self.owner.0 == handle(nid.hwnd) && self.uid == nid.uid,
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if !self.hicon.is_invalid() {
            unsafe {
                let _ = DestroyIcon(self.hicon);
            }
        }
    }
}

#[derive(Default)]
struct State {
    host: HWND,
    popup: HWND,
    icons: Vec<TrayIcon>,
    visible: bool,
    hover: Option<usize>,
    tracking_mouse: bool,
    last_click: Option<(usize, Instant)>,
    /// Where the menu bar's chevron is, in screen coordinates.
    anchor: RECT,
    /// Focus goes back here when dismissed with Esc.
    previous: HWND,
    /// Frosted glass captured when the popup opened, and the size it was for.
    backdrop: (Vec<u32>, (i32, i32)),
    font: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn create() -> Result<()> {
    unsafe {
        let host = gfx::create_window(w!("Shell_TrayWnd"), Some(host_proc), WS_EX_TOOLWINDOW | WS_EX_TOPMOST)?;
        let popup = gfx::create_window(
            w!("CustomUI.TrayPopup"),
            Some(popup_proc),
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        )?;
        let font = gfx::create_font(-(FONT_SIZE * scale()) as i32, FW_NORMAL, gfx::ui_font());
        STATE.with_borrow_mut(|s| {
            s.host = host;
            s.popup = popup;
            s.font = font;
        });
        reassert();
        broadcast();
    }
    Ok(())
}

pub fn destroy() {
    let (host, popup) = STATE.with_borrow_mut(|s| {
        s.icons.clear();
        (std::mem::take(&mut s.host), std::mem::take(&mut s.popup))
    });
    unsafe {
        for hwnd in [popup, host] {
            if !hwnd.is_invalid() {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

/// Keeps our host above Explorer's tray so FindWindow("Shell_TrayWnd") returns ours.
pub fn reassert() {
    let host = STATE.with_borrow(|s| s.host);
    if !host.is_invalid() {
        unsafe {
            let _ = SetWindowPos(host, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// Asks every program to (re-)add its icons; they'll reach us now.
pub fn broadcast() {
    unsafe {
        let message = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = PostMessageW(HWND_BROADCAST, message, WPARAM(0), LPARAM(0));
    }
}

pub fn has_icons() -> bool {
    STATE.with_borrow(|s| s.icons.iter().any(|i| !i.hidden))
}

pub fn set_anchor(anchor: RECT) {
    STATE.with_borrow_mut(|s| s.anchor = anchor);
}

fn scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA && lparam.0 != 0 {
        let data = &*(lparam.0 as *const COPYDATASTRUCT);
        return on_copydata(wparam, lparam, data);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Sends the same WM_COPYDATA on to Explorer and returns its answer.
unsafe fn forward(wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(tray) = system_taskbar::explorer_tray() else { return LRESULT(0) };
    let mut result = 0usize;
    let ok = SendMessageTimeoutW(tray, WM_COPYDATA, wparam, lparam, SMTO_ABORTIFHUNG, 2000, Some(&mut result));
    if ok.0 == 0 {
        return LRESULT(0);
    }
    LRESULT(result as isize)
}

unsafe fn on_copydata(wparam: WPARAM, lparam: LPARAM, data: &COPYDATASTRUCT) -> LRESULT {
    match data.dwData {
        COPYDATA_NOTIFYICON => {
            forward(wparam, lparam);
            match read::<TrayData>(data) {
                Some(tray) if tray.signature == TRAY_SIGNATURE => LRESULT(notify(tray.message, &tray.nid) as isize),
                _ => LRESULT(0),
            }
        }
        COPYDATA_ICON_RECT => {
            // Where "our" icons are: the chevron. Programs use it to place their flyouts.
            let anchor = STATE.with_borrow(|s| s.anchor);
            match read::<IconIdentifier32>(data).map(|id| id.message) {
                Some(1) => LRESULT(make_long(anchor.left, anchor.top) as isize),
                Some(2) => LRESULT(make_long(anchor.right, anchor.bottom) as isize),
                _ => LRESULT(0),
            }
        }
        // AppBar requests (including our own) are Explorer's business.
        COPYDATA_APPBAR => forward(wparam, lparam),
        _ => forward(wparam, lparam),
    }
}

fn make_long(low: i32, high: i32) -> u32 {
    (low as u32 & 0xFFFF) | (high as u32 & 0xFFFF) << 16
}

unsafe fn notify(message: u32, nid: &NotifyIconData32) -> bool {
    let size = (ICON * scale()).round() as i32;
    let handled = STATE.with_borrow_mut(|s| {
        let index = s.icons.iter().position(|i| i.is(nid));
        match message {
            NIM_ADD | NIM_MODIFY => {
                // A modify for an icon we never saw (added before we started) acts as an add.
                let index = index.unwrap_or_else(|| {
                    s.icons.push(TrayIcon {
                        owner: HWND(handle(nid.hwnd)),
                        uid: nid.uid,
                        guid: None,
                        callback: 0,
                        version: 0,
                        tip: String::new(),
                        hidden: false,
                        hicon: HICON::default(),
                        image: None,
                    });
                    s.icons.len() - 1
                });
                let icon = &mut s.icons[index];
                if nid.flags & NIF_GUID != 0 {
                    icon.guid = Some(nid.guid);
                }
                if nid.flags & NIF_MESSAGE != 0 {
                    icon.callback = nid.callback_message;
                }
                if nid.flags & NIF_TIP != 0 {
                    icon.tip = utf16(&nid.tip);
                }
                if nid.flags & NIF_STATE != 0 && nid.state_mask & NIS_HIDDEN != 0 {
                    icon.hidden = nid.state & NIS_HIDDEN != 0;
                }
                if nid.flags & NIF_ICON != 0 {
                    if let Ok(copy) = CopyIcon(HICON(handle(nid.icon))) {
                        if !icon.hicon.is_invalid() {
                            let _ = DestroyIcon(icon.hicon);
                        }
                        icon.hicon = copy;
                        icon.image = icons::from_hicon(copy, size).map(Rc::new);
                    }
                }
                true
            }
            NIM_DELETE => match index {
                Some(i) => {
                    s.icons.remove(i);
                    s.hover = None;
                    true
                }
                None => false,
            },
            NIM_SETVERSION => match index {
                Some(i) => {
                    s.icons[i].version = nid.version;
                    true
                }
                None => false,
            },
            _ => true,
        }
    });
    // Icons of programs that died without deleting them would linger forever.
    STATE.with_borrow_mut(|s| s.icons.retain(|i| IsWindow(i.owner).as_bool()));
    changed();
    handled
}

fn changed() {
    menubar::redraw();
    if STATE.with_borrow(|s| s.visible) {
        unsafe { render_popup() };
    }
}

/// Opens the popup under `anchor` (the chevron, in screen coordinates), or closes it.
pub fn toggle_popup(anchor: RECT) {
    let (popup, visible) = STATE.with_borrow_mut(|s| {
        s.anchor = anchor;
        (s.popup, s.visible)
    });
    if visible {
        hide_popup();
        return;
    }
    STATE.with_borrow_mut(|s| {
        s.icons.retain(|i| unsafe { IsWindow(i.owner).as_bool() });
        s.visible = true;
        s.hover = None;
        s.last_click = None;
    });
    unsafe {
        // Frosted glass: what's under the popup, captured before it shows.
        let dpi = scale();
        let (width, height) = popup_size(STATE.with_borrow(|s| shown(s).len()), dpi);
        let at = popup_position(&anchor, width, dpi);
        let pixels = gfx::frosted(RECT { left: at.x, top: at.y, right: at.x + width, bottom: at.y + height }).unwrap_or_default();
        STATE.with_borrow_mut(|s| s.backdrop = (pixels, (width, height)));
        render_popup();
        let _ = ShowWindow(popup, SW_SHOW);
        let previous = gfx::force_foreground(popup);
        STATE.with_borrow_mut(|s| s.previous = previous);
    }
}

fn hide_popup() {
    let popup = STATE.with_borrow_mut(|s| {
        s.tracking_mouse = false;
        if std::mem::replace(&mut s.visible, false) { s.popup } else { HWND::default() }
    });
    if !popup.is_invalid() {
        unsafe {
            let _ = ShowWindow(popup, SW_HIDE);
        }
    }
}

/// Indices of the icons shown in the popup.
fn shown(s: &State) -> Vec<usize> {
    (0..s.icons.len()).filter(|&i| !s.icons[i].hidden).collect()
}

fn popup_size(count: usize, dpi: f32) -> (i32, i32) {
    let columns = count.clamp(1, COLUMNS);
    let rows = count.div_ceil(COLUMNS).max(1);
    let width = PAD * 2.0 + CELL * columns as f32;
    let height = PAD * 2.0 + CELL * rows as f32 + FOOTER_H;
    ((width * dpi).round() as i32, (height * dpi).round() as i32)
}

fn cell_rect(slot: usize, dpi: f32) -> RECT {
    let (col, row) = ((slot % COLUMNS) as f32, (slot / COLUMNS) as f32);
    let left = ((PAD + col * CELL) * dpi).round() as i32;
    let top = ((PAD + row * CELL) * dpi).round() as i32;
    let size = (CELL * dpi).round() as i32;
    RECT { left, top, right: left + size, bottom: top + size }
}

unsafe fn render_popup() {
    let dpi = scale();
    let px = |v: f32| (v * dpi).round() as i32;
    let (popup, anchor, count) = STATE.with_borrow(|s| (s.popup, s.anchor, shown(s).len()));
    let (width, height) = popup_size(count, dpi);
    let Some(mut canvas) = Canvas::new(width, height) else { return };

    STATE.with_borrow(|s| {
        let dc = canvas.dc;
        // If icons came or went since opening, the captured glass no longer fits.
        let backdrop = if s.backdrop.1 == (width, height) { s.backdrop.0.as_slice() } else { &[] };
        canvas.backdrop(backdrop, width, COLOR_PANEL);
        let shown = shown(s);
        if let Some(slot) = s.hover.and_then(|h| shown.iter().position(|&i| i == h)) {
            canvas.fill_shape(&cell_rect(slot, dpi), CELL_RADIUS * dpi, COLOR_TEXT, 30);
        }
        let footer = RECT { left: px(PAD + 6.0), top: height - px(PAD + FOOTER_H), right: width - px(PAD + 6.0), bottom: height - px(PAD) };
        SelectObject(dc, s.font);
        let tip = s.hover.map(|i| s.icons[i].tip.as_str()).filter(|t| !t.is_empty());
        SetTextColor(dc, if tip.is_some() { COLOR_TEXT } else { COLOR_DIM });
        gfx::draw_text(dc, tip.unwrap_or(if shown.is_empty() { "Значков нет" } else { "Значки в трее" }), &footer, DT_LEFT | DT_END_ELLIPSIS);

        canvas.finish_panel(RADIUS * dpi);

        let size = px(ICON);
        for (slot, &i) in shown.iter().enumerate() {
            if let Some(image) = &s.icons[i].image {
                let r = gfx::centered(&cell_rect(slot, dpi), size);
                canvas.draw_icon(image, r.left, r.top, size, None, 1.0);
            }
        }
    });

    let at = popup_position(&anchor, width, dpi);
    canvas.present(popup, at.x, at.y, 255);
}

/// Right-aligned under the chevron, kept on screen.
fn popup_position(anchor: &RECT, width: i32, dpi: f32) -> POINT {
    let gap = (GAP * dpi).round() as i32;
    POINT { x: (anchor.right - width).max(gap), y: anchor.bottom + gap }
}

unsafe fn icon_at(p: POINT) -> Option<usize> {
    let dpi = scale();
    STATE.with_borrow(|s| {
        shown(s).into_iter().enumerate().find_map(|(slot, i)| {
            let r = cell_rect(slot, dpi);
            (p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom).then_some(i)
        })
    })
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        // Closes when the user clicks elsewhere, or when the program we clicked takes the foreground.
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => hide_popup(),
        WM_KEYDOWN if wparam.0 == 0x1B => {
            hide_popup();
            gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
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
            let hover = icon_at(point());
            if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
                render_popup();
            }
        }
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.hover = None;
                s.tracking_mouse = false;
            });
            render_popup();
        }
        WM_LBUTTONUP => {
            if let Some(i) = icon_at(point()) {
                // Our window class has no CS_DBLCLKS; detect double clicks ourselves.
                let double = STATE.with_borrow_mut(|s| {
                    let double = s.last_click.is_some_and(|(j, t)| j == i && t.elapsed().as_millis() <= GetDoubleClickTime() as u128);
                    s.last_click = if double { None } else { Some((i, Instant::now())) };
                    double
                });
                click(i, if double { Click::Double } else { Click::Left });
            }
        }
        WM_RBUTTONUP => {
            if let Some(i) = icon_at(point()) {
                click(i, Click::Right);
            }
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

enum Click {
    Left,
    Double,
    Right,
}

/// Sends the mouse messages the real taskbar would, in the format the icon asked
/// for with NIM_SETVERSION, and lets the program take the foreground for its menu.
unsafe fn click(index: usize, kind: Click) {
    let Some((owner, uid, callback, version)) = STATE.with_borrow(|s| {
        s.icons.get(index).map(|i| (i.owner, i.uid, i.callback, i.version))
    }) else {
        return;
    };
    if callback == 0 {
        return;
    }
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let mut pid = 0;
    GetWindowThreadProcessId(owner, Some(&mut pid));
    let _ = AllowSetForegroundWindow(pid);

    let v4 = version >= NOTIFYICON_VERSION_4;
    let events: &[u32] = match kind {
        Click::Left if v4 => &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT],
        Click::Left => &[WM_LBUTTONDOWN, WM_LBUTTONUP],
        Click::Double => &[WM_LBUTTONDBLCLK, WM_LBUTTONUP],
        Click::Right if v4 => &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU],
        Click::Right => &[WM_RBUTTONDOWN, WM_RBUTTONUP],
    };
    for &event in events {
        let (wparam, lparam) = if v4 {
            (WPARAM(make_long(cursor.x, cursor.y) as usize), LPARAM(make_long(event as i32, uid as i32) as isize))
        } else {
            (WPARAM(uid as usize), LPARAM(event as isize))
        };
        let _ = PostMessageW(owner, callback, wparam, lparam);
    }
}
