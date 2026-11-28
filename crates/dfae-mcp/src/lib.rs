//! MCP (Model Context Protocol) for the engine: servers as tools, and the Glama directory to find them in.
//!
//! * [`glama`] searches the Glama MCP directory: discovery is the engine's own.
//! * [`broker`] opens remote servers and signs the user in to them: with [`mcpc`] (the engine runs mcpc) or [`remote`] (mcpc in a
//!   broker of its own), authentication, sessions and reconnecting are all mcpc's.
//! * [`client`] speaks the protocol (Streamable HTTP to a server or to its mcpc proxy, and stdio for the operator's local servers).
//! * [`hub::McpHub`] knows the servers, registers their tools in the engine's tool registry and finds new servers on demand.

pub mod broker;
pub mod client;
pub mod glama;
pub mod hub;
pub mod mcpc;
pub mod remote;
mod tool;

pub use broker::{Broker, BrokerError, DirectBroker, LoginStart};
pub use client::{McpClient, McpError, Spec};
pub use glama::{AuthType, CREDIT, Connector, Detail, DirTool, Glama, GlamaError, Page};
pub use hub::{Candidate, DiscoverError, Found, HubConfig, McpHub, Need, Server, ServerView, Shortlist, SignIn, Source, ToolView};
pub use mcpc::{McpcBroker, McpcConfig, find_mcpc};
pub use remote::RemoteBroker;
pub use tool::{server_name, server_of, tool_name};
