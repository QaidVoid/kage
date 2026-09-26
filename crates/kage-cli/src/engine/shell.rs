//! User shell commands.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use kage_core::message::ShellRun;
use kage_core::protocol::HostEvent;
use kage_core::sync::lock;
use kage_core::{Content, Message, Role, SessionId, ToolUpdate};
use kage_tools::{ProgressSink, ToolContext, ToolError};

use super::bus::Bus;
use super::runner::Work;
use super::{Idle, Input, flush_pending, record_late_title, settle_working};

/// A user shell command that ended, with what it took from the session.
pub(super) struct ShellDone {
    session: SessionId,
    command: String,
    output: String,
    exit_code: Option<i32>,
    /// The idle state a command started on an idle session held, so no
    /// run started while it ran.
    idle: Option<Idle>,
}

/// Publishes the tail of a running user shell command and keeps the
/// latest one, which stands in for the output when the command is
/// cancelled.
struct ShellProgress {
    bus: Arc<Bus>,
    session: SessionId,
    command: String,
    tail: Mutex<String>,
}

impl ProgressSink for ShellProgress {
    fn emit(&self, update: ToolUpdate) {
        lock(&self.tail).clone_from(&update.content);
        self.bus.publish(
            self.session,
            HostEvent::ShellOutput {
                command: self.command.clone(),
                tail: update.content,
            },
        );
    }
}

impl super::Dispatcher {
    /// Run a user shell command on a worker thread. On an idle session it
    /// holds the session like a run, so prompts wait for it and a cancel
    /// stops it. During a run it runs alongside, and the run's cancel
    /// stops it too.
    pub(super) fn shell(&mut self, id: SessionId, command: String) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        let idle = session.idle.take();
        if idle.is_some() {
            session.cancel.reset();
        }
        session.shells += 1;
        session.state.working = true;
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
        let cancel = session.cancel.child();
        let workdir = session.workdir.clone();
        let progress = Arc::new(ShellProgress {
            bus: Arc::clone(&self.bus),
            session: id,
            command,
            tail: Mutex::default(),
        });
        let tx = self.tx.clone();
        thread::spawn(move || {
            progress.emit(ToolUpdate {
                content: String::new(),
                structured: None,
            });
            let cx = ToolContext::new(&workdir, &cancel).with_progress(progress.clone());
            let (exit_code, output) = run_shell(&progress.command, &cx).unwrap_or_else(|_| {
                let tail = lock(&progress.tail);
                let output = if tail.trim().is_empty() {
                    "cancelled".to_owned()
                } else {
                    format!("{}\ncancelled", tail.trim_end())
                };
                (None, output)
            });
            let _ = tx.send(Input::ShellDone(Box::new(ShellDone {
                session: id,
                command: progress.command.clone(),
                output,
                exit_code,
                idle,
            })));
        });
    }

    /// Show a finished shell command, fire `user_bash` for plugins, and
    /// add its output to the history, recorded, for the model's next
    /// turn. A command that held the session gives it back and starts
    /// what was submitted meanwhile.
    pub(super) fn shell_done(&mut self, done: ShellDone) {
        let ShellDone {
            session: id,
            command,
            output,
            exit_code,
            idle,
        } = done;
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.shells -= 1;
        let text = ShellRun {
            command: command.clone(),
            exit_code,
            output: output.clone(),
        }
        .to_text();
        session
            .pending_history
            .push(Message::new(Role::User, vec![Content::Text { text }], None));
        let held = idle.is_some();
        if let Some(idle) = idle {
            session.idle = Some(idle);
            session.cancel.reset();
            record_late_title(&self.bus, id, session);
        }
        flush_pending(&self.bus, id, session);
        if let Some(rt) = &session.plugins {
            crate::plugins::notify_user_bash(rt, &command, exit_code);
        }
        self.bus.publish(
            id,
            HostEvent::ShellFinished {
                command,
                output,
                exit_code,
            },
        );
        settle_working(&self.bus, id, session);
        if !held || self.shutting_down {
            return;
        }
        let steered: Vec<String> = lock(&session.steering).drain(..).collect();
        for text in steered.into_iter().rev() {
            session.queued.push_front(vec![Content::Text { text }]);
        }
        if let Some(content) = session.queued.pop_front() {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        }
    }
}

/// Run a user shell command with the bash tool's runner in `cx`'s
/// workdir, streaming its tail to `cx`'s progress sink, and capture stdout
/// and stderr together, truncated so a chatty command cannot flood the
/// context. Returns the exit code (`None` when a signal ended the command
/// or it failed to spawn) and the output.
///
/// # Errors
///
/// [`ToolError::Cancelled`] when `cx` was cancelled and the command
/// killed.
pub(crate) fn run_shell(
    command: &str,
    cx: &ToolContext<'_>,
) -> Result<(Option<i32>, String), ToolError> {
    const OUTPUT_CAP: usize = 8 * 1024;
    // The `!` shell is user-typed, so the env-scrub policy for
    // model-authored commands does not apply.
    let output = match kage_tools::builtin::bash::run(command, cx.workdir(), Duration::MAX, &[], cx)
    {
        Ok(output) => output,
        Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
        Err(err) => return Ok((None, format!("failed to run: {err}"))),
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if combined.chars().count() > OUTPUT_CAP {
        let cut: String = combined.chars().take(OUTPUT_CAP).collect();
        combined = format!("{cut}\n... (output truncated)");
    }
    Ok((output.exit_code, combined))
}
