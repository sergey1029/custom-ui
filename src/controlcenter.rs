//! Control Center: a macOS / Windows 11 quick-settings style panel under the
//! menu bar's volume and network icons. Wi-Fi and Bluetooth tiles (radio
//! toggles), volume and display brightness sliders, and now-playing controls.
//!
//! Radio and DDC/CI brightness calls are slow (and WinRT ones block), so they
//! run on a worker thread that posts results back.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

use windows::core::{w, Result};
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Networking::Connectivity::NetworkInformation;
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, rgb, Canvas, COLOR_DIM, COLOR_TEXT};
use crate::media::{self, Command};
use crate::status::{self, Network, Volume};
use crate::apps;

/// Posted by the worker when radio / brightness / connection info is fresh.
const WM_CC_UPDATE: u32 = WM_APP + 5;

const WIDTH: f32 = 340.0;
const PAD: f32 = 14.0;
const GAP: f32 = 10.0;
const TILE_H: f32 = 62.0;
const CIRCLE: f32 = 36.0;
const LABEL_H: f32 = 22.0;
const SLIDER_ROW: f32 = 30.0;
const TRACK_H: f32 = 6.0;
const KNOB: f32 = 16.0;
const MEDIA_H: f32 = 64.0;
const BUTTON: f32 = 34.0;
const FOOTER_H: f32 = 26.0;
const RADIUS: f32 = 20.0;
const CARD_RADIUS: f32 = 16.0;
const OFFSET: f32 = 6.0;
const FONT_SIZE: f32 = 13.0;
const SMALL_FONT: f32 = 12.0;
const GLYPH_SIZE: f32 = 16.0;
/// Taller than the panel ever gets (all rows shown), for capturing its backdrop.
const MAX_HEIGHT: f32 = 460.0;

const TIMER_POLL: usize = 1;

const COLOR_PANEL: COLORREF = rgb(30, 30, 34);
const COLOR_OFF: COLORREF = rgb(70, 70, 80);
const COLOR_TRACK: COLORREF = rgb(66, 66, 76);
const COLOR_BUTTON_HOVER: COLORREF = rgb(64, 64, 74);

/// What the worker knows. `None` radios mean the machine has no such adapter.
#[derive(Clone, Default, PartialEq)]
struct Snapshot {
    wifi: Option<bool>,
    bluetooth: Option<bool>,
    /// 0..=100; `None` if no monitor answers DDC/CI.
    brightness: Option<u32>,
    /// Name of the network that provides internet (SSID for Wi-Fi).
    connection: String,
}

enum Job {
    Refresh,
    Radio(RadioKind, bool),
    Brightness(u32),
}

static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);
static JOBS: Mutex<Option<Sender<Job>>> = Mutex::new(None);

fn submit(job: Job) {
    if let Some(tx) = &*JOBS.lock().unwrap() {
        let _ = tx.send(job);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Wifi,
    Bluetooth,
    Mute,
    Volume,
    Brightness,
    Previous,
    PlayPause,
    Next,
    Settings,
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    visible: bool,
    previous: HWND,
    /// Frosted glass: the screen under the panel, captured when it opened.
    backdrop: Vec<u32>,
    anchor: RECT,
    snapshot: Snapshot,
    volume: Option<Volume>,
    network: Option<Network>,
    hover: Option<Part>,
    pressed: Option<Part>,
    dragging: Option<Part>,
    tracking_mouse: bool,
    font: HFONT,
    bold_font: HFONT,
    small_font: HFONT,
    glyph_font: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn create() -> Result<()> {
    unsafe {
        let hwnd = gfx::create_window(
            w!("CustomUI.ControlCenter"),
            Some(wndproc),
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        )?;
        let dpi = scale();
        let size = |v: f32| -(v * dpi) as i32;
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.font = gfx::create_font(size(FONT_SIZE), FW_NORMAL, gfx::ui_font());
            s.bold_font = gfx::create_font(size(FONT_SIZE), FW_SEMIBOLD, gfx::ui_font());
            s.small_font = gfx::create_font(size(SMALL_FONT), FW_NORMAL, gfx::ui_font());
            s.glyph_font = gfx::create_font(size(GLYPH_SIZE), FW_NORMAL, w!("Segoe Fluent Icons"));
        });

        let (tx, rx) = mpsc::channel();
        let notify = hwnd.0 as isize;
        std::thread::spawn(move || worker(rx, notify));
        *JOBS.lock().unwrap() = Some(tx);
    }
    Ok(())
}

pub fn destroy() {
    JOBS.lock().unwrap().take();
    let hwnd = STATE.with_borrow_mut(|s| std::mem::take(&mut s.hwnd));
    if !hwnd.is_invalid() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Opens under `anchor` (the clicked menu bar icon, screen coordinates) or closes.
pub fn toggle(anchor: RECT) {
    if STATE.with_borrow(|s| s.visible) {
        return hide();
    }
    submit(Job::Refresh);
    let hwnd = STATE.with_borrow_mut(|s| {
        s.anchor = anchor;
        s.visible = true;
        s.hover = None;
        s.volume = status::volume();
        s.network = Some(status::network());
        if let Some(snapshot) = SNAPSHOT.lock().unwrap().clone() {
            s.snapshot = snapshot;
        }
        s.hwnd
    });
    unsafe {
        // Frosted glass: what's under the panel (generously tall, it grows with
        // brightness and media rows), captured before it shows.
        let dpi = scale();
        let width = (WIDTH * dpi).round() as i32;
        let at = STATE.with_borrow(|s| origin(s, width, dpi));
        let height = (MAX_HEIGHT * dpi).round() as i32;
        let backdrop = gfx::frosted(RECT { left: at.x, top: at.y, right: at.x + width, bottom: at.y + height }).unwrap_or_default();
        STATE.with_borrow_mut(|s| s.backdrop = backdrop);
        render(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let previous = gfx::force_foreground(hwnd);
        STATE.with_borrow_mut(|s| s.previous = previous);
        SetTimer(hwnd, TIMER_POLL, 300, None);
    }
}

fn hide() {
    let hwnd = STATE.with_borrow_mut(|s| {
        s.tracking_mouse = false;
        s.dragging = None;
        if std::mem::replace(&mut s.visible, false) { s.hwnd } else { HWND::default() }
    });
    if !hwnd.is_invalid() {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_POLL);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Repaints if open (e.g. the track changed).
pub fn refresh() {
    let (hwnd, visible) = STATE.with_borrow(|s| (s.hwnd, s.visible));
    if visible {
        unsafe { render(hwnd) };
    }
}

fn scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

// ---- worker ----

fn worker(rx: Receiver<Job>, notify: isize) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let _ = Radio::RequestAccessAsync().and_then(|op| op.get());
    while let Ok(job) = rx.recv() {
        let mut jobs = vec![job];
        jobs.extend(rx.try_iter());
        // While a slider is dragged only the newest brightness matters.
        if let Some(level) = jobs.iter().rev().find_map(|j| match j {
            Job::Brightness(level) => Some(*level),
            _ => None,
        }) {
            set_brightness(level);
        }
        for job in &jobs {
            if let Job::Radio(kind, on) = job {
                set_radio(*kind, *on);
            }
        }
        // Re-reading brightness mid-drag would make the slider jump back.
        if jobs.iter().all(|j| matches!(j, Job::Brightness(_))) {
            continue;
        }
        let radios = radios();
        let snapshot = Snapshot {
            wifi: radio_state(&radios, RadioKind::WiFi),
            bluetooth: radio_state(&radios, RadioKind::Bluetooth),
            brightness: brightness(),
            connection: NetworkInformation::GetInternetConnectionProfile()
                .and_then(|p| p.ProfileName())
                .map(|n| n.to_string())
                .unwrap_or_default(),
        };
        *SNAPSHOT.lock().unwrap() = Some(snapshot);
        unsafe {
            let _ = PostMessageW(HWND(notify as *mut _), WM_CC_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

fn radios() -> Vec<Radio> {
    Radio::GetRadiosAsync()
        .and_then(|op| op.get())
        .map(|list| (0..list.Size().unwrap_or(0)).filter_map(|i| list.GetAt(i).ok()).collect())
        .unwrap_or_default()
}

fn radio_state(radios: &[Radio], kind: RadioKind) -> Option<bool> {
    radios
        .iter()
        .find(|r| r.Kind().ok() == Some(kind))
        .and_then(|r| r.State().ok())
        .map(|state| state == RadioState::On)
}

fn set_radio(kind: RadioKind, on: bool) {
    let state = if on { RadioState::On } else { RadioState::Off };
    for radio in radios().iter().filter(|r| r.Kind().ok() == Some(kind)) {
        let _ = radio.SetStateAsync(state).and_then(|op| op.get());
    }
}

/// Every physical monitor; the caller destroys the handles.
unsafe fn physical_monitors() -> Vec<PHYSICAL_MONITOR> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        (*(data.0 as *mut Vec<HMONITOR>)).push(monitor);
        true.into()
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut monitors as *mut Vec<HMONITOR> as isize));
    let mut physical = Vec::new();
    for monitor in monitors {
        let mut count = 0;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).is_err() || count == 0 {
            continue;
        }
        let mut batch = vec![PHYSICAL_MONITOR::default(); count as usize];
        if GetPhysicalMonitorsFromHMONITOR(monitor, &mut batch).is_ok() {
            physical.extend(batch);
        }
    }
    physical
}

/// DDC/CI brightness of the first monitor that supports it, as 0..=100.
fn brightness() -> Option<u32> {
    unsafe {
        let monitors = physical_monitors();
        let level = monitors.iter().find_map(|m| {
            let (mut min, mut current, mut max) = (0, 0, 0);
            let ok = GetMonitorBrightness(m.hPhysicalMonitor, &mut min, &mut current, &mut max) != 0;
            (ok && max > min).then(|| (current.saturating_sub(min)) * 100 / (max - min))
        });
        let _ = DestroyPhysicalMonitors(&monitors);
        level
    }
}

fn set_brightness(percent: u32) {
    unsafe {
        let monitors = physical_monitors();
        for m in &monitors {
            let (mut min, mut current, mut max) = (0, 0, 0);
            if GetMonitorBrightness(m.hPhysicalMonitor, &mut min, &mut current, &mut max) != 0 && max > min {
                let _ = SetMonitorBrightness(m.hPhysicalMonitor, min + (max - min) * percent.min(100) / 100);
            }
        }
        let _ = DestroyPhysicalMonitors(&monitors);
    }
}

// ---- UI ----

struct Layout {
    width: i32,
    height: i32,
    /// Cards behind groups: tiles, sliders, media.
    cards: Vec<RECT>,
    parts: Vec<(Part, RECT)>,
    /// Section captions inside cards.
    labels: Vec<(&'static str, RECT)>,
    /// Full slider rows (the knob travels inside `parts`' slider rects).
    media: Option<RECT>,
}

fn layout(s: &State, has_track: bool, dpi: f32) -> Layout {
    let px = |v: f32| (v * dpi).round() as i32;
    let rect = |x: f32, y: f32, w: f32, h: f32| RECT { left: px(x), top: px(y), right: px(x + w), bottom: px(y + h) };
    let inner = WIDTH - PAD * 2.0;
    let mut cards = Vec::new();
    let mut parts = Vec::new();
    let mut labels = Vec::new();
    let mut y = PAD;

    let tile_w = (inner - GAP) / 2.0;
    let wifi = rect(PAD, y, tile_w, TILE_H);
    let bluetooth = rect(PAD + tile_w + GAP, y, tile_w, TILE_H);
    cards.extend([wifi, bluetooth]);
    parts.extend([(Part::Wifi, wifi), (Part::Bluetooth, bluetooth)]);
    y += TILE_H + GAP;

    let mut slider = |caption: &'static str, icon: Part, track: Part, y: &mut f32| {
        let card_h = LABEL_H + SLIDER_ROW + 10.0;
        cards.push(rect(PAD, *y, inner, card_h));
        labels.push((caption, rect(PAD + 14.0, *y + 6.0, inner - 28.0, LABEL_H)));
        let row = *y + 6.0 + LABEL_H;
        parts.push((icon, rect(PAD + 8.0, row, SLIDER_ROW, SLIDER_ROW)));
        parts.push((track, rect(PAD + 14.0 + SLIDER_ROW, row, inner - SLIDER_ROW - 30.0, SLIDER_ROW)));
        *y += card_h + GAP;
    };
    slider("Звук", Part::Mute, Part::Volume, &mut y);
    if s.snapshot.brightness.is_some() {
        // The sun icon isn't clickable; reuse the slider part for it.
        slider("Яркость", Part::Brightness, Part::Brightness, &mut y);
    }

    let media = has_track.then(|| {
        let card = rect(PAD, y, inner, MEDIA_H);
        cards.push(card);
        let top = y + (MEDIA_H - BUTTON) / 2.0;
        let right = PAD + inner - 10.0;
        for (i, part) in [Part::Previous, Part::PlayPause, Part::Next].into_iter().enumerate() {
            parts.push((part, rect(right - BUTTON * (3 - i) as f32, top, BUTTON, BUTTON)));
        }
        y += MEDIA_H + GAP;
        card
    });

    parts.push((Part::Settings, rect(PAD + inner - 130.0, y, 130.0, FOOTER_H)));
    y += FOOTER_H;
    Layout { width: px(WIDTH), height: px(y + PAD - 4.0), cards, parts, labels, media }
}

fn part_rect(l: &Layout, part: Part) -> Option<RECT> {
    l.parts.iter().find(|(p, _)| *p == part).map(|(_, r)| *r)
}

/// The slider track's rect (the second one when a part has two, as Brightness does).
fn slider_rect(l: &Layout, part: Part) -> Option<RECT> {
    l.parts.iter().rfind(|(p, _)| *p == part).map(|(_, r)| *r)
}

unsafe fn origin(s: &State, width: i32, dpi: f32) -> POINT {
    // Right edge a little past the clicked icon, kept on screen like macOS's.
    let screen = GetSystemMetrics(SM_CXSCREEN);
    let margin = (8.0 * dpi) as i32;
    let right = (s.anchor.right + (60.0 * dpi) as i32).min(screen - margin);
    POINT { x: (right - width).max(margin), y: s.anchor.bottom + (OFFSET * dpi) as i32 }
}

unsafe fn render(hwnd: HWND) {
    let dpi = scale();
    let track = media::current();
    let (l, origin) = STATE.with_borrow(|s| {
        let l = layout(s, track.is_some(), dpi);
        let origin = origin(s, l.width, dpi);
        (l, origin)
    });
    let Some(mut canvas) = Canvas::new(l.width, l.height) else { return };
    STATE.with_borrow(|s| draw(&mut canvas, s, &l, track.as_ref(), dpi));
    canvas.present(hwnd, origin.x, origin.y, 255);
}

unsafe fn draw(canvas: &mut Canvas, s: &State, l: &Layout, track: Option<&media::Track>, dpi: f32) {
    let px = |v: f32| (v * dpi).round() as i32;
    let dc = canvas.dc;
    canvas.backdrop(&s.backdrop, l.width, COLOR_PANEL);

    // macOS's Control Center modules: lighter translucent cards on the glass.
    for card in &l.cards {
        let hovered = l.parts.iter().any(|(p, r)| r == card && s.hover == Some(*p));
        canvas.fill_shape(card, CARD_RADIUS * dpi, COLOR_TEXT, if hovered { 34 } else { 20 });
    }

    // Tiles.
    let (wifi_on, wifi_title, wifi_sub, wifi_glyph) = match (s.snapshot.wifi, s.network) {
        (Some(true), Some(Network::Wifi)) => (true, "Wi-Fi", s.snapshot.connection.clone(), "\u{E701}"),
        (Some(true), _) => (true, "Wi-Fi", "Не подключено".to_owned(), "\u{E701}"),
        (Some(false), _) => (false, "Wi-Fi", "Выкл.".to_owned(), "\u{E701}"),
        (None, Some(Network::Ethernet)) => {
            let name = if s.snapshot.connection.is_empty() { "Подключено".to_owned() } else { s.snapshot.connection.clone() };
            (true, "Ethernet", name, "\u{E839}")
        }
        (None, _) => (false, "Сеть", "Нет подключения".to_owned(), "\u{E839}"),
    };
    let (bt_on, bt_sub) = match s.snapshot.bluetooth {
        Some(true) => (true, "Вкл."),
        Some(false) => (false, "Выкл."),
        None => (false, "Недоступно"),
    };
    for (part, on, title, subtitle, glyph) in [
        (Part::Wifi, wifi_on, wifi_title, wifi_sub.as_str(), wifi_glyph),
        (Part::Bluetooth, bt_on, "Bluetooth", bt_sub, "\u{E702}"),
    ] {
        let Some(r) = part_rect(l, part) else { continue };
        let size = px(CIRCLE);
        let circle = RECT { left: r.left + px(12.0), top: r.top + (r.bottom - r.top - size) / 2, right: r.left + px(12.0) + size, bottom: 0 };
        let circle = RECT { bottom: circle.top + size, ..circle };
        gfx::fill_round(dc, &circle, if on { gfx::accent() } else { COLOR_OFF }, size);
        SelectObject(dc, s.glyph_font);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, glyph, &circle, DT_CENTER);
        let text_left = circle.right + px(10.0);
        let mid = (r.top + r.bottom) / 2;
        SelectObject(dc, s.bold_font);
        gfx::draw_text(dc, title, &RECT { left: text_left, top: r.top + px(8.0), right: r.right - px(8.0), bottom: mid + px(1.0) }, DT_LEFT | DT_END_ELLIPSIS);
        SelectObject(dc, s.small_font);
        SetTextColor(dc, COLOR_DIM);
        gfx::draw_text(dc, subtitle, &RECT { left: text_left, top: mid - px(1.0), right: r.right - px(8.0), bottom: r.bottom - px(8.0) }, DT_LEFT | DT_END_ELLIPSIS);
    }

    // Slider captions.
    SelectObject(dc, s.bold_font);
    SetTextColor(dc, COLOR_TEXT);
    for (caption, r) in &l.labels {
        gfx::draw_text(dc, caption, r, DT_LEFT);
    }

    // Volume and brightness.
    let mut sliders = Vec::new();
    if let (Some(icon), Some(track)) = (part_rect(l, Part::Mute), slider_rect(l, Part::Volume)) {
        let (glyph, value, muted) = match &s.volume {
            Some(v) => (status::volume_glyph(v), v.level, v.muted),
            None => ("\u{E74F}", 0.0, true),
        };
        if s.hover == Some(Part::Mute) {
            gfx::fill_round(dc, &icon, COLOR_BUTTON_HOVER, icon.right - icon.left);
        }
        sliders.push((icon, track, glyph, value, muted));
    }
    if let (Some(icon), Some(value)) = (part_rect(l, Part::Brightness), s.snapshot.brightness) {
        let track = slider_rect(l, Part::Brightness).unwrap_or(icon);
        sliders.push((icon, track, "\u{E706}", value as f32 / 100.0, false));
    }
    let mut knobs = Vec::new();
    for (icon, track, glyph, value, muted) in sliders {
        SelectObject(dc, s.glyph_font);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, glyph, &icon, DT_CENTER);
        let h = px(TRACK_H);
        let mid = (track.top + track.bottom) / 2;
        let bar = RECT { left: track.left, top: mid - h / 2, right: track.right, bottom: mid - h / 2 + h };
        gfx::fill_round(dc, &bar, COLOR_TRACK, h);
        let x = bar.left + ((bar.right - bar.left) as f32 * value.clamp(0.0, 1.0)) as i32;
        gfx::fill_round(dc, &RECT { right: x.max(bar.left + h), ..bar }, if muted { COLOR_DIM } else { COLOR_TEXT }, h);
        knobs.push((x, mid));
    }

    // Now playing.
    if let (Some(card), Some(track)) = (l.media, track) {
        let text_right = part_rect(l, Part::Previous).map_or(card.right, |r| r.left) - px(8.0);
        let mid = (card.top + card.bottom) / 2;
        SelectObject(dc, s.bold_font);
        SetTextColor(dc, COLOR_TEXT);
        gfx::draw_text(dc, &track.title, &RECT { left: card.left + px(14.0), top: card.top + px(10.0), right: text_right, bottom: mid + px(1.0) }, DT_LEFT | DT_END_ELLIPSIS);
        SelectObject(dc, s.small_font);
        SetTextColor(dc, COLOR_DIM);
        gfx::draw_text(dc, &track.artist, &RECT { left: card.left + px(14.0), top: mid - px(1.0), right: text_right, bottom: card.bottom - px(10.0) }, DT_LEFT | DT_END_ELLIPSIS);
        SelectObject(dc, s.glyph_font);
        for (part, glyph) in [
            (Part::Previous, "\u{E892}"),
            (Part::PlayPause, if track.playing { "\u{E769}" } else { "\u{E768}" }),
            (Part::Next, "\u{E893}"),
        ] {
            let Some(r) = part_rect(l, part) else { continue };
            if s.hover == Some(part) {
                gfx::fill_round(dc, &r, COLOR_BUTTON_HOVER, r.right - r.left);
            }
            SetTextColor(dc, COLOR_TEXT);
            gfx::draw_text(dc, glyph, &r, DT_CENTER);
        }
    }

    if let Some(r) = part_rect(l, Part::Settings) {
        SelectObject(dc, s.small_font);
        SetTextColor(dc, if s.hover == Some(Part::Settings) { COLOR_TEXT } else { COLOR_DIM });
        gfx::draw_text(dc, "Все параметры  \u{203A}", &RECT { right: r.right - px(4.0), ..r }, DT_RIGHT);
    }

    canvas.finish_panel(RADIUS * dpi);

    // Knobs last so they're antialiased circles.
    let knob = px(KNOB);
    for (x, y) in knobs {
        let r = RECT { left: x - knob / 2, top: y - knob / 2, right: x + knob / 2, bottom: y + knob / 2 };
        canvas.fill_shape(&r, knob as f32 / 2.0, COLOR_TEXT, 255);
    }
}

unsafe fn part_at(p: POINT) -> Option<Part> {
    let dpi = scale();
    let has_track = media::current().is_some();
    STATE.with_borrow(|s| {
        let l = layout(s, has_track, dpi);
        l.parts
            .iter()
            .rev()
            .find(|(_, r)| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom)
            .map(|(part, _)| *part)
    })
}

/// Moves a slider to the cursor.
unsafe fn drag_to(hwnd: HWND, part: Part, x: i32) {
    let dpi = scale();
    let has_track = media::current().is_some();
    let Some(track) = STATE.with_borrow(|s| slider_rect(&layout(s, has_track, dpi), part)) else { return };
    let value = ((x - track.left) as f32 / (track.right - track.left).max(1) as f32).clamp(0.0, 1.0);
    match part {
        Part::Volume => {
            status::set_volume(value);
            STATE.with_borrow_mut(|s| s.volume = Some(Volume { level: value, muted: false }));
        }
        Part::Brightness => {
            let percent = (value * 100.0).round() as u32;
            submit(Job::Brightness(percent));
            STATE.with_borrow_mut(|s| s.snapshot.brightness = Some(percent));
        }
        _ => return,
    }
    render(hwnd);
}

unsafe fn activate(hwnd: HWND, part: Part) {
    let (wifi, bluetooth) = STATE.with_borrow(|s| (s.snapshot.wifi, s.snapshot.bluetooth));
    match part {
        Part::Wifi => match wifi {
            Some(on) => {
                // Optimistic; the worker's refresh confirms.
                STATE.with_borrow_mut(|s| s.snapshot.wifi = Some(!on));
                submit(Job::Radio(RadioKind::WiFi, !on));
            }
            None => {
                hide();
                status::open_network();
            }
        },
        Part::Bluetooth => match bluetooth {
            Some(on) => {
                STATE.with_borrow_mut(|s| s.snapshot.bluetooth = Some(!on));
                submit(Job::Radio(RadioKind::Bluetooth, !on));
            }
            None => {
                hide();
                apps::open("ms-settings:bluetooth");
            }
        },
        Part::Mute => {
            status::toggle_mute();
            STATE.with_borrow_mut(|s| s.volume = status::volume());
        }
        Part::Previous => media::send(Command::Previous),
        Part::PlayPause => media::send(Command::TogglePlayPause),
        Part::Next => media::send(Command::Next),
        Part::Settings => {
            hide();
            apps::open("ms-settings:");
            return;
        }
        Part::Volume | Part::Brightness => {}
    }
    render(hwnd);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => hide(),
        WM_KEYDOWN if wparam.0 == 0x1B => {
            hide();
            gfx::restore_foreground(STATE.with_borrow(|s| s.previous));
        }
        WM_CC_UPDATE => {
            if let Some(snapshot) = SNAPSHOT.lock().unwrap().clone() {
                STATE.with_borrow_mut(|s| {
                    let dragging_brightness = s.dragging == Some(Part::Brightness);
                    let brightness = s.snapshot.brightness;
                    s.snapshot = snapshot;
                    if dragging_brightness {
                        s.snapshot.brightness = brightness;
                    }
                });
            }
            refresh();
        }
        WM_TIMER if wparam.0 == TIMER_POLL => {
            // Volume keys, other apps and cable changes while we're open.
            let (volume, network) = (status::volume(), Some(status::network()));
            let changed = STATE.with_borrow_mut(|s| {
                if s.dragging.is_some() {
                    return false;
                }
                let changed = s.volume != volume || s.network != network;
                s.volume = volume;
                s.network = network;
                changed
            });
            if changed {
                render(hwnd);
            }
        }
        WM_MOUSEMOVE => {
            let p = point();
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
            if let Some(part) = STATE.with_borrow(|s| s.dragging) {
                drag_to(hwnd, part, p.x);
            } else {
                let hover = part_at(p);
                if STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hover, hover) != hover) {
                    render(hwnd);
                }
            }
        }
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.tracking_mouse = false;
                s.hover = None;
            });
            render(hwnd);
        }
        WM_LBUTTONDOWN => {
            let part = part_at(point());
            STATE.with_borrow_mut(|s| s.pressed = part);
            if let Some(part @ (Part::Volume | Part::Brightness)) = part {
                STATE.with_borrow_mut(|s| s.dragging = Some(part));
                SetCapture(hwnd);
                drag_to(hwnd, part, point().x);
            }
        }
        WM_LBUTTONUP => {
            let (pressed, dragging) = STATE.with_borrow_mut(|s| (s.pressed.take(), s.dragging.take()));
            if dragging.is_some() {
                let _ = ReleaseCapture();
            } else if let Some(part) = part_at(point()).filter(|p| Some(*p) == pressed) {
                activate(hwnd, part);
            }
        }
        WM_RBUTTONUP => match part_at(point()) {
            Some(Part::Wifi) => {
                hide();
                status::open_network();
            }
            Some(Part::Bluetooth) => {
                hide();
                apps::open("ms-settings:bluetooth");
            }
            Some(Part::Mute | Part::Volume) => {
                hide();
                status::open_sound_settings();
            }
            _ => {}
        },
        WM_CAPTURECHANGED => STATE.with_borrow_mut(|s| s.dragging = None),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}
