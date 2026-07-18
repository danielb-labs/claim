//! The verb implementations. Each module is one `claim` subcommand's logic; the
//! CLI grammar that feeds them lives in [`crate::cli`].

pub mod add;
pub mod amend;
pub mod check;
pub mod docs;
pub mod drift;
pub mod graph;
pub mod init;
pub mod list;
pub mod log;
pub mod retire;
pub mod stats;
