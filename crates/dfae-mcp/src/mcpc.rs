//! The engine's remote MCP servers as [mcpc](https://github.com/apify/mcpc) sessions.
//!
//! Every remote server the engine uses is one mcpc session, `@dfae-<server>`, opened with a local proxy:
//!
//! ```text
//! engine ──MCP (Streamable HTTP, bearer)──► mcpc proxy ──► mcpc bridge (session, OAuth tokens, refresh, keepalive) ──► server
//! ```
//!
//! so authentication is all mcpc's (`mcpc login`: protected resource and authorization server metadata, Client ID Metadata
//! Documents or Dynamic Client Registration, PKCE, resource indicators, refresh-token rotation), and the engine holds only the
//! proxy's bearer token, which it makes up per session and keeps in memory.
//!
//! mcpc is run as a command with `--json`, a private `MCPC_HOME_DIR` and a minimal environment: its bridges inherit that environment,
//! so the engine's keys are never in it. A sign-in is `mcpc login` with its standard input piped: mcpc prints the authorization URL
//! and waits for the provider to send the browser back to its loopback callback (`http://127.0.0.1:<port>/callback`). Where the
//! browser cannot reach that (the engine in a container, on another machine), [`McpcBroker::deliver`] hands the provider's answer
//! on — after checking its `state` against the sign-in's own, which mcpc 0.8.0 does not do on that path itself.

use crate::broker::{Broker, BrokerError, LoginStart, callback_params, canonical_url, random_token, same};
use crate::client::Spec;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The mcpc release this integration is written and tested against.
pub const MCPC_VERSION: &str = "0.8.0";

/// How long a sign-in link stays good.
pub const LOGIN_TTL: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone)]
pub struct McpcConfig {
    /// The `mcpc` executable.
    pub bin: PathBuf,
    /// `MCPC_HOME_DIR`: sessions, profiles and (on a host without a keyring) credentials. Private to the engine: outside the
    /// workspace, mode 0700.
    pub home: PathBuf,
    /// The address the sessions' proxies listen on: `127.0.0.1` when the engine runs beside mcpc, `0.0.0.0` in a broker container.
    pub proxy_bind: String,
    /// The host the engine reaches the proxies at (the broker container's name, or `127.0.0.1`).
    pub proxy_host: String,
    /// The port of mcpc's OAuth callback (`--callback-port`); `None`: mcpc's own (13316, 31613, 16133, which its hosted client
    /// metadata document names).
    pub callback_port: Option<u16>,
    /// One mcpc command (a connect includes the server's handshake).
    pub timeout: Duration,
}

impl McpcConfig {
    pub fn new(bin: PathBuf, home: PathBuf) -> Self {
        Self { bin, home, proxy_bind: "127.0.0.1".into(), proxy_host: "127.0.0.1".into(), callback_port: None, timeout: Duration::from_secs(60) }
    }
}

/// Find mcpc: `explicit` (`DFAE_MCPC_BIN`) if given, else `mcpc` on `PATH`. Only an executable that answers `--version` counts.
pub fn find_mcpc(explicit: Option<&Path>) -> Option<(PathBuf, String)> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(p) => vec![p.to_path_buf()],
        None => std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join("mcpc")).collect()).unwrap_or_default(),
    };
    for bin in candidates {
        if !bin.is_file() {
            continue;
        }
        let Ok(out) = std::process::Command::new(&bin).arg("--version").env("NO_COLOR", "1").stdin(Stdio::null()).output() else { continue };
        if out.status.success() {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return Some((bin, version));
        }
    }
    None
}

/// What one mcpc command said when it failed: its exit code (1 client, 2 server, 3 network, 4 authentication) and message.
#[derive(Debug, Clone, PartialEq)]
pub struct McpcError {
    pub code: i32,
    pub message: String,
}

impl From<McpcError> for BrokerError {
    fn from(e: McpcError) -> Self {
        // what follows mcpc's first paragraph is advice for someone at its command line (`mcpc login …`), not for the engine's user
        let first = e.message.split("\n\n").next().unwrap_or(&e.message).trim().to_string();
        if e.code == 4 { BrokerError::NeedsLogin(first) } else { BrokerError::Failed(e.message) }
    }
}

/// The first JSON value in `text` that starts a line (mcpc's `{ "error": …, "code": … }` may follow other output on stderr).
pub(crate) fn first_json(text: &str) -> Option<Value> {
    let mut starts: Vec<usize> = vec![];
    if text.starts_with(['{', '[']) {
        starts.push(0);
    }
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1).filter(|&i| text[i..].starts_with(['{', '['])));
    starts.into_iter().find_map(|i| serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>().next().and_then(Result::ok))
}

/// mcpc's answer to a command: its JSON on success, its error otherwise.
pub(crate) fn outcome(code: Option<i32>, stdout: &str, stderr: &str) -> Result<Value, McpcError> {
    match code {
        Some(0) => Ok(if stdout.trim().is_empty() { Value::Null } else { first_json(stdout).unwrap_or(Value::Null) }),
        code => {
            let v = first_json(stderr).or_else(|| first_json(stdout));
            let message = v
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|e| e.as_str())
                .map(String::from)
                .unwrap_or_else(|| stderr.trim().chars().rev().take(400).collect::<String>().chars().rev().collect());
            let code = v.as_ref().and_then(|v| v.get("code")).and_then(|c| c.as_i64()).map(|c| c as i32).or(code).unwrap_or(1);
            Err(McpcError { code, message: if message.is_empty() { format!("mcpc failed (exit code {code})") } else { message } })
        }
    }
}

/// `@dfae-<server>`: what mcpc calls the engine's session for a server (`[a-zA-Z0-9_-]{1,64}`).
pub fn session_name(server: &str) -> String {
    let s: String = server.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).take(58).collect();
    format!("@dfae-{s}")
}

/// What `mcpc login` printed: the authorization URL, its `state`, and the loopback port its callback listens on.
pub(crate) fn parse_authorization(stderr: &str) -> Option<(String, String, u16)> {
    let line = stderr.lines().find_map(|l| l.trim().strip_prefix("Authorization URL:"))?.trim();
    let url = reqwest::Url::parse(line).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let state = q.get("state").filter(|s| !s.is_empty())?.clone();
    let redirect = reqwest::Url::parse(q.get("redirect_uri")?).ok()?;
    // mcpc only ever listens on the loopback interface
    if !matches!(redirect.host_str(), Some("127.0.0.1" | "localhost")) {
        return None;
    }
    Some((url.to_string(), state, redirect.port()?))
}

#[derive(Debug, Clone)]
enum LoginState {
    Waiting,
    Done,
    Failed(String),
}

struct Login {
    url: String,
    port: u16,
    started: Instant,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    status: tokio::sync::watch::Sender<LoginState>,
}

#[derive(Clone)]
struct Open {
    /// What it was opened for: the URL and the headers' digest (a changed key opens it again).
    key: String,
    spec: Spec,
}

pub struct McpcBroker {
    cfg: McpcConfig,
    /// What the engine opened in this run, by its server name (the proxy's bearer token is only known here).
    open: tokio::sync::Mutex<HashMap<String, Open>>,
    /// Sign-ins waiting for the user, by `state`.
    logins: Mutex<HashMap<String, Arc<Login>>>,
    /// Held while a login starts: mcpc picks a free callback port and binds it a moment later, so two logins started at once can
    /// pick the same one. Once it prints the URL, its port is bound and the next may start.
    starting: tokio::sync::Mutex<()>,
    /// The canonical URLs of the servers mcpc holds a sign-in for (`None`: to be read again).
    profiles: Mutex<Option<Vec<String>>>,
    http: reqwest::Client,
}

impl McpcBroker {
    pub fn new(cfg: McpcConfig) -> Arc<Self> {
        if let Err(e) = private_dir(&cfg.home) {
            tracing::warn!(home = %cfg.home.display(), "cannot prepare mcpc's home: {e}");
        }
        // the callback is on this machine's loopback interface: never through a proxy
        let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(30)).build().expect("http");
        Arc::new(Self { cfg, open: Default::default(), logins: Default::default(), starting: Default::default(), profiles: Mutex::new(None), http })
    }

    pub fn config(&self) -> &McpcConfig {
        &self.cfg
    }

    /// mcpc's environment: enough to run Node and reach the network, and nothing of the engine's (its bridges inherit it).
    fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.cfg.bin);
        cmd.env_clear();
        for k in [
            "PATH",
            "LANG",
            "TMPDIR",
            "TZ",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "NO_PROXY",
            "https_proxy",
            "http_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "NODE_EXTRA_CA_CERTS",
        ] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.env("HOME", &self.cfg.home).env("MCPC_HOME_DIR", &self.cfg.home).env("NO_COLOR", "1").env("FORCE_COLOR", "0");
        cmd.current_dir(&self.cfg.home).kill_on_drop(true);
        cmd
    }

    /// One mcpc command with `--json`.
    pub async fn run(&self, args: &[&str]) -> Result<Value, McpcError> {
        let mut cmd = self.command();
        cmd.arg("--json").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|e| McpcError { code: 1, message: format!("cannot run mcpc ({}): {e}", self.cfg.bin.display()) })?;
        let out = tokio::time::timeout(self.cfg.timeout, child.wait_with_output())
            .await
            .map_err(|_| McpcError { code: 3, message: format!("mcpc {} did not finish within {}s", args.first().unwrap_or(&""), self.cfg.timeout.as_secs()) })?
            .map_err(|e| McpcError { code: 1, message: format!("mcpc: {e}") })?;
        outcome(out.status.code(), &String::from_utf8_lossy(&out.stdout), &String::from_utf8_lossy(&out.stderr))
    }

    /// mcpc's sessions and profiles (`mcpc --json`).
    async fn listing(&self) -> Result<Value, McpcError> {
        self.run(&[]).await
    }

    async fn session_status(&self, name: &str) -> Option<String> {
        let l = self.listing().await.ok()?;
        l["sessions"].as_array()?.iter().find(|s| s["name"] == name).and_then(|s| s["status"].as_str()).map(String::from)
    }

    async fn profiles(&self) -> Vec<String> {
        if let Some(p) = self.profiles.lock().clone() {
            return p;
        }
        let found: Vec<String> = match self.listing().await {
            Ok(l) => l["profiles"].as_array().map(|a| a.iter().filter_map(|p| p["serverUrl"].as_str()).map(canonical_url).collect()).unwrap_or_default(),
            Err(e) => {
                tracing::warn!("cannot read mcpc's profiles: {}", e.message);
                return vec![];
            }
        };
        *self.profiles.lock() = Some(found.clone());
        found
    }

    /// A free TCP port on the proxies' address (the proxy binds it a moment later; a lost race is retried).
    fn free_port(&self) -> Result<u16, BrokerError> {
        let l = std::net::TcpListener::bind((self.cfg.proxy_bind.as_str(), 0)).map_err(|e| BrokerError::Failed(format!("no free port for a proxy: {e}")))?;
        Ok(l.local_addr().map_err(|e| BrokerError::Failed(e.to_string()))?.port())
    }

    async fn connect(&self, name: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<Spec, BrokerError> {
        let token = random_token();
        // headers (an API key) go in a config entry only mcpc reads, for the moment of the connect: never on a command line
        let config = if headers.is_empty() {
            None
        } else {
            let path = self.cfg.home.join(format!("connect-{}.json", uuid::Uuid::new_v4().simple()));
            let body = serde_json::json!({"mcpServers": {"server": {"url": url, "headers": headers}}});
            write_private(&path, &serde_json::to_vec(&body).unwrap_or_default())
                .map_err(|e| BrokerError::Failed(format!("cannot write {}: {e}", path.display())))?;
            Some(path)
        };
        let target = config.as_ref().map(|p| format!("{}:server", p.display())).unwrap_or_else(|| url.to_string());
        let mut last = BrokerError::Failed("not connected".into());
        for _ in 0..3 {
            let port = self.free_port()?;
            let proxy = format!("{}:{port}", self.cfg.proxy_bind);
            let r = self.run(&["connect", &target, name, "--proxy", &proxy, "--proxy-bearer-token", &token]).await;
            match r {
                Ok(_) => {
                    if let Some(p) = &config {
                        let _ = std::fs::remove_file(p);
                    }
                    let mut auth = BTreeMap::new();
                    auth.insert("Authorization".to_string(), format!("Bearer {token}"));
                    return Ok(Spec::Http { url: format!("http://{}:{port}", self.cfg.proxy_host), headers: auth });
                }
                Err(e) if e.message.contains("already in use") => last = BrokerError::Failed(e.message),
                Err(e) => {
                    last = e.into();
                    break;
                }
            }
        }
        if let Some(p) = &config {
            let _ = std::fs::remove_file(p);
        }
        // a session that did not open stays behind as `unauthorized` or `crashed`: it is not the engine's to keep
        let _ = self.run(&["close", name]).await;
        Err(last)
    }
}

/// A directory only the engine's user can enter.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(bytes)
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)
}

fn digest(url: &str, headers: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    for (k, v) in headers {
        h.update([0]);
        h.update(k.to_ascii_lowercase().as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
    }
    hex::encode(h.finalize())
}

impl McpcBroker {
    /// One `mcpc login`, up to the authorization URL it prints.
    async fn start_login(&self, url: &str) -> Result<LoginStart, BrokerError> {
        let mut cmd = self.command();
        cmd.arg("--json").arg("login").arg(url);
        let port_arg;
        if let Some(p) = self.cfg.callback_port {
            port_arg = p.to_string();
            cmd.arg("--callback-port").arg(&port_arg);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| BrokerError::Failed(format!("cannot run mcpc ({}): {e}", self.cfg.bin.display())))?;
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().ok_or_else(|| BrokerError::Failed("mcpc: no stdout".into()))?;
        let mut stderr = BufReader::new(child.stderr.take().ok_or_else(|| BrokerError::Failed("mcpc: no stderr".into()))?);
        // discovery and client registration come first; then mcpc prints the URL and waits
        let mut seen = String::new();
        let found = tokio::time::timeout(self.cfg.timeout, async {
            let mut line = String::new();
            loop {
                line.clear();
                match stderr.read_line(&mut line).await {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {
                        seen.push_str(&line);
                        if let Some(a) = parse_authorization(&seen) {
                            return Some(a);
                        }
                    }
                }
            }
        })
        .await;
        let (auth_url, state, port) = match found {
            Ok(Some(a)) => a,
            other => {
                let _ = child.start_kill();
                let mut rest = String::new();
                let _ = tokio::time::timeout(Duration::from_secs(5), stderr.read_to_string(&mut rest)).await;
                seen.push_str(&rest);
                let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await.ok().and_then(|s| s.ok()).and_then(|s| s.code());
                let why = match outcome(status.filter(|c| *c != 0).or(Some(1)), "", &seen) {
                    Err(e) => e.message,
                    Ok(_) => "mcpc gave no authorization URL".into(),
                };
                return Err(BrokerError::Failed(if other.is_err() { format!("mcpc login did not start in time: {why}") } else { why }));
            }
        };
        let (tx, _) = tokio::sync::watch::channel(LoginState::Waiting);
        let login = Arc::new(Login { url: url.to_string(), port, started: Instant::now(), stdin: tokio::sync::Mutex::new(stdin), status: tx });
        self.logins.lock().insert(state.clone(), login.clone());
        // the rest of the login: mcpc exchanges the code and keeps the tokens, or fails, or the link expires
        let done = login.clone();
        tokio::spawn(async move {
            let mut err_rest = String::new();
            let mut out = String::new();
            let wait = async {
                let _ = tokio::join!(stderr.read_to_string(&mut err_rest), stdout.read_to_string(&mut out));
                child.wait().await
            };
            let result = match tokio::time::timeout(LOGIN_TTL, wait).await {
                Ok(Ok(status)) => outcome(status.code(), &out, &format!("{seen}{err_rest}")).map(|_| ()),
                Ok(Err(e)) => Err(McpcError { code: 1, message: e.to_string() }),
                Err(_) => Err(McpcError { code: 1, message: "nobody signed in before the link expired".into() }),
            };
            let _ = done.status.send(match result {
                Ok(()) => LoginState::Done,
                Err(e) => LoginState::Failed(e.message),
            });
        });
        Ok(LoginStart { state, url: auth_url })
    }
}

#[async_trait]
impl Broker for McpcBroker {
    fn kind(&self) -> &'static str {
        "mcpc"
    }

    fn signs_in(&self) -> bool {
        true
    }

    async fn open(&self, server: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<Spec, BrokerError> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(BrokerError::Unsupported(format!("not a remote MCP server: {url}")));
        }
        let name = session_name(server);
        let key = digest(url, headers);
        let mut open = self.open.lock().await;
        if let Some(o) = open.get(server).filter(|o| o.key == key).cloned() {
            // alive (mcpc reconnects a dropped connection by itself): the same proxy; otherwise it is opened anew below
            if self.session_status(&name).await.as_deref() == Some("live") {
                return Ok(o.spec);
            }
        }
        // whatever else has this name (a dead session, one from an earlier run whose proxy token is gone) is closed first
        let _ = self.run(&["close", &name]).await;
        open.remove(server);
        let spec = self.connect(&name, url, headers).await?;
        open.insert(server.to_string(), Open { key, spec: spec.clone() });
        Ok(spec)
    }

    async fn close(&self, server: &str) {
        self.open.lock().await.remove(server);
        let _ = self.run(&["close", &session_name(server)]).await;
    }

    async fn unauthorized(&self, server: &str) -> bool {
        matches!(self.session_status(&session_name(server)).await.as_deref(), Some("unauthorized"))
    }

    async fn signed_in(&self, url: &str) -> bool {
        let want = canonical_url(url);
        self.profiles().await.contains(&want)
    }

    async fn begin_login(&self, url: &str) -> Result<LoginStart, BrokerError> {
        self.logins.lock().retain(|_, l| l.started.elapsed() < LOGIN_TTL && matches!(*l.status.borrow(), LoginState::Waiting));
        let _one_at_a_time = self.starting.lock().await;
        match self.start_login(url).await {
            // a callback port taken by something else after mcpc found it free: once more picks another
            Err(BrokerError::Failed(m)) if m.contains("EADDRINUSE") => self.start_login(url).await,
            other => other,
        }
    }

    async fn deliver(&self, state: &str, query: &str) -> Result<(), BrokerError> {
        let login = self.logins.lock().iter().find(|(s, _)| same(s, state)).map(|(_, l)| l.clone());
        let login = login.ok_or_else(|| BrokerError::Failed("this sign-in is unknown or has expired: ask the agent again".into()))?;
        if !matches!(*login.status.borrow(), LoginState::Waiting) {
            return Err(BrokerError::Failed("this sign-in is already over".into()));
        }
        let params = callback_params(query);
        // the answer must carry this sign-in's own state (mcpc 0.8.0 checks it on its callback server, not on a pasted URL)
        let theirs = params.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str()).unwrap_or("");
        if !same(theirs, state) {
            return Err(BrokerError::Failed("the answer does not belong to this sign-in (its state differs)".into()));
        }
        let mut target = reqwest::Url::parse(&format!("http://127.0.0.1:{}/callback", login.port)).map_err(|e| BrokerError::Failed(e.to_string()))?;
        target.query_pairs_mut().extend_pairs(params.iter());
        // as the browser would have: mcpc's own callback server takes it
        match self.http.get(target.clone()).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => tracing::debug!(status = %r.status(), "mcpc's callback refused the answer; pasting it instead"),
            Err(e) => tracing::debug!("mcpc's callback is not reachable ({e}); pasting the answer instead"),
        }
        // mcpc also reads a pasted callback URL from its standard input
        let mut stdin = login.stdin.lock().await;
        let w = stdin.as_mut().ok_or_else(|| BrokerError::Failed("mcpc no longer reads the sign-in".into()))?;
        w.write_all(format!("{target}\n").as_bytes()).await.map_err(|e| BrokerError::Failed(format!("cannot hand the sign-in to mcpc: {e}")))?;
        w.flush().await.map_err(|e| BrokerError::Failed(e.to_string()))?;
        Ok(())
    }

    async fn wait_login(&self, state: &str, timeout: Duration) -> Result<(), BrokerError> {
        let login = self.logins.lock().get(state).cloned().ok_or_else(|| BrokerError::Failed("no such sign-in".into()))?;
        let mut rx = login.status.subscribe();
        let result = tokio::time::timeout(timeout, async {
            loop {
                match &*rx.borrow_and_update() {
                    LoginState::Done => return Ok(()),
                    LoginState::Failed(e) => return Err(BrokerError::Failed(e.clone())),
                    LoginState::Waiting => {}
                }
                if rx.changed().await.is_err() {
                    return Err(BrokerError::Failed("the sign-in was abandoned".into()));
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(BrokerError::Failed(format!("nobody signed in within {} minutes", timeout.as_secs().div_ceil(60)))));
        if result.is_ok() {
            *self.profiles.lock() = None;
            tracing::info!(server = %login.url, "signed in with mcpc");
        }
        if !matches!(*login.status.borrow(), LoginState::Waiting) {
            self.logins.lock().remove(state);
        }
        result
    }

    async fn logout(&self, url: &str) {
        if let Err(e) = self.run(&["logout", url]).await {
            tracing::debug!(server = %url, "mcpc logout: {}", e.message);
        }
        *self.profiles.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcpc_errors_are_read_from_its_json() {
        let e = outcome(Some(4), "", "some log line\n{\n  \"error\": \"Authentication required by server.\",\n  \"code\": 4\n}\n").unwrap_err();
        assert_eq!(e, McpcError { code: 4, message: "Authentication required by server.".into() });
        assert_eq!(BrokerError::from(e), BrokerError::NeedsLogin("Authentication required by server.".into()));
        let advice = McpcError { code: 4, message: "Authentication required by server.\n\nTry restarting the session first:\n  mcpc @x restart".into() };
        assert_eq!(BrokerError::from(advice), BrokerError::NeedsLogin("Authentication required by server.".into()));
        let e = outcome(Some(1), "", "{\"error\": \"Port 1 is already in use\", \"code\": 1}").unwrap_err();
        assert!(matches!(BrokerError::from(e), BrokerError::Failed(m) if m.contains("already in use")));
        assert_eq!(outcome(Some(0), "{\"sessions\": []}\n", "").unwrap()["sessions"], serde_json::json!([]));
        assert_eq!(outcome(Some(3), "", "boom").unwrap_err().message, "boom");
    }

    #[test]
    fn the_authorization_url_of_a_login_is_read_from_what_mcpc_prints() {
        let printed = "\nAuthorization URL: https://mcp.linear.app/authorize?response_type=code&client_id=https%3A%2F%2Fapify.github.io%2Fmcpc%2Fclient-metadata.json&code_challenge=x&code_challenge_method=S256&redirect_uri=http%3A%2F%2F127.0.0.1%3A13316%2Fcallback&scope=read+write&resource=https%3A%2F%2Fmcp.linear.app%2Fmcp&state=d4e52cb4\nOpening browser...\n";
        let (url, state, port) = parse_authorization(printed).unwrap();
        assert!(url.starts_with("https://mcp.linear.app/authorize?"));
        assert_eq!((state.as_str(), port), ("d4e52cb4", 13316));
        // a callback that is not on the loopback interface is not mcpc's
        assert!(parse_authorization("Authorization URL: https://a/authorize?state=s&redirect_uri=https%3A%2F%2Fevil.example%2Fcb").is_none());
        assert!(parse_authorization("Authorization URL: javascript:alert(1)").is_none());
        assert!(parse_authorization("Opening browser...").is_none());
    }

    #[test]
    fn session_names_are_what_mcpc_accepts() {
        assert_eq!(session_name("secure_weather"), "@dfae-secure-weather");
        let long = session_name(&"x".repeat(200));
        assert!(long.len() <= 65 && long.starts_with("@dfae-"));
    }
}
