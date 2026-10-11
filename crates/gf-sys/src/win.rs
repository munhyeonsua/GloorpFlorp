//! Windows implementations behind the portable API in `lib.rs`.

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{BOOL, PWSTR};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ERROR_NOT_FOUND};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, MONITORINFOF_PRIMARY,
    WS_EX_TOOLWINDOW,
};

use crate::{AudioApp, GameAudioSource, Monitor, SampleCallback};

/// Owned kernel handle, closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn list_monitors() -> windows::core::Result<Vec<Monitor>> {
    // SAFETY: plain DXGI calls; every out-struct is a properly sized local.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = factory.EnumAdapters1(0)?;
        let mut monitors = Vec::new();
        for i in 0.. {
            let output = match adapter.EnumOutputs(i) {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e),
            };
            let desc = output.GetDesc()?;
            let r = desc.DesktopCoordinates;
            let width = r.right.saturating_sub(r.left).max(0) as u32;
            let height = r.bottom.saturating_sub(r.top).max(0) as u32;
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let primary = GetMonitorInfoW(desc.Monitor, &mut info).as_bool()
                && info.dwFlags & MONITORINFOF_PRIMARY != 0;
            monitors.push(Monitor::new(i, width, height, primary));
        }
        log::debug!("monitors: {monitors:?}");
        Ok(monitors)
    }
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the `&mut Vec<HWND>` passed to EnumWindows below,
    // which outlives the (synchronous) enumeration.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    true.into()
}

/// Visible, unowned, titled, non-tool, non-cloaked top-level window?
unsafe fn is_app_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) <= 0 {
            return false;
        }
        if matches!(GetWindow(hwnd, GW_OWNER), Ok(owner) if !owner.is_invalid()) {
            return false;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // Hidden UWP frames (Settings, Text Input, ...) report visible but cloaked.
        let mut cloaked = 0u32;
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        );
        ok.is_err() || cloaked == 0
    }
}

unsafe fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn exe_name(pid: u32) -> Option<String> {
    // SAFETY: the handle is owned and closed by OwnedHandle; buffer and size match.
    unsafe {
        let process = OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
        let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        (!name.is_empty()).then(|| name.to_string())
    }
}

pub(crate) fn list_audio_apps() -> Vec<AudioApp> {
    let mut windows: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `windows`, which outlives the call.
    if let Err(e) = unsafe { EnumWindows(Some(collect_window), LPARAM(&raw mut windows as isize)) }
    {
        log::warn!("couldn't list windows: {e}");
        return Vec::new();
    }
    let own_pid = std::process::id();
    let mut apps: Vec<AudioApp> = Vec::new();
    for hwnd in windows {
        // SAFETY: hwnd came from EnumWindows; a window closing meanwhile only
        // makes these calls fail harmlessly.
        unsafe {
            if !is_app_window(hwnd) {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 || pid == own_pid || apps.iter().any(|a| a.pid == pid) {
                continue;
            }
            let title = window_title(hwnd);
            if title.trim().is_empty() {
                continue;
            }
            // Keep programs whose exe we can't read (e.g. protected) so they
            // can still be picked by title.
            let exe = exe_name(pid).unwrap_or_else(|| format!("Process {pid}"));
            apps.push(AudioApp { pid, exe, title });
        }
    }
    apps
}

/// Windows build number (e.g. 19041), not affected by compatibility shims.
fn windows_build() -> Option<u32> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: info is a correctly sized, initialised OSVERSIONINFOW.
    let status = unsafe { RtlGetVersion(&mut info) };
    status.is_ok().then_some(info.dwBuildNumber)
}

/// Windows 10 version 2004, the first with process loopback capture.
const MIN_BUILD: u32 = 19041;
const TOO_OLD: &str = "Game-audio capture needs Windows 10 version 2004 or newer.";
const PROGRAM_CLOSED: &str =
    "The program you picked for game audio has closed. Start it again, then pick it again in the game-audio list.";

/// Body of the capture thread (see `capture::launch`).
pub(crate) fn loopback_capture(
    source: GameAudioSource,
    on_samples: SampleCallback,
    stop: &AtomicBool,
    ready: &dyn Fn(),
) -> Result<(), String> {
    match windows_build() {
        Some(build) if build < MIN_BUILD => return Err(TOO_OLD.to_string()),
        Some(build) => log::debug!("Windows build {build}"),
        None => log::warn!("couldn't read the Windows version; trying game-audio capture anyway"),
    }
    let hr = wasapi::initialize_mta();
    if hr.is_err() {
        return Err(format!(
            "Couldn't start game-audio capture (COM error {:#010x}). Restart GloorpFlorp and try again.",
            hr.0
        ));
    }
    // All COM objects live inside `capture_loop` and are released before CoUninitialize.
    let result = capture_loop(source, on_samples, stop, ready);
    wasapi::deinitialize();
    result
}

fn wasapi_error(what: &str, e: wasapi::WasapiError) -> String {
    format!("Game-audio capture failed while {what} ({e}). Turn game audio off and on again, or restart GloorpFlorp.")
}

fn capture_loop(
    source: GameAudioSource,
    mut on_samples: SampleCallback,
    stop: &AtomicBool,
    ready: &dyn Fn(),
) -> Result<(), String> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WasapiError, WaveFormat};

    // include_tree=false is PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE.
    let (pid, include_tree) = match source {
        GameAudioSource::AllExceptThisApp => (std::process::id(), false),
        GameAudioSource::Process { pid } => (pid, true),
    };
    // Watch the chosen program so we can say when it has closed.
    let watched = match source {
        GameAudioSource::AllExceptThisApp => None,
        GameAudioSource::Process { pid } => {
            // SAFETY: plain handle open; owned by OwnedHandle.
            match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
                Ok(h) => Some(OwnedHandle(h)),
                Err(e) => {
                    log::warn!("can't open process {pid}: {e}");
                    return Err(PROGRAM_CLOSED.to_string());
                }
            }
        }
    };
    let has_exited = || {
        watched.as_ref().is_some_and(|h| {
            // SAFETY: valid owned handle; zero timeout only polls.
            (unsafe { WaitForSingleObject(h.0, 0) }) == WAIT_OBJECT_0
        })
    };

    let mut client =
        AudioClient::new_application_loopback_client(pid, include_tree).map_err(|e| {
            match windows_build() {
                Some(b) if b < MIN_BUILD => TOO_OLD.to_string(),
                _ => wasapi_error("connecting to Windows audio", e),
            }
        })?;
    // Process loopback can't report a mix format; ask for ours and let
    // Windows convert (AUTOCONVERTPCM). 20 ms buffer as in Microsoft's sample.
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| wasapi_error("setting up the audio format", e))?;
    let event = client
        .set_get_eventhandle()
        .map_err(|e| wasapi_error("setting up audio events", e))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|e| wasapi_error("opening the capture stream", e))?;
    client
        .start_stream()
        .map_err(|e| wasapi_error("starting the capture stream", e))?;
    log::info!("game-audio process loopback running (pid {pid}, include_tree {include_tree})");
    ready();

    let block_align = format.get_blockalign() as usize;
    let mut bytes: Vec<u8> = vec![0; 4800 * block_align];
    let mut samples: Vec<f32> = Vec::with_capacity(4800 * 2);
    let mut last_exit_check = Instant::now();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match event.wait_for_event(100) {
            Ok(()) | Err(WasapiError::EventTimeout) => {}
            Err(e) => break Err(wasapi_error("waiting for audio", e)),
        }
        let drained = (|| -> Result<(), WasapiError> {
            loop {
                let frames = capture.get_next_packet_size()?.unwrap_or(0) as usize;
                if frames == 0 {
                    return Ok(());
                }
                if bytes.len() < frames * block_align {
                    bytes.resize(frames * block_align, 0);
                }
                let (n, info) = capture.read_from_device(&mut bytes)?;
                let n = n as usize;
                if n == 0 {
                    return Ok(());
                }
                samples.clear();
                if info.flags.silent {
                    samples.resize(n * 2, 0.0);
                } else {
                    samples.extend(
                        bytes[..n * block_align]
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                    );
                }
                on_samples(&samples);
            }
        })();
        if let Err(e) = drained {
            break Err(if has_exited() {
                PROGRAM_CLOSED.to_string()
            } else {
                wasapi_error("reading audio", e)
            });
        }
        if last_exit_check.elapsed() >= Duration::from_millis(250) {
            last_exit_check = Instant::now();
            if has_exited() {
                break Err(PROGRAM_CLOSED.to_string());
            }
        }
    };
    if let Err(e) = client.stop_stream() {
        log::debug!("stopping loopback stream: {e}");
    }
    result
}

/// Process-wide job object that kills its members when this app exits.
/// Stored as the raw handle value; it is never closed.
fn app_job() -> Option<isize> {
    static JOB: OnceLock<Option<isize>> = OnceLock::new();
    *JOB.get_or_init(|| {
        // SAFETY: creating and configuring a fresh, unnamed job object.
        unsafe {
            let job = match CreateJobObjectW(None, None) {
                Ok(j) => j,
                Err(e) => {
                    log::warn!("couldn't create job object: {e}");
                    return None;
                }
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Err(e) = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) {
                log::warn!("couldn't configure job object: {e}");
                let _ = CloseHandle(job);
                return None;
            }
            Some(job.0 as isize)
        }
    })
}

pub(crate) fn bind_child_to_app_lifetime(child: &std::process::Child) {
    let Some(job) = app_job() else { return };
    let process = HANDLE(child.as_raw_handle());
    // SAFETY: both handles are valid for the duration of the call; the child
    // handle stays owned by `child`.
    if let Err(e) = unsafe { AssignProcessToJobObject(HANDLE(job as *mut _), process) } {
        log::warn!(
            "couldn't tie child process {} to the app's lifetime: {e}",
            child.id()
        );
    }
}
