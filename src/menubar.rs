//! The menu bar, as on a Mac: a full-width strip of frosted glass along the top
//! (the wallpaper blurred and tinted), with a notch in the middle where the
//! Coucou bubble sits, like a MacBook's. Left: the system menu, the front app's
//! bold name and its menus. Right of the notch: the playing track. Right: the
//! tray, keyboard layout, volume, network, battery, Spotlight, Control Center
//! and the clock.
//!
//! It also owns the window-event hooks and feeds the window list to the dock.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::core::{w, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{ABE_TOP, ABN_FULLSCREENAPP, ABN_POSCHANGED};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::appmenu::{self, Source, Title};
use crate::gfx::{self, rgb, Canvas, COLOR_BG, COLOR_DIM, COLOR_TEXT};
use crate::media::{self, Command, Track};
use crate::status::{self, Battery, Network, Volume};
use crate::{appbar, apps, calendar, config, controlcenter, coucou, dock, spotlight, system_taskbar, tasks, tray};

fn bar_height() -> f32 {
    config::int("menubar.height") as f32
}

/// Configured; Coucou's collapsed bubble is ~288 px wide at the top center, the default leaves a little air around it.
fn notch_width() -> f32 {
    if config::flag("menubar.notch") { config::int("menubar.notch_width") as f32 } else { 0.0 }
}
const MEDIA_WIDTH: f32 = 340.0;
const EDGE: f32 = 10.0;
/// Horizontal padding inside a title or status item.
const ITEM_PAD: f32 = 9.0;
const LOGO_WIDTH: f32 = 36.0;
const ICON_WIDTH: f32 = 30.0;
const LAYOUT_WIDTH: f32 = 34.0;
const VOLUME_WIDE: f32 = 70.0;
const BATTERY_WIDTH: f32 = 66.0;
const HIGHLIGHT_RADIUS: f32 = 5.0;
const HIGHLIGHT_INSET: f32 = 3.0;
const FONT_SIZE: f32 = 13.0;
const GLYPH_SIZE: f32 = 14.0;
/// One wheel notch over the volume icon.
const VOLUME_STEP: f32 = 0.02;
/// How long the volume percentage stays up after a change.
const VOLUME_FLASH: Duration = Duration::from_millis(1500);

const TIMER_REFRESH: usize = 1;
const TIMER_CLOCK: usize = 2;
/// Layout and volume have no change notification we can cheaply use; poll them quickly.
const TIMER_STATUS: usize = 3;

const COLOR_CHARGING: COLORREF = rgb(76, 217, 100);
const COLOR_LOW: COLORREF = rgb(255, 92, 80);
const COLOR_HIGHLIGHT: COLORREF = rgb(255, 255, 255);

/// Clickable things on the bar.
#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Logo,
    App,
    /// Index into `State::titles`.
    Menu(usize),
    Media,
    Tray,
    Layout,
    Volume,
    Network,
    Battery,
    Spotlight,
    ControlCenter,
    Clock,
}

const WEEKDAYS: [&str; 7] = ["Вс", "Пн", "Вт", "Ср", "Чт", "Пт", "Сб"];
const MONTHS: [&str; 12] = [
    "янв.", "февр.", "мар.", "апр.", "мая", "июн.", "июл.", "авг.", "сент.", "окт.", "нояб.", "дек.",
];

#[derive(Default)]
struct State {
    hwnd: HWND,
    hooks: Vec<HWINEVENTHOOK>,
    font: HFONT,
    bold_font: HFONT,
    glyph_font: HFONT,
    hover: Option<Hit>,
    /// The title whose menu is open (drawn pressed).
    open: Option<Hit>,
    tracking_mouse: bool,
    layout: String,
    volume: Option<Volume>,
    volume_flash: Option<Instant>,
    network: Option<Network>,
    battery: Option<Battery>,
    /// Broadcast when Explorer (re)creates its taskbar.
    taskbar_created: u32,
    /// The app in front (not one of ours), its name and menus.
    foreground: HWND,
    app: String,
    titles: Vec<Title>,
}

// Everything runs on the UI thread. Keep borrows short: cross-process calls
// (SendMessageTimeout, ShowWindow) can re-enter the window procedure.
thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn create() -> Result<HWND> {
    unsafe {
        // NOACTIVATE: clicking the bar must not steal focus from the active window.
        let hwnd = gfx::create_window(
            w!("CustomUI.MenuBar"),
            Some(wndproc),
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
        )?;
        media::start(hwnd);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.taskbar_created = taskbar_created;
        });
        update_fonts(hwnd);
        appbar::register(hwnd);
        place(hwnd);
        install_hooks();
        SetTimer(hwnd, TIMER_CLOCK, 1000, None);
        SetTimer(hwnd, TIMER_STATUS, 250, None);
        poll_fast();
        poll_slow();
        refresh(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(hwnd)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_TIMER => match wparam.0 {
            TIMER_REFRESH => {
                let _ = KillTimer(hwnd, TIMER_REFRESH);
                refresh(hwnd);
            }
            TIMER_CLOCK => {
                // Also picks up Coucou if it was started after us.
                coucou::pin();
                system_taskbar::enforce();
                tray::reassert();
                poll_slow();
                render(hwnd);
            }
            TIMER_STATUS if poll_fast() => render(hwnd),
            _ => {}
        },
        media::WM_MEDIA_CHANGED => {
            render(hwnd);
            controlcenter::refresh();
        }
        WM_SETTINGCHANGE => {
            // The wallpaper (among other things) may have changed.
            gfx::forget_wallpaper();
            render(hwnd);
        }
        WM_MOUSEMOVE => on_mouse_move(hwnd, point()),
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.hover = None;
                s.tracking_mouse = false;
            });
            render(hwnd);
        }
        WM_LBUTTONUP => on_click(hwnd, point()),
        WM_RBUTTONUP => match hit_at(hwnd, point()) {
            Some(Hit::Volume) => status::open_sound_settings(),
            _ => {
                let mut cursor = POINT::default();
                let _ = GetCursorPos(&mut cursor);
                open_menu(hwnd, Hit::Logo, RECT { left: cursor.x, top: cursor.y, right: cursor.x, bottom: cursor.y });
            }
        },
        WM_MOUSEWHEEL => {
            // Wheel coordinates are in screen space.
            let mut p = point();
            let _ = ScreenToClient(hwnd, &mut p);
            if hit_at(hwnd, p) == Some(Hit::Volume) {
                let notches = (wparam.0 >> 16 & 0xFFFF) as i16 as f32 / 120.0;
                status::change_volume(notches * VOLUME_STEP);
                flash_volume(hwnd);
            }
        }
        appbar::WM_APPBAR_CALLBACK => match wparam.0 as u32 {
            ABN_POSCHANGED => place(hwnd),
            // Get out of the way of fullscreen games/videos, come back afterwards.
            ABN_FULLSCREENAPP => {
                let after = if lparam.0 != 0 { HWND_BOTTOM } else { HWND_TOPMOST };
                let _ = SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            _ => {}
        },
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            gfx::forget_wallpaper();
            update_fonts(hwnd);
            place(hwnd);
        }
        WM_DESTROY => {
            media::stop();
            let hooks = STATE.with_borrow_mut(|s| std::mem::take(&mut s.hooks));
            for hook in hooks {
                let _ = UnhookWinEvent(hook);
            }
            coucou::release();
            appbar::unregister(hwnd);
            PostQuitMessage(0);
        }
        // Our own tray host broadcasts this too; only an Explorer restart needs handling.
        _ if msg != 0 && msg == STATE.with_borrow(|s| s.taskbar_created) && system_taskbar::explorer_restarted() => {
            // Its taskbar is back, our AppBars are forgotten, and programs just
            // re-added their icons to whichever tray was on top.
            system_taskbar::hide();
            appbar::register(hwnd);
            place(hwnd);
            dock::reregister();
            tray::reassert();
            tray::broadcast();
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

fn scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

unsafe fn place(hwnd: HWND) {
    let rc = appbar::reserve(hwnd, ABE_TOP, (bar_height() * scale(hwnd)) as i32);
    let _ = MoveWindow(hwnd, rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, false);
    render(hwnd);
}

unsafe fn update_fonts(hwnd: HWND) {
    let height = -(FONT_SIZE * scale(hwnd)) as i32;
    let font = gfx::create_font(height, FW_NORMAL, gfx::ui_font());
    let bold = gfx::create_font(height, FW_BOLD, gfx::ui_font());
    let glyphs = gfx::create_font(-(GLYPH_SIZE * scale(hwnd)) as i32, FW_NORMAL, w!("Segoe Fluent Icons"));
    let old = STATE.with_borrow_mut(|s| {
        [
            std::mem::replace(&mut s.font, font),
            std::mem::replace(&mut s.bold_font, bold),
            std::mem::replace(&mut s.glyph_font, glyphs),
        ]
    });
    for f in old.into_iter().filter(|f| !f.is_invalid()) {
        let _ = DeleteObject(f);
    }
}

unsafe fn install_hooks() {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    let hooks: Vec<_> = ranges
        .iter()
        .map(|&(min, max)| {
            SetWinEventHook(min, max, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
        })
        .filter(|h| !h.is_invalid())
        .collect();
    STATE.with_borrow_mut(|s| s.hooks = hooks);
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    // Events come in bursts; restarting the timer debounces them into one refresh.
    let bar = STATE.with_borrow(|s| s.hwnd);
    SetTimer(bar, TIMER_REFRESH, 50, None);
}

unsafe fn refresh(hwnd: HWND) {
    coucou::pin();
    dock::update(tasks::collect());
    if let Some((window, app)) = front_app() {
        let titles = if config::flag("menubar.app_menus") { appmenu::titles(window) } else { Vec::new() };
        STATE.with_borrow_mut(|s| {
            s.foreground = window;
            s.app = app;
            s.titles = titles;
        });
    }
    render(hwnd);
}

/// The window in front and its app's name, like the bold app name in macOS's
/// menu bar. `None` while one of our own windows (Spotlight, a panel) is in front.
unsafe fn front_app() -> Option<(HWND, String)> {
    let foreground = GetForegroundWindow();
    if foreground.is_invalid() {
        return Some((foreground, String::new()));
    }
    let mut pid = 0;
    GetWindowThreadProcessId(foreground, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return None;
    }
    let class = tasks::class_name(foreground);
    if class == "Progman" || class == "WorkerW" {
        return Some((foreground, "Рабочий стол".to_owned()));
    }
    let exe = tasks::exe_path(foreground);
    // Store apps all run in ApplicationFrameHost; their title is the app name.
    let app = if exe.is_empty() || exe.to_lowercase().ends_with("\\applicationframehost.exe") {
        tasks::title(foreground).unwrap_or_default()
    } else {
        apps::exe_name(&exe)
    };
    Some((foreground, app))
}

/// Bar regions in client coordinates.
struct Layout {
    /// Logo, app name, menu titles.
    left: Vec<(Hit, RECT)>,
    notch: RECT,
    media: RECT,
    /// Tray … clock, left to right.
    right: Vec<(Hit, RECT)>,
}

fn showing_volume_percent(s: &State) -> bool {
    s.hover == Some(Hit::Volume) || s.volume_flash.is_some()
}

/// Text width in `font` (measured on a scratch DC).
unsafe fn text_width(font: HFONT, text: &str) -> i32 {
    let dc = CreateCompatibleDC(None);
    let old = SelectObject(dc, font);
    let width = gfx::text_width(dc, text);
    SelectObject(dc, old);
    let _ = DeleteDC(dc);
    width
}

unsafe fn layout(client: &RECT, scale: f32, s: &State) -> Layout {
    let px = |v: f32| (v * scale).round() as i32;
    let (top, bottom) = (client.top, client.bottom);
    let slot = |left: i32, width: i32| RECT { left, top, right: left + width, bottom };
    let pad = px(ITEM_PAD);

    let notch_w = px(notch_width());
    let notch_left = (client.left + client.right - notch_w) / 2;
    let notch = RECT { left: notch_left, right: notch_left + notch_w, ..*client };

    // Left: logo, bold app name, menus — as many as fit before the notch.
    let mut left = Vec::new();
    let mut x = client.left + px(EDGE);
    let logo = slot(x, px(LOGO_WIDTH));
    x = logo.right;
    left.push((Hit::Logo, logo));
    let limit = notch.left - px(EDGE);
    if !s.app.is_empty() {
        let w = text_width(s.bold_font, &s.app) + pad * 2;
        let app = slot(x, w.min(limit - x).max(0));
        x = app.right;
        left.push((Hit::App, app));
    }
    for (i, title) in s.titles.iter().enumerate() {
        let w = text_width(s.font, &title.label) + pad * 2;
        if x + w > limit {
            break;
        }
        left.push((Hit::Menu(i), slot(x, w)));
        x += w;
    }

    // Right, from the edge inwards, in macOS's order.
    let mut items = Vec::new();
    if tray::has_icons() && config::flag("tray.enabled") {
        items.push((Hit::Tray, px(ICON_WIDTH)));
    }
    if !s.layout.is_empty() && config::flag("menubar.layout") {
        items.push((Hit::Layout, px(LAYOUT_WIDTH)));
    }
    if s.volume.is_some() && config::flag("menubar.volume") {
        items.push((Hit::Volume, px(if showing_volume_percent(s) { VOLUME_WIDE } else { ICON_WIDTH })));
    }
    if s.network.is_some() && config::flag("menubar.network") {
        items.push((Hit::Network, px(ICON_WIDTH)));
    }
    if s.battery.is_some() && config::flag("menubar.battery") {
        items.push((Hit::Battery, px(BATTERY_WIDTH)));
    }
    if config::flag("menubar.spotlight") {
        items.push((Hit::Spotlight, px(ICON_WIDTH)));
    }
    if config::flag("menubar.control_center") {
        items.push((Hit::ControlCenter, px(ICON_WIDTH)));
    }
    items.push((Hit::Clock, text_width(s.font, &clock_text()) + pad * 2));
    let mut right_x = client.right - px(EDGE);
    let mut right: Vec<(Hit, RECT)> = items
        .into_iter()
        .rev()
        .map(|(hit, width)| {
            let r = slot(right_x - width, width);
            right_x = r.left;
            (hit, r)
        })
        .collect();
    right.reverse();

    let media_left = notch.right + pad * 2;
    let media = slot(media_left, px(MEDIA_WIDTH).min(right_x - pad * 2 - media_left).max(0));
    Layout { left, notch, media, right }
}

unsafe fn client_rect(hwnd: HWND) -> RECT {
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    client
}

unsafe fn hit_at(hwnd: HWND, p: POINT) -> Option<Hit> {
    let client = client_rect(hwnd);
    let scale = scale(hwnd);
    let has_track = now_playing().is_some();
    STATE.with_borrow(|s| {
        let l = layout(&client, scale, s);
        let inside = |r: &RECT| p.x >= r.left && p.x < r.right;
        if has_track && inside(&l.media) {
            return Some(Hit::Media);
        }
        l.left.iter().chain(&l.right).find(|(_, r)| inside(r)).map(|(hit, _)| *hit)
    })
}

/// Layout and volume; returns whether anything visible changed.
fn poll_fast() -> bool {
    let layout = status::layout();
    let volume = status::volume();
    STATE.with_borrow_mut(|s| {
        let flash_over = s.volume_flash.is_some_and(|t| t.elapsed() >= VOLUME_FLASH);
        if flash_over {
            s.volume_flash = None;
        }
        let changed = flash_over || s.layout != layout || s.volume != volume;
        s.layout = layout;
        s.volume = volume;
        changed
    })
}

/// Network and battery change rarely; checked with the clock.
fn poll_slow() {
    let network = status::network();
    let battery = status::battery();
    STATE.with_borrow_mut(|s| {
        s.network = Some(network);
        s.battery = battery;
    });
}

/// Shows the volume percentage for a moment after a change.
unsafe fn flash_volume(hwnd: HWND) {
    let volume = status::volume();
    STATE.with_borrow_mut(|s| {
        s.volume = volume;
        s.volume_flash = Some(Instant::now());
    });
    render(hwnd);
}

/// Opens a left-side menu under its title (drawn pressed meanwhile).
unsafe fn open_menu(hwnd: HWND, hit: Hit, anchor: RECT) {
    let (source, window, app) = STATE.with_borrow(|s| {
        let source = match hit {
            Hit::Logo => Some(Source::System),
            Hit::App => Some(Source::App),
            Hit::Menu(i) => s.titles.get(i).map(|t| t.source.clone()),
            _ => None,
        };
        (source, s.foreground, s.app.clone())
    });
    let Some(source) = source else { return };
    STATE.with_borrow_mut(|s| s.open = Some(hit));
    render(hwnd);
    let quit = appmenu::open(&source, anchor, window, &app);
    STATE.with_borrow_mut(|s| {
        s.open = None;
        s.hover = None;
    });
    render(hwnd);
    if quit {
        let _ = DestroyWindow(hwnd);
    }
}

unsafe fn on_click(hwnd: HWND, p: POINT) {
    let Some(hit) = hit_at(hwnd, p) else { return };
    let anchor = item_on_screen(hwnd, hit);
    match hit {
        Hit::Logo | Hit::App | Hit::Menu(_) => {
            if let Some(anchor) = anchor {
                open_menu(hwnd, hit, anchor);
            }
        }
        Hit::Media => media::send(Command::TogglePlayPause),
        // The switch is applied asynchronously by the target window; the status poll shows it.
        Hit::Layout => status::next_layout(),
        Hit::Volume | Hit::Network | Hit::ControlCenter => {
            if let Some(anchor) = anchor {
                controlcenter::toggle(anchor);
            }
        }
        Hit::Tray => {
            if let Some(anchor) = anchor {
                tray::toggle_popup(anchor);
            }
        }
        Hit::Battery => status::open_power_settings(),
        Hit::Spotlight => spotlight::toggle(),
        Hit::Clock => {
            if let Some(anchor) = anchor {
                calendar::toggle(anchor);
            }
        }
    }
}

/// The playing track, unless it's turned off in the settings.
fn now_playing() -> Option<Track> {
    media::current().filter(|_| config::flag("menubar.now_playing"))
}

fn clock_text() -> String {
    let t = unsafe { GetLocalTime() };
    if !config::flag("menubar.date") {
        return format!("{:02}:{:02}", t.wHour, t.wMinute);
    }
    format!(
        "{} {} {}  {:02}:{:02}",
        WEEKDAYS[t.wDayOfWeek as usize % 7],
        t.wDay,
        MONTHS[(t.wMonth as usize).clamp(1, 12) - 1],
        t.wHour,
        t.wMinute
    )
}

fn track_text(track: &Track) -> String {
    if track.artist.is_empty() {
        format!("♪  {}", track.title)
    } else {
        format!("♪  {} — {}", track.artist, track.title)
    }
}

/// An item's rect in screen coordinates, to anchor menus and panels.
unsafe fn item_on_screen(hwnd: HWND, item: Hit) -> Option<RECT> {
    let client = client_rect(hwnd);
    let scale = scale(hwnd);
    let r = STATE.with_borrow(|s| {
        let l = layout(&client, scale, s);
        l.left.into_iter().chain(l.right).find(|(hit, _)| *hit == item).map(|(_, r)| r)
    })?;
    let mut win = RECT::default();
    let _ = GetWindowRect(hwnd, &mut win);
    Some(RECT { left: r.left + win.left, top: r.top + win.top, right: r.right + win.left, bottom: r.bottom + win.top })
}

/// Repaints from outside (the tray, when icons come and go).
pub fn redraw() {
    let hwnd = STATE.with_borrow(|s| s.hwnd);
    if !hwnd.is_invalid() {
        unsafe { render(hwnd) };
    }
}

/// Battery outline with a fill for the charge (green while charging, red when low).
unsafe fn draw_battery(dc: HDC, r: &RECT, b: &Battery, scale: f32) -> RECT {
    let px = |v: f32| (v * scale).round() as i32;
    let (w, h) = (px(22.0), px(11.0));
    let top = r.top + (r.bottom - r.top - h) / 2;
    let body = RECT { left: r.left + px(6.0), top, right: r.left + px(6.0) + w, bottom: top + h };
    gfx::fill_round(dc, &body, COLOR_DIM, px(4.0));
    let border = px(1.0).max(1);
    let inner = RECT { left: body.left + border, top: body.top + border, right: body.right - border, bottom: body.bottom - border };
    gfx::fill_round(dc, &inner, COLOR_BG, px(3.0));
    let nub = RECT { left: body.right + border, top: top + h / 3, right: body.right + border + px(2.0), bottom: top + h - h / 3 };
    gfx::fill(dc, &nub, COLOR_DIM);
    let level = RECT { left: inner.left + border, top: inner.top + border, right: inner.left + border + (inner.right - inner.left - border * 2) * b.percent as i32 / 100, bottom: inner.bottom - border };
    let color = if b.charging { COLOR_CHARGING } else if b.percent <= 20 { COLOR_LOW } else { COLOR_TEXT };
    gfx::fill_round(dc, &level, color, px(2.0));
    RECT { left: nub.right + px(5.0), ..*r }
}

/// The system menu's mark: a neutral crescent (not anyone's logo).
fn draw_logo(canvas: &mut Canvas, r: &RECT, scale: f32) {
    let (cx, cy) = ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0);
    let radius = 7.0 * scale;
    let (bite_x, bite_y, bite_r) = (cx + 3.6 * scale, cy - 2.8 * scale, 6.0 * scale);
    let width = canvas.width;
    let pixels = canvas.pixels();
    for y in (cy - radius - 1.0) as i32..=(cy + radius + 1.0) as i32 {
        for x in (cx - radius - 1.0) as i32..=(cx + radius + 1.0) as i32 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let body = (radius - ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() + 0.5).clamp(0.0, 1.0);
            let bite = (bite_r - ((fx - bite_x).powi(2) + (fy - bite_y).powi(2)).sqrt() + 0.5).clamp(0.0, 1.0);
            let a = (255.0 * body * (1.0 - bite)) as u32;
            if a > 0 && x >= 0 && y >= 0 && x < width {
                if let Some(p) = pixels.get_mut((y * width + x) as usize) {
                    let base = *p;
                    let mix = |shift: u32| (((base >> shift & 0xFF) * (255 - a) + 255 * a) / 255) << shift;
                    *p = (base & 0xFF00_0000) | mix(16) | mix(8) | mix(0);
                }
            }
        }
    }
}

/// macOS's Control Center mark: two toggles, knobs on opposite sides.
fn draw_control_center(canvas: &mut Canvas, r: &RECT, scale: f32) {
    let px = |v: f32| (v * scale).round() as i32;
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    let (w, h, gap) = (px(15.0), px(6.0), px(3.0));
    for (i, knob_left) in [(0, true), (1, false)] {
        let top = cy - h - gap / 2 + i * (h + gap);
        let pill = RECT { left: cx - w / 2, top, right: cx + w / 2, bottom: top + h };
        canvas.stroke_shape(&pill, h as f32 / 2.0, COLOR_TEXT, 255);
        let knob_x = if knob_left { pill.left } else { pill.right - h };
        canvas.fill_shape(&RECT { left: knob_x, top, right: knob_x + h, bottom: top + h }, h as f32 / 2.0, COLOR_TEXT, 255);
    }
}

/// Layered windows don't get WM_PAINT; every visual change calls this instead.
unsafe fn render(hwnd: HWND) {
    let client = client_rect(hwnd);
    let mut win = RECT::default();
    let _ = GetWindowRect(hwnd, &mut win);
    let Some(mut canvas) = Canvas::new(client.right, client.bottom) else { return };
    let scale = scale(hwnd);
    let px = |v: f32| (v * scale).round() as i32;
    let dc = canvas.dc;

    // Frosted glass: the wallpaper under the bar, blurred and tinted.
    let (screen_w, screen_h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    match gfx::frosted_wallpaper(screen_w, screen_h) {
        Some(wall) => canvas.fill_from(&wall, screen_w, win.left, win.top),
        None => gfx::fill(dc, &client, COLOR_BG),
    }

    let track = now_playing();
    let (l, logo, control_center) = STATE.with_borrow(|s| {
        let l = layout(&client, scale, s);
        let mut logo = None;
        let mut control_center = None;

        // Hover / pressed highlights first, so text sits on top of them.
        for (hit, r) in l.left.iter().chain(&l.right) {
            let alpha = if s.open == Some(*hit) {
                64
            } else if s.hover == Some(*hit) {
                36
            } else {
                continue;
            };
            let inset = px(HIGHLIGHT_INSET);
            let bar = RECT { left: r.left, top: r.top + inset, right: r.right, bottom: r.bottom - inset };
            canvas.fill_shape(&bar, HIGHLIGHT_RADIUS * scale, COLOR_HIGHLIGHT, alpha);
        }
        if track.is_some() && s.hover == Some(Hit::Media) {
            let inset = px(HIGHLIGHT_INSET);
            let bar = RECT { top: l.media.top + inset, bottom: l.media.bottom - inset, ..l.media };
            canvas.fill_shape(&bar, HIGHLIGHT_RADIUS * scale, COLOR_HIGHLIGHT, 36);
        }

        let old_font = SelectObject(dc, s.font);
        SetTextColor(dc, COLOR_TEXT);
        let pad = px(ITEM_PAD);
        for (hit, r) in &l.left {
            let text = RECT { left: r.left + pad, right: r.right - pad, ..*r };
            match hit {
                Hit::Logo => logo = Some(*r),
                Hit::App => {
                    SelectObject(dc, s.bold_font);
                    gfx::draw_text(dc, &s.app, &text, DT_LEFT | DT_END_ELLIPSIS);
                    SelectObject(dc, s.font);
                }
                Hit::Menu(i) => gfx::draw_text(dc, &s.titles[*i].label, &text, DT_LEFT),
                _ => {}
            }
        }

        if let Some(track) = &track {
            SetTextColor(dc, if track.playing { COLOR_TEXT } else { COLOR_DIM });
            let inner = RECT { left: l.media.left + pad, right: l.media.right - pad, ..l.media };
            gfx::draw_text(dc, &track_text(track), &inner, DT_CENTER | DT_END_ELLIPSIS);
        }

        for (hit, r) in &l.right {
            SetTextColor(dc, COLOR_TEXT);
            match hit {
                Hit::Layout => {
                    SelectObject(dc, s.font);
                    gfx::draw_text(dc, &s.layout, r, DT_CENTER);
                }
                Hit::Volume => {
                    let Some(volume) = &s.volume else { continue };
                    SelectObject(dc, s.glyph_font);
                    if showing_volume_percent(s) {
                        let glyph = RECT { left: r.left + px(8.0), right: r.left + px(ICON_WIDTH), ..*r };
                        gfx::draw_text(dc, status::volume_glyph(volume), &glyph, DT_LEFT);
                        SelectObject(dc, s.font);
                        let percent = if volume.muted { "выкл".to_owned() } else { format!("{}%", (volume.level * 100.0).round()) };
                        gfx::draw_text(dc, &percent, &RECT { left: glyph.right, right: r.right - px(8.0), ..*r }, DT_RIGHT);
                    } else {
                        gfx::draw_text(dc, status::volume_glyph(volume), r, DT_CENTER);
                    }
                }
                Hit::Network => {
                    SelectObject(dc, s.glyph_font);
                    let glyph = match s.network {
                        Some(Network::Wifi) => "\u{E701}",
                        Some(Network::Ethernet) => "\u{E839}",
                        _ => {
                            SetTextColor(dc, COLOR_DIM);
                            "\u{E839}"
                        }
                    };
                    gfx::draw_text(dc, glyph, r, DT_CENTER);
                }
                Hit::Battery => {
                    let Some(battery) = &s.battery else { continue };
                    let text = draw_battery(dc, r, battery, scale);
                    SelectObject(dc, s.font);
                    SetTextColor(dc, COLOR_TEXT);
                    gfx::draw_text(dc, &format!("{}%", battery.percent), &text, DT_LEFT);
                }
                Hit::Tray => {
                    SelectObject(dc, s.glyph_font);
                    gfx::draw_text(dc, "\u{E70D}", r, DT_CENTER);
                }
                Hit::Spotlight => {
                    SelectObject(dc, s.glyph_font);
                    gfx::draw_text(dc, "\u{E721}", r, DT_CENTER);
                }
                Hit::ControlCenter => control_center = Some(*r),
                Hit::Clock => {
                    SelectObject(dc, s.font);
                    gfx::draw_text(dc, &clock_text(), r, DT_CENTER);
                }
                _ => {}
            }
        }
        SelectObject(dc, old_font);
        (l, logo, control_center)
    });

    // Everything except the notch is bar; the notch lets the Coucou bubble show.
    let left = RECT { right: l.notch.left, ..client };
    let right = RECT { left: l.notch.right, ..client };
    canvas.mask(&[(left, 0.0), (right, 0.0)]);
    if let Some(r) = logo {
        draw_logo(&mut canvas, &r, scale);
    }
    if let Some(r) = control_center {
        draw_control_center(&mut canvas, &r, scale);
    }
    if let Some(chevron) = item_on_screen(hwnd, Hit::Tray) {
        tray::set_anchor(chevron);
    }
    canvas.present(hwnd, win.left, win.top, 255);
}

unsafe fn on_mouse_move(hwnd: HWND, p: POINT) {
    let start_tracking = STATE.with_borrow_mut(|s| !std::mem::replace(&mut s.tracking_mouse, true));
    if start_tracking {
        // Ask for WM_MOUSELEAVE so the hover highlight goes away.
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }
    let hover = hit_at(hwnd, p);
    let changed = STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover);
    if changed {
        render(hwnd);
    }
}
