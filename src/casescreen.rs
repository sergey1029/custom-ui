//! The numeric display on the case: the generic "数显" HID panel (VID 5131,
//! PID 2007) that HT PLAYER drives. It shows three numbers — CPU °C, GPU °C,
//! RAM % — from a report that has to keep coming every 200 ms, or it goes dark.
//! Temperatures come from LibreHardwareMonitor's web server (reading them
//! ourselves needs a kernel driver); in turns, the first field shows the
//! outdoor temperature instead: for the screen's own city if one is set, else
//! the weather widget's.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{Result, HSTRING, PCWSTR};
use windows::Data::Json::JsonObject;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::{CreateFileW, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::{config, net};

/// Lowercase, as it appears in the device interface path.
const DEVICE_ID: &str = "vid_5131&pid_2007";
/// The device's output report: report id 1 plus 64 bytes.
const REPORT_LEN: usize = 65;
const SEND_EVERY: Duration = Duration::from_millis(200);
const SENSORS_EVERY: Duration = Duration::from_secs(1);
/// How often to look for the display while it isn't plugged in.
const FIND_EVERY: Duration = Duration::from_secs(5);
const LHM_URL: &str = "http://127.0.0.1:8085/data.json";
/// LibreHardwareMonitor sensor names to prefer, best first (Intel, AMD).
const CPU_SENSORS: [&str; 3] = ["CPU Package", "Core (Tctl/Tdie)", "Core Max"];
const GPU_SENSORS: [&str; 1] = ["GPU Core"];

const GENERIC_READ_WRITE: u32 = 0xC000_0000;

/// Outdoor °C in the screen's own city, once fetched.
static OUTDOOR: Mutex<Option<f64>> = Mutex::new(None);

pub fn start() {
    if !config::flag("casescreen.enabled") {
        return;
    }
    let weather = config::flag("casescreen.weather");
    let city = config::text("casescreen.city").trim().to_owned();
    let own_city = weather && !city.is_empty();
    if own_city {
        watch_city(city);
    }
    let stats_for = Duration::from_secs(config::int("casescreen.stats_seconds") as u64);
    let weather_for = Duration::from_secs(config::int("casescreen.weather_seconds") as u64);
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let started = Instant::now();
        let mut device: Option<Device> = None;
        let mut sensors = Sensors::default();
        let mut sensors_due = started;
        loop {
            if device.is_none() {
                device = Device::open();
                if device.is_none() {
                    std::thread::sleep(FIND_EVERY);
                    continue;
                }
            }
            let now = Instant::now();
            if now >= sensors_due {
                sensors = Sensors::read();
                sensors_due = now + SENSORS_EVERY;
            }
            let outdoor = match (weather, own_city) {
                (false, _) => None,
                (true, true) => *OUTDOOR.lock().unwrap(),
                (true, false) => net::weather().map(|w| w.temperature),
            };
            let cycle = (stats_for + weather_for).as_millis().max(1);
            let first = match outdoor {
                // Weather after the sensors in each cycle. The display has no
                // minus sign, so frost shows as its absolute value.
                Some(t) if (now - started).as_millis() % cycle >= stats_for.as_millis() => t.abs(),
                _ => sensors.cpu.unwrap_or(0.0),
            };
            let packet = report(byte(first), byte(sensors.gpu.unwrap_or(0.0)), byte(ram_load()), &unsafe { GetLocalTime() });
            if device.as_ref().is_some_and(|d| d.write(&packet).is_err()) {
                // Unplugged: look for it again.
                device = None;
            }
            std::thread::sleep(SEND_EVERY);
        }
    });
}

/// Keeps OUTDOOR current for `city`, on its own thread so a slow network never
/// holds up the reports.
fn watch_city(city: String) {
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let mut location = None;
        loop {
            if location.is_none() {
                location = net::geocode(&city).ok();
            }
            let temperature = location.as_ref().and_then(|l| net::fetch_weather(l).ok()).map(|w| w.temperature);
            if temperature.is_some() {
                *OUTDOOR.lock().unwrap() = temperature;
            }
            std::thread::sleep(if temperature.is_some() { net::WEATHER_EVERY } else { net::RETRY_AFTER });
        }
    });
}

/// An open handle to the display.
struct Device(HANDLE);

impl Device {
    fn open() -> Option<Device> {
        let path = find_path()?;
        let path: Vec<u16> = path.encode_utf16().chain([0]).collect();
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .ok()
        .map(Device)
    }

    fn write(&self, packet: &[u8; REPORT_LEN]) -> Result<()> {
        unsafe { WriteFile(self.0, Some(packet), None, None) }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// The display's HID interface path, if it is plugged in.
fn find_path() -> Option<String> {
    unsafe {
        let guid = HidD_GetHidGuid();
        let mut len = 0u32;
        if CM_Get_Device_Interface_List_SizeW(&mut len, &guid, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS {
            return None;
        }
        let mut buffer = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(&guid, PCWSTR::null(), &mut buffer, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS {
            return None;
        }
        // A list of NUL-terminated paths, ending with an empty one.
        buffer
            .split(|&c| c == 0)
            .map(String::from_utf16_lossy)
            .find(|p| p.to_lowercase().contains(DEVICE_ID))
    }
}

/// The report HT PLAYER sends this display (its generic layout). The display
/// reads CPU °C from byte 4, GPU °C from byte 14 and RAM % from byte 34; the
/// rest is filled in the same way in case the firmware looks at it.
fn report(cpu: u8, gpu: u8, ram: u8, time: &SYSTEMTIME) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[..4].copy_from_slice(&[1, 0, 1, 2]);
    r[4] = cpu;
    r[14] = gpu;
    r[26] = (time.wYear / 100) as u8;
    r[27] = (time.wYear % 100) as u8;
    r[28] = time.wMonth as u8;
    r[29] = time.wDay as u8;
    r[30] = time.wHour as u8;
    r[31] = time.wMinute as u8;
    r[32] = time.wSecond as u8;
    r[33] = time.wDayOfWeek as u8;
    r[34] = ram;
    // HT PLAYER's defaults: cycle interval and "dark flash".
    r[38] = 5;
    r[40] = 1;
    r
}

/// The display shows a byte as is, 0–255.
fn byte(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

fn ram_load() -> f64 {
    let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut status) }.map_or(0.0, |_| status.dwMemoryLoad as f64)
}

/// Temperatures from LibreHardwareMonitor; None while it isn't running.
#[derive(Default)]
struct Sensors {
    cpu: Option<f64>,
    gpu: Option<f64>,
}

/// One temperature sensor: its id like "/intelcpu/0/temperature/8", name, °C.
struct Reading {
    id: String,
    name: String,
    value: f64,
}

impl Sensors {
    fn read() -> Sensors {
        let mut readings = Vec::new();
        if let Ok(root) = net::get(LHM_URL).and_then(|body| JsonObject::Parse(&HSTRING::from(body))) {
            let _ = collect(&root, &mut readings);
        }
        Sensors { cpu: pick(&readings, "cpu/", &CPU_SENSORS), gpu: pick(&readings, "/gpu-", &GPU_SENSORS) }
    }
}

/// Walks LibreHardwareMonitor's tree (nodes with "Children"; sensors also have
/// "SensorId" and a "Value" like "48,7 °C") and keeps the temperatures.
fn collect(node: &JsonObject, out: &mut Vec<Reading>) -> Result<()> {
    let key = |k: &str| HSTRING::from(k);
    if node.HasKey(&key("SensorId"))? {
        let id = node.GetNamedString(&key("SensorId"))?.to_string();
        if id.contains("/temperature/") {
            if let Some(value) = parse_value(&node.GetNamedString(&key("Value"))?.to_string()) {
                out.push(Reading { id, name: node.GetNamedString(&key("Text"))?.to_string(), value });
            }
        }
    }
    if node.HasKey(&key("Children"))? {
        let children = node.GetNamedArray(&key("Children"))?;
        for i in 0..children.Size()? {
            collect(&children.GetObjectAt(i)?, out)?;
        }
    }
    Ok(())
}

/// The preferred sensor among those whose id contains `device`, else the first.
fn pick(readings: &[Reading], device: &str, preferred: &[&str]) -> Option<f64> {
    let mine: Vec<&Reading> = readings.iter().filter(|r| r.id.contains(device)).collect();
    preferred
        .iter()
        .find_map(|name| mine.iter().find(|r| r.name == *name))
        .or(mine.first())
        .map(|r| r.value)
}

/// "48,7 °C" (the decimal separator follows the system locale) → 48.7.
fn parse_value(text: &str) -> Option<f64> {
    let number: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | '-'))
        .map(|c| if c == ',' { '.' } else { c })
        .collect();
    number.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        assert_eq!(parse_value("48,7 °C"), Some(48.7));
        assert_eq!(parse_value("53.0 °C"), Some(53.0));
        assert_eq!(parse_value("-3,5 °C"), Some(-3.5));
        assert_eq!(parse_value("n/a"), None);
        assert_eq!(byte(47.6), 48);
        assert_eq!(byte(-4.0), 0);
        assert_eq!(byte(300.0), 255);
    }

    #[test]
    fn picks_preferred_sensor() {
        let reading = |id: &str, name: &str, value| Reading { id: id.into(), name: name.into(), value };
        let readings = [
            reading("/lpc/it8686e/0/temperature/0", "Temperature #1", 39.0),
            reading("/intelcpu/0/temperature/0", "Core Max", 52.0),
            reading("/intelcpu/0/temperature/8", "CPU Package", 53.0),
            reading("/gpu-nvidia/0/temperature/2", "GPU Hot Spot", 67.6),
            reading("/gpu-nvidia/0/temperature/0", "GPU Core", 55.0),
        ];
        assert_eq!(pick(&readings, "cpu/", &CPU_SENSORS), Some(53.0));
        assert_eq!(pick(&readings, "/gpu-", &GPU_SENSORS), Some(55.0));
        assert_eq!(pick(&readings[..1], "cpu/", &CPU_SENSORS), None);
    }

    #[test]
    fn report_layout() {
        let time = SYSTEMTIME { wYear: 2026, wMonth: 10, wDayOfWeek: 0, wDay: 25, wHour: 13, wMinute: 5, wSecond: 9, wMilliseconds: 0 };
        let r = report(12, 34, 56, &time);
        assert_eq!(&r[..5], &[1, 0, 1, 2, 12]);
        assert_eq!(r[14], 34);
        assert_eq!(r[34], 56);
        assert_eq!(&r[26..34], &[20, 26, 10, 25, 13, 5, 9, 0]);
    }
}
