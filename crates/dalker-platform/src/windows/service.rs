//! The `Dalker` Windows service.
//!
//! The service itself shows nothing and serves nothing. It runs as LocalSystem in
//! session 0 and keeps one agent process (`dalkerd` with the service's own
//! arguments) running *as SYSTEM inside the active console session*. That is the
//! only kind of process that can capture and control the sign-in screen, the lock
//! screen and UAC prompts (the `Winlogon` desktop) as well as the user's desktop,
//! including elevated windows.
//!
//! * The console session is polled; when it changes (user switch, logoff, RDP
//!   taking the console) the agent is stopped and started again in the new session.
//! * A crashed agent is restarted (with back-off).
//! * The agent lives in a job object that is killed when the service exits.
//! * Ctrl+Alt+Del: `SendSAS` only works from a service, so the agent signals
//!   [`SAS_EVENT`] and the service calls `SendSAS` (needs the
//!   `SoftwareSASGeneration` policy, which the installer sets).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenEventW, OpenProcessToken,
    ResumeThread, SetEvent, TerminateProcess, WaitForMultipleObjects, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EVENT_MODIFY_STATE, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Service name, as registered by the installer.
pub const SERVICE_NAME: &str = "Dalker";
/// Signalled by the agent to ask the service for Ctrl+Alt+Del.
const SAS_EVENT: PCWSTR = w!("Global\\DalkerSAS");
/// No interactive session attached to the console (e.g. during a session switch).
const NO_SESSION: u32 = 0xFFFF_FFFF;
const POLL: Duration = Duration::from_millis(500);

static CHILD_COMMAND: OnceLock<String> = OnceLock::new();
/// `SERVICE_STATUS_HANDLE` as an integer (raw pointers are not `Sync`).
static STATUS_HANDLE: OnceLock<usize> = OnceLock::new();
static STOP: AtomicBool = AtomicBool::new(false);
static STOP_EVENT: OnceLock<usize> = OnceLock::new();

/// Runs the service; returns when the SCM stops it. `child_command` is the full
/// command line of the agent to keep running in the console session.
///
/// Fails at once when the process was not started by the SCM.
pub fn run(child_command: String) -> Result<(), String> {
    let _ = CHILD_COMMAND.set(child_command);
    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .map_err(|e| format!("not started by the service control manager ({e}); install with the MSI"))
}

/// Asks the Dalker service to send Ctrl+Alt+Del (used by the agent it started).
pub fn request_secure_attention() -> Result<(), String> {
    unsafe {
        let ev = OpenEventW(EVENT_MODIFY_STATE, false, SAS_EVENT)
            .map_err(|e| format!("Ctrl+Alt+Del needs the Dalker service ({e})"))?;
        let r = SetEvent(ev).map_err(|e| format!("signal Ctrl+Alt+Del: {e}"));
        let _ = CloseHandle(ev);
        r
    }
}

fn set_status(state: SERVICE_STATUS_CURRENT_STATE, exit_code: u32) {
    let Some(handle) = STATUS_HANDLE.get() else { return };
    let accepted = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED { 0 } else { 10_000 },
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(*handle as *mut c_void), &status);
    }
}

unsafe extern "system" fn control_handler(control: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            set_status(SERVICE_STOP_PENDING, 0);
            if let Some(ev) = STOP_EVENT.get() {
                let _ = SetEvent(HANDLE(*ev as *mut c_void));
            }
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        // ERROR_CALL_NOT_IMPLEMENTED
        _ => 120,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    match RegisterServiceCtrlHandlerExW(PCWSTR(name.as_mut_ptr()), Some(control_handler), None) {
        Ok(h) => {
            let _ = STATUS_HANDLE.set(h.0 as usize);
        }
        Err(e) => {
            tracing::error!("RegisterServiceCtrlHandlerExW: {e}");
            return;
        }
    }
    set_status(SERVICE_START_PENDING, 0);
    let code = match supervise() {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("service failed: {e}");
            // ERROR_SERVICE_SPECIFIC_ERROR would need a code; a generic failure lets
            // the SCM recovery actions restart us.
            1
        }
    };
    set_status(SERVICE_STOPPED, code);
}

/// The agent process the service started.
struct Child {
    process: HANDLE,
    pid: u32,
    session: u32,
    _job: Job,
}

struct Job(HANDLE);

impl Drop for Job {
    fn drop(&mut self) {
        // Closing the last handle kills everything in the job (KILL_ON_JOB_CLOSE).
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Child {
    fn exit_code(&self) -> Option<u32> {
        unsafe {
            if WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            let _ = GetExitCodeProcess(self.process, &mut code);
            Some(code)
        }
    }

    fn kill(self) {
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = WaitForSingleObject(self.process, 5_000);
            let _ = CloseHandle(self.process);
        }
    }
}

fn supervise() -> Result<(), String> {
    let command = CHILD_COMMAND.get().cloned().ok_or("no agent command")?;
    let (stop_event, sas_event) = unsafe {
        let stop = CreateEventW(None, true, false, PCWSTR::null()).map_err(|e| format!("stop event: {e}"))?;
        // Default DACL of a SYSTEM token: only SYSTEM and administrators can signal it.
        let sas = CreateEventW(None, false, false, SAS_EVENT).map_err(|e| format!("Ctrl+Alt+Del event: {e}"))?;
        (stop, sas)
    };
    let _ = STOP_EVENT.set(stop_event.0 as usize);
    set_status(SERVICE_RUNNING, 0);
    tracing::info!(command = %command, "service running");

    let mut child: Option<Child> = None;
    let mut failures = 0u32;
    let mut next_start = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };

        if let Some(c) = &child {
            if let Some(code) = c.exit_code() {
                tracing::warn!(pid = c.pid, code, "agent exited");
                failures += 1;
                // 1 s, 2 s, 4 s ... up to 30 s between restarts of a crashing agent.
                next_start = Instant::now() + Duration::from_secs((1u64 << failures.min(5)).min(30));
                if let Some(c) = child.take() {
                    unsafe {
                        let _ = CloseHandle(c.process);
                    }
                }
            } else if c.session != session {
                tracing::info!(from = c.session, to = session, "console session changed; restarting agent");
                if let Some(c) = child.take() {
                    c.kill();
                }
                failures = 0;
                next_start = Instant::now();
            }
        }

        if child.is_none() && session != NO_SESSION && Instant::now() >= next_start {
            match unsafe { spawn_in_session(&command, session) } {
                Ok(c) => {
                    tracing::info!(pid = c.pid, session, "agent started");
                    child = Some(c);
                }
                Err(e) => {
                    tracing::error!(session, "cannot start agent: {e}");
                    next_start = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        // A child that has been up for a while is healthy again.
        if child.is_some() && failures > 0 && Instant::now() > next_start + Duration::from_secs(60) {
            failures = 0;
        }

        let woke = unsafe { WaitForMultipleObjects(&[stop_event, sas_event], false, POLL.as_millis() as u32) };
        if woke.0 == WAIT_OBJECT_0.0 + 1 {
            tracing::info!("sending Ctrl+Alt+Del");
            unsafe { SendSAS(false) };
        } else if woke != WAIT_TIMEOUT && woke != WAIT_OBJECT_0 {
            std::thread::sleep(POLL);
        }
    }

    tracing::info!("service stopping");
    if let Some(c) = child.take() {
        c.kill();
    }
    unsafe {
        let _ = CloseHandle(sas_event);
    }
    Ok(())
}

/// Starts `command` as SYSTEM in `session`, on its interactive window station.
unsafe fn spawn_in_session(command: &str, session: u32) -> Result<Child, String> {
    let mut own = HANDLE::default();
    OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).map_err(|e| format!("open token: {e}"))?;
    let mut token = HANDLE::default();
    let dup = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token);
    let _ = CloseHandle(own);
    dup.map_err(|e| format!("duplicate token: {e}"))?;

    let result = (|| {
        // Needs SeTcbPrivilege, which LocalSystem has.
        SetTokenInformation(token, TokenSessionId, (&session as *const u32).cast(), 4)
            .map_err(|e| format!("set token session: {e}"))?;

        let job = Job(CreateJobObjectW(None, PCWSTR::null()).map_err(|e| format!("create job: {e}"))?);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("configure job: {e}"))?;

        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut cmdline: Vec<u16> = command.encode_utf16().chain([0]).collect();
        let mut info = PROCESS_INFORMATION::default();
        // The environment (DALKER_LOG_DIR etc.) is inherited from the service.
        CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &startup,
            &mut info,
        )
        .map_err(|e| format!("CreateProcessAsUser: {e}"))?;
        if let Err(e) = AssignProcessToJobObject(job.0, info.hProcess) {
            let _ = TerminateProcess(info.hProcess, 1);
            let _ = CloseHandle(info.hThread);
            let _ = CloseHandle(info.hProcess);
            return Err(format!("assign job: {e}"));
        }
        ResumeThread(info.hThread);
        let _ = CloseHandle(info.hThread);
        Ok(Child { process: info.hProcess, pid: info.dwProcessId, session, _job: job })
    })();
    let _ = CloseHandle(token);
    result
}

/// Quotes one argument for a Windows command line (CommandLineToArgvW rules).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_commandline_to_argv_rules() {
        assert_eq!(quote_arg("--bind"), "--bind");
        assert_eq!(quote_arg(r"C:\Program Files\Dalker\dalkerd.exe"), r#""C:\Program Files\Dalker\dalkerd.exe""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }
}
