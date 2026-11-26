//! An MCP server's tool as one of the engine's tools: its JSON Schema becomes typed parameters, its result becomes text.

use crate::client::{McpError, RemoteTool};
use crate::hub::Link;
use async_trait::async_trait;
use dfae_core::*;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// How a parameter is written in the server's schema, when the engine's four scalar types cannot say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Scalar,
    /// An object, an array or anything else: the model writes it as JSON text, and it is parsed before the call.
    Json,
}

pub(crate) struct McpTool {
    meta: ToolMetadata,
    remote: String,
    shapes: HashMap<String, Shape>,
    link: Arc<Link>,
}

/// Lowercase letters, digits and single underscores: what every model and every prompt handles.
pub(crate) fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '_' };
        if c == '_' && (out.is_empty() || out.ends_with('_')) {
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches('_').to_string()
}

/// The name a server is known by: what it was called, made safe to put in a tool name.
pub fn server_name(name: &str) -> String {
    slug(name)
}

/// `mcp__<server>__<tool>`: the prefix says where the tool lives (and keeps it out of tasks that have not attached the server).
pub fn tool_name(server: &str, tool: &str) -> String {
    let t: String = slug(tool).chars().take(64usize.saturating_sub(server.len() + 7).max(8)).collect();
    format!("mcp__{server}__{t}")
}

/// The server a tool name belongs to, if it is an MCP tool.
pub fn server_of(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__")?.split("__").next().filter(|s| !s.is_empty())
}

impl McpTool {
    pub(crate) fn new(server: &str, remote: &RemoteTool, link: Arc<Link>, vetted: bool, taken: &mut Vec<String>) -> Self {
        let mut name = tool_name(server, &remote.name);
        let mut n = 2;
        while taken.contains(&name) {
            name = format!("{}_{n}", tool_name(server, &remote.name));
            n += 1;
        }
        taken.push(name.clone());
        let (params, shapes) = params_of(&remote.input_schema);
        let ann = remote.annotations.clone().unwrap_or_default();
        let read_only = ann.read_only == Some(true);
        let destructive = ann.destructive == Some(true);
        let desc = remote.description.clone().unwrap_or_default();
        let desc: String = desc.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect();
        let mut keywords: Vec<String> = vec![server.replace('_', " "), "mcp".into()];
        keywords.extend(remote.name.split(['_', '-', '.']).filter(|w| w.len() > 2).map(String::from));
        let meta = ToolMetadata {
            name,
            description: if desc.is_empty() { format!("{} (a tool of the MCP server {server})", remote.name) } else { desc },
            category: "mcp".into(),
            risk: if destructive {
                Risk::High
            } else if read_only {
                Risk::Low
            } else {
                Risk::Moderate
            },
            // a server nobody vetted (found in a directory) gets approval asked for anything that is not plainly read-only
            requires_approval: !read_only && (destructive || !vetted),
            side_effects: !read_only,
            reversible: read_only || ann.idempotent == Some(true),
            cost_class: "medium".into(),
            params,
            keywords,
            direct_patterns: vec![],
            cacheable: read_only,
            contract: vec![],
        };
        Self { meta, remote: remote.name.clone(), shapes, link }
    }

    /// The arguments as the server's schema wants them: JSON text parsed, numbers and flags typed.
    fn arguments(&self, input: &Value) -> Value {
        let mut out = Map::new();
        for (k, v) in input.as_object().into_iter().flatten() {
            if v.is_null() {
                continue;
            }
            let v = match (self.shapes.get(k), v) {
                (Some(Shape::Json), Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| v.clone()),
                _ => v.clone(),
            };
            out.insert(k.clone(), v);
        }
        Value::Object(out)
    }
}

/// Parameters of a tool from its input schema (`properties` and `required`).
fn params_of(schema: &Value) -> (Vec<ParamSpec>, HashMap<String, Shape>) {
    let required: Vec<&str> = schema.get("required").and_then(|r| r.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    let mut params = Vec::new();
    let mut shapes = HashMap::new();
    for (name, p) in schema.get("properties").and_then(|p| p.as_object()).into_iter().flatten() {
        let ty = match p.get("type").and_then(|t| t.as_str()) {
            Some("integer") => Some(ParamType::Integer),
            Some("number") => Some(ParamType::Number),
            Some("boolean") => Some(ParamType::Boolean),
            Some("string") => Some(ParamType::String),
            _ => None,
        };
        let shape = if ty.is_some() { Shape::Scalar } else { Shape::Json };
        let mut desc = p.get("description").and_then(|d| d.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(options) = p.get("enum").and_then(|e| e.as_array()) {
            let list: Vec<String> = options.iter().take(12).map(|o| o.as_str().map(String::from).unwrap_or_else(|| o.to_string())).collect();
            desc = format!("{desc} (one of: {})", list.join(", ")).trim().to_string();
        }
        if shape == Shape::Json {
            let kind = p.get("type").and_then(|t| t.as_str()).unwrap_or("value");
            desc = format!("{desc} (JSON {kind})").trim().to_string();
        }
        let desc: String = desc.chars().take(240).collect();
        let mut spec = ParamSpec::new(name, ty.unwrap_or(ParamType::String), required.contains(&name.as_str()), &desc);
        if let Some(d) = p.get("default").filter(|d| !d.is_null()) {
            spec = spec.default(d.clone());
        }
        params.push(spec);
        shapes.insert(name.clone(), shape);
    }
    (params, shapes)
}

#[async_trait]
impl Tool for McpTool {
    fn metadata(&self) -> ToolMetadata {
        self.meta.clone()
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args = self.arguments(&input);
        let call = self.link.call(&self.remote, args);
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::new(ErrorKind::Cancelled, "cancelled")),
            r = call => r,
        };
        match result {
            Ok(r) if r.is_error => Err(ToolError::new(ErrorKind::Other, if r.text.is_empty() { "the tool reported an error".into() } else { clip(&r.text) })),
            Ok(r) => Ok(ToolOutput::ok(if r.text.is_empty() { "(the tool returned no content)".into() } else { clip(&r.text) })),
            Err(McpError::Rpc { code: -32602, message }) => Err(ToolError::new(ErrorKind::InvalidArgs, message)),
            Err(McpError::Timeout(s)) => Err(ToolError::new(ErrorKind::Timeout, format!("the MCP server did not answer within {s}s"))),
            Err(e @ McpError::Transport(_)) => Err(ToolError::new(ErrorKind::Transient, e.to_string())),
            Err(e) => Err(ToolError::new(ErrorKind::Other, e.to_string())),
        }
    }
}

fn clip(s: &str) -> String {
    const MAX: usize = 60_000;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more bytes cut]", &s[..cut], s.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_say_where_a_tool_lives() {
        assert_eq!(tool_name("github", "Create-Issue"), "mcp__github__create_issue");
        assert_eq!(server_of("mcp__github__create_issue"), Some("github"));
        assert_eq!(server_of("fs_read"), None);
        assert!(tool_name("s", &"x".repeat(200)).len() <= 64);
        assert_eq!(slug("  --Foo.Bar  "), "foo_bar");
    }

    #[test]
    fn a_json_schema_becomes_typed_parameters() {
        let (params, shapes) = params_of(&json!({
            "type": "object",
            "properties": {
                "repo": {"type": "string", "description": "owner/name"},
                "limit": {"type": "integer"},
                "dry_run": {"type": "boolean"},
                "labels": {"type": "array", "items": {"type": "string"}},
                "sort": {"type": "string", "enum": ["new", "old"]},
                "filter": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            },
            "required": ["repo"]
        }));
        let by = |n: &str| params.iter().find(|p| p.name == n).unwrap().clone();
        assert!(by("repo").required && !by("limit").required);
        assert_eq!((by("limit").ty, by("dry_run").ty), (ParamType::Integer, ParamType::Boolean));
        assert!(by("labels").description.contains("JSON array") && shapes["labels"] == Shape::Json);
        assert!(by("sort").description.contains("one of: new, old"));
        assert_eq!(shapes["filter"], Shape::Json, "a union is written as JSON text");
    }
}
