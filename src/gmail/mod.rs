//! Gmail integration: OAuth'd access to the standard Gmail REST API, surfaced as
//! `gmail_*` tools the agent and artoo's sensors can call.
//!
//! This lives in the harness (not threepio) on purpose: threepio is a *bridge*
//! to remote OAuth MCP servers, whereas this is a local capability backed by the
//! Gmail REST API — the same role as `web_fetch`. Google's hosted Gmail *MCP*
//! server requires the Workspace Developer Preview (no consumer @gmail.com), so
//! the REST API is the path that actually works for a personal account.
//!
//! One-time setup: `yoda gmail-login` (opens a browser). After that the cached
//! refresh token lets the headless daemon renew access on its own.

mod auth;
mod client;
mod tools;

pub use auth::{is_configured, login};
pub use client::{Gmail, ThreadSummary};
pub use tools::gmail_tools as tools;
