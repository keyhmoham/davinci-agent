//! The toolbox: where the agent installs what a task needs, and the environment every sandbox shell command runs in.
//!
//! The container is read-only and the agent is an unprivileged user, so "install" means *user-space install*: pip, npm, cargo, go,
//! downloaded binaries and — through `dfae-apt` — Debian packages are put in one directory of the workspace and found through
//! `PATH`, `LD_LIBRARY_PATH` and the package managers' own settings. Nothing outside it is touched, and it survives restarts.

use crate::policy::PolicyEngine;
use std::path::{Path, PathBuf};

/// `dfae-apt <package>…`: Debian packages without root (apt downloads, `dpkg -x` unpacks into the toolbox).
const DFAE_APT: &str = include_str!("assets/dfae-apt");

pub fn tools_dir(policy: &PolicyEngine) -> PathBuf {
    let raw = policy.config().shell.tools_dir.replace("{workspace}", &policy.workspace().to_string_lossy());
    PathBuf::from(raw)
}

/// Make the toolbox exist (directories, the `dfae-apt` helper). Cheap enough to do before every command.
pub fn ensure(dir: &Path) {
    for d in ["bin", "py/bin", "npm/bin", "cargo/bin", "go", "cache", "root"] {
        let _ = std::fs::create_dir_all(dir.join(d));
    }
    let helper = dir.join("bin/dfae-apt");
    if std::fs::read_to_string(&helper).ok().as_deref() != Some(DFAE_APT) && std::fs::write(&helper, DFAE_APT).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755));
        }
    }
}

/// The environment of a sandbox command: a plain one, plus the toolbox, plus what makes tools run without a human.
pub fn shell_env(policy: &PolicyEngine) -> Vec<(String, String)> {
    let t = tools_dir(policy);
    ensure(&t);
    let p = |rel: &str| t.join(rel).to_string_lossy().into_owned();
    let arch = format!("{}-linux-gnu", std::env::consts::ARCH);
    let path = [
        p("bin"),
        p("py/bin"),
        p("npm/bin"),
        p("cargo/bin"),
        p("root/usr/bin"),
        p("root/usr/sbin"),
        p("root/bin"),
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
    ]
    .join(":");
    let libs = [p(&format!("root/usr/lib/{arch}")), p(&format!("root/lib/{arch}")), p("root/usr/lib")].join(":");
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let mut env: Vec<(String, String)> = [
        ("PATH", path),
        ("LD_LIBRARY_PATH", libs),
        ("DFAE_TOOLS", t.to_string_lossy().into_owned()),
        ("HOME", "/tmp".into()),
        ("TMPDIR", "/tmp".into()),
        ("LANG", "C.UTF-8".into()),
        ("TERM", "dumb".into()),
        // Python: user installs go to the toolbox (pip falls back to them when site-packages is read-only); a venv still works
        ("PYTHONUSERBASE", p("py")),
        ("PIP_BREAK_SYSTEM_PACKAGES", "1".into()),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1".into()),
        ("PIP_NO_INPUT", "1".into()),
        ("PIP_CACHE_DIR", p("cache/pip")),
        ("PYTHONUNBUFFERED", "1".into()),
        // Node
        ("NPM_CONFIG_PREFIX", p("npm")),
        ("npm_config_cache", p("cache/npm")),
        ("npm_config_yes", "true".into()),
        ("npm_config_update_notifier", "false".into()),
        ("npm_config_fund", "false".into()),
        ("npm_config_audit", "false".into()),
        // Rust and Go
        ("CARGO_HOME", p("cargo")),
        ("RUSTUP_HOME", p("rustup")),
        ("GOPATH", p("go")),
        ("GOBIN", p("bin")),
        ("GOCACHE", p("cache/go")),
        ("XDG_CACHE_HOME", p("cache")),
        // nobody is there to answer a prompt
        ("DEBIAN_FRONTEND", "noninteractive".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("GIT_PAGER", "cat".into()),
        ("PAGER", "cat".into()),
        ("GIT_AUTHOR_NAME", "DFAE agent".into()),
        ("GIT_AUTHOR_EMAIL", "agent@dfae.invalid".into()),
        ("GIT_COMMITTER_NAME", "DFAE agent".into()),
        ("GIT_COMMITTER_EMAIL", "agent@dfae.invalid".into()),
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "init.defaultBranch".into()),
        ("GIT_CONFIG_VALUE_0", "main".into()),
        ("MAKEFLAGS", format!("-j{cpus}")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for k in &policy.config().shell.env_passthrough {
        if let Ok(v) = std::env::var(k) {
            env.push((k.clone(), v));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{PolicyConfig, PolicyEngine};

    #[test]
    fn the_toolbox_is_on_the_path_and_installs_land_in_it() {
        let ws = tempfile::tempdir().unwrap();
        let policy = PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf());
        let env: std::collections::HashMap<_, _> = shell_env(&policy).into_iter().collect();
        let tools = ws.path().join(".tools");
        assert!(tools.join("bin/dfae-apt").is_file(), "the apt helper is installed");
        assert!(env["PATH"].starts_with(&tools.join("bin").to_string_lossy().to_string()));
        assert_eq!(env["PYTHONUSERBASE"], tools.join("py").to_string_lossy());
        assert_eq!(env["PIP_BREAK_SYSTEM_PACKAGES"], "1");
        assert_eq!(env["DEBIAN_FRONTEND"], "noninteractive");
        assert!(!env.contains_key("PLANNER_API_KEY") && !env.contains_key("GLAMA_API_KEY"), "the engine's keys do not reach commands");
    }
}
