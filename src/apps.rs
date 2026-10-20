//! Apps as Windows 11's Start menu lists them (shell:AppsFolder, which includes
//! Store apps), plus helpers for launching things and naming executables.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;

use windows::core::{Interface, Result, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Storage::EnhancedStorage::{PKEY_FileDescription, PKEY_Link_TargetParsingPath};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

pub struct App {
    pub name: String,
    /// Parsing name inside AppsFolder: the AppUserModelID.
    pub id: String,
    /// The exe a desktop app's shortcut points to, when it has one.
    pub target: Option<String>,
}

impl App {
    /// What ShellExecute and the icon loader understand for this app.
    pub fn parsing_name(&self) -> String {
        format!("shell:AppsFolder\\{}", self.id)
    }
}

/// All apps, sorted by name.
pub fn all() -> Vec<App> {
    unsafe { enumerate().unwrap_or_default() }
}

unsafe fn enumerate() -> Result<Vec<App>> {
    let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, HANDLE::default())?;
    let items: IEnumShellItems = folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)?;
    let mut apps = Vec::new();
    loop {
        let mut batch = [None];
        let mut fetched = 0;
        if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
            break;
        }
        let Some(item) = batch[0].take() else { break };
        let (Ok(name), Ok(id)) = (
            item.GetDisplayName(SIGDN_NORMALDISPLAY).map(|p| take(p)),
            item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING).map(|p| take(p)),
        ) else {
            continue;
        };
        let target = item
            .cast::<IShellItem2>()
            .ok()
            .and_then(|i| i.GetString(&PKEY_Link_TargetParsingPath).ok())
            .map(|p| take(p))
            .filter(|t| !t.is_empty());
        apps.push(App { name, id, target });
    }
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    Ok(apps)
}

/// Converts a shell-allocated string and frees it.
unsafe fn take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const c_void));
    s
}

/// Opens a file, folder, exe or `shell:` path the way Explorer would.
pub fn open(target: &str) {
    let file = HSTRING::from(target);
    unsafe {
        ShellExecuteW(HWND::default(), PCWSTR::null(), PCWSTR(file.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Starts `program` with command-line `args`, without a console window showing.
pub fn run(program: &str, args: &str) {
    let (program, args) = (HSTRING::from(program), HSTRING::from(args));
    unsafe {
        ShellExecuteW(HWND::default(), PCWSTR::null(), PCWSTR(program.as_ptr()), PCWSTR(args.as_ptr()), PCWSTR::null(), SW_HIDE);
    }
}

/// Opens an Explorer window with `path` selected.
pub fn reveal(path: &str) {
    let args = HSTRING::from(format!("/select,\"{path}\""));
    unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR::null(),
            windows::core::w!("explorer.exe"),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

thread_local! {
    static NAMES: RefCell<HashMap<String, String>> = RefCell::default();
}

fn cached(key: &str, compute: impl FnOnce() -> String) -> String {
    if let Some(name) = NAMES.with_borrow(|n| n.get(key).cloned()) {
        return name;
    }
    let name = compute();
    NAMES.with_borrow_mut(|n| n.insert(key.to_owned(), name.clone()));
    name
}

/// Human name for an executable: its version-info description ("Google Chrome"),
/// else the file name.
pub fn exe_name(path: &str) -> String {
    cached(path, || {
        let described = unsafe {
            let wide = HSTRING::from(path);
            SHCreateItemFromParsingName::<_, _, IShellItem2>(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)
                .and_then(|item| item.GetString(&PKEY_FileDescription))
                .map(|p| take(p))
                .ok()
        };
        described
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
    })
}

/// Where a .lnk shortcut points, if it points at a file system path.
pub fn link_target(path: &str) -> Option<String> {
    unsafe {
        let wide = HSTRING::from(path);
        SHCreateItemFromParsingName::<_, _, IShellItem2>(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)
            .and_then(|item| item.GetString(&PKEY_Link_TargetParsingPath))
            .map(|p| take(p))
            .ok()
            .filter(|t| !t.is_empty())
    }
}

/// Display name of any shell item, e.g. `shell:AppsFolder\<id>`.
pub fn item_name(parsing_name: &str) -> Option<String> {
    let name = cached(parsing_name, || unsafe {
        let wide = HSTRING::from(parsing_name);
        SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)
            .and_then(|item| item.GetDisplayName(SIGDN_NORMALDISPLAY))
            .map(|p| take(p))
            .unwrap_or_default()
    });
    (!name.is_empty()).then_some(name)
}
