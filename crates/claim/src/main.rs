//! The `claim` command-line tool.
//!
//! Command wiring is added one verb per build item (see CLAUDE.md). This entry
//! point stays a thin dispatcher: argument parsing lives with each command, the
//! work lives in `claim-core`.

fn main() -> anyhow::Result<()> {
    // Replaced by the argument parser and command dispatch in the CLI build
    // items. Kept minimal so the workspace compiles from the first commit.
    eprintln!("claim {}: no commands wired yet", env!("CARGO_PKG_VERSION"));
    std::process::exit(2);
}
