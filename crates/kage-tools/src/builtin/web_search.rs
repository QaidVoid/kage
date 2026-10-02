//! `web_search` tool: search the web and return the results' titles,
//! URLs and snippets.
//!
//! DuckDuckGo's HTML endpoint answers by default and needs no key. A
//! SearXNG instance's JSON API answers instead when `[tools.web_search]`
//! names one, for a search that does not depend on scraping a page.

use std::fmt::Write as _;
use std::io::Read as _;
use std::time::Duration;

use kage_core::config::{SearchEngine, WebSearchConfig};
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolContext, ToolError, schema_for};

/// Whole-request budget.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The largest answer page read.
const MAX_BYTES: u64 = 2_000_000;

/// Results returned when the model asks for no count, and at most.
const DEFAULT_COUNT: usize = 8;
const MAX_COUNT: usize = 20;

/// Input shape for the `web_search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WebSearchInput {
    /// What to search for.
    query: String,
    /// How many results to return. Defaults to 8, at most 20.
    #[serde(default)]
    count: Option<usize>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    /// The page title.
    pub title: String,
    /// The page URL.
    pub url: String,
    /// The engine's excerpt of the page.
    pub snippet: String,
}

/// Search the web through the configured engine.
#[derive(Debug, Default)]
pub struct WebSearchTool {
    config: WebSearchConfig,
}

impl WebSearchTool {
    /// A tool searching through the engine `config` names.
    #[must_use]
    pub fn new(config: WebSearchConfig) -> Self {
        Self { config }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web. Returns up to `count` results, each with its title, \
         URL and a short snippet. Use web_fetch to read a result's page."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<WebSearchInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Network
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: WebSearchInput = serde_json::from_value(input)?;
        let query = input.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidInput("query is empty".to_owned()));
        }
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let count = input.count.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);
        let mut results = match self.config.engine {
            SearchEngine::Duckduckgo => duckduckgo(query)?,
            SearchEngine::Searxng => {
                let base = self.config.url.as_deref().ok_or_else(|| {
                    ToolError::Other(
                        "[tools.web_search] engine = \"searxng\" needs a url".to_owned(),
                    )
                })?;
                searxng(base, query)?
            }
        };
        results.truncate(count);
        Ok(output(query, &results))
    }
}

/// The tool's answer: a numbered list for the model, the results as data
/// for a client.
fn output(query: &str, results: &[SearchResult]) -> ToolOutput {
    let mut text = String::new();
    if results.is_empty() {
        let _ = write!(text, "No results for {query}.");
    }
    for (ix, result) in results.iter().enumerate() {
        let _ = writeln!(text, "{}. {}\n   {}", ix + 1, result.title, result.url);
        if !result.snippet.is_empty() {
            let _ = writeln!(text, "   {}", result.snippet);
        }
    }
    ToolOutput {
        is_error: false,
        text: text.trim_end().to_owned(),
        structured: Some(serde_json::json!({ "query": query, "results": results })),
        terminate: false,
    }
}

fn get(url: &url::Url) -> Result<String, ToolError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let response = agent
        .get(url.as_str())
        .header("user-agent", "kage/0.1 (+https://github.com/QaidVoid/kage)")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => ToolError::Other(format!(
                "{} answered http {code}",
                url.host_str().unwrap_or("the engine")
            )),
            other => ToolError::Other(format!("search failed: {other}")),
        })?;
    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_BYTES)
        .read_to_string(&mut body)
        .map_err(|e| ToolError::Other(format!("read search results: {e}")))?;
    Ok(body)
}

fn duckduckgo(query: &str) -> Result<Vec<SearchResult>, ToolError> {
    let mut url = url::Url::parse("https://html.duckduckgo.com/html/").expect("static url");
    url.query_pairs_mut().append_pair("q", query);
    let page = get(&url)?;
    let results = parse_duckduckgo(&page);
    if results.is_empty() && page.contains("anomaly") {
        return Err(ToolError::Other(
            "DuckDuckGo refused the search as automated; try again later, or set a SearXNG \
             instance under [tools.web_search]"
                .to_owned(),
        ));
    }
    Ok(results)
}

/// The results of a DuckDuckGo HTML answer page, ads left out.
fn parse_duckduckgo(page: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for block in page.split("class=\"result__a\"").skip(1) {
        let Some(href) = attr(block, "href") else {
            continue;
        };
        let Some(url) = result_url(&href) else {
            continue;
        };
        let title = block
            .split_once('>')
            .and_then(|(_, rest)| rest.split_once("</a>"))
            .map(|(title, _)| text_of(title))
            .unwrap_or_default();
        let snippet = block
            .split_once("class=\"result__snippet\"")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(_, rest)| {
                let end = ["</a>", "</div>"]
                    .iter()
                    .filter_map(|close| rest.find(close))
                    .min()
                    .unwrap_or(rest.len());
                text_of(&rest[..end])
            })
            .unwrap_or_default();
        if title.is_empty() || results.iter().any(|r: &SearchResult| r.url == url) {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// The value of attribute `name` at the start of `tag`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')? + start;
    Some(decode(&tag[start..end]))
}

/// The page a DuckDuckGo result link leads to: the `uddg` target of its
/// redirect, or the link itself. `None` for an ad.
fn result_url(href: &str) -> Option<String> {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    let parsed = url::Url::parse(&absolute).ok()?;
    if parsed.path().starts_with("/y.js") {
        return None;
    }
    match parsed.query_pairs().find(|(key, _)| key == "uddg") {
        Some((_, target)) => Some(target.into_owned()),
        None => Some(absolute),
    }
}

/// `html` as plain text: tags dropped, entities decoded, spaces folded.
fn text_of(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn searxng(base: &str, query: &str) -> Result<Vec<SearchResult>, ToolError> {
    let mut url = url::Url::parse(base)
        .and_then(|base| base.join("search"))
        .map_err(|e| ToolError::Other(format!("[tools.web_search] url: {e}")))?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json");
    let body = get(&url)?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        ToolError::Other(format!(
            "{} did not answer with JSON; enable the json format in its settings",
            url.host_str().unwrap_or("the SearXNG instance")
        ))
    })?;
    Ok(json["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter_map(|result| {
                    Some(SearchResult {
                        title: result["title"].as_str()?.to_owned(),
                        url: result["url"].as_str()?.to_owned(),
                        snippet: result["content"].as_str().unwrap_or_default().to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::{output, parse_duckduckgo, result_url};

    const PAGE: &str = r#"
        <div class="result results_links">
          <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Faws.amazon.com%2Fblogs%2Fbackoff%2F&amp;rut=abc">Exponential Backoff &amp; <b>Jitter</b></a>
          </h2>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">Add <b>jitter</b> to   spread retries.</a>
        </div>
        <div class="result result--ad">
          <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x">Buy retries</a>
        </div>
        <div class="result">
          <a rel="nofollow" class="result__a" href="https://example.com/plain">Plain</a>
        </div>
    "#;

    /// Asks DuckDuckGo for real; run with `--ignored` to check the page
    /// shape still parses.
    #[test]
    #[ignore = "reaches the network"]
    fn duckduckgo_answers_with_results() {
        let results = super::duckduckgo("rust programming language").unwrap();
        assert!(results.len() >= 3, "{results:?}");
        assert!(
            results.iter().all(|r| r.url.starts_with("http")),
            "{results:?}"
        );
    }

    #[test]
    fn a_duckduckgo_page_reads_as_results_without_ads() {
        let results = parse_duckduckgo(PAGE);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Exponential Backoff & Jitter");
        assert_eq!(results[0].url, "https://aws.amazon.com/blogs/backoff/");
        assert_eq!(results[0].snippet, "Add jitter to spread retries.");
        assert_eq!(results[1].url, "https://example.com/plain");
        assert_eq!(results[1].snippet, "");
        assert_eq!(result_url("https://duckduckgo.com/y.js?x=1"), None);
    }

    #[test]
    fn the_answer_numbers_the_results_and_carries_them_as_data() {
        let out = output("jitter", &parse_duckduckgo(PAGE));
        assert!(out.text.starts_with("1. Exponential Backoff & Jitter\n   https://aws.amazon.com/blogs/backoff/\n   Add jitter"), "{}", out.text);
        let structured = out.structured.unwrap();
        assert_eq!(structured["query"], "jitter");
        assert_eq!(structured["results"][1]["title"], "Plain");
        assert_eq!(output("none", &[]).text, "No results for none.");
    }
}
