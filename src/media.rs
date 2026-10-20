//! Now-playing info from the system media transport controls — the same source
//! the Windows volume flyout uses, so it works with Spotify, browsers, players, etc.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use windows::core::Result;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to the bar whenever the current track or its play state changes.
pub const WM_MEDIA_CHANGED: u32 = WM_APP + 2;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub playing: bool,
}

pub enum Command {
    TogglePlayPause,
    Next,
    Previous,
}

static CURRENT: Mutex<Option<Track>> = Mutex::new(None);
static COMMANDS: Mutex<Option<Sender<Command>>> = Mutex::new(None);

pub fn current() -> Option<Track> {
    CURRENT.lock().unwrap().clone()
}

/// Passes a user command (from the menu bar or Control Center) to the worker.
pub fn send(command: Command) {
    if let Some(tx) = &*COMMANDS.lock().unwrap() {
        let _ = tx.send(command);
    }
}

/// Stops the worker by dropping its command channel.
pub fn stop() {
    COMMANDS.lock().unwrap().take();
}

/// Starts the worker thread; WinRT async calls block, so they stay off the UI thread.
pub fn start(notify: HWND) {
    let (tx, rx) = mpsc::channel();
    // HWND isn't Send; the raw value is fine to use from any thread for PostMessage.
    let notify = notify.0 as isize;
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let Ok(manager) = Manager::RequestAsync().and_then(|op| op.get()) else { return };
        loop {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(command) => {
                    let _ = run(&manager, command);
                }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let track = query(&manager).ok().flatten();
            let changed = {
                let mut current = CURRENT.lock().unwrap();
                let changed = *current != track;
                *current = track;
                changed
            };
            if changed {
                unsafe {
                    let _ = PostMessageW(HWND(notify as *mut _), WM_MEDIA_CHANGED, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
    *COMMANDS.lock().unwrap() = Some(tx);
}

fn query(manager: &Manager) -> Result<Option<Track>> {
    // Errors here just mean nothing is playing.
    let Ok(session) = manager.GetCurrentSession() else { return Ok(None) };
    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = props.Title()?.to_string();
    if title.is_empty() {
        return Ok(None);
    }
    let artist = props.Artist()?.to_string();
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;
    Ok(Some(Track { title, artist, playing }))
}

fn run(manager: &Manager, command: Command) -> Result<()> {
    let session = manager.GetCurrentSession()?;
    match command {
        Command::TogglePlayPause => session.TryTogglePlayPauseAsync()?.get()?,
        Command::Next => session.TrySkipNextAsync()?.get()?,
        Command::Previous => session.TrySkipPreviousAsync()?.get()?,
    };
    Ok(())
}
