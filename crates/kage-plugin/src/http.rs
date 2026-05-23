//! `kage.http` - sandboxed HTTP fetch helpers.
//!
//! All requests share the SSRF guards used by the built-in `web_fetch`
//! tool: the URL must be `http(s)`, the host must resolve to a routable
//! address, and the response body is capped to keep one malicious link
//! from filling memory.
//!
//! Available:
//!
//! ```lua
//! local res = kage.http.get(url)
//! local res = kage.http.post(url, { headers = {...}, body = "...", json = {...}, max_bytes = N })
//! local res = kage.http.delete(url, { headers = {...} })
//! local res = kage.http.post_stream(url, opts, function(ev) ... end)
//! ```
//!
//! `get` / `post` / `delete` return `{ status, body, content_type, truncated }`.
//! `post_stream` returns `{ status, content_type }` and dispatches each
//! decoded SSE frame to the callback as `{ event = name, data = payload }`
//! (multi-line `data:` lines are joined with `\n`). The callback is
//! invoked once per blank-line-terminated frame.

use std::io::{BufRead, BufReader, Read};

use kage_tools::ssrf;
use mlua::{Function, Lua, Table, Value};

use crate::api::lua_to_json;
use crate::error::PluginError;

/// Default cap on response body bytes for non-streaming requests.
const DEFAULT_MAX_BYTES: u64 = 2_000_000;

/// Default total-bytes cap for streamed responses. Larger than the
/// non-streaming cap since a streaming chat response can legitimately
/// run into the tens of megabytes over many tokens.
const DEFAULT_STREAM_MAX_BYTES: u64 = 32_000_000;

/// Install `kage.http` on the running Lua state.
pub fn install_http(lua: &Lua) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let http = lua.create_table()?;

    http.set(
        "get",
        lua.create_function(move |lua, (url, opts): (String, Option<Table>)| {
            let request = build_request(opts.as_ref())
                .map_err(|err| mlua::Error::external(format!("http.get {url}: {err}")))?;
            let max_bytes = request.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
            let result = simple_request("GET", &url, Some(&request), max_bytes)
                .map_err(|err| mlua::Error::external(format!("http.get {url}: {err}")))?;
            simple_result_to_table(lua, &result)
        })?,
    )?;

    http.set(
        "post",
        lua.create_function(move |lua, (url, opts): (String, Option<Table>)| {
            let request = build_request(opts.as_ref())
                .map_err(|err| mlua::Error::external(format!("http.post {url}: {err}")))?;
            let max_bytes = request.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
            let result = simple_request("POST", &url, Some(&request), max_bytes)
                .map_err(|err| mlua::Error::external(format!("http.post {url}: {err}")))?;
            simple_result_to_table(lua, &result)
        })?,
    )?;

    http.set(
        "delete",
        lua.create_function(move |lua, (url, opts): (String, Option<Table>)| {
            let request = build_request(opts.as_ref())
                .map_err(|err| mlua::Error::external(format!("http.delete {url}: {err}")))?;
            let max_bytes = request.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
            let result = simple_request("DELETE", &url, Some(&request), max_bytes)
                .map_err(|err| mlua::Error::external(format!("http.delete {url}: {err}")))?;
            simple_result_to_table(lua, &result)
        })?,
    )?;

    http.set(
        "post_stream",
        lua.create_function(
            move |lua, (url, opts, on_event): (String, Option<Table>, Function)| {
                let request = build_request(opts.as_ref()).map_err(|err| {
                    mlua::Error::external(format!("http.post_stream {url}: {err}"))
                })?;
                let max_bytes = request.max_bytes.unwrap_or(DEFAULT_STREAM_MAX_BYTES);
                stream_request(lua, &url, &request, max_bytes, &on_event)
                    .map_err(|err| mlua::Error::external(format!("http.post_stream {url}: {err}")))
            },
        )?,
    )?;

    kage.set("http", http)?;
    Ok(())
}

/// Caller-supplied request details parsed out of the Lua `opts` table.
#[derive(Default)]
struct RequestSpec {
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    content_type: Option<String>,
    max_bytes: Option<u64>,
}

fn build_request(opts: Option<&Table>) -> Result<RequestSpec, String> {
    let mut spec = RequestSpec::default();
    let Some(opts) = opts else {
        return Ok(spec);
    };

    if let Ok(headers) = opts.get::<Table>("headers") {
        for pair in headers.pairs::<String, String>() {
            let (k, v) = pair.map_err(|e| format!("headers: {e}"))?;
            spec.headers.push((k, v));
        }
    }

    let body: Option<String> = opts.get("body").ok();
    let json: Option<Value> = opts.get("json").ok();
    if body.is_some() && matches!(json, Some(Value::Table(_))) {
        return Err("opts.body and opts.json are mutually exclusive".to_owned());
    }
    if let Some(b) = body {
        spec.body = Some(b.into_bytes());
    } else if let Some(Value::Table(t)) = json {
        let encoded = serde_json::to_vec(
            &lua_to_json(Value::Table(t)).map_err(|e| format!("json encode: {e}"))?,
        )
        .map_err(|e| format!("json serialize: {e}"))?;
        spec.body = Some(encoded);
        spec.content_type = Some("application/json".to_owned());
    }

    if let Ok(cap) = opts.get::<u64>("max_bytes") {
        spec.max_bytes = Some(cap);
    }

    Ok(spec)
}

struct SimpleResult {
    status: u16,
    body: String,
    content_type: String,
    truncated: bool,
}

fn simple_result_to_table(lua: &Lua, r: &SimpleResult) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("status", r.status)?;
    table.set("body", r.body.clone())?;
    table.set("content_type", r.content_type.clone())?;
    table.set("truncated", r.truncated)?;
    Ok(table)
}

fn simple_request(
    method: &str,
    url: &str,
    spec: Option<&RequestSpec>,
    max_bytes: u64,
) -> Result<SimpleResult, String> {
    let (parsed, agent) = prepare(url)?;
    let response = dispatch(method, &agent, parsed.as_str(), spec)?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut reader = response.into_body().into_reader();
    let mut buf = Vec::new();
    let mut taken = (&mut reader).take(max_bytes + 1);
    let _ = taken.read_to_end(&mut buf);
    let truncated = u64::try_from(buf.len()).unwrap_or(u64::MAX) > max_bytes;
    if truncated {
        buf.truncate(usize::try_from(max_bytes).unwrap_or(buf.len()));
    }
    Ok(SimpleResult {
        status,
        body: String::from_utf8_lossy(&buf).into_owned(),
        content_type,
        truncated,
    })
}

fn stream_request(
    lua: &Lua,
    url: &str,
    spec: &RequestSpec,
    max_bytes: u64,
    on_event: &Function,
) -> Result<Table, String> {
    let (parsed, agent) = prepare(url)?;
    let response = dispatch("POST", &agent, parsed.as_str(), Some(spec))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let raw_reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
    let capped = raw_reader.take(max_bytes);
    let mut reader = BufReader::new(capped);

    let result = run_sse_loop(lua, &mut reader, on_event);

    let table = lua.create_table().map_err(|e| format!("lua table: {e}"))?;
    table
        .set("status", status)
        .map_err(|e| format!("lua set status: {e}"))?;
    table
        .set("content_type", content_type)
        .map_err(|e| format!("lua set content_type: {e}"))?;
    result?;
    Ok(table)
}

fn run_sse_loop<R: BufRead>(lua: &Lua, reader: &mut R, on_event: &Function) -> Result<(), String> {
    loop {
        match read_sse_frame(reader)? {
            Some(frame) => {
                let payload = lua.create_table().map_err(|e| format!("lua table: {e}"))?;
                payload
                    .set("event", frame.event)
                    .map_err(|e| format!("lua set event: {e}"))?;
                payload
                    .set("data", frame.data)
                    .map_err(|e| format!("lua set data: {e}"))?;
                on_event
                    .call::<()>(payload)
                    .map_err(|e| format!("on_event raised: {e}"))?;
            }
            None => return Ok(()),
        }
    }
}

struct SseFrame {
    event: String,
    data: String,
}

fn read_sse_frame<R: BufRead>(reader: &mut R) -> Result<Option<SseFrame>, String> {
    let mut event = String::new();
    let mut data = String::new();
    let mut have_content = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("transport: {e}"))?;
        if n == 0 {
            if have_content {
                return Ok(Some(SseFrame { event, data }));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if have_content {
                return Ok(Some(SseFrame { event, data }));
            }
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            rest.trim_start().clone_into(&mut event);
            have_content = true;
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            have_content = true;
        }
    }
}

fn prepare(url: &str) -> Result<(url::Url, ureq::Agent), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme: {other}")),
    }
    ssrf::check(&parsed).map_err(|e| e.to_string())?;
    Ok((parsed, build_agent()))
}

/// Build a ureq agent tuned for plugin HTTP calls. The default agent
/// drops the `Authorization` header on any redirect; an apex-to-www
/// redirect (or any same-host 30x) then arrives unauthenticated and the
/// upstream rejects it with what surfaces as a "redirect failed"
/// transport error. Keeping auth on same-host redirects matches what
/// every real HTTP client does and is the only way an apex-domain POST
/// against a host that 301s to www can succeed.
fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
        .build()
        .new_agent()
}

fn dispatch(
    method: &str,
    agent: &ureq::Agent,
    url: &str,
    spec: Option<&RequestSpec>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    match method {
        "GET" => dispatch_bodyless(agent.get(url), spec),
        "DELETE" => dispatch_bodyless(agent.delete(url), spec),
        "POST" => dispatch_with_body(agent.post(url), spec),
        other => Err(format!("unsupported method: {other}")),
    }
}

fn dispatch_bodyless(
    mut req: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    spec: Option<&RequestSpec>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    req = req.header("user-agent", "kage-plugin/0.1");
    if let Some(spec) = spec {
        for (k, v) in &spec.headers {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    req.call().map_err(|e| format!("transport error: {e}"))
}

fn dispatch_with_body(
    mut req: ureq::RequestBuilder<ureq::typestate::WithBody>,
    spec: Option<&RequestSpec>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    req = req.header("user-agent", "kage-plugin/0.1");
    if let Some(spec) = spec {
        if let Some(ct) = &spec.content_type {
            req = req.header("content-type", ct);
        }
        for (k, v) in &spec.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &spec.body {
            return req
                .send(body.as_slice())
                .map_err(|e| format!("transport error: {e}"));
        }
    }
    req.send_empty()
        .map_err(|e| format!("transport error: {e}"))
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn rejects_private_url() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval("return kage.http.get('http://127.0.0.1:8080/x')");
        assert!(res.is_err());
    }

    #[test]
    fn rejects_non_http_scheme() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval("return kage.http.get('file:///etc/passwd')");
        assert!(res.is_err());
    }

    #[test]
    fn rejects_invalid_url() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval("return kage.http.get('not a url')");
        assert!(res.is_err());
    }

    #[test]
    fn post_rejects_private_url() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval(
            "return kage.http.post('http://127.0.0.1:8080/x', { json = { hello = 'world' } })",
        );
        assert!(res.is_err());
    }

    #[test]
    fn delete_rejects_private_url() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval("return kage.http.delete('http://127.0.0.1:8080/x')");
        assert!(res.is_err());
    }

    #[test]
    fn post_stream_rejects_private_url() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval(
            "return kage.http.post_stream('http://127.0.0.1:8080/x', \
             { json = { stream = true } }, function() end)",
        );
        assert!(res.is_err());
    }

    #[test]
    fn post_rejects_body_and_json_together() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval(
            "return kage.http.post('https://example.com', \
             { body = 'x', json = { a = 1 } })",
        );
        assert!(res.is_err());
    }

    #[test]
    fn sse_parser_joins_multi_line_data() {
        use std::io::BufReader;
        let bytes: &[u8] = b"event: msg\ndata: hello\ndata: world\n\n";
        let mut reader = BufReader::new(bytes);
        let frame = super::read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.event, "msg");
        assert_eq!(frame.data, "hello\nworld");
        assert!(super::read_sse_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn sse_parser_skips_comments_and_blank_runs() {
        use std::io::BufReader;
        let bytes: &[u8] = b": comment\n\n\ndata: only\n\n";
        let mut reader = BufReader::new(bytes);
        let frame = super::read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.event, "");
        assert_eq!(frame.data, "only");
    }

    #[test]
    fn sse_parser_flushes_trailing_frame_at_eof() {
        use std::io::BufReader;
        let bytes: &[u8] = b"data: trailing";
        let mut reader = BufReader::new(bytes);
        let frame = super::read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.data, "trailing");
        assert!(super::read_sse_frame(&mut reader).unwrap().is_none());
    }
}
