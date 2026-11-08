//! `dalkerd.com`: the console front end of `dalkerd.exe` on Windows.
//!
//! A release `dalkerd.exe` is a GUI-subsystem program, so `cmd` and PowerShell don't wait for
//! it: a prompt such as `set-password` would read the console at the same time as the shell.
//! This console-subsystem launcher is what a terminal runs for `dalkerd ...` (`.com` comes
//! before `.exe` in `PATHEXT`): it starts `dalkerd.exe` from its own folder with the same
//! arguments and standard handles, waits for it and returns its exit code, so the shell waits
//! too. Built as `dalkerd-console.exe`; the installer ships it as `dalkerd.com`.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p.with_file_name("dalkerd.exe"),
        Err(e) => {
            eprintln!("dalkerd: can't find dalkerd.exe: {e}");
            return ExitCode::FAILURE;
        }
    };
    dalker_platform::outlive_console_ctrl_c();
    // Standard handles are inherited explicitly, so the GUI-subsystem child reads and writes
    // this console (or the pipes this launcher was given).
    let status = Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env(dalker_platform::CONSOLE_LAUNCHER_ENV, "1")
        .status();
    match status {
        Ok(s) => match s.code() {
            Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            None => ExitCode::FAILURE,
        },
        Err(e) => {
            eprintln!("dalkerd: can't start {}: {e}", exe.display());
            ExitCode::FAILURE
        }
    }
}
