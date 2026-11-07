// SPDX-License-Identifier: AGPL-3.0-only

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nm", version, about = "NM email marketing platform")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage the secret key
    #[command(subcommand)]
    Keys(KeysCommand),
}

#[derive(Subcommand)]
pub enum KeysCommand {
    /// Print a new random NM_SECRET_KEY value
    Generate,
}
