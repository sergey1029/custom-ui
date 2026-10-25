//! Data for the desktop widgets, fetched on a background thread: weather from
//! Open-Meteo (no API key; the city comes from IP geolocation unless one is set)
//! and headlines from an RSS feed.

use std::ffi::c_void;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{w, Error, Result, HRESULT, HSTRING, PCWSTR};
use windows::Data::Json::JsonObject;
use windows::Data::Xml::Dom::{IXmlNode, XmlDocument};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::{calendar, config};

/// Posted to the desktop when new weather or news data is in.
pub const WM_NET_UPDATE: u32 = WM_APP + 6;

pub const DEFAULT_FEED: &str = "https://lenta.ru/rss/news";
pub const WEATHER_EVERY: Duration = Duration::from_secs(30 * 60);
/// After a failed fetch, try again this soon instead of waiting a full period.
pub const RETRY_AFTER: Duration = Duration::from_secs(2 * 60);
const MAX_HEADLINES: u32 = 8;

#[derive(Clone)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    pub city: String,
}

#[derive(Clone)]
pub struct Day {
    /// 0 = Monday.
    pub weekday: u32,
    pub code: i32,
    pub high: f64,
    pub low: f64,
}

#[derive(Clone)]
pub struct Weather {
    pub city: String,
    pub temperature: f64,
    /// WMO weather code.
    pub code: i32,
    pub is_day: bool,
    pub high: f64,
    pub low: f64,
    pub days: Vec<Day>,
}

#[derive(Clone)]
pub struct Headline {
    pub title: String,
    pub link: String,
    /// "HH:MM" from the item's pubDate, if it has one.
    pub time: String,
}

#[derive(Clone)]
pub struct News {
    pub source: String,
    pub items: Vec<Headline>,
}

static WEATHER: Mutex<Option<Weather>> = Mutex::new(None);
static NEWS: Mutex<Option<News>> = Mutex::new(None);
/// Set once IP geolocation found the city, so the desktop can remember it.
static DISCOVERED: Mutex<Option<Location>> = Mutex::new(None);
static REFRESH: Mutex<Option<Sender<()>>> = Mutex::new(None);

pub fn weather() -> Option<Weather> {
    WEATHER.lock().unwrap().clone()
}

pub fn news() -> Option<News> {
    NEWS.lock().unwrap().clone()
}

pub fn discovered_location() -> Option<Location> {
    DISCOVERED.lock().unwrap().take()
}

/// Fetches everything again now.
pub fn refresh() {
    if let Some(tx) = &*REFRESH.lock().unwrap() {
        let _ = tx.send(());
    }
}

/// `city`, when set, overrides the remembered location (found by name once).
pub fn start(notify: HWND, location: Option<Location>, feed: String, city: String) {
    let (tx, rx) = mpsc::channel();
    let notify = notify.0 as isize;
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let mut location = location;
        let mut weather_due = Instant::now();
        let mut news_due = Instant::now();
        loop {
            let now = Instant::now();
            let mut changed = false;
            if now >= weather_due {
                let city = city.trim();
                if !city.is_empty() && location.as_ref().is_none_or(|l| !l.city.eq_ignore_ascii_case(city)) {
                    if let Ok(found) = geocode(city) {
                        location = Some(found);
                        *DISCOVERED.lock().unwrap() = location.clone();
                    }
                }
                if location.is_none() {
                    location = locate().ok();
                    *DISCOVERED.lock().unwrap() = location.clone();
                }
                match location.as_ref().map(fetch_weather) {
                    Some(Ok(weather)) => {
                        *WEATHER.lock().unwrap() = Some(weather);
                        weather_due = now + WEATHER_EVERY;
                        changed = true;
                    }
                    _ => weather_due = now + RETRY_AFTER,
                }
            }
            if now >= news_due {
                match fetch_news(&feed) {
                    Ok(news) => {
                        *NEWS.lock().unwrap() = Some(news);
                        news_due = now + Duration::from_secs(config::int("news.refresh") as u64 * 60);
                        changed = true;
                    }
                    Err(_) => news_due = now + RETRY_AFTER,
                }
            }
            if changed {
                unsafe {
                    let _ = PostMessageW(HWND(notify as *mut _), WM_NET_UPDATE, WPARAM(0), LPARAM(0));
                }
            }
            let wait = weather_due.min(news_due).saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait.max(Duration::from_secs(1))) {
                Ok(()) => {
                    weather_due = Instant::now();
                    news_due = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    *REFRESH.lock().unwrap() = Some(tx);
}

fn h(s: &str) -> HSTRING {
    HSTRING::from(s)
}

/// Closes a WinHTTP handle when dropped.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn check(handle: *mut c_void) -> Result<Handle> {
    if handle.is_null() {
        Err(Error::from_win32())
    } else {
        Ok(Handle(handle))
    }
}

/// GET over WinHTTP, the body decoded as UTF-8. Not WinRT's HttpClient: it
/// fails the TLS handshake with Open-Meteo (0x80072F8F) where the system TLS
/// stack is fine. Responses are small, so no compression is requested: WinHTTP's
/// automatic decompression aborts on Open-Meteo's gzip responses.
pub fn get(url: &str) -> Result<String> {
    let (secure, rest) = match url.split_once("://") {
        Some(("https", rest)) => (true, rest),
        Some((_, rest)) => (false, rest),
        None => (false, url),
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().unwrap_or(if secure { 443 } else { 80 })),
        None => (authority, if secure { 443 } else { 80 }),
    };
    unsafe {
        // Some feeds refuse requests without a browser-ish User-Agent.
        let session = check(WinHttpOpen(
            w!("Mozilla/5.0 (Windows NT 10.0) CustomUI"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        let connection = check(WinHttpConnect(session.0, &h(host), port, 0))?;
        let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let request = check(WinHttpOpenRequest(connection.0, w!("GET"), &h(path), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags))?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

        let mut status = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )?;
        if !(200..300).contains(&status) {
            return Err(Error::new(HRESULT(0x8019_0000u32 as i32 | status as i32), format!("HTTP {status}")));
        }

        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available)?;
            if available == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(request.0, body[start..].as_mut_ptr() as *mut c_void, available, &mut read)?;
            body.truncate(start + read as usize);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// City and coordinates from the public IP (ip-api.com, free, no key).
fn locate() -> Result<Location> {
    let json = JsonObject::Parse(&h(&get("http://ip-api.com/json/?lang=ru&fields=status,city,lat,lon")?))?;
    Ok(Location {
        lat: json.GetNamedNumber(&h("lat"))?,
        lon: json.GetNamedNumber(&h("lon"))?,
        city: json.GetNamedString(&h("city"))?.to_string(),
    })
}

/// Coordinates of a city by name (Open-Meteo's geocoding, free, no key). The
/// location keeps the name as typed, so the next start knows it's resolved.
pub fn geocode(city: &str) -> Result<Location> {
    let name: String = city
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    let url = format!("https://geocoding-api.open-meteo.com/v1/search?name={name}&count=1&language=ru&format=json");
    let json = JsonObject::Parse(&h(&get(&url)?))?;
    let first = json.GetNamedArray(&h("results"))?.GetObjectAt(0)?;
    Ok(Location { lat: first.GetNamedNumber(&h("latitude"))?, lon: first.GetNamedNumber(&h("longitude"))?, city: city.to_owned() })
}

pub fn fetch_weather(location: &Location) -> Result<Weather> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={:.4}&longitude={:.4}\
         &current=temperature_2m,weather_code,is_day\
         &daily=weather_code,temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=5",
        location.lat, location.lon
    );
    let json = JsonObject::Parse(&h(&get(&url)?))?;
    let current = json.GetNamedObject(&h("current"))?;
    let daily = json.GetNamedObject(&h("daily"))?;
    let dates = daily.GetNamedArray(&h("time"))?;
    let codes = daily.GetNamedArray(&h("weather_code"))?;
    let highs = daily.GetNamedArray(&h("temperature_2m_max"))?;
    let lows = daily.GetNamedArray(&h("temperature_2m_min"))?;
    let mut days = Vec::new();
    for i in 0..dates.Size()? {
        let date = dates.GetStringAt(i)?.to_string();
        let mut parts = date.split('-').filter_map(|p| p.parse::<u32>().ok());
        let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else { continue };
        days.push(Day {
            weekday: calendar::weekday(y as i32, m, d),
            code: codes.GetNumberAt(i)? as i32,
            high: highs.GetNumberAt(i)?,
            low: lows.GetNumberAt(i)?,
        });
    }
    let today = days.first().cloned();
    Ok(Weather {
        city: location.city.clone(),
        temperature: current.GetNamedNumber(&h("temperature_2m"))?,
        code: current.GetNamedNumber(&h("weather_code"))? as i32,
        is_day: current.GetNamedNumber(&h("is_day"))? != 0.0,
        high: today.as_ref().map_or(0.0, |d| d.high),
        low: today.as_ref().map_or(0.0, |d| d.low),
        days,
    })
}

fn fetch_news(feed: &str) -> Result<News> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&h(&get(feed)?))?;
    let nodes = doc.GetElementsByTagName(&h("item"))?;
    let mut items = Vec::new();
    for i in 0..nodes.Length()?.min(MAX_HEADLINES) {
        let node = nodes.Item(i)?;
        let title = child_text(&node, "title");
        if title.is_empty() {
            continue;
        }
        items.push(Headline { title, link: child_text(&node, "link"), time: clock_time(&child_text(&node, "pubDate")) });
    }
    Ok(News { source: host(feed), items })
}

fn child_text(node: &IXmlNode, tag: &str) -> String {
    node.SelectSingleNode(&h(tag))
        .and_then(|n| n.InnerText())
        .map(|t| t.to_string().trim().to_owned())
        .unwrap_or_default()
}

/// The first "HH:MM" in an RSS date like "Tue, 20 Oct 2026 17:20:00 +0300".
fn clock_time(date: &str) -> String {
    let bytes = date.as_bytes();
    (0..bytes.len().saturating_sub(4))
        .find(|&i| {
            bytes[i].is_ascii_digit() && bytes[i + 1].is_ascii_digit() && bytes[i + 2] == b':' && bytes[i + 3].is_ascii_digit() && bytes[i + 4].is_ascii_digit()
        })
        .map(|i| date[i..i + 5].to_owned())
        .unwrap_or_default()
}

/// "https://www.lenta.ru/rss/news" → "lenta.ru".
fn host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or(rest);
    host.trim_start_matches("www.").to_owned()
}

/// Russian description and a Segoe UI Symbol glyph for a WMO weather code.
pub fn describe(code: i32, is_day: bool) -> (&'static str, &'static str) {
    match code {
        0 if is_day => ("Ясно", "\u{2600}"),
        0 => ("Ясно", "\u{263E}"),
        1 | 2 => ("Переменная облачность", "\u{26C5}"),
        3 => ("Пасмурно", "\u{2601}"),
        45 | 48 => ("Туман", "\u{2601}"),
        51..=57 => ("Морось", "\u{2602}"),
        61..=67 | 80..=82 => ("Дождь", "\u{2602}"),
        71..=77 | 85 | 86 => ("Снег", "\u{2744}"),
        95..=99 => ("Гроза", "\u{26A1}"),
        _ => ("—", "\u{2601}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{clock_time, host};

    #[test]
    #[ignore]
    fn live_get() {
        for url in ["http://ip-api.com/json/?fields=city", "https://lenta.ru/rss/news", "https://api.open-meteo.com/v1/forecast?latitude=1&longitude=1&current=temperature_2m"] {
            match super::get(url) {
                Ok(body) => println!("OK {url}: {} bytes", body.len()),
                Err(e) => println!("ERR {url}: {e:?}"),
            }
        }
    }

    /// Hits the network; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_weather() {
        let location = super::Location { lat: 55.75, lon: 37.62, city: "Москва".into() };
        match super::fetch_weather(&location) {
            Ok(w) => println!("{} {} {} days", w.city, w.temperature, w.days.len()),
            Err(e) => panic!("{e:?}"),
        }
    }

    #[test]
    fn parsing_helpers() {
        assert_eq!(clock_time("Tue, 20 Oct 2026 17:20:00 +0300"), "17:20");
        assert_eq!(clock_time("no time here"), "");
        assert_eq!(host("https://www.lenta.ru/rss/news"), "lenta.ru");
        assert_eq!(host("https://meduza.io/rss/all"), "meduza.io");
    }
}
