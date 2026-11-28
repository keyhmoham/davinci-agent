//! Deterministic policy engine.

use dfae_core::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    RequireApproval(String),
    Deny(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesystemPolicy {
    /// Directories tools may read. `{workspace}` expands to the workspace.
    pub read_roots: Vec<String>,
    pub write_roots: Vec<String>,
    pub write_requires_approval: bool,
    /// Path components that are never touched.
    pub deny_names: Vec<String>,
}

/// How much of a shell the agent gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellMode {
    /// One allowlisted command, no shell (no pipes, redirects, `&&`): for running on a machine that matters.
    #[default]
    Allowlist,
    /// A full bash in the workspace — pipes, redirects, git, build and test tools. The container
    /// (read-only root, no capabilities, unprivileged user, memory/pid limits) is the boundary, so
    /// use it only inside one.
    Sandbox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellPolicy {
    pub mode: ShellMode,
    /// Longest `timeout_secs` a sandbox command may ask for.
    pub max_timeout_secs: u64,
    /// Read-only commands that run without approval.
    pub allowed: Vec<String>,
    /// Commands that run only after explicit approval.
    pub approval: Vec<String>,
    /// Commands whose path-like arguments must stay inside the read roots.
    pub path_checked: Vec<String>,
    /// Per-command arguments that turn a read-only command into a writing or
    /// executing one (`find -exec`, `sort -o`). Matches `ARG`, `ARG=value`, and
    /// joined short forms (`-ofile`).
    pub denied_args: std::collections::BTreeMap<String, Vec<String>>,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
    /// (sandbox mode) Where the agent installs what a task needs — packages, programs, downloaded binaries. It lives in the
    /// workspace so installs outlive the container; `{workspace}` expands.
    pub tools_dir: String,
    /// (sandbox mode) Default `timeout_secs` of a command, and of one that installs or builds (pip, npm, cargo, apt, make, git clone…).
    pub default_timeout_secs: u64,
    pub install_timeout_secs: u64,
    /// (sandbox mode) Variables of the engine's own environment that commands inherit: proxies and certificate bundles, so installs work
    /// behind them. Keys and tokens are never on this list.
    pub env_passthrough: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkPolicy {
    /// Domains fetched without approval; `*.example.com` matches subdomains.
    pub allowed_domains: Vec<String>,
    /// Public hosts that are not listed need approval (true) or are denied (false).
    pub unlisted_requires_approval: bool,
    /// Read-only GETs to any *public* host run without approval (the web is the agent's window on the
    /// world). Private/loopback addresses stay blocked, and once the task is suspected of a prompt
    /// injection, unlisted hosts need approval again (a fetched URL is an exfiltration channel).
    pub open_web: bool,
    pub max_response_bytes: usize,
    pub timeout_secs: u64,
    /// Ports on this machine where the agent's own servers run (the ones its plans start a server on). With the sandbox shell,
    /// `http_get` may ask `localhost` / `127.0.0.1` on these ports and no others: checking a site it built is not SSRF, the engine's
    /// API, the database and every other local service stay out of reach. Empty: loopback is always blocked.
    pub own_server_ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PolicyConfig {
    pub filesystem: FilesystemPolicy,
    pub shell: ShellPolicy,
    pub network: NetworkPolicy,
    /// Set from `DFAE_APPROVALS` at start-up (not read from the policy file).
    #[serde(skip)]
    pub approvals: ApprovalMode,
}

impl Default for FilesystemPolicy {
    fn default() -> Self {
        Self {
            read_roots: vec!["{workspace}".into()],
            write_roots: vec!["{workspace}".into()],
            write_requires_approval: true,
            deny_names: vec![".git".into(), ".env".into(), "id_rsa".into(), "id_ed25519".into(), ".ssh".into(), ".aws".into(), "mcp-oauth.json".into()],
        }
    }
}

impl Default for ShellPolicy {
    fn default() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Self {
            mode: ShellMode::Allowlist,
            max_timeout_secs: 900,
            allowed: v(&[
                "ls", "cat", "head", "tail", "wc", "grep", "find", "echo", "date", "uname", "df", "free", "uptime", "ps", "pwd", "whoami", "hostname", "stat",
                "du", "sort", "diff", "cut", "tr", "basename", "dirname", "nproc", "id",
            ]),
            approval: v(&["mkdir", "touch", "cp", "mv"]),
            path_checked: v(&["cat", "head", "tail", "wc", "grep", "find", "ls", "stat", "du", "sort", "diff", "cut", "tr"]),
            denied_args: [
                ("find", v(&["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls"])),
                ("sort", v(&["-o", "--output"])),
                ("date", v(&["-s", "--set"])),
            ]
            .into_iter()
            .map(|(k, a)| (k.to_string(), a))
            .collect(),
            timeout_secs: 10,
            max_output_bytes: 64 * 1024,
            tools_dir: "{workspace}/.tools".into(),
            default_timeout_secs: 120,
            install_timeout_secs: 600,
            env_passthrough: [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "NO_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "no_proxy",
                "all_proxy",
                "SSL_CERT_FILE",
                "SSL_CERT_DIR",
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                "GIT_SSL_CAINFO",
                "NODE_EXTRA_CA_CERTS",
                "PIP_INDEX_URL",
                "PIP_EXTRA_INDEX_URL",
                "PIP_TRUSTED_HOST",
                "NPM_CONFIG_REGISTRY",
                "GOPROXY",
                "GOSUMDB",
                "TZ",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Self {
            allowed_domains: v(&[
                "example.com",
                "example.org",
                "*.wikipedia.org",
                "github.com",
                "*.github.com",
                "raw.githubusercontent.com",
                "docs.rs",
                "crates.io",
                "nvd.nist.gov",
                "*.postgresql.org",
                "httpbin.org",
                "*.python.org",
                "*.rust-lang.org",
            ]),
            unlisted_requires_approval: true,
            open_web: false,
            max_response_bytes: 256 * 1024,
            timeout_secs: 15,
            own_server_ports: (8000..=8010).collect(),
        }
    }
}

impl PolicyConfig {
    pub fn from_toml(s: &str) -> Result<Self, String> {
        toml::from_str(s).map_err(|e| e.to_string())
    }
}

/// Lexically normalise a path (resolve `.` and `..` without touching the disk).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub struct PolicyEngine {
    cfg: PolicyConfig,
    workspace: PathBuf,
}

impl PolicyEngine {
    pub fn new(cfg: PolicyConfig, workspace: PathBuf) -> Self {
        let workspace = std::fs::create_dir_all(&workspace).ok().and_then(|_| workspace.canonicalize().ok()).unwrap_or(workspace);
        Self { cfg, workspace }
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.cfg
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn expand(&self, roots: &[String]) -> Vec<PathBuf> {
        roots
            .iter()
            .map(|r| {
                let r = r.replace("{workspace}", &self.workspace.to_string_lossy());
                let p = PathBuf::from(r);
                normalize(&p).canonicalize().unwrap_or_else(|_| normalize(&p))
            })
            .collect()
    }

    pub fn guard(&self) -> PathGuard {
        PathGuard {
            workspace: self.workspace.clone(),
            read_roots: self.expand(&self.cfg.filesystem.read_roots),
            write_roots: self.expand(&self.cfg.filesystem.write_roots),
            deny_names: self.cfg.filesystem.deny_names.clone(),
        }
    }

    /// Evaluate the policy for a concrete tool call.
    pub fn check(&self, meta: &ToolMetadata, args: &Value, tainted: bool) -> PolicyDecision {
        let guard = self.guard();
        let mut decision = match meta.name.as_str() {
            "fs_list" | "fs_read" | "fs_search" => match guard.read_path(args.get("path").and_then(|v| v.as_str()).unwrap_or(".")) {
                Ok(_) => PolicyDecision::Allow,
                Err(e) => PolicyDecision::Deny(e.message),
            },
            "fs_write" => match guard.write_path(args.get("path").and_then(|v| v.as_str()).unwrap_or("")) {
                Ok(_) if self.cfg.filesystem.write_requires_approval => PolicyDecision::RequireApproval("writes a file".into()),
                Ok(_) => PolicyDecision::Allow,
                Err(e) => PolicyDecision::Deny(e.message),
            },
            "shell" => {
                let cwd_ok = match args.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty()) {
                    Some(c) => guard.read_path(c).map(|_| ()),
                    None => Ok(()),
                };
                match cwd_ok {
                    Ok(()) => self.check_shell(&guard, args.get("command").and_then(|v| v.as_str()).unwrap_or("")),
                    Err(e) => PolicyDecision::Deny(e.message),
                }
            }
            "fs_glob" => match guard.read_path(args.get("path").and_then(|v| v.as_str()).unwrap_or(".")) {
                Ok(_) => PolicyDecision::Allow,
                Err(e) => PolicyDecision::Deny(e.message),
            },
            // changes to existing files and the tree: every path involved must be writable
            "fs_edit" | "fs_mkdir" | "fs_delete" | "fs_move" | "fs_copy" => {
                let paths: Vec<&str> = ["path", "from", "to"].iter().filter_map(|k| args.get(*k).and_then(|v| v.as_str())).collect();
                match paths.iter().find_map(|p| guard.write_path(p).err()) {
                    Some(e) => PolicyDecision::Deny(e.message),
                    None if self.cfg.filesystem.write_requires_approval => PolicyDecision::RequireApproval("changes files".into()),
                    None => PolicyDecision::Allow,
                }
            }
            "http_get" => self.check_url(args.get("url").and_then(|v| v.as_str()).unwrap_or(""), tainted),
            _ => PolicyDecision::Allow,
        };
        if decision == PolicyDecision::Allow {
            if self.cfg.approvals == ApprovalMode::Never {
                // nothing waits for a human: only what the rules above refused is refused
            } else if meta.requires_approval {
                decision = PolicyDecision::RequireApproval(format!("tool '{}' always requires approval", meta.name));
            } else if meta.risk == Risk::High {
                decision = PolicyDecision::RequireApproval("high-risk tool".into());
            } else if tainted && meta.side_effects {
                decision = PolicyDecision::RequireApproval("context contains suspected prompt injection".into());
            }
        }
        // without approvals, "needs a human" means "go ahead"
        if self.cfg.approvals == ApprovalMode::Never && matches!(decision, PolicyDecision::RequireApproval(_)) {
            decision = PolicyDecision::Allow;
        }
        decision
    }

    fn check_shell(&self, guard: &PathGuard, command: &str) -> PolicyDecision {
        let sh = &self.cfg.shell;
        if sh.mode == ShellMode::Sandbox {
            return if command.trim().is_empty() { PolicyDecision::Deny("empty command".into()) } else { PolicyDecision::Allow };
        }
        let Some(argv) = shlex::split(command) else {
            return PolicyDecision::Deny("unparseable command line".into());
        };
        let Some(bin) = argv.first() else {
            return PolicyDecision::Deny("empty command".into());
        };
        if bin.contains('/') {
            return PolicyDecision::Deny("commands must be given by bare name, not path".into());
        }
        let allowed = sh.allowed.iter().any(|c| c == bin);
        let needs_approval = sh.approval.iter().any(|c| c == bin);
        if !allowed && !needs_approval {
            return PolicyDecision::Deny(format!("command '{bin}' is not on the allowlist"));
        }
        let denied: &[String] = sh.denied_args.get(bin.as_str()).map(|v| v.as_slice()).unwrap_or(&[]);
        for a in &argv[1..] {
            let hit = denied.iter().any(|d| {
                a == d
                    || a.strip_prefix(d.as_str()).is_some_and(|rest| rest.starts_with('='))
                    || (d.len() == 2 && !a.starts_with("--") && a.len() > 2 && a.starts_with(d.as_str()))
            });
            if hit {
                return PolicyDecision::Deny(format!("argument '{a}' is not permitted for {bin}"));
            }
        }
        if sh.path_checked.iter().any(|c| c == bin) || needs_approval {
            // Every non-flag argument — and the value of `--flag=value` — is
            // resolved (symlinks included) and must stay inside the roots. This
            // also catches `du --files0-from=/etc/passwd` and links that point out.
            for a in &argv[1..] {
                let candidate: &str = if a.starts_with('-') {
                    match a.split_once('=') {
                        Some((_, v)) => v,
                        None => continue,
                    }
                } else {
                    a
                };
                if candidate.is_empty() {
                    continue;
                }
                let res = if needs_approval { guard.write_path(candidate).map(|_| ()) } else { guard.read_path(candidate).map(|_| ()) };
                if let Err(e) = res {
                    return PolicyDecision::Deny(e.message);
                }
            }
        }
        if needs_approval { PolicyDecision::RequireApproval(format!("'{bin}' modifies the filesystem")) } else { PolicyDecision::Allow }
    }

    /// Scheme, credentials and local/private-address checks that apply to *every* outbound request,
    /// including the fixed backends of the search tool. Returns the lowercase host.
    pub fn public_host(&self, url: &str) -> Result<String, String> {
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err("URL must include a scheme".into());
        };
        if scheme != "http" && scheme != "https" {
            return Err(format!("scheme '{scheme}' is not permitted"));
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') {
            return Err("credentials in URLs are not permitted".into());
        }
        let host = authority
            .rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
            .map(|(h, _)| h)
            .unwrap_or(authority)
            .trim_matches(['[', ']'])
            .to_lowercase();
        if host.is_empty() {
            return Err("URL has no host".into());
        }
        if let Ok(ip) = host.parse::<std::net::IpAddr>()
            && is_private_ip(&ip)
        {
            return Err("requests to private or loopback addresses are blocked".into());
        }
        if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") {
            return Err("requests to local hosts are blocked".into());
        }
        Ok(host)
    }

    /// A URL of one of the agent's own servers: loopback, on a port where its plans serve, with the sandbox shell (where those
    /// servers run). Only plain `http`, no credentials.
    pub fn own_server(&self, url: &str) -> bool {
        if self.cfg.shell.mode != ShellMode::Sandbox {
            return false;
        }
        let Some(rest) = url.strip_prefix("http://") else { return false };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') {
            return false;
        }
        let Some((host, port)) = authority.rsplit_once(':') else { return false };
        matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0") && port.parse::<u16>().is_ok_and(|p| self.cfg.network.own_server_ports.contains(&p))
    }

    fn check_url(&self, url: &str, tainted: bool) -> PolicyDecision {
        if self.own_server(url) {
            return PolicyDecision::Allow;
        }
        let host = match self.public_host(url) {
            Ok(h) => h,
            Err(e) => return PolicyDecision::Deny(e),
        };
        if self.cfg.network.allowed_domains.iter().any(|d| domain_matches(d, &host)) {
            PolicyDecision::Allow
        } else if self.cfg.network.open_web {
            if tainted && self.cfg.approvals == ApprovalMode::Ask {
                PolicyDecision::RequireApproval(format!("'{host}' is not on the allowlist and the context contains suspected prompt injection"))
            } else {
                PolicyDecision::Allow
            }
        } else if self.cfg.network.unlisted_requires_approval && self.cfg.approvals == ApprovalMode::Ask {
            PolicyDecision::RequireApproval(format!("'{host}' is not on the network allowlist"))
        } else {
            PolicyDecision::Deny(format!("'{host}' is not on the network allowlist"))
        }
    }

    pub fn host_allowed_without_approval(&self, url: &str) -> bool {
        self.check_url(url, false) == PolicyDecision::Allow
    }
}

fn domain_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.to_lowercase();
    match p.strip_prefix("*.") {
        Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
        None => host == p,
    }
}

pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr::*;
    match ip {
        V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.octets()[0] == 0
                || (v.octets()[0] == 100 && (64..128).contains(&v.octets()[1]))
        }
        V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
                || v.to_ipv4_mapped().map(|m| is_private_ip(&V4(m))).unwrap_or(false)
        }
    }
}

/// Resolves and validates filesystem paths. Used by the policy engine *and*
/// again inside the tools at execution time.
#[derive(Clone)]
pub struct PathGuard {
    pub workspace: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    deny_names: Vec<String>,
}

impl PathGuard {
    fn resolve(&self, raw: &str, roots: &[PathBuf], what: &str) -> Result<PathBuf, ToolError> {
        if raw.contains('\0') {
            return Err(ToolError::policy("path contains NUL"));
        }
        let p = Path::new(raw);
        let joined = if p.is_absolute() { p.to_path_buf() } else { self.workspace.join(p) };
        let norm = normalize(&joined);
        // resolve symlinks of the deepest existing ancestor to defeat link escapes
        let mut existing = norm.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !existing.exists() {
            match existing.file_name() {
                Some(n) => tail.push(n.to_os_string()),
                None => break,
            }
            if !existing.pop() {
                break;
            }
        }
        let mut real = existing.canonicalize().unwrap_or(existing);
        for t in tail.into_iter().rev() {
            real.push(t);
        }
        if !roots.iter().any(|r| real.starts_with(r)) {
            return Err(ToolError::policy(format!("{what} access outside the allowed roots: {raw}")));
        }
        for c in real.components() {
            if let Component::Normal(n) = c
                && self.deny_names.iter().any(|d| n.to_string_lossy() == d.as_str())
            {
                return Err(ToolError::policy(format!("access to '{}' is not permitted", n.to_string_lossy())));
            }
        }
        Ok(real)
    }

    pub fn read_path(&self, raw: &str) -> Result<PathBuf, ToolError> {
        self.resolve(raw, &self.read_roots, "read")
    }

    /// Does `raw` exist, as an entry of the workspace? A symbolic link counts as existing without following it: a virtualenv's
    /// `.venv/bin/python` points at the system's python, outside the roots, and is still there.
    pub fn exists(&self, raw: &str) -> Result<bool, ToolError> {
        let joined = if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { self.workspace.join(raw) };
        let norm = normalize(&joined);
        let (Some(parent), Some(name)) = (norm.parent(), norm.file_name()) else { return self.read_path(raw).map(|p| p.exists()) };
        // the directory that holds it is resolved and must be inside the roots; the entry itself is not followed
        let real_parent = self.resolve(&parent.to_string_lossy(), &self.read_roots, "read")?;
        if self.deny_names.iter().any(|d| name.to_string_lossy() == d.as_str()) {
            return Err(ToolError::policy(format!("access to '{}' is not permitted", name.to_string_lossy())));
        }
        Ok(std::fs::symlink_metadata(real_parent.join(name)).is_ok())
    }

    pub fn write_path(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.trim().is_empty() {
            return Err(ToolError::args("empty path"));
        }
        self.resolve(raw, &self.write_roots, "write")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_program_outside_the_workspace_still_exists_there() {
        let (p, d) = engine();
        std::fs::create_dir_all(d.path().join(".venv/bin")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", d.path().join(".venv/bin/python")).unwrap();
        let g = p.guard();
        assert!(g.read_path(".venv/bin/python").is_err(), "reading through the link leaves the workspace");
        assert!(g.exists(".venv/bin/python").unwrap());
        assert!(!g.exists(".venv/bin/python3").unwrap());
        assert!(g.exists("../outside").is_err() && g.exists("/etc/passwd").is_err(), "a path outside the roots is still refused");
        assert!(g.exists(".git").is_err(), "protected names stay protected");
    }

    fn engine() -> (PolicyEngine, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        (PolicyEngine::new(PolicyConfig::default(), d.path().to_path_buf()), d)
    }

    fn meta(name: &str) -> ToolMetadata {
        ToolMetadata {
            name: name.into(),
            description: String::new(),
            category: String::new(),
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

    #[test]
    fn shipped_policy_file_parses_and_matches_the_defaults() {
        let shipped = PolicyConfig::from_toml(include_str!("../../../config/policy.toml")).expect("config/policy.toml must parse");
        // the one deliberate difference: the shipped file opens the web, the code default stays closed
        let mut expected = PolicyConfig::default();
        assert!(!expected.network.open_web, "the built-in default must stay closed");
        expected.network.open_web = true;
        // ...and, since the shipped file is only used inside the container, it gives the agent a full shell
        assert_eq!(expected.shell.mode, ShellMode::Allowlist, "the built-in default must stay restricted");
        expected.shell.mode = ShellMode::Sandbox;
        assert_eq!(serde_json::to_value(&shipped).unwrap(), serde_json::to_value(expected).unwrap());
    }

    #[test]
    fn partial_policy_files_fall_back_to_safe_defaults() {
        let p = PolicyConfig::from_toml("[network]\nunlisted_requires_approval = false\n").unwrap();
        assert!(!p.network.unlisted_requires_approval);
        assert!(p.filesystem.write_requires_approval, "unspecified sections keep their defaults");
        assert!(PolicyConfig::from_toml("[filesystem\n").is_err());
    }

    #[test]
    fn blocks_path_escapes() {
        let (e, _d) = engine();
        for p in ["/etc/passwd", "../../etc/passwd", "a/../../../etc/shadow", "/proc/self/environ"] {
            assert!(matches!(e.check(&meta("fs_read"), &serde_json::json!({"path": p}), false), PolicyDecision::Deny(_)), "{p}");
        }
        assert_eq!(e.check(&meta("fs_read"), &serde_json::json!({"path": "notes/a.txt"}), false), PolicyDecision::Allow);
    }

    #[test]
    fn the_agents_own_servers_may_be_asked_and_nothing_else_on_this_machine() {
        let mut cfg = PolicyConfig::default();
        cfg.network.open_web = true;
        let allowlist = PolicyEngine::new(cfg.clone(), std::env::temp_dir());
        assert!(!allowlist.own_server("http://localhost:8000/"), "only with the sandbox shell, where the agent's servers run");
        cfg.shell.mode = ShellMode::Sandbox;
        let p = PolicyEngine::new(cfg, std::env::temp_dir());
        for ok in ["http://localhost:8000/", "http://127.0.0.1:8010/about.html", "http://0.0.0.0:8003"] {
            assert!(p.own_server(ok), "{ok}");
        }
        for no in [
            "http://localhost:8080/v1/tasks",
            "http://127.0.0.1:5432",
            "http://localhost/",
            "https://localhost:8000/",
            "http://u:p@localhost:8000/",
            "http://10.0.0.5:8000/",
        ] {
            assert!(!p.own_server(no), "{no}");
        }
        let get = |u: &str| p.check(&meta("http_get"), &serde_json::json!({"url": u}), false);
        assert_eq!(get("http://localhost:8001/"), PolicyDecision::Allow);
        assert!(matches!(get("http://localhost:8080/"), PolicyDecision::Deny(_)), "the engine's own API stays out of reach");
    }

    #[test]
    fn symlink_escape_is_blocked() {
        let (e, d) = engine();
        std::os::unix::fs::symlink("/etc", d.path().join("link")).unwrap();
        assert!(matches!(e.check(&meta("fs_read"), &serde_json::json!({"path": "link/passwd"}), false), PolicyDecision::Deny(_)));
    }

    #[test]
    fn writes_need_approval_and_secret_names_are_denied() {
        let (e, _d) = engine();
        assert!(matches!(e.check(&meta("fs_write"), &serde_json::json!({"path": "a.txt", "content": "x"}), false), PolicyDecision::RequireApproval(_)));
        assert!(matches!(e.check(&meta("fs_write"), &serde_json::json!({"path": ".env", "content": "x"}), false), PolicyDecision::Deny(_)));
        assert!(matches!(e.check(&meta("fs_read"), &serde_json::json!({"path": ".git/config"}), false), PolicyDecision::Deny(_)));
    }

    #[test]
    fn shell_allowlist() {
        let (e, _d) = engine();
        let c = |s: &str| e.check(&meta("shell"), &serde_json::json!({"command": s}), false);
        assert_eq!(c("df -h"), PolicyDecision::Allow);
        assert_eq!(c("ls -la"), PolicyDecision::Allow);
        assert!(matches!(c("rm -rf /"), PolicyDecision::Deny(_)));
        assert!(matches!(c("/bin/ls"), PolicyDecision::Deny(_)));
        assert!(matches!(c("cat /etc/passwd"), PolicyDecision::Deny(_)));
        assert!(matches!(c("find . -exec rm {} ;"), PolicyDecision::Deny(_)));
        assert!(matches!(c("find . -okdir rm {} ;"), PolicyDecision::Deny(_)));
        // reading / writing outside through option values
        assert!(matches!(c("du --files0-from=/etc/passwd"), PolicyDecision::Deny(_)));
        assert!(matches!(c("grep -f /etc/passwd x"), PolicyDecision::Deny(_)));
        assert!(matches!(c("grep --file=/etc/passwd x"), PolicyDecision::Deny(_)));
        assert!(matches!(c("sort -o out.txt in.txt"), PolicyDecision::Deny(_)));
        assert!(matches!(c("sort -oout.txt in.txt"), PolicyDecision::Deny(_)));
        assert!(matches!(c("sort --output=out.txt in.txt"), PolicyDecision::Deny(_)));
        assert!(matches!(c("date -s 2020-01-01"), PolicyDecision::Deny(_)));
        assert!(matches!(c("uniq in.txt out.txt"), PolicyDecision::Deny(_)), "uniq can write its second operand and is not allowlisted");
        // ordinary flags that merely look similar stay allowed
        assert_eq!(c("grep -o foo notes.txt"), PolicyDecision::Allow);
        assert_eq!(c("grep -rn TODO ."), PolicyDecision::Allow);
        assert_eq!(c("find . -name '*.rs' -type f"), PolicyDecision::Allow);
        assert_eq!(c("sort notes.txt"), PolicyDecision::Allow);
        assert_eq!(c("head -n 5 notes.txt"), PolicyDecision::Allow);
        assert_eq!(c("ls -o"), PolicyDecision::Allow);
        assert!(matches!(c("mkdir out"), PolicyDecision::RequireApproval(_)));
        assert!(matches!(c("ls; rm -rf /"), PolicyDecision::Deny(_)));
    }

    #[test]
    fn shell_cannot_follow_a_symlink_out_of_the_workspace() {
        let (e, d) = engine();
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("pw")).unwrap();
        std::os::unix::fs::symlink("/etc", d.path().join("etc")).unwrap();
        let c = |s: &str| e.check(&meta("shell"), &serde_json::json!({"command": s}), false);
        assert!(matches!(c("cat pw"), PolicyDecision::Deny(_)));
        assert!(matches!(c("ls etc"), PolicyDecision::Deny(_)));
        assert!(matches!(c("grep root etc/passwd"), PolicyDecision::Deny(_)));
    }

    #[test]
    fn network_policy() {
        let (e, _d) = engine();
        let c = |u: &str| e.check(&meta("http_get"), &serde_json::json!({"url": u}), false);
        assert_eq!(c("https://example.com/x"), PolicyDecision::Allow);
        assert_eq!(c("https://en.wikipedia.org/wiki/Rust"), PolicyDecision::Allow);
        assert!(matches!(c("https://unknown.example.net/"), PolicyDecision::RequireApproval(_)));
        assert!(matches!(c("http://127.0.0.1:8080/"), PolicyDecision::Deny(_)));
        assert!(matches!(c("http://169.254.169.254/latest/meta-data"), PolicyDecision::Deny(_)));
        assert!(matches!(c("http://localhost/"), PolicyDecision::Deny(_)));
        assert!(matches!(c("file:///etc/passwd"), PolicyDecision::Deny(_)));
        assert!(matches!(c("https://user:pw@example.com/"), PolicyDecision::Deny(_)));
        assert!(matches!(c("https://evilexample.com/"), PolicyDecision::RequireApproval(_)));
    }

    #[test]
    fn open_web_allows_public_hosts_but_never_private_ones_and_not_after_an_injection() {
        let d = tempfile::tempdir().unwrap();
        let cfg = PolicyConfig::from_toml("[network]\nopen_web = true\n").unwrap();
        let e = PolicyEngine::new(cfg, d.path().to_path_buf());
        let c = |u: &str, tainted: bool| e.check(&meta("http_get"), &serde_json::json!({"url": u}), tainted);
        assert_eq!(c("https://news.ycombinator.com/", false), PolicyDecision::Allow);
        assert!(
            matches!(c("https://news.ycombinator.com/?q=secret", true), PolicyDecision::RequireApproval(_)),
            "a suspected injection turns a fetch into a possible leak"
        );
        assert_eq!(c("https://example.com/", true), PolicyDecision::Allow, "allowlisted hosts stay open");
        for u in ["http://127.0.0.1/", "http://10.0.0.5/", "http://169.254.169.254/", "http://localhost/", "https://u:p@example.com/", "ftp://example.com/"] {
            assert!(matches!(c(u, false), PolicyDecision::Deny(_)), "{u}");
        }
        assert!(e.public_host("https://api.open-meteo.com/v1/forecast").is_ok());
        assert!(e.public_host("http://192.168.1.1/").is_err());
    }

    #[test]
    fn taint_forces_approval_for_side_effects() {
        let (e, _d) = engine();
        let mut m = meta("memory_store");
        m.side_effects = true;
        assert_eq!(e.check(&m, &serde_json::json!({}), false), PolicyDecision::Allow);
        assert!(matches!(e.check(&m, &serde_json::json!({}), true), PolicyDecision::RequireApproval(_)));
    }
}
