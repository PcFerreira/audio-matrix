//! Versioned, bounded local IPC. Windows pipe/mutex ACLs grant only this user
//! and SYSTEM access; names also include the Windows session and instance ID.
use serde::{Serialize, de::DeserializeOwned};
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
#[cfg(test)]
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err("IPC frame exceeds 8 MiB".into());
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend(body);
    Ok(frame)
}
pub fn frame_length(header: [u8; 4]) -> Result<usize, String> {
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("Invalid IPC frame length".into());
    }
    Ok(length)
}
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err("Invalid IPC frame length".into());
    }
    serde_json::from_slice(body).map_err(|e| format!("Invalid IPC payload: {e}"))
}
pub fn validate_instance(instance: &str) -> Result<(), String> {
    if instance.is_empty()
        || instance.len() > 48
        || !instance
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("Instance ID must be 1-48 ASCII letters, numbers, '-' or '_'".into());
    }
    Ok(())
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    use windows::{
        Win32::{
            Foundation::*,
            Security::{Authorization::*, *},
            Storage::FileSystem::*,
            System::{IO::*, Pipes::*, Threading::*},
        },
        core::{PCWSTR, PWSTR},
    };

    pub struct Owned(pub HANDLE);
    // SAFETY: kernel handles may be used across threads; lifetime is owned and
    // never closed while another thread uses it. Only pipe owner threads do I/O.
    unsafe impl Send for Owned {}
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    pub struct Identity {
        pub sid: String,
        pub session: u32,
    }
    pub fn identity() -> Result<Identity, String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|e| e.to_string())?;
            let token = Owned(token);
            let mut needed = 0;
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
            if needed == 0 || needed > 65536 {
                return Err("Could not read current user token".into());
            }
            // usize alignment satisfies TOKEN_USER/SID alignment on x64.
            let mut storage =
                vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .map_err(|e| e.to_string())?;
            let user = &*storage.as_ptr().cast::<TOKEN_USER>();
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid).map_err(|e| e.to_string())?;
            let result = sid.to_string().map_err(|e| e.to_string());
            let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
            let mut session = 0;
            windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
                GetCurrentProcessId(),
                &mut session,
            )
            .map_err(|e| e.to_string())?;
            Ok(Identity {
                sid: result?,
                session,
            })
        }
    }
    struct Security {
        descriptor: PSECURITY_DESCRIPTOR,
    }
    impl Security {
        fn new(sid: &str) -> Result<Self, String> {
            let string = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(string.as_ptr()),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(Self { descriptor })
        }
        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor.0,
                bInheritHandle: false.into(),
            }
        }
    }
    impl Drop for Security {
        fn drop(&mut self) {
            let _ = unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
        }
    }
    pub struct Instance {
        pub name: String,
        pub identity: Identity,
    }
    impl Instance {
        pub fn new(name: &str) -> Result<Self, String> {
            validate_instance(name)?;
            Ok(Self {
                name: name.into(),
                identity: identity()?,
            })
        }
        fn suffix(&self) -> String {
            format!(
                "{}-{}-{}",
                self.identity.sid, self.identity.session, self.name
            )
        }
        fn pipe_name(&self) -> Vec<u16> {
            wide(&format!(r"\\.\pipe\AudioMatrix-v2-{}", self.suffix()))
        }
        pub fn guard(&self, role: &str) -> Result<Option<Owned>, String> {
            let name = wide(&format!(r"Local\AudioMatrix-v2-{role}-{}", self.suffix()));
            let security = Security::new(&self.identity.sid)?;
            let attributes = security.attributes();
            unsafe {
                SetLastError(ERROR_SUCCESS);
                let handle = CreateMutexW(Some(&attributes), false, PCWSTR(name.as_ptr()))
                    .map_err(|e| e.to_string())?;
                let exists = GetLastError() == ERROR_ALREADY_EXISTS;
                let guard = Owned(handle);
                if exists { Ok(None) } else { Ok(Some(guard)) }
            }
        }
        pub fn connect(&self) -> Result<Pipe, String> {
            let name = self.pipe_name();
            let deadline = Instant::now() + Duration::from_secs(2);
            let handle = loop {
                let result = unsafe {
                    CreateFileW(
                        PCWSTR(name.as_ptr()),
                        GENERIC_READ.0 | GENERIC_WRITE.0,
                        FILE_SHARE_MODE::default(),
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED,
                        None,
                    )
                };
                match result {
                    Ok(handle) => break handle,
                    Err(error)
                        if error.code() == ERROR_PIPE_BUSY.to_hresult()
                            && Instant::now() < deadline =>
                    {
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 100) };
                    }
                    Err(error) => return Err(error.to_string()),
                }
            };
            Ok(Pipe {
                handle: Owned(handle),
            })
        }
        fn listener(&self) -> Result<Pipe, String> {
            let name = self.pipe_name();
            let security = Security::new(&self.identity.sid)?;
            let attributes = security.attributes();
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    4,
                    65536,
                    65536,
                    1000,
                    Some(&attributes),
                )
            };
            if handle.is_invalid() {
                return Err(windows::core::Error::from_thread().to_string());
            }
            Ok(Pipe {
                handle: Owned(handle),
            })
        }
    }
    pub struct Pipe {
        handle: Owned,
    }
    impl Pipe {
        fn operation(
            &self,
            operation: impl FnOnce(&mut OVERLAPPED, &mut u32) -> windows::core::Result<()>,
            timeout: Duration,
        ) -> Result<u32, String> {
            let event =
                Owned(unsafe { CreateEventW(None, true, false, None) }.map_err(|e| e.to_string())?);
            let mut pending = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let mut transferred = 0;
            match operation(&mut pending, &mut transferred) {
                Ok(()) => return Ok(transferred),
                Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => return Ok(0),
                Err(error) if error.code() != ERROR_IO_PENDING.to_hresult() => {
                    return Err(error.to_string());
                }
                Err(_) => {}
            }
            let result = unsafe {
                GetOverlappedResultEx(
                    self.handle.0,
                    &pending,
                    &mut transferred,
                    timeout.as_millis().min(u32::MAX as u128) as u32,
                    false,
                )
            };
            if let Err(error) = result {
                // Cancellation must complete BEFORE the buffer/OVERLAPPED/event
                // go out of scope, even when an I/O deadline expires.
                unsafe {
                    let _ = CancelIoEx(self.handle.0, Some(&pending));
                    let _ = GetOverlappedResult(self.handle.0, &pending, &mut transferred, true);
                }
                return Err(error.to_string());
            }
            Ok(transferred)
        }
        fn accept(&self) -> Result<u32, String> {
            self.operation(
                |overlapped, _| unsafe { ConnectNamedPipe(self.handle.0, Some(overlapped)) },
                Duration::from_millis(200),
            )?;
            let mut pid = 0;
            unsafe {
                GetNamedPipeClientProcessId(self.handle.0, &mut pid).map_err(|e| e.to_string())?;
            }
            Ok(pid)
        }
        fn read_exact(&self, mut bytes: &mut [u8], deadline: Instant) -> Result<(), String> {
            while !bytes.is_empty() {
                let timeout = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or("IPC read timed out")?;
                let count = self.operation(
                    |overlapped, transferred| unsafe {
                        ReadFile(
                            self.handle.0,
                            Some(bytes),
                            Some(transferred),
                            Some(overlapped),
                        )
                    },
                    timeout,
                )? as usize;
                if count == 0 {
                    return Err("IPC connection closed".into());
                }
                bytes = &mut bytes[count..];
            }
            Ok(())
        }
        fn read_raw(&self) -> Result<Vec<u8>, String> {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut header = [0; 4];
            self.read_exact(&mut header, deadline)?;
            let mut body = vec![0; frame_length(header)?];
            self.read_exact(&mut body, deadline)?;
            Ok(body)
        }
        pub fn read<T: DeserializeOwned>(&self) -> Result<T, String> {
            let value = decode(&self.read_raw()?)?;
            // DisconnectNamedPipe discards unread data. An explicit bounded
            // acknowledgement keeps the reply alive until the client read it;
            // unlike FlushFileBuffers it cannot stall on a non-reading client.
            self.write_bytes(&[0xA5])?;
            Ok(value)
        }
        pub fn write<T: Serialize>(&self, value: &T) -> Result<(), String> {
            let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
            self.write_raw(&body)
        }
        fn write_raw(&self, body: &[u8]) -> Result<(), String> {
            if body.is_empty() || body.len() > MAX_FRAME {
                return Err("Invalid IPC frame length".into());
            }
            let mut bytes = Vec::with_capacity(body.len() + 4);
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(body);
            self.write_bytes(&bytes)
        }
        fn write_bytes(&self, mut bytes: &[u8]) -> Result<(), String> {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !bytes.is_empty() {
                let timeout = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or("IPC write timed out")?;
                let count = self.operation(
                    |overlapped, transferred| unsafe {
                        WriteFile(
                            self.handle.0,
                            Some(bytes),
                            Some(transferred),
                            Some(overlapped),
                        )
                    },
                    timeout,
                )? as usize;
                if count == 0 {
                    return Err("IPC connection closed".into());
                }
                bytes = &bytes[count..];
            }
            Ok(())
        }
    }
    pub struct Listener {
        stop: Arc<AtomicBool>,
        workers: Vec<std::thread::JoinHandle<()>>,
    }
    impl Listener {
        pub fn start(
            instance: &str,
            handler: impl Fn(u32, Vec<u8>) -> Result<Vec<u8>, String> + Send + Sync + 'static,
        ) -> Result<Self, String> {
            let handler = Arc::new(handler);
            let stop = Arc::new(AtomicBool::new(false));
            let mut listener = Self {
                stop: stop.clone(),
                workers: Vec::new(),
            };
            for _ in 0..4 {
                let pipe = Instance::new(instance)?.listener()?;
                let stopped = stop.clone();
                let handler = handler.clone();
                listener.workers.push(
                    std::thread::Builder::new()
                        .name("engine-ipc".into())
                        .spawn(move || {
                            while !stopped.load(Ordering::Acquire) {
                                if let Ok(pid) = pipe.accept() {
                                    let result: Result<(), String> = (|| {
                                        let request = pipe.read_raw()?;
                                        let response = handler(pid, request)?;
                                        pipe.write_raw(&response)?;
                                        let mut ack = [0];
                                        pipe.read_exact(
                                            &mut ack,
                                            Instant::now() + Duration::from_secs(2),
                                        )?;
                                        if ack != [0xA5] {
                                            return Err("Invalid IPC acknowledgement".into());
                                        }
                                        Ok(())
                                    })(
                                    );
                                    if let Err(error) = result {
                                        eprintln!("Local engine IPC: {error}");
                                    }
                                    unsafe {
                                        let _ = DisconnectNamedPipe(pipe.handle.0);
                                    }
                                }
                            }
                        })
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok(listener)
        }
    }
    impl Drop for Listener {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        #[ignore = "creates isolated per-user pipe/mutex handles; no audio or persistent writes"]
        fn native_ipc_mutex_and_pipe_round_trips_are_scoped_and_reusable() {
            let name = format!("native-ipc-{}", std::process::id());
            let identity = Instance::new(&name).unwrap();
            let guard = identity.guard("engine").unwrap().unwrap();
            assert!(identity.guard("engine").unwrap().is_none());
            assert!(identity.guard("console").unwrap().is_some());
            drop(guard);
            let _guard = identity.guard("engine").unwrap().unwrap();
            let server = Listener::start(&name, |peer, bytes| {
                assert_eq!(peer, std::process::id());
                let value: String = decode(&bytes)?;
                serde_json::to_vec(&value).map_err(|e| e.to_string())
            })
            .unwrap();
            let value = "large frame ".repeat(20_000);
            for _ in 0..20 {
                let pipe = identity.connect().unwrap();
                pipe.write(&value).unwrap();
                // Reproduce a slow client: server must not discard its reply.
                std::thread::sleep(Duration::from_millis(10));
                assert_eq!(pipe.read::<String>().unwrap(), value);
            }
            // A dropped/malformed client cannot poison subsequent connections.
            let pipe = identity.connect().unwrap();
            pipe.write(&123_u32).unwrap();
            assert!(pipe.read::<String>().is_err());
            drop(pipe);
            let pipe = identity.connect().unwrap();
            pipe.write(&value).unwrap();
            assert_eq!(pipe.read::<String>().unwrap(), value);
            drop(pipe);
            drop(server);
            assert!(identity.connect().is_err());
        }
    }
}
#[cfg(windows)]
pub use native::{Instance, Listener};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipc_bounds_reject_empty_oversized_and_truncated_frames() {
        assert!(frame_length([0; 4]).is_err());
        assert!(frame_length(((MAX_FRAME + 1) as u32).to_le_bytes()).is_err());
        assert!(decode::<Vec<u8>>(b"[").is_err());
        let encoded = encode(&vec![1_u8, 2, 3]).unwrap();
        assert_eq!(
            frame_length(encoded[..4].try_into().unwrap()).unwrap(),
            encoded.len() - 4
        );
        assert_eq!(decode::<Vec<u8>>(&encoded[4..]).unwrap(), vec![1, 2, 3]);
        assert!(validate_instance("../default").is_err());
        assert!(validate_instance("test-123").is_ok());
    }
}
