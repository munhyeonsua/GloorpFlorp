//! Windows implementations behind the portable API in `lib.rs`.

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{BOOL, PWSTR};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    CloseHandle, E_INVALIDARG, HANDLE, HWND, LPARAM, RECT, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
    DXGI_MODE_ROTATION_ROTATE90,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ERROR_NOT_FOUND};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindow,
    GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, MONITORINFOF_PRIMARY, WS_EX_TOOLWINDOW,
};

use crate::{AudioApp, GameAudioSource, Monitor, SampleCallback, WindowState};

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
            monitors.push(Monitor {
                hmonitor: desc.Monitor.0 as u64,
                ..Monitor::new(i, width, height, primary, rotation_degrees(desc.Rotation))
            });
        }
        log::debug!("monitors: {monitors:?}");
        Ok(monitors)
    }
}

/// Clockwise degrees for a DXGI rotation (unspecified counts as none).
fn rotation_degrees(r: DXGI_MODE_ROTATION) -> u32 {
    match r {
        DXGI_MODE_ROTATION_ROTATE90 => 90,
        DXGI_MODE_ROTATION_ROTATE180 => 180,
        DXGI_MODE_ROTATION_ROTATE270 => 270,
        _ => 0,
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

/// Store (UWP) apps: the visible window is a frame owned by
/// ApplicationFrameHost.exe; the app's own process owns a child window of it.
const UWP_FRAME_CLASS: &str = "ApplicationFrameWindow";
const UWP_APP_CLASS: &str = "Windows.UI.Core.CoreWindow";
const UWP_FRAME_HOST_EXE: &str = "ApplicationFrameHost.exe";

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

unsafe fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

unsafe extern "system" fn find_uwp_app(child: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the `&mut u32` passed to EnumChildWindows below,
    // which outlives the (synchronous) enumeration.
    let found = unsafe { &mut *(lparam.0 as *mut u32) };
    if unsafe { class_name(child) } == UWP_APP_CLASS {
        *found = unsafe { window_pid(child) };
    }
    // Keep looking until found.
    (*found == 0).into()
}

/// The process behind a Store app's frame window; `None` while the app is
/// suspended (its window isn't inside the frame then).
unsafe fn uwp_app_pid(frame: HWND, frame_pid: u32) -> Option<u32> {
    let mut pid = 0u32;
    // The result only says whether the callback stopped early.
    let _ = unsafe {
        EnumChildWindows(
            Some(frame),
            Some(find_uwp_app),
            LPARAM(&raw mut pid as isize),
        )
    };
    (pid != 0 && pid != frame_pid).then_some(pid)
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
            // Skip our own windows before anything asks for a title: for
            // them Windows sends a message to our UI thread, which may be
            // busy (or shutting down) and would block us.
            let mut pid = window_pid(hwnd);
            if pid == 0 || pid == own_pid || !is_app_window(hwnd) {
                continue;
            }
            if class_name(hwnd) == UWP_FRAME_CLASS {
                match uwp_app_pid(hwnd, pid) {
                    Some(app) => pid = app,
                    None => continue,
                }
            }
            if pid == own_pid || apps.iter().any(|a| a.pid == pid) {
                continue;
            }
            let title = window_title(hwnd);
            if title.trim().is_empty() {
                continue;
            }
            // Keep programs whose exe we can't read (e.g. protected) so they
            // can still be picked by title.
            let exe = exe_name(pid).unwrap_or_else(|| format!("Process {pid}"));
            // Capturing the frame host would capture nothing.
            if exe.eq_ignore_ascii_case(UWP_FRAME_HOST_EXE) {
                continue;
            }
            apps.push(AudioApp {
                pid,
                exe,
                title,
                hwnd: hwnd.0 as usize as u64,
            });
        }
    }
    apps
}

pub(crate) fn window_state(hwnd: u64) -> WindowState {
    let hwnd = HWND(hwnd as usize as *mut _);
    // SAFETY: plain queries on a window handle; one that is closed (or was
    // never valid) only makes them fail.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return WindowState::Gone;
        }
        if IsIconic(hwnd).as_bool() {
            return WindowState::Minimized;
        }
        let mut r = RECT::default();
        match GetClientRect(hwnd, &mut r) {
            Ok(()) => WindowState::Open {
                width: r.right.max(0) as u32,
                height: r.bottom.max(0) as u32,
            },
            Err(_) => WindowState::Gone,
        }
    }
}

/// Whether the process behind `process` runs elevated (as administrator).
fn is_elevated(process: HANDLE) -> Option<bool> {
    // SAFETY: the token handle is owned by OwnedHandle; the out-struct is a
    // correctly sized local.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let token = OwnedHandle(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .ok()?;
        Some(elevation.TokenIsElevated != 0)
    }
}

pub(crate) fn foreground_is_elevated() -> bool {
    static SELF_ELEVATED: OnceLock<bool> = OnceLock::new();
    // SAFETY: GetCurrentProcess is a pseudo-handle that needs no closing.
    let self_elevated =
        *SELF_ELEVATED.get_or_init(|| is_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false));
    if self_elevated {
        // Windows shows an elevated app's keys to other elevated apps.
        return false;
    }
    // SAFETY: plain queries; the process handle is owned by OwnedHandle.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let pid = window_pid(hwnd);
        if pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let process = OwnedHandle(process);
        is_elevated(process.0).unwrap_or(false)
    }
}

/// Windows build number (e.g. 19041), not affected by compatibility shims.
pub(crate) fn windows_build() -> Option<u32> {
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
    "The program you picked for game audio has closed. Start it again, then click Start sending game audio.";
const NO_ACCESS: &str =
    "Windows won't let GloorpFlorp listen to the program you picked for game audio. \
     Pick another program under \"Only one program\", or choose \"Everything except GloorpFlorp\", \
     then click Start sending game audio.";

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

/// Opens the picked program for exit watching. `Err` if it no longer runs;
/// `Ok(None)` if Windows won't let us watch it (e.g. a protected or
/// system process): capture is still tried, just without noticing its exit.
fn watch_process(pid: u32) -> Result<Option<OwnedHandle>, String> {
    // SAFETY: plain handle open; owned by OwnedHandle.
    match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(h) => Ok(Some(OwnedHandle(h))),
        // What OpenProcess says for a process id that isn't running.
        Err(e) if e.code() == E_INVALIDARG => {
            log::warn!("can't open process {pid}: {e}");
            Err(PROGRAM_CLOSED.to_string())
        }
        Err(e) => {
            log::warn!("can't watch process {pid} for exit ({e}); capturing without that");
            Ok(None)
        }
    }
}

fn wasapi_error(what: &str, e: wasapi::WasapiError) -> String {
    format!("Game-audio capture failed while {what} ({e}). Click Start sending game audio to try again, or restart GloorpFlorp.")
}

/// How often lost-sample counts are written to the log.
const GAP_REPORT_EVERY: Duration = Duration::from_secs(10);

fn capture_loop(
    source: GameAudioSource,
    mut on_samples: SampleCallback,
    stop: &AtomicBool,
    ready: &dyn Fn(),
) -> Result<(), String> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WasapiError, WaveFormat};

    // A game using every core must not starve this thread (lost samples).
    let _priority = crate::raise_audio_thread_priority();

    // include_tree=false is PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE.
    let (pid, include_tree) = match source {
        GameAudioSource::AllExceptThisApp => (std::process::id(), false),
        GameAudioSource::Process { pid } => (pid, true),
    };
    // Watch the chosen program so we can say when it has closed.
    let watched = match source {
        GameAudioSource::AllExceptThisApp => None,
        GameAudioSource::Process { pid } => watch_process(pid)?,
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
                // A program we may not even watch: most likely Windows refuses it too.
                _ if watched.is_none() && include_tree => {
                    log::warn!("process loopback for {pid} failed: {e}");
                    NO_ACCESS.to_string()
                }
                _ => wasapi_error("connecting to Windows audio", e),
            }
        })?;
    // Process loopback can't report a mix format; ask for ours and let
    // Windows convert (AUTOCONVERTPCM). A 100 ms buffer (Microsoft's sample
    // uses 20 ms): we still read every ~10 ms event, so it adds no delay,
    // but this thread can be held up that long without losing samples.
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 1_000_000,
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
    // Packets marked "data discontinuity": Windows dropped samples because we
    // didn't read in time. Logged as a summary, never per packet.
    let mut gaps = 0u32;
    let mut last_gap_report = Instant::now();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if gaps > 0 && last_gap_report.elapsed() >= GAP_REPORT_EVERY {
            log::debug!("game audio: samples lost {gaps} times in the last {GAP_REPORT_EVERY:?}");
            gaps = 0;
            last_gap_report = Instant::now();
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
                // (The flag is meaningless on the first packet after start.)
                if info.flags.data_discontinuity && info.index != 0 {
                    gaps += 1;
                }
                samples.clear();
                if info.flags.silent {
                    samples.resize(n * 2, 0.0);
                } else {
                    samples.extend(
                        bytes[..n * block_align]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|b| f32::from_le_bytes(*b)),
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
    if gaps > 0 {
        log::debug!("game audio: samples lost {gaps} times in the last few seconds");
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
        // shortcut: such a child is only stopped by its owner (Viewer,
        // Encoder), not by `close_children`; keep a list of them if this
        // warning ever shows up in a log.
        log::warn!(
            "couldn't tie child process {} to the app's lifetime: {e}",
            child.id()
        );
    }
}

pub(crate) fn close_children() {
    let Some(job) = app_job() else { return };
    // SAFETY: the job handle is valid for the life of the process.
    if let Err(e) = unsafe { TerminateJobObject(HANDLE(job as *mut _), 1) } {
        log::warn!("couldn't end the app's child processes: {e}");
    }
}

pub(crate) fn exit_now(code: i32) {
    // SAFETY: ends this process; on success the call doesn't return.
    let _ = unsafe { TerminateProcess(GetCurrentProcess(), code as u32) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotations() {
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION(0)), 0); // unspecified
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION(1)), 0); // identity
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_ROTATE90), 90);
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_ROTATE180), 180);
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_ROTATE270), 270);
    }

    #[test]
    fn program_closed_vs_not_allowed() {
        // No process can have this id (ids are multiples of 4 below 2^32 - 4).
        assert_eq!(
            watch_process(0xFFFF_FFFC).err().as_deref(),
            Some(PROGRAM_CLOSED)
        );
        // "System" (pid 4) always runs; a normal user may not open it, an
        // administrator may. Either way it is not reported as closed.
        assert!(watch_process(4).is_ok());
        assert!(watch_process(std::process::id()).unwrap().is_some());
    }

    #[test]
    fn messages_name_the_real_buttons() {
        for m in [PROGRAM_CLOSED, NO_ACCESS] {
            assert!(m.ends_with("click Start sending game audio."), "{m}");
            assert!(!m.contains("game-audio list"));
        }
        assert!(NO_ACCESS.contains("\"Only one program\""));
        assert!(NO_ACCESS.contains("\"Everything except GloorpFlorp\""));
        assert!(!NO_ACCESS.contains("  "), "{NO_ACCESS}");
    }
}
