//! MCP servers for a task: the ones already known that fit the request, and, when none does and the request is about an outside
//! service, the best ones the Glama directory has — connected on the spot, and their tools handed to this task.

use crate::engine::{Inner, TaskCtx};
use crate::prompts;
use dfae_core::*;
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// "use the github mcp", "add an MCP for Notion": the request itself asks for one.
static MENTIONS_MCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bmcps?\b").unwrap());

/// Tools of one MCP server a task is shown at most.
const MAX_TOOLS_SHOWN: usize = 8;

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
            let query = self.mcp_query(ctx, goal).await;
            self.emit(ctx, EventKind::Phase { phase: "discovering".into(), detail: format!("searching the MCP directory for \"{query}\"") }).await;
            match hub.discover(&query).await {
                Ok(found) => {
                    for f in found {
                        listings.insert(f.server.clone(), f.listing);
                        chosen.push((f.server, if f.known { "registry" } else { "glama" }));
                    }
                }
                Err(e) => {
                    self.emit(ctx, EventKind::Warning { message: format!("no MCP server could be added for this request: {e}") }).await;
                }
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

    /// A few words that name the capability the request needs: one small call, and the words are all the directory ever hears of it.
    async fn mcp_query(&self, ctx: &Arc<TaskCtx>, goal: &str) -> String {
        let fallback = || dfae_core::text::content_tokens(goal).into_iter().filter(|t| t.len() > 2).take(5).collect::<Vec<_>>().join(" ");
        let Some(tier) = self.pick_tier(ExecMode::Assisted) else { return fallback() };
        let mut req = self.gen_request(tier, prompts::MCP_QUERY_SYSTEM, tokens::truncate_tokens(goal, 200), "mcp_query");
        req.max_tokens = 40;
        match self.generate_text(ctx, tier, req).await {
            Ok(text) => {
                let q: String = text.trim().trim_matches(['"', '`', '\'', '.']).lines().next().unwrap_or("").chars().take(60).collect();
                if q.trim().is_empty() { fallback() } else { q.trim().to_string() }
            }
            Err(_) => fallback(),
        }
    }
}
