//! The text block that carries an attached resource in a user message.
//!
//! MCP resource mentions, MCP prompt results and editor context all attach
//! contents the same way, so the model, the session file and every client
//! agree on one format:
//!
//! ```text
//! <resource uri="test://static/resource/1" server="everything" mime="text/plain">
//! ...contents...
//! </resource>
//! ```
//!
//! [`render`] builds the block and [`parse`] reads its first line back,
//! so a client can show a short `attached` line instead of the contents.
//! Binary contents the model cannot read as text get an empty block
//! from [`render_binary`] that names their size.

use std::fmt::Write as _;

const OPEN: &str = "<resource ";
const CLOSE: &str = "</resource>";

/// What [`parse`] reads back from a resource block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceRef {
    /// Resource URI.
    pub uri: String,
    /// MCP server the resource came from. `None` for editor context.
    pub server: Option<String>,
    /// MIME type, when the block names one.
    pub mime: Option<String>,
    /// Size of the contents in bytes, or of the binary contents the
    /// block stands for.
    pub bytes: usize,
    /// Whether the block stands for binary contents it does not carry.
    pub binary: bool,
}

/// Wrap `text` in a resource block. `server` is omitted for editor
/// context, and `mime` when it is unknown.
#[must_use]
pub fn render(uri: &str, server: Option<&str>, mime: Option<&str>, text: &str) -> String {
    let mut out = attributes(uri, server, mime);
    let _ = write!(out, ">\n{text}\n{CLOSE}");
    out
}

/// An empty block for `bytes` of binary contents of type `mime`, with
/// the size in a `size` attribute. An image is sent as its own content
/// right after the block.
#[must_use]
pub fn render_binary(uri: &str, server: Option<&str>, mime: &str, bytes: usize) -> String {
    let mut out = attributes(uri, server, Some(mime));
    let _ = write!(out, " size=\"{bytes}\">\n\n{CLOSE}");
    out
}

fn attributes(uri: &str, server: Option<&str>, mime: Option<&str>) -> String {
    let mut out = format!("{OPEN}uri=\"{}\"", escape(uri));
    if let Some(server) = server {
        let _ = write!(out, " server=\"{}\"", escape(server));
    }
    if let Some(mime) = mime {
        let _ = write!(out, " mime=\"{}\"", escape(mime));
    }
    out
}

/// Read the attributes of a block built by [`render`], or `None` when
/// `text` is not one. Only the first line is parsed for attributes.
#[must_use]
pub fn parse(text: &str) -> Option<ResourceRef> {
    let (head, body) = text.split_once('\n')?;
    let mut attrs = head.strip_prefix(OPEN)?.strip_suffix('>')?;
    let mut uri = None;
    let mut server = None;
    let mut mime = None;
    let mut size = None;
    while !attrs.is_empty() {
        let (name, rest) = attrs.split_once("=\"")?;
        let (value, rest) = rest.split_once('"')?;
        let value = unescape(value);
        match name {
            "uri" => uri = Some(value),
            "server" => server = Some(value),
            "mime" => mime = Some(value),
            "size" => size = Some(value.parse().ok()?),
            _ => return None,
        }
        attrs = rest.strip_prefix(' ').unwrap_or(rest);
    }
    let contents = body
        .strip_suffix(CLOSE)
        .map_or(body, |b| b.strip_suffix('\n').unwrap_or(b));
    Some(ResourceRef {
        uri: uri?,
        server,
        mime,
        bytes: size.unwrap_or(contents.len()),
        binary: size.is_some(),
    })
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_then_parse_roundtrips() {
        let block = render(
            "test://static/resource/1",
            Some("everything"),
            Some("text/plain"),
            "hello\nworld",
        );
        assert_eq!(
            block,
            "<resource uri=\"test://static/resource/1\" server=\"everything\" mime=\"text/plain\">\nhello\nworld\n</resource>"
        );
        assert_eq!(
            parse(&block),
            Some(ResourceRef {
                uri: "test://static/resource/1".into(),
                server: Some("everything".into()),
                mime: Some("text/plain".into()),
                bytes: 11,
                binary: false,
            })
        );
    }

    #[test]
    fn binary_blocks_carry_their_size() {
        let block = render_binary("test://img", Some("fix"), "image/png", 2048);
        assert_eq!(
            block,
            "<resource uri=\"test://img\" server=\"fix\" mime=\"image/png\" size=\"2048\">\n\n</resource>"
        );
        assert_eq!(
            parse(&block),
            Some(ResourceRef {
                uri: "test://img".into(),
                server: Some("fix".into()),
                mime: Some("image/png".into()),
                bytes: 2048,
                binary: true,
            })
        );
    }

    #[test]
    fn attributes_escape_and_server_is_optional() {
        let uri = "file:///a \"b\" <c>&d.txt";
        let block = render(uri, None, None, "");
        assert!(block.starts_with("<resource uri=\"file:///a &quot;b&quot; &lt;c>&amp;d.txt\">\n"));
        let parsed = parse(&block).unwrap();
        assert_eq!(parsed.uri, uri);
        assert_eq!(parsed.server, None);
        assert_eq!(parsed.bytes, 0);
    }

    #[test]
    fn ordinary_text_does_not_parse() {
        for text in [
            "",
            "hello",
            "<resource>\nx\n</resource>",
            "<resource uri=\"x\">",
            "<resource name=\"x\">\nbody\n</resource>",
            "see <resource uri=\"x\">\nbody\n</resource>",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }
}
