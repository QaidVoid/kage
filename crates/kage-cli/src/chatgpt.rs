//! `ChatGPT` sign-in for the `OpenAI` provider: the browser OAuth flow the
//! Codex CLI uses, pointed at the same public client.
//!
//! `kage auth login openai` offers this instead of an API key. The
//! browser authorizes kage against `auth.openai.com` (PKCE S256, a
//! random `state`), the code comes back to a one-shot listener on
//! `127.0.0.1:1455` (paste the redirect URL when no browser can reach
//! this machine), and the tokens land in `auth.json` as an OAuth
//! credential carrying the `chatgpt_account_id` parsed from the
//! `id_token`. The provider registry then routes `openai-responses`
//! models through the `ChatGPT` backend at
//! `https://chatgpt.com/backend-api/codex`.
//!
//! Access tokens expire within hours, so two paths renew them:
//! [`ensure_fresh`] at registry build, and the provider's 401 handler
//! through [`ChatGptBearer`]. Both persist the renewal.
//!
//! Every constant here matches the flow Codex CLI performs; they are
//! collected at the top so a backend change is a one-place edit.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::{AuthStore, OAuthCredential};

/// The public OAuth client id of the Codex CLI, reused for `ChatGPT`
/// sign-in. Not a secret: it rides every authorize URL.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The issuer of `ChatGPT` OAuth tokens.
pub const ISSUER: &str = "https://auth.openai.com";

/// The Responses API of the `ChatGPT` backend that a `ChatGPT` login can
/// call. Requests append `/responses`.
pub const BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Scopes of the login. `offline_access` is what makes the refresh
/// token exist.
pub const SCOPES: &str = "openid profile email offline_access";

/// The loopback port the Codex CLI registered its redirect URI on.
/// The auth server only redirects there (or to [`CALLBACK_PORT_FALLBACK`]).
pub const CALLBACK_PORT: u16 = 1455;

/// The fallback port when 1455 is taken, as the Codex CLI falls back.
pub const CALLBACK_PORT_FALLBACK: u16 = 1457;

/// The redirect path of the registered callback URIs.
const CALLBACK_PATH: &str = "/auth/callback";

/// How long a login waits for the browser to come back.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// The `originator` header and authorize parameter the backend expects
/// from first-party CLI clients.
pub const ORIGINATOR: &str = "codex_cli_rs";

/// The `OpenAI-Beta` value the `ChatGPT` Responses endpoint routes on.
const OPENAI_BETA: &str = "responses=experimental";

/// Refresh at registry build when the stored token expires within this
/// window, the window the Codex CLI renews in. Renewing earlier only
/// spends refresh-token rotations.
const REFRESH_SLACK: chrono::Duration = chrono::Duration::minutes(5);

/// Which provider id the `ChatGPT` credential is stored under.
pub const PROVIDER_ID: &str = "openai";

/// Cap on a token endpoint answer body.
const MAX_BODY: u64 = 1024 * 1024;

/// Cap on the request head a browser sends the callback listener.
const MAX_REQUEST_HEAD: u64 = 16 * 1024;

/// How long the loopback waits for a connected browser to send its
/// request before moving on.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the wait checks its deadline and cancel flag.
const POLL: Duration = Duration::from_millis(25);

/// What `Debug` prints in place of a secret.
const REDACTED: &str = "***";

/// A failed login or refresh. Messages never carry a token.
#[derive(Debug, thiserror::Error)]
pub enum ChatGptError {
    /// The local listener or the random source failed.
    #[error("{0}")]
    Local(String),
    /// The browser did not come back in time.
    #[error("timed out waiting for the browser to return")]
    Timeout,
    /// The wait was stopped by Ctrl+C.
    #[error("the login was cancelled")]
    Cancelled,
    /// The redirect carried another `state` than this login sent.
    #[error("the authorization redirect carried the wrong state. Start the login again")]
    State,
    /// The authorization server refused.
    #[error("the authorization server refused: {0}")]
    Denied(String),
    /// A token answer lacks a required field or a token is malformed.
    #[error("{0}")]
    Response(String),
    /// A request could not be sent or its answer not read.
    #[error("{0}")]
    Http(String),
}

/// A PKCE verifier and its S256 challenge. `Debug` redacts the
/// verifier.
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// A fresh verifier of 43 unreserved characters (32 random bytes)
    /// and its challenge, `BASE64URL(SHA256(verifier))`.
    fn new() -> Result<Self, ChatGptError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| ChatGptError::Local(format!("random source: {e}")))?;
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &REDACTED)
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// The URL the browser opens: the `ChatGPT` authorize endpoint with the
/// S256 challenge, a fresh `state`, and the parameters the Codex CLI
/// sends so the account picker behaves the same.
fn authorize_url(pkce: &Pkce, state: &str, redirect_uri: &str) -> String {
    let mut url = url::Url::parse(&format!("{ISSUER}/oauth/authorize")).expect("constant URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("scope", SCOPES)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);
    }
    url.into()
}

/// Fresh 32 random bytes as an unpadded base64url `state`.
fn random_state() -> Result<String, ChatGptError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| ChatGptError::Local(format!("random source: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The redirect URI for `port`.
fn redirect_uri_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

/// The claims of a JWT payload: base64url-decoded JSON. The signature
/// is not checked, matching the Codex CLI: the token arrived over TLS
/// straight from the authorization server.
fn jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The `ChatGPT` workspace/account id inside an `id_token` or access
/// token, under the `https://api.openai.com/auth` claim.
fn account_id_from_jwt(jwt: &str) -> Option<String> {
    let claims = jwt_claims(jwt)?;
    let auth = claims.get("https://api.openai.com/auth")?;
    auth.get("chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// When a JWT access token stops working, from its `exp` claim.
fn expiry_from_jwt(jwt: &str) -> Option<DateTime<Utc>> {
    let exp = jwt_claims(jwt)?.get("exp").and_then(Value::as_i64)?;
    DateTime::from_timestamp(exp, 0)
}

/// The raw token-endpoint answer, before it becomes a credential.
#[derive(Clone, Default)]
struct TokenAnswer {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

impl fmt::Debug for TokenAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenAnswer")
            .field("access_token", &REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| REDACTED))
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// An agent for the token endpoint: statuses are read rather than
/// raised, and redirects are not followed.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// An OAuth error code reduced to the characters codes use, so a
/// hostile answer or redirect cannot inject text into the terminal.
fn error_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect()
}

/// Parse a token-endpoint answer body.
fn parse_answer(status: u16, body: &str) -> Result<TokenAnswer, ChatGptError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ChatGptError::Response("the token answer is not JSON".to_owned()))?;
    if !(200..300).contains(&status) {
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .map_or_else(|| "no error code".to_owned(), error_code);
        return Err(ChatGptError::Denied(format!("status {status}: {code}")));
    }
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Ok(TokenAnswer {
        access_token: text("access_token").ok_or_else(|| {
            ChatGptError::Response("the token answer has no access_token".to_owned())
        })?,
        refresh_token: text("refresh_token"),
        id_token: text("id_token"),
        expires_in: value.get("expires_in").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }),
        scope: text("scope"),
    })
}

/// POST the authorization-code grant (form-encoded, as the endpoint
/// requires) and parse the answer.
fn exchange_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenAnswer, ChatGptError> {
    let endpoint = format!("{ISSUER}/oauth/token");
    let form: &[(&str, &str)] = &[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let mut response = agent()
        .post(&endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .send_form(form.iter().copied())
        .map_err(|e| ChatGptError::Http(format!("token request: {e}")))?;
    let body = read_body(&mut response);
    parse_answer(response.status().as_u16(), &body)
}

/// POST the refresh-token grant. `ChatGPT` renewal is JSON-encoded, and
/// an answer without a new refresh token keeps the old one.
fn refresh_tokens(refresh_token: &str) -> Result<TokenAnswer, ChatGptError> {
    let endpoint = format!("{ISSUER}/oauth/token");
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    let mut response = agent()
        .post(&endpoint)
        .header("content-type", "application/json")
        .send_json(&body)
        .map_err(|e| ChatGptError::Http(format!("token renewal: {e}")))?;
    let text = read_body(&mut response);
    let mut answer = parse_answer(response.status().as_u16(), &text)?;
    if answer.refresh_token.is_none() {
        answer.refresh_token = Some(refresh_token.to_owned());
    }
    Ok(answer)
}

/// Read a response body, capped.
fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> String {
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .unwrap_or_default()
}

/// Build the stored credential from a token answer. Expiry prefers the
/// `expires_in` the server stated, falling back to the access token's
/// own `exp` claim. The account id comes from the `id_token`, else the
/// access token, else the previous credential.
fn credential_from(answer: &TokenAnswer, previous: Option<&OAuthCredential>) -> OAuthCredential {
    let expires_at = answer
        .expires_in
        .and_then(|secs| chrono::Duration::try_seconds(i64::try_from(secs).ok()?))
        .and_then(|d| Utc::now().checked_add_signed(d))
        .or_else(|| expiry_from_jwt(&answer.access_token));
    let account_id = answer
        .id_token
        .as_deref()
        .and_then(account_id_from_jwt)
        .or_else(|| account_id_from_jwt(&answer.access_token))
        .or_else(|| previous.and_then(|p| p.account_id.clone()));
    OAuthCredential {
        access_token: answer.access_token.clone(),
        refresh_token: answer
            .refresh_token
            .clone()
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at,
        scope: Some(answer.scope.clone().unwrap_or_else(|| SCOPES.to_owned())),
        account_id,
    }
}

/// A one-shot redirect listener on `127.0.0.1` for the browser to call
/// back. Answers the callback path, 404 for anything else.
struct Callback {
    listener: TcpListener,
    port: u16,
}

impl Callback {
    /// Bind the registered port, falling back like the Codex CLI does.
    fn bind() -> Result<Self, ChatGptError> {
        Self::bind_on(&[CALLBACK_PORT, CALLBACK_PORT_FALLBACK]).ok_or_else(|| {
            ChatGptError::Local(format!(
                "callback listener: ports {CALLBACK_PORT} and {CALLBACK_PORT_FALLBACK} are \
                 both in use; free one and start the login again"
            ))
        })
    }

    /// Bind the first free port of `ports`. The standard listener sets
    /// `SO_REUSEADDR` on unix, so a second login rebinds while the last
    /// one's connections sit in `TIME_WAIT`.
    fn bind_on(ports: &[u16]) -> Option<Self> {
        ports.iter().find_map(|&port| {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok()?;
            let port = listener.local_addr().ok()?.port();
            Some(Self { listener, port })
        })
    }

    /// The redirect URI this listener answers.
    fn redirect_uri(&self) -> String {
        redirect_uri_for(self.port)
    }

    /// Serve until a callback for `state` arrives, `cancel` is set, or
    /// `timeout` passes.
    fn wait(
        self,
        state: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, ChatGptError> {
        let local = |e| ChatGptError::Local(format!("callback listener: {e}"));
        self.listener.set_nonblocking(true).map_err(local)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(ChatGptError::Cancelled);
            }
            if std::time::Instant::now() >= deadline {
                return Err(ChatGptError::Timeout);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(result) = serve(&stream, state) {
                        return result;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => return Err(local(e)),
            }
        }
    }
}

/// Answer one browser connection. `None` means it was not the callback
/// (another path, or no readable request), so the wait goes on.
fn serve(mut stream: &TcpStream, state: &str) -> Option<Result<String, ChatGptError>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    let target = read_target(stream)?;
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path != CALLBACK_PATH {
        respond(&mut stream, "404 Not Found", "Not found.");
        return None;
    }
    let result = accept(query, state);
    if result.is_ok() {
        respond(
            &mut stream,
            "200 OK",
            "kage is authorized. You can close this tab.",
        );
    } else {
        respond(
            &mut stream,
            "400 Bad Request",
            "kage could not finish the login. Return to the terminal.",
        );
    }
    Some(result)
}

/// Read the request line and the headers, returning the request target.
/// The headers are drained so closing the socket does not reset the
/// connection before the browser reads the answer.
fn read_target(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_HEAD));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_owned();
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header.trim_end().is_empty() => break,
            Ok(_) => {}
        }
    }
    Some(target)
}

/// Check the query of a redirect against this login's `state` and
/// return its code.
fn accept(query: &str, state: &str) -> Result<String, ChatGptError> {
    let (mut code, mut carried, mut error) = (None, None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => carried = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if carried.as_deref() != Some(state) {
        return Err(ChatGptError::State);
    }
    if let Some(error) = error {
        return Err(ChatGptError::Denied(error_code(&error)));
    }
    code.filter(|code| !code.is_empty())
        .ok_or_else(|| ChatGptError::Response("the redirect carries no code".to_owned()))
}

/// Read the code from a redirect URL the user pasted, for hosts whose
/// browser cannot reach the loopback listener (SSH without forwarding).
fn parse_redirect(pasted: &str, state: &str) -> Result<String, ChatGptError> {
    let url = url::Url::parse(pasted.trim())
        .map_err(|_| ChatGptError::Response("the pasted text is not a URL".to_owned()))?;
    accept(url.query().unwrap_or_default(), state)
}

/// Write a small HTML page and close.
fn respond(stream: &mut &TcpStream, status: &str, message: &str) {
    let body = format!("<!doctype html><title>kage</title><p>{message}</p>\n");
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Run the whole login: authorize in the browser, wait for the
/// callback or a pasted redirect URL, trade the code for tokens, and
/// return the credential to store. The signal hook routes Ctrl+C into
/// `cancel` for the length of the login, so an interrupted login does
/// not kill the host process. The terminal reader stops when the login
/// ends, so a TUI that suspended itself for `:login` gets its input
/// back.
///
/// # Errors
///
/// A message for every way a login can fail, ready for a `kage: ` line.
pub fn login(cancel: &Arc<AtomicBool>) -> Result<OAuthCredential, String> {
    let sigint = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(cancel)).ok();
    let stop = AtomicBool::new(false);
    let result = thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let stop = &stop;
        scope.spawn(move || kage_tui::terminal::forward_typed_lines(&tx, stop));
        let result = login_inner(&rx, cancel).map_err(|e| e.to_string());
        stop.store(true, Ordering::SeqCst);
        result
    });
    if let Some(id) = sigint {
        signal_hook::low_level::unregister(id);
    }
    result
}

/// The login steps, with the typed error kept for tests.
fn login_inner(
    pasted: &Receiver<String>,
    cancel: &AtomicBool,
) -> Result<OAuthCredential, ChatGptError> {
    let pkce = Pkce::new()?;
    let state = random_state()?;
    let callback = Callback::bind()?;
    let redirect = callback.redirect_uri();
    let url = authorize_url(&pkce, &state, &redirect);
    eprintln!("Sign in with ChatGPT. Open this URL in your browser:\n\n{url}\n");
    if crate::opener::open(std::ffi::OsStr::new(&url)).is_err() {
        eprintln!("kage: no browser detected; open the URL above manually");
    }
    eprintln!(
        "Waiting for the browser to come back ({} minutes, Ctrl+C to cancel).",
        LOGIN_TIMEOUT.as_secs() / 60
    );
    eprintln!("On another machine? Paste the URL your browser was sent to and press Enter:");
    eprint!("> ");
    let _ = io::stderr().flush();
    let code = wait_for_code(callback, &state, LOGIN_TIMEOUT, pasted, cancel)?;
    let answer = exchange_code(&code, &pkce.verifier, &redirect)?;
    let credential = credential_from(&answer, None);
    if credential.account_id.is_none() {
        eprintln!("kage: the login carries no ChatGPT account id; requests may be refused");
    }
    Ok(credential)
}

/// The first of the browser's redirect and a pasted one, or
/// [`ChatGptError::Cancelled`] once `cancel` is set. Blank lines are
/// ignored, and a closed input leaves only the browser. When the
/// browser wins or the wait is cancelled, the prompt line is ended.
fn wait_for_code(
    callback: Callback,
    state: &str,
    timeout: Duration,
    pasted: &Receiver<String>,
    cancel: &AtomicBool,
) -> Result<String, ChatGptError> {
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let stop = &stop;
        let browser = scope.spawn(move || callback.wait(state, timeout, stop));
        let stopped = || ChatGptError::Local("the callback listener stopped".to_owned());
        loop {
            if browser.is_finished() {
                eprintln!();
                return browser.join().unwrap_or_else(|_| Err(stopped()));
            }
            if cancel.load(Ordering::SeqCst) {
                stop.store(true, Ordering::SeqCst);
                let _ = browser.join();
                eprintln!();
                return Err(ChatGptError::Cancelled);
            }
            match pasted.recv_timeout(POLL) {
                Ok(line) if line.trim().is_empty() => {}
                Ok(line) => {
                    stop.store(true, Ordering::SeqCst);
                    let _ = browser.join();
                    return parse_redirect(&line, state);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => thread::sleep(POLL),
            }
        }
    })
}

/// Renew `credential` at the token endpoint. The refresh token is kept
/// when the answer does not rotate it; the account id never changes.
///
/// # Errors
///
/// When the endpoint refuses or the answer is unusable; the caller
/// should ask for a fresh login.
pub fn refresh_credential(credential: &OAuthCredential) -> Result<OAuthCredential, String> {
    let Some(refresh_token) = credential.refresh_token.as_deref() else {
        return Err(
            "the ChatGPT login has no refresh token; run `kage auth login openai`".to_owned(),
        );
    };
    let answer = refresh_tokens(refresh_token).map_err(|e| e.to_string())?;
    Ok(credential_from(&answer, Some(credential)))
}

/// Refresh the stored `ChatGPT` credential when it expires within
/// [`REFRESH_SLACK`], persisting the renewal. A no-op for any other
/// credential shape, and silent: a failed renewal only fails when the
/// caller needs a token, which is the 401 path's job.
///
/// # Errors
///
/// When a due refresh cannot be persisted after succeeding; the next
/// login overwrites the file, but the user should know the renewal was
/// not saved.
pub fn ensure_fresh(store: &mut AuthStore, now: DateTime<Utc>) -> Result<(), String> {
    let Some(crate::auth::Credential::Oauth(oauth)) = store.credential(PROVIDER_ID) else {
        return Ok(());
    };
    if !oauth.expires_within(REFRESH_SLACK, now) {
        return Ok(());
    }
    let renewed = refresh_credential(oauth)?;
    store.set(PROVIDER_ID, crate::auth::Credential::Oauth(renewed));
    store.save()
}

/// Headers every ChatGPT-backend request carries beside the bearer:
/// the experimental Responses opt-in, the originator the public client
/// is known by, and the account id when the `id_token` carried one.
#[must_use]
pub fn backend_headers(account_id: Option<&str>) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("openai-beta".to_owned(), OPENAI_BETA.to_owned());
    headers.insert("originator".to_owned(), ORIGINATOR.to_owned());
    if let Some(id) = account_id {
        headers.insert("chatgpt-account-id".to_owned(), id.to_owned());
    }
    headers
}

/// The provider-side renewable credential: hands the current token to
/// every request and renews through the auth store at `path` on a 401.
/// Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct ChatGptBearer {
    /// The token of the moment. Held across a renewal, so concurrent
    /// rejections renew once.
    token: Arc<Mutex<String>>,
    path: PathBuf,
}

impl ChatGptBearer {
    /// A bearer source starting from `credential`, usually the one
    /// `providers.rs` just read from the auth store at `path`.
    #[must_use]
    pub fn new(credential: &OAuthCredential, path: PathBuf) -> Self {
        Self {
            token: Arc::new(Mutex::new(credential.access_token.clone())),
            path,
        }
    }
}

impl fmt::Debug for ChatGptBearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptBearer")
            .field("token", &REDACTED)
            .field("path", &self.path)
            .finish()
    }
}

impl kage_provider::BearerSource for ChatGptBearer {
    fn token(&self) -> String {
        self.token
            .lock()
            .map_or_else(|_| String::new(), |token| token.clone())
    }

    fn refresh(&self, rejected: &str) -> Result<String, kage_provider::ProviderError> {
        let mut current = self
            .token
            .lock()
            .map_err(|_| renewal_failed("the token lock is poisoned"))?;
        let mut store = AuthStore::load_from(&self.path).map_err(|e| renewal_failed(&e))?;
        let Some(crate::auth::Credential::Oauth(oauth)) = store.credential(PROVIDER_ID) else {
            return Err(kage_provider::ProviderError::Auth(
                "no ChatGPT login stored; run `kage auth login openai`".to_owned(),
            ));
        };
        // Single-flight: another request or process may have renewed
        // while the rejected token was in flight. Renewing again would
        // reuse a rotated refresh token, which the server refuses.
        if oauth.access_token != rejected {
            current.clone_from(&oauth.access_token);
            return Ok(oauth.access_token.clone());
        }
        let renewed = refresh_credential(oauth).map_err(|e| renewal_failed(&e))?;
        let token = renewed.access_token.clone();
        store.set(PROVIDER_ID, crate::auth::Credential::Oauth(renewed));
        store.save_to(&self.path).map_err(|e| renewal_failed(&e))?;
        current.clone_from(&token);
        Ok(token)
    }
}

/// The renewal error message, with the actionable suffix.
fn renewal_failed(e: &str) -> kage_provider::ProviderError {
    kage_provider::ProviderError::Auth(format!(
        "renewing the ChatGPT login failed: {e}. Run `kage auth login openai`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JWT with the claims of `json`, signed by nothing (the parser
    /// ignores signatures).
    fn jwt(json: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(json.to_string());
        format!("{header}.{payload}.c2ln")
    }

    #[test]
    fn the_account_id_comes_from_the_auth_claim() {
        let id_token = jwt(&serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-123",
                "chatgpt_plan_type": "plus",
            },
        }));
        assert_eq!(account_id_from_jwt(&id_token).as_deref(), Some("acc-123"));
        assert_eq!(account_id_from_jwt("not-a-jwt"), None);
        assert_eq!(account_id_from_jwt(""), None);
        let without = jwt(&serde_json::json!({ "email": "dev@example.com" }));
        assert_eq!(account_id_from_jwt(&without), None);
    }

    #[test]
    fn the_expiry_comes_from_the_exp_claim() {
        let token = jwt(&serde_json::json!({ "exp": 1_800_000_000i64 }));
        let at = expiry_from_jwt(&token).unwrap();
        assert_eq!(at.timestamp(), 1_800_000_000);
        assert_eq!(expiry_from_jwt("x.y.z"), None);
    }

    #[test]
    fn the_authorize_url_carries_every_codex_parameter() {
        let pkce = Pkce::new().unwrap();
        let redirect = redirect_uri_for(CALLBACK_PORT);
        let url = url::Url::parse(&authorize_url(&pkce, "state-1", &redirect)).unwrap();
        assert!(
            url.as_str()
                .starts_with("https://auth.openai.com/oauth/authorize?"),
            "{url}"
        );
        let query: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["scope"], SCOPES);
        assert_eq!(query["state"], "state-1");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"], pkce.challenge);
        assert_eq!(query["redirect_uri"], redirect);
        assert_eq!(query["codex_cli_simplified_flow"], "true");
        assert_eq!(query["originator"], ORIGINATOR);
        assert_eq!(query["id_token_add_organizations"], "true");
        assert!(!query.contains_key("resource"), "no RFC 8707 resource");
        assert!(!format!("{pkce:?}").contains(&pkce.verifier));
    }

    #[test]
    fn a_token_answer_becomes_a_stored_credential() {
        let answer = TokenAnswer {
            access_token: jwt(&serde_json::json!({ "exp": 1_800_000_000i64 })).clone(),
            refresh_token: Some("refresh-1".to_owned()),
            id_token: Some(
                jwt(&serde_json::json!({
                    "https://api.openai.com/auth": { "chatgpt_account_id": "acc-9" },
                }))
                .clone(),
            ),
            expires_in: Some(3600),
            scope: Some("openid".to_owned()),
        };
        let credential = credential_from(&answer, None);
        assert_eq!(credential.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(credential.account_id.as_deref(), Some("acc-9"));
        assert!(credential.expires_at.is_some());
        assert_eq!(credential.scope.as_deref(), Some("openid"));

        let no_expiry = TokenAnswer {
            expires_in: None,
            access_token: answer.access_token.clone(),
            ..TokenAnswer::default()
        };
        let credential = credential_from(&no_expiry, None);
        assert_eq!(
            credential.expires_at.map(|t| t.timestamp()),
            Some(1_800_000_000),
            "expiry falls back to the token's own exp"
        );
        assert_eq!(credential.scope.as_deref(), Some(SCOPES));
    }

    #[test]
    fn a_refresh_keeps_the_old_refresh_token_and_account_id() {
        let previous = OAuthCredential {
            access_token: "old".to_owned(),
            refresh_token: Some("keep-me".to_owned()),
            expires_at: None,
            scope: None,
            account_id: Some("acc-1".to_owned()),
        };
        let answer = TokenAnswer {
            access_token: "new".to_owned(),
            refresh_token: None,
            id_token: None,
            expires_in: Some(60),
            scope: None,
        };
        let renewed = credential_from(&answer, Some(&previous));
        assert_eq!(renewed.access_token, "new");
        assert_eq!(renewed.refresh_token.as_deref(), Some("keep-me"));
        assert_eq!(renewed.account_id.as_deref(), Some("acc-1"));
    }

    #[test]
    fn a_refused_token_request_reports_the_oauth_error_without_secrets() {
        let body = r#"{"error":"invalid_grant","error_description":"refresh-secret is bad"}"#;
        let err = parse_answer(400, body).unwrap_err();
        assert!(
            matches!(err, ChatGptError::Denied(ref d) if d.contains("invalid_grant")),
            "{err}"
        );
        assert!(!err.to_string().contains("refresh-secret"), "{err}");
        let err = parse_answer(200, r#"{"access_token":""}"#).unwrap_err();
        assert!(matches!(err, ChatGptError::Response(_)), "{err}");
    }

    /// A listener on a port the system picks, so parallel test
    /// processes never fight over the fixed Codex ports.
    fn ephemeral() -> Callback {
        Callback::bind_on(&[0]).unwrap()
    }

    /// The callback server hands over the code for the right state and
    /// ignores other paths.
    #[test]
    fn the_callback_checks_state_and_hands_over_the_code() {
        let callback = ephemeral();
        let port = callback.port;
        assert_eq!(callback.redirect_uri(), redirect_uri_for(port));
        let browser = std::thread::spawn(move || {
            let get = |target: &str| {
                let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
                write!(
                    stream,
                    "GET {target} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n"
                )
                .unwrap();
                let mut out = String::new();
                let _ = stream.read_to_string(&mut out);
                out
            };
            [
                get("/favicon.ico"),
                get(&format!("{CALLBACK_PATH}?code=the-code&state=s1")),
            ]
        });
        let code = callback
            .wait("s1", Duration::from_secs(10), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(code, "the-code");
        let [favicon, ok] = browser.join().unwrap();
        assert!(favicon.contains("404"), "{favicon}");
        assert!(ok.contains("200"), "{ok}");
    }

    /// A forged state or a provider error ends the wait.
    #[test]
    fn the_callback_refuses_a_forged_state_and_a_provider_error() {
        let callback = ephemeral();
        let port = callback.port;
        let browser = std::thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            write!(
                stream,
                "GET {CALLBACK_PATH}?code=x&state=forged HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n"
            )
            .unwrap();
            let mut out = String::new();
            let _ = stream.read_to_string(&mut out);
            out
        });
        let err = callback
            .wait("s1", Duration::from_secs(10), &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, ChatGptError::State), "{err}");
        assert!(browser.join().unwrap().contains("400"));

        let callback = ephemeral();
        let port = callback.port;
        let browser = std::thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            write!(
                stream,
                "GET {CALLBACK_PATH}?error=access_denied&state=s1 HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n"
            )
            .unwrap();
            let mut out = String::new();
            let _ = stream.read_to_string(&mut out);
            out
        });
        let err = callback
            .wait("s1", Duration::from_secs(10), &AtomicBool::new(false))
            .unwrap_err();
        assert!(
            matches!(&err, ChatGptError::Denied(e) if e == "access_denied"),
            "{err}"
        );
        assert!(browser.join().unwrap().contains("400"));
    }

    #[test]
    fn the_callback_times_out_and_cancels() {
        let err = ephemeral()
            .wait("s1", Duration::from_millis(80), &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, ChatGptError::Timeout), "{err}");
        let err = ephemeral()
            .wait("s1", Duration::from_secs(30), &AtomicBool::new(true))
            .unwrap_err();
        assert!(matches!(err, ChatGptError::Cancelled), "{err}");
    }

    /// A pasted redirect ends the wait while the listener still runs,
    /// and blank lines are skipped.
    #[test]
    fn a_pasted_redirect_wins_over_the_browser() {
        let (tx, rx) = mpsc::channel();
        tx.send(String::new()).unwrap();
        tx.send(format!(
            "{}?code=pasted&state=s1\n",
            redirect_uri_for(CALLBACK_PORT)
        ))
        .unwrap();
        let code = wait_for_code(
            ephemeral(),
            "s1",
            Duration::from_secs(30),
            &rx,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(code, "pasted");

        let err = parse_redirect("not a url", "s1").unwrap_err();
        assert!(matches!(err, ChatGptError::Response(_)), "{err}");
        let err =
            parse_redirect("http://127.0.0.1/auth/callback?code=c&state=s2", "s1").unwrap_err();
        assert!(matches!(err, ChatGptError::State), "{err}");
    }

    /// Ctrl+C ends the wait even with nothing typed.
    #[test]
    fn the_wait_for_a_code_is_cancellable() {
        let (_tx, rx) = mpsc::channel();
        let err = wait_for_code(
            ephemeral(),
            "s1",
            Duration::from_secs(30),
            &rx,
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(matches!(err, ChatGptError::Cancelled), "{err}");
    }

    #[test]
    fn the_account_id_falls_back_to_the_access_token() {
        let answer = TokenAnswer {
            access_token: jwt(&serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "acc-at" },
            })),
            ..TokenAnswer::default()
        };
        assert_eq!(
            credential_from(&answer, None).account_id.as_deref(),
            Some("acc-at")
        );
    }

    /// A rejection of a token another request already replaced adopts
    /// the stored token instead of rotating the refresh token again.
    #[test]
    fn a_stale_rejection_adopts_the_stored_token_without_renewing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::empty();
        store.set_oauth(
            PROVIDER_ID,
            OAuthCredential {
                access_token: "renewed-elsewhere".to_owned(),
                refresh_token: Some("rotated".to_owned()),
                ..OAuthCredential::default()
            },
        );
        store.save_to(&path).unwrap();
        let bearer = ChatGptBearer::new(
            &OAuthCredential {
                access_token: "stale".to_owned(),
                ..OAuthCredential::default()
            },
            path,
        );
        let token = kage_provider::BearerSource::refresh(&bearer, "stale").unwrap();
        assert_eq!(token, "renewed-elsewhere");
        assert_eq!(
            kage_provider::BearerSource::token(&bearer),
            "renewed-elsewhere"
        );
    }

    #[test]
    fn ensure_fresh_leaves_everything_but_a_due_chatgpt_login_alone() {
        let mut store = AuthStore::empty();
        store.set_api_key("openai", "sk-key");
        ensure_fresh(&mut store, Utc::now()).unwrap();
        assert_eq!(store.access_token("openai"), Some("sk-key"));

        let mut store = AuthStore::empty();
        store.set(
            PROVIDER_ID,
            crate::auth::Credential::Oauth(OAuthCredential {
                access_token: "live".to_owned(),
                refresh_token: None,
                expires_at: Some(Utc::now() + chrono::Duration::hours(5)),
                scope: None,
                account_id: None,
            }),
        );
        ensure_fresh(&mut store, Utc::now()).unwrap();
        assert_eq!(store.access_token("openai"), Some("live"));
    }

    #[test]
    fn the_bearer_hands_out_its_token_and_redacts_itself() {
        let credential = OAuthCredential {
            access_token: "at-1".to_owned(),
            ..OAuthCredential::default()
        };
        let bearer = ChatGptBearer::new(&credential, PathBuf::from("auth.json"));
        assert_eq!(kage_provider::BearerSource::token(&bearer), "at-1");
        assert!(format!("{bearer:?}").contains(REDACTED));
        assert!(!format!("{bearer:?}").contains("at-1"));
    }
}
