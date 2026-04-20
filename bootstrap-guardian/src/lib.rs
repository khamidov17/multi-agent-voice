//! Bootstrap guardian — write-guarding process for Claudir.
//!
//! Prevents Nova (via Claude Code `Edit`/`Write` through the harness MCP
//! `protected_write` shim) from modifying its own harness, wrapper, or
//! launch config. Works by being the only process that holds write
//! permissions on protected paths — other processes must ask via UDS and
//! authenticate with an HMAC key whose knowledge is restricted to the
//! harness.
//!
//! Core modules:
//! - [`proto`]: wire types (Req / Resp / ErrCode).
//! - [`config`]: guardian runtime configuration.
//! - [`auth`]: HMAC + SO_PEERCRED.
//! - [`nonce`]: monotonic nonce persistence.
//! - [`paths`]: canonicalize + whitelist decider.
//! - [`audit`]: append-only JSONL audit log.
//! - [`server`]: UDS listener + request handler.

pub mod audit;
pub mod auth;
pub mod config;
pub mod nonce;
pub mod paths;
pub mod proto;
pub mod server;

pub use config::GuardianConfig;
pub use proto::{ErrCode, Op, Req, Resp};
pub use server::Guardian;
