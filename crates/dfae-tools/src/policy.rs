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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellPolicy {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkPolicy {
    /// Domains fetched without approval; `*.example.com` matches subdomains.
    pub allowed_domains: Vec<String>,
    /// Public hosts that are not listed need approval (true) or are denied (false).
    pub unlisted_requires_approval: bool,
    pub max_response_bytes: usize,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PolicyConfig {
    pub filesystem: FilesystemPolicy,
    pub shell: ShellPolicy,
    pub network: NetworkPolicy,
}

impl Default for FilesystemPolicy {
    fn default() -> Self {
        Self {
            read_roots: vec!["{workspace}".into()],
            write_roots: vec!["{workspace}".into()],
            write_requires_approval: true,
            deny_names: vec![".git".into(), ".env".into(), "id_rsa".into(), "id_ed25519".into(), ".ssh".into(), ".aws".into()],
        }
    }
}

impl Default for ShellPolicy {
    fn default() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Self {
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
            max_response_bytes: 256 * 1024,
            timeout_secs: 15,
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
            "shell" => self.check_shell(&guard, args.get("command").and_then(|v| v.as_str()).unwrap_or("")),
            "http_get" => self.check_url(args.get("url").and_then(|v| v.as_str()).unwrap_or("")),
            _ => PolicyDecision::Allow,
        };
        if decision == PolicyDecision::Allow {
            if meta.requires_approval {
                decision = PolicyDecision::RequireApproval(format!("tool '{}' always requires approval", meta.name));
            } else if meta.risk == Risk::High {
                decision = PolicyDecision::RequireApproval("high-risk tool".into());
            } else if tainted && meta.side_effects {
                decision = PolicyDecision::RequireApproval("context contains suspected prompt injection".into());
            }
        }
        decision
    }

    fn check_shell(&self, guard: &PathGuard, command: &str) -> PolicyDecision {
        let sh = &self.cfg.shell;
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

    fn check_url(&self, url: &str) -> PolicyDecision {
        let Some((scheme, rest)) = url.split_once("://") else {
            return PolicyDecision::Deny("URL must include a scheme".into());
        };
        if scheme != "http" && scheme != "https" {
            return PolicyDecision::Deny(format!("scheme '{scheme}' is not permitted"));
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') {
            return PolicyDecision::Deny("credentials in URLs are not permitted".into());
        }
        let host = authority
            .rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
            .map(|(h, _)| h)
            .unwrap_or(authority)
            .trim_matches(['[', ']'])
            .to_lowercase();
        if host.is_empty() {
            return PolicyDecision::Deny("URL has no host".into());
        }
        if let Ok(ip) = host.parse::<std::net::IpAddr>()
            && is_private_ip(&ip)
        {
            return PolicyDecision::Deny("requests to private or loopback addresses are blocked".into());
        }
        if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") {
            return PolicyDecision::Deny("requests to local hosts are blocked".into());
        }
        if self.cfg.network.allowed_domains.iter().any(|d| domain_matches(d, &host)) {
            PolicyDecision::Allow
        } else if self.cfg.network.unlisted_requires_approval {
            PolicyDecision::RequireApproval(format!("'{host}' is not on the network allowlist"))
        } else {
            PolicyDecision::Deny(format!("'{host}' is not on the network allowlist"))
        }
    }

    pub fn host_allowed_without_approval(&self, url: &str) -> bool {
        self.check_url(url) == PolicyDecision::Allow
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
        }
    }

    #[test]
    fn shipped_policy_file_parses_and_matches_the_defaults() {
        let shipped = PolicyConfig::from_toml(include_str!("../../../config/policy.toml")).expect("config/policy.toml must parse");
        assert_eq!(serde_json::to_value(&shipped).unwrap(), serde_json::to_value(PolicyConfig::default()).unwrap());
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
    fn taint_forces_approval_for_side_effects() {
        let (e, _d) = engine();
        let mut m = meta("memory_store");
        m.side_effects = true;
        assert_eq!(e.check(&m, &serde_json::json!({}), false), PolicyDecision::Allow);
        assert!(matches!(e.check(&m, &serde_json::json!({}), true), PolicyDecision::RequireApproval(_)));
    }
}
