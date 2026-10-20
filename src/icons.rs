//! Large shell icons with alpha, loaded through IShellItemImageFactory (the same
//! artwork Explorer and the Start menu use) and cached per parsing name and size.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY};
use windows::Win32::UI::WindowsAndMessaging::{DrawIconEx, DI_NORMAL, HICON};

use crate::gfx;

pub struct Icon {
    size: i32,
    /// Premultiplied BGRA, `size`×`size`.
    pixels: Vec<u32>,
    /// Last resampled size: icons mostly sit at one size, so this hits almost always.
    scaled: RefCell<Option<(i32, Rc<Vec<u32>>)>>,
}

impl Icon {
    pub fn scaled(&self, size: i32) -> Rc<Vec<u32>> {
        if let Some((cached, pixels)) = &*self.scaled.borrow() {
            if *cached == size {
                return pixels.clone();
            }
        }
        let pixels = Rc::new(resample(&self.pixels, self.size, size));
        *self.scaled.borrow_mut() = Some((size, pixels.clone()));
        pixels
    }
}

/// Renders an HICON (any kind: alpha, or old color+mask) at `size`. It's drawn
/// once over black and once over white; the difference between the two is the
/// coverage, and the result over black is already premultiplied color.
pub unsafe fn from_hicon(hicon: HICON, size: i32) -> Option<Icon> {
    let render = |background: u32| -> Option<Vec<u32>> {
        let mut canvas = gfx::Canvas::new(size, size)?;
        canvas.pixels().fill(background);
        DrawIconEx(canvas.dc, 0, 0, hicon, size, size, 0, None, DI_NORMAL).ok()?;
        Some(canvas.pixels().to_vec())
    };
    let black = render(0xFF00_0000)?;
    let white = render(0xFFFF_FFFF)?;
    let pixels = black
        .iter()
        .zip(&white)
        .map(|(&b, &w)| {
            let alpha = 255 - (w >> 8 & 0xFF).saturating_sub(b >> 8 & 0xFF);
            let channel = |shift: u32| (b >> shift & 0xFF).min(alpha) << shift;
            alpha << 24 | channel(16) | channel(8) | channel(0)
        })
        .collect();
    Some(Icon { size, pixels, scaled: RefCell::new(None) })
}

/// (parsing name, size) → icon, or `None` if the shell couldn't produce one.
type Cache = HashMap<(String, i32), Option<Rc<Icon>>>;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::default();
}

/// Icon for anything the shell can parse: an exe/lnk path, `shell:AppsFolder\<id>`,
/// or a `::{CLSID}` like the Recycle Bin. Failures are cached too.
pub fn get(parsing_name: &str, size: i32) -> Option<Rc<Icon>> {
    let key = (parsing_name.to_owned(), size);
    if let Some(hit) = CACHE.with_borrow(|c| c.get(&key).cloned()) {
        return hit;
    }
    let icon = unsafe { load(parsing_name, size) }.map(Rc::new);
    CACHE.with_borrow_mut(|c| c.insert(key, icon.clone()));
    icon
}

/// Icon pixels (size, premultiplied BGRA) loaded on any thread with COM
/// initialized, for `insert` on the UI thread later.
pub fn load_pixels(parsing_name: &str, size: i32) -> Option<(i32, Vec<u32>)> {
    unsafe { load(parsing_name, size) }.map(|icon| (icon.size, icon.pixels))
}

/// Caches what `load_pixels` produced (`None`: the shell had no icon).
pub fn insert(parsing_name: &str, size: i32, loaded: Option<(i32, Vec<u32>)>) {
    let icon = loaded.map(|(actual, pixels)| Rc::new(Icon { size: actual, pixels, scaled: RefCell::new(None) }));
    CACHE.with_borrow_mut(|c| c.insert((parsing_name.to_owned(), size), icon));
}

/// The icon if it's already loaded (or known to be missing), without loading
/// it: `None` means "not tried yet". Lets big grids fill in icons gradually.
pub fn peek(parsing_name: &str, size: i32) -> Option<Option<Rc<Icon>>> {
    CACHE.with_borrow(|c| c.get(&(parsing_name.to_owned(), size)).cloned())
}

unsafe fn load(parsing_name: &str, size: i32) -> Option<Icon> {
    let name = HSTRING::from(parsing_name);
    let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None::<&IBindCtx>).ok()?;
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let bitmap = factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY).ok()?;
    let pixels = read_bitmap(bitmap);
    let _ = DeleteObject(bitmap);
    let (size, pixels) = pixels?;
    Some(Icon { size, pixels, scaled: RefCell::new(None) })
}

unsafe fn read_bitmap(bitmap: HBITMAP) -> Option<(i32, Vec<u32>)> {
    let mut bm = BITMAP::default();
    if GetObjectW(bitmap, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
        return None;
    }
    let size = bm.bmWidth;
    if size <= 0 || bm.bmHeight != size {
        return None;
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u32; (size * size) as usize];
    let dc = CreateCompatibleDC(None);
    let lines = GetDIBits(dc, bitmap, 0, size as u32, Some(pixels.as_mut_ptr() as *mut c_void), &mut info, DIB_RGB_COLORS);
    let _ = DeleteDC(dc);
    if lines == 0 {
        return None;
    }
    fix_alpha(&mut pixels);
    Some((size, pixels))
}

/// The factory hands out premultiplied or straight alpha, or none at all for old icons.
fn fix_alpha(pixels: &mut [u32]) {
    if pixels.iter().all(|p| p >> 24 == 0) {
        for p in pixels.iter_mut() {
            *p |= 0xFF00_0000;
        }
        return;
    }
    let straight = pixels.iter().any(|&p| {
        let a = p >> 24;
        (p >> 16 & 0xFF) > a || (p >> 8 & 0xFF) > a || (p & 0xFF) > a
    });
    if straight {
        for p in pixels.iter_mut() {
            *p = gfx::premultiply(*p & 0x00FF_FFFF, *p >> 24);
        }
    }
}

/// Box filter: each target pixel averages the source pixels it covers. Works on
/// premultiplied pixels, so edges stay clean.
fn resample(src: &[u32], from: i32, to: i32) -> Vec<u32> {
    if from == to {
        return src.to_vec();
    }
    let span = |i: i32| {
        let start = i * from / to;
        (start, ((i + 1) * from / to).max(start + 1).min(from))
    };
    let mut out = vec![0u32; (to * to) as usize];
    for dy in 0..to {
        let (y0, y1) = span(dy);
        for dx in 0..to {
            let (x0, x1) = span(dx);
            let mut acc = [0u32; 4];
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = src[(y * from + x) as usize];
                    for (c, slot) in acc.iter_mut().enumerate() {
                        *slot += p >> (c * 8) & 0xFF;
                    }
                    n += 1;
                }
            }
            out[(dy * to + dx) as usize] = acc.iter().enumerate().fold(0, |v, (c, sum)| v | (sum / n) << (c * 8));
        }
    }
    out
}
