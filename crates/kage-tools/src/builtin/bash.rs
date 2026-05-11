//! `bash` tool: run a shell command with a timeout and truncated output.
//!
//! T3.7 ships an unsandboxed implementation. Phase 10 wires the call through
//! `kage-sandbox` so the same tool can run inside bubblewrap or sandbox-exec
//! when those backends are available, without changing the call shape.

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, resolve, schema_for};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_STREAM_BYTES: usize = 100_000;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Input shape for the `bash` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct BashInput {
    /// Shell command to run, executed via `bash -c <command>`.
    command: String,
    /// Optional working directory, relative to the workdir. Defaults to the workdir.
    #[serde(default)]
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

fn run_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let stdout_handle = thread::spawn(move || read_capped(&mut stdout));
    let stderr_handle = thread::spawn(move || read_capped(&mut stderr));

    let start = Instant::now();
    let status = loop {
        if cx.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::Cancelled);
        }
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::Timeout {
                name: "bash".into(),
                millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
        thread::sleep(POLL_INTERVAL);
    };

    let (stdout_bytes, stdout_truncated) = stdout_handle.join().unwrap_or_default();
    let (stderr_bytes, stderr_truncated) = stderr_handle.join().unwrap_or_default();

    let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let exit_code = status.code();

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
        is_error: !status.success(),
        text,
        structured: Some(serde_json::json!({
            "exit_code": exit_code,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "cwd": cwd_display(cwd),
        })),
    })
}

fn cwd_display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn read_capped<R: Read>(reader: &mut R) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(4_096);
    let mut taken = reader.take((MAX_STREAM_BYTES as u64) + 1);
    let _ = taken.read_to_end(&mut buf);
    let truncated = buf.len() > MAX_STREAM_BYTES;
    if truncated {
        buf.truncate(MAX_STREAM_BYTES);
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
