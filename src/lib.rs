//! Yoda as a library: the agentic-harness core, reusable by other components in
//! the Yoda family. The `yoda` binary (`main.rs`) is one consumer; the proactive
//! `artoo` layer is another — it embeds this crate to drive the same provider,
//! tools, and permission gate rather than reimplementing them.

pub mod agent;
pub mod config;
pub mod gmail;
pub mod kibitzer;
pub mod mcp;
pub mod permission;
pub mod provider;
pub mod session;
pub mod skill;
pub mod tools;
pub mod ui;
