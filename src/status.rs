//! System status for the menu bar's right side: keyboard layout, volume,
//! network and battery, plus the actions their icons trigger.

use std::cell::RefCell;

use windows::Win32::Foundation::{BOOL, LPARAM, WPARAM};
use windows::Win32::Globalization::{GetLocaleInfoW, LOCALE_SISO639LANGNAME};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST,
    IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, PostMessageW, WM_INPUTLANGCHANGEREQUEST,
};

use crate::apps;

#[derive(Clone, Copy, PartialEq)]
pub struct Volume {
    /// 0.0..=1.0
    pub level: f32,
    pub muted: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Network {
    Wifi,
    Ethernet,
    Offline,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

thread_local! {
    static ENUMERATOR: RefCell<Option<IMMDeviceEnumerator>> = const { RefCell::new(None) };
}

/// The default output device's volume control. Looked up on every call so a
/// device switch (headphones plugged in) is followed.
fn endpoint() -> Option<IAudioEndpointVolume> {
    ENUMERATOR.with_borrow_mut(|cached| unsafe {
        if cached.is_none() {
            *cached = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
        }
        let device = cached.as_ref()?.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    })
}

pub fn volume() -> Option<Volume> {
    let endpoint = endpoint()?;
    unsafe {
        Some(Volume {
            level: endpoint.GetMasterVolumeLevelScalar().ok()?,
            muted: endpoint.GetMute().ok()?.as_bool(),
        })
    }
}

/// Changes the volume by `delta` (e.g. 0.02) and unmutes, like the volume keys do.
pub fn change_volume(delta: f32) {
    let Some(endpoint) = endpoint() else { return };
    unsafe {
        let level = endpoint.GetMasterVolumeLevelScalar().unwrap_or(0.0);
        let _ = endpoint.SetMasterVolumeLevelScalar((level + delta).clamp(0.0, 1.0), std::ptr::null());
        let _ = endpoint.SetMute(BOOL::from(false), std::ptr::null());
    }
}

/// Segoe Fluent speaker glyph for the level (crossed out when muted).
pub fn volume_glyph(v: &Volume) -> &'static str {
    match v.level {
        _ if v.muted => "\u{E74F}",
        l if l < 0.01 => "\u{E992}",
        l if l < 0.34 => "\u{E993}",
        l if l < 0.67 => "\u{E994}",
        _ => "\u{E995}",
    }
}

/// Sets an absolute level (0.0..=1.0), e.g. from a slider, and unmutes.
pub fn set_volume(level: f32) {
    let Some(endpoint) = endpoint() else { return };
    unsafe {
        let _ = endpoint.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
        let _ = endpoint.SetMute(BOOL::from(false), std::ptr::null());
    }
}

pub fn toggle_mute() {
    let Some(endpoint) = endpoint() else { return };
    unsafe {
        let muted = endpoint.GetMute().map(|m| m.as_bool()).unwrap_or(false);
        let _ = endpoint.SetMute(BOOL::from(!muted), std::ptr::null());
    }
}

/// Connected = an adapter that is up and has a default gateway; Wi-Fi wins
/// over Ethernet when both are.
pub fn network() -> Network {
    unsafe {
        // u64 keeps the buffer aligned for the structs inside it.
        let mut buffer = vec![0u64; 4096];
        let mut size = (buffer.len() * 8) as u32;
        let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
        let first = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        if GetAdaptersAddresses(0, flags, None, Some(first), &mut size) != 0 {
            return Network::Offline;
        }
        let mut best = Network::Offline;
        let mut adapter = first;
        while let Some(a) = adapter.as_ref() {
            if a.OperStatus == IfOperStatusUp && !a.FirstGatewayAddress.is_null() {
                if a.IfType == IF_TYPE_IEEE80211 {
                    return Network::Wifi;
                }
                best = Network::Ethernet;
            }
            adapter = a.Next;
        }
        best
    }
}

/// `None` on machines without a battery.
pub fn battery() -> Option<Battery> {
    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut status).ok()? };
    // 128 = no system battery, 255 = unknown.
    if status.BatteryFlag & 128 != 0 || status.BatteryFlag == 255 || status.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery { percent: status.BatteryLifePercent, charging: status.ACLineStatus == 1 })
}

/// Two-letter code of the foreground window's input language ("RU", "EN").
pub fn layout() -> String {
    unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let lang = (GetKeyboardLayout(thread).0 as usize & 0xFFFF) as u32;
        let mut buf = [0u16; 9];
        let len = GetLocaleInfoW(lang, LOCALE_SISO639LANGNAME, Some(&mut buf));
        if len <= 1 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize - 1]).to_uppercase()
    }
}

/// Switches the foreground window to the next input language, as Alt+Shift would.
pub fn next_layout() {
    const HKL_NEXT: isize = 1;
    unsafe {
        let _ = PostMessageW(GetForegroundWindow(), WM_INPUTLANGCHANGEREQUEST, WPARAM(0), LPARAM(HKL_NEXT));
    }
}

/// Windows 11's network quick-settings flyout.
pub fn open_network() {
    apps::open("ms-availablenetworks:");
}

pub fn open_sound_settings() {
    apps::open("ms-settings:sound");
}

pub fn open_power_settings() {
    apps::open("ms-settings:batterysaver");
}
