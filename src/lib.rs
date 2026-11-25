//! machinestream-mcp: an MCP server whose only job is remote checks.
//!
//! MachineStream runs an AI coding agent's tests, type checks, linters and builds remotely,
//! on the exact state of the checkout, and returns the same exit code and output a local
//! run gives. The client side of that (the snapshot of the checkout, the stream a run is
//! answered on, the catalog of tools, sign-in) lives in the engine crates shared with the
//! ContextStream MCP server. This crate is the small MCP surface and the CLI around them.

pub mod billing;
pub mod catalog;
pub mod check;
pub mod cli;
pub mod config;
pub mod credentials;
pub mod engine;
pub mod format;
pub mod http;
pub mod install;
pub mod peer;
pub mod runlog;
pub mod server;
pub mod setup;
pub mod setup_cli;
pub mod status;
