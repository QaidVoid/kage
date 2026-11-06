//! Reaping of orphaned grandchildren for long-lived processes.
//!
//! A kage daemon lives inside a PID namespace where it is PID 1, so
//! every process a finished tool call left behind reparents to the
//! daemon once its own parent exits. Nothing waits on those orphans,
//! and each unreaped one pins a PID until the daemon exits. On a busy
//! daemon that bleeds the namespace's PIDs dry. [`start`] installs a
//! background thread that collects them.
//!
//! The reaper cannot know which children a caller still intends to
//! wait on, so every status it collects is recorded, and the
//! [`try_wait`] and [`wait`] wrappers fall back to that record
//! when the kernel reports `ECHILD` because the reaper won the
//! race to a tracked child.

use std::io;
use std::process::{Child, ExitStatus};

pub use imp::{drain, start, try_wait, wait};
pub use protected::{ProtectedChild, quarantined_pids, recover_protected, spawn_protected};

/// Protected process groups require exclusive control over child reaping.
///
/// On Linux, callers must not enable SIGCHLD auto-reaping or install another
/// reaper, and must not wait on protected children outside this API. Group
/// members other than the leader that leave the original group are not covered.
/// Spawn hooks must not call this reaper or protected spawning, because spawn
/// holds its process-wide lock. The process must have a trustworthy, accessible
/// procfs mounted at `/proc` for unrelated children to be reaped while
/// protection is active.
#[cfg(target_os = "linux")]
mod protected {
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
    use std::sync::{Mutex, OnceLock};

    static QUARANTINE: OnceLock<Mutex<Vec<(i32, Child, bool)>>> = OnceLock::new();

    static PROTECTED: OnceLock<Mutex<HashMap<i32, usize>>> = OnceLock::new();

    pub(super) fn state() -> &'static Mutex<HashMap<i32, usize>> {
        PROTECTED.get_or_init(|| Mutex::new(HashMap::new()))
    }

    #[cfg(test)]
    static FORCE_ESRCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[cfg(test)]
    static FAIL_SCAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub(super) fn children() -> io::Result<HashSet<i32>> {
        #[cfg(test)]
        if FAIL_SCAN.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other("injected procfs failure"));
        }
        children_in(std::path::Path::new("/proc/self/task"))
    }

    fn children_in(tasks: &std::path::Path) -> io::Result<HashSet<i32>> {
        let mut children = HashSet::new();
        for task in std::fs::read_dir(tasks)? {
            let task = task?;
            let text = std::fs::read_to_string(task.path().join("children"))?;
            for pid in text.split_whitespace() {
                let pid: i32 = pid.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid child PID in procfs")
                })?;
                if pid <= 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid child PID in procfs",
                    ));
                }
                children.insert(pid);
            }
        }
        Ok(children)
    }

    /// An exclusively owned process group leader reserved against the reaper.
    ///
    /// Dropping an unfinished guard moves it to a process-wide quarantine.
    /// Recover it with [`recover_protected`] to retry cleanup. The PID stays
    /// reserved until cleanup succeeds. If ownership is lost, the guard can
    /// never safely signal or release that reservation. It remains permanently
    /// quarantinable and can be discovered with [`quarantined_pids`].
    #[must_use]
    pub struct ProtectedChild {
        child: Option<Child>,
        pid: i32,
        identity_lost: bool,
    }

    /// Spawn a new process group leader and reserve it atomically with respect
    /// to this module's global reaper. The command's process group is set to its
    /// own PID. The process must not use SIGCHLD auto-reaping, another global
    /// reaper, or external waits on this child. Spawn hooks must not reenter the
    /// reaper or this API. Violating the exclusive-reaping contract can cause a
    /// signal to reach a reused PID. Unrelated reaping while a guard exists
    /// also requires a trustworthy, accessible procfs mounted at `/proc`.
    pub fn spawn_protected(command: &mut Command) -> io::Result<ProtectedChild> {
        let mut protected = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        command.process_group(0);
        let child = command.spawn()?;
        let pid = child.id().cast_signed();
        *protected.entry(pid).or_default() += 1;
        Ok(ProtectedChild {
            child: Some(child),
            pid,
            identity_lost: false,
        })
    }

    /// Recover a dropped guard from quarantine by its leader PID.
    pub fn recover_protected(pid: u32) -> Option<ProtectedChild> {
        let pid = i32::try_from(pid).ok()?;
        let mut quarantine = QUARANTINE
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = quarantine.iter().position(|(p, _, _)| *p == pid)?;
        let (_, child, identity_lost) = quarantine.swap_remove(index);
        Some(ProtectedChild {
            child: Some(child),
            pid,
            identity_lost,
        })
    }

    /// List quarantined leader PIDs so callers can recover dropped guards.
    pub fn quarantined_pids() -> Vec<u32> {
        QUARANTINE
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(pid, _, _)| pid.cast_unsigned())
            .collect()
    }

    impl ProtectedChild {
        /// Return the reserved process group leader's PID.
        #[must_use]
        pub fn id(&self) -> u32 {
            self.pid.cast_unsigned()
        }

        /// Return whether ownership was lost, so cleanup can never succeed.
        #[must_use]
        pub fn identity_lost(&self) -> bool {
            self.identity_lost
        }

        /// Take the leader's piped standard input, if configured and available.
        pub fn take_stdin(&mut self) -> Option<ChildStdin> {
            self.child.as_mut()?.stdin.take()
        }

        /// Take the leader's piped standard output, if configured and available.
        pub fn take_stdout(&mut self) -> Option<ChildStdout> {
            self.child.as_mut()?.stdout.take()
        }

        /// Take the leader's piped standard error, if configured and available.
        pub fn take_stderr(&mut self) -> Option<ChildStderr> {
            self.child.as_mut()?.stderr.take()
        }

        /// Signal the original group and its leader, then wait for and reap the
        /// leader. The leader is signaled even if it has changed groups, and it
        /// may receive the signal twice if it remains in the original group.
        /// Other group members that moved elsewhere are not signaled.
        /// A signal or wait
        /// error retains the reservation. An `ECHILD` error marks identity lost
        /// and permanently disables signaling through this guard. Other errors
        /// can be retried. Success means the leader was reaped, not that every
        /// group member exited. No lock is held during the blocking wait.
        /// Any standard input still held by the guard is closed before signaling.
        /// A standard input previously taken by the caller must be closed by the
        /// caller if the child needs EOF to exit.
        /// The signal must be nonzero. Use a terminating signal if the leader
        /// might otherwise keep running indefinitely.
        pub fn finish(&mut self, signal: i32) -> io::Result<ExitStatus> {
            if self.child.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "child already reaped",
                ));
            }
            if signal <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "signal must be positive",
                ));
            }
            if self.identity_lost {
                return Err(io::Error::other("protected child identity was lost"));
            }
            let signal = nix::sys::signal::Signal::try_from(signal)
                .map_err(|err| io::Error::from_raw_os_error(err as i32))?;
            drop(self.child.as_mut().and_then(|child| child.stdin.take()));
            let pid = nix::unistd::Pid::from_raw(self.pid);
            let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT;
            // Preflight detects a previously lost child before signaling. The
            // exclusive-reaper contract is still needed to prevent a race.
            if let Err(err) = waitid(Id::Pid(pid), flags | WaitPidFlag::WNOHANG) {
                if err == nix::errno::Errno::ECHILD {
                    self.identity_lost = true;
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            #[cfg(test)]
            let signal_result = if FORCE_ESRCH.load(std::sync::atomic::Ordering::SeqCst) {
                Err(nix::errno::Errno::ESRCH)
            } else {
                nix::sys::signal::killpg(pid, signal)
            };
            #[cfg(not(test))]
            let signal_result = nix::sys::signal::killpg(pid, signal);
            if let Err(err) = signal_result
                && err != nix::errno::Errno::ESRCH
            {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            // The leader's PID remains pinned even if it leaves the original
            // group. Signal it directly as well so it cannot escape cleanup.
            if let Err(err) = nix::sys::signal::kill(pid, signal) {
                if err != nix::errno::Errno::ESRCH {
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
                match waitid(Id::Pid(pid), flags | WaitPidFlag::WNOHANG) {
                    Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => {}
                    Err(nix::errno::Errno::ECHILD) => {
                        self.identity_lost = true;
                        return Err(io::Error::from_raw_os_error(libc::ECHILD));
                    }
                    Err(e) => return Err(io::Error::from_raw_os_error(e as i32)),
                    Ok(_) => return Err(io::Error::from_raw_os_error(err as i32)),
                }
            }
            // WNOWAIT pins the PID until the nonblocking reap and reservation
            // removal are performed under the same lock.
            if let Err(err) = waitid(Id::Pid(pid), flags) {
                if err == nix::errno::Errno::ECHILD {
                    self.identity_lost = true;
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            let mut protected = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let raw = match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => code << 8,
                Ok(WaitStatus::Signaled(_, signal, core)) => {
                    signal as i32 | if core { 0x80 } else { 0 }
                }
                Err(err) => {
                    if err == nix::errno::Errno::ECHILD {
                        self.identity_lost = true;
                    }
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
                _ => return Err(io::Error::other("protected child was not ready to reap")),
            };
            let count = protected
                .get_mut(&self.pid)
                .expect("protected child reservation missing");
            *count -= 1;
            if *count == 0 {
                protected.remove(&self.pid);
            }
            self.child = None;
            Ok(ExitStatus::from_raw(raw))
        }
    }

    impl Drop for ProtectedChild {
        fn drop(&mut self) {
            if let Some(child) = self.child.take() {
                QUARANTINE
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((self.pid, child, self.identity_lost));
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, Write};
        use std::os::unix::process::ExitStatusExt;
        use std::time::{Duration, Instant};

        fn wait_for(mut check: impl FnMut() -> bool) {
            let end = Instant::now() + Duration::from_secs(10);
            while !check() {
                assert!(Instant::now() < end, "timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn zombie(pid: u32) -> bool {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit(')').next().map(str::to_owned))
                .is_some_and(|s| s.split_whitespace().next() == Some("Z"))
        }

        fn exercise_edge_cases() {
            // Simulate a missing group after its leader has exited.
            let mut no_group = spawn_protected(&mut Command::new("true")).unwrap();
            wait_for(|| zombie(no_group.id()));
            FORCE_ESRCH.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(no_group.finish(libc::SIGTERM).unwrap().success());
            FORCE_ESRCH.store(false, std::sync::atomic::Ordering::SeqCst);

            let mut pipe = spawn_protected(
                Command::new("sh")
                    .args(["-c", "read x; echo \"$x\"; exec sleep 10"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped()),
            )
            .unwrap();
            writeln!(pipe.take_stdin().unwrap(), "hello").unwrap();
            let mut line = String::new();
            std::io::BufReader::new(pipe.take_stdout().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "hello\n");
            assert!(pipe.take_stderr().is_some());
            pipe.finish(libc::SIGKILL).unwrap();

            let mut eof = spawn_protected(
                Command::new("sh")
                    .args([
                        "-c",
                        "trap 'read x || exit 0' TERM; echo ready; while :; do sleep 0.05; done",
                    ])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped()),
            )
            .unwrap();
            let mut ready = String::new();
            std::io::BufReader::new(eof.take_stdout().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            assert!(eof.finish(libc::SIGTERM).unwrap().success());

            // A competing waiter violates the contract, but confirmed loss
            // must nevertheless permanently disable further signals.
            let mut lost = spawn_protected(&mut Command::new("true")).unwrap();
            let lost_pid = lost.id();
            wait_for(|| zombie(lost_pid));
            waitpid(nix::unistd::Pid::from_raw(lost_pid.cast_signed()), None).unwrap();
            assert_eq!(
                lost.finish(libc::SIGTERM).unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );
            drop(lost);
            assert!(quarantined_pids().contains(&lost_pid));
            let mut lost = recover_protected(lost_pid).unwrap();
            assert!(
                lost.finish(libc::SIGTERM)
                    .unwrap_err()
                    .to_string()
                    .contains("identity")
            );
            drop(lost);
        }

        fn exercise_moved_leader() {
            let mut moved = spawn_protected(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "reaper::protected::tests::isolated_protected_reaping",
                        "--nocapture",
                    ])
                    .env("KAGE_REAPER_MOVED_CHILD", "1")
                    .stdout(std::process::Stdio::piped()),
            )
            .unwrap();
            let mut output = std::io::BufReader::new(moved.take_stdout().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert!(output.read_line(&mut line).unwrap() != 0);
                if line.contains("moved-ready") {
                    break;
                }
            }
            assert_eq!(
                nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(moved.id().cast_signed())))
                    .unwrap(),
                nix::unistd::getpgrp()
            );
            assert_eq!(
                moved.finish(libc::SIGKILL).unwrap().signal(),
                Some(libc::SIGKILL)
            );
        }

        #[test]
        fn isolated_protected_reaping() {
            const ENV: &str = "KAGE_REAPER_ISOLATED";
            if std::env::var_os("KAGE_REAPER_MOVED_CHILD").is_some() {
                let group = nix::unistd::getpgid(Some(nix::unistd::getppid())).unwrap();
                nix::unistd::setpgid(nix::unistd::getpid(), group).unwrap();
                println!("moved-ready");
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            if std::env::var_os(ENV).is_none() {
                let status = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "reaper::protected::tests::isolated_protected_reaping",
                        "--nocapture",
                    ])
                    .env(ENV, "1")
                    .status()
                    .unwrap();
                assert!(status.success());
                return;
            }

            super::super::start();
            for _ in 0..50 {
                let mut child = spawn_protected(&mut Command::new("true")).unwrap();
                wait_for(|| zombie(child.id()));
                super::super::drain();
                assert!(zombie(child.id()));
                assert!(child.finish(libc::SIGTERM).unwrap().success());
            }

            let mut threads = Vec::new();
            for _ in 0..4 {
                threads.push(std::thread::spawn(|| {
                    for _ in 0..20 {
                        let child = spawn_protected(&mut Command::new("true")).unwrap();
                        let pid = child.id();
                        wait_for(|| zombie(pid));
                        drop(child);
                        super::super::drain();
                        assert!(zombie(pid));
                        let mut recovered = recover_protected(pid).unwrap();
                        assert!(recovered.finish(libc::SIGTERM).unwrap().success());
                    }
                }));
            }
            for thread in threads {
                thread.join().unwrap();
            }

            let mut child = spawn_protected(&mut Command::new("true")).unwrap();
            wait_for(|| zombie(child.id()));
            let other = std::thread::spawn(|| Command::new("true").spawn().unwrap().id())
                .join()
                .unwrap();
            wait_for(|| zombie(other));
            FAIL_SCAN.store(true, std::sync::atomic::Ordering::SeqCst);
            super::super::drain();
            assert!(zombie(child.id()));
            assert!(zombie(other));
            FAIL_SCAN.store(false, std::sync::atomic::Ordering::SeqCst);
            wait_for(|| {
                super::super::drain();
                !std::path::Path::new(&format!("/proc/{other}")).exists()
            });
            assert!(zombie(child.id()));
            assert!(child.finish(0).is_err());
            assert!(zombie(child.id()));
            child.finish(libc::SIGTERM).unwrap();

            exercise_moved_leader();

            exercise_edge_cases();

            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let signaled = dir.path().join("signaled");
            let script = format!(
                "trap 'exit 0' TERM; (trap 'echo yes > {}; exit 0' TERM; echo yes > {}; while :; do sleep 0.05; done) & wait",
                signaled.display(),
                ready.display()
            );
            let mut leader = spawn_protected(Command::new("sh").args(["-c", &script])).unwrap();
            wait_for(|| ready.exists());
            leader.finish(libc::SIGTERM).unwrap();
            wait_for(|| signaled.exists());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod protected {
    use std::io;
    use std::process::{Command, ExitStatus};

    /// Protected process groups are unavailable on this platform.
    pub struct ProtectedChild {
        _private: (),
    }

    /// Return an error because protected spawning requires Linux procfs.
    pub fn spawn_protected(_: &mut Command) -> io::Result<ProtectedChild> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "protected children require Linux",
        ))
    }

    /// Return an empty list because protected guards are unavailable.
    #[must_use]
    pub fn quarantined_pids() -> Vec<u32> {
        Vec::new()
    }

    /// Protected guards cannot exist on this platform.
    #[must_use]
    pub fn recover_protected(_: u32) -> Option<ProtectedChild> {
        None
    }

    impl ProtectedChild {
        /// Return the process identifier.
        #[must_use]
        pub fn id(&self) -> u32 {
            unreachable!("protected children require Linux")
        }
        /// Protected children cannot exist on this platform.
        #[must_use]
        pub fn identity_lost(&self) -> bool {
            unreachable!("protected children require Linux")
        }
        /// Protected children have no standard input on this platform.
        pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
            None
        }
        /// Protected children have no standard output on this platform.
        pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
            None
        }
        /// Protected children have no standard error on this platform.
        pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
            None
        }
        /// Return an error because protected children require Linux.
        pub fn finish(&mut self, _: i32) -> io::Result<ExitStatus> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "protected children require Linux",
            ))
        }
    }
}

#[cfg(unix)]
mod imp {
    #[cfg(target_os = "linux")]
    use super::protected;
    use super::{Child, ExitStatus, io};
    use std::collections::HashMap;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    /// How often the reaper looks for finished orphans.
    const PERIOD: Duration = Duration::from_millis(250);
    /// How long a recorded status is kept for a late waiter.
    const RETAIN: Duration = Duration::from_secs(300);
    /// How long a waiter whose child was just reaped waits for the
    /// status to become visible in the record.
    const GRACE: Duration = Duration::from_millis(100);

    #[derive(Clone, Copy)]
    struct Record {
        raw: i32,
        at: Instant,
    }

    static RECORDS: OnceLock<Mutex<HashMap<u32, Record>>> = OnceLock::new();
    static STARTED: OnceLock<()> = OnceLock::new();

    fn records() -> &'static Mutex<HashMap<u32, Record>> {
        RECORDS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Start the background reaper once per process; later calls do
    /// nothing.
    pub fn start() {
        STARTED.get_or_init(|| {
            let spawned = std::thread::Builder::new()
                .name("kage-reaper".to_owned())
                .spawn(|| {
                    loop {
                        drain();
                        std::thread::sleep(PERIOD);
                    }
                });
            if spawned.is_err() {
                // The daemon still works without the reaper; it only
                // leaks orphan PIDs again.
                eprintln!("kage: could not start the orphan reaper thread");
            }
        });
    }

    /// Reap every finished child, recording each status. Public so
    /// tests can drive a reaping pass by hand.
    pub fn drain() {
        #[cfg(target_os = "linux")]
        {
            let guard = protected::state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_empty() {
                while reap_one(None) {}
            } else {
                match protected::children() {
                    Ok(children) => {
                        for pid in children {
                            if !guard.contains_key(&pid) {
                                reap_one(Some(pid));
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("kage: orphan reaper could not enumerate children: {err}");
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        while reap_one(None) {}
        let now = Instant::now();
        records()
            .lock()
            .unwrap()
            .retain(|_, record| now.duration_since(record.at) < RETAIN);
    }

    fn reap_one(pid: Option<i32>) -> bool {
        let (pid, raw) = match waitpid(
            pid.map(nix::unistd::Pid::from_raw),
            Some(WaitPidFlag::WNOHANG),
        ) {
            Ok(WaitStatus::Exited(pid, code)) => (pid, code << 8),
            Ok(WaitStatus::Signaled(pid, signal, core)) => {
                (pid, (signal as i32) | if core { 0x80 } else { 0 })
            }
            Ok(_) | Err(_) => return false,
        };
        let Ok(pid) = u32::try_from(pid.as_raw()) else {
            return false;
        };
        records().lock().unwrap().insert(
            pid,
            Record {
                raw,
                at: Instant::now(),
            },
        );
        true
    }

    /// The recorded status for `pid`, waiting out the moment between
    /// the reaper reaping the child and recording it.
    fn recorded(pid: u32) -> Option<i32> {
        let deadline = Instant::now() + GRACE;
        loop {
            if let Some(record) = records().lock().unwrap().remove(&pid) {
                return Some(record.raw);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Like [`Child::try_wait`], but a status the reaper already
    /// collected is still reported.
    pub fn try_wait(child: &mut Child) -> io::Result<Option<ExitStatus>> {
        match child.try_wait() {
            Ok(result) => Ok(result),
            Err(err) if err.raw_os_error() == Some(libc::ECHILD) => {
                Ok(recorded(child.id()).map(ExitStatus::from_raw))
            }
            Err(err) => Err(err),
        }
    }

    /// Like [`Child::wait`], but a status the reaper already collected
    /// is still reported.
    pub fn wait(child: &mut Child) -> io::Result<ExitStatus> {
        match child.wait() {
            Ok(status) => Ok(status),
            Err(err) if err.raw_os_error() == Some(libc::ECHILD) => match recorded(child.id()) {
                Some(raw) => Ok(ExitStatus::from_raw(raw)),
                None => Err(err),
            },
            Err(err) => Err(err),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Command;

        /// The process state letter from `/proc/pid/stat`, or `None`
        /// once the process is fully gone.
        fn state(pid: u32) -> Option<String> {
            let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let after = text.rsplit(')').next()?;
            after.split_whitespace().next().map(str::to_owned)
        }

        /// Drive draining passes until the child `pid` has been
        /// reaped and its status recorded, which is the moment the
        /// kernel starts reporting `ECHILD` to its waiter. A drain
        /// pass from another test may do the reaping; it records the
        /// status all the same.
        fn drain_until_reaped(pid: u32) {
            for _ in 0..100 {
                drain();
                if records().lock().unwrap().contains_key(&pid) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("pid {pid} was never reaped and recorded");
        }

        #[test]
        #[allow(clippy::zombie_processes)]
        fn drain_reaps_a_finished_child() {
            let child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            assert_eq!(state(pid), None, "zombie survived the drain");
        }

        #[test]
        fn wait_recovers_a_stolen_status() {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            let status = wait(&mut child).expect("wait after the reaper stole the status");
            assert!(status.success());
        }

        #[test]
        fn try_wait_recovers_a_stolen_status() {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            let status = try_wait(&mut child)
                .expect("try_wait after the reaper stole the status")
                .expect("child already exited");
            assert!(status.success());
        }

        #[test]
        fn wait_recovers_a_signaled_status() {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg("kill -TERM $$")
                .spawn()
                .unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            let status = wait(&mut child).expect("wait after the reaper stole the status");
            assert_eq!(status.signal(), Some(15));
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Child, ExitStatus, io};

    /// Nothing to start: only Unix reaps orphaned children.
    pub fn start() {}

    /// Nothing to drain: only Unix reaps orphaned children.
    pub fn drain() {}

    /// [`Child::try_wait`], as no reaper takes statuses here.
    pub fn try_wait(child: &mut Child) -> io::Result<Option<ExitStatus>> {
        child.try_wait()
    }

    /// [`Child::wait`], as no reaper takes statuses here.
    pub fn wait(child: &mut Child) -> io::Result<ExitStatus> {
        child.wait()
    }
}
