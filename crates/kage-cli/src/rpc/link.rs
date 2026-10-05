//! Local links: a kage TUI attached to a session `kage serve` hosts.
//!
//! Serve listens on a unix socket ([`crate::serve_registry`]) that only
//! its own user may reach. A client opens with one handshake line,
//! `{"attach":{"session":"<id>","v":1}}`, and is accepted only for a
//! session this serve has open. Then the link speaks the engine
//! protocol as newline-delimited JSON. The client is sent a
//! `SessionChanged` replayed from the session file, the live state of
//! the session ([`Live::envelopes`](super::live::Live::envelopes)), and
//! from then on every envelope of the session and its agents. It sends
//! [`Command`] lines back. A command without a session goes to the
//! attached one. Commands for sessions outside its agent tree, and the
//! ones that would replace or stop the session, are refused with a
//! notice. Prompts are claimed per connection like ACP prompts, so a
//! prompt from a second client is refused while one runs. The link
//! holds the session open until the client disconnects.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Read as _, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use kage_core::protocol::{Command, CommandKind, Envelope, HostEvent, NoticeLevel};
use kage_core::sync::lock;
use kage_core::{MessageId, SessionId};
use serde::{Deserialize, Serialize};

use super::host::Host;
use crate::engine::SubscriptionId;
use crate::serve::{ACCEPT_POLL, Log};

/// The handshake version this build speaks.
pub(crate) const LINK_VERSION: u32 = 1;

/// The longest handshake line read before the client is refused.
const MAX_HELLO: u64 = 4096;

/// The first line a client sends.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Hello {
    /// Attach to the hosted session `session`.
    Attach {
        /// The session to attach to.
        session: SessionId,
        /// The handshake version the client speaks.
        v: u32,
    },
}

/// Serve's answer to a [`Hello`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reply {
    /// The link is up; envelopes follow.
    Attached {
        /// The serve's process id.
        pid: u32,
    },
    /// The attach was refused, with the reason.
    Refused(String),
}

/// Accepts links on `listener` on a thread of their own until `stop`
/// is set, serving each on its own thread.
pub(crate) fn listen(listener: UnixListener, host: Arc<Host>, stop: Arc<AtomicBool>, log: &Log) {
    let _ = listener.set_nonblocking(true);
    let thread_log = Arc::clone(log);
    let spawned = thread::Builder::new()
        .name("kage-serve-link".to_owned())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let host = Arc::clone(&host);
                        let log = Arc::clone(&thread_log);
                        let _ = thread::Builder::new()
                            .name("kage-serve-link-conn".to_owned())
                            .spawn(move || serve(&host, stream, &log));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                    Err(e) => {
                        thread_log(&format!("link accept failed: {e}"));
                        thread::sleep(ACCEPT_POLL);
                    }
                }
            }
        });
    if spawned.is_err() {
        log("warning: cannot start the link listener; the TUI cannot attach");
    }
}

/// Serves one link until the client disconnects.
pub(crate) fn serve(host: &Host, stream: UnixStream, log: &Log) {
    // An accepted socket inherits the listener's nonblocking flag on
    // some platforms.
    let _ = stream.set_nonblocking(false);
    if !peer_is_us(&stream) {
        log("refuse link (peer runs as another user)");
        return;
    }
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let connection = host.next_connection();
    let (tx, rx) = mpsc::channel();
    let attached =
        hello(&mut reader).and_then(|root| attach(host, root, &tx).map(|sub| (root, sub)));
    let (root, subscription) = match attached {
        Ok(attached) => attached,
        Err(reason) => {
            log(&format!("refuse link ({reason})"));
            let _ = write_line(&mut writer, &Reply::Refused(reason));
            return;
        }
    };
    if write_line(
        &mut writer,
        &Reply::Attached {
            pid: std::process::id(),
        },
    )
    .is_err()
    {
        detach(host, root, connection, subscription);
        return;
    }
    log(&format!("link attach {root} (connection {connection})"));
    let pump = thread::spawn(move || {
        for envelope in rx {
            if write_line(&mut writer, &envelope).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
    });
    let mut line = String::new();
    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
        let refused = match serde_json::from_str::<Command>(&line) {
            Ok(command) => route(host, root, connection, command).err(),
            Err(e) => Some(format!("unreadable command: {e}")),
        };
        if let Some(text) = refused {
            let _ = tx.send(notice(root, text));
        }
        line.clear();
    }
    detach(host, root, connection, subscription);
    drop(tx);
    let _ = pump.join();
    log(&format!("link detach {root} (connection {connection})"));
}

/// Reads the handshake and returns the session it names.
fn hello(reader: &mut impl BufRead) -> Result<SessionId, String> {
    let mut line = String::new();
    reader
        .take(MAX_HELLO)
        .read_line(&mut line)
        .map_err(|e| format!("handshake: {e}"))?;
    match serde_json::from_str(&line) {
        Ok(Hello::Attach { session, v }) if v == LINK_VERSION => Ok(session),
        Ok(Hello::Attach { v, .. }) => Err(format!(
            "link version {v} is not supported; this serve speaks {LINK_VERSION}"
        )),
        Err(e) => Err(format!("unreadable handshake: {e}")),
    }
}

/// Subscribes `tx` to `root`'s tree with forwarding held back, then,
/// with the bus held, queues the session file and the live state and
/// opens the gate, so every envelope reaches the client exactly once.
/// Counts the link as an attachment that keeps the session open.
fn attach(
    host: &Host,
    root: SessionId,
    tx: &mpsc::Sender<Envelope>,
) -> Result<SubscriptionId, String> {
    let gate = Arc::new(AtomicBool::new(false));
    let subscription = {
        let (tx, gate, live) = (tx.clone(), Arc::clone(&gate), Arc::clone(&host.live));
        host.engine.subscribe(Box::new(move |envelope| {
            if gate.load(Ordering::SeqCst) && lock(&live).in_tree(root, envelope.session) {
                let _ = tx.send(envelope.clone());
            }
        }))
    };
    let attached = host.engine.hold_events(|| {
        if host.open_settings(root).is_none() {
            return Err(format!("this serve does not host session {root}"));
        }
        let path = host.sessions.join(format!("{root}.jsonl"));
        for envelope in snapshot(host, root, &path)? {
            let _ = tx.send(envelope);
        }
        host.attach(root, false);
        gate.store(true, Ordering::SeqCst);
        Ok(())
    });
    if attached.is_err() {
        host.engine.unsubscribe(subscription);
    }
    attached.map(|()| subscription)
}

/// The session at `path` as a client that knows nothing of it needs
/// it: its file as `SessionChanged`, then its live state.
fn snapshot(host: &Host, root: SessionId, path: &Path) -> Result<Vec<Envelope>, String> {
    let replay = kage_session::replay(path).map_err(|e| e.to_string())?;
    let file: HashSet<MessageId> = replay.history.iter().map(|m| m.id).collect();
    let changed = HostEvent::SessionChanged {
        path: path.to_path_buf(),
        title: replay.title,
        messages: replay.history.into_iter().map(Arc::new).collect(),
        compaction: replay.compaction,
    };
    let mut out = vec![Envelope {
        session: root,
        seq: 0,
        event: changed.into(),
    }];
    out.extend(lock(&host.live).envelopes(root, &file));
    Ok(out)
}

/// Forwards `command` from the link attached to `root`, or says why it
/// is refused.
fn route(
    host: &Host,
    root: SessionId,
    connection: u64,
    mut command: Command,
) -> Result<(), String> {
    let target = *command.session.get_or_insert(root);
    if !lock(&host.live).in_tree(root, target) {
        return Err(format!(
            "session {target} is not part of the attached session"
        ));
    }
    if let Some(op) = unavailable(&command.kind) {
        return Err(format!(
            "{op} is not available while attached to kage serve"
        ));
    }
    match &command.kind {
        CommandKind::ResolvePermission { request_id, .. } => {
            // An ask already answered, or raised outside this tree, is
            // not this client's to answer.
            let asker = lock(&host.live).asker(*request_id);
            if !asker.is_some_and(|asker| lock(&host.live).in_tree(root, asker)) {
                return Ok(());
            }
        }
        CommandKind::Prompt { .. } if !host.claim_prompt(target, connection) => {
            return Err("session is busy; wait for the running prompt to finish".to_owned());
        }
        _ => {}
    }
    host.engine.send(command);
    Ok(())
}

/// What a link client may not do to a session other clients share.
fn unavailable(kind: &CommandKind) -> Option<&'static str> {
    match kind {
        CommandKind::NewSession => Some("a new session"),
        CommandKind::LoadSession { .. } => Some("resuming another session"),
        CommandKind::Clone => Some("cloning"),
        CommandKind::Fork { switch: true, .. } => Some("switching to a fork"),
        CommandKind::Close | CommandKind::Shutdown => Some("stopping the session"),
        _ => None,
    }
}

/// The link is gone: stop forwarding, forget its prompts, release the
/// session, and decline the asks no other client is left to answer.
fn detach(host: &Host, root: SessionId, connection: u64, subscription: SubscriptionId) {
    host.engine.unsubscribe(subscription);
    host.release_prompts_of(connection);
    host.release(root);
    if host.held(root) {
        return;
    }
    host.decline_asks_under(root);
}

fn notice(session: SessionId, text: String) -> Envelope {
    Envelope {
        session,
        seq: 0,
        event: HostEvent::Notice {
            level: NoticeLevel::Error,
            text,
            transient: false,
        }
        .into(),
    }
}

/// Writes `value` as one JSON line.
pub(crate) fn write_line(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// Whether the peer of `stream` runs as this process's user.
fn peer_is_us(stream: &UnixStream) -> bool {
    peer_uid(stream).is_some_and(|uid| uid == nix::unistd::geteuid().as_raw())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    getsockopt(stream, PeerCredentials)
        .ok()
        .map(|creds| creds.uid())
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    nix::unistd::getpeereid(stream)
        .ok()
        .map(|(uid, _)| uid.as_raw())
}

/// No way to tell the peer's user here, so every link is refused.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn peer_uid(_stream: &UnixStream) -> Option<u32> {
    None
}
