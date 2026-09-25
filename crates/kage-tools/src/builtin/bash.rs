//! `bash` tool: run a shell command with a timeout and truncated output.
//!
//! Commands run unsandboxed today; an OS-level isolation backend (for
//! example bubblewrap) can land later without changing the call shape.

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::{Risk, ToolOutput, ToolUpdate, sync::lock};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ExecMode, Tool, ToolContext, ToolError, resolve, schema_for};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_STREAM_BYTES: usize = 100_000;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const TAIL_BYTES: usize = 4_096;
const TAIL_LINES: usize = 10;

/// The most recent output of both streams, reported as progress while the
/// command runs.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    changed: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > TAIL_BYTES {
            let excess = self.bytes.len() - TAIL_BYTES;
            self.bytes.drain(..excess);
        }
        self.changed = true;
    }

    /// The last [`TAIL_LINES`] lines, if output arrived since the last call.
    fn take_update(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.changed) {
            return None;
        }
        let text = String::from_utf8_lossy(&self.bytes);
        let lines: Vec<&str> = text.lines().collect();
        Some(lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n"))
    }
}

/// Input shape for the `bash` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct BashInput {
    /// Shell command to run, executed via `bash -c <command>`.
    command: String,
    /// Optional working directory, relative to the workdir. Defaults to the workdir.
    #[serde(default, deserialize_with = "super::optional_path")]
    cwd: Option<String>,
    /// Hard timeout in milliseconds. Defaults to 120000 (2 minutes).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Run a shell command.
#[derive(Debug, Default)]
pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command via `bash -c`. Returns combined stdout/stderr and the \
         exit code. Default timeout is 120 seconds; output is truncated at 100KB."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<BashInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Exec
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Sequential)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: BashInput = serde_json::from_value(input)?;
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let cwd = match &input.cwd {
            Some(c) => resolve(cx.workdir(), Path::new(c))?,
            None => cx.workdir().to_path_buf(),
        };
        run_command(&input.command, &cwd, timeout, cx)
    }
}

/// What a finished command left: both streams, each capped at 100KB, and
/// how it exited.
#[derive(Debug)]
pub struct CommandOutput {
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Whether stdout went past the cap.
    pub stdout_truncated: bool,
    /// Captured stderr.
    pub stderr: Vec<u8>,
    /// Whether stderr went past the cap.
    pub stderr_truncated: bool,
    /// Exit code, or `None` when a signal ended the command.
    pub exit_code: Option<i32>,
}

/// Run `command` with `bash -c` in `cwd`, in its own process group, and
/// report the last lines of its output through `cx`'s progress sink while
/// it runs. A cancel of `cx` or passing `timeout` kills the whole group.
///
/// # Errors
///
/// [`ToolError::Cancelled`] or [`ToolError::Timeout`] after a kill, an
/// error naming `cwd` when it does not exist, or an I/O error when the
/// shell cannot start.
pub fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cx: &ToolContext<'_>,
) -> Result<CommandOutput, ToolError> {
    if !cwd.exists() {
        return Err(ToolError::Other(format!(
            "working directory {} does not exist",
            cwd.display()
        )));
    }
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so the kill below reaches grandchildren
        // that inherited our pipes.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let tail = Arc::new(Mutex::new(Tail::default()));
    let stdout_tail = Arc::clone(&tail);
    let stderr_tail = Arc::clone(&tail);
    let stdout_handle = thread::spawn(move || read_capped(&mut stdout, &stdout_tail));
    let stderr_handle = thread::spawn(move || read_capped(&mut stderr, &stderr_tail));

    let watch = cx.cancel_flag().watch();
    let start = Instant::now();
    let mut last_progress = start;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            kill_process_group(&mut child);
            let _ = child.wait();
            return Err(ToolError::Timeout {
                name: "bash".into(),
                millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            if let Some(content) = lock(&tail).take_update() {
                cx.update(ToolUpdate {
                    content,
                    structured: None,
                });
            }
        }
        if watch.receiver().recv_timeout(POLL_INTERVAL).is_ok() {
            kill_process_group(&mut child);
            let _ = child.wait();
            return Err(ToolError::Cancelled);
        }
    };

    // Bash is done, but backgrounded grandchildren may still hold our
    // pipes; they would block the joins below on EOF forever. Once the
    // shell has exited, anything left in its process group is a
    // straggler: release the group before reading.
    kill_process_group(&mut child);
    let _ = child.wait();
    let (stdout, stdout_truncated) = stdout_handle.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_handle.join().unwrap_or_default();
    Ok(CommandOutput {
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        exit_code: status.code(),
    })
}

fn run_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    let CommandOutput {
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        exit_code,
    } = run(command, cwd, timeout, cx)?;
    let stdout_text = String::from_utf8_lossy(&stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr).into_owned();

    let mut text = String::new();
    if !stdout_text.is_empty() {
        text.push_str("stdout:\n");
        text.push_str(&stdout_text);
        if stdout_truncated {
            text.push_str("\n[... stdout truncated at 100KB ...]");
        }
        if !stderr_text.is_empty() {
            text.push('\n');
        }
    }
    if !stderr_text.is_empty() {
        text.push_str("stderr:\n");
        text.push_str(&stderr_text);
        if stderr_truncated {
            text.push_str("\n[... stderr truncated at 100KB ...]");
        }
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    let _ = write!(
        text,
        "\nexit: {}",
        exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string()),
    );

    Ok(ToolOutput {
        is_error: exit_code != Some(0),
        text,
        structured: Some(serde_json::json!({
            "exit_code": exit_code,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "cwd": cwd_display(cwd),
        })),
        terminate: false,
    })
}

fn cwd_display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Kill the spawned shell and everything it left running. The child runs
/// in its own process group (`pgid == pid`), so a group kill reaches
/// grandchildren that inherited our pipes; killing only the shell can
/// leave those alive, and the reader threads then block on EOF forever.
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pgid = nix::unistd::Pid::from_raw(child.id().cast_signed());
        if nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

/// Read `reader` to EOF, keeping at most [`MAX_STREAM_BYTES`] and feeding
/// every chunk to `tail`. Reading continues past the cap so the command
/// never blocks on a full pipe.
fn read_capped<R: Read>(reader: &mut R, tail: &Mutex<Tail>) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(4_096);
    let mut chunk = [0u8; 8_192];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        lock(tail).push(&chunk[..n]);
        let room = MAX_STREAM_BYTES - buf.len();
        if n > room {
            truncated = true;
        }
        buf.extend_from_slice(&chunk[..n.min(room)]);
    }
    (buf, truncated)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        BashTool.execute(input, &cx)
    }

    #[test]
    fn echoes_to_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({"command":"echo hello"})).unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("hello"));
        assert!(out.text.contains("exit: 0"));
    }

    #[test]
    fn nonzero_exit_marks_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({"command":"exit 7"})).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("exit: 7"));
        assert_eq!(out.structured.unwrap()["exit_code"], 7);
    }

    #[test]
    fn captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({"command":"echo oops 1>&2"})).unwrap();
        assert!(out.text.contains("stderr:"));
        assert!(out.text.contains("oops"));
    }

    #[derive(Default)]
    struct Collect(Mutex<Vec<String>>);

    impl crate::ProgressSink for Collect {
        fn emit(&self, update: ToolUpdate) {
            lock(&self.0).push(update.content);
        }
    }

    #[test]
    fn streams_output_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let sink = Arc::new(Collect::default());
        let cx = ToolContext::new(dir.path(), &cancel).with_progress(sink.clone());
        let out = BashTool
            .execute(
                serde_json::json!({"command":"echo first; sleep 0.4; echo second"}),
                &cx,
            )
            .unwrap();
        assert!(out.text.contains("second"));
        let updates = lock(&sink.0);
        assert!(
            updates.iter().any(|u| u == "first"),
            "expected a progress update before the command finished: {updates:?}"
        );
    }

    #[test]
    fn timeout_returns_timeout_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            dir.path(),
            serde_json::json!({"command":"sleep 1","timeout_ms":150}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Timeout { .. }));
    }

    #[test]
    fn timeout_reaps_pipe_holding_grandchildren() {
        // `sleep` inherits our pipes; killing only the shell leaves it
        // alive and the reader threads blocked on EOF until it exits,
        // so this call would take the full sleep duration.
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let err = run(
            dir.path(),
            serde_json::json!({"command":"sleep 5; echo done","timeout_ms":150}),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, ToolError::Timeout { .. }));
        assert!(
            elapsed < Duration::from_secs(3),
            "kill took {elapsed:?}; process group was not killed"
        );
    }

    #[test]
    fn cancel_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let trip = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            trip.cancel();
        });
        let started = Instant::now();
        let cx = ToolContext::new(dir.path(), &cancel);
        let err = super::run("sleep 5; echo done", dir.path(), Duration::MAX, &cx).unwrap_err();
        canceller.join().unwrap();
        assert!(matches!(err, ToolError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn backgrounded_grandchild_does_not_block_success_path() {
        // Bash exits immediately; the backgrounded sleep keeps the pipe
        // write end open. Without releasing the process group before the
        // joins, this call blocks until the sleep finishes.
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = run(
            dir.path(),
            serde_json::json!({"command":"sleep 30 & echo started"}),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(out.text.contains("started"));
        assert!(
            elapsed < Duration::from_secs(5),
            "blocked on orphaned pipe holder for {elapsed:?}"
        );
    }

    #[test]
    fn missing_working_directory_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let err = run(&gone, serde_json::json!({"command":"pwd"})).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("working directory {} does not exist", gone.display())
        );
    }

    #[test]
    fn a_null_string_cwd_runs_in_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        for cwd in ["null", ""] {
            let out = run(dir.path(), serde_json::json!({"command":"pwd","cwd":cwd})).unwrap();
            let workdir = dir.path().canonicalize().unwrap();
            assert!(
                out.text.contains(&*workdir.to_string_lossy()),
                "{}",
                out.text
            );
        }
    }

    #[test]
    fn cwd_resolves_under_workdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/marker.txt"), "x").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"command":"cat marker.txt","cwd":"sub"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains('x'));
    }
}
