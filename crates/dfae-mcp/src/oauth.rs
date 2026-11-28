//! Signing in to a remote MCP server for the user, as the MCP authorization spec has it (2025-11-25: OAuth 2.1 with PKCE, Protected
//! Resource Metadata (RFC 9728), authorization server metadata (RFC 8414 / OpenID Connect discovery), Dynamic Client Registration
//! (RFC 7591) and resource indicators (RFC 8707)).
//!
//! A server answers 401 with `WWW-Authenticate: Bearer resource_metadata="…"`; [`discover`] finds its authorization server from that,
//! [`Flow::start`] registers this engine as a client (once per authorization server) and writes the URL the user opens to sign in,
//! and the provider sends the browser back to the engine's callback with a code, which [`Flow::finish`] exchanges for tokens. A
//! [`Grant`] then gives every request its bearer token and refreshes it when it expires.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error)]
pub enum OAuthError {
    #[error("{0}")]
    Discovery(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("the provider refused: {0}")]
    Refused(String),
    #[error("{0}")]
    Transport(String),
}

/// What a server's 401 (or 403) says about signing in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
}

impl Challenge {
    /// `Bearer resource_metadata="https://…", scope="a b", error="insufficient_scope"` (any order, quoted or not).
    pub fn parse(header: &str) -> Self {
        let mut c = Challenge::default();
        let rest = header.trim();
        let rest = rest.strip_prefix("Bearer").or_else(|| rest.strip_prefix("bearer")).unwrap_or(rest);
        let mut s = rest;
        while let Some(eq) = s.find('=') {
            let key = s[..eq].trim().trim_start_matches(',').trim().to_ascii_lowercase();
            let after = s[eq + 1..].trim_start();
            let (value, next) = if let Some(q) = after.strip_prefix('"') {
                match q.find('"') {
                    Some(end) => (q[..end].to_string(), &q[end + 1..]),
                    None => (q.to_string(), ""),
                }
            } else {
                let end = after.find(',').unwrap_or(after.len());
                (after[..end].trim().to_string(), &after[end..])
            };
            match key.as_str() {
                "resource_metadata" => c.resource_metadata = Some(value),
                "scope" => c.scope = Some(value),
                "error" => c.error = Some(value),
                _ => {}
            }
            s = next;
        }
        c
    }
}

/// Where and how to sign in to one MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthServer {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    /// The scopes asked for (the challenge's, else everything the resource supports; empty: none named).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The MCP server's canonical URI, sent as `resource` (RFC 8707).
    pub resource: String,
}

/// The MCP server's canonical URI: lowercase scheme and host, no fragment, no trailing slash.
pub fn canonical_resource(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            u.set_fragment(None);
            u.set_query(None);
            let s = u.to_string();
            s.trim_end_matches('/').to_string()
        }
        Err(_) => url.trim_end_matches('/').to_string(),
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("http client")
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let r = http.get(url).header("accept", "application/json").send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json::<Value>().await.ok()
}

/// `https://h/a/b` + `/.well-known/x` → `https://h/.well-known/x/a/b` (path insertion), and the root form.
fn well_known(base: &str, name: &str) -> (Option<String>, String) {
    let u = reqwest::Url::parse(base).ok();
    let origin = u.as_ref().map(|u| u.origin().ascii_serialization()).unwrap_or_else(|| base.trim_end_matches('/').to_string());
    let path = u.as_ref().map(|u| u.path().trim_end_matches('/').to_string()).unwrap_or_default();
    let root = format!("{origin}/.well-known/{name}");
    let with_path = (!path.is_empty() && path != "/").then(|| format!("{origin}/.well-known/{name}{path}"));
    (with_path, root)
}

/// Find the authorization server of the MCP server at `endpoint` from its 401's challenge, in the order the spec gives.
pub async fn discover(endpoint: &str, challenge: &Challenge) -> Result<AuthServer, OAuthError> {
    let http = http();
    // 1. protected resource metadata: the challenge's URL, else the path-based and then the root well-known URI
    let mut prm_urls: Vec<String> = Vec::new();
    if let Some(u) = &challenge.resource_metadata {
        prm_urls.push(u.clone());
    }
    let (with_path, root) = well_known(endpoint, "oauth-protected-resource");
    prm_urls.extend(with_path);
    prm_urls.push(root);
    let mut prm: Option<Value> = None;
    for u in &prm_urls {
        if let Some(v) = get_json(&http, u).await.filter(|v| v.get("authorization_servers").and_then(|a| a.as_array()).is_some_and(|a| !a.is_empty())) {
            prm = Some(v);
            break;
        }
    }
    let prm = prm.ok_or_else(|| OAuthError::Discovery(format!("{endpoint} does not say where to sign in (no protected resource metadata)")))?;
    let issuer = prm["authorization_servers"][0].as_str().unwrap_or_default().trim_end_matches('/').to_string();
    // 2. authorization server metadata: RFC 8414, then OpenID Connect (path insertion first for an issuer with a path)
    let (as_path, as_root) = well_known(&issuer, "oauth-authorization-server");
    let (oidc_path, oidc_root) = well_known(&issuer, "openid-configuration");
    let candidates: Vec<String> = match (as_path, oidc_path) {
        (Some(a), Some(o)) => vec![a, o, format!("{issuer}/.well-known/openid-configuration")],
        _ => vec![as_root, oidc_root],
    };
    let mut meta: Option<Value> = None;
    for u in &candidates {
        if let Some(v) = get_json(&http, u).await.filter(|v| v.get("authorization_endpoint").is_some() && v.get("token_endpoint").is_some()) {
            meta = Some(v);
            break;
        }
    }
    let meta = meta.ok_or_else(|| OAuthError::Discovery(format!("the authorization server {issuer} publishes no metadata")))?;
    // PKCE: the server must say it supports S256, or the client must not go on
    let s256 = meta["code_challenge_methods_supported"].as_array().is_some_and(|a| a.iter().any(|m| m == "S256"));
    if !s256 {
        return Err(OAuthError::Unsupported(format!("{issuer} does not support PKCE (S256), which MCP sign-in requires")));
    }
    let scopes: Vec<String> = match &challenge.scope {
        Some(s) => s.split_whitespace().map(String::from).collect(),
        None => prm["scopes_supported"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default(),
    };
    let s = |k: &str| meta[k].as_str().map(String::from);
    let resource = prm["resource"].as_str().map(canonical_resource).unwrap_or_else(|| canonical_resource(endpoint));
    Ok(AuthServer {
        issuer,
        authorization_endpoint: s("authorization_endpoint").unwrap_or_default(),
        token_endpoint: s("token_endpoint").unwrap_or_default(),
        registration_endpoint: s("registration_endpoint"),
        scopes,
        resource,
    })
}

/// This engine as a client of one authorization server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Client {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}

/// Register with Dynamic Client Registration (RFC 7591) as a public client.
pub async fn register(server: &AuthServer, redirect_uri: &str) -> Result<Client, OAuthError> {
    let endpoint = server.registration_endpoint.as_deref().ok_or_else(|| {
        OAuthError::Unsupported(format!("{} does not let clients register themselves; a client id registered with it is needed", server.issuer))
    })?;
    let body = serde_json::json!({
        "client_name": "DFAE agent",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": server.scopes.join(" "),
    });
    let r = http().post(endpoint).json(&body).send().await.map_err(|e| OAuthError::Transport(e.to_string()))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(OAuthError::Refused(format!("registration answered HTTP {status}: {}", v.to_string().chars().take(200).collect::<String>())));
    }
    let client_id = v["client_id"].as_str().ok_or_else(|| OAuthError::Refused("registration gave no client_id".into()))?.to_string();
    Ok(Client { client_id, client_secret: v["client_secret"].as_str().map(String::from), redirect_uri: redirect_uri.to_string() })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds since the epoch.
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A random string of URL-safe characters (two v4 UUIDs: 244 random bits).
fn random() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for c in chars.iter().take(chunk.len() + 1) {
            out.push(A[*c as usize] as char);
        }
    }
    out
}

/// The S256 challenge of a PKCE verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// `application/x-www-form-urlencoded` of the pairs.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut u = reqwest::Url::parse("http://x/").expect("url");
    u.query_pairs_mut().extend_pairs(pairs.iter().copied());
    u.query().unwrap_or("").to_string()
}

async fn token_request(server: &AuthServer, client: &Client, pairs: &[(&str, &str)]) -> Result<Tokens, OAuthError> {
    let mut all: Vec<(&str, &str)> = pairs.to_vec();
    all.push(("client_id", &client.client_id));
    if let Some(s) = &client.client_secret {
        all.push(("client_secret", s));
    }
    all.push(("resource", &server.resource));
    let r = http()
        .post(&server.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(form(&all))
        .send()
        .await
        .map_err(|e| OAuthError::Transport(e.to_string()))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let why = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no reason given");
        return Err(OAuthError::Refused(format!("the token endpoint answered HTTP {status}: {why}")));
    }
    let access_token = v["access_token"].as_str().ok_or_else(|| OAuthError::Refused("no access_token in the answer".into()))?.to_string();
    Ok(Tokens {
        access_token,
        refresh_token: v["refresh_token"].as_str().map(String::from),
        expires_at: v["expires_in"].as_i64().map(|s| now() + s),
        scope: v["scope"].as_str().map(String::from),
    })
}

/// A sign-in that waits for the user: what to open, and what to check when the provider sends the browser back.
#[derive(Debug, Clone)]
pub struct Flow {
    /// The `state` sent with it: the key the callback is matched by.
    pub state: String,
    verifier: String,
    pub server: AuthServer,
    pub client: Client,
    /// The URL the user opens to sign in.
    pub url: String,
}

impl Flow {
    /// Write the sign-in URL (PKCE S256, state, resource, scopes) for a client already registered.
    pub fn start(server: AuthServer, client: Client) -> Result<Self, OAuthError> {
        let state = random();
        let verifier = random();
        let mut u = reqwest::Url::parse(&server.authorization_endpoint).map_err(|e| OAuthError::Discovery(format!("bad authorization endpoint: {e}")))?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &client.client_id)
                .append_pair("redirect_uri", &client.redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &pkce_challenge(&verifier))
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &server.resource);
            if !server.scopes.is_empty() {
                q.append_pair("scope", &server.scopes.join(" "));
            }
        }
        Ok(Self { state, verifier, url: u.to_string(), server, client })
    }

    /// The code the provider sent back, for tokens.
    pub async fn finish(&self, code: &str) -> Result<Tokens, OAuthError> {
        token_request(
            &self.server,
            &self.client,
            &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &self.client.redirect_uri), ("code_verifier", &self.verifier)],
        )
        .await
    }
}

/// What is kept for one MCP server once its user signed in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub server: AuthServer,
    pub client: Client,
    pub tokens: Tokens,
}

/// The tokens of one server, handed to every request; refreshed when they expire or the server refuses them.
pub struct Grant {
    saved: Mutex<Saved>,
    store: Option<Arc<Store>>,
    refreshing: tokio::sync::Mutex<()>,
}

impl Grant {
    pub fn new(saved: Saved, store: Option<Arc<Store>>) -> Arc<Self> {
        Arc::new(Self { saved: Mutex::new(saved), store, refreshing: tokio::sync::Mutex::new(()) })
    }

    /// The access token, unless it is known to have expired.
    pub fn token(&self) -> Option<String> {
        let s = self.saved.lock();
        match s.tokens.expires_at {
            Some(at) if at <= now() + 30 => None,
            _ => Some(s.tokens.access_token.clone()),
        }
    }

    pub fn resource(&self) -> String {
        self.saved.lock().server.resource.clone()
    }

    /// Ask for new tokens with the refresh token. `false`: there is none, or it was refused (the user must sign in again).
    pub async fn refresh(&self) -> bool {
        let _one = self.refreshing.lock().await;
        let saved = self.saved.lock().clone();
        // another call refreshed while this one waited
        if saved.tokens.expires_at.is_some_and(|at| at > now() + 30) && self.token().is_some_and(|t| t != saved.tokens.access_token) {
            return true;
        }
        let Some(rt) = saved.tokens.refresh_token.clone() else { return false };
        match token_request(&saved.server, &saved.client, &[("grant_type", "refresh_token"), ("refresh_token", &rt)]).await {
            Ok(mut t) => {
                // a provider that does not rotate refresh tokens keeps the old one valid
                if t.refresh_token.is_none() {
                    t.refresh_token = Some(rt);
                }
                let mut s = self.saved.lock();
                s.tokens = t;
                if let Some(store) = &self.store {
                    store.put(s.clone());
                }
                true
            }
            Err(e) => {
                tracing::info!(resource = %saved.server.resource, "refreshing the MCP sign-in failed: {e}");
                false
            }
        }
    }
}

/// Signed-in servers and registered clients. On disk only encrypted (ChaCha20-Poly1305, with a key derived from a secret the engine
/// holds and its commands do not: `DFAE_SECRET_KEY`, else the API token), in a file only the engine's user can read; without a
/// secret they are kept in memory only, so a command the agent runs as the same user cannot read them.
pub struct Store {
    path: Option<PathBuf>,
    key: Option<[u8; 32]>,
    data: Mutex<StoreData>,
}

#[derive(Default, Serialize, Deserialize)]
struct StoreData {
    /// By canonical resource URI.
    #[serde(default)]
    grants: BTreeMap<String, Saved>,
    /// By issuer + redirect URI: a client is registered once per authorization server.
    #[serde(default)]
    clients: BTreeMap<String, Client>,
}

impl Store {
    /// `secret`: what the file's key is derived from; `None` keeps everything in memory.
    pub fn open(path: Option<PathBuf>, secret: Option<&str>) -> Arc<Self> {
        let key: Option<[u8; 32]> = secret.filter(|s| !s.is_empty()).map(|s| Sha256::digest(format!("dfae mcp sign-in store v1\0{s}").as_bytes()).into());
        let path = path.filter(|_| key.is_some());
        let data = match (&path, &key) {
            (Some(p), Some(k)) => std::fs::read_to_string(p).ok().and_then(|t| open_sealed(k, &t)).unwrap_or_default(),
            _ => StoreData::default(),
        };
        Arc::new(Self { path, key, data: Mutex::new(data) })
    }

    pub fn get(&self, resource: &str) -> Option<Saved> {
        self.data.lock().grants.get(&canonical_resource(resource)).cloned()
    }

    pub fn put(&self, saved: Saved) {
        self.data.lock().grants.insert(saved.server.resource.clone(), saved);
        self.write();
    }

    pub fn forget(&self, resource: &str) {
        self.data.lock().grants.remove(&canonical_resource(resource));
        self.write();
    }

    pub fn client(&self, issuer: &str, redirect_uri: &str) -> Option<Client> {
        self.data.lock().clients.get(&format!("{issuer} {redirect_uri}")).cloned()
    }

    pub fn put_client(&self, issuer: &str, client: Client) {
        self.data.lock().clients.insert(format!("{issuer} {}", client.redirect_uri), client);
        self.write();
    }

    fn write(&self) {
        let (Some(path), Some(key)) = (&self.path, &self.key) else { return };
        let Ok(plain) = serde_json::to_vec(&*self.data.lock()) else { return };
        let Some(text) = seal(key, &plain) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        #[cfg(unix)]
        let written = {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(text.as_bytes()))
        };
        #[cfg(not(unix))]
        let written = std::fs::write(&tmp, text);
        if written.is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn seal(key: &[u8; 32], plain: &[u8]) -> Option<String> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(key.into());
    let nonce_bytes: [u8; 12] = uuid::Uuid::new_v4().as_bytes()[..12].try_into().ok()?;
    let sealed = cipher.encrypt(&nonce_bytes.into(), plain).ok()?;
    serde_json::to_string(&serde_json::json!({"v": 1, "nonce": hex::encode(nonce_bytes), "data": hex::encode(sealed)})).ok()
}

fn open_sealed(key: &[u8; 32], text: &str) -> Option<StoreData> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    let v: Value = serde_json::from_str(text).ok()?;
    let nonce: [u8; 12] = hex::decode(v["nonce"].as_str()?).ok()?.try_into().ok()?;
    let data = hex::decode(v["data"].as_str()?).ok()?;
    let plain = chacha20poly1305::ChaCha20Poly1305::new(key.into()).decrypt(&nonce.into(), data.as_slice()).ok()?;
    serde_json::from_slice(&plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_is_sealed_on_disk_and_kept_in_memory_without_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-oauth.json");
        let saved = Saved {
            server: AuthServer {
                issuer: "https://a".into(),
                authorization_endpoint: "https://a/authorize".into(),
                token_endpoint: "https://a/token".into(),
                registration_endpoint: None,
                scopes: vec![],
                resource: "https://mcp.example.com/mcp".into(),
            },
            client: Client { client_id: "c".into(), client_secret: None, redirect_uri: "http://localhost/cb".into() },
            tokens: Tokens { access_token: "very-secret-token".into(), refresh_token: Some("rt".into()), expires_at: None, scope: None },
        };
        let store = Store::open(Some(path.clone()), Some("engine secret"));
        store.put(saved.clone());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("very-secret-token") && !on_disk.contains("mcp.example.com"), "{on_disk}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(Store::open(Some(path.clone()), Some("engine secret")).get("https://mcp.example.com/mcp").unwrap().tokens.access_token, "very-secret-token");
        assert!(Store::open(Some(path.clone()), Some("another secret")).get("https://mcp.example.com/mcp").is_none(), "a wrong key reads nothing");
        // no secret: nothing is written
        let mem_path = dir.path().join("mem.json");
        let mem = Store::open(Some(mem_path.clone()), None);
        mem.put(saved);
        assert!(mem.get("https://mcp.example.com/mcp").is_some() && !mem_path.exists());
    }

    #[test]
    fn a_challenge_is_read_whatever_its_order_and_quoting() {
        let c = Challenge::parse(
            r#"Bearer error="insufficient_scope", scope="files:read files:write", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#,
        );
        assert_eq!(c.resource_metadata.as_deref(), Some("https://mcp.example.com/.well-known/oauth-protected-resource"));
        assert_eq!(c.scope.as_deref(), Some("files:read files:write"));
        assert_eq!(c.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(Challenge::parse("Bearer realm=x, error=invalid_token").error.as_deref(), Some("invalid_token"));
        assert_eq!(Challenge::parse(""), Challenge::default());
    }

    #[test]
    fn pkce_and_resource_uris_are_as_the_specs_have_them() {
        // RFC 7636 appendix B
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(canonical_resource("HTTPS://MCP.Example.com/mcp/#x"), "https://mcp.example.com/mcp");
        assert_eq!(canonical_resource("https://mcp.example.com/"), "https://mcp.example.com");
        assert_eq!(
            well_known("https://example.com/public/mcp", "oauth-protected-resource"),
            (
                Some("https://example.com/.well-known/oauth-protected-resource/public/mcp".into()),
                "https://example.com/.well-known/oauth-protected-resource".into()
            )
        );
        assert_eq!(well_known("https://auth.example.com", "openid-configuration").0, None);
    }

    #[test]
    fn a_sign_in_url_carries_pkce_state_and_the_resource() {
        let server = AuthServer {
            issuer: "https://auth.example.com".into(),
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            registration_endpoint: None,
            scopes: vec!["read".into(), "write".into()],
            resource: "https://mcp.example.com/mcp".into(),
        };
        let client = Client { client_id: "abc".into(), client_secret: None, redirect_uri: "http://localhost:8080/v1/mcp/oauth/callback".into() };
        let f = Flow::start(server, client).unwrap();
        let u = reqwest::Url::parse(&f.url).unwrap();
        let q: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], pkce_challenge(&f.verifier));
        assert_eq!(q["state"], f.state);
        assert_eq!(q["resource"], "https://mcp.example.com/mcp");
        assert_eq!(q["scope"], "read write");
        assert_eq!(q["redirect_uri"], "http://localhost:8080/v1/mcp/oauth/callback");
        assert!(f.state.len() >= 43 && f.verifier.len() >= 43);
    }
}
