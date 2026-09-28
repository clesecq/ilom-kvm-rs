//! One ILOM console session shared by every VNC client.
//!
//! The session starts with the first client and stops once no client has
//! been connected for the idle timeout: the ILOM has only a few session
//! slots, and each start costs a web login.

use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, Weak, mpsc},
    thread,
    time::{Duration, Instant},
};

use ilom_kvm_core::session::{
    ConnectionState, Source, ViewerCommand, ViewerHandle, ViewerShared, spawn_viewer,
};
use tracing::info;

pub struct Hub {
    source: Source,
    idle_timeout: Duration,
    inner: Mutex<Inner>,
    /// Bumped on every session or client event; waiters sleep on `wake`.
    signal: Mutex<u64>,
    wake: Condvar,
}

#[derive(Default)]
struct Inner {
    session: Option<ViewerHandle>,
    clients: usize,
    /// Bumped on every detach, so a stale idle timer does nothing.
    idle_epoch: u64,
    /// Final error (rejected credentials, changed certificate, spent JNLP):
    /// retrying cannot help, so later clients get this message.
    failed: Option<String>,
}

/// How long a connected session may show no picture before clients get a
/// black screen: a powered-off host sends no frames at all.
const BLANK_GRACE: Duration = Duration::from_secs(3);

impl Hub {
    pub fn new(source: Source, idle_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            source,
            idle_timeout,
            inner: Mutex::new(Inner::default()),
            signal: Mutex::new(0),
            wake: Condvar::new(),
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Wakes every waiter.
    pub fn notify(&self) {
        let mut signal = self
            .signal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *signal = signal.wrapping_add(1);
        self.wake.notify_all();
    }

    /// Calls `check` after every event until it returns `Some`, or until
    /// `timeout` passes. `check` must not call [`Hub::notify`].
    pub fn wait_for<T>(
        &self,
        timeout: Option<Duration>,
        mut check: impl FnMut() -> Option<T>,
    ) -> Option<T> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut signal = self
            .signal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(value) = check() {
                return Some(value);
            }
            let slice = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    left.min(Duration::from_millis(500))
                }
                None => Duration::from_millis(500),
            };
            signal = self
                .wake
                .wait_timeout(signal, slice)
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
    }

    /// Registers a client, starting the session if needed.
    pub fn attach(self: &Arc<Self>) -> Result<Attachment, String> {
        let mut inner = self.inner();
        if let Some(message) = &inner.failed {
            return Err(message.clone());
        }
        let running = inner
            .session
            .as_ref()
            .is_some_and(|session| !matches!(state(&session.shared), ConnectionState::Stopped));
        let stale = if running { None } else { inner.session.take() };
        if !running {
            info!("starting the ILOM session");
            let hub = Arc::downgrade(self);
            inner.session = Some(spawn_viewer(self.source.clone(), move || {
                if let Some(hub) = Weak::upgrade(&hub) {
                    hub.notify();
                }
            }));
        }
        inner.clients += 1;
        let session = inner.session.as_ref().expect("session just started");
        let attachment = Attachment {
            hub: self.clone(),
            shared: session.shared.clone(),
            commands: session.commands.clone(),
        };
        drop(inner);
        // Stopping joins session threads, which call `notify`; never hold a
        // lock across it.
        drop(stale);
        Ok(attachment)
    }

    fn detach(self: &Arc<Self>) {
        let mut inner = self.inner();
        inner.clients -= 1;
        if inner.clients > 0 {
            return;
        }
        inner.idle_epoch += 1;
        if self.idle_timeout.is_zero() {
            let session = inner.session.take();
            drop(inner);
            stop(session);
            return;
        }
        let epoch = inner.idle_epoch;
        drop(inner);
        let hub = self.clone();
        let spawned = thread::Builder::new()
            .name("ilom-vnc-idle".into())
            .spawn(move || {
                thread::sleep(hub.idle_timeout);
                let session = {
                    let mut inner = hub.inner();
                    if inner.clients > 0 || inner.idle_epoch != epoch {
                        return;
                    }
                    inner.session.take()
                };
                stop(session);
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the idle timer");
        }
    }

    fn fail(&self, message: &str) {
        let session = {
            let mut inner = self.inner();
            inner.failed.get_or_insert_with(|| message.to_string());
            inner.session.take()
        };
        stop(session);
    }
}

fn stop(session: Option<ViewerHandle>) {
    if let Some(mut session) = session {
        info!("no VNC client left, closing the ILOM session");
        session.stop_and_wait();
    }
}

fn state(shared: &ViewerShared) -> ConnectionState {
    shared
        .status
        .lock()
        .map_or(ConnectionState::Error, |status| status.state)
}

/// A connected client's handle on the session.
pub struct Attachment {
    hub: Arc<Hub>,
    pub shared: Arc<ViewerShared>,
    pub commands: mpsc::Sender<ViewerCommand>,
}

impl Attachment {
    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    pub fn send(&self, command: ViewerCommand) {
        let _ = self.commands.send(command);
    }

    /// The session's final error, if it ended for good. The hub remembers
    /// it and refuses later clients with the same message.
    pub fn failure(&self) -> Option<String> {
        let status = self.shared.status.lock().ok()?;
        match status.state {
            ConnectionState::Rejected { .. } | ConnectionState::Error => {
                let message = status.message.clone();
                drop(status);
                self.hub.fail(&message);
                Some(message)
            }
            _ => None,
        }
    }

    /// The session ended for good. Unlike [`Attachment::failure`], safe to
    /// call inside [`Hub::wait_for`].
    pub fn is_final(&self) -> bool {
        matches!(
            state(&self.shared),
            ConnectionState::Rejected { .. } | ConnectionState::Error
        )
    }

    /// Waits until clients can be shown something: a first frame, or a
    /// connected session that stays blank for a moment.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let mut connected_since = None;
        let ready = self.hub.wait_for(Some(timeout), || {
            if self
                .shared
                .latest_frame
                .lock()
                .is_ok_and(|frame| frame.is_some())
            {
                return Some(Ok(()));
            }
            // `failure` would stop the session, whose threads call `notify`
            // while `wait_for` holds its lock.
            if self.is_final() {
                return Some(Err(()));
            }
            match state(&self.shared) {
                ConnectionState::Connected => {
                    let since = *connected_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= BLANK_GRACE {
                        return Some(Ok(()));
                    }
                }
                _ => connected_since = None,
            }
            None
        });
        match ready {
            Some(Ok(())) => Ok(()),
            Some(Err(())) => Err(self
                .failure()
                .unwrap_or_else(|| "the ILOM session failed".into())),
            None => Err("timed out waiting for the ILOM console".into()),
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.hub.detach();
    }
}
