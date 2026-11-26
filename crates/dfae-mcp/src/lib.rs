//! MCP (Model Context Protocol) for the engine: servers as tools, and the Glama directory to find them in.
//!
//! * [`client`] speaks the protocol (Streamable HTTP and stdio).
//! * [`hub::McpHub`] knows the servers, registers their tools in the engine's tool registry and finds new servers on demand.
//! * [`glama`] searches the Glama MCP directory.

pub mod client;
pub mod glama;
pub mod hub;
mod tool;

pub use client::{McpClient, McpError, Spec};
pub use glama::{AuthType, CREDIT, Connector, Glama, GlamaError};
pub use hub::{DiscoverError, Found, HubConfig, McpHub, Server, ServerView, Source, ToolView};
pub use tool::{server_name, server_of, tool_name};
