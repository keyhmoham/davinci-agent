//! Verification engine: judge completion on *evidence*, not on what a model
//! says it did.

use crate::answer;
use crate::engine::{Inner, TaskCtx};
use dfae_core::*;
use regex::Regex;
use std::sync::Arc;

/// Ask something the agent started on this machine whether it answers, waiting for it to come up (a dev server builds on its first
/// request). `Ok(evidence)` when it answers 2xx and, if a text is wanted, the page has it; `Err(why)` says what was found instead — the
/// title and first heading, which is what a planner needs to see that a scaffold's default page is being served.
pub(crate) async fn probe_url(url: &str, value: Option<&str>, patience: std::time::Duration) -> Result<String, String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', ':', '?']).next().unwrap_or("");
    if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]") {
        return Err(format!("{url} is not on this machine: only what the agent started here is checked this way"));
    }
    let url = url.replace("//0.0.0.0", "//127.0.0.1");
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let deadline = tokio::time::Instant::now() + patience;
    let mut last: String;
    loop {
        match http.get(&url).send().await {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                let seen = describe_page(&body);
                if !status.is_success() {
                    last = format!("HTTP {status}{seen}");
                } else if let Some(v) = value.filter(|v| !visible_text(&body).to_lowercase().contains(&v.to_lowercase())) {
                    last = format!("HTTP {status}, but the page does not say '{v}'{seen}");
                } else {
                    return Ok(format!("HTTP {status}, {} bytes{seen}", body.len()));
                }
            }
            Err(e) => last = format!("no answer ({})", e.to_string().chars().take(80).collect::<String>()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(last);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// What a visitor can read on an HTML page: no head (a title is not a page), scripts or styles, no tags. A scaffold's default page with
/// the new project's name in its title must not pass for the page that was asked for.
fn visible_text(body: &str) -> String {
    static HIDDEN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?is)<head\b.*?</head>|<script\b.*?</script>|<style\b.*?</style>|<template\b.*?</template>|<!--.*?-->").unwrap()
    });
    static TAGS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
    let shown = HIDDEN.replace_all(body, " ");
    let text = TAGS.replace_all(&shown, " ");
    let text = text
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `; title "…", first heading "…"` of an HTML page.
fn describe_page(body: &str) -> String {
    static TITLE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    static H1: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?is)<h1[^>]*>(.*?)</h1>").unwrap());
    static TAGS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
    let text = |re: &Regex| re.captures(body).map(|c| TAGS.replace_all(&c[1], " ").split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty());
    let mut s = String::new();
    if let Some(t) = text(&TITLE) {
        s.push_str(&format!("; title \"{}\"", t.chars().take(80).collect::<String>()));
    }
    if let Some(h) = text(&H1) {
        s.push_str(&format!("; first heading \"{}\"", h.chars().take(80).collect::<String>()));
    }
    s
}

impl Inner {
    /// Evaluate a declarative check against a step output.
    pub(crate) fn eval_check(&self, c: &Check, output: &str) -> (bool, String) {
        match c {
            Check::NonEmpty => (!output.trim().is_empty(), format!("{} bytes of output", output.len())),
            Check::Contains { value } => (output.to_lowercase().contains(&value.to_lowercase()), format!("looked for '{value}'")),
            Check::NotContains { value } => (!output.to_lowercase().contains(&value.to_lowercase()), format!("looked for absence of '{value}'")),
            Check::Matches { pattern } => match Regex::new(pattern) {
                Ok(re) => (re.is_match(output), format!("/{pattern}/")),
                Err(e) => (false, format!("bad pattern: {e}")),
            },
            Check::ExitOk => (true, "tool reported success".into()),
            Check::FileExists { path } => match self.policy.guard().exists(path) {
                Ok(found) => (found, path.clone()),
                Err(e) => (false, e.message),
            },
            // asked after the step, not here (it takes a request): see `probe_url`
            Check::UrlServes { .. } => (true, "checked by asking it".into()),
            Check::FileContains { path, value } => match self.policy.guard().read_path(path) {
                Ok(p) => match std::fs::read_to_string(&p) {
                    Ok(text) => (text.contains(value.as_str()), format!("looked for '{value}' in {}", p.display())),
                    Err(e) => (false, format!("{}: {e}", p.display())),
                },
                Err(e) => (false, e.message),
            },
        }
    }

    pub(crate) async fn stage_verify(self: &Arc<Self>, ctx: &Arc<TaskCtx>) -> VerificationResult {
        let rec = ctx.rec.lock().await.clone();
        let mut checks: Vec<CheckResult> = Vec::new();
        let mut tool_ok = 0usize;
        let (mut ok, mut failed, mut skipped) = (0usize, 0usize, 0usize);
        if let Some(plan) = &rec.plan {
            for s in &plan.steps {
                let label = s.tool.as_deref().unwrap_or(if s.kind == StepKind::Agent { "agent" } else { "reasoning" });
                match s.status {
                    StepStatus::Succeeded => {
                        ok += 1;
                        // a delegated step counts as evidence only if the agent itself verified its work
                        let agent_verified = s.kind == StepKind::Agent && s.evidence.iter().any(|e| e.contains(":verified") || e.contains(":partial"));
                        if s.kind == StepKind::Tool || agent_verified {
                            tool_ok += 1;
                        }
                        checks.push(CheckResult {
                            check_id: format!("{}:{}", s.id, label),
                            description: match s.kind {
                                StepKind::Tool => "tool reported success".into(),
                                StepKind::Agent => format!(
                                    "delegated agent finished ({})",
                                    s.evidence.iter().find(|e| e.starts_with("child:")).map(|e| e.rsplit(':').next().unwrap_or("")).unwrap_or("unverified")
                                ),
                                StepKind::Reasoning => "reasoning step produced output".into(),
                            },
                            status: CheckStatus::Passed,
                            evidence: tokens::truncate_tokens(s.summary.as_deref().unwrap_or(""), 24),
                        });
                        let out = s.output.as_deref().unwrap_or("");
                        for c in s.checks.iter().filter(|c| **c != Check::ExitOk) {
                            let (pass, ev) = self.eval_check(c, out);
                            checks.push(CheckResult {
                                check_id: format!("{}:check", s.id),
                                description: c.describe(),
                                status: if pass { CheckStatus::Passed } else { CheckStatus::Failed },
                                evidence: ev,
                            });
                        }
                        for ev in &s.evidence {
                            checks.push(CheckResult {
                                check_id: format!("{}:post", s.id),
                                description: "independent post-condition".into(),
                                status: CheckStatus::Passed,
                                evidence: ev.clone(),
                            });
                        }
                    }
                    StepStatus::Failed => {
                        failed += 1;
                        checks.push(CheckResult {
                            check_id: format!("{}:{}", s.id, label),
                            description: "step completed".into(),
                            status: CheckStatus::Failed,
                            evidence: s.error.clone().unwrap_or_default(),
                        });
                    }
                    StepStatus::Skipped | StepStatus::Cancelled => {
                        skipped += 1;
                        checks.push(CheckResult {
                            check_id: format!("{}:{}", s.id, label),
                            description: "step ran".into(),
                            status: CheckStatus::Skipped,
                            evidence: s.error.clone().unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
            }
        }
        let total = rec.plan.as_ref().map(|p| p.steps.len()).unwrap_or(0);
        let checks_passed = checks.iter().filter(|c| c.status == CheckStatus::Passed).count();
        let checks_failed = checks.iter().filter(|c| c.status == CheckStatus::Failed).count();
        let failed_checks: Vec<String> = checks.iter().filter(|c| c.status == CheckStatus::Failed).map(|c| format!("{}: {}", c.check_id, c.evidence)).collect();

        // The decision model is consulted only where evidence alone is ambiguous
        // (reasoning steps, failures, skips); pure tool plans are judged by checks.
        let needs_judgement = rec.plan.as_ref().map(|p| p.has_reasoning()).unwrap_or(false) || failed > 0 || skipped > 0;
        let completion = if needs_judgement && total > 0 {
            let mut st = self.decision_state(&rec.goal, &rec.entities);
            st.completion_facts = Some(CompletionFacts {
                steps_ok: ok,
                steps_failed: failed,
                steps_total: total,
                checks_passed,
                checks_failed,
                evidence: answer::evidence_block(&rec, 300), // the decision model is self-hosted: show it the evidence
            });
            Some(self.note(ctx, self.decider.assess_completion(st).await).await)
        } else {
            None
        };

        let status = if checks_failed > 0 || failed > 0 {
            VerificationStatus::Failed
        } else if tool_ok == 0 {
            VerificationStatus::Unverified
        } else if skipped > 0 || completion.map(|p| p < 0.5).unwrap_or(false) {
            VerificationStatus::Partial
        } else {
            VerificationStatus::Verified
        };
        VerificationResult { status, checks, failed_checks, unverified_claims: vec![], completion_confidence: completion }
    }
}

#[cfg(test)]
mod probe_tests {
    use super::probe_url;
    use std::time::Duration;

    async fn serve(body: &'static str, status: u16) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let reply = format!("HTTP/1.1 {status} X\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_served_page_is_asked_for_what_it_says_and_a_wrong_one_says_what_it_shows_instead() {
        let page = "<html><head><title>Create Next App</title></head><body><h1>To get started, edit the page</h1></body></html>";
        let url = serve(page, 200).await;
        let ok = probe_url(&url, Some("to get started"), Duration::from_secs(3)).await.unwrap();
        assert!(ok.starts_with("HTTP 200 OK") || ok.starts_with("HTTP 200"), "{ok}");
        // a name that is only in the title is not on the page
        let titled =
            serve("<html><head><title>Acme Analytics</title></head><body><h1>To get started</h1><script>var x=\"Acme Analytics\"</script></body></html>", 200)
                .await;
        assert!(probe_url(&titled, Some("Acme Analytics"), Duration::from_secs(1)).await.unwrap_err().contains("does not say"));
        let err = probe_url(&url, Some("Acme Analytics"), Duration::from_secs(1)).await.unwrap_err();
        assert!(
            err.contains("does not say 'Acme Analytics'") && err.contains("title \"Create Next App\"") && err.contains("first heading \"To get started"),
            "{err}"
        );
        assert!(probe_url(&serve("gone", 500).await, None, Duration::from_secs(1)).await.unwrap_err().contains("HTTP 500"));
        assert!(probe_url("http://127.0.0.1:1", None, Duration::from_secs(1)).await.unwrap_err().contains("no answer"));
        assert!(probe_url("https://example.com", None, Duration::from_secs(1)).await.unwrap_err().contains("not on this machine"));
    }
}
