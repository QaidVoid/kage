//! `kage serve`: the Agent Client Protocol over WebSocket on one TCP
//! listener.
//!
//! One endpoint, `/acp`, behind a bearer token. A `GET` upgrade request
//! that presents the token in the `Authorization` header, in a
//! `Sec-WebSocket-Protocol` entry, or in the `token` query parameter is
//! upgraded and then served exactly like a `kage rpc` connection on the
//! shared [`Host`]; the `101` response names the connection with
//! `Acp-Connection-Id`. Every other request gets a plain HTTP reply:
//! `401` without a valid token, `405` for non-upgrade traffic on
//! `/acp`, `431` for an oversize request head, and `503` once
//! [`MAX_CONNECTIONS`] connections are already being served. The other
//! routes serve the web client bundle: `GET /` with the page and
//! `GET /<file>` with a file under the `--web-dir` directory
//! ([`assets`]), `405` for other methods, `404` elsewhere.
//!
//! On unix, serve also registers a local socket ([`crate::rpc::link`])
//! that a kage TUI of the same user attaches through, so `kage resume`
//! on a session open here joins it instead of failing on its lock.
//!
//! The accept loop runs on the main thread and hands every connection
//! its own thread. SIGINT and SIGTERM cancel every run, give the
//! sessions a moment to close their files, and exit with status 0; a
//! second signal exits at once.
//!
//! Log lines go to stderr prefixed `kage serve:`. The connect URL in
//! the startup output is the only place the token is ever rendered;
//! refusals name the peer address, never a presented value.

use std::io::{self, Read as _};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use kage_remote::head::{self, Auth, Head, HeadError};
use kage_remote::pipe;
use kage_remote::token::Token;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::rpc::host::Host;

mod assets;

use assets::WebDir;

/// How many connections may be served at once. The next one is refused
/// with `503`.
pub(crate) const MAX_CONNECTIONS: usize = 16;

/// The TCP port bound when `--port` is not given.
pub(crate) const DEFAULT_PORT: u16 = 7433;

/// How long a shutdown waits for runs to unwind and session files to
/// close before the process exits anyway. A run stuck inside a
/// provider read that never yields cannot be interrupted (the same
/// tradeoff print mode accepts).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// How long the accept loop sleeps between polls. The listener is
/// nonblocking so the shutdown flag is checked promptly.
pub(crate) const ACCEPT_POLL: Duration = Duration::from_millis(25);

/// How long a rejected connection may take to collect its reply before
/// the socket is dropped.
const REJECT_DRAIN: Duration = Duration::from_millis(200);

/// The id of the next served connection, named in the 101's
/// `Acp-Connection-Id` and in the serve log lines.
static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(0);

/// Consumes the next connection id.
fn next_connection_id() -> u64 {
    NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed)
}

/// The sink for one `kage serve:` line, without the prefix. Real runs
/// print to stderr; tests collect the lines.
pub(crate) type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// Where the token guarding the endpoint lives:
/// `$XDG_DATA_HOME/kage/remote-token`, next to `auth.json`.
fn token_path() -> Result<PathBuf, String> {
    Ok(crate::data_root()?.join("remote-token"))
}

/// The web bundle directory when `--web-dir` is not given: a `web/`
/// directory beside the executable, where a bundle copied next to the
/// binary is picked up without flags.
pub(crate) fn default_web_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .map_or_else(|| PathBuf::from("web"), |dir| dir.join("web"))
}

/// Entry point for the `Serve` subcommand.
pub(crate) fn run(
    model: Option<&str>,
    system_role: &str,
    bind_host: &str,
    port: u16,
    rotate_token: bool,
    web_dir: Option<&Path>,
) -> ExitCode {
    let log: Log = Arc::new(|line| eprintln!("kage serve: {line}"));
    let host = match Host::start(model, system_role) {
        Ok(host) => host,
        Err(e) => {
            eprintln!("kage: serve: {e}");
            return ExitCode::from(1);
        }
    };
    let (token_path, token) = match load_token(rotate_token) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("kage: serve: {e}");
            return ExitCode::from(1);
        }
    };
    let bundle_dir = web_dir.map_or_else(default_web_dir, Path::to_path_buf);
    let web = WebDir::open(&bundle_dir);
    if !web.available() {
        log(&format!(
            "web UI unavailable: no index.html under {}; pass --web-dir to serve it",
            bundle_dir.display()
        ));
    }
    let listener = match TcpListener::bind((bind_host, port)) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("kage: serve: cannot bind {bind_host}:{port}: {e}; pass a free --port");
            return ExitCode::from(1);
        }
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("kage: serve: bound address: {e}");
            return ExitCode::from(1);
        }
    };
    log(&format!("token file {}", token_path.display()));
    log(&format!(
        "connect: ws://{addr}/acp?token={}",
        token.as_str()
    ));
    if web.available() {
        log(&format!("web UI: http://{addr}/"));
    }
    warn_non_loopback(bind_host, &log);

    let stop = Arc::new(AtomicBool::new(false));
    install_signals(&stop, &log);
    #[cfg(unix)]
    let registration = start_link(&host, &stop, &log);
    accept_until(&listener, &host, &token, &web, &stop, &log);

    #[cfg(unix)]
    drop(registration);
    log("shutting down; cancelling runs");
    host.shutdown();
    thread::sleep(SHUTDOWN_GRACE);
    ExitCode::SUCCESS
}

/// Registers the link socket TUIs attach through and starts accepting
/// on it. Serve runs on without it when that fails.
#[cfg(unix)]
fn start_link(
    host: &Arc<Host>,
    stop: &Arc<AtomicBool>,
    log: &Log,
) -> Option<crate::serve_registry::Registration> {
    let registered =
        crate::paths::runtime_dir().and_then(|dir| match crate::serve_registry::register(&dir) {
            Ok((listener, registration)) => Ok((dir, listener, registration)),
            Err(e) => Err(format!("{}: {e}", dir.display())),
        });
    match registered {
        Ok((dir, listener, registration)) => {
            log(&format!("TUI attach socket in {}", dir.display()));
            crate::rpc::link::listen(listener, Arc::clone(host), Arc::clone(stop), log);
            Some(registration)
        }
        Err(e) => {
            log(&format!("warning: the TUI cannot attach here: {e}"));
            None
        }
    }
}

/// Loads the token at [`token_path`], or replaces it when `rotate` is
/// set. Returns the path beside the token for the startup line.
fn load_token(rotate: bool) -> Result<(PathBuf, Arc<Token>), String> {
    let path = token_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let token = if rotate {
        Token::rotate(&path)
    } else {
        Token::load_or_create(&path)
    }
    .map_err(|e| format!("token at {}: {e}", path.display()))?;
    Ok((path, Arc::new(token)))
}

/// Registers SIGINT and SIGTERM handlers. The first signal sets `stop`
/// so the accept loop unwinds into the graceful shutdown; a second one
/// exits immediately. When the handler thread cannot start, a plain
/// flag still stops the loop.
fn install_signals(stop: &Arc<AtomicBool>, log: &Log) {
    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM]) else {
        log("warning: cannot register signal handlers; kill the process to stop it");
        return;
    };
    let thread_stop = Arc::clone(stop);
    let thread_log = Arc::clone(log);
    let spawned = thread::Builder::new()
        .name("kage-serve-signal".to_owned())
        .spawn(move || {
            let mut received = signals.forever();
            if received.next().is_some() {
                thread_stop.store(true, Ordering::SeqCst);
                thread_log("shutting down; signal again to exit at once");
                if received.next().is_some() {
                    std::process::exit(130);
                }
            }
        });
    if spawned.is_err() {
        let _ = signal_hook::flag::register(SIGINT, Arc::clone(stop));
    }
}

/// Accepts connections until `stop` is set. Every connection gets its
/// own thread; once [`MAX_CONNECTIONS`] threads are live the next peer
/// is refused with `503`.
fn accept_until(
    listener: &TcpListener,
    host: &Arc<Host>,
    token: &Arc<Token>,
    web: &WebDir,
    stop: &AtomicBool,
    log: &Log,
) {
    let _ = listener.set_nonblocking(true);
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                log(&format!("connect {peer}"));
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    log(&format!("refuse {peer} (503)"));
                    reject(stream, 503, "Service Unavailable", BODY_503);
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let thread_host = Arc::clone(host);
                let thread_token = Arc::clone(token);
                let thread_web = web.clone();
                let thread_log = Arc::clone(log);
                let thread_active = Arc::clone(&active);
                let spawned = thread::Builder::new()
                    .name("kage-serve-conn".to_owned())
                    .spawn(move || {
                        handle_connection(
                            stream,
                            peer,
                            &thread_host,
                            &thread_token,
                            &thread_web,
                            &thread_log,
                        );
                        thread_active.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::SeqCst);
                    log(&format!("refuse {peer} (thread spawn failed)"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) => {
                log(&format!("accept failed: {e}"));
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

/// The routing table for one accepted connection: read the head, then
/// upgrade `/acp` on a valid token, or answer with the plain reply the
/// request earns.
fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    host: &Arc<Host>,
    token: &Arc<Token>,
    web: &WebDir,
    log: &Log,
) {
    match head::read_head(&mut stream) {
        Err(HeadError::TooLarge) => {
            log(&format!("refuse {peer} (431)"));
            reject(
                stream,
                431,
                "Request Header Fields Too Large",
                "request head is too large\n",
            );
        }
        Err(_) => log(&format!("refuse {peer} (unreadable request head)")),
        Ok(head) => route(head, stream, peer, host, token, web, log),
    }
}

/// Routes a parsed request head.
fn route(
    head: Head,
    mut stream: TcpStream,
    peer: SocketAddr,
    host: &Arc<Host>,
    token: &Arc<Token>,
    web: &WebDir,
    log: &Log,
) {
    if head.path != "/acp" {
        if !head.method.eq_ignore_ascii_case("GET") {
            log(&format!("refuse {peer} (405)"));
            assets::reject_method(&mut stream);
            return;
        }
        match web.serve(&head.path, &mut stream) {
            assets::Outcome::Served => {}
            assets::Outcome::NotFound => log(&format!("refuse {peer} (404)")),
            assets::Outcome::Traversal => log(&format!("refuse {peer} (traversal)")),
        }
        return;
    }
    let Some(auth) = head::authorize(&head, token) else {
        log(&format!("refuse {peer} (401)"));
        reject(stream, 401, "Unauthorized", BODY_401);
        return;
    };
    let upgraded = head.method.eq_ignore_ascii_case("GET")
        && head
            .header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if !upgraded {
        log(&format!("refuse {peer} (405)"));
        reject(stream, 405, "Method Not Allowed", BODY_405);
        return;
    }
    let subprotocol = match &auth {
        Auth::Subprotocol(entry) => Some(entry.as_str()),
        Auth::Bearer | Auth::Query => None,
    };
    let id = next_connection_id().to_string();
    match pipe::upgrade(&mut stream, head, &id, subprotocol) {
        Ok((reader, writer)) => {
            log(&format!("attach {peer} (connection {id})"));
            let _ = Arc::clone(host).serve(reader, writer);
            log(&format!("disconnect {peer} (connection {id})"));
        }
        Err(e) => log(&format!("refuse {peer} (upgrade failed: {e})")),
    }
}

/// Writes a plain HTTP reply and drains the unread inbound, so the
/// peer receives the answer instead of a reset when the socket closes.
fn reject(mut stream: TcpStream, status: u16, reason: &str, body: &str) {
    if head::respond(&mut stream, status, reason, body).is_err() {
        return;
    }
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(REJECT_DRAIN));
    let mut sink = [0u8; 4096];
    loop {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

const BODY_401: &str = "a valid token is required\n";
const BODY_405: &str =
    "the ACP endpoint /acp speaks the WebSocket protocol; send a GET upgrade request\n";
const BODY_503: &str = "the server is at its connection limit; try again later\n";

/// Prints the plain-text warning when `bind_host` does not name a
/// loopback address, because kage itself serves no TLS.
fn warn_non_loopback(bind_host: &str, log: &Log) {
    let loopback = match bind_host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bind_host.eq_ignore_ascii_case("localhost"),
    };
    if !loopback {
        log(&format!(
            "warning: {bind_host} is not loopback: the connection has no TLS, so anyone who \
             learns the connect URL can drive kage on this machine"
        ));
    }
}

#[cfg(test)]
mod tests;
