//! Bounded host ownership of external agent processes, transports and workers.
//!
//! A slot is reserved, and its cleanup thread started, before an agent process
//! is spawned. Marking a slot terminal only stores atomics and sends on an
//! unbounded channel, so it never waits for transport I/O, process exit or a
//! worker. The cleanup thread keeps the host capacity reserved until every
//! registered resource has been released. A child whose cleanup fails is
//! retained and retried. A child whose identity was lost moves to the reaper's
//! quarantine and keeps its slot reserved permanently. Cleanup can still stall
//! indefinitely if the operating system never lets the leader exit.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kage_acp::connection::UnixShutdown;
use kage_core::reaper::ProtectedChild;
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;

const RETRY_MIN: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Host-wide capacity for external sessions, including ones still cleaning up.
pub(crate) struct Lifecycles {
    live: Arc<AtomicUsize>,
    limit: usize,
}

impl Lifecycles {
    /// Capacity for at most `limit` slots at once.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            live: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    /// Reserve a slot and start its cleanup thread before anything is spawned.
    pub(crate) fn reserve(&self) -> Result<Startup, RpcError> {
        if self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.limit).then_some(live + 1)
            })
            .is_err()
        {
            return Err(RpcError::internal("too many external sessions"));
        }
        let (wake, wakes) = mpsc::channel();
        let slot = Arc::new(Slot {
            terminal: AtomicBool::new(false),
            wake,
            transport: OnceLock::new(),
            owned: Mutex::new(Owned::default()),
            workers: AtomicUsize::new(0),
            #[cfg(test)]
            test: hooks::Hooks::default(),
        });
        let owner = Arc::clone(&slot);
        let live = Arc::clone(&self.live);
        if let Err(error) = thread::Builder::new()
            .name("kage-external-lifecycle".into())
            .spawn(move || run(&owner, &wakes, &live))
        {
            self.live.fetch_sub(1, Ordering::AcqRel);
            return Err(RpcError::internal(format!(
                "cannot start external session cleanup: {error}"
            )));
        }
        Ok(Startup {
            slot,
            committed: false,
        })
    }

    /// Slots reserved now, including those still cleaning up.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
}

struct Transport {
    shutdown: UnixShutdown,
    reader_stopped: Arc<AtomicBool>,
}

impl Transport {
    fn signal(&self) {
        self.reader_stopped.store(true, Ordering::SeqCst);
        self.shutdown.signal_shutdown();
    }
}

#[derive(Default)]
struct Owned {
    sealed: bool,
    child: Option<ProtectedChild>,
    reader: Option<JoinHandle<()>>,
}

/// Cleanup ownership for one external session.
pub(crate) struct Slot {
    terminal: AtomicBool,
    wake: mpsc::Sender<()>,
    transport: OnceLock<Transport>,
    owned: Mutex<Owned>,
    workers: AtomicUsize,
    #[cfg(test)]
    test: hooks::Hooks,
}

impl Slot {
    /// Mark the session terminal and signal its transport without waiting.
    pub(crate) fn terminate(&self) {
        self.terminal.store(true, Ordering::SeqCst);
        if let Some(transport) = self.transport.get() {
            transport.signal();
        }
        let _ = self.wake.send(());
    }

    /// Run `work` on a thread the slot waits for before releasing capacity.
    /// Fails once the slot is terminal or if the thread cannot be spawned.
    pub(crate) fn spawn(
        self: &Arc<Self>,
        name: &str,
        work: impl FnOnce() + Send + 'static,
    ) -> io::Result<()> {
        self.workers.fetch_add(1, Ordering::SeqCst);
        let job = Job {
            work,
            _worker: Worker(Arc::clone(self)),
        };
        if self.terminal.load(Ordering::SeqCst) {
            drop(job);
            return Err(io::Error::other("external session is closed"));
        }
        thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let Job { work, _worker } = job;
                work();
            })
            .map(drop)
    }

    fn wake(&self) {
        let _ = self.wake.send(());
    }
}

// Fields drop in order, so rejected work is released before the worker count.
struct Job<F> {
    work: F,
    _worker: Worker,
}

struct Worker(Arc<Slot>);

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.workers.fetch_sub(1, Ordering::SeqCst);
        self.0.wake();
    }
}

/// Registration of startup resources. Dropping it seals the slot, and an
/// uncommitted startup also marks the slot terminal.
pub(crate) struct Startup {
    slot: Arc<Slot>,
    committed: bool,
}

impl Startup {
    pub(crate) fn slot(&self) -> &Arc<Slot> {
        &self.slot
    }

    /// Hand the spawned leader to the slot.
    pub(crate) fn child(&self, child: ProtectedChild) {
        lock(&self.slot.owned).child = Some(child);
        self.slot.wake();
    }

    /// Hand the transport and its reader thread to the slot. A slot that is
    /// already terminal signals the transport before returning.
    pub(crate) fn transport(
        &self,
        shutdown: UnixShutdown,
        reader_stopped: Arc<AtomicBool>,
        reader: JoinHandle<()>,
    ) {
        let transport = Transport {
            shutdown,
            reader_stopped,
        };
        let registered = self.slot.transport.set(transport).is_ok();
        debug_assert!(registered, "transport registered twice");
        lock(&self.slot.owned).reader = Some(reader);
        if self.slot.terminal.load(Ordering::SeqCst)
            && let Some(transport) = self.slot.transport.get()
        {
            transport.signal();
        }
        self.slot.wake();
    }

    /// Finish startup. The caller now owns marking the slot terminal.
    pub(crate) fn commit(mut self) -> Arc<Slot> {
        self.committed = true;
        Arc::clone(&self.slot)
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        lock(&self.slot.owned).sealed = true;
        if self.committed {
            self.slot.wake();
        } else {
            self.slot.terminate();
        }
    }
}

enum Finish {
    Done,
    Retry,
    Lost,
}

fn finish(child: &mut ProtectedChild) -> Finish {
    match child.finish(nix::libc::SIGKILL) {
        Ok(_) => Finish::Done,
        Err(_) if child.identity_lost() => Finish::Lost,
        Err(_) => Finish::Retry,
    }
}

fn run(slot: &Slot, wakes: &mpsc::Receiver<()>, live: &AtomicUsize) {
    while !slot.terminal.load(Ordering::SeqCst) {
        if wakes.recv().is_err() {
            return;
        }
    }
    let mut shut = false;
    let mut lost = false;
    let mut retry = RETRY_MIN;
    let mut next_attempt = Instant::now();
    loop {
        if !shut && let Some(transport) = slot.transport.get() {
            transport.signal();
            transport.shutdown.shutdown();
            shut = true;
        }
        let (sealed, reader) = {
            let mut owned = lock(&slot.owned);
            (owned.sealed, owned.reader.take())
        };
        let child = if Instant::now() >= next_attempt {
            lock(&slot.owned).child.take()
        } else {
            None
        };
        if let Some(mut child) = child {
            #[cfg(test)]
            let result = slot.test.injected().unwrap_or_else(|| finish(&mut child));
            #[cfg(not(test))]
            let result = finish(&mut child);
            match result {
                Finish::Done => {}
                Finish::Lost => lost = true,
                Finish::Retry => {
                    lock(&slot.owned).child = Some(child);
                    next_attempt = Instant::now() + retry;
                    retry = (retry * 2).min(RETRY_MAX);
                }
            }
        }
        if let Some(reader) = reader {
            let _ = reader.join();
        }
        let pending = lock(&slot.owned).child.is_some();
        if sealed && !pending && slot.workers.load(Ordering::SeqCst) == 0 {
            break;
        }
        let woken = if pending {
            !matches!(
                wakes.recv_timeout(next_attempt.saturating_duration_since(Instant::now())),
                Err(mpsc::RecvTimeoutError::Disconnected)
            )
        } else {
            wakes.recv().is_ok()
        };
        if !woken {
            return;
        }
    }
    if !lost {
        live.fetch_sub(1, Ordering::AcqRel);
    }
    #[cfg(test)]
    slot.test.finished();
}

#[cfg(test)]
impl Slot {
    /// Fail the next cleanup attempts with these results, in order.
    pub(crate) fn inject(&self, results: &[bool]) {
        self.test.inject(results);
    }

    /// Wait until the cleanup thread has finished with every resource.
    pub(crate) fn wait_finished(&self, timeout: Duration) -> bool {
        self.test.wait_finished(timeout)
    }

    /// Receive a message for each cleanup attempt of the child.
    pub(crate) fn attempts(&self) -> mpsc::Receiver<()> {
        self.test.attempts()
    }
}

#[cfg(test)]
mod hooks {
    use super::{Finish, lock};
    use std::sync::{Condvar, Mutex, mpsc};
    use std::time::Duration;

    #[derive(Default)]
    pub(super) struct Hooks {
        injected: Mutex<Vec<bool>>,
        attempts: Mutex<Option<mpsc::Sender<()>>>,
        finished: (Mutex<bool>, Condvar),
    }

    impl Hooks {
        // `true` injects a retryable failure, `false` a lost identity.
        pub(super) fn inject(&self, results: &[bool]) {
            lock(&self.injected).extend(results.iter().rev());
        }

        pub(super) fn injected(&self) -> Option<Finish> {
            if let Some(attempts) = lock(&self.attempts).as_ref() {
                let _ = attempts.send(());
            }
            lock(&self.injected)
                .pop()
                .map(|retry| if retry { Finish::Retry } else { Finish::Lost })
        }

        pub(super) fn attempts(&self) -> mpsc::Receiver<()> {
            let (tx, rx) = mpsc::channel();
            *lock(&self.attempts) = Some(tx);
            rx
        }

        pub(super) fn finished(&self) {
            *lock(&self.finished.0) = true;
            self.finished.1.notify_all();
        }

        pub(super) fn wait_finished(&self, timeout: Duration) -> bool {
            let done = lock(&self.finished.0);
            let (done, _) = self
                .finished
                .1
                .wait_timeout_while(done, timeout, |done| !*done)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *done
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_acp::connection::UnixConnection;
    use kage_core::reaper::{recover_protected, spawn_protected};
    use std::io::BufReader;
    use std::process::Command;

    const WAIT: Duration = Duration::from_secs(5);

    fn sleeper() -> ProtectedChild {
        spawn_protected(Command::new("sleep").arg("30")).unwrap()
    }

    fn reaped(pid: u32) -> bool {
        !std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[test]
    fn capacity_is_reserved_before_spawn_and_released_after_cleanup() {
        let lifecycles = Lifecycles::new(1);
        let startup = lifecycles.reserve().unwrap();
        assert!(lifecycles.reserve().is_err());
        let child = sleeper();
        let pid = child.id();
        startup.child(child);
        let slot = Arc::clone(startup.slot());
        drop(startup);
        assert!(slot.wait_finished(WAIT));
        assert!(reaped(pid));
        assert_eq!(lifecycles.live(), 0);
        drop(lifecycles.reserve().unwrap());
    }

    #[test]
    fn committed_slot_waits_for_terminal() {
        let lifecycles = Lifecycles::new(1);
        let startup = lifecycles.reserve().unwrap();
        let child = sleeper();
        let pid = child.id();
        startup.child(child);
        let slot = startup.commit();
        assert!(!slot.wait_finished(Duration::from_millis(50)));
        assert!(!reaped(pid));
        slot.terminate();
        assert!(slot.wait_finished(WAIT));
        assert!(reaped(pid));
        assert_eq!(lifecycles.live(), 0);
    }

    #[test]
    fn terminate_does_not_wait_for_workers() {
        let lifecycles = Lifecycles::new(1);
        let slot = lifecycles.reserve().unwrap().commit();
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        slot.spawn("test-worker", move || {
            entered_tx.send(()).unwrap();
            let _ = released.recv();
        })
        .unwrap();
        entered.recv_timeout(WAIT).unwrap();
        slot.terminate();
        assert!(!slot.wait_finished(Duration::from_millis(50)));
        assert_eq!(lifecycles.live(), 1);
        assert!(slot.spawn("late-worker", || {}).is_err());
        release.send(()).unwrap();
        assert!(slot.wait_finished(WAIT));
        assert_eq!(lifecycles.live(), 0);
    }

    #[test]
    fn failed_cleanup_retains_child_and_slot_until_retry_succeeds() {
        let lifecycles = Lifecycles::new(1);
        let startup = lifecycles.reserve().unwrap();
        let attempts = startup.slot().attempts();
        startup.slot().inject(&[true, true]);
        let child = sleeper();
        let pid = child.id();
        startup.child(child);
        let slot = startup.commit();
        slot.terminate();
        attempts.recv_timeout(WAIT).unwrap();
        assert_eq!(lifecycles.live(), 1);
        assert!(!reaped(pid));
        attempts.recv_timeout(WAIT).unwrap();
        attempts.recv_timeout(WAIT).unwrap();
        assert!(slot.wait_finished(WAIT));
        assert!(reaped(pid));
        assert_eq!(lifecycles.live(), 0);
    }

    #[test]
    fn lost_identity_keeps_slot_reserved_and_quarantines_child() {
        let lifecycles = Lifecycles::new(1);
        let startup = lifecycles.reserve().unwrap();
        startup.slot().inject(&[false]);
        let child = sleeper();
        let pid = child.id();
        startup.child(child);
        let slot = Arc::clone(startup.slot());
        drop(startup);
        assert!(slot.wait_finished(WAIT));
        assert!(lifecycles.reserve().is_err());
        let deadline = std::time::Instant::now() + WAIT;
        let mut child = loop {
            if let Some(child) = recover_protected(pid) {
                break child;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child not quarantined"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(lifecycles.live(), 1);
        child.finish(nix::libc::SIGKILL).unwrap();
    }

    #[test]
    fn startup_unwind_kills_registered_child() {
        let lifecycles = Lifecycles::new(1);
        let pid = std::sync::atomic::AtomicU32::new(0);
        let slot = Mutex::new(None);
        let result = std::panic::catch_unwind(|| {
            let startup = lifecycles.reserve().unwrap();
            *lock(&slot) = Some(Arc::clone(startup.slot()));
            let child = sleeper();
            pid.store(child.id(), Ordering::SeqCst);
            startup.child(child);
            panic!("injected startup panic");
        });
        assert!(result.is_err());
        let slot = lock(&slot).take().unwrap();
        assert!(slot.wait_finished(WAIT));
        assert!(reaped(pid.load(Ordering::SeqCst)));
        assert_eq!(lifecycles.live(), 0);
    }

    #[test]
    fn transport_registered_after_terminal_is_signaled_and_shut_down() {
        let lifecycles = Lifecycles::new(1);
        let startup = lifecycles.reserve().unwrap();
        startup.slot().terminate();
        let (read_end, write_end) = std::io::pipe().unwrap();
        let (upstream, upstream_writer) = std::io::pipe().unwrap();
        let (connection, incoming, reader) =
            UnixConnection::new_managed(BufReader::new(upstream), write_end.into()).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        startup.transport(connection.shutdown_handle(), Arc::clone(&stopped), reader);
        assert!(stopped.load(Ordering::SeqCst));
        let slot = Arc::clone(startup.slot());
        drop(upstream_writer);
        drop(incoming);
        drop(startup);
        assert!(slot.wait_finished(WAIT));
        assert_eq!(lifecycles.live(), 0);
        drop(read_end);
    }
}
