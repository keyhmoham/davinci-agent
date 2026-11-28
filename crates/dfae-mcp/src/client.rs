//! A Model Context Protocol client: JSON-RPC 2.0 over Streamable HTTP (remote servers) or over a child process's stdio (local ones).
//!
//! Only what an agent needs is implemented: `initialize`, `tools/list` and `tools/call`. The client advertises no capabilities of its
//! own, so a server has nothing to ask of it (requests it sends anyway are answered with "method not found").

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The protocol revision this client speaks (a server may answer with an older one; that one is then used).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    /// The server could not be reached, or the connection broke: reconnecting may help.
    #[error("{0}")]
    Transport(String),
    /// The server answered, but not in a way this client understands.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server answered with a JSON-RPC error.
    #[error("{message} (code {code})")]
    Rpc { code: i64, message: String },
    #[error("timed out after {0}s")]
    Timeout(u64),
    /// HTTP 401 or 403: credentials are missing, expired or not enough. `challenge` is the `WWW-Authenticate` header, which says where
    /// to sign in (MCP authorization).
    #[error("{message}")]
    Unauthorized { status: u16, challenge: Option<String>, message: String },
}

impl McpError {
    pub fn retryable(&self) -> bool {
        matches!(self, McpError::Transport(_))
    }

    /// The server refused the call for want of credentials (HTTP 401 or 403): it cannot be used as it is, whatever its listing says.
    pub fn needs_login(&self) -> bool {
        matches!(self, McpError::Unauthorized { .. })
    }
}

/// How to reach a server.
#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    /// A remote server speaking Streamable HTTP (or the local proxy of its mcpc session); `headers` carry credentials (`Authorization: Bearer …`).
    Http { url: String, headers: BTreeMap<String, String> },
    /// A local command speaking JSON-RPC lines on stdin/stdout. Only ever built from the operator's own configuration.
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String> },
}

impl Spec {
    pub fn describe(&self) -> String {
        match self {
            Spec::Http { url, .. } => url.clone(),
            Spec::Stdio { command, args, .. } => format!("{command} {}", args.join(" ")).trim().to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive: Option<bool>,
    #[serde(default, rename = "idempotentHint")]
    pub idempotent: Option<bool>,
    #[serde(default, rename = "openWorldHint")]
    pub open_world: Option<bool>,
}

/// A tool as a server lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: Option<ToolAnnotations>,
}

/// What `tools/call` returned, flattened to what an agent can read.
#[derive(Debug, Clone, Default)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub instructions: Option<String>,
}

#[async_trait]
trait Conn: Send + Sync {
    /// One request; the result of the matching response.
    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, McpError>;
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;
    /// Tell the server the session is over (best effort).
    async fn close(&self);
    /// The protocol revision the server agreed to, for the headers that follow `initialize`.
    fn set_protocol(&self, _version: &str) {}
}

/// An open session with one server.
pub struct McpClient {
    conn: Arc<dyn Conn>,
    pub info: ServerInfo,
}

impl McpClient {
    pub async fn connect(spec: &Spec, timeout: Duration) -> Result<Self, McpError> {
        let conn: Arc<dyn Conn> = match spec {
            Spec::Http { url, headers } => Arc::new(HttpConn::new(url, headers)?),
            Spec::Stdio { command, args, env } => Arc::new(StdioConn::spawn(command, args, env)?),
        };
        let init = conn
            .call(
                "initialize",
                json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "dfae", "version": env!("CARGO_PKG_VERSION")}}),
                timeout,
            )
            .await?;
        if let Some(v) = init.get("protocolVersion").and_then(|v| v.as_str()) {
            conn.set_protocol(v);
        }
        let info = ServerInfo {
            name: init["serverInfo"]["name"].as_str().unwrap_or_default().to_string(),
            version: init["serverInfo"]["version"].as_str().unwrap_or_default().to_string(),
            instructions: init["instructions"].as_str().map(|s| s.to_string()).filter(|s| !s.trim().is_empty()),
        };
        conn.notify("notifications/initialized", json!({})).await?;
        Ok(Self { conn, info })
    }

    /// Every tool the server lists (at most 200: a server with more is a catalogue, not a toolbox).
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<RemoteTool>, McpError> {
        let mut tools: Vec<RemoteTool> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..8 {
            let params = cursor.as_ref().map(|c| json!({"cursor": c})).unwrap_or_else(|| json!({}));
            let r = self.conn.call("tools/list", params, timeout).await?;
            let page: Vec<RemoteTool> =
                serde_json::from_value(r.get("tools").cloned().unwrap_or(json!([]))).map_err(|e| McpError::Protocol(format!("tools/list: {e}")))?;
            tools.extend(page);
            cursor = r.get("nextCursor").and_then(|c| c.as_str()).map(String::from);
            if cursor.is_none() || tools.len() >= 200 {
                break;
            }
        }
        tools.truncate(200);
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value, timeout: Duration) -> Result<CallResult, McpError> {
        let r = self.conn.call("tools/call", json!({"name": name, "arguments": arguments}), timeout).await?;
        Ok(flatten_result(&r))
    }

    pub async fn close(&self) {
        self.conn.close().await;
    }
}

/// `content` blocks as text; a result with only structured content is shown as its JSON.
pub fn flatten_result(r: &Value) -> CallResult {
    let mut parts: Vec<String> = Vec::new();
    for block in r.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
            Some("image") | Some("audio") => {
                let kind = block["type"].as_str().unwrap_or("image");
                let mime = block.get("mimeType").and_then(|m| m.as_str()).unwrap_or("?");
                let bytes = block.get("data").and_then(|d| d.as_str()).map(|d| d.len() * 3 / 4).unwrap_or(0);
                parts.push(format!("[{kind}: {mime}, {bytes} bytes]"));
            }
            Some("resource") => {
                let res = &block["resource"];
                match res.get("text").and_then(|t| t.as_str()) {
                    Some(t) => parts.push(t.to_string()),
                    None => parts.push(format!("[resource {}]", res.get("uri").and_then(|u| u.as_str()).unwrap_or("?"))),
                }
            }
            Some("resource_link") => parts.push(format!("[resource link {}]", block.get("uri").and_then(|u| u.as_str()).unwrap_or("?"))),
            _ => {}
        }
    }
    if parts.is_empty()
        && let Some(s) = r.get("structuredContent").filter(|s| !s.is_null())
    {
        parts.push(s.to_string());
    }
    CallResult { text: parts.join("\n"), is_error: r.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) }
}

// ------------------------------------------------------------------ HTTP ----

struct HttpConn {
    http: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: parking_lot::Mutex<Option<String>>,
    protocol: parking_lot::Mutex<String>,
    next: AtomicU64,
}

impl HttpConn {
    fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<Self, McpError> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(McpError::Transport(format!("not an HTTP(S) URL: {url}")));
        }
        let http = reqwest::Client::builder().build().map_err(|e| McpError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            url: url.to_string(),
            headers: headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            session: Default::default(),
            protocol: parking_lot::Mutex::new(PROTOCOL_VERSION.into()),
            next: AtomicU64::new(1),
        })
    }

    fn request(&self, body: &Value, timeout: Duration) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .post(&self.url)
            .timeout(timeout)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", self.protocol.lock().clone())
            .json(body);
        if let Some(s) = self.session.lock().clone() {
            rb = rb.header("mcp-session-id", s);
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        rb
    }

    async fn post(&self, body: &Value, timeout: Duration) -> Result<reqwest::Response, McpError> {
        let resp =
            self.request(body, timeout).send().await.map_err(|e| {
                if e.is_timeout() { McpError::Timeout(timeout.as_secs()) } else { McpError::Transport(format!("cannot reach {}: {e}", self.url)) }
            })?;
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock() = Some(s.to_string());
        }
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let challenge = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).map(String::from);
        let text: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
        let msg = format!("{} answered HTTP {status}: {text}", self.url);
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(McpError::Unauthorized { status: status.as_u16(), challenge, message: msg });
        }
        // a session the server has forgotten, a server that is restarting or busy: worth another try after reconnecting
        Err(if status.is_server_error() || status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            McpError::Transport(msg)
        } else {
            McpError::Protocol(msg)
        })
    }
}

#[async_trait]
impl Conn for HttpConn {
    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, McpError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self.post(&body, timeout).await?;
        let is_sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("text/event-stream"));
        if !is_sse {
            let text = resp.text().await.map_err(|e| McpError::Transport(e.to_string()))?;
            let v: Value =
                serde_json::from_str(&text).map_err(|e| McpError::Protocol(format!("not JSON ({e}): {}", text.chars().take(120).collect::<String>())))?;
            let msgs = match v {
                Value::Array(a) => a,
                one => vec![one],
            };
            return msgs
                .iter()
                .find_map(|m| response_to(m, id))
                .unwrap_or_else(|| Err(McpError::Protocol("the reply holds no response to the request".into())));
        }
        // a stream of server-sent events: notifications and progress may come first, the response ends it
        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let next = tokio::time::timeout_at(deadline, stream.next()).await.map_err(|_| McpError::Timeout(timeout.as_secs()))?;
            match next {
                Some(Ok(chunk)) => {
                    for data in parser.push(&chunk) {
                        if let Ok(m) = serde_json::from_str::<Value>(&data)
                            && let Some(r) = response_to(&m, id)
                        {
                            return r;
                        }
                    }
                }
                Some(Err(e)) => return Err(McpError::Transport(format!("the event stream broke: {e}"))),
                None => {
                    for data in parser.finish() {
                        if let Ok(m) = serde_json::from_str::<Value>(&data)
                            && let Some(r) = response_to(&m, id)
                        {
                            return r;
                        }
                    }
                    return Err(McpError::Transport("the event stream ended without a response".into()));
                }
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.post(&body, Duration::from_secs(15)).await.map(|_| ())
    }

    async fn close(&self) {
        let Some(session) = self.session.lock().take() else { return };
        let mut rb = self.http.delete(&self.url).timeout(Duration::from_secs(5)).header("mcp-session-id", session);
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        let _ = rb.send().await;
    }

    fn set_protocol(&self, version: &str) {
        *self.protocol.lock() = version.to_string();
    }
}

/// `Some(result)` when `m` is the response to request `id`.
fn response_to(m: &Value, id: u64) -> Option<Result<Value, McpError>> {
    if m.get("id").and_then(|i| i.as_u64()) != Some(id) || m.get("method").is_some() {
        return None;
    }
    if let Some(e) = m.get("error") {
        return Some(Err(McpError::Rpc {
            code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000),
            message: e.get("message").and_then(|c| c.as_str()).unwrap_or("error").to_string(),
        }));
    }
    m.get("result").map(|r| Ok(r.clone()))
}

/// Server-sent events: `data:` lines up to a blank line make one message.
#[derive(Default)]
struct SseParser {
    buf: String,
    pending: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // a multi-byte character may be split between two chunks
        self.pending.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(s) => s.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&self.pending[..valid]).replace("\r\n", "\n");
        self.pending.drain(..valid);
        self.buf.push_str(&text);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find("\n\n") {
            let event: String = self.buf.drain(..i + 2).collect();
            if let Some(d) = Self::data(&event) {
                out.push(d);
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buf);
        Self::data(&rest).into_iter().collect()
    }

    fn data(event: &str) -> Option<String> {
        let lines: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(|l| l.strip_prefix(' ').unwrap_or(l)).collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

// ----------------------------------------------------------------- stdio ----

type Pending = Arc<parking_lot::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>;

struct StdioConn {
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    pending: Pending,
    next: AtomicU64,
    child: tokio::sync::Mutex<tokio::process::Child>,
    dead: Arc<std::sync::atomic::AtomicBool>,
}

impl StdioConn {
    fn spawn(command: &str, args: &[String], env: &BTreeMap<String, String>) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        // a server gets a plain environment plus what the operator wrote for it, not the engine's secrets (API keys, tokens)
        cmd.env_clear();
        for k in [
            "PATH",
            "HOME",
            "LANG",
            "TMPDIR",
            "USER",
            "SHELL",
            "NODE_PATH",
            "XDG_CACHE_HOME",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
        ] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.envs(env);
        let mut child = cmd.spawn().map_err(|e| McpError::Transport(format!("cannot start `{command}`: {e}")))?;
        let stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take().ok_or_else(|| McpError::Transport("no stdin".into()))?));
        let stdout = child.stdout.take().ok_or_else(|| McpError::Transport("no stdout".into()))?;
        let stderr = child.stderr.take();
        let pending: Pending = Default::default();
        let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (p, d, w) = (pending.clone(), dead.clone(), stdin.clone());
        // reader: a response goes to the request waiting for it; a request from the server is answered on the spot
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
                if m.get("method").is_some() {
                    if let Some(id) = m.get("id") {
                        let answer = if m["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                        };
                        let _ = w.lock().await.write_all(format!("{answer}\n").as_bytes()).await;
                    }
                    continue;
                }
                if let Some(id) = m.get("id").and_then(|i| i.as_u64())
                    && let Some(tx) = p.lock().remove(&id)
                {
                    let _ = tx.send(m);
                }
            }
            // the process ended: whoever waits is told by the closed channel
            d.store(true, Ordering::Relaxed);
            p.lock().clear();
        });
        if let Some(err) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::debug!(target: "mcp_stderr", "{l}");
                }
            });
        }
        Ok(Self { stdin, pending, next: AtomicU64::new(1), child: tokio::sync::Mutex::new(child), dead })
    }

    async fn write(&self, m: &Value) -> Result<(), McpError> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(McpError::Transport("the server process has exited".into()));
        }
        let mut line = m.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|e| McpError::Transport(format!("write failed: {e}")))?;
        stdin.flush().await.map_err(|e| McpError::Transport(format!("write failed: {e}")))
    }
}

#[async_trait]
impl Conn for StdioConn {
    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, McpError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id, tx);
        if let Err(e) = self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await {
            self.pending.lock().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(McpError::Timeout(timeout.as_secs()))
            }
            Ok(Err(_)) => Err(McpError::Transport("the server process exited".into())),
            Ok(Ok(m)) => response_to(&m, id).unwrap_or_else(|| Err(McpError::Protocol("a response without result or error".into()))),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    async fn close(&self) {
        let _ = self.child.lock().await.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_are_split_even_across_chunks_and_characters() {
        let mut p = SseParser::default();
        let msg = "event: message\r\ndata: {\"a\": \"héllo\"}\r\n\r\ndata: {\"b\":1}\n\n";
        let bytes = msg.as_bytes();
        let cut = msg.find('é').unwrap() + 1; // in the middle of the two bytes of é
        let mut got = p.push(&bytes[..cut]);
        got.extend(p.push(&bytes[cut..]));
        assert_eq!(got, vec!["{\"a\": \"héllo\"}".to_string(), "{\"b\":1}".to_string()]);
        // an event without the closing blank line is still delivered at the end of the stream
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"c\":2}\n").is_empty());
        assert_eq!(p.finish(), vec!["{\"c\":2}".to_string()]);
    }

    #[test]
    fn a_result_is_read_as_text() {
        let r = flatten_result(
            &json!({"content": [{"type": "text", "text": "a"}, {"type": "image", "mimeType": "image/png", "data": "AAAA"}, {"type": "resource", "resource": {"uri": "file:///x", "text": "body"}}], "isError": false}),
        );
        assert_eq!(r.text, "a\n[image: image/png, 3 bytes]\nbody");
        assert!(!r.is_error);
        let r = flatten_result(&json!({"content": [], "structuredContent": {"n": 1}, "isError": true}));
        assert_eq!((r.text.as_str(), r.is_error), ("{\"n\":1}", true));
    }

    #[test]
    fn only_the_matching_response_counts() {
        assert!(response_to(&json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}), 1).is_none());
        assert!(response_to(&json!({"jsonrpc": "2.0", "id": 2, "result": {}}), 1).is_none());
        assert!(response_to(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}), 1).is_none(), "a request from the server is not our response");
        assert_eq!(response_to(&json!({"id": 1, "result": {"x": 1}}), 1).unwrap().unwrap()["x"], 1);
        assert!(matches!(response_to(&json!({"id": 1, "error": {"code": -32602, "message": "bad"}}), 1).unwrap(), Err(McpError::Rpc { code: -32602, .. })));
    }
}
