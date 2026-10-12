//! Native notification-area owner. Never touches audio workers or user presets.
use std::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Show,
    Mute,
    Louder,
    Quieter,
    ToggleRouting,
    Exit,
    Suspend,
    Resume,
}

pub struct Tray {
    pub events: mpsc::Receiver<Action>,
    #[cfg(windows)]
    window: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(windows)]
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Tray {
    pub fn start(ctx: eframe::egui::Context, native_window: usize) -> Result<Self, String> {
        #[cfg(windows)]
        {
            native::start(ctx, native_window)
        }
        #[cfg(not(windows))]
        {
            let _ = (ctx, native_window);
            Err("System tray requires Windows".into())
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            let window = self.window.load(std::sync::atomic::Ordering::Acquire);
            if window != 0 {
                // SAFETY: only our owner thread destroys this window after receiving close.
                let _ = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(windows::Win32::Foundation::HWND(window as *mut _)),
                        windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    )
                };
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

pub fn set_startup(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::{Win32::System::Registry::*, core::w};
        // Only this application's per-user Run value is changed, on an explicit UI toggle.
        unsafe {
            let mut key = HKEY::default();
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                Some(0),
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
            .ok()
            .map_err(|e| e.to_string())?;
            let result = if enabled {
                let executable = std::env::current_exe().map_err(|e| e.to_string());
                match executable {
                    Ok(path) => {
                        let value: Vec<u16> = format!("\"{}\"", path.display())
                            .encode_utf16()
                            .chain(Some(0))
                            .collect();
                        let bytes = std::slice::from_raw_parts(
                            value.as_ptr().cast::<u8>(),
                            value.len() * 2,
                        );
                        RegSetValueExW(key, w!("AudioMatrix"), Some(0), REG_SZ, Some(bytes))
                            .ok()
                            .map_err(|e| e.to_string())
                    }
                    Err(error) => Err(error),
                }
            } else {
                let result = RegDeleteValueW(key, w!("AudioMatrix"));
                if result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                    Ok(())
                } else {
                    result.ok().map_err(|e| e.to_string())
                }
            };
            let _ = RegCloseKey(key);
            result
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("Startup registration requires Windows".into())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        cell::RefCell,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use windows::{
        Win32::{
            Foundation::*,
            System::LibraryLoader::GetModuleHandleW,
            UI::{Shell::*, WindowsAndMessaging::*},
        },
        core::w,
    };
    const CALLBACK: u32 = WM_APP + 1;
    struct State {
        owner: HWND,
        tx: mpsc::Sender<Action>,
        ctx: eframe::egui::Context,
        icon: NOTIFYICONDATAW,
        taskbar_created: u32,
    }
    thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }

    fn deliver(state: &State, action: Action) {
        let _ = state.tx.send(action);
        state.ctx.request_repaint();
        // Windows may suppress ordinary redraw requests for hidden windows.
        // Wake our own HWND explicitly so tray controls are processed even
        // while invisible; Show also restores a minimized window natively.
        if !state.owner.is_invalid() {
            unsafe {
                if action == Action::Show {
                    let _ = ShowWindowAsync(state.owner, SW_RESTORE);
                }
                let _ = PostMessageW(Some(state.owner), WM_PAINT, WPARAM(0), LPARAM(0));
            }
        }
    }

    unsafe extern "system" fn procedure(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        // SAFETY: called by Windows on the creating thread; every pointer passed
        // to Win32 below points to live thread-owned structures or OS handles.
        unsafe {
            if message == WM_CLOSE {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            if message == WM_DESTROY {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            STATE.with(|slot| {
                let state = slot.borrow();
                let Some(state) = state.as_ref() else {
                    return;
                };
                if message == WM_POWERBROADCAST {
                    match wp.0 {
                        4 => deliver(state, Action::Suspend),
                        7 | 18 => deliver(state, Action::Resume),
                        _ => {}
                    }
                    return;
                }
                if message == WM_ENDSESSION && wp.0 != 0 {
                    deliver(state, Action::Exit);
                    return;
                }
                if message == WM_TIMER
                    && !state.owner.is_invalid()
                    && (!IsWindowVisible(state.owner).as_bool() || IsIconic(state.owner).as_bool())
                {
                    // Keep endpoint retries, engine events and checkpoints alive
                    // when Windows suppresses invisible-window redraw requests.
                    let _ = PostMessageW(Some(state.owner), WM_PAINT, WPARAM(0), LPARAM(0));
                    return;
                }
                if message == state.taskbar_created {
                    let _ = Shell_NotifyIconW(NIM_ADD, &state.icon);
                    let _ = Shell_NotifyIconW(NIM_SETVERSION, &state.icon);
                }
                if message == WM_COMMAND {
                    let action = match wp.0 & 0xffff {
                        1 => Some(Action::Show),
                        2 => Some(Action::Mute),
                        3 => Some(Action::Louder),
                        4 => Some(Action::Quieter),
                        5 => Some(Action::ToggleRouting),
                        6 => Some(Action::Exit),
                        _ => None,
                    };
                    if let Some(action) = action {
                        deliver(state, action);
                    }
                    return;
                }
                if message != CALLBACK {
                    return;
                }
                let event = lp.0 as u32 & 0xffff;
                let action =
                    if event == WM_LBUTTONUP || event == NIN_SELECT || event == NIN_SELECT + 1 {
                        Some(Action::Show)
                    } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                        let menu = match CreatePopupMenu() {
                            Ok(m) => m,
                            Err(_) => return,
                        };
                        for (id, label) in [
                            (1, w!("Show Audio Matrix")),
                            (2, w!("Toggle mute all")),
                            (3, w!("Volume +1 dB")),
                            (4, w!("Volume -1 dB")),
                            (5, w!("Start / stop routing")),
                            (6, w!("Exit (stop audio)")),
                        ] {
                            let _ = AppendMenuW(menu, MF_STRING, id, label);
                        }
                        let mut cursor = POINT::default();
                        let _ = GetCursorPos(&mut cursor);
                        let _ = SetForegroundWindow(hwnd);
                        let command = TrackPopupMenu(
                            menu,
                            TPM_RETURNCMD | TPM_NONOTIFY,
                            cursor.x,
                            cursor.y,
                            Some(0),
                            hwnd,
                            None,
                        )
                        .0;
                        let _ = DestroyMenu(menu);
                        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                        match command {
                            1 => Some(Action::Show),
                            2 => Some(Action::Mute),
                            3 => Some(Action::Louder),
                            4 => Some(Action::Quieter),
                            5 => Some(Action::ToggleRouting),
                            6 => Some(Action::Exit),
                            _ => None,
                        }
                    } else {
                        None
                    };
                if let Some(action) = action {
                    deliver(state, action);
                }
            });
            if message == WM_POWERBROADCAST {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, message, wp, lp)
            }
        }
    }
    pub(super) fn start(ctx: eframe::egui::Context, native_window: usize) -> Result<Tray, String> {
        let (tx, events) = mpsc::channel();
        let (ready, result) = mpsc::sync_channel(1);
        let window = Arc::new(AtomicUsize::new(0));
        let address = window.clone();
        let worker = std::thread::Builder::new()
            .name("notification-area".into())
            .spawn(move || {
                let run = || -> windows::core::Result<()> {
                    // SAFETY: hidden message window/icon/menu live and are destroyed
                    // on this thread. The shared address is only used for PostMessage.
                    unsafe {
                        let instance = GetModuleHandleW(None)?;
                        let class = WNDCLASSW {
                            lpfnWndProc: Some(procedure),
                            hInstance: instance.into(),
                            lpszClassName: w!("AudioMatrixTrayOwner"),
                            ..Default::default()
                        };
                        RegisterClassW(&class);
                        let hwnd = CreateWindowExW(
                            WINDOW_EX_STYLE::default(),
                            class.lpszClassName,
                            w!("Audio Matrix tray"),
                            WINDOW_STYLE::default(),
                            0,
                            0,
                            0,
                            0,
                            None,
                            None,
                            Some(instance.into()),
                            None,
                        )?;
                        let mut icon = NOTIFYICONDATAW {
                            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                            hWnd: hwnd,
                            uID: 1,
                            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                            uCallbackMessage: CALLBACK,
                            hIcon: LoadIconW(None, IDI_APPLICATION)?,
                            ..Default::default()
                        };
                        for (dst, src) in icon
                            .szTip
                            .iter_mut()
                            .zip("Audio Matrix - routing console".encode_utf16())
                        {
                            *dst = src;
                        }
                        if !Shell_NotifyIconW(NIM_ADD, &icon).as_bool() {
                            let _ = DestroyWindow(hwnd);
                            return Err(windows::core::Error::from_thread());
                        }
                        // Version 3 retains ordinary mouse callback semantics.
                        icon.Anonymous.uVersion = 3;
                        let _ = Shell_NotifyIconW(NIM_SETVERSION, &icon);
                        STATE.with(|slot| {
                            *slot.borrow_mut() = Some(State {
                                owner: HWND(native_window as *mut _),
                                tx,
                                ctx,
                                icon,
                                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                            })
                        });
                        address.store(hwnd.0 as usize, Ordering::Release);
                        SetTimer(Some(hwnd), 1, 250, None);
                        let _ = ready.send(Ok(()));
                        let mut message = MSG::default();
                        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                        STATE.with(|slot| {
                            if let Some(state) = slot.borrow_mut().take() {
                                let _ = Shell_NotifyIconW(NIM_DELETE, &state.icon);
                            }
                        });
                        address.store(0, Ordering::Release);
                        Ok(())
                    }
                };
                if let Err(error) = run() {
                    let _ = ready.send(Err(error.to_string()));
                }
            })
            .map_err(|e| e.to_string())?;
        match result.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Tray {
                events,
                window,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        #[ignore = "requires a Windows desktop; creates only a temporary tray icon, no audio/registry changes"]
        fn native_tray_callback_and_cleanup() {
            let tray = start(eframe::egui::Context::default(), 0).unwrap();
            let window = tray.window.clone();
            let hwnd = HWND(window.load(Ordering::Acquire) as *mut _);
            unsafe {
                PostMessageW(
                    Some(hwnd),
                    CALLBACK,
                    WPARAM(1),
                    LPARAM(WM_LBUTTONUP as isize),
                )
                .unwrap();
            }
            assert_eq!(
                tray.events
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                Action::Show
            );
            for (event, expected) in [
                (4, Action::Suspend),
                (18, Action::Resume),
                (7, Action::Resume),
            ] {
                unsafe {
                    PostMessageW(Some(hwnd), WM_POWERBROADCAST, WPARAM(event), LPARAM(0)).unwrap();
                }
                assert_eq!(
                    tray.events
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap(),
                    expected
                );
            }
            drop(tray);
            assert_eq!(window.load(Ordering::Acquire), 0);
            assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        }
    }
}
