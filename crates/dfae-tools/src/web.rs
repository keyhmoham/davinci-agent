//! The agent's window on the world: one hardened fetch routine shared by `http_get` and
//! `web_search`.
//!
//! * `web_search` — SearXNG (`SEARXNG_URL`) or Brave (`BRAVE_API_KEY`) when configured, else
//!   DuckDuckGo's HTML endpoint, else Wikipedia's search API. Optionally reads the top pages.
//!
//! Every request, and every redirect hop, is checked: http(s) only, no credentials in the
//! URL, and no hostname that resolves to a private, loopback or link-local address.

use crate::policy::{PolicyEngine, is_private_ip};
use async_trait::async_trait;
use dfae_core::*;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static SCRIPT_STYLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>|<svg[^>]*>.*?</svg>|<!--.*?-->").unwrap()
});
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());

pub(crate) fn html_to_text(html: &str) -> String {
    let t = SCRIPT_STYLE.replace_all(html, " ");
    let t = TAGS.replace_all(&t, " ");
    let t = t
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) struct Fetched {
    pub status: u16,
    pub ctype: String,
    pub text: String,
    pub truncated: bool,
}

/// GET `url`, following up to 3 redirects. `trusted` skips only the domain allowlist (used for the
/// fixed backends of the search tool); every other check always applies.
pub(crate) async fn fetch(
    policy: &PolicyEngine,
    meta: &ToolMetadata,
    url: &str,
    headers: &[(&str, &str)],
    cancel: &CancellationToken,
    trusted: bool,
) -> Result<Fetched, ToolError> {
    let net = &policy.config().network;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(net.timeout_secs))
        .user_agent("Mozilla/5.0 (compatible; dfae/0.1)")
        .build()
        .map_err(|e| ToolError::other(e.to_string()))?;
    let mut url = url.to_string();
    for hop in 0..4 {
        // the agent's own server (loopback, a serving port, the sandbox shell) may be asked directly — never reached by a redirect
        let own = !trusted && policy.own_server(&url);
        if own && hop > 0 {
            return Err(ToolError::policy("a redirect to a local address is blocked"));
        }
        if trusted {
            policy.public_host(&url).map_err(ToolError::policy)?;
        } else if let crate::policy::PolicyDecision::Deny(m) = policy.check(meta, &json!({"url": url}), false) {
            return Err(ToolError::policy(m));
        }
        let parsed = reqwest::Url::parse(&url).map_err(|e| ToolError::args(format!("bad URL: {e}")))?;
        let host = parsed.host_str().unwrap_or_default().to_string();
        let port = parsed.port_or_known_default().unwrap_or(80);
        let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| ToolError::new(ErrorKind::Transient, format!("DNS lookup failed for {host}: {e}")))?
            .collect();
        if addrs.is_empty() || (!own && addrs.iter().any(|a| is_private_ip(&a.ip()))) {
            return Err(ToolError::policy(format!("{host} resolves to a private or loopback address")));
        }
        let mut rb = client.get(parsed.clone());
        for (k, v) in headers {
            rb = rb.header(*k, *v);
        }
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::new(ErrorKind::Cancelled, "cancelled")),
            r = rb.send() => r.map_err(|e| if e.is_timeout() { ToolError::new(ErrorKind::Timeout, e.to_string()) } else { ToolError::new(ErrorKind::Transient, e.to_string()) })?,
        };
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| ToolError::other("redirect without location"))?;
            url = parsed.join(loc).map_err(|e| ToolError::other(e.to_string()))?.to_string();
            if hop == 3 {
                return Err(ToolError::other("too many redirects"));
            }
            continue;
        }
        let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let cap = net.max_response_bytes;
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ToolError::new(ErrorKind::Transient, e.to_string()))?;
            if buf.len() + chunk.len() > cap {
                buf.extend_from_slice(&chunk[..cap - buf.len()]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        let text = if ctype.contains("html") { html_to_text(&body) } else { body };
        return Ok(Fetched { status: status.as_u16(), ctype: ctype.split(';').next().unwrap_or("").to_string(), text, truncated });
    }
    Err(ToolError::other("too many redirects"))
}

fn http_meta() -> ToolMetadata {
    ToolMetadata {
        name: "http_get".into(),
        description: String::new(),
        category: "web".into(),
        risk: Risk::Low,
        requires_approval: false,
        side_effects: false,
        reversible: true,
        cost_class: "free".into(),
        params: vec![],
        keywords: vec![],
        direct_patterns: vec![],
        cacheable: false,
        contract: vec![],
    }
}

fn clip(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars { format!("{}…", t.trim_end()) } else { t }
}

// ------------------------------------------------------------ web_search ----

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

static DDG_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)(<a\s[^>]*class="[^"]*result__a[^"]*"[^>]*>)(.*?)</a>"#).unwrap());
static DDG_SNIPPET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).unwrap());
static HREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="([^"]+)""#).unwrap());

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<encoded target>`.
fn ddg_target(href: &str) -> Option<String> {
    let href = href.replace("&amp;", "&");
    let abs = if href.starts_with("//") { format!("https:{href}") } else { href };
    let u = reqwest::Url::parse(&abs).ok()?;
    if u.host_str().map(|h| h.ends_with("duckduckgo.com")).unwrap_or(false) {
        return u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned()).filter(|t| t.starts_with("http"));
    }
    abs.starts_with("http").then_some(abs)
}

pub(crate) fn parse_ddg(html: &str, max: usize) -> Vec<Hit> {
    let mut out = Vec::new();
    let links: Vec<_> = DDG_LINK.captures_iter(html).collect();
    for (i, c) in links.iter().enumerate() {
        let tag_end = c.get(0).map(|m| m.end()).unwrap_or(0);
        let next_start = links.get(i + 1).and_then(|n| n.get(0)).map(|m| m.start()).unwrap_or(html.len());
        let Some(url) = HREF.captures(&c[1]).and_then(|h| ddg_target(&h[1])) else { continue };
        // sponsored results go through y.js
        if url.contains("duckduckgo.com/y.js") {
            continue;
        }
        let snippet = DDG_SNIPPET.captures(&html[tag_end..next_start]).map(|s| html_to_text(&s[1])).unwrap_or_default();
        out.push(Hit { title: html_to_text(&c[2]), url, snippet });
        if out.len() >= max {
            break;
        }
    }
    out
}

fn parse_searx(v: &Value, max: usize) -> Vec<Hit> {
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(Hit {
                        title: r["title"].as_str()?.to_string(),
                        url: r["url"].as_str()?.to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                    })
                })
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_brave(v: &Value, max: usize) -> Vec<Hit> {
    v["web"]["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(Hit {
                        title: html_to_text(r["title"].as_str()?),
                        url: r["url"].as_str()?.to_string(),
                        snippet: html_to_text(r["description"].as_str().unwrap_or("")),
                    })
                })
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_wikipedia(v: &Value, max: usize) -> Vec<Hit> {
    v["query"]["search"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let title = r["title"].as_str()?;
                    Some(Hit {
                        title: title.to_string(),
                        url: format!("https://en.wikipedia.org/wiki/{}", title.replace(' ', "_")),
                        snippet: html_to_text(r["snippet"].as_str().unwrap_or("")),
                    })
                })
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

fn enc(s: &str) -> String {
    reqwest::Url::parse_with_params("http://x/", &[("q", s)]).map(|u| u.query().unwrap_or("").trim_start_matches("q=").to_string()).unwrap_or_default()
}

pub struct WebSearch {
    pub(crate) policy: Arc<PolicyEngine>,
}

impl WebSearch {
    async fn search(&self, q: &str, max: usize, cancel: &CancellationToken) -> Result<(String, Vec<Hit>), ToolError> {
        let meta = http_meta();
        let get = |url: String, headers: Vec<(&'static str, String)>| {
            let policy = self.policy.clone();
            let meta = meta.clone();
            let cancel = cancel.clone();
            async move {
                let hs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
                fetch(&policy, &meta, &url, &hs, &cancel, true).await
            }
        };
        // why each engine did not produce results, in the order they were tried
        let mut notes: Vec<String> = Vec::new();
        let mut answered = false; // some engine replied properly, just with nothing
        let note = |notes: &mut Vec<String>, engine: &str, why: String| notes.push(format!("{engine}: {why}"));
        // 1. a configured SearXNG instance, 2. Brave with a key — both return JSON
        if let Ok(base) = std::env::var("SEARXNG_URL")
            && !base.trim().is_empty()
        {
            let url = format!("{}/search?q={}&format=json", base.trim_end_matches('/'), enc(q));
            match get(url, vec![("accept", "application/json".into())]).await {
                Ok(f) if f.status == 200 => {
                    let hits = serde_json::from_str::<Value>(&f.text).map(|v| parse_searx(&v, max)).unwrap_or_default();
                    if !hits.is_empty() {
                        return Ok(("searxng".into(), hits));
                    }
                    answered = true;
                }
                Ok(f) => note(&mut notes, "searxng", format!("HTTP {}", f.status)),
                Err(e) => note(&mut notes, "searxng", e.message),
            }
        }
        if let Ok(key) = std::env::var("BRAVE_API_KEY")
            && !key.trim().is_empty()
        {
            let url = format!("https://api.search.brave.com/res/v1/web/search?q={}&count={max}", enc(q));
            match get(url, vec![("accept", "application/json".into()), ("x-subscription-token", key)]).await {
                Ok(f) if f.status == 200 => {
                    let hits = serde_json::from_str::<Value>(&f.text).map(|v| parse_brave(&v, max)).unwrap_or_default();
                    if !hits.is_empty() {
                        return Ok(("brave".into(), hits));
                    }
                    answered = true;
                }
                Ok(f) => note(&mut notes, "brave", format!("HTTP {}", f.status)),
                Err(e) => note(&mut notes, "brave", e.message),
            }
        }
        // 3. DuckDuckGo (keyless; it may answer bots with a challenge page), 4. Wikipedia (keyless)
        // the result page must be parsed as markup, so it bypasses the text flattening of `fetch`
        match self.raw_get(&format!("https://html.duckduckgo.com/html/?q={}", enc(q)), cancel).await {
            Ok(html) => {
                let hits = parse_ddg(&html, max);
                if !hits.is_empty() {
                    return Ok(("duckduckgo".into(), hits));
                }
                note(&mut notes, "duckduckgo", "no results (it may be blocking automated requests)".into());
            }
            Err(e) => note(&mut notes, "duckduckgo", e.message),
        }
        match get(format!("https://en.wikipedia.org/w/api.php?action=query&list=search&format=json&srlimit={max}&srsearch={}", enc(q)), vec![]).await {
            Ok(f) if f.status == 200 => {
                let hits = serde_json::from_str::<Value>(&f.text).map(|v| parse_wikipedia(&v, max)).unwrap_or_default();
                if !hits.is_empty() {
                    return Ok(("wikipedia".into(), hits));
                }
                answered = true;
            }
            Ok(f) => note(&mut notes, "wikipedia", format!("HTTP {}", f.status)),
            Err(e) => note(&mut notes, "wikipedia", e.message),
        }
        if answered && notes.is_empty() {
            return Err(ToolError::new(ErrorKind::NotFound, format!("no search results for '{q}'")));
        }
        Err(ToolError::new(
            ErrorKind::Transient,
            format!("no search engine returned results ({}); set SEARXNG_URL or BRAVE_API_KEY for a dependable backend", notes.join("; ")),
        ))
    }

    /// Like `fetch`, but returns the markup untouched (needed to parse result pages).
    async fn raw_get(&self, url: &str, cancel: &CancellationToken) -> Result<String, ToolError> {
        self.policy.public_host(url).map_err(ToolError::policy)?;
        let net = &self.policy.config().network;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(net.timeout_secs))
            .user_agent("Mozilla/5.0 (compatible; dfae/0.1)")
            .build()
            .map_err(|e| ToolError::other(e.to_string()))?;
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::new(ErrorKind::Cancelled, "cancelled")),
            r = client.get(url).send() => r.map_err(|e| ToolError::new(ErrorKind::Transient, e.to_string()))?,
        };
        if !resp.status().is_success() {
            return Err(ToolError::new(ErrorKind::Transient, format!("HTTP {}", resp.status())));
        }
        let bytes = resp.bytes().await.map_err(|e| ToolError::new(ErrorKind::Transient, e.to_string()))?;
        Ok(String::from_utf8_lossy(&bytes[..bytes.len().min(net.max_response_bytes)]).into_owned())
    }
}

pub(crate) fn render_hits(engine: &str, hits: &[Hit]) -> String {
    let mut s = format!("results via {engine}:");
    for (i, h) in hits.iter().enumerate() {
        s.push_str(&format!("\n{}. {} — {}", i + 1, clip(&h.title, 90), h.url));
        if !h.snippet.is_empty() {
            s.push_str(&format!("\n   {}", clip(&h.snippet, 200)));
        }
    }
    s
}

#[async_trait]
impl Tool for WebSearch {
    fn metadata(&self) -> ToolMetadata {
        let mut m = ToolMetadata {
            name: "web_search".into(),
            description: "search the web: titles, URLs, snippets (fetch_top=1..2 also reads the top pages). For current weather prefer http_get on https://wttr.in/<city>?format=3".into(),
            category: "web".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: false,
            reversible: true,
            cost_class: "free".into(),
            params: vec![
                ParamSpec::new("query", ParamType::String, true, "search query").hints(&["quoted"]),
                ParamSpec::new("max_results", ParamType::Integer, false, "1-8, default 5"),
                ParamSpec::new("fetch_top", ParamType::Integer, false, "also read the first N result pages (0-2, default 0)"),
            ],
            keywords: ["search", "google", "web", "internet", "online", "latest", "news", "lookup", "look", "find", "who", "current", "price", "weather", "temperature", "forecast"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            // only phrasings that are unmistakably about the web; a bare "search for X" may mean the files
            direct_patterns: vec![
                r"(?i)\b(?:google|look\s+up|lookup|duckduckgo)\s+(?P<query>[^\n]{3,200}?)\s*[?.!]?\s*$".into(),
                r"(?i)\bsearch\s+(?:the\s+)?(?:web|internet|online)\s+(?:for\s+)?(?P<query>[^\n]{3,200}?)\s*[?.!]?\s*$".into(),
                r"(?i)\bsearch\s+(?:for\s+)?(?P<query>[^\n]{3,200}?)\s+(?:online|on\s+the\s+(?:web|internet)|on\s+google)\s*[?.!]?\s*$".into(),
            ],
            cacheable: true,
            contract: vec![Check::Matches { pattern: r"^results via".into() }],
        };
        m.params[1].default = Some(json!(5));
        m
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let q = input.get("query").and_then(|v| v.as_str()).map(str::trim).filter(|q| !q.is_empty()).ok_or_else(|| ToolError::args("missing 'query'"))?;
        let max = input.get("max_results").and_then(|v| v.as_u64()).unwrap_or(5).clamp(1, 8) as usize;
        let top = input.get("fetch_top").and_then(|v| v.as_u64()).unwrap_or(0).min(2) as usize;
        let (engine, hits) = self.search(q, max, &ctx.cancel).await?;
        let mut out = render_hits(&engine, &hits);
        let meta = http_meta();
        for h in hits.iter().take(top) {
            // result URLs are not chosen by the model: read-only GETs, still barred from private hosts
            if let Ok(f) = fetch(&self.policy, &meta, &h.url, &[], &ctx.cancel, true).await
                && f.status == 200
                && !f.text.is_empty()
            {
                out.push_str(&format!("\n--- {} ---\n{}", h.url, clip(&f.text, 1800)));
            }
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_results_and_skips_ads() {
        let html = r#"
        <div class="result"><h2><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa%3Fx%3D1&amp;rut=abc">Example <b>A</b></a></h2>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">First &amp; best snippet</a></div>
        <div class="result"><a class="result__a" href="https://duckduckgo.com/y.js?ad=1">Ad</a></div>
        <div class="result"><a class="result__a" href="https://rust-lang.org/">Rust</a>
        <a class="result__snippet">A language</a></div>"#;
        let hits = parse_ddg(html, 5);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0], Hit { title: "Example A".into(), url: "https://example.org/a?x=1".into(), snippet: "First & best snippet".into() });
        assert_eq!(hits[1].url, "https://rust-lang.org/");
        assert_eq!(parse_ddg(html, 1).len(), 1);
        assert!(render_hits("duckduckgo", &hits).starts_with("results via duckduckgo:\n1. Example A — https://example.org/a?x=1"));
    }

    #[test]
    fn parses_json_engines() {
        let s = json!({"results": [{"title": "T", "url": "https://a.b", "content": "c"}, {"title": "no url"}]});
        assert_eq!(parse_searx(&s, 5).len(), 1);
        let b = json!({"web": {"results": [{"title": "<b>B</b>", "url": "https://b.c", "description": "d"}]}});
        assert_eq!(parse_brave(&b, 5)[0].title, "B");
        let w = json!({"query": {"search": [{"title": "New York City", "snippet": "a <span>big</span> city"}]}});
        let h = parse_wikipedia(&w, 5);
        assert_eq!(h[0].url, "https://en.wikipedia.org/wiki/New_York_City");
        assert_eq!(h[0].snippet, "a big city");
    }

    #[test]
    fn query_encoding() {
        assert_eq!(enc("new york & co"), "new+york+%26+co");
    }
}
