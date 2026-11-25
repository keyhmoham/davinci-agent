//! Tool metadata, the `Tool` trait and execution context.

use crate::{ErrorKind, Risk, ToolError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub ty: ParamType,
    pub required: bool,
    pub description: String,
    /// Entity labels (from the extractor) that can fill this parameter.
    #[serde(default)]
    pub entity_hints: Vec<String>,
    /// Value used by the deterministic binder when nothing else matches.
    #[serde(default)]
    pub default: Option<Value>,
}

impl ParamSpec {
    pub fn new(name: &str, ty: ParamType, required: bool, description: &str) -> Self {
        Self { name: name.into(), ty, required, description: description.into(), entity_hints: vec![], default: None }
    }
    pub fn hints(mut self, hints: &[&str]) -> Self {
        self.entity_hints = hints.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn default(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub category: String,
    pub risk: Risk,
    pub requires_approval: bool,
    pub side_effects: bool,
    pub reversible: bool,
    /// `free`, `low`, `medium`, `high`.
    pub cost_class: String,
    pub params: Vec<ParamSpec>,
    /// Extra lexical hints for tool selection.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Regexes with named groups that bind arguments straight from the user's
    /// text — the basis of the SLM-free Direct mode.
    #[serde(default)]
    pub direct_patterns: Vec<String>,
}

impl ToolMetadata {
    /// Token-lean signature shown to models: `fs_read(path:str*, max_bytes:int) read a file`
    pub fn signature(&self) -> String {
        let ps: Vec<String> = self
            .params
            .iter()
            .map(|p| {
                let t = match p.ty {
                    ParamType::String => "str",
                    ParamType::Integer => "int",
                    ParamType::Number => "num",
                    ParamType::Boolean => "bool",
                };
                format!("{}:{}{}", p.name, t, if p.required { "*" } else { "" })
            })
            .collect();
        format!("{}({}) {}", self.name, ps.join(", "), self.description)
    }

    /// Text used for lexical tool selection.
    pub fn search_text(&self) -> String {
        format!("{} {} {} {}", self.name.replace('_', " "), self.description, self.category, self.keywords.join(" "))
    }

    /// Validate JSON arguments against the parameter specs.
    pub fn validate_args(&self, args: &Value) -> Result<(), String> {
        let obj = match args {
            Value::Object(m) => m.clone(),
            Value::Null => Default::default(),
            _ => return Err("arguments must be a JSON object".into()),
        };
        for p in &self.params {
            match obj.get(&p.name) {
                None | Some(Value::Null) => {
                    if p.required {
                        return Err(format!("missing required argument '{}'", p.name));
                    }
                }
                Some(v) => {
                    let ok = match p.ty {
                        ParamType::String => v.is_string(),
                        ParamType::Integer => v.is_i64() || v.is_u64(),
                        ParamType::Number => v.is_number(),
                        ParamType::Boolean => v.is_boolean(),
                    };
                    if !ok {
                        return Err(format!("argument '{}' must be {:?}", p.name, p.ty));
                    }
                }
            }
        }
        for k in obj.keys() {
            if !self.params.iter().any(|p| &p.name == k) {
                return Err(format!("unknown argument '{k}'"));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub task_id: String,
    pub step_id: String,
    pub workspace: PathBuf,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub ok: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), ok: true, exit_code: Some(0), evidence: vec![] }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn metadata(&self) -> ToolMetadata;

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;

    /// Independent post-condition check (e.g. read the file back). Returns
    /// human-readable evidence strings, or an error describing the mismatch.
    async fn verify(&self, _input: &Value, _output: &ToolOutput, _ctx: &ToolContext) -> Option<Result<String, String>> {
        None
    }
}

/// Convenience for tools: pull a typed argument.
pub fn arg_str<'a>(v: &'a Value, k: &str) -> Result<&'a str, ToolError> {
    v.get(k)
        .and_then(|x| x.as_str())
        .ok_or_else(|| ToolError::new(ErrorKind::InvalidArgs, format!("missing string argument '{k}'")))
}
