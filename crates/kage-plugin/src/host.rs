//! The thread that owns the plugin runtime's Lua state.
//!
//! [`LuaHost`] is a cloneable handle that queues jobs for one owner
//! thread. Every Lua call in this crate runs there, so no other thread
//! ever touches the Lua state: callers either wait for a reply
//! (optionally bounded by a deadline or a cancel flag) or, for render
//! surfaces, read output retained from an earlier job (see
//! [`crate::retained`]).
//!
//! Jobs run one at a time, in submission order. A long Lua tool
//! therefore occupies the owner thread for its whole duration.
//! Retained render output stays on screen meanwhile, but commands,
//! keybindings, event dispatch, and render refreshes queue behind it.
//! A Lua provider stream does not: it runs as a coroutine the owner
//! polls between jobs (see [`crate::tasks`]), yielding while it waits
//! on the network. Callbacks queued with `kage.schedule`, `kage.defer`
//! and `kage.timer` run on the same thread between jobs, never during
//! one (see [`crate::schedule`]).
//!
//! A job must never wait synchronously on another host request: the
//! owner thread would be waiting on itself.
//!
//! The thread lives as long as any strong [`LuaHost`] exists. Callbacks
//! installed into Lua hold a [`WeakHost`] (and weak registry handles)
//! so the Lua state never keeps its own owner thread alive.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Weak};
use std::task::{Context, Wake, Waker};
use std::thread;
use std::time::Duration;

use crossbeam_channel::select_biased;
use kage_core::CancelFlag;
use mlua::Lua;

use crate::error::PluginError;
use crate::tasks::Task;
use crate::{schedule, tasks, watchdog};

/// A queued job. It must call [`State::finish`] once its work is done
/// and before it replies, so a caller woken by the reply already sees
/// the job accounted for in [`LuaHost::is_idle`].
type Job = Box<dyn FnOnce(&Lua, &State) + Send>;

/// What reaches the owner thread: a job, or word that a task it drives
/// can make progress.
enum Msg {
    Job(Job),
    Wake,
}

/// Cloneable handle that runs jobs on the Lua owner thread.
#[derive(Clone)]
pub(crate) struct LuaHost {
    inner: Arc<Inner>,
}

/// Non-owning [`LuaHost`] handle for callbacks that live inside Lua.
#[derive(Clone)]
pub(crate) struct WeakHost(Weak<Inner>);

struct Inner {
    tx: Sender<Msg>,
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    in_flight: AtomicUsize,
    missed_render: AtomicBool,
    redraw: Arc<AtomicBool>,
    blocks: Arc<AtomicBool>,
}

/// Receiving end of a new host, consumed by [`Owner::spawn`] once the
/// Lua state is fully set up.
pub(crate) struct Owner {
    rx: Receiver<Msg>,
    state: Arc<State>,
    /// Where task wakers post their wakeups. Weak, so a pending task
    /// never keeps the owner thread alive.
    host: Weak<Inner>,
}

impl LuaHost {
    /// Create a host handle and the owner side that will run its jobs.
    pub(crate) fn new() -> (Self, Owner) {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(State::default());
        let inner = Arc::new(Inner { tx, state });
        let owner = Owner {
            rx,
            state: Arc::clone(&inner.state),
            host: Arc::downgrade(&inner),
        };
        (Self { inner }, owner)
    }

    pub(crate) fn downgrade(&self) -> WeakHost {
        WeakHost(Arc::downgrade(&self.inner))
    }

    /// Queue `f` and return a receiver for its result.
    pub(crate) fn queue<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Lua) -> R + Send + 'static,
    ) -> Result<Receiver<R>, PluginError> {
        let (tx, rx) = mpsc::channel();
        self.enqueue(Box::new(move |lua, state| {
            let reply = f(lua);
            state.finish();
            let _ = tx.send(reply);
        }))?;
        Ok(rx)
    }

    /// Queue `job` without waiting for it.
    pub(crate) fn submit(
        &self,
        job: impl FnOnce(&Lua) + Send + 'static,
    ) -> Result<(), PluginError> {
        self.queue(job).map(drop)
    }

    /// Run `f` on the owner thread and wait for its result.
    pub(crate) fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Lua) -> R + Send + 'static,
    ) -> Result<R, PluginError> {
        self.queue(f)?.recv().map_err(|_| gone())
    }

    /// Like [`Self::call`], but give up after `timeout`. A job that has
    /// not started by then is skipped. `None` on timeout or when the
    /// owner thread is gone.
    pub(crate) fn call_within<R: Send + 'static>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&Lua) -> R + Send + 'static,
    ) -> Option<R> {
        let (rx, abandoned) = self.request(f).ok()?;
        let reply = rx.recv_timeout(timeout).ok();
        if reply.is_none() {
            abandoned.store(true, Ordering::SeqCst);
        }
        reply
    }

    /// Like [`Self::call`], but stop waiting as soon as `cancel` is set.
    /// `Ok(None)` means the caller cancelled; a job that has not started
    /// by then is skipped.
    pub(crate) fn call_cancellable<R: Send + 'static>(
        &self,
        cancel: &CancelFlag,
        f: impl FnOnce(&Lua) -> R + Send + 'static,
    ) -> Result<Option<R>, PluginError> {
        let (rx, abandoned) = self.request(f)?;
        let watch = cancel.watch();
        select_biased! {
            recv(rx) -> reply => reply.map(Some).map_err(|_| gone()),
            recv(watch.receiver()) -> _ => {
                abandoned.store(true, Ordering::SeqCst);
                Ok(None)
            }
        }
    }

    fn request<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Lua) -> R + Send + 'static,
    ) -> Result<(crossbeam_channel::Receiver<R>, Arc<AtomicBool>), PluginError> {
        let abandoned = Arc::new(AtomicBool::new(false));
        let skip = Arc::clone(&abandoned);
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.enqueue(Box::new(move |lua, state| {
            let reply = (!skip.load(Ordering::SeqCst)).then(|| f(lua));
            state.finish();
            if let Some(reply) = reply {
                let _ = tx.send(reply);
            }
        }))?;
        Ok((rx, abandoned))
    }

    fn enqueue(&self, job: Job) -> Result<(), PluginError> {
        let state = &self.inner.state;
        state.in_flight.fetch_add(1, Ordering::SeqCst);
        if self.inner.tx.send(Msg::Job(job)).is_err() {
            state.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(gone());
        }
        Ok(())
    }

    /// `true` when no job is queued or running.
    pub(crate) fn is_idle(&self) -> bool {
        self.inner.state.in_flight.load(Ordering::SeqCst) == 0
    }

    /// Number of jobs queued or running on the owner thread.
    pub(crate) fn in_flight(&self) -> usize {
        self.inner.state.in_flight.load(Ordering::SeqCst)
    }

    /// Record that a render skipped a recompute because the owner was
    /// busy. The redraw flag is set once the queue drains, so the host
    /// renders again and the recompute happens then.
    pub(crate) fn note_missed_render(&self) {
        self.inner.state.missed_render.store(true, Ordering::SeqCst);
    }

    /// Flag set whenever retained render output may have changed.
    pub(crate) fn redraw_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.state.redraw)
    }

    /// Flag set when block renderer output arrived or a skipped block
    /// render can now run, so block heights may have changed.
    pub(crate) fn blocks_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.state.blocks)
    }
}

impl State {
    fn finish(&self) {
        if self.in_flight.fetch_sub(1, Ordering::SeqCst) == 1
            && self.missed_render.swap(false, Ordering::SeqCst)
        {
            self.blocks.store(true, Ordering::SeqCst);
            self.redraw.store(true, Ordering::SeqCst);
        }
    }
}

impl WeakHost {
    /// Upgrade from inside a Lua callback. Fails once every strong
    /// handle is gone, which only happens while the runtime shuts down.
    pub(crate) fn upgrade(&self) -> mlua::Result<LuaHost> {
        self.0
            .upgrade()
            .map(|inner| LuaHost { inner })
            .ok_or_else(runtime_gone)
    }
}

/// Upgrade a weak registry handle from inside a Lua callback.
pub(crate) fn upgrade<T>(weak: &Weak<T>) -> mlua::Result<Arc<T>> {
    weak.upgrade().ok_or_else(runtime_gone)
}

fn runtime_gone() -> mlua::Error {
    mlua::Error::external("plugin runtime is shutting down")
}

fn gone() -> PluginError {
    PluginError::Host("the Lua owner thread is gone".to_owned())
}

impl Owner {
    /// Start the owner thread. It takes `lua` and runs queued jobs until
    /// every [`LuaHost`] is dropped, pending callbacks or not. Between
    /// jobs it waits no longer than the next callback deadline, and after
    /// each job or wakeup it runs the due callbacks (see
    /// [`crate::schedule`]). A panicking job or callback pass is contained
    /// so the thread keeps serving later requests.
    pub(crate) fn spawn(self, lua: Lua) -> Result<(), PluginError> {
        let Owner { rx, state, host } = self;
        tasks::install(&lua);
        thread::Builder::new()
            .name("kage-lua".to_owned())
            .spawn(move || {
                let mut running: Vec<(Task, Arc<TaskWake>)> = Vec::new();
                loop {
                    let msg = match schedule::next_wait(&lua) {
                        None => match rx.recv() {
                            Ok(msg) => Some(msg),
                            Err(_) => break,
                        },
                        Some(wait) => match rx.recv_timeout(wait) {
                            Ok(msg) => Some(msg),
                            Err(RecvTimeoutError::Timeout) => None,
                            Err(RecvTimeoutError::Disconnected) => break,
                        },
                    };
                    if let Some(Msg::Job(job)) = msg
                        && catch_unwind(AssertUnwindSafe(|| job(&lua, &state))).is_err()
                    {
                        state.finish();
                    }
                    running.extend(tasks::take_spawned(&lua).into_iter().map(|task| {
                        let wake = Arc::new(TaskWake {
                            woken: AtomicBool::new(true),
                            host: host.clone(),
                        });
                        (task, wake)
                    }));
                    running.retain_mut(|(task, wake)| poll_task(&lua, task, wake));
                    if schedule::next_wait(&lua) == Some(Duration::ZERO) {
                        state.in_flight.fetch_add(1, Ordering::SeqCst);
                        let _ = catch_unwind(AssertUnwindSafe(|| schedule::run_due(&lua)));
                        state.finish();
                    }
                }
            })
            .map(drop)
            .map_err(|e| PluginError::Host(format!("failed to start the Lua thread: {e}")))
    }
}

/// A driven task's waker: it marks the task woken and wakes the owner
/// thread, which polls woken tasks between jobs.
struct TaskWake {
    woken: AtomicBool,
    host: Weak<Inner>,
}

impl Wake for TaskWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.woken.swap(true, Ordering::SeqCst)
            && let Some(inner) = self.host.upgrade()
        {
            let _ = inner.tx.send(Msg::Wake);
        }
    }
}

/// Poll `task` if it was woken, under the watchdog budget so one resume
/// cannot run away. `false` once it is done or panicked.
fn poll_task(lua: &Lua, task: &mut Task, wake: &Arc<TaskWake>) -> bool {
    if !wake.woken.swap(false, Ordering::SeqCst) {
        return true;
    }
    let waker = Waker::from(Arc::clone(wake));
    let mut cx = Context::from_waker(&waker);
    let polled = catch_unwind(AssertUnwindSafe(|| {
        watchdog::run(lua, watchdog::BUDGET, || {
            Ok::<_, mlua::Error>(task.as_mut().poll(&mut cx))
        })
    }));
    matches!(polled, Ok(Ok(std::task::Poll::Pending)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TIMEOUT, occupy};

    fn started() -> LuaHost {
        let (host, owner) = LuaHost::new();
        owner.spawn(Lua::new()).unwrap();
        host
    }

    #[test]
    fn jobs_run_in_submission_order() {
        let host = started();
        host.call(|lua| lua.globals().set("seen", lua.create_table()?))
            .unwrap()
            .unwrap();
        for i in 0..50 {
            host.submit(move |lua| {
                let seen: mlua::Table = lua.globals().get("seen").unwrap();
                seen.push(i).unwrap();
            })
            .unwrap();
        }
        let seen: Vec<i64> = host
            .call(|lua| lua.load("return seen").eval::<Vec<i64>>().unwrap())
            .unwrap();
        assert_eq!(seen, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn call_within_times_out_and_skips_the_stale_job() {
        let host = started();
        let busy = occupy(&host);
        let reply = host.call_within(Duration::from_millis(20), |lua| {
            lua.globals().set("ran", true).unwrap();
        });
        assert!(reply.is_none());
        busy.assert_held();
        busy.open();
        let ran: bool = host
            .call(|lua| {
                lua.globals()
                    .get::<Option<bool>>("ran")
                    .unwrap()
                    .unwrap_or(false)
            })
            .unwrap();
        assert!(!ran, "an abandoned job must not run");
    }

    #[test]
    fn call_cancellable_returns_when_cancelled() {
        let host = started();
        let busy = occupy(&host);
        let cancel = CancelFlag::new();
        cancel.cancel();
        assert!(host.call_cancellable(&cancel, |_| ()).unwrap().is_none());
        busy.assert_held();
        busy.open();
    }

    #[test]
    fn a_panicking_job_does_not_kill_the_thread() {
        let host = started();
        assert!(host.call(|_| -> () { panic!("boom") }).is_err());
        assert_eq!(host.call(|_| 7).unwrap(), 7);
    }

    #[test]
    fn owner_thread_drops_lua_when_the_last_handle_drops() {
        let lua = Lua::new();
        lua.globals()
            .set("kage", lua.create_table().unwrap())
            .unwrap();
        crate::watchdog::install(&lua).unwrap();
        let (_, sink) = crate::testing::recording_sink();
        schedule::install(&lua, sink, Arc::default(), crate::watchdog::BUDGET).unwrap();
        let (host, owner) = LuaHost::new();
        owner.spawn(lua).unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        host.call(move |lua| {
            let keep = lua
                .create_function(move |_, ()| {
                    let _ = &tx;
                    Ok(())
                })
                .unwrap();
            lua.globals().set("keep", keep).unwrap();
            lua.load("kage.timer(function() keep() end, 50) kage.defer(keep, 60000)")
                .exec()
                .unwrap();
        })
        .unwrap();
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        let weak = host.downgrade();
        drop(host);
        assert_eq!(
            rx.recv_timeout(TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        );
        assert!(weak.upgrade().is_err());
    }
}
