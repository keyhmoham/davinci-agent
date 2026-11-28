//! MCP servers for a task: the ones already known that fit the request, and, when none does and the request is about an outside
//! service, the best ones the Glama directory has — connected on the spot, and their tools handed to this task.

use crate::engine::{Inner, TaskCtx};
use crate::prompts;
use dfae_core::*;
use dfae_mcp::{Candidate, Need};
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// "use the github mcp", "add an MCP for Notion": the request itself asks for one.
static MENTIONS_MCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bmcps?\b").unwrap());

/// Tools of one MCP server a task is shown at most.
const MAX_TOOLS_SHOWN: usize = 8;
/// Outside services one request is searched for at most.
const MAX_NEEDS: usize = 2;
/// Candidates of one need the decision model judges.
const SHORTLIST_JUDGED: usize = 6;

impl Inner {
    /// Attach the MCP servers this request needs to the task. `true` when its tools are now available.
    pub(crate) async fn stage_mcp(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str, routing: &Routing) -> bool {
        let Some(hub) = self.mcp.get().cloned() else { return false };
        // servers that are already known, and fit: shared words are a first cut; whether a server can really do what the request
        // needs is the decision model's question (seen: a Hacker News and a Wikipedia server "fitting" a GitHub request on common words)
        let candidates: Vec<String> = hub.relevant(goal).await.into_iter().take(5).collect();
        let mut chosen: Vec<(String, &'static str)> = self.servers_that_fit(ctx, goal, candidates).await.into_iter().take(3).map(|n| (n, "registry")).collect();
        let mut listings: std::collections::HashMap<String, String> = Default::default();
        // none does: look in the directory when the request is about an outside service
        if chosen.is_empty() && hub.discovery_enabled() && (routing.external || MENTIONS_MCP.is_match(goal)) {
            let needs = self.mcp_needs(ctx, goal).await;
            let searched: Vec<String> = needs.iter().flat_map(|n| n.queries.iter().map(|q| format!("\"{q}\""))).collect();
            self.emit(ctx, EventKind::Phase { phase: "discovering".into(), detail: format!("searching the MCP directory for {}", searched.join(", ")) }).await;
            // one shortlist per need, each read in detail (its tools), all judged in one decision call, then the best connected
            let mut lists: Vec<(usize, Candidate)> = Vec::new();
            let mut problems: Vec<String> = Vec::new();
            for (i, need) in needs.iter().enumerate() {
                match hub.shortlist(need).await {
                    Ok(list) => lists.extend(list.candidates.into_iter().take(SHORTLIST_JUDGED).map(|c| (i, c))),
                    Err(e) => problems.push(e.to_string()),
                }
            }
            let fit = self.candidates_that_fit(ctx, goal, &needs, lists).await;
            let per_need = if needs.len() > 1 { 1 } else { hub.max_auto() };
            for (i, _) in needs.iter().enumerate() {
                let picks: Vec<Candidate> = fit.iter().filter(|(n, _)| *n == i).map(|(_, c)| c.clone()).collect();
                if picks.is_empty() {
                    continue;
                }
                let mut connected = 0;
                for f in hub.connect(&picks, per_need).await {
                    connected += 1;
                    if chosen.iter().any(|(n, _)| *n == f.server) {
                        continue;
                    }
                    listings.insert(f.server.clone(), f.listing);
                    chosen.push((f.server, if f.known { "registry" } else { "glama" }));
                }
                // no server that can be used as it is fits this need, and one behind a sign-in does: the user is asked to sign in
                if connected == 0
                    && let Some(c) = picks.iter().find(|c| c.sign_in)
                    && let Some(f) = self.sign_in(ctx, &hub, c).await
                {
                    listings.insert(f.server.clone(), f.listing);
                    chosen.push((f.server, "glama"));
                }
            }
            if chosen.is_empty() {
                let why = if problems.is_empty() { "the servers the directory has do not fit it".to_string() } else { problems.join("; ") };
                self.emit(ctx, EventKind::Warning { message: format!("no MCP server could be added for this request: {why}") }).await;
            }
        }
        if chosen.is_empty() {
            return false;
        }
        for (name, source) in chosen {
            let Some(view) = hub.server(&name) else { continue };
            ctx.mcp.lock().insert(name.clone());
            // a server with many tools offers the planner the ones that fit the request (each tool listed costs tokens on every call)
            if view.tools.len() > MAX_TOOLS_SHOWN {
                let docs: Vec<String> =
                    view.tools.iter().map(|t| format!("{} {}", t.name.rsplit("__").next().unwrap_or(&t.name).replace('_', " "), t.description)).collect();
                let scores = dfae_core::text::bm25_scores(goal, &docs);
                let mut ranked: Vec<(usize, f32)> = scores.into_iter().enumerate().collect();
                ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
                let mut hidden = ctx.mcp_hidden.lock();
                for (i, _) in ranked.into_iter().skip(MAX_TOOLS_SHOWN) {
                    hidden.insert(view.tools[i].name.clone());
                }
            }
            let listing = listings.remove(&name).or(view.listing.clone());
            let source = if view.source == "config" { "config" } else { source };
            self.emit(
                ctx,
                EventKind::McpConnected { server: name, tools: view.tools.iter().map(|t| t.name.clone()).collect(), source: source.into(), listing },
            )
            .await;
        }
        true
    }

    /// Ask the user to sign in to a directory server, in the chat, and wait for the provider to send them back (the engine's
    /// callback finishes the sign-in and connects the server). `None`: the sign-in could not be started, failed, or was not done in time.
    async fn sign_in(&self, ctx: &Arc<TaskCtx>, hub: &Arc<dfae_mcp::McpHub>, c: &Candidate) -> Option<dfae_mcp::Found> {
        let wait = std::time::Duration::from_secs(self.cfg.mcp.sign_in_wait_secs);
        let s = match hub.begin_sign_in(c).await {
            Ok(s) => s,
            Err(e) => {
                self.emit(ctx, EventKind::Warning { message: format!("{} needs you to sign in, but signing in cannot be started: {e}", c.connector.name) })
                    .await;
                return None;
            }
        };
        self.emit(
            ctx,
            EventKind::McpSignInRequired {
                server: s.server.clone(),
                title: s.title.clone(),
                url: s.url.clone(),
                listing: s.listing.clone(),
                wait_secs: wait.as_secs(),
            },
        )
        .await;
        self.set_status(ctx, TaskStatus::WaitingForUser).await;
        let outcome = tokio::select! {
            r = hub.wait_sign_in(&s.state, wait) => r,
            _ = ctx.cancel.cancelled() => Err("cancelled".into()),
        };
        self.set_status(ctx, TaskStatus::Running).await;
        match outcome {
            Ok(f) => {
                self.emit(
                    ctx,
                    EventKind::McpSignedIn { server: f.server.clone(), ok: true, detail: format!("signed in to {}: {} tool(s)", s.title, f.tools.len()) },
                )
                .await;
                Some(f)
            }
            Err(e) => {
                self.emit(ctx, EventKind::McpSignedIn { server: s.server.clone(), ok: false, detail: e }).await;
                None
            }
        }
    }

    /// Of the known servers whose words match the request, the ones the decision model says can do what it needs (one yes/no per
    /// server, all in one call). Without a decision model the word match stands.
    async fn servers_that_fit(&self, ctx: &Arc<TaskCtx>, goal: &str, candidates: Vec<String>) -> Vec<String> {
        if candidates.is_empty() || self.decider.provider_name() == "local" {
            return candidates;
        }
        let hub = self.mcp.get().cloned();
        let mut questions = std::collections::BTreeMap::new();
        for (i, name) in candidates.iter().enumerate() {
            let tools = hub
                .as_ref()
                .and_then(|h| h.server(name))
                .map(|v| v.tools.iter().take(12).map(|t| t.name.rsplit("__").next().unwrap_or(&t.name).to_string()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            questions.insert(
                format!("server_{i}"),
                Question::noul(format!(
                    "Is the outside service \"{}\" (its tools: {tools}) the one this request is about, and can its tools do what the request needs? A service about something else does not fit, whatever words it shares with the request.",
                    name.replace('_', " ")
                )),
            );
        }
        let req = DecisionRequest { kind: DecisionKind::SelectTools, state: self.decision_state(goal, &[]), questions };
        let (resp, record, entry) = self.decider.decide(req).await;
        if let Some(e) = entry {
            self.add_ledger(ctx, e).await;
        }
        ctx.rec.lock().await.decisions.push(record);
        // a provider that could not answer (the local fallback has no answer for these): keep the word match
        if resp.provider == "local-fallback" {
            return candidates;
        }
        // a question the provider left unanswered keeps the word match
        candidates.into_iter().enumerate().filter(|(i, _)| resp.noul(&format!("server_{i}")).is_none_or(|p| p >= 0.5)).map(|(_, n)| n).collect()
    }

    /// Of the directory's candidates, the ones the decision model says can do what the request needs, judged on their tools as the
    /// directory lists them (one yes/no per candidate, all in one call). Without a decision model the ranking stands.
    async fn candidates_that_fit(&self, ctx: &Arc<TaskCtx>, goal: &str, needs: &[Need], cands: Vec<(usize, Candidate)>) -> Vec<(usize, Candidate)> {
        if cands.is_empty() || self.decider.provider_name() == "local" {
            return cands;
        }
        let mut questions = std::collections::BTreeMap::new();
        for (i, (n, c)) in cands.iter().enumerate() {
            let tools: Vec<String> = c
                .tools
                .iter()
                .take(10)
                .map(|t| {
                    let d: String = t.description.clone().unwrap_or_default().chars().take(90).collect();
                    if d.is_empty() { t.name.clone() } else { format!("{} ({d})", t.name) }
                })
                .collect();
            let tools = if tools.is_empty() { "not listed".to_string() } else { tools.join("; ") };
            let about: String = c.connector.description.clone().unwrap_or_default().chars().take(200).collect();
            questions.insert(
                format!("server_{i}"),
                Question::noul(format!(
                    "Can the outside service \"{}\" ({about}; its tools: {tools}) do this part of the request: \"{}\"? A service about something else does not fit, whatever words it shares with the request.",
                    c.connector.name,
                    needs.get(*n).map(|x| x.capability.as_str()).unwrap_or(goal)
                )),
            );
        }
        let req = DecisionRequest { kind: DecisionKind::SelectTools, state: self.decision_state(goal, &[]), questions };
        let (resp, record, entry) = self.decider.decide(req).await;
        if let Some(e) = entry {
            self.add_ledger(ctx, e).await;
        }
        ctx.rec.lock().await.decisions.push(record);
        if resp.provider == "local-fallback" {
            return cands;
        }
        cands.into_iter().enumerate().filter(|(i, _)| resp.noul(&format!("server_{i}")).is_none_or(|p| p >= 0.5)).map(|(_, c)| c).collect()
    }

    /// What the request needs from the directory: one small call writes, per outside service, the capability in a tool's own words
    /// and two or three searches (those words are all the directory ever hears of the request).
    async fn mcp_needs(&self, ctx: &Arc<TaskCtx>, goal: &str) -> Vec<Need> {
        let fallback = || {
            let q = dfae_core::text::content_tokens(goal).into_iter().filter(|t| t.len() > 2).take(5).collect::<Vec<_>>().join(" ");
            vec![Need { capability: goal.chars().take(200).collect(), queries: vec![q] }]
        };
        let Some(tier) = self.pick_tier(ExecMode::Assisted) else { return fallback() };
        let mut req = self.gen_request(tier, prompts::MCP_QUERY_SYSTEM, tokens::truncate_tokens(goal, 200), "mcp_query");
        req.max_tokens = 160;
        match self.generate_text(ctx, tier, req).await {
            Ok(text) => {
                let needs = parse_needs(&text, goal);
                if needs.is_empty() { fallback() } else { needs }
            }
            Err(_) => fallback(),
        }
    }
}

/// `{"needs": [{"capability": ..., "queries": [...]}]}`, or a bare query on one line (what a small model may answer instead).
fn parse_needs(text: &str, goal: &str) -> Vec<Need> {
    let clean = |q: &str| -> String { q.trim().trim_matches(['"', '`', '\'', '.']).chars().take(60).collect::<String>().trim().to_string() };
    let json = text.find('{').and_then(|a| text.rfind('}').map(|b| &text[a..=b])).and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok());
    if let Some(v) = json {
        let needs: Vec<Need> = v
            .get("needs")
            .and_then(|n| n.as_array())
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .take(MAX_NEEDS)
            .filter_map(|n| {
                let queries: Vec<String> = n
                    .get("queries")
                    .and_then(|q| q.as_array())
                    .map(|a| a.iter().filter_map(|q| q.as_str()).map(clean).filter(|q| !q.is_empty()).take(3).collect())
                    .unwrap_or_default();
                let capability: String = n.get("capability").and_then(|c| c.as_str()).unwrap_or("").trim().chars().take(200).collect();
                if queries.is_empty() && capability.is_empty() {
                    return None;
                }
                Some(Need { capability: if capability.is_empty() { queries[0].clone() } else { capability }, queries })
            })
            .collect();
        return needs;
    }
    let q = clean(text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
    if q.is_empty() { vec![] } else { vec![Need { capability: goal.chars().take(200).collect(), queries: vec![q] }] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_are_read_from_json_or_from_a_bare_query() {
        let n = parse_needs(
            r#"Sure: {"needs": [{"capability": "Read issues of a GitHub repository", "queries": ["github issues", "issue tracker"]},
                {"capability": "Post a message to a Slack channel", "queries": ["slack"]}, {"capability": "x", "queries": ["y"]}]}"#,
            "goal",
        );
        assert_eq!(n.len(), 2, "at most two needs");
        assert_eq!(n[0].queries, vec!["github issues", "issue tracker"]);
        assert_eq!(n[1].capability, "Post a message to a Slack channel");
        let bare = parse_needs("\"weather forecast\"\n", "what is the weather in Paris");
        assert_eq!(bare, vec![Need { capability: "what is the weather in Paris".into(), queries: vec!["weather forecast".into()] }]);
        assert!(parse_needs("  ", "g").is_empty());
    }
}
