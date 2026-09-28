//! The AI chat: one replicated ledger per machine, exchanged over SSH,
//! with managed turns, a background service, and MCP tools.
#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses these private modules"
)]

pub(crate) mod address;
pub(crate) mod args;
pub(crate) mod cli;
pub(crate) mod ops;
pub(crate) mod provider;
pub(crate) mod pulse;
pub(crate) mod runner;
pub(crate) mod serve;
pub(crate) mod setup;
pub(crate) mod store;
pub(crate) mod sync;
pub(crate) mod view;
