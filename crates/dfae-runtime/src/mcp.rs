//! MCP servers for a task: the ones already known that fit the request, and, when none does and the request is about an outside
//! service, the best ones the Glama directory has — connected on the spot, and their tools handed to this task.

use crate::engine::{Inner, TaskCtx};
use crate::prompts;
use dfae_core::*;
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// "use the github mcp", "add an MCP for Notion": the request itself asks for one.
static MENTIONS_MCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bmcps?\b").unwrap());

impl Inner {
    /// Attach the MCP servers this request needs to the task. `true` when its tools are now available.
    pub(crate) async fn stage_mcp(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str, routing: &Routing) -> bool {
        let Some(hub) = self.mcp.get().cloned() else { return false };
        // servers that are already known, and fit
        let mut chosen: Vec<(String, &'static str)> = hub.relevant(goal).await.into_iter().take(3).map(|n| (n, "registry")).collect();
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
