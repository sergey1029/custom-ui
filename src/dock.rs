//! The dock: a floating macOS-style strip at the bottom with the launcher,
//! pinned apps, running apps and the trash. Icons magnify under the cursor,
//! bounce while an app starts, can be dragged to reorder (or dragged up and
//! out to unpin) and have context menus. Pins persist in
//! %APPDATA%\CustomUI\dock.tsv.

use std::cell::RefCell;
use std::collections::HashMap;
use std::f32::consts::PI;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{SHEmptyRecycleBinW, ABE_BOTTOM, ABN_FULLSCREENAPP, ABN_POSCHANGED};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, rgb, Canvas, COLOR_BG, COLOR_TEXT};
use crate::tasks::{self, Task};
use crate::macmenu::{Anchor, Menu};
use crate::{appbar, apps, config, icons, launchpad};

fn icon_size() -> f32 {
    config::int("dock.icon_size") as f32
}

const GAP: f32 = 6.0;
const PAD: f32 = 8.0;
const SEPARATOR: f32 = 13.0;
const MARGIN: f32 = 6.0;
const RADIUS: f32 = 20.0;

fn max_scale() -> f32 {
    if config::flag("dock.magnify") { config::int("dock.magnify_max") as f32 / 100.0 } else { 1.0 }
}

/// Magnification fades out this many icon widths away from the cursor.
const REACH: f32 = 2.6;
/// Room above the dock for bounces and the name label, plus whatever magnification needs.
const HEADROOM: f32 = 70.0;
const LABEL_H: f32 = 26.0;
const LABEL_GAP: f32 = 10.0;
const BOUNCE: Duration = Duration::from_millis(1800);
const BOUNCE_PERIOD: f32 = 0.6;
const DRAG_THRESHOLD: i32 = 6;
/// Dragging a pin this far above the dock and letting go unpins it.
const UNPIN_DISTANCE: f32 = 70.0;
/// Source size for icons; plenty for magnification at high DPI.
const ICON_LOAD: i32 = 256;
const FONT_SIZE: f32 = 13.0;

const TIMER_ANIM: usize = 1;

const COLOR_LABEL: COLORREF = rgb(40, 40, 46);
const COLOR_DOT: COLORREF = rgb(210, 210, 220);
const COLOR_SEPARATOR: COLORREF = rgb(80, 80, 90);
const LAUNCHPAD_TOP: COLORREF = rgb(246, 246, 250);
const LAUNCHPAD_BOTTOM: COLORREF = rgb(196, 198, 210);
const LAUNCHPAD_TILES: [COLORREF; 9] = [
    rgb(255, 105, 97),
    rgb(255, 159, 67),
    rgb(254, 202, 87),
    rgb(72, 199, 142),
    rgb(56, 189, 204),
    rgb(84, 160, 255),
    rgb(95, 111, 230),
    rgb(165, 105, 230),
    rgb(240, 98, 166),
];
const COLOR_PLACEHOLDER: COLORREF = rgb(78, 78, 92);

const TRASH: &str = "shell:RecycleBinFolder";

const CMD_OPEN: usize = 1;
const CMD_PIN: usize = 2;
const CMD_REVEAL: usize = 3;
const CMD_QUIT: usize = 4;
const CMD_EMPTY_TRASH: usize = 5;
const CMD_WINDOW: usize = 100;

/// An app the dock knows how to start and recognize.
#[derive(Clone)]
pub struct Pin {
    pub name: String,
    /// Handed to ShellExecute; also the icon source and the pin's identity.
    pub launch: String,
    pub aumid: Option<String>,
    pub path: Option<String>,
}

impl Pin {
    fn matches(&self, task: &Task) -> bool {
        self.aumid.as_deref().is_some_and(|id| task.aumid.as_deref() == Some(id))
            || self.path.as_deref().is_some_and(|p| !task.exe.is_empty() && p.eq_ignore_ascii_case(&task.exe))
    }

    fn for_task(task: &Task) -> Pin {
        // Store apps all run inside ApplicationFrameHost; only their AUMID tells them apart.
        let hosted = task.exe.is_empty() || task.exe.to_lowercase().ends_with("\\applicationframehost.exe");
        match &task.aumid {
            Some(id) if hosted => {
                let launch = format!("shell:AppsFolder\\{id}");
                let name = apps::item_name(&launch).unwrap_or_else(|| task.title.clone());
                Pin { name, launch, aumid: Some(id.clone()), path: None }
            }
            _ => Pin {
                name: apps::exe_name(&task.exe),
                launch: task.exe.clone(),
                aumid: task.aumid.clone(),
                path: Some(task.exe.clone()),
            },
        }
    }
}

enum Kind {
    Launchpad,
    App { pin: Pin, pinned: bool },
    Trash,
}

struct Item {
    kind: Kind,
    /// The app's windows, most recently active first.
    windows: Vec<HWND>,
}

impl Item {
    fn name(&self) -> &str {
        match &self.kind {
            Kind::Launchpad => "Приложения",
            Kind::App { pin, .. } => &pin.name,
            Kind::Trash => "Корзина",
        }
    }

    fn kind_launch(&self) -> Option<&str> {
        match &self.kind {
            Kind::App { pin, .. } => Some(&pin.launch),
            _ => None,
        }
    }
}

enum Slot {
    Item(usize),
    Separator,
}

struct Drag {
    launch: String,
    at: POINT,
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    pins: Vec<Pin>,
    tasks: Vec<Task>,
    items: Vec<Item>,
    slots: Vec<Slot>,
    /// Current magnification per item; eases toward the cursor-driven target.
    scales: Vec<f32>,
    cursor: Option<POINT>,
    tracking_mouse: bool,
    animating: bool,
    /// Launch key → when the app was started; cleared once it shows a window.
    bounces: HashMap<String, Instant>,
    press: Option<(usize, POINT)>,
    drag: Option<Drag>,
    font: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn create() -> Result<HWND> {
    unsafe {
        let hwnd = gfx::create_window(
            w!("CustomUI.Dock"),
            Some(wndproc),
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
        )?;
        STATE.with_borrow_mut(|s| {
            s.hwnd = hwnd;
            s.pins = load_pins();
        });
        rebuild();
        update_font(hwnd);
        // Turned off: the window still exists (the menu bar feeds it the window
        // list), it just never shows or reserves space.
        if config::flag("dock.enabled") {
            appbar::register(hwnd);
            place(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(hwnd)
    }
}

pub fn destroy() {
    let hwnd = STATE.with_borrow(|s| s.hwnd);
    if !hwnd.is_invalid() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// After Explorer restarts every AppBar has to register again.
pub fn reregister() {
    let hwnd = STATE.with_borrow(|s| s.hwnd);
    if !config::flag("dock.enabled") {
        return;
    }
    appbar::register(hwnd);
    unsafe { place(hwnd) };
}

/// New window list from the menu bar's window-event hooks.
pub fn update(tasks: Vec<Task>) {
    STATE.with_borrow_mut(|s| s.tasks = tasks);
    rebuild();
    redraw();
}

pub fn is_pinned(launch: &str) -> bool {
    STATE.with_borrow(|s| s.pins.iter().any(|p| p.launch == launch))
}

/// Pins the app, or unpins it if it already is.
pub fn toggle_pin(pin: Pin) {
    STATE.with_borrow_mut(|s| {
        if let Some(i) = s.pins.iter().position(|p| p.launch == pin.launch) {
            s.pins.remove(i);
        } else {
            s.pins.push(pin);
        }
        save_pins(&s.pins);
    });
    rebuild();
    redraw();
}

fn redraw() {
    let hwnd = STATE.with_borrow(|s| s.hwnd);
    if !hwnd.is_invalid() {
        unsafe { render(hwnd) };
    }
}

/// Recomputes items and slots from pins and windows. Name lookups may call into
/// the shell, so this runs outside any state borrow.
fn rebuild() {
    let (pins, tasks) = STATE.with_borrow(|s| (s.pins.clone(), s.tasks.clone()));
    let (items, slots) = build(&pins, &tasks);
    STATE.with_borrow_mut(|s| {
        for item in &items {
            if let Kind::App { pin, .. } = &item.kind {
                if !item.windows.is_empty() {
                    s.bounces.remove(&pin.launch);
                }
            }
        }
        s.scales.resize(items.len(), 1.0);
        s.items = items;
        s.slots = slots;
    });
}

/// Launcher, pinned apps | running unpinned apps | trash.
fn build(pins: &[Pin], tasks: &[Task]) -> (Vec<Item>, Vec<Slot>) {
    let mut claimed = vec![false; tasks.len()];
    let mut items = vec![Item { kind: Kind::Launchpad, windows: Vec::new() }];
    for pin in pins {
        let mut windows = Vec::new();
        for (i, task) in tasks.iter().enumerate() {
            if !claimed[i] && pin.matches(task) {
                claimed[i] = true;
                windows.push(task.hwnd);
            }
        }
        items.push(Item { kind: Kind::App { pin: pin.clone(), pinned: true }, windows });
    }
    let pinned_end = items.len();

    let mut groups: Vec<(String, usize)> = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        if claimed[i] {
            continue;
        }
        let key = task.aumid.clone().unwrap_or_else(|| task.exe.to_lowercase());
        if let Some(&(_, index)) = groups.iter().find(|(k, _)| *k == key) {
            items[index].windows.push(task.hwnd);
        } else {
            groups.push((key, items.len()));
            items.push(Item { kind: Kind::App { pin: Pin::for_task(task), pinned: false }, windows: vec![task.hwnd] });
        }
    }

    let mut slots: Vec<Slot> = (0..pinned_end).map(Slot::Item).collect();
    if items.len() > pinned_end {
        slots.push(Slot::Separator);
        slots.extend((pinned_end..items.len()).map(Slot::Item));
    }
    if config::flag("dock.trash") {
        slots.push(Slot::Separator);
        slots.push(Slot::Item(items.len()));
        items.push(Item { kind: Kind::Trash, windows: Vec::new() });
    }
    (items, slots)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let point = || POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: (lparam.0 >> 16 & 0xFFFF) as i16 as i32 };
    match msg {
        WM_TIMER if wparam.0 == TIMER_ANIM => tick(hwnd),
        WM_MOUSEMOVE => on_mouse_move(hwnd, point()),
        gfx::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                s.tracking_mouse = false;
                if s.drag.is_none() {
                    s.cursor = None;
                }
            });
            animate(hwnd);
        }
        WM_LBUTTONDOWN => on_button_down(hwnd, point()),
        WM_LBUTTONUP => on_button_up(hwnd, point()),
        WM_RBUTTONUP => on_right_click(hwnd, point()),
        WM_CAPTURECHANGED => {
            // Capture lost mid-drag (e.g. Alt+Tab): keep the order reached so far.
            let dragged = STATE.with_borrow_mut(|s| {
                s.press = None;
                s.drag.take().is_some()
            });
            if dragged {
                STATE.with_borrow(|s| save_pins(&s.pins));
                rebuild();
                animate(hwnd);
            }
        }
        appbar::WM_APPBAR_CALLBACK => match wparam.0 as u32 {
            ABN_POSCHANGED => place(hwnd),
            ABN_FULLSCREENAPP => {
                let after = if lparam.0 != 0 { HWND_BOTTOM } else { HWND_TOPMOST };
                let _ = SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            _ => {}
        },
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            update_font(hwnd);
            place(hwnd);
        }
        WM_DESTROY => appbar::unregister(hwnd),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

fn scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

unsafe fn place(hwnd: HWND) {
    let dpi = scale(hwnd);
    let thickness = ((icon_size() + PAD * 2.0 + MARGIN * 2.0) * dpi) as i32;
    let rc = appbar::reserve(hwnd, ABE_BOTTOM, thickness);
    // The window reaches above the reserved strip; that part is transparent and click-through.
    let height = thickness + ((HEADROOM + icon_size() * (max_scale() - 1.0)) * dpi) as i32;
    let _ = MoveWindow(hwnd, rc.left, rc.bottom - height, rc.right - rc.left, height, false);
    render(hwnd);
}

unsafe fn update_font(hwnd: HWND) {
    let font = gfx::create_font(-(FONT_SIZE * scale(hwnd)) as i32, FW_NORMAL, gfx::ui_font());
    let old = STATE.with_borrow_mut(|s| std::mem::replace(&mut s.font, font));
    if !old.is_invalid() {
        let _ = DeleteObject(old);
    }
}

unsafe fn client(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    rect
}

struct Layout {
    bg: RECT,
    icons: Vec<(usize, RECT)>,
    separators: Vec<i32>,
}

/// Icons sit on the dock's bottom padding and grow upwards; the row stays centered.
fn layout(slots: &[Slot], scales: &[f32], client: &RECT, dpi: f32) -> Layout {
    let (icon, gap, pad) = (icon_size() * dpi, GAP * dpi, PAD * dpi);
    let widths: Vec<f32> = slots
        .iter()
        .map(|slot| match slot {
            Slot::Item(i) => icon * scales.get(*i).copied().unwrap_or(1.0),
            Slot::Separator => SEPARATOR * dpi,
        })
        .collect();
    let total = widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
    let bottom = client.bottom as f32 - MARGIN * dpi;
    let icon_bottom = (bottom - pad).round() as i32;

    let start = (client.left + client.right) as f32 / 2.0 - total / 2.0;
    let mut x = start;
    let mut icons = Vec::new();
    let mut separators = Vec::new();
    for (slot, width) in slots.iter().zip(&widths) {
        match slot {
            Slot::Item(i) => {
                let size = width.round() as i32;
                let left = x.round() as i32;
                icons.push((*i, RECT { left, top: icon_bottom - size, right: left + size, bottom: icon_bottom }));
            }
            Slot::Separator => separators.push((x + width / 2.0).round() as i32),
        }
        x += width + gap;
    }
    let end = x - gap;
    let bg = RECT {
        left: (start - pad).round() as i32,
        top: (bottom - icon - pad * 2.0).round() as i32,
        right: (end + pad).round() as i32,
        bottom: bottom.round() as i32,
    };
    Layout { bg, icons, separators }
}

/// macOS-style magnification: a cosine bump centered on the cursor, measured
/// against the unmagnified layout so icons don't run away from the pointer.
fn target_scales(s: &State, client: &RECT, dpi: f32) -> Vec<f32> {
    let mut targets = vec![1.0; s.items.len()];
    let Some(cursor) = s.cursor else { return targets };
    if s.drag.is_some() {
        return targets;
    }
    let base = layout(&s.slots, &[], client, dpi);
    let unit = (icon_size() + GAP) * dpi;
    for (i, r) in &base.icons {
        let d = (cursor.x - (r.left + r.right) / 2).abs() as f32 / unit;
        if d < REACH {
            targets[*i] = 1.0 + (max_scale() - 1.0) * ((PI * d / REACH).cos() + 1.0) / 2.0;
        }
    }
    targets
}

fn hit(l: &Layout, p: POINT) -> Option<usize> {
    l.icons
        .iter()
        .find(|(_, r)| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < l.bg.bottom)
        .map(|(i, _)| *i)
}

fn bounce_offset(started: Option<&Instant>, dpi: f32) -> i32 {
    let Some(started) = started else { return 0 };
    let t = started.elapsed().as_secs_f32();
    if t >= BOUNCE.as_secs_f32() {
        return 0;
    }
    (icon_size() * dpi * 0.45 * (PI * t / BOUNCE_PERIOD).sin().abs()) as i32
}

fn removing(drag: &Drag, l: &Layout, dpi: f32) -> bool {
    (drag.at.y as f32) < l.bg.top as f32 - UNPIN_DISTANCE * dpi
}

/// Starts the animation timer if it isn't running; each tick renders.
unsafe fn animate(hwnd: HWND) {
    let start = STATE.with_borrow_mut(|s| !std::mem::replace(&mut s.animating, true));
    if start {
        SetTimer(hwnd, TIMER_ANIM, 15, None);
    }
}

unsafe fn tick(hwnd: HWND) {
    let client = client(hwnd);
    let dpi = scale(hwnd);
    let busy = STATE.with_borrow_mut(|s| {
        let targets = target_scales(s, &client, dpi);
        let mut moving = false;
        for (current, target) in s.scales.iter_mut().zip(&targets) {
            let delta = target - *current;
            if delta.abs() > 0.002 {
                *current += delta * 0.3;
                moving = true;
            } else {
                *current = *target;
            }
        }
        s.bounces.retain(|_, started| started.elapsed() < BOUNCE);
        let busy = moving || !s.bounces.is_empty() || s.drag.is_some();
        if !busy {
            s.animating = false;
        }
        busy
    });
    if !busy {
        let _ = KillTimer(hwnd, TIMER_ANIM);
    }
    render(hwnd);
}

unsafe fn render(hwnd: HWND) {
    let client = client(hwnd);
    let mut win = RECT::default();
    let _ = GetWindowRect(hwnd, &mut win);
    let Some(mut canvas) = Canvas::new(client.right, client.bottom) else { return };
    let dpi = scale(hwnd);
    let px = |v: f32| (v * dpi) as i32;

    // Frosted glass like macOS's dock: the wallpaper under it, blurred and tinted.
    let (screen_w, screen_h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let wallpaper = gfx::frosted_wallpaper(screen_w, screen_h);

    STATE.with_borrow(|s| {
        let l = layout(&s.slots, &s.scales, &client, dpi);
        let dc = canvas.dc;

        match &wallpaper {
            Some(wall) => canvas.fill_from(wall, screen_w, win.left, win.top),
            None => gfx::fill(dc, &l.bg, COLOR_BG),
        }
        for &x in &l.separators {
            let line = RECT { left: x, top: l.bg.top + px(PAD + 6.0), right: x + px(1.0).max(1), bottom: l.bg.bottom - px(PAD + 6.0) };
            gfx::fill(dc, &line, COLOR_SEPARATOR);
        }
        let mut shapes = vec![(l.bg, RADIUS * dpi)];

        // Name label above the hovered icon (or the dragged one when it's about to be removed).
        let label = match (&s.drag, s.cursor) {
            (Some(drag), _) if removing(drag, &l, dpi) => {
                let size = px(icon_size());
                Some(("Убрать из Дока".to_owned(), drag.at.x, drag.at.y - size / 2))
            }
            (None, Some(cursor)) => hit(&l, cursor).and_then(|i| {
                let (_, r) = l.icons.iter().find(|(j, _)| *j == i)?;
                let lift = s.items[i].kind_launch().and_then(|k| s.bounces.get(k)).map_or(0, |b| bounce_offset(Some(b), dpi));
                Some((s.items[i].name().to_owned(), (r.left + r.right) / 2, r.top - lift))
            }),
            _ => None,
        };
        if let Some((text, center, icon_top)) = label {
            SelectObject(dc, s.font);
            let width = gfx::text_width(dc, &text) + px(24.0);
            let bottom = icon_top - px(LABEL_GAP);
            let rect = RECT { left: center - width / 2, top: bottom - px(LABEL_H), right: center + width / 2, bottom };
            gfx::fill(dc, &rect, COLOR_LABEL);
            SetTextColor(dc, COLOR_TEXT);
            gfx::draw_text(dc, &text, &rect, DT_CENTER);
            shapes.push((rect, LABEL_H * dpi / 2.0));
        }

        canvas.mask(&shapes);
        // The thin light rim macOS draws around its materials.
        canvas.stroke_shape(&l.bg, RADIUS * dpi, COLOR_TEXT, 40);

        let dragged = s.drag.as_ref().map(|d| d.launch.as_str());
        for (i, r) in &l.icons {
            let item = &s.items[*i];
            if dragged.is_some() && item.kind_launch() == dragged {
                continue;
            }
            let lift = bounce_offset(item.kind_launch().and_then(|k| s.bounces.get(k)), dpi);
            let r = RECT { top: r.top - lift, bottom: r.bottom - lift, ..*r };
            draw_item(&mut canvas, item, &r, 1.0);
            if !item.windows.is_empty() && config::flag("dock.indicators") {
                let center = (r.left + r.right) / 2;
                let y = l.bg.bottom - px(PAD) / 2;
                let radius = px(2.0).max(2);
                let dot = RECT { left: center - radius, top: y - radius, right: center + radius, bottom: y + radius };
                canvas.fill_shape(&dot, radius as f32, COLOR_DOT, 255);
            }
        }

        if let Some(drag) = &s.drag {
            if let Some(item) = s.items.iter().find(|it| it.kind_launch() == Some(drag.launch.as_str())) {
                let size = px(icon_size());
                let r = RECT { left: drag.at.x - size / 2, top: drag.at.y - size / 2, right: drag.at.x + size / 2, bottom: drag.at.y + size / 2 };
                draw_item(&mut canvas, item, &r, if removing(drag, &l, dpi) { 0.45 } else { 0.9 });
            }
        }
    });

    canvas.present(hwnd, win.left, win.top, 255);
}

fn draw_item(canvas: &mut Canvas, item: &Item, r: &RECT, opacity: f32) {
    let size = r.right - r.left;
    let source = match &item.kind {
        Kind::Launchpad => return draw_launchpad(canvas, r, opacity),
        Kind::App { pin, .. } => pin.launch.as_str(),
        Kind::Trash => TRASH,
    };
    match icons::get(source, ICON_LOAD) {
        Some(icon) => canvas.draw_icon(&icon, r.left, r.top, size, None, opacity),
        None => {
            let inset = size / 10;
            let square = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
            canvas.fill_shape(&square, size as f32 * 0.22, COLOR_PLACEHOLDER, (255.0 * opacity) as u32);
        }
    }
}

/// The launcher's own icon, after macOS's Launchpad: a light disc with a soft
/// top-to-bottom gradient and a 3×3 grid of small rounded tiles in muted colors.
fn draw_launchpad(canvas: &mut Canvas, r: &RECT, opacity: f32) {
    let size = (r.right - r.left) as f32;
    let alpha = (255.0 * opacity) as u32;
    let inset = (size * 0.06).round() as i32;
    let disc = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
    let radius = (disc.right - disc.left) as f32 / 2.0;
    canvas.fill_shape_gradient(&disc, radius, LAUNCHPAD_TOP, LAUNCHPAD_BOTTOM, alpha);

    let tile = size * 0.15;
    let step = tile + size * 0.055;
    let (cx, cy) = ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0);
    for (i, color) in LAUNCHPAD_TILES.iter().enumerate() {
        let (col, row) = ((i % 3) as f32 - 1.0, (i / 3) as f32 - 1.0);
        let (x, y) = (cx + col * step, cy + row * step);
        let rect = RECT {
            left: (x - tile / 2.0).round() as i32,
            top: (y - tile / 2.0).round() as i32,
            right: (x + tile / 2.0).round() as i32,
            bottom: (y + tile / 2.0).round() as i32,
        };
        canvas.fill_shape(&rect, tile * 0.32, *color, alpha);
    }
}

unsafe fn on_mouse_move(hwnd: HWND, p: POINT) {
    let start_tracking = STATE.with_borrow_mut(|s| {
        s.cursor = Some(p);
        !std::mem::replace(&mut s.tracking_mouse, true)
    });
    if start_tracking {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }

    let client = client(hwnd);
    let dpi = scale(hwnd);
    let reordered = STATE.with_borrow_mut(|s| {
        if s.drag.is_none() {
            let Some((i, at)) = s.press else { return false };
            if (p.x - at.x).abs() <= DRAG_THRESHOLD && (p.y - at.y).abs() <= DRAG_THRESHOLD {
                return false;
            }
            let Kind::App { pin, pinned: true } = &s.items[i].kind else { return false };
            s.drag = Some(Drag { launch: pin.launch.clone(), at: p });
        }
        let Some(drag) = &mut s.drag else { return false };
        drag.at = p;
        let launch = drag.launch.clone();

        // The new position is the number of other pins whose icon center is left of the cursor.
        let l = layout(&s.slots, &s.scales, &client, dpi);
        let before = l
            .icons
            .iter()
            .filter(|(i, r)| {
                matches!(&s.items[*i].kind, Kind::App { pin, pinned: true } if pin.launch != launch)
                    && (r.left + r.right) / 2 < p.x
            })
            .count();
        let Some(from) = s.pins.iter().position(|pin| pin.launch == launch) else { return false };
        if before == from {
            return false;
        }
        let pin = s.pins.remove(from);
        s.pins.insert(before.min(s.pins.len()), pin);
        true
    });
    if reordered {
        rebuild();
    }
    animate(hwnd);
}

unsafe fn on_button_down(hwnd: HWND, p: POINT) {
    let client = client(hwnd);
    let dpi = scale(hwnd);
    let pressed = STATE.with_borrow_mut(|s| {
        let l = layout(&s.slots, &s.scales, &client, dpi);
        s.press = hit(&l, p).map(|i| (i, p));
        s.press.is_some()
    });
    if pressed {
        SetCapture(hwnd);
    }
}

unsafe fn on_button_up(hwnd: HWND, p: POINT) {
    let client = client(hwnd);
    let dpi = scale(hwnd);
    // Taken before ReleaseCapture so WM_CAPTURECHANGED sees nothing to cancel.
    let (press, drag) = STATE.with_borrow_mut(|s| (s.press.take(), s.drag.take()));
    let _ = ReleaseCapture();

    if let Some(drag) = drag {
        STATE.with_borrow_mut(|s| {
            let l = layout(&s.slots, &s.scales, &client, dpi);
            if removing(&drag, &l, dpi) {
                s.pins.retain(|pin| pin.launch != drag.launch);
            }
            save_pins(&s.pins);
        });
        rebuild();
        animate(hwnd);
        return;
    }

    let Some((pressed, _)) = press else { return };
    let released = STATE.with_borrow(|s| hit(&layout(&s.slots, &s.scales, &client, dpi), p));
    if released == Some(pressed) {
        click(hwnd, pressed);
    }
}

enum Action {
    Launcher,
    Trash,
    Launch(String),
    Windows(Vec<HWND>),
}

unsafe fn click(hwnd: HWND, index: usize) {
    let action = STATE.with_borrow(|s| {
        let item = s.items.get(index)?;
        Some(match &item.kind {
            Kind::Launchpad => Action::Launcher,
            Kind::Trash => Action::Trash,
            Kind::App { pin, .. } if item.windows.is_empty() => Action::Launch(pin.launch.clone()),
            Kind::App { .. } => Action::Windows(item.windows.clone()),
        })
    });
    match action {
        Some(Action::Launcher) => launchpad::toggle(),
        Some(Action::Trash) => apps::open(TRASH),
        Some(Action::Launch(target)) => launch(hwnd, &target),
        Some(Action::Windows(windows)) => {
            if let [only] = windows[..] {
                tasks::activate(only);
            } else {
                // Several windows: cycle through them, like clicking a grouped taskbar button.
                let foreground = GetForegroundWindow();
                let next = windows.iter().position(|&w| w == foreground).map_or(0, |i| (i + 1) % windows.len());
                tasks::focus(windows[next]);
            }
        }
        None => {}
    }
}

unsafe fn launch(hwnd: HWND, target: &str) {
    apps::open(target);
    STATE.with_borrow_mut(|s| s.bounces.insert(target.to_owned(), Instant::now()));
    animate(hwnd);
}

unsafe fn on_right_click(hwnd: HWND, p: POINT) {
    let client = client(hwnd);
    let dpi = scale(hwnd);
    let Some((index, anchor)) = STATE.with_borrow(|s| {
        let l = layout(&s.slots, &s.scales, &client, dpi);
        let i = hit(&l, p)?;
        let (_, r) = l.icons.iter().find(|(j, _)| *j == i)?;
        Some((i, POINT { x: (r.left + r.right) / 2, y: r.top }))
    }) else {
        return;
    };
    let _ = show_menu(hwnd, index, anchor);
    // The menu swallowed the mouse; the cursor is probably elsewhere now.
    STATE.with_borrow_mut(|s| {
        s.cursor = None;
        s.tracking_mouse = false;
    });
    animate(hwnd);
}

unsafe fn show_menu(hwnd: HWND, index: usize, mut anchor: POINT) -> Result<()> {
    let Some((kind, windows)) = STATE.with_borrow(|s| {
        let item = s.items.get(index)?;
        let kind = match &item.kind {
            Kind::Launchpad => None,
            Kind::Trash => Some(None),
            Kind::App { pin, pinned } => Some(Some((pin.clone(), *pinned))),
        };
        Some((kind, item.windows.clone()))
    }) else {
        return Ok(());
    };

    // Like macOS's dock menu: the app's windows first, then the app's options.
    let mut menu = Menu::new();
    match &kind {
        None => {
            menu.item(CMD_OPEN, "Открыть");
        }
        Some(None) => {
            menu.item(CMD_OPEN, "Открыть").item(CMD_EMPTY_TRASH, "Очистить Корзину");
        }
        Some(Some((pin, pinned))) => {
            let foreground = GetForegroundWindow();
            for (n, &window) in windows.iter().enumerate() {
                menu.check(CMD_WINDOW + n, tasks::title(window).unwrap_or_default(), window == foreground);
            }
            menu.separator();
            menu.item(CMD_OPEN, if windows.is_empty() { "Открыть" } else { "Новое окно" });
            let mut options = Menu::new();
            options.check(CMD_PIN, "Оставить в Доке", *pinned);
            if pin.path.is_some() {
                options.item(CMD_REVEAL, "Показать в Проводнике");
            }
            menu.submenu("Параметры", options);
            if !windows.is_empty() {
                menu.separator();
                menu.item(CMD_QUIT, if windows.len() == 1 { "Завершить".to_owned() } else { format!("Закрыть все окна ({})", windows.len()) });
            }
        }
    }

    let _ = ClientToScreen(hwnd, &mut anchor);
    let cmd = menu.show(Anchor::Above(POINT { x: anchor.x, y: anchor.y - (8.0 * scale(hwnd)) as i32 }));

    match (cmd, kind) {
        (0, _) => {}
        (CMD_OPEN, None) => launchpad::toggle(),
        (CMD_OPEN, Some(None)) => apps::open(TRASH),
        (CMD_EMPTY_TRASH, _) => {
            let _ = SHEmptyRecycleBinW(hwnd, PCWSTR::null(), 0);
        }
        (CMD_OPEN, Some(Some((pin, _)))) => launch(hwnd, &pin.launch),
        (CMD_PIN, Some(Some((pin, _)))) => toggle_pin(pin),
        (CMD_REVEAL, Some(Some((pin, _)))) => {
            if let Some(path) = &pin.path {
                apps::reveal(path);
            }
        }
        (CMD_QUIT, _) => {
            for window in windows {
                let _ = PostMessageW(window, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        (cmd, _) if cmd >= CMD_WINDOW => {
            if let Some(&window) = windows.get(cmd - CMD_WINDOW) {
                tasks::focus(window);
            }
        }
        _ => {}
    }
    Ok(())
}

fn pins_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("CustomUI").join("dock.tsv"))
}

/// One pin per line: name, launch target, AUMID, exe path (tab-separated, last two optional).
fn load_pins() -> Vec<Pin> {
    let Some(text) = pins_file().and_then(|f| std::fs::read_to_string(f).ok()) else {
        return default_pins();
    };
    let optional = |field: Option<&str>| field.filter(|f| !f.is_empty()).map(str::to_owned);
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.to_owned();
            let launch = fields.next().filter(|l| !l.is_empty())?.to_owned();
            Some(Pin { name, launch, aumid: optional(fields.next()), path: optional(fields.next()) })
        })
        .collect()
}

fn save_pins(pins: &[Pin]) {
    let Some(file) = pins_file() else { return };
    let text: String = pins
        .iter()
        .map(|p| {
            format!("{}\t{}\t{}\t{}\n", p.name, p.launch, p.aumid.as_deref().unwrap_or(""), p.path.as_deref().unwrap_or(""))
        })
        .collect();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, text);
}

fn default_pins() -> Vec<Pin> {
    let windows = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let explorer = format!(r"{windows}\explorer.exe");
    vec![Pin {
        name: "Проводник".to_owned(),
        launch: explorer.clone(),
        aumid: Some("Microsoft.Windows.Explorer".to_owned()),
        path: Some(explorer),
    }]
}
