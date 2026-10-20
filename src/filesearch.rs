//! File and folder search for Spotlight through the Windows Search index (the
//! same index Explorer's search box uses), scoped to the user's profile. Runs on
//! a worker thread; only the newest query is answered, older ones are skipped.

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

use windows::core::{Result, HSTRING};
use windows::Storage::Search::{FolderDepth, IndexerOption, QueryOptions};
use windows::Storage::{StorageFolder, StorageItemTypes};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to Spotlight when results for query id `wparam` are ready.
pub const WM_FILES_FOUND: u32 = WM_APP + 4;

const MAX_RESULTS: u32 = 6;
/// Typing pauses this long before the index is asked.
const DEBOUNCE: Duration = Duration::from_millis(120);

#[derive(Clone)]
pub struct Found {
    pub name: String,
    pub path: String,
    pub folder: bool,
}

static RESULTS: Mutex<(u64, Vec<Found>)> = Mutex::new((0, Vec::new()));
static QUERIES: Mutex<Option<Sender<(u64, String)>>> = Mutex::new(None);

pub fn start(notify: HWND) {
    let (tx, rx) = mpsc::channel::<(u64, String)>();
    let notify = notify.0 as isize;
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let Some(home) = std::env::var_os("USERPROFILE") else { return };
        let Ok(root) = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(home.as_os_str())).and_then(|op| op.get()) else {
            return;
        };
        while let Ok(mut latest) = rx.recv() {
            std::thread::sleep(DEBOUNCE);
            if let Some(newer) = rx.try_iter().last() {
                latest = newer;
            }
            let (id, query) = latest;
            let found = search(&root, &query).unwrap_or_default();
            *RESULTS.lock().unwrap() = (id, found);
            unsafe {
                let _ = PostMessageW(HWND(notify as *mut _), WM_FILES_FOUND, WPARAM(id as usize), LPARAM(0));
            }
        }
    });
    *QUERIES.lock().unwrap() = Some(tx);
}

/// Asks for results for `query`, tagged with `id` so stale answers can be ignored.
pub fn request(id: u64, query: &str) {
    if let Some(tx) = &*QUERIES.lock().unwrap() {
        let _ = tx.send((id, query.to_owned()));
    }
}

/// Results for `id`, if they're the latest ones in.
pub fn results(id: u64) -> Option<Vec<Found>> {
    let results = RESULTS.lock().unwrap();
    (results.0 == id).then(|| results.1.clone())
}

fn search(root: &StorageFolder, query: &str) -> Result<Vec<Found>> {
    let options = QueryOptions::new()?;
    options.SetFolderDepth(FolderDepth::Deep)?;
    // Never fall back to crawling the disk; that would take ages.
    options.SetIndexerOption(IndexerOption::OnlyUseIndexer)?;
    options.SetUserSearchFilter(&HSTRING::from(query))?;
    let items = root.CreateItemQueryWithOptions(&options)?.GetItemsAsync(0, MAX_RESULTS)?.get()?;
    let mut found = Vec::new();
    for i in 0..items.Size()? {
        let item = items.GetAt(i)?;
        found.push(Found {
            name: item.Name()?.to_string(),
            path: item.Path()?.to_string(),
            folder: item.IsOfType(StorageItemTypes::Folder).unwrap_or(false),
        });
    }
    Ok(found)
}
