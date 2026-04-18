//! Library surface for `holdmybeer-cli` — exposes modules that integration
//! tests need to reach. The binary still uses `mod lifecycle;` via
//! `src/main.rs`; this file only exists so tests can `use
//! holdmybeer_cli::lifecycle::*`.

pub mod client;
pub mod init;
pub mod lifecycle;
pub mod team;
pub mod team_cli;
pub mod team_init;
