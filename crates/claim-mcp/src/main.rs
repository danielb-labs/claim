//! The `claim-mcp` server.
//!
//! Exposes the claim store to agents over the Model Context Protocol with two
//! verbs — `query` (verified facts for the paths at hand) and `report` (append a
//! verdict from work an agent just did). Built in the MCP build item; this entry
//! point is a placeholder so the workspace compiles from the first commit.

fn main() -> anyhow::Result<()> {
    eprintln!(
        "claim-mcp {}: not implemented yet",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}
