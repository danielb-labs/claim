//! The verb implementations. Each module is one `claim` subcommand's logic; the
//! CLI grammar that feeds them lives in [`crate::cli`].

pub mod add;
pub mod check;
pub mod drift;
pub mod init;
pub mod list;
pub mod log;
