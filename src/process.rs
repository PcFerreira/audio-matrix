//! Process/window lifecycle helpers, always scoped to this user's instance.
use crate::{
    ipc,
    protocol::{Envelope, Reply, Request, VERSION},
};
pub fn rpc(instance: &str, request: Request) -> Result<Reply, String> {
    #[cfg(windows)]
    {
        let pipe = ipc::Instance::new(instance)?.connect()?;
        pipe.write(&Envelope {
            version: VERSION,
            request,
        })?;
        let reply: Reply = pipe.read()?;
        if reply.version != VERSION || reply.snapshot.version != VERSION {
            return Err("Engine protocol version mismatch".into());
        }
        Ok(reply)
    }
    #[cfg(not(windows))]
    {
        let _ = (instance, request);
        Err("Background engine requires Windows".into())
    }
}
fn spawn(arguments: &[String]) -> Result<(), String> {
    let mut command =
        std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn().map_err(|e| e.to_string())?;
    Ok(())
}
pub fn launch_console(instance: &str, test_mode: bool) -> Result<(), String> {
    let mut args = vec!["--instance".into(), instance.into()];
    if test_mode {
        args.push("--test-engine".into());
    }
    spawn(&args)
}
pub fn ensure_engine(instance: &str, test_mode: bool) -> Result<Reply, String> {
    ipc::validate_instance(instance)?;
    if let Ok(reply) = rpc(instance, Request::Status) {
        if reply.snapshot.test_mode != test_mode {
            return Err("Existing engine mode differs; use another instance".into());
        }
        return Ok(reply);
    }
    let mut args = vec!["--engine".into(), "--instance".into(), instance.into()];
    if test_mode {
        args.push("--test-engine".into());
    }
    spawn(&args)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(reply) = rpc(instance, Request::Status) {
            if reply.snapshot.test_mode != test_mode {
                return Err("Existing engine mode differs; use another instance".into());
            }
            return Ok(reply);
        }
        if std::time::Instant::now() >= deadline {
            return Err("Could not connect to background engine; no audio was started".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
pub fn window_belongs_to(window: u64, pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow},
        };
        let hwnd = HWND(window as usize as *mut _);
        let mut owner = 0;
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return false;
            }
            GetWindowThreadProcessId(hwnd, Some(&mut owner));
        }
        owner == pid
    }
    #[cfg(not(windows))]
    {
        let _ = (window, pid);
        false
    }
}
pub fn show_window(window: u64) {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            UI::WindowsAndMessaging::*,
        };
        let hwnd = HWND(window as usize as *mut _);
        unsafe {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            let _ = PostMessageW(Some(hwnd), WM_PAINT, WPARAM(0), LPARAM(0));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
}
pub fn wake_window(window: u64) {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            UI::WindowsAndMessaging::*,
        };
        if window != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(window as usize as *mut _)),
                    WM_PAINT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
}
