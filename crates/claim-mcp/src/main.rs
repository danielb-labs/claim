//! The `claim-mcp` server binary.
//!
//! Exposes one claim store to agents over the Model Context Protocol with three
//! tools — `query` (the recorded facts for the paths at hand, as dated evidence),
//! `report` (append a verdict an agent reached, with evidence, under its own git
//! identity), and `create` (record a new claim the agent just established, verified
//! now, for the caller to commit and review). It is a thin shell over [`claim_core`]
//! and [`claim_store`]: the protocol wiring lives in [`server`], the tool logic in
//! [`query`], [`report`], and [`create`], and this entry point only builds the
//! server and serves it over stdio — the transport an MCP client (an agent) connects
//! a subprocess over.
//!
//! The server discovers the store from its working directory per call, so it
//! always reads the store for the repository the agent launched it in.

mod create;
mod query;
mod report;
mod server;
#[cfg(test)]
mod testkit;

use anyhow::Context;
use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

use crate::server::ClaimServer;

/// Serve the three tools over stdio until the client disconnects.
///
/// Logs go to stderr only: stdout is the MCP transport and must carry nothing but
/// protocol frames, so a stray print there would corrupt the stream. `serve`
/// performs the initialize handshake and returns a running service; `waiting`
/// blocks until the peer closes the connection.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = ClaimServer::new()
        .serve(stdio())
        .await
        .context("failed to start the claim MCP server over stdio")?;
    service
        .waiting()
        .await
        .context("the claim MCP server stopped with an error")?;
    Ok(())
}
