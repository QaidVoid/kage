//! OAuth tokens for remote MCP servers: storage, refresh and login.
//!
//! Tokens live in `$XDG_DATA_HOME/kage/mcp-auth.json` ([`McpAuthStore`]),
//! written with mode `0600` by the same helper as `auth.json` and keyed
//! by the canonical server URL, so a token only ever goes to the server
//! it was issued for. [`McpTokens`] hands them to the HTTP transport and
//! refreshes them. [`login`] runs the authorization flow of
//! [`kage_mcp::oauth`] in the terminal, for `kage mcp login` and for
//! `/mcp login` while the TUI is suspended.
//!
//! Only the authorization URL and status lines are printed. No token,
//! code or verifier reaches the terminal, an error or a `Debug` string.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_core::config::{Config, McpServer};
use kage_core::sync::lock;
use kage_mcp::TokenSource;
use kage_mcp::oauth::{self, AuthCode, Expected, Loopback, OAuthError, Pkce};
use serde::{Deserialize, Serialize};

use crate::auth::OAuthCredential;

/// On-disk shape of `mcp-auth.json`.
const FORMAT_VERSION: u32 = 1;

/// A token whose deadline falls inside this window from now is treated
/// as expired, so a request about to fire does not race the expiry.
const REFRESH_SLACK: chrono::Duration = chrono::Duration::seconds(60);

/// How often the login checks for a pasted redirect or a cancel while
/// it waits.
const POLL: Duration = Duration::from_millis(50);

/// Serializes token refreshes in this process. Another kage process can
/// still race, which the re-read before each refresh narrows.
static REFRESH: Mutex<()> = Mutex::new(());

/// The stored tokens of every logged-in MCP server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpAuthStore {
    /// Schema version so future bumps can migrate.
    #[serde(default = "default_version")]
    pub(crate) version: u32,
    /// Entries keyed by canonical server URL.
    #[serde(default)]
    pub(crate) servers: BTreeMap<String, McpAuthEntry>,
}

/// One server's login: where its tokens come from, and the tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpAuthEntry {
    /// Issuer of the authorization server.
    pub(crate) issuer: String,
    /// Where refresh tokens are traded.
    pub(crate) token_endpoint: String,
    /// The client id kage used, configured or registered.
    pub(crate) client_id: String,
    /// The `resource` indicator the token was issued for.
    pub(crate) resource: String,
    /// The tokens themselves.
    pub(crate) token: OAuthCredential,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl McpAuthStore {
    /// `$XDG_DATA_HOME/kage/mcp-auth.json`, next to `auth.json`.
    pub(crate) fn default_path() -> Result<PathBuf, String> {
        Ok(crate::data_root()?.join("mcp-auth.json"))
    }

    /// Load the store from `path`, empty when the file does not exist.
    pub(crate) fn load_from(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| format!("mcp auth: parse {}: {e}", path.display())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("mcp auth: read {}: {err}", path.display())),
        }
    }

    /// Save the store to `path` with mode `0600` on Unix.
    pub(crate) fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("mcp auth: mkdir {}: {err}", parent.display()))?;
        }
        let mut bumped = self.clone();
        bumped.version = FORMAT_VERSION;
        let raw =
            serde_json::to_string_pretty(&bumped).map_err(|e| format!("mcp auth: encode: {e}"))?;
        kage_core::fsutil::atomic_write_private(path, raw.as_bytes())
            .map_err(|err| format!("mcp auth: write {}: {err}", path.display()))
    }
}

/// The [`TokenSource`] backed by `mcp-auth.json`. The file is read on
/// every request, so a login in another process or a token it rotated
/// is picked up without a restart.
pub(crate) struct McpTokens {
    path: PathBuf,
}

impl McpTokens {
    /// Tokens stored at `path`.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The entry for `url`, with its canonical key.
    fn entry(&self, url: &str) -> Option<(String, McpAuthEntry)> {
        let key = oauth::canonical_url(url).ok()?;
        let entry = McpAuthStore::load_from(&self.path)
            .ok()?
            .servers
            .remove(&key)?;
        Some((key, entry))
    }

    /// Refresh the entry under `key` and save the rotated tokens. The
    /// caller holds [`REFRESH`], and the file was just read under it.
    fn refresh(&self, key: &str, entry: &McpAuthEntry) -> Option<String> {
        let refresh_token = entry.token.refresh_token.as_deref()?;
        let tokens = oauth::refresh(
            &entry.token_endpoint,
            &entry.client_id,
            &entry.resource,
            refresh_token,
        )
        .ok()?;
        let token = credential(tokens);
        let access = token.access_token.clone();
        let mut store = McpAuthStore::load_from(&self.path).ok()?;
        store.servers.insert(
            key.to_owned(),
            McpAuthEntry {
                token,
                ..entry.clone()
            },
        );
        store.save_to(&self.path).ok()?;
        Some(access)
    }
}

impl TokenSource for McpTokens {
    fn bearer(&self, url: &str) -> Option<String> {
        let _guard = lock(&REFRESH);
        let (key, entry) = self.entry(url)?;
        let now = Utc::now();
        if !entry.token.expires_within(REFRESH_SLACK, now) {
            return Some(entry.token.access_token);
        }
        self.refresh(&key, &entry).or_else(|| {
            (!entry.token.expires_within(chrono::Duration::zero(), now))
                .then_some(entry.token.access_token)
        })
    }

    fn rejected(&self, url: &str) -> Option<String> {
        let _guard = lock(&REFRESH);
        let (key, entry) = self.entry(url)?;
        self.refresh(&key, &entry)
    }
}

/// The token source every run hands the MCP manager, `None` when the
/// data directory cannot be resolved.
pub(crate) fn token_source() -> Option<Arc<dyn TokenSource>> {
    let path = McpAuthStore::default_path().ok()?;
    Some(Arc::new(McpTokens::new(path)))
}

/// Tokens from the token endpoint in the stored form.
fn credential(tokens: oauth::Tokens) -> OAuthCredential {
    OAuthCredential {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at.map(DateTime::<Utc>::from),
        scope: tokens.scope,
    }
}

/// The URL of server `name` in `servers`, which must be an HTTP server.
fn server_url<'a>(name: &str, servers: &'a BTreeMap<String, McpServer>) -> Result<&'a str, String> {
    let spec = servers
        .get(name)
        .ok_or_else(|| format!("no MCP server named `{name}` is configured here"))?;
    spec.url
        .as_deref()
        .ok_or_else(|| format!("`{name}` is a stdio server. Only HTTP servers log in with OAuth"))
}

/// Log in to MCP server `name` of `servers` and store its tokens at
/// `path`. Prints the authorization URL and waits for the first of the
/// browser coming back to the loopback listener or a redirect URL
/// arriving on `pasted`, and gives up once `cancel` is set. `open`
/// tries to show the URL in a browser. The client id is the configured
/// one, else the one stored by an earlier login at the same issuer,
/// else a newly registered one.
pub(crate) fn login(
    name: &str,
    servers: &BTreeMap<String, McpServer>,
    path: &Path,
    pasted: &Receiver<String>,
    open: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let url = server_url(name, servers)?;
    let config = servers[name].oauth.clone().unwrap_or_default();
    let discovery = oauth::discover(url).map_err(|e| e.to_string())?;
    let loopback = Loopback::bind().map_err(|e| e.to_string())?;
    let redirect_uri = loopback.redirect_uri();
    let stored = McpAuthStore::load_from(path)?
        .servers
        .remove(&discovery.resource)
        .filter(|entry| entry.issuer == discovery.issuer)
        .map(|entry| entry.client_id);
    let client_id = match config.client_id.or(stored) {
        Some(id) => id,
        None => oauth::register(&discovery, &redirect_uri).map_err(|e| e.to_string())?,
    };
    let pkce = Pkce::new().map_err(|e| e.to_string())?;
    let expected = Expected::new(&discovery).map_err(|e| e.to_string())?;
    let scope = config.scope.or_else(|| discovery.scope.clone());
    let authorize = oauth::authorize_url(
        &discovery,
        &client_id,
        &redirect_uri,
        &pkce,
        &expected,
        scope.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    eprintln!("kage: authorize kage for MCP server `{name}` in your browser:\n");
    eprintln!("  {authorize}\n");
    open(&authorize);
    eprintln!(
        "kage: waiting for the browser to return ({} minutes).",
        oauth::LOGIN_TIMEOUT.as_secs() / 60
    );
    eprintln!("kage: on another machine? paste the URL your browser was sent to and press Enter:");
    eprint!("> ");
    let _ = io::stderr().flush();
    let code = wait_for_code(loopback, &expected, pasted, cancel).map_err(|e| e.to_string())?;
    let tokens = oauth::exchange(&discovery, &client_id, &redirect_uri, &code, &pkce)
        .map_err(|e| e.to_string())?;
    let mut store = McpAuthStore::load_from(path)?;
    store.servers.insert(
        discovery.resource.clone(),
        McpAuthEntry {
            issuer: discovery.issuer,
            token_endpoint: discovery.token_endpoint,
            client_id,
            resource: discovery.resource,
            token: credential(tokens),
        },
    );
    store.save_to(path)?;
    eprintln!("kage: {name} authorized");
    Ok(())
}

/// The first of the browser's redirect and a pasted one, or
/// [`OAuthError::Cancelled`] once `cancel` is set. Blank lines are
/// ignored, and a closed input leaves only the browser. When the
/// browser wins or the wait is cancelled, the prompt line is ended.
fn wait_for_code(
    loopback: Loopback,
    expected: &Expected,
    pasted: &Receiver<String>,
    cancel: &AtomicBool,
) -> Result<AuthCode, OAuthError> {
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let stop = &stop;
        let browser = scope.spawn(move || loopback.wait(expected, oauth::LOGIN_TIMEOUT, stop));
        let stopped = || OAuthError::Local("the loopback listener stopped".to_owned());
        loop {
            if browser.is_finished() {
                eprintln!();
                return browser.join().unwrap_or_else(|_| Err(stopped()));
            }
            if cancel.load(Ordering::SeqCst) {
                stop.store(true, Ordering::SeqCst);
                let _ = browser.join();
                eprintln!();
                return Err(OAuthError::Cancelled);
            }
            match pasted.recv_timeout(POLL) {
                Ok(line) if line.trim().is_empty() => {}
                Ok(line) => {
                    stop.store(true, Ordering::SeqCst);
                    let _ = browser.join();
                    return oauth::parse_redirect(&line, expected);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => thread::sleep(POLL),
            }
        }
    })
}

/// Forget the tokens of server `url` in the store at `path`. Returns
/// whether there were any.
fn forget(path: &Path, url: &str) -> Result<bool, String> {
    let key = oauth::canonical_url(url).map_err(|e| e.to_string())?;
    let mut store = McpAuthStore::load_from(path)?;
    if store.servers.remove(&key).is_none() {
        return Ok(false);
    }
    store.save_to(path)?;
    Ok(true)
}

/// Forward stdin lines to `tx` until the first non-blank one, or until
/// nobody listens any more.
fn read_paste(tx: &Sender<String>) {
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        let done = !line.trim().is_empty();
        if tx.send(line).is_err() || done {
            return;
        }
    }
}

/// Show `url` in a browser when there seems to be one. Never fails, and
/// never lets the opener touch the terminal.
fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        "xdg-open"
    } else {
        return;
    };
    let child = std::process::Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || child.wait());
    }
}

/// The MCP servers of the layered config for the current directory, so
/// an untrusted project's servers stay invisible.
fn configured_servers() -> Result<BTreeMap<String, McpServer>, String> {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::trust::warn_if_untrusted(&workdir);
    Config::load_layered(&workdir)
        .map(|config| config.mcp.servers)
        .map_err(|e| format!("config: {e}"))
}

/// `kage mcp login <server>`.
pub(crate) fn run_login(name: &str) -> ExitCode {
    let result = configured_servers().and_then(|servers| {
        let path = McpAuthStore::default_path()?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || read_paste(&tx));
        login(
            name,
            &servers,
            &path,
            &rx,
            &open_browser,
            &AtomicBool::new(false),
        )
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: mcp login {name}: {e}");
            ExitCode::from(1)
        }
    }
}

/// `kage mcp logout <server>`.
pub(crate) fn run_logout(name: &str) -> ExitCode {
    let result = configured_servers().and_then(|servers| {
        let url = server_url(name, &servers)?;
        forget(&McpAuthStore::default_path()?, url)
    });
    match result {
        Ok(true) => {
            eprintln!("kage: forgot the token for {name}");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("kage: no stored token for {name}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: mcp logout {name}: {e}");
            ExitCode::from(1)
        }
    }
}

/// `/mcp login <server>` while the TUI is suspended: the login of
/// [`run_login`] against the session's `servers`. The terminal reader
/// stops as soon as the login ends, so kage takes the terminal back
/// without waiting for a key press. Ctrl+C cancels the login instead of
/// ending kage.
pub(crate) fn tui_login(name: &str, servers: &BTreeMap<String, McpServer>) -> Result<(), String> {
    let path = McpAuthStore::default_path()?;
    let cancel = Arc::new(AtomicBool::new(false));
    let sigint = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancel)).ok();
    let stop = AtomicBool::new(false);
    let result = thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let stop = &stop;
        scope.spawn(move || kage_tui::terminal::forward_typed_lines(&tx, stop));
        let result = login(name, servers, &path, &rx, &open_browser, &cancel);
        stop.store(true, Ordering::SeqCst);
        result
    });
    if let Some(id) = sigint {
        signal_hook::low_level::unregister(id);
    }
    if let Err(e) = &result {
        eprintln!("kage: mcp login {name}: {e}");
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufReader, Read};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    use kage_core::config::McpOAuth;
    use tempfile::tempdir;

    use super::*;

    /// One request the fake server answered.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        body: String,
    }

    impl Seen {
        fn form(&self) -> HashMap<String, String> {
            url_form(&self.body)
        }
    }

    fn url_form(query: &str) -> HashMap<String, String> {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), decode(v)))
            .collect()
    }

    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// A resource server and authorization server on 127.0.0.1: `/mcp`
    /// challenges, discovery documents and `/register` answer, and
    /// `/token` issues `access-<n>` and `refresh-<n>` for the n-th
    /// token request.
    struct FakeServer {
        base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl FakeServer {
        fn start() -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (log, origin) = (Arc::clone(&seen), base.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    answer(&stream, &origin, &log);
                }
            });
            Self { base, seen }
        }

        fn tokens_issued(&self) -> Vec<Seen> {
            let seen = lock(&self.seen);
            seen.iter()
                .filter(|s| s.path == "/token")
                .cloned()
                .collect()
        }
    }

    fn answer(mut stream: &TcpStream, base: &str, log: &Mutex<Vec<Seen>>) {
        let Some(request) = read_request(stream) else {
            return;
        };
        let issued = {
            let mut log = lock(log);
            log.push(request.clone());
            log.iter().filter(|s| s.path == "/token").count()
        };
        let (status, extra, body) = match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/mcp") => (
                401,
                format!("www-authenticate: Bearer resource_metadata=\"{base}/prm\"\r\n"),
                String::new(),
            ),
            ("GET", "/prm") => (
                200,
                String::new(),
                serde_json::json!({
                    "resource": format!("{base}/mcp"),
                    "authorization_servers": [base],
                    "scopes_supported": ["read"],
                })
                .to_string(),
            ),
            ("GET", "/.well-known/oauth-authorization-server") => (
                200,
                String::new(),
                serde_json::json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "registration_endpoint": format!("{base}/register"),
                    "code_challenge_methods_supported": ["S256"],
                })
                .to_string(),
            ),
            ("POST", "/register") => (
                201,
                String::new(),
                serde_json::json!({ "client_id": "kage-client" }).to_string(),
            ),
            ("POST", "/token") => (
                200,
                String::new(),
                serde_json::json!({
                    "access_token": format!("access-{issued}"),
                    "refresh_token": format!("refresh-{issued}"),
                    "token_type": "Bearer",
                    "expires_in": 3600,
                })
                .to_string(),
            ),
            _ => (404, String::new(), String::new()),
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}\
             content-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    }

    fn read_request(stream: &TcpStream) -> Option<Seen> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).ok()?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().ok()?;
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).ok()?;
        Some(Seen {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn entry(base: &str, expires_in: i64) -> McpAuthEntry {
        McpAuthEntry {
            issuer: base.to_owned(),
            token_endpoint: format!("{base}/token"),
            client_id: "kage-client".to_owned(),
            resource: format!("{base}/mcp"),
            token: OAuthCredential {
                access_token: "access-old".to_owned(),
                refresh_token: Some("refresh-old".to_owned()),
                expires_at: Some(Utc::now() + chrono::Duration::seconds(expires_in)),
                scope: None,
            },
        }
    }

    fn store_with(path: &Path, entries: &[(&str, McpAuthEntry)]) {
        let mut store = McpAuthStore::default();
        for (key, entry) in entries {
            store.servers.insert((*key).to_owned(), entry.clone());
        }
        store.save_to(path).unwrap();
    }

    fn server(command: Option<&str>, url: Option<&str>) -> McpServer {
        McpServer {
            command: command.map(str::to_owned),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: url.map(str::to_owned),
            headers: BTreeMap::new(),
            disabled: false,
            oauth: None,
        }
    }

    fn http_server(url: &str) -> McpServer {
        server(None, Some(url))
    }

    #[test]
    fn stored_tokens_are_matched_by_canonical_url() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        store_with(
            &path,
            &[(
                "https://mcp.example.com/mcp",
                entry("https://auth.example.com", 3600),
            )],
        );
        let tokens = McpTokens::new(path);
        assert_eq!(
            tokens.bearer("https://MCP.example.com/mcp/").as_deref(),
            Some("access-old")
        );
        assert_eq!(tokens.bearer("https://other.example.com/mcp"), None);
        assert_eq!(tokens.bearer("https://mcp.example.com/other"), None);
    }

    #[test]
    fn a_token_near_expiry_is_refreshed_and_the_rotation_saved() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let key = format!("{}/mcp", server.base);
        store_with(&path, &[(&key, entry(&server.base, 10))]);
        let tokens = McpTokens::new(path.clone());

        assert_eq!(tokens.bearer(&key).as_deref(), Some("access-1"));
        let issued = server.tokens_issued();
        assert_eq!(issued.len(), 1);
        let form = issued[0].form();
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "refresh-old");
        assert_eq!(form["resource"], key);

        let saved = McpAuthStore::load_from(&path).unwrap();
        let token = &saved.servers[&key].token;
        assert_eq!(token.refresh_token.as_deref(), Some("refresh-1"));
        assert!(!token.expires_within(REFRESH_SLACK, Utc::now()));
        assert_eq!(tokens.bearer(&key).as_deref(), Some("access-1"));
        assert_eq!(server.tokens_issued().len(), 1);
    }

    #[test]
    fn a_rejected_token_is_refreshed_once() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let key = format!("{}/mcp", server.base);
        store_with(&path, &[(&key, entry(&server.base, 3600))]);
        let tokens = McpTokens::new(path);

        assert_eq!(tokens.bearer(&key).as_deref(), Some("access-old"));
        assert!(server.tokens_issued().is_empty());
        assert_eq!(tokens.rejected(&key).as_deref(), Some("access-1"));
        assert_eq!(server.tokens_issued().len(), 1);
        assert_eq!(tokens.bearer(&key).as_deref(), Some("access-1"));
        assert_eq!(tokens.rejected("https://unknown.example.com/mcp"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_logout_forgets_one_entry() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().unwrap();
        let path = dir.path().join("kage").join("mcp-auth.json");
        store_with(
            &path,
            &[
                (
                    "https://a.example.com/mcp",
                    entry("https://a.example.com", 3600),
                ),
                (
                    "https://b.example.com/mcp",
                    entry("https://b.example.com", 3600),
                ),
            ],
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        assert!(forget(&path, "https://A.example.com/mcp/").unwrap());
        assert!(!forget(&path, "https://a.example.com/mcp").unwrap());
        let left = McpAuthStore::load_from(&path).unwrap();
        let keys: Vec<&str> = left.servers.keys().map(String::as_str).collect();
        assert_eq!(keys, ["https://b.example.com/mcp"]);
    }

    #[test]
    fn login_with_a_pasted_redirect_stores_the_tokens() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let url = format!("{}/mcp", server.base);
        let servers = BTreeMap::from([("remote".to_owned(), http_server(&url))]);
        let (tx, rx) = mpsc::channel();
        let opened = Mutex::new(String::new());
        let open = |authorize: &str| {
            let query = authorize.split_once('?').unwrap().1;
            let params = url_form(query);
            tx.send(String::new()).unwrap();
            tx.send(format!(
                "{}?code=the-code&state={}",
                params["redirect_uri"], params["state"]
            ))
            .unwrap();
            authorize.clone_into(&mut lock(&opened));
        };

        login(
            "remote",
            &servers,
            &path,
            &rx,
            &open,
            &AtomicBool::new(false),
        )
        .unwrap();

        let authorize = lock(&opened).clone();
        let params = url_form(authorize.split_once('?').unwrap().1);
        assert_eq!(params["client_id"], "kage-client");
        assert_eq!(params["resource"], url);
        assert_eq!(params["scope"], "read");
        let issued = server.tokens_issued();
        assert_eq!(issued.len(), 1);
        let form = issued[0].form();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "the-code");
        assert_eq!(form["redirect_uri"], params["redirect_uri"]);

        let saved = McpAuthStore::load_from(&path).unwrap();
        let stored = &saved.servers[&url];
        assert_eq!(stored.client_id, "kage-client");
        assert_eq!(stored.token_endpoint, format!("{}/token", server.base));
        assert_eq!(stored.token.access_token, "access-1");
        assert_eq!(
            McpTokens::new(path).bearer(&url).as_deref(),
            Some("access-1")
        );
    }

    #[test]
    fn a_second_login_reuses_the_stored_client() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let url = format!("{}/mcp", server.base);
        let servers = BTreeMap::from([("remote".to_owned(), http_server(&url))]);
        let (tx, rx) = mpsc::channel();
        let open = |authorize: &str| {
            let params = url_form(authorize.split_once('?').unwrap().1);
            assert_eq!(params["client_id"], "kage-client");
            tx.send(format!(
                "http://127.0.0.1/callback?code=c&state={}",
                params["state"]
            ))
            .unwrap();
        };
        login(
            "remote",
            &servers,
            &path,
            &rx,
            &open,
            &AtomicBool::new(false),
        )
        .unwrap();
        login(
            "remote",
            &servers,
            &path,
            &rx,
            &open,
            &AtomicBool::new(false),
        )
        .unwrap();
        let registered = lock(&server.seen)
            .iter()
            .filter(|s| s.path == "/register")
            .count();
        assert_eq!(registered, 1);
        assert_eq!(server.tokens_issued().len(), 2);

        let mut store = McpAuthStore::load_from(&path).unwrap();
        store.servers.get_mut(&url).unwrap().issuer = "https://other.example.com".to_owned();
        store.save_to(&path).unwrap();
        login(
            "remote",
            &servers,
            &path,
            &rx,
            &open,
            &AtomicBool::new(false),
        )
        .unwrap();
        let registered = lock(&server.seen)
            .iter()
            .filter(|s| s.path == "/register")
            .count();
        assert_eq!(registered, 2, "another issuer's client is not reused");
    }

    #[test]
    fn login_uses_the_configured_client_and_scope() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let url = format!("{}/mcp", server.base);
        let mut spec = http_server(&url);
        spec.oauth = Some(McpOAuth {
            client_id: Some("pre-registered".to_owned()),
            scope: Some("read write".to_owned()),
        });
        let servers = BTreeMap::from([("remote".to_owned(), spec)]);
        let (tx, rx) = mpsc::channel();
        let open = |authorize: &str| {
            let params = url_form(authorize.split_once('?').unwrap().1);
            assert_eq!(params["client_id"], "pre-registered");
            assert_eq!(params["scope"], "read write");
            tx.send(format!(
                "http://127.0.0.1/callback?code=c&state={}",
                params["state"]
            ))
            .unwrap();
        };
        login(
            "remote",
            &servers,
            &path,
            &rx,
            &open,
            &AtomicBool::new(false),
        )
        .unwrap();
        let seen = lock(&server.seen);
        assert!(!seen.iter().any(|s| s.path == "/register"));
    }

    #[test]
    fn login_refuses_stdio_and_unknown_servers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let servers = BTreeMap::from([("local".to_owned(), server(Some("server"), None))]);
        let (_tx, rx) = mpsc::channel();
        let err = login(
            "local",
            &servers,
            &path,
            &rx,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("stdio server"), "{err}");
        let err = login(
            "nope",
            &servers,
            &path,
            &rx,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("no MCP server named `nope`"), "{err}");
        assert!(!path.exists());
    }

    #[test]
    fn a_cancelled_login_stores_nothing() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let url = format!("{}/mcp", server.base);
        let servers = BTreeMap::from([("remote".to_owned(), http_server(&url))]);
        let (_tx, rx) = mpsc::channel();
        let cancel = AtomicBool::new(true);
        let err = login("remote", &servers, &path, &rx, &|_| {}, &cancel).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(server.tokens_issued().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn a_pasted_redirect_with_the_wrong_state_fails_without_secrets() {
        let server = FakeServer::start();
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let url = format!("{}/mcp", server.base);
        let servers = BTreeMap::from([("remote".to_owned(), http_server(&url))]);
        let (tx, rx) = mpsc::channel();
        let open = |_: &str| {
            tx.send("http://127.0.0.1/callback?code=secret-code&state=forged".to_owned())
                .unwrap();
        };
        let err = login(
            "remote",
            &servers,
            &path,
            &rx,
            &open,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("wrong state"), "{err}");
        assert!(!err.contains("secret-code"), "{err}");
        assert!(server.tokens_issued().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn entry_debug_output_redacts_the_tokens() {
        let entry = entry("https://auth.example.com", 3600);
        let debug = format!("{entry:?}");
        assert!(debug.contains("kage-client"), "{debug}");
        assert!(!debug.contains("access-old"), "{debug}");
        assert!(!debug.contains("refresh-old"), "{debug}");
    }
}
