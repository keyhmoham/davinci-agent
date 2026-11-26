//! The MCP hub: the servers the engine knows (the operator's, and those found in the directory), their tools in the engine's
//! tool registry, and the finding of new ones on demand.

use crate::client::{CallResult, McpClient, McpError, Spec};
use crate::glama::{AuthType, Connector, Glama, GlamaError};
use crate::tool::{McpTool, server_of, slug};
use dfae_tools::ToolRegistry;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// One server's connection, opened when first needed and again after it breaks.
pub(crate) struct Link {
    spec: Spec,
    client: tokio::sync::Mutex<Option<Arc<McpClient>>>,
    connect_timeout: Duration,
    call_timeout: Duration,
}

impl Link {
    fn new(spec: Spec, connect_timeout: Duration, call_timeout: Duration) -> Arc<Self> {
        Arc::new(Self { spec, client: tokio::sync::Mutex::new(None), connect_timeout, call_timeout })
    }

    async fn client(&self) -> Result<Arc<McpClient>, McpError> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let c = Arc::new(McpClient::connect(&self.spec, self.connect_timeout).await?);
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn reset(&self, broken: &Arc<McpClient>) {
        let mut slot = self.client.lock().await;
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, broken)) {
            *slot = None;
        }
        drop(slot);
        broken.close().await;
    }

    /// One tool call; a broken connection is opened again once.
    pub(crate) async fn call(&self, tool: &str, args: Value) -> Result<CallResult, McpError> {
        let mut last = McpError::Transport("not connected".into());
        for _ in 0..2 {
            let client = self.client().await?;
            match client.call_tool(tool, args.clone(), self.call_timeout).await {
                Err(e) if e.retryable() => {
                    self.reset(&client).await;
                    last = e;
                }
                other => return other,
            }
        }
        Err(last)
    }
}

/// Where a server came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The operator's own configuration: trusted, may be a local command.
    Config,
    /// Added through the API.
    Api,
    /// Found in the Glama directory for a request.
    Glama,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Config => "config",
            Source::Api => "api",
            Source::Glama => "glama",
        }
    }
}

pub struct Server {
    pub name: String,
    pub source: Source,
    /// The server's own page in the directory (credit and link as the directory's licence asks).
    pub listing: Option<String>,
    pub description: String,
    spec: Spec,
    /// Part of the operator's configuration: offered to every task, not only to those it looks relevant to.
    always: bool,
    link: Arc<Link>,
    tools: Mutex<Vec<String>>,
    doc: Mutex<String>,
    error: Mutex<Option<String>>,
    instructions: Mutex<Option<String>>,
    /// When the last attempt to open it failed: a server that is down is not retried on every request.
    failed_at: Mutex<Option<std::time::Instant>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolView {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerView {
    pub name: String,
    pub source: &'static str,
    pub endpoint: String,
    pub listing: Option<String>,
    pub connected: bool,
    pub error: Option<String>,
    pub tools: Vec<ToolView>,
}

/// A server found in the directory and made available.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub server: String,
    pub title: String,
    pub listing: String,
    pub tools: Vec<String>,
    /// The connector was already known: nothing new was connected.
    pub known: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum DiscoverError {
    #[error("MCP discovery is not configured (set GLAMA_API_KEY)")]
    NotConfigured,
    #[error(transparent)]
    Directory(#[from] GlamaError),
    #[error("no usable MCP server for \"{query}\" in the directory{}", if .skipped.is_empty() { String::new() } else { format!(" ({})", .skipped.join("; ")) })]
    NoneUsable { query: String, skipped: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub connect_timeout: Duration,
    pub call_timeout: Duration,
    /// How many servers one discovery adds at most.
    pub max_auto: usize,
    /// Credentials for directory servers that want an API key: `DFAE_MCP_KEY_<SLUG>` in the environment, sent as a bearer token.
    pub keys: BTreeMap<String, String>,
    /// Where servers found in the directory are remembered.
    pub state_path: Option<PathBuf>,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { connect_timeout: Duration::from_secs(20), call_timeout: Duration::from_secs(60), max_auto: 2, keys: BTreeMap::new(), state_path: None }
    }
}

impl HubConfig {
    /// `DFAE_MCP_KEY_SENTRY=…` → `sentry`.
    pub fn keys_from_env() -> BTreeMap<String, String> {
        std::env::vars()
            .filter_map(|(k, v)| k.strip_prefix("DFAE_MCP_KEY_").map(|n| (slug(n), v.trim().to_string())))
            .filter(|(n, v)| !n.is_empty() && !v.is_empty())
            .collect()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    servers: Vec<Saved>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Saved {
    name: String,
    url: String,
    listing: String,
    description: String,
    /// The slug a key was looked up under (`keys`), if the server needed one: the key itself is never written down.
    key: Option<String>,
}

pub struct McpHub {
    registry: ToolRegistry,
    cfg: HubConfig,
    glama: Option<Glama>,
    servers: RwLock<BTreeMap<String, Arc<Server>>>,
    /// One discovery at a time: two tasks asking for the same capability should connect it once.
    discovery: tokio::sync::Mutex<()>,
    /// Searches that found nothing usable, and when: the same words are not searched again for a while.
    misses: Mutex<std::collections::HashMap<String, (std::time::Instant, DiscoverError)>>,
}

impl McpHub {
    pub fn new(registry: ToolRegistry, cfg: HubConfig, glama: Option<Glama>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            cfg,
            glama,
            servers: RwLock::new(BTreeMap::new()),
            discovery: tokio::sync::Mutex::new(()),
            misses: Mutex::new(Default::default()),
        })
    }

    pub fn discovery_enabled(&self) -> bool {
        self.glama.is_some()
    }

    pub fn max_auto(&self) -> usize {
        self.cfg.max_auto
    }

    // ---------------------------------------------------------------- servers ----

    /// Make a server known and open it. Its tools join the tool registry (hidden from tasks until a task attaches the server).
    pub async fn add(&self, name: &str, spec: Spec, source: Source, listing: Option<String>, description: &str, always: bool) -> Result<Arc<Server>, McpError> {
        let name = slug(name);
        if name.is_empty() {
            return Err(McpError::Protocol("a server needs a name".into()));
        }
        let existing = self.servers.read().get(&name).cloned();
        let server = match existing {
            Some(s) if s.spec == spec => s,
            Some(_) => return Err(McpError::Protocol(format!("an MCP server named '{name}' already exists"))),
            None => {
                let s = Arc::new(Server {
                    name: name.clone(),
                    source,
                    listing,
                    description: description.to_string(),
                    link: Link::new(spec.clone(), self.cfg.connect_timeout, self.cfg.call_timeout),
                    spec,
                    always,
                    tools: Mutex::new(vec![]),
                    doc: Mutex::new(String::new()),
                    error: Mutex::new(None),
                    instructions: Mutex::new(None),
                    failed_at: Mutex::new(None),
                });
                self.servers.write().insert(name.clone(), s.clone());
                s
            }
        };
        *server.failed_at.lock() = None;
        self.load(&server).await?;
        Ok(server)
    }

    /// Connect (again, if needed) and register the server's tools.
    async fn load(&self, server: &Arc<Server>) -> Result<(), McpError> {
        if !server.tools.lock().is_empty() {
            return Ok(());
        }
        if let Some(at) = *server.failed_at.lock()
            && at.elapsed() < Duration::from_secs(60)
        {
            return Err(McpError::Transport(server.error.lock().clone().unwrap_or_else(|| "not available".into())));
        }
        let result = async {
            let client = server.link.client().await?;
            let listed = client.list_tools(self.cfg.connect_timeout).await?;
            Ok::<_, McpError>((client.info.instructions.clone(), listed))
        }
        .await;
        match result {
            Ok((instructions, listed)) => {
                // what nobody vetted (a directory's server) is asked about before anything but a read-only tool runs
                let vetted = server.source != Source::Glama;
                let mut taken: Vec<String> = self.registry.names();
                let mut names = Vec::new();
                let mut doc = format!("{} {}", server.name.replace('_', " "), server.description);
                for t in &listed {
                    let tool = McpTool::new(&server.name, t, server.link.clone(), vetted, &mut taken);
                    let meta = dfae_core::Tool::metadata(&tool);
                    doc.push_str(&format!(" {} {}", t.name.replace(['_', '-'], " "), t.description.clone().unwrap_or_default()));
                    names.push(meta.name.clone());
                    self.registry.register(Arc::new(tool));
                }
                *server.tools.lock() = names;
                *server.doc.lock() = doc;
                *server.instructions.lock() = instructions;
                *server.error.lock() = None;
                *server.failed_at.lock() = None;
                Ok(())
            }
            Err(e) => {
                *server.error.lock() = Some(e.to_string());
                *server.failed_at.lock() = Some(std::time::Instant::now());
                Err(e)
            }
        }
    }

    pub async fn remove(&self, name: &str) -> bool {
        let Some(s) = self.servers.write().remove(name) else { return false };
        self.registry.unregister_prefix(&format!("mcp__{}__", s.name));
        if let Ok(c) = s.link.client().await {
            c.close().await;
        }
        self.save();
        true
    }

    pub fn servers(&self) -> Vec<ServerView> {
        self.servers.read().values().map(|s| self.view(s)).collect()
    }

    pub fn server(&self, name: &str) -> Option<ServerView> {
        self.servers.read().get(name).map(|s| self.view(s))
    }

    fn view(&self, s: &Arc<Server>) -> ServerView {
        let names = s.tools.lock().clone();
        ServerView {
            name: s.name.clone(),
            source: s.source.as_str(),
            endpoint: match &s.spec {
                Spec::Http { url, .. } => url.clone(),
                Spec::Stdio { command, .. } => format!("stdio: {command}"),
            },
            listing: s.listing.clone(),
            connected: !names.is_empty(),
            error: s.error.lock().clone(),
            tools: names
                .iter()
                .filter_map(|n| self.registry.meta(n))
                .map(|m| ToolView { name: m.name, description: m.description.chars().take(160).collect() })
                .collect(),
        }
    }

    /// What the server says about using its tools (not shown to the user; it can steer the model, so it is only offered as a hint).
    pub fn instructions(&self, server: &str) -> Option<String> {
        self.servers.read().get(server).and_then(|s| s.instructions.lock().clone())
    }

    pub fn tools_of(&self, server: &str) -> Vec<String> {
        self.servers.read().get(server).map(|s| s.tools.lock().clone()).unwrap_or_default()
    }

    // -------------------------------------------------------------- relevance ----

    /// Servers already known whose tools fit the request, best first; the operator's `always` servers come with them.
    /// A server that is known but not connected is connected here (it may have been down when the engine started).
    pub async fn relevant(&self, request: &str) -> Vec<String> {
        let all: Vec<Arc<Server>> = self.servers.read().values().cloned().collect();
        let mut hits: Vec<(usize, String)> = Vec::new();
        for s in all {
            if s.tools.lock().is_empty() && self.load(&s).await.is_err() {
                continue;
            }
            let score = relevance(request, &s.name, &s.doc.lock());
            if s.always || score > 0 {
                hits.push((if s.always { usize::MAX } else { score }, s.name.clone()));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, n)| n).collect()
    }

    // -------------------------------------------------------------- discovery ----

    /// What the directory has for `query` (nothing is connected).
    pub async fn search(&self, query: &str, first: u32) -> Result<Vec<Connector>, DiscoverError> {
        let glama = self.glama.as_ref().ok_or(DiscoverError::NotConfigured)?;
        Ok(glama.search(query, first).await?)
    }

    /// Search the directory for what `query` describes, connect the best servers that can be used as they are, and return them.
    pub async fn discover(&self, query: &str) -> Result<Vec<Found>, DiscoverError> {
        let glama = self.glama.as_ref().ok_or(DiscoverError::NotConfigured)?;
        let _one_at_a_time = self.discovery.lock().await;
        let key = query.to_lowercase();
        if let Some((at, err)) = self.misses.lock().get(&key)
            && at.elapsed() < Duration::from_secs(600)
        {
            return Err(err.clone());
        }
        let found = glama.search(query, 20).await?;
        let mut skipped: Vec<String> = Vec::new();
        let mut candidates: Vec<(Connector, Option<String>)> = Vec::new();
        for c in found {
            let key = self.cfg.keys.get(&slug(&c.slug)).cloned();
            let usable = c.open()
                || (matches!(c.auth, AuthType::ApiKey) && key.is_some() && !c.deprecated && c.transport == "streamable_http" && c.healthy != Some(false));
            if usable {
                candidates.push((c, key));
            } else if skipped.len() < 4 {
                skipped.push(match c.auth {
                    AuthType::Oauth2 | AuthType::Basic if !c.deprecated => {
                        format!("{} needs {}", c.name, if c.auth == AuthType::Oauth2 { "OAuth" } else { "a login" })
                    }
                    AuthType::ApiKey if !c.deprecated => format!("{} needs an API key (DFAE_MCP_KEY_{})", c.name, slug(&c.slug).to_uppercase()),
                    _ if c.paid() => format!("{} is a paid service", c.name),
                    _ => format!("{} is not available", c.name),
                });
            }
        }
        // healthy ones first, the directory's order otherwise
        candidates.sort_by_key(|(c, _)| match c.healthy {
            Some(true) => 0,
            _ => 1,
        });
        let mut result: Vec<Found> = Vec::new();
        let batch: Vec<(Connector, Option<String>)> = candidates.into_iter().take(self.cfg.max_auto + 1).collect();
        let attempts = batch.iter().map(|(c, key)| self.add_connector(c, key.clone()));
        for (outcome, (c, _)) in futures_util::future::join_all(attempts).await.into_iter().zip(&batch) {
            match outcome {
                Ok((server, known)) if result.len() < self.cfg.max_auto => {
                    result.push(Found {
                        server: server.name.clone(),
                        title: c.name.clone(),
                        listing: c.listing.clone(),
                        tools: self.tools_of(&server.name),
                        known,
                    });
                }
                Ok((server, false)) => {
                    // connected but not needed: one more than the limit was tried, in case one of them was down
                    self.remove(&server.name).await;
                }
                Ok(_) => {}
                Err(e) => tracing::info!(server = %c.name, "directory server not usable: {e}"),
            }
        }
        if result.is_empty() {
            let err = DiscoverError::NoneUsable { query: query.to_string(), skipped };
            self.misses.lock().insert(key, (std::time::Instant::now(), err.clone()));
            return Err(err);
        }
        self.save();
        Ok(result)
    }

    async fn add_connector(&self, c: &Connector, key: Option<String>) -> Result<(Arc<Server>, bool), McpError> {
        let known = self.servers.read().values().find(|s| matches!(&s.spec, Spec::Http { url, .. } if *url == c.endpoint)).cloned();
        if let Some(s) = known {
            self.load(&s).await?;
            return Ok((s, true));
        }
        let mut headers = BTreeMap::new();
        if let Some(k) = key {
            headers.insert("Authorization".to_string(), format!("Bearer {k}"));
        }
        let mut name = slug(&c.slug);
        if self.servers.read().contains_key(&name) {
            name = slug(&format!("{}_{}", c.namespace, c.slug));
        }
        let description = c.description.clone().unwrap_or_default();
        let server = self.add(&name, Spec::Http { url: c.endpoint.clone(), headers }, Source::Glama, Some(c.listing.clone()), &description, false).await?;
        Ok((server, false))
    }

    // ----------------------------------------------------------------- memory ----

    fn save(&self) {
        let Some(path) = &self.cfg.state_path else { return };
        let servers: Vec<Saved> = self
            .servers
            .read()
            .values()
            .filter(|s| s.source == Source::Glama)
            .filter_map(|s| match &s.spec {
                Spec::Http { url, headers } => Some(Saved {
                    name: s.name.clone(),
                    url: url.clone(),
                    listing: s.listing.clone().unwrap_or_default(),
                    description: s.description.clone(),
                    key: (!headers.is_empty()).then(|| s.name.clone()),
                }),
                _ => None,
            })
            .collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(&State { servers }) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Servers found in earlier runs are known again (and opened in the background).
    pub fn restore(self: &Arc<Self>) {
        let Some(path) = &self.cfg.state_path else { return };
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(state) = serde_json::from_str::<State>(&text) else { return };
        for saved in state.servers {
            let hub = self.clone();
            tokio::spawn(async move {
                let mut headers = BTreeMap::new();
                if saved.key.is_some() {
                    match hub.cfg.keys.get(&slug(&saved.name)) {
                        Some(k) => {
                            headers.insert("Authorization".to_string(), format!("Bearer {k}"));
                        }
                        None => return,
                    }
                }
                if let Err(e) = hub
                    .add(&saved.name, Spec::Http { url: saved.url.clone(), headers }, Source::Glama, Some(saved.listing.clone()), &saved.description, false)
                    .await
                {
                    tracing::warn!(server = %saved.name, "could not reopen an MCP server found earlier: {e}");
                }
            });
        }
    }

    // ----------------------------------------------------------------- config ----

    /// The operator's servers, from a file in the usual `{"mcpServers": {name: {command, args, env} | {url, headers}}}` shape.
    /// `${VAR}` in a value is replaced by the environment variable, so secrets stay out of the file. Servers open in the background.
    pub fn load_config(self: &Arc<Self>, text: &str) -> Result<usize, String> {
        let specs = parse_config(text)?;
        let n = specs.len();
        for (name, spec, always) in specs {
            let hub = self.clone();
            tokio::spawn(async move {
                if let Err(e) = hub.add(&name, spec, Source::Config, None, "", always).await {
                    tracing::warn!(server = %name, "MCP server from the configuration is not available: {e}");
                }
            });
        }
        Ok(n)
    }

    /// Names of the servers behind a set of tool names.
    pub fn servers_of(&self, tools: &[String]) -> HashSet<String> {
        tools.iter().filter_map(|t| server_of(t)).map(String::from).collect()
    }
}

/// How well a request fits a server: the number of distinct words they share, not counting the verbs every tool has. A word that is
/// the server's own name (`github`) is enough on its own.
fn relevance(request: &str, name: &str, doc: &str) -> usize {
    use dfae_core::text::content_tokens;
    const GENERIC: &[&str] = &[
        "list", "get", "create", "search", "find", "show", "read", "write", "update", "delete", "tool", "server", "mcp", "use", "make", "set", "add", "new",
        "info", "data", "query", "fetch", "send", "item", "result", "name", "file", "text", "content", "type", "value", "id", "user", "api", "run", "call",
    ];
    let wanted: HashSet<String> = content_tokens(request).into_iter().filter(|t| t.len() > 2 && !GENERIC.contains(&t.as_str())).collect();
    let have: HashSet<String> = content_tokens(doc).into_iter().collect();
    let in_name: HashSet<String> = content_tokens(&name.replace('_', " ")).into_iter().collect();
    let shared = wanted.intersection(&have).count();
    if wanted.intersection(&in_name).next().is_some() {
        shared.max(2)
    } else if shared >= 2 {
        shared
    } else {
        0
    }
}

type ParsedConfig = Vec<(String, Spec, bool)>;

fn parse_config(text: &str) -> Result<ParsedConfig, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let servers = v.get("mcpServers").or_else(|| v.get("servers")).and_then(|s| s.as_object()).ok_or("expected an object \"mcpServers\"")?;
    let expand = |s: &str| expand_env(s);
    let mut out = Vec::new();
    for (name, def) in servers {
        if def.get("disabled").and_then(|d| d.as_bool()) == Some(true) {
            continue;
        }
        let map = |key: &str| -> BTreeMap<String, String> {
            def.get(key)
                .and_then(|m| m.as_object())
                .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), expand(v)))).collect())
                .unwrap_or_default()
        };
        let always = def.get("always").and_then(|a| a.as_bool()).unwrap_or(false);
        let spec = if let Some(url) = def.get("url").and_then(|u| u.as_str()) {
            Spec::Http { url: expand(url), headers: map("headers") }
        } else if let Some(cmd) = def.get("command").and_then(|c| c.as_str()) {
            let args = def.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(expand)).collect()).unwrap_or_default();
            Spec::Stdio { command: expand(cmd), args, env: map("env") }
        } else {
            return Err(format!("server '{name}' has neither \"url\" nor \"command\""));
        };
        out.push((name.clone(), spec, always));
    }
    Ok(out)
}

/// `${NAME}` → the environment variable (empty when unset).
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                out.push_str(&std::env::var(&rest[i + 2..i + 2 + j]).unwrap_or_default());
                rest = &rest[i + 3 + j..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_fits_the_server_it_names_or_whose_tools_it_describes() {
        let doc = "github Repository hosting. list issues create pull request search repositories";
        assert!(relevance("show my open github issues", "github", doc) >= 2, "the server's own name is enough");
        assert!(relevance("search repositories for rust crates and open a pull request", "other", doc) >= 2);
        assert_eq!(relevance("summarise the weather in Paris", "github", doc), 0);
        assert_eq!(relevance("list the files", "github", doc), 0, "list/files are what every tool has");
    }

    #[test]
    fn the_usual_config_shape_is_read_and_secrets_come_from_the_environment() {
        unsafe { std::env::set_var("DFAE_TEST_MCP_TOKEN", "s3cret") };
        let cfg = parse_config(
            r#"{"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "pkg", "${DFAE_TEST_MCP_TOKEN}"], "env": {"K": "${DFAE_TEST_MCP_TOKEN}"}},
                "remote": {"url": "https://x.example/mcp", "headers": {"Authorization": "Bearer ${DFAE_TEST_MCP_TOKEN}"}, "always": true},
                "off": {"command": "x", "disabled": true}
            }}"#,
        )
        .unwrap();
        assert_eq!(cfg.len(), 2);
        let fs = cfg.iter().find(|(n, ..)| n == "fs").unwrap();
        assert_eq!(
            fs.1,
            Spec::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "pkg".into(), "s3cret".into()],
                env: [("K".to_string(), "s3cret".to_string())].into()
            }
        );
        let remote = cfg.iter().find(|(n, ..)| n == "remote").unwrap();
        assert!(remote.2);
        assert!(matches!(&remote.1, Spec::Http { headers, .. } if headers["Authorization"] == "Bearer s3cret"));
        assert!(parse_config(r#"{"mcpServers": {"x": {}}}"#).is_err());
    }
}
