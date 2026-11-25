//! Process execution helpers: no shell, cleared environment, hard timeout,
//! output cap. The container is the isolation boundary; this layer limits
//! what a single command can do inside it.

use dfae_core::*;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub truncated: bool,
}

pub async fn run_argv(argv: &[String], cwd: &Path, timeout: Duration, max_bytes: usize, cancel: &CancellationToken) -> Result<ExecResult, ToolError> {
    let (bin, args) = argv.split_first().ok_or_else(|| ToolError::args("empty command"))?;
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::new(ErrorKind::NotFound, format!("command not found: {bin}"))
        } else {
            ToolError::io(format!("spawn failed: {e}"))
        }
    })?;
    let mut so = child.stdout.take().expect("stdout piped");
    let mut se = child.stderr.take().expect("stderr piped");
    let cap = max_bytes as u64;
    let read_out = async {
        let mut a = Vec::new();
        {
            let mut t = (&mut so).take(cap);
            let _ = t.read_to_end(&mut a).await;
        }
        // drain the remainder so the child is not blocked on a full pipe
        let _ = tokio::io::copy(&mut so, &mut tokio::io::sink()).await;
        a
    };
    let read_err = async {
        let mut b = Vec::new();
        {
            let mut t = (&mut se).take(cap);
            let _ = t.read_to_end(&mut b).await;
        }
        let _ = tokio::io::copy(&mut se, &mut tokio::io::sink()).await;
        b
    };
    let read = async { tokio::join!(read_out, read_err) };
    let waited = async {
        let (a, b) = read.await;
        let status = child.wait().await;
        (a, b, status)
    };
    tokio::select! {
        _ = cancel.cancelled() => Err(ToolError::new(ErrorKind::Cancelled, "cancelled")),
        r = tokio::time::timeout(timeout, waited) => match r {
            Err(_) => Err(ToolError::new(ErrorKind::Timeout, format!("command timed out after {}s", timeout.as_secs()))),
            Ok((a, b, status)) => {
                let status = status.map_err(|e| ToolError::io(e.to_string()))?;
                Ok(ExecResult {
                    truncated: a.len() >= max_bytes || b.len() >= max_bytes,
                    stdout: String::from_utf8_lossy(&a).into_owned(),
                    stderr: String::from_utf8_lossy(&b).into_owned(),
                    code: status.code(),
                })
            }
        },
    }
}
