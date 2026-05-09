//! `kage.http.get(url)` - sandboxed HTTP fetch helper.
//!
//! Uses the same SSRF guards as the built-in `web_fetch` tool: the URL
//! must be `http(s)`, the host must resolve to a routable address, and
//! the response body is capped to keep one malicious link from filling
//! memory. Returns a Lua table:
//! ```lua
//! local res = kage.http.get('https://example.com')
//! -- res.status        : integer, HTTP status code
//! -- res.body          : string, decoded body (truncated at cap)
//! -- res.content_type  : string, value of Content-Type or ""
//! -- res.truncated     : boolean
//! ```

use std::io::Read;

use kage_tools::ssrf;
use mlua::{Lua, Table};

use crate::error::PluginError;

/// Default cap on response body bytes; matches `web_fetch`.
const DEFAULT_MAX_BYTES: u64 = 2_000_000;

/// Install `kage.http.get` on the running Lua state.
pub fn install_http(lua: &Lua) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let http = lua.create_table()?;

    http.set(
        "get",
        lua.create_function(move |lua, url: String| {
            let result = fetch(&url, DEFAULT_MAX_BYTES)
                .map_err(|err| mlua::Error::external(format!("http.get {url}: {err}")))?;
            let table = lua.create_table()?;
            table.set("status", result.status)?;
            table.set("body", result.body)?;
            table.set("content_type", result.content_type)?;
            table.set("truncated", result.truncated)?;
            Ok(table)
        })?,
    )?;

    kage.set("http", http)?;
    Ok(())
}

struct FetchResult {
    status: u16,
    body: String,
    content_type: String,
    truncated: bool,
}

fn fetch(url: &str, max_bytes: u64) -> Result<FetchResult, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme: {other}")),
    }
    ssrf::check(&parsed).map_err(|e| e.to_string())?;
    let agent = ureq::Agent::new_with_defaults();
    let response = agent
        .get(parsed.as_str())
        .header("user-agent", "kage-plugin/0.1")
        .call()
        .map_err(|e| format!("transport error: {e}"))?;
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
    Ok(FetchResult {
        status,
        body: String::from_utf8_lossy(&buf).into_owned(),
        content_type,
        truncated,
    })
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
}
