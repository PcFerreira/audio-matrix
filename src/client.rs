//! UI-side transport. No COM/audio handles; dropping this client detaches only.
use crate::{
    audio::OutputStats,
    process,
    protocol::{Reply, Request, Snapshot},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;
enum Work {
    Request(Request),
    Finish {
        update: Box<crate::protocol::Update>,
        shutdown: bool,
        done: mpsc::SyncSender<()>,
    },
}
#[derive(Default)]
struct Mailbox {
    latest: Option<Result<Reply, String>>,
    errors: std::collections::VecDeque<String>,
}
pub struct Client {
    mailbox: Arc<Mutex<Mailbox>>,
    commands: mpsc::SyncSender<Work>,
    pending: Arc<AtomicUsize>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Client {
    pub fn connect(
        instance: &str,
        test_mode: bool,
        ctx: eframe::egui::Context,
        window: u64,
    ) -> Result<(Self, Reply), String> {
        let initial = process::ensure_engine(instance, test_mode)?;
        if window != 0 {
            let reply = process::rpc(instance, Request::AttachUi { window })?;
            if let Some(error) = reply.error {
                return Err(error);
            }
        }
        let (commands, rx) = mpsc::sync_channel::<Work>(64);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let received = mailbox.clone();
        let pending = Arc::new(AtomicUsize::new(0));
        let queued = pending.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(true));
        let connection = connected.clone();
        let stopped = stop.clone();
        let name = instance.to_owned();
        let worker = std::thread::Builder::new()
            .name("console-ipc".into())
            .spawn(move || {
                loop {
                    let request = match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(Work::Request(request)) => Some(request),
                        Ok(Work::Finish {
                            update,
                            shutdown,
                            done,
                        }) => {
                            // Ordered after all queued edits; no parallel final
                            // RPC can be overwritten by an older queued edit.
                            let _ = process::rpc(&name, Request::Configure(update));
                            let _ = process::rpc(
                                &name,
                                if shutdown {
                                    Request::Shutdown
                                } else {
                                    Request::DetachUi
                                },
                            );
                            let _ = done.send(());
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout)
                            if !stopped.load(Ordering::Acquire) =>
                        {
                            None
                        }
                        Err(_) => break,
                    };
                    let result = process::rpc(&name, request.clone().unwrap_or(Request::Status));
                    if request.is_some() {
                        queued.fetch_sub(1, Ordering::AcqRel);
                    }
                    let failed = result.is_err();
                    {
                        let mut mailbox = received.lock().unwrap_or_else(|e| e.into_inner());
                        if let Ok(reply) = &result
                            && let Some(error) = &reply.error
                        {
                            if mailbox.errors.len() == 16 {
                                mailbox.errors.pop_front();
                            }
                            mailbox.errors.push_back(error.clone());
                        }
                        mailbox.latest = Some(result);
                    }
                    ctx.request_repaint();
                    process::wake_window(window);
                    if failed {
                        connection.store(false, Ordering::Release);
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok((
            Self {
                mailbox,
                commands,
                pending,
                connected,
                stop,
                worker: Some(worker),
            },
            initial,
        ))
    }
    pub fn send(&self, request: Request) -> Result<(), String> {
        if !self.is_connected() {
            return Err("Engine control connection is closed; reconnect from App".into());
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.commands.try_send(Work::Request(request)).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err("Engine command queue unavailable/full".into());
        }
        Ok(())
    }
    pub fn idle(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
    pub fn drain(&self) -> Vec<Result<Reply, String>> {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        let mut results: Vec<_> = mailbox.latest.take().into_iter().collect();
        // Keep bounded command errors even if newer status snapshots coalesce.
        results.extend(mailbox.errors.drain(..).map(Err));
        results
    }
    pub fn finish(&self, update: crate::protocol::Update, shutdown: bool) {
        let (done, completed) = mpsc::sync_channel(1);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut work = Work::Finish {
            update: Box::new(update),
            shutdown,
            done,
        };
        loop {
            match self.commands.try_send(work) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) if std::time::Instant::now() < deadline => {
                    work = returned;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
        let _ =
            completed.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    }
    pub fn realign(&self) {
        let _ = self.send(Request::ReAlign);
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take().filter(|w| w.is_finished()) {
            let _ = worker.join();
        }
    }
}
pub struct Handle {
    pub device_id: String,
    pub stats: Arc<Mutex<OutputStats>>,
    ready: bool,
}
impl Handle {
    pub fn is_ready(&self) -> bool {
        self.ready
    }
}
pub struct Engine {
    pub outputs: Vec<Handle>,
    live: bool,
    main_clock_id: Option<String>,
}
impl Engine {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            outputs: snapshot
                .outputs
                .iter()
                .map(|o| Handle {
                    device_id: o.device_id.clone(),
                    stats: Arc::new(Mutex::new(o.stats.clone())),
                    ready: o.ready,
                })
                .collect(),
            live: snapshot.live,
            main_clock_id: snapshot.main_clock_id.clone(),
        }
    }
    pub fn is_live(&self) -> bool {
        self.live
    }
    pub fn main_clock_device_id(&self) -> Option<&str> {
        self.main_clock_id.as_deref()
    }
}
