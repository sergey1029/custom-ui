//! Drawing shared by the shell's layered windows. GDI paints into a 32-bit DIB,
//! alpha is derived per pixel afterwards (GDI can't write it), shell icons are
//! composited with real alpha, and the result goes out via UpdateLayeredWindow.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use windows::core::{w, Error, Result, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::icons::Icon;

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

/// The configured UI font. Windows 11's variable font by default: the closest
/// built-in match for macOS's SF Pro (which may not be used outside Apple platforms).
pub fn ui_font() -> PCWSTR {
    static FONT: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
    PCWSTR(FONT.get_or_init(|| crate::config::text("font").encode_utf16().chain([0]).collect()).as_ptr())
}

/// The configured accent: menu highlights, selected and "on" states.
pub fn accent() -> COLORREF {
    let (r, g, b) = crate::config::color("accent");
    rgb(r, g, b)
}
/// Its optical size for big type (large numbers, titles).
pub const DISPLAY_FONT: PCWSTR = w!("Segoe UI Variable Display");

pub const COLOR_BG: COLORREF = rgb(22, 22, 25);
pub const COLOR_TEXT: COLORREF = rgb(236, 236, 240);
pub const COLOR_DIM: COLORREF = rgb(142, 142, 152);

// Lives in the heavy Win32_UI_Controls feature; not worth pulling it in for one constant.
pub const WM_MOUSELEAVE: u32 = 0x02A3;

/// Registers a window class (once; later calls reuse it) and creates a hidden popup of it.
pub unsafe fn create_window(class: PCWSTR, wndproc: WNDPROC, ex_style: WINDOW_EX_STYLE) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: wndproc,
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: class,
        ..Default::default()
    };
    if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        return Err(Error::from_win32());
    }
    CreateWindowExW(ex_style, class, w!("Custom UI"), WS_POPUP, 0, 0, 0, 0, None, None, instance, None)
}

/// Activates one of our popups. Windows only lets the process that got the last
/// input take the foreground, and clicks on our no-activate bars don't count;
/// sharing input state with the current foreground thread gets around that.
/// Returns the window that was in front, to hand focus back to on Esc.
pub unsafe fn force_foreground(hwnd: HWND) -> HWND {
    let foreground = GetForegroundWindow();
    let their_thread = GetWindowThreadProcessId(foreground, None);
    let our_thread = GetCurrentThreadId();
    let attach = their_thread != 0 && their_thread != our_thread;
    if attach {
        let _ = AttachThreadInput(our_thread, their_thread, true);
    }
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(hwnd);
    if attach {
        let _ = AttachThreadInput(our_thread, their_thread, false);
    }
    foreground
}

/// Gives the foreground back after one of our popups was dismissed with Esc.
pub unsafe fn restore_foreground(previous: HWND) {
    if !previous.is_invalid() && IsWindow(previous).as_bool() {
        let _ = SetForegroundWindow(previous);
    }
}

/// Undocumented uxtheme switch (ordinal 135, SetPreferredAppMode) that makes this
/// process's popup menus dark; silently does nothing if it's missing.
pub unsafe fn enable_dark_menus() {
    let Ok(lib) = LoadLibraryW(w!("uxtheme.dll")) else { return };
    if let Some(f) = GetProcAddress(lib, PCSTR(135 as *const u8)) {
        let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
        set_preferred_app_mode(2); // ForceDark
    }
    if let Some(f) = GetProcAddress(lib, PCSTR(136 as *const u8)) {
        let flush_menu_themes: extern "system" fn() = std::mem::transmute(f);
        flush_menu_themes();
    }
}

pub unsafe fn create_font(height: i32, weight: FONT_WEIGHT, face: PCWSTR) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        face,
    )
}

/// Off-screen 32-bit top-down DIB with a DC for GDI drawing.
pub struct Canvas {
    pub dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    pub width: i32,
    pub height: i32,
}

impl Canvas {
    pub unsafe fn new(width: i32, height: i32) -> Option<Canvas> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bitmap);
        SetBkMode(dc, TRANSPARENT);
        Some(Canvas { dc, bitmap, old, bits: bits as *mut u32, width, height })
    }

    /// Premultiplied BGRA pixels, row-major. Flushes pending GDI drawing first.
    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height) as usize)
        }
    }

    fn bounds(&self) -> RECT {
        RECT { left: 0, top: 0, right: self.width, bottom: self.height }
    }

    /// Turns the GDI drawing into real alpha: inside the rounded `shapes`, pixels
    /// still showing `bg` get `bg_alpha` and anything drawn over it stays opaque;
    /// everything outside the shapes becomes fully transparent (and click-through).
    pub fn shape_alpha(&mut self, bg: COLORREF, bg_alpha: u32, shapes: &[(RECT, f32)]) {
        let bg = to_pixel(bg);
        let width = self.width;
        for (i, p) in self.pixels().iter_mut().enumerate() {
            let (x, y) = (i as i32 % width, i as i32 / width);
            let cover = shapes.iter().map(|(r, radius)| rounded_coverage(x, y, r, *radius)).fold(0.0, f32::max);
            let rgb = *p & 0x00FF_FFFF;
            let base = if rgb == bg { bg_alpha } else { 255 };
            *p = premultiply(rgb, (base as f32 * cover).round() as u32);
        }
    }

    /// Fills the canvas with the part of a `src_width`-wide opaque image whose
    /// top-left corner is at (`x`, `y`) (e.g. the frosted wallpaper under a window).
    pub fn fill_from(&mut self, src: &[u32], src_width: i32, x: i32, y: i32) {
        let (width, height) = (self.width, self.height);
        let src_height = src.len() as i32 / src_width.max(1);
        let pixels = self.pixels();
        for row in 0..height {
            let sy = (y + row).clamp(0, src_height - 1);
            for col in 0..width {
                let sx = (x + col).clamp(0, src_width - 1);
                pixels[(row * width + col) as usize] = src[(sy * src_width + sx) as usize] | 0xFF00_0000;
            }
        }
    }

    /// Starts a pop-up panel: its frosted backdrop (captured at the panel's
    /// on-screen spot when it opened, `width` px wide and at least as tall as the
    /// canvas), or a flat `fallback` color if there's none.
    pub fn backdrop(&mut self, src: &[u32], width: i32, fallback: COLORREF) {
        if width == self.width && src.len() >= (width * self.height) as usize {
            self.fill_from(src, width, 0, 0);
        } else {
            unsafe { fill(self.dc, &self.bounds(), fallback) };
        }
    }

    /// Ends a pop-up panel: rounded corners (outside transparent) and macOS's light rim.
    pub fn finish_panel(&mut self, radius: f32) {
        let bounds = self.bounds();
        self.mask(&[(bounds, radius)]);
        self.stroke_shape(&bounds, radius, COLOR_TEXT, 34);
    }

    /// For canvases painted over a frosted backdrop: everything inside the
    /// rounded `shapes` becomes opaque (antialiased at the edges), the rest
    /// fully transparent and click-through.
    pub fn mask(&mut self, shapes: &[(RECT, f32)]) {
        let width = self.width;
        for (i, p) in self.pixels().iter_mut().enumerate() {
            let (x, y) = (i as i32 % width, i as i32 / width);
            let cover = shapes.iter().map(|(r, radius)| rounded_coverage(x, y, r, *radius)).fold(0.0, f32::max);
            *p = premultiply(*p & 0x00FF_FFFF, (255.0 * cover).round() as u32);
        }
    }

    /// A 1px antialiased outline just inside a rounded rect (macOS's light edge on materials).
    pub fn stroke_shape(&mut self, rect: &RECT, radius: f32, color: COLORREF, alpha: u32) {
        let color = to_pixel(color);
        let inner = RECT { left: rect.left + 1, top: rect.top + 1, right: rect.right - 1, bottom: rect.bottom - 1 };
        let (width, height) = (self.width, self.height);
        let pixels = self.pixels();
        for y in rect.top.max(0)..rect.bottom.min(height) {
            for x in rect.left.max(0)..rect.right.min(width) {
                let ring = rounded_coverage(x, y, rect, radius) - rounded_coverage(x, y, &inner, (radius - 1.0).max(0.0));
                let a = (alpha as f32 * ring.clamp(0.0, 1.0)).round() as u32;
                if a > 0 {
                    let p = &mut pixels[(y * width + x) as usize];
                    *p = over(premultiply(color, a), *p);
                }
            }
        }
    }

    /// Antialiased rounded rect composited over what's already there.
    pub fn fill_shape(&mut self, rect: &RECT, radius: f32, color: COLORREF, alpha: u32) {
        let color = to_pixel(color);
        let (width, height) = (self.width, self.height);
        let pixels = self.pixels();
        for y in rect.top.max(0)..rect.bottom.min(height) {
            for x in rect.left.max(0)..rect.right.min(width) {
                let a = (alpha as f32 * rounded_coverage(x, y, rect, radius)).round() as u32;
                if a > 0 {
                    let p = &mut pixels[(y * width + x) as usize];
                    *p = over(premultiply(color, a), *p);
                }
            }
        }
    }

    /// Like `fill_shape`, with a vertical gradient from `top` to `bottom`.
    pub fn fill_shape_gradient(&mut self, rect: &RECT, radius: f32, top: COLORREF, bottom: COLORREF, alpha: u32) {
        let (top, bottom) = (to_pixel(top), to_pixel(bottom));
        let (width, height) = (self.width, self.height);
        let span = (rect.bottom - rect.top).max(1) as f32;
        let pixels = self.pixels();
        for y in rect.top.max(0)..rect.bottom.min(height) {
            let t = (y - rect.top) as f32 / span;
            let mix = |shift: u32| {
                let (a, b) = ((top >> shift & 0xFF) as f32, (bottom >> shift & 0xFF) as f32);
                ((a + (b - a) * t).round() as u32) << shift
            };
            let color = mix(16) | mix(8) | mix(0);
            for x in rect.left.max(0)..rect.right.min(width) {
                let a = (alpha as f32 * rounded_coverage(x, y, rect, radius)).round() as u32;
                if a > 0 {
                    let p = &mut pixels[(y * width + x) as usize];
                    *p = over(premultiply(color, a), *p);
                }
            }
        }
    }

    /// Composites `icon` scaled to `size` at (x, y), only inside `clip`.
    pub fn draw_icon(&mut self, icon: &Icon, x: i32, y: i32, size: i32, clip: Option<&RECT>, opacity: f32) {
        if size <= 0 {
            return;
        }
        let scaled = icon.scaled(size);
        let bounds = self.bounds();
        let clip = clip.map_or(bounds, |c| RECT {
            left: c.left.max(0),
            top: c.top.max(0),
            right: c.right.min(bounds.right),
            bottom: c.bottom.min(bounds.bottom),
        });
        let fade = (opacity.clamp(0.0, 1.0) * 256.0) as u32;
        let width = self.width;
        let pixels = self.pixels();
        for sy in 0..size {
            let ty = y + sy;
            if ty < clip.top || ty >= clip.bottom {
                continue;
            }
            for sx in 0..size {
                let tx = x + sx;
                if tx < clip.left || tx >= clip.right {
                    continue;
                }
                let mut src = scaled[(sy * size + sx) as usize];
                if fade < 256 {
                    src = scale_pixel(src, fade);
                }
                let dst = &mut pixels[(ty * width + tx) as usize];
                *dst = over(src, *dst);
            }
        }
    }

    /// Shows the canvas as the layered window's content at (x, y) in screen coordinates.
    pub unsafe fn present(&self, hwnd: HWND, x: i32, y: i32, opacity: u8) {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: opacity,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pos = POINT { x, y };
        let size = SIZE { cx: self.width, cy: self.height };
        let src = POINT::default();
        let screen = GetDC(None);
        let _ = UpdateLayeredWindow(
            hwnd,
            screen,
            Some(&pos as *const _),
            Some(&size as *const _),
            self.dc,
            Some(&src as *const _),
            COLORREF(0),
            Some(&blend as *const _),
            ULW_ALPHA,
        );
        ReleaseDC(None, screen);
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

/// The screen as it is now, blurred and darkened (`darken` 0..1 keeps that much
/// brightness): opaque pixels for a full-screen frosted backdrop.
pub unsafe fn blurred_screen(width: i32, height: i32, darken: f32) -> Option<Vec<u32>> {
    let shot = capture(RECT { left: 0, top: 0, right: width, bottom: height })?;
    let mut out = blur(&shot, width, height, 8);
    tint(&mut out, rgb(0, 0, 0), 1.0 - darken, 1.0);
    Some(out)
}

/// What's on screen inside `rect` right now (layered windows included).
pub unsafe fn capture(rect: RECT) -> Option<Vec<u32>> {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    let mut shot = Canvas::new(width, height)?;
    let screen = GetDC(None);
    // CAPTUREBLT includes layered windows (the menu bar and the dock).
    let copied = BitBlt(shot.dc, 0, 0, width, height, screen, rect.left, rect.top, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0)).is_ok();
    ReleaseDC(None, screen);
    copied.then(|| shot.pixels().to_vec())
}

/// macOS-style "vibrancy" for a pop-up about to open at `rect`: the screen under
/// it, blurred, tinted dark and a bit more saturated. Opaque pixels.
pub unsafe fn frosted(rect: RECT) -> Option<Vec<u32>> {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    // A margin around the rect so the blur near its edges sees real neighbours.
    let pad = 24;
    let outer = RECT { left: rect.left - pad, top: rect.top - pad, right: rect.right + pad, bottom: rect.bottom + pad };
    let shot = capture(outer)?;
    let blurred = blur(&shot, width + pad * 2, height + pad * 2, 6);
    let mut out = crop(&blurred, width + pad * 2, RECT { left: pad, top: pad, right: pad + width, bottom: pad + height });
    vibrancy_tint(&mut out);
    Some(out)
}

/// Dark vibrancy: how macOS's dark translucent materials look over content.
const VIBRANCY_TINT: COLORREF = rgb(28, 28, 32);

/// With the configured darkness and saturation.
fn vibrancy_tint(pixels: &mut [u32]) {
    let amount = crate::config::int("glass.darkness") as f32 / 100.0;
    let saturation = crate::config::int("glass.saturation") as f32 / 100.0;
    tint(pixels, VIBRANCY_TINT, amount, saturation);
}

/// Width, height and pixels of the frosted wallpaper.
type Wallpaper = Option<(i32, i32, Rc<Vec<u32>>)>;

thread_local! {
    /// The blurred, tinted wallpaper, full screen: what the always-visible bars sit on.
    static WALLPAPER: RefCell<Wallpaper> = const { RefCell::new(None) };
}

/// The frosted wallpaper as the menu bar and the dock show through: the desktop
/// window rendered alone (no other windows, not even ours), blurred and tinted.
/// Cached; `forget_wallpaper` after it changes.
pub fn frosted_wallpaper(width: i32, height: i32) -> Option<Rc<Vec<u32>>> {
    if let Some(cached) = WALLPAPER.with_borrow(|w| w.as_ref().filter(|(cw, ch, _)| *cw == width && *ch == height).map(|(_, _, p)| p.clone())) {
        return Some(cached);
    }
    let pixels = unsafe {
        let progman = FindWindowW(w!("Progman"), None).ok()?;
        let mut shot = Canvas::new(width, height)?;
        if !PrintWindow(progman, shot.dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool() {
            return None;
        }
        let shot = shot.pixels().to_vec();
        let mut out = blur(&shot, width, height, 8);
        vibrancy_tint(&mut out);
        Rc::new(out)
    };
    WALLPAPER.with_borrow_mut(|w| *w = Some((width, height, pixels.clone())));
    Some(pixels)
}

pub fn forget_wallpaper() {
    WALLPAPER.with_borrow_mut(|w| *w = None);
}

/// `rect` out of a `width`-wide pixel buffer.
pub fn crop(src: &[u32], width: i32, rect: RECT) -> Vec<u32> {
    let mut out = Vec::with_capacity(((rect.right - rect.left) * (rect.bottom - rect.top)).max(0) as usize);
    for y in rect.top..rect.bottom {
        let start = (y * width + rect.left) as usize;
        out.extend_from_slice(&src[start..start + (rect.right - rect.left) as usize]);
    }
    out
}

/// Mixes `amount` of `color` into opaque pixels and scales saturation.
pub fn tint(pixels: &mut [u32], color: COLORREF, amount: f32, saturation: f32) {
    let c = to_pixel(color);
    let target = [(c >> 16 & 0xFF) as f32, (c >> 8 & 0xFF) as f32, (c & 0xFF) as f32];
    for p in pixels.iter_mut() {
        let mut rgb = [(*p >> 16 & 0xFF) as f32, (*p >> 8 & 0xFF) as f32, (*p & 0xFF) as f32];
        let gray = rgb[0] * 0.3 + rgb[1] * 0.59 + rgb[2] * 0.11;
        for (i, v) in rgb.iter_mut().enumerate() {
            let saturated = gray + (*v - gray) * saturation;
            *v = (saturated + (target[i] - saturated) * amount).clamp(0.0, 255.0);
        }
        *p = 0xFF00_0000 | (rgb[0] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[2] as u32;
    }
}

/// Downscales by `factor`, box-blurs, and stretches back with bilinear
/// filtering: a cheap, wide blur. Output is opaque.
pub fn blur(src: &[u32], width: i32, height: i32, factor: i32) -> Vec<u32> {
    let factor = factor.max(1);
    let (sw, sh) = ((width + factor - 1) / factor, (height + factor - 1) / factor);
    let mut small = vec![[0f32; 3]; (sw * sh) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let mut acc = [0f32; 3];
            let mut n = 0.0;
            for yy in y * factor..((y + 1) * factor).min(height) {
                for xx in x * factor..((x + 1) * factor).min(width) {
                    let p = src[(yy * width + xx) as usize];
                    acc[0] += (p >> 16 & 0xFF) as f32;
                    acc[1] += (p >> 8 & 0xFF) as f32;
                    acc[2] += (p & 0xFF) as f32;
                    n += 1.0;
                }
            }
            small[(y * sw + x) as usize] = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
    }
    // Three box passes each way approximate a gaussian.
    for _ in 0..3 {
        box_blur(&mut small, sw, sh, 2, true);
        box_blur(&mut small, sw, sh, 2, false);
    }

    let mut out = vec![0u32; (width * height) as usize];
    let sample = |x: i32, y: i32| small[(y.clamp(0, sh - 1) * sw + x.clamp(0, sw - 1)) as usize];
    for y in 0..height {
        let fy = (y as f32 + 0.5) / factor as f32 - 0.5;
        let (y0, ty) = (fy.floor() as i32, fy - fy.floor());
        for x in 0..width {
            let fx = (x as f32 + 0.5) / factor as f32 - 0.5;
            let (x0, tx) = (fx.floor() as i32, fx - fx.floor());
            let (a, b, c, d) = (sample(x0, y0), sample(x0 + 1, y0), sample(x0, y0 + 1), sample(x0 + 1, y0 + 1));
            let channel = |i: usize| {
                let top = a[i] + (b[i] - a[i]) * tx;
                let bottom = c[i] + (d[i] - c[i]) * tx;
                (top + (bottom - top) * ty).clamp(0.0, 255.0) as u32
            };
            out[(y * width + x) as usize] = 0xFF00_0000 | channel(0) << 16 | channel(1) << 8 | channel(2);
        }
    }
    out
}

fn box_blur(pixels: &mut [[f32; 3]], width: i32, height: i32, radius: i32, horizontal: bool) {
    let (lines, length) = if horizontal { (height, width) } else { (width, height) };
    let index = |line: i32, i: i32| if horizontal { (line * width + i) as usize } else { (i * width + line) as usize };
    let mut row = vec![[0f32; 3]; length as usize];
    for line in 0..lines {
        for i in 0..length {
            let mut acc = [0f32; 3];
            for k in -radius..=radius {
                let p = pixels[index(line, (i + k).clamp(0, length - 1))];
                for c in 0..3 {
                    acc[c] += p[c];
                }
            }
            let n = (radius * 2 + 1) as f32;
            row[i as usize] = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
        for i in 0..length {
            pixels[index(line, i)] = row[i as usize];
        }
    }
}

/// COLORREF is 0x00BBGGRR, DIB pixels are 0x00RRGGBB.
fn to_pixel(c: COLORREF) -> u32 {
    let c = c.0;
    (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF)
}

pub fn premultiply(rgb: u32, alpha: u32) -> u32 {
    let channel = |shift: u32| ((rgb >> shift & 0xFF) * alpha / 255) << shift;
    alpha << 24 | channel(16) | channel(8) | channel(0)
}

/// `fade` is 0..=256.
fn scale_pixel(p: u32, fade: u32) -> u32 {
    let channel = |shift: u32| (((p >> shift & 0xFF) * fade) >> 8) << shift;
    channel(24) | channel(16) | channel(8) | channel(0)
}

/// Premultiplied source-over.
fn over(src: u32, dst: u32) -> u32 {
    let inv = 255 - (src >> 24);
    let channel = |shift: u32| ((src >> shift & 0xFF) + (dst >> shift & 0xFF) * inv / 255).min(255) << shift;
    channel(24) | channel(16) | channel(8) | channel(0)
}

/// How much of pixel (x, y) lies inside `rect` with corners rounded by `radius`:
/// 1 inside, 0 outside, fractional (antialiased) along the corner arcs.
pub fn rounded_coverage(x: i32, y: i32, rect: &RECT, radius: f32) -> f32 {
    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
    let (l, t, r, b) = (rect.left as f32, rect.top as f32, rect.right as f32, rect.bottom as f32);
    if fx < l || fx > r || fy < t || fy > b {
        return 0.0;
    }
    // Square corners: the arc formula below would give every pixel half coverage.
    if radius < 0.5 {
        return 1.0;
    }
    // Nearest point of the rect shrunk by the radius; away from the corners that's the pixel itself.
    let cx = fx.max(l + radius).min(r - radius);
    let cy = fy.max(t + radius).min(b - radius);
    let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
    (radius - dist + 0.5).clamp(0.0, 1.0)
}

pub unsafe fn fill(hdc: HDC, rect: &RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rect, brush);
    let _ = DeleteObject(brush);
}

pub unsafe fn fill_round(hdc: HDC, rect: &RECT, color: COLORREF, radius: i32) {
    let brush = CreateSolidBrush(color);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
    // With a null pen RoundRect leaves out the right and bottom edge.
    let _ = RoundRect(hdc, rect.left, rect.top, rect.right + 1, rect.bottom + 1, radius, radius);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(brush);
}

// Both text helpers bail out on "": windows-rs passes an empty slice as a dangling
// pointer with length 0, and DrawTextW crashes on that.
pub unsafe fn draw_text(hdc: HDC, text: &str, rect: &RECT, format: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = *rect;
    DrawTextW(hdc, &mut wide, &mut rect, format | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

/// A `size`×`size` square centered in `r`.
pub fn centered(r: &RECT, size: i32) -> RECT {
    let left = r.left + (r.right - r.left - size) / 2;
    let top = r.top + (r.bottom - r.top - size) / 2;
    RECT { left, top, right: left + size, bottom: top + size }
}

/// Wraps onto as many lines as fit in `rect`, with an ellipsis if it's cut.
pub unsafe fn draw_text_wrapped(hdc: HDC, text: &str, rect: &RECT) {
    draw_text_wrapped_aligned(hdc, text, rect, DT_LEFT);
}

/// `draw_text_wrapped` with `align` = DT_LEFT / DT_CENTER / DT_RIGHT.
pub unsafe fn draw_text_wrapped_aligned(hdc: HDC, text: &str, rect: &RECT, align: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = *rect;
    DrawTextW(hdc, &mut wide, &mut rect, align | DT_WORDBREAK | DT_END_ELLIPSIS | DT_EDITCONTROL | DT_NOPREFIX);
}

/// Opaque vertical gradient (GDI, so text can be drawn over it afterwards).
pub unsafe fn fill_gradient(hdc: HDC, rect: &RECT, top: COLORREF, bottom: COLORREF) {
    let vertex = |x: i32, y: i32, c: COLORREF| TRIVERTEX {
        x,
        y,
        Red: ((c.0 & 0xFF) << 8) as u16,
        Green: (c.0 & 0xFF00) as u16,
        Blue: ((c.0 >> 16 & 0xFF) << 8) as u16,
        Alpha: 0xFF00,
    };
    let vertices = [vertex(rect.left, rect.top, top), vertex(rect.right, rect.bottom, bottom)];
    let mesh = GRADIENT_RECT { UpperLeft: 0, LowerRight: 1 };
    let _ = GradientFill(hdc, &vertices, &mesh as *const _ as *const c_void, 1, GRADIENT_FILL_RECT_V);
}

pub unsafe fn text_width(hdc: HDC, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT::default();
    DrawTextW(hdc, &mut wide, &mut rect, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
    rect.right - rect.left
}
