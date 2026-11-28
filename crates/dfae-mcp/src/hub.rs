//! The MCP hub: the servers the engine knows (the operator's, and those found in the directory), their tools in the engine's
//! tool registry, and the finding of new ones on demand.

use crate::client::{CallResult, McpClient, McpError, Spec};
use crate::glama::{AuthType, Connector, Detail, DirTool, Glama, GlamaError};
use crate::oauth::{self, Grant, Store};
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
    /// A call was refused for want of credentials.
    locked: std::sync::atomic::AtomicBool,
    /// The user's sign-in, for a server that wants one.
    grant: Option<Arc<Grant>>,
}

impl Link {
    fn new(spec: Spec, connect_timeout: Duration, call_timeout: Duration, grant: Option<Arc<Grant>>) -> Arc<Self> {
        Arc::new(Self { spec, client: tokio::sync::Mutex::new(None), connect_timeout, call_timeout, locked: Default::default(), grant })
    }

    fn locked(&self) -> bool {
        self.locked.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn client(&self) -> Result<Arc<McpClient>, McpError> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        // an access token that has expired is refreshed before it is sent
        if let Some(g) = &self.grant
            && g.token().is_none()
        {
            g.refresh().await;
        }
        let c = Arc::new(McpClient::connect_with(&self.spec, self.connect_timeout, self.grant.clone()).await?);
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

    /// One tool call; a broken connection is opened again once, and a refused sign-in is refreshed once.
    pub(crate) async fn call(&self, tool: &str, args: Value) -> Result<CallResult, McpError> {
        let mut last = McpError::Transport("not connected".into());
        let mut refreshed = false;
        for _ in 0..2 {
            let client = self.client().await?;
            match client.call_tool(tool, args.clone(), self.call_timeout).await {
                Err(e) if e.retryable() => {
                    self.reset(&client).await;
                    last = e;
                }
                Err(e) if e.needs_login() => {
                    if let Some(g) = &self.grant
                        && !refreshed
                        && g.refresh().await
                    {
                        refreshed = true;
                        self.reset(&client).await;
                        last = e;
                        continue;
                    }
                    self.locked.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(e);
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

/// What a task needs from the directory: the capability in a tool's own words, and the searches that may find it (most specific first).
#[derive(Debug, Clone, PartialEq)]
pub struct Need {
    pub capability: String,
    pub queries: Vec<String>,
}

impl Need {
    /// A need that is only its query.
    pub fn query(q: &str) -> Self {
        Self { capability: q.to_string(), queries: vec![q.to_string()] }
    }
}

/// A directory server that can be used as it is and may serve a need, with what it can do (its tools, read before it is connected).
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub connector: Connector,
    pub tools: Vec<DirTool>,
    pub documentation: Option<String>,
    /// How well it fits the need: the directory's rank over all the searches, and its tools' words against the capability.
    pub score: f32,
    /// The user must sign in to it first (OAuth): it is offered, not connected.
    pub sign_in: bool,
    #[serde(skip)]
    key: Option<String>,
}

/// The candidates for a need, best first, and why the best of the others cannot be used.
#[derive(Debug, Clone)]
pub struct Shortlist {
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<String>,
}

/// How many candidates of a search have their details read and are ranked on their tools.
const SHORTLIST_DEPTH: usize = 8;
/// A directory entry's details are read again after this long.
const DETAIL_TTL: Duration = Duration::from_secs(3600);

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
    /// Where servers found in the directory are remembered (and, beside it in `mcp-oauth.json`, the user's sign-ins).
    pub state_path: Option<PathBuf>,
    /// Where an MCP provider sends the user's browser back after signing in (`…/v1/mcp/oauth/callback` of this engine's API). Without
    /// it, servers that want a sign-in are not offered.
    pub redirect_uri: Option<String>,
    /// What the sign-in store's key is derived from (`DFAE_SECRET_KEY`, else the API token). Without it, sign-ins live in memory only.
    pub secret: Option<String>,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            call_timeout: Duration::from_secs(60),
            max_auto: 2,
            keys: BTreeMap::new(),
            state_path: None,
            redirect_uri: None,
            secret: None,
        }
    }
}

/// A sign-in the user was asked for: the URL to open, and the key the provider's answer is matched by.
#[derive(Debug, Clone, Serialize)]
pub struct SignIn {
    pub state: String,
    pub url: String,
    pub server: String,
    pub title: String,
    pub listing: Option<String>,
}

#[derive(Debug, Clone)]
enum SignInState {
    Waiting,
    Done(Found),
    Failed(String),
}

struct Pending {
    flow: oauth::Flow,
    name: String,
    title: String,
    endpoint: String,
    listing: Option<String>,
    description: String,
    source: Source,
    started: std::time::Instant,
    status: tokio::sync::watch::Sender<SignInState>,
}

/// How long a sign-in link stays good.
const SIGN_IN_TTL: Duration = Duration::from_secs(1800);

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
    /// Endpoints of directory servers that asked for a login when called although listed as open: never connected again.
    #[serde(default)]
    locked: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Saved {
    name: String,
    url: String,
    listing: String,
    description: String,
    /// The slug a key was looked up under (`keys`), if the server needed one: the key itself is never written down.
    key: Option<String>,
    /// The user signed in to it (the tokens are in the sign-in store, not here).
    #[serde(default)]
    oauth: bool,
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
    /// Connector details already read, by connector id.
    details: Mutex<std::collections::HashMap<String, (std::time::Instant, Detail)>>,
    /// Endpoints of directory servers that refused calls for want of credentials.
    locked: Mutex<HashSet<String>>,
    /// The user's sign-ins and this engine's registrations with authorization servers.
    oauth: Arc<Store>,
    /// Sign-ins waiting for the provider's answer, by `state`.
    pending: Mutex<std::collections::HashMap<String, Pending>>,
}

impl McpHub {
    pub fn new(registry: ToolRegistry, cfg: HubConfig, glama: Option<Glama>) -> Arc<Self> {
        let cfg_oauth_path = cfg.state_path.as_ref().map(|p| p.with_file_name("mcp-oauth.json"));
        let cfg_secret = cfg.secret.clone();
        Arc::new(Self {
            registry,
            cfg,
            glama,
            servers: RwLock::new(BTreeMap::new()),
            discovery: tokio::sync::Mutex::new(()),
            misses: Mutex::new(Default::default()),
            details: Mutex::new(Default::default()),
            locked: Mutex::new(Default::default()),
            oauth: Store::open(cfg_oauth_path, cfg_secret.as_deref()),
            pending: Mutex::new(Default::default()),
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
        // a server the user signed in to before is opened with that sign-in
        let grant = match &spec {
            Spec::Http { url, headers } if !headers.contains_key("Authorization") => self.oauth.get(url).map(|s| Grant::new(s, Some(self.oauth.clone()))),
            _ => None,
        };
        self.add_with(name, spec, source, listing, description, always, grant).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn add_with(
        &self,
        name: &str,
        spec: Spec,
        source: Source,
        listing: Option<String>,
        description: &str,
        always: bool,
        grant: Option<Arc<Grant>>,
    ) -> Result<Arc<Server>, McpError> {
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
                    link: Link::new(spec.clone(), self.cfg.connect_timeout, self.cfg.call_timeout, grant),
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
        self.forget_locked().await;
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

    /// Directory servers that refused a call for want of credentials are forgotten, and their endpoints never connected again. A
    /// server the user signed in to whose sign-in no longer works (refused, and not refreshed) is forgotten with its sign-in: it is
    /// offered for a new sign-in the next time it is needed.
    pub async fn forget_locked(&self) {
        let locked: Vec<Arc<Server>> =
            self.servers.read().values().filter(|s| s.link.locked() && (s.source == Source::Glama || s.link.grant.is_some())).cloned().collect();
        for s in locked {
            if let Some(g) = &s.link.grant {
                tracing::info!(server = %s.name, "the sign-in to an MCP server no longer works: the user is asked again next time");
                self.oauth.forget(&g.resource());
            } else {
                if let Spec::Http { url, .. } = &s.spec {
                    self.locked.lock().insert(url.clone());
                }
                tracing::info!(server = %s.name, "a directory server asked for a login when called: it is not used again");
            }
            self.remove(&s.name).await;
        }
    }

    // -------------------------------------------------------------- discovery ----

    /// What the directory has for `query` (nothing is connected).
    pub async fn search(&self, query: &str, first: u32) -> Result<Vec<Connector>, DiscoverError> {
        let glama = self.glama.as_ref().ok_or(DiscoverError::NotConfigured)?;
        Ok(glama.search(query, first).await?)
    }

    /// Search the directory for what `query` describes, connect the best servers that can be used as they are, and return them.
    pub async fn discover(&self, query: &str) -> Result<Vec<Found>, DiscoverError> {
        let list = self.shortlist(&Need::query(query)).await?;
        let found = self.connect(&list.candidates, self.cfg.max_auto).await;
        if found.is_empty() {
            let err = DiscoverError::NoneUsable { query: query.to_string(), skipped: list.skipped };
            self.misses.lock().insert(query.to_lowercase(), (std::time::Instant::now(), err.clone()));
            return Err(err);
        }
        Ok(found)
    }

    /// The servers the directory has for a need that this client can use as they are, best first; nothing is connected.
    ///
    /// Every query is searched for servers that need no credentials (and, when keys are configured, for those that take an API key):
    /// the directory filters by attribute, so its first page is not spent on OAuth servers. The first query is also searched
    /// unfiltered — a fallback should the attribute names change, and where the reasons others were skipped come from. The ranked
    /// lists are merged (reciprocal rank fusion), the best are read in detail, and they are ranked again on what their tools say.
    pub async fn shortlist(&self, need: &Need) -> Result<Shortlist, DiscoverError> {
        let glama = self.glama.as_ref().ok_or(DiscoverError::NotConfigured)?;
        let mut queries: Vec<String> = Vec::new();
        for q in need.queries.iter().chain(std::iter::once(&need.capability)) {
            let q = q.trim().to_string();
            if !q.is_empty() && !queries.iter().any(|x| x.eq_ignore_ascii_case(&q)) && queries.len() < 3 {
                queries.push(q);
            }
        }
        if queries.is_empty() {
            return Err(DiscoverError::NoneUsable { query: String::new(), skipped: vec![] });
        }
        let miss_key = queries.join(" | ").to_lowercase();
        if let Some((at, err)) = self.misses.lock().get(&miss_key)
            && at.elapsed() < Duration::from_secs(600)
        {
            return Err(err.clone());
        }
        let mut filters: Vec<&[&str]> = vec![&["auth:none"]];
        if !self.cfg.keys.is_empty() {
            filters.push(&["auth:api_key"]);
        }
        // with a callback to send the user back to, servers that want a sign-in are offered too
        if self.cfg.redirect_uri.is_some() {
            filters.push(&["auth:oauth2"]);
        }
        let mut searches: Vec<(&str, &[&str])> = vec![(queries[0].as_str(), &[])];
        for q in &queries {
            for f in &filters {
                searches.push((q.as_str(), *f));
            }
        }
        let pages = futures_util::future::join_all(searches.iter().map(|(q, f)| glama.search_page(q, 20, f, None))).await;
        let mut lists: Vec<Vec<Connector>> = Vec::new();
        let mut first_error = None;
        for p in pages {
            match p {
                Ok(p) => lists.push(p.connectors),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        if lists.is_empty() {
            return Err(first_error.map(DiscoverError::Directory).unwrap_or(DiscoverError::NoneUsable { query: queries[0].clone(), skipped: vec![] }));
        }
        // reciprocal rank fusion: a server that several searches rank high is the directory's best guess
        let mut fused: BTreeMap<String, (f32, Connector)> = BTreeMap::new();
        for list in lists {
            for (rank, c) in list.into_iter().enumerate() {
                let e = fused.entry(c.id.clone()).or_insert((0.0, c));
                e.0 += 1.0 / (60.0 + rank as f32);
            }
        }
        let mut ranked: Vec<(f32, Connector)> = fused.into_values().collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
        self.forget_locked().await;
        let mut skipped: Vec<String> = Vec::new();
        let mut usable: Vec<(f32, Connector, Option<String>)> = Vec::new();
        for (score, c) in ranked {
            if self.locked.lock().contains(&c.endpoint) {
                if skipped.len() < 4 {
                    skipped.push(format!("{} asked for a login when called", c.name));
                }
                continue;
            }
            let key = self.cfg.keys.get(&slug(&c.slug)).cloned();
            let reachable = !c.deprecated && c.transport == "streamable_http" && c.healthy != Some(false) && !c.paid();
            // the directory's health check cannot sign in: every server behind one reads "unhealthy" with no tools (seen: the official
            // Linear), so for those it says nothing; whether it answers is asked when the sign-in starts
            let reachable_signed = !c.deprecated && c.transport == "streamable_http" && !c.paid();
            let ok = c.open() || (matches!(c.auth, AuthType::ApiKey) && key.is_some() && reachable) || (self.signs_in(&c) && reachable_signed);
            if ok {
                usable.push((score, c, key));
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
        // servers usable as they are first, those behind a sign-in beside them, each with its own room: a directory with many of one
        // kind must not crowd out the other (seen: Apify scrapers behind a sign-in pushing out the open Hacker News servers)
        let (mut open, mut signed): (Vec<_>, Vec<_>) = usable.into_iter().partition(|(_, c, _)| c.auth != AuthType::Oauth2);
        open.truncate(SHORTLIST_DEPTH);
        signed.truncate(SHORTLIST_DEPTH / 2);
        let mut usable = open;
        usable.extend(signed);
        // what each one can do, before anything is connected
        let details = futures_util::future::join_all(usable.iter().map(|(_, c, _)| self.detail(glama, c))).await;
        let mut candidates: Vec<Candidate> = Vec::new();
        for ((fusion, c, key), detail) in usable.into_iter().zip(details) {
            let detail = detail.unwrap_or_default();
            let sign_in = c.auth == AuthType::Oauth2 && self.oauth.get(&c.endpoint).is_none();
            // a server the directory saw with no tools has nothing to offer (one behind a sign-in shows none until signed in)
            if detail.tools.is_empty() && c.tool_count == 0 && c.healthy == Some(true) && c.auth != AuthType::Oauth2 {
                continue;
            }
            candidates.push(Candidate { connector: c, tools: detail.tools, documentation: detail.documentation, score: fusion, sign_in, key });
        }
        if candidates.is_empty() {
            let err = DiscoverError::NoneUsable { query: queries[0].clone(), skipped: skipped.clone() };
            self.misses.lock().insert(miss_key, (std::time::Instant::now(), err.clone()));
            return Err(err);
        }
        // A server is what its tools do, not what it calls itself (gateways name themselves after whatever is searched): it is
        // ranked mostly on its best single tool against the need, every tool of every candidate scored in one corpus.
        let wanted = format!("{} {}", need.capability, queries.join(" "));
        let mut owners: Vec<usize> = Vec::new();
        let mut tool_docs: Vec<String> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            for t in &c.tools {
                owners.push(i);
                tool_docs.push(tool_doc(t));
            }
        }
        let mut best_tool = vec![0f32; candidates.len()];
        for (i, s) in owners.into_iter().zip(dfae_core::text::bm25_scores(&wanted, &tool_docs)) {
            best_tool[i] = best_tool[i].max(s);
        }
        let headers: Vec<String> = candidates.iter().map(|c| format!("{} {}", c.connector.name, c.connector.description.clone().unwrap_or_default())).collect();
        let header = dfae_core::text::bm25_scores(&wanted, &headers);
        let rel = |v: &[f32]| {
            let m = v.iter().copied().fold(0f32, f32::max).max(f32::EPSILON);
            v.iter().map(|x| x / m).collect::<Vec<f32>>()
        };
        let (best_tool, header) = (rel(&best_tool), rel(&header));
        let fusion_top = candidates.iter().map(|c| c.score).fold(f32::MIN, f32::max).max(f32::EPSILON);
        let about_money = MONEY.is_match(&wanted);
        for (i, c) in candidates.iter_mut().enumerate() {
            let quality = c.connector.quality.unwrap_or(2.5) as f32 / 5.0;
            let health = if c.connector.healthy == Some(true) {
                1.0
            } else if c.connector.auth == AuthType::Oauth2 {
                0.5 // not known: the directory cannot check a server behind a sign-in
            } else {
                0.0
            };
            // with no tool list (the details could not be read) the header stands in for the tools
            let tools = if c.tools.is_empty() { header[i] } else { best_tool[i] };
            let mut score = 0.30 * (c.score / fusion_top) + 0.45 * tools + 0.10 * header[i] + 0.08 * quality + 0.07 * health;
            // wallets, balances and purchases: a pay-as-you-go service, which answers a free call with a payment request
            if !about_money && c.tools.iter().any(|t| MONEY.is_match(&t.name.replace(['_', '-'], " "))) {
                score -= 0.15;
            }
            // asking the user to sign in costs them a step: an open server that does as well is preferred
            if c.sign_in {
                score -= 0.08;
            }
            // the service's own server: its endpoint is on the service's domain (`mcp.linear.app` for "linear issues"), the strongest
            // sign of an official one, which no name or description can fake
            if official(&c.connector, &queries) {
                score += 0.2;
            }
            c.score = score;
        }
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.connector.name.cmp(&b.connector.name)));
        Ok(Shortlist { candidates, skipped })
    }

    async fn detail(&self, glama: &Glama, c: &Connector) -> Result<Detail, GlamaError> {
        if let Some((at, d)) = self.details.lock().get(&c.id)
            && at.elapsed() < DETAIL_TTL
        {
            return Ok(d.clone());
        }
        let d = glama.detail(c).await;
        match &d {
            Ok(d) => {
                self.details.lock().insert(c.id.clone(), (std::time::Instant::now(), d.clone()));
            }
            Err(e) => tracing::debug!(server = %c.name, "no details from the directory: {e}"),
        }
        d
    }

    /// Connect the first `max` of `candidates` that open (one more is tried, in case one of them is down), and return them. Only
    /// candidates that score close to the best are connected: a second server is worth its tokens only when it is about as good.
    pub async fn connect(&self, candidates: &[Candidate], max: usize) -> Vec<Found> {
        let _one_at_a_time = self.discovery.lock().await;
        let mut result: Vec<Found> = Vec::new();
        let open: Vec<&Candidate> = candidates.iter().filter(|c| !c.sign_in).collect();
        let best = open.first().map(|c| c.score).unwrap_or(0.0);
        let batch: Vec<&Candidate> = open.into_iter().take(max + 1).filter(|c| c.score >= best - 0.15 * best.abs()).collect();
        let attempts = batch.iter().map(|c| self.add_connector(&c.connector, c.key.clone()));
        for (outcome, cand) in futures_util::future::join_all(attempts).await.into_iter().zip(&batch) {
            let c = &cand.connector;
            match outcome {
                Ok((server, known)) if result.len() < max => {
                    result.push(Found {
                        server: server.name.clone(),
                        title: c.name.clone(),
                        listing: c.listing.clone(),
                        tools: self.tools_of(&server.name),
                        known,
                    });
                }
                Ok((server, false)) => {
                    // connected but not needed
                    self.remove(&server.name).await;
                }
                Ok(_) => {}
                Err(e) => tracing::info!(server = %c.name, "directory server not usable: {e}"),
            }
        }
        if !result.is_empty() {
            self.save();
        }
        result
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

    // ---------------------------------------------------------------- sign-in ----

    /// Can the user be asked to sign in to this directory server?
    fn signs_in(&self, c: &Connector) -> bool {
        c.auth == AuthType::Oauth2 && self.cfg.redirect_uri.is_some() && !c.attributes.iter().any(|a| a == "oauth:unavailable")
    }

    /// Ask the user to sign in to a directory server: find its authorization server from its 401, register this engine with it
    /// (once), and return the URL to open. The provider sends the browser back to [`HubConfig::redirect_uri`], whose handler calls
    /// [`McpHub::finish_sign_in`].
    pub async fn begin_sign_in(&self, c: &Candidate) -> Result<SignIn, String> {
        let mut name = slug(&c.connector.slug);
        if self.servers.read().get(&name).is_some_and(|s| !matches!(&s.spec, Spec::Http { url, .. } if *url == c.connector.endpoint)) {
            name = slug(&format!("{}_{}", c.connector.namespace, c.connector.slug));
        }
        self.begin(
            &name,
            &c.connector.name,
            &c.connector.endpoint,
            Some(c.connector.listing.clone()),
            c.connector.description.as_deref().unwrap_or(""),
            Source::Glama,
        )
        .await
    }

    /// What signing in to `endpoint` would take, read from its public metadata only (a diagnostic: nothing is registered).
    pub async fn probe_sign_in(endpoint: &str) -> String {
        let challenge = match probe(endpoint, Duration::from_secs(20)).await {
            Ok(c) => c,
            Err(e) => return format!("unreachable: {}", e.chars().take(80).collect::<String>()),
        };
        match oauth::discover(endpoint, &challenge).await {
            Ok(a) => format!(
                "ok: issuer {} · {} · scopes [{}]",
                a.issuer,
                if a.registration_endpoint.is_some() { "self-registration" } else { "NO self-registration" },
                a.scopes.join(" ")
            ),
            Err(e) => format!("no: {e}"),
        }
    }

    /// Ask the user to sign in to a server that is already known (the operator's, or one added through the API) and refused for want
    /// of it.
    pub async fn begin_sign_in_server(&self, name: &str) -> Result<SignIn, String> {
        let s = self.servers.read().get(name).cloned().ok_or_else(|| format!("no MCP server '{name}'"))?;
        let Spec::Http { url, .. } = &s.spec else { return Err("a local (stdio) server takes its credentials from its environment".into()) };
        self.begin(&s.name, &s.name, url, s.listing.clone(), &s.description, s.source).await
    }

    async fn begin(&self, name: &str, title: &str, endpoint: &str, listing: Option<String>, description: &str, source: Source) -> Result<SignIn, String> {
        let redirect = self.cfg.redirect_uri.clone().ok_or("signing in to MCP servers is not configured (no callback URL)")?;
        // one link per server at a time: a second task that needs it waits for the same sign-in
        let existing = {
            let pending = self.pending.lock();
            pending.iter().find(|(_, p)| p.endpoint == endpoint && p.started.elapsed() < SIGN_IN_TTL && matches!(*p.status.borrow(), SignInState::Waiting)).map(
                |(state, p)| SignIn {
                    state: state.clone(),
                    url: p.flow.url.clone(),
                    server: p.name.clone(),
                    title: p.title.clone(),
                    listing: p.listing.clone(),
                },
            )
        };
        if let Some(s) = existing {
            return Ok(s);
        }
        let challenge = probe(endpoint, self.cfg.connect_timeout).await?;
        let server = oauth::discover(endpoint, &challenge).await.map_err(|e| e.to_string())?;
        let client = match self.oauth.client(&server.issuer, &redirect) {
            Some(c) => c,
            None => {
                let c = oauth::register(&server, &redirect).await.map_err(|e| e.to_string())?;
                self.oauth.put_client(&server.issuer, c.clone());
                c
            }
        };
        let flow = oauth::Flow::start(server, client).map_err(|e| e.to_string())?;
        let sign_in = SignIn { state: flow.state.clone(), url: flow.url.clone(), server: slug(name), title: title.to_string(), listing: listing.clone() };
        let (status, _) = tokio::sync::watch::channel(SignInState::Waiting);
        let mut pending = self.pending.lock();
        pending.retain(|_, p| p.started.elapsed() < SIGN_IN_TTL);
        pending.insert(
            flow.state.clone(),
            Pending {
                flow,
                name: slug(name),
                title: title.to_string(),
                endpoint: endpoint.to_string(),
                listing,
                description: description.to_string(),
                source,
                started: std::time::Instant::now(),
                status,
            },
        );
        Ok(sign_in)
    }

    /// The provider sent the user back with a code (or an error): exchange it for tokens, keep them, connect the server and tell
    /// whoever waits. Returns the server's title, for the page the user sees.
    pub async fn finish_sign_in(&self, state: &str, code: Option<&str>, error: Option<&str>) -> Result<String, String> {
        let (flow, name, title, endpoint, listing, description, source, status) = {
            let pending = self.pending.lock();
            let p = pending.get(state).ok_or("this sign-in link is unknown or has expired: ask the agent again")?;
            if p.started.elapsed() > SIGN_IN_TTL {
                return Err("this sign-in link has expired: ask the agent again".into());
            }
            (p.flow.clone(), p.name.clone(), p.title.clone(), p.endpoint.clone(), p.listing.clone(), p.description.clone(), p.source, p.status.clone())
        };
        let outcome: Result<Found, String> = async {
            if let Some(e) = error {
                return Err(format!("the sign-in to {title} was not completed ({e})"));
            }
            let code = code.ok_or("the provider sent no code back")?;
            let tokens = flow.finish(code).await.map_err(|e| e.to_string())?;
            let saved = oauth::Saved { server: flow.server.clone(), client: flow.client.clone(), tokens };
            self.oauth.put(saved.clone());
            // a server known without the sign-in (it refused) is opened again with it
            if self.servers.read().get(&name).is_some_and(|s| s.link.grant.is_none()) {
                self.remove(&name).await;
            }
            let grant = Grant::new(saved, Some(self.oauth.clone()));
            let spec = Spec::Http { url: endpoint.clone(), headers: BTreeMap::new() };
            let server = self
                .add_with(&name, spec, source, listing.clone(), &description, false, Some(grant))
                .await
                .map_err(|e| format!("signed in, but {title} could not be opened: {e}"))?;
            self.save();
            Ok(Found {
                server: server.name.clone(),
                title: title.clone(),
                listing: listing.clone().unwrap_or_default(),
                tools: self.tools_of(&server.name),
                known: false,
            })
        }
        .await;
        self.pending.lock().remove(state);
        let _ = status.send(match &outcome {
            Ok(f) => SignInState::Done(f.clone()),
            Err(e) => SignInState::Failed(e.clone()),
        });
        outcome.map(|_| title)
    }

    /// Wait until the user has signed in (the server is connected), the sign-in failed, or `timeout` passed.
    pub async fn wait_sign_in(&self, state: &str, timeout: Duration) -> Result<Found, String> {
        let mut rx = self.pending.lock().get(state).map(|p| p.status.subscribe()).ok_or("no such sign-in")?;
        let wait = async {
            loop {
                match &*rx.borrow_and_update() {
                    SignInState::Done(f) => return Ok(f.clone()),
                    SignInState::Failed(e) => return Err(e.clone()),
                    SignInState::Waiting => {}
                }
                if rx.changed().await.is_err() {
                    return match &*rx.borrow() {
                        SignInState::Done(f) => Ok(f.clone()),
                        SignInState::Failed(e) => Err(e.clone()),
                        SignInState::Waiting => Err("the sign-in was abandoned".into()),
                    };
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or_else(|_| Err(format!("nobody signed in within {} minutes", timeout.as_secs() / 60)))
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
                    oauth: s.link.grant.is_some(),
                }),
                _ => None,
            })
            .collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut locked: Vec<String> = self.locked.lock().iter().cloned().collect();
        locked.sort();
        if let Ok(text) = serde_json::to_string_pretty(&State { servers, locked }) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Servers found in earlier runs are known again (and opened in the background).
    pub fn restore(self: &Arc<Self>) {
        let Some(path) = &self.cfg.state_path else { return };
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(state) = serde_json::from_str::<State>(&text) else { return };
        self.locked.lock().extend(state.locked);
        for saved in state.servers {
            let hub = self.clone();
            tokio::spawn(async move {
                let mut headers = BTreeMap::new();
                // a sign-in that is gone (refused, forgotten) leaves the server to be offered again when it is needed
                if saved.oauth && hub.oauth.get(&saved.url).is_none() {
                    return;
                }
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

/// Is the connector served from the domain of a service the queries name (`linear` → `mcp.linear.app`, `notion` → `mcp.notion.com`)?
fn official(c: &Connector, queries: &[String]) -> bool {
    let host = reqwest::Url::parse(&c.endpoint).ok().and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase())).unwrap_or_default();
    // the registrable part: the label before the public suffix (`linear` of `mcp.linear.app`; good enough for the names that matter)
    let labels: Vec<&str> = host.split('.').collect();
    let Some(site) = labels.len().checked_sub(2).and_then(|i| labels.get(i)) else { return false };
    let name = c.name.to_ascii_lowercase();
    site.len() > 3
        && queries.iter().flat_map(|q| q.split(|ch: char| !ch.is_alphanumeric())).map(|w| w.to_ascii_lowercase()).any(|w| w == *site && name.contains(&w))
}

/// Ask a server without credentials what it wants: its 401 and `WWW-Authenticate` challenge.
async fn probe(endpoint: &str, timeout: Duration) -> Result<oauth::Challenge, String> {
    match McpClient::connect(&Spec::Http { url: endpoint.to_string(), headers: BTreeMap::new() }, timeout).await {
        Err(McpError::Unauthorized { challenge, .. }) => Ok(challenge.map(|c| oauth::Challenge::parse(&c)).unwrap_or_default()),
        Err(e) => Err(format!("{endpoint} could not be reached: {e}")),
        Ok(c) => {
            c.close().await;
            Ok(oauth::Challenge::default())
        }
    }
}

/// The words a tool is ranked on: its name, title and description.
fn tool_doc(t: &DirTool) -> String {
    format!("{} {} {}", t.name.replace(['_', '-'], " "), t.title.clone().unwrap_or_default(), t.description.clone().unwrap_or_default())
}

/// Words of payment: in a tool's name they mark a pay-as-you-go service, unless payments are what is asked for.
static MONEY: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\b(?:wallet|balance|credits?|buy|purchase|payments?|billing|x402|usdc|invoice)\b").unwrap());

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
