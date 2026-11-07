// SPDX-License-Identifier: AGPL-3.0-only

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nm", version, about = "NM email marketing platform")]
pub struct Cli {
    /// Defaults to `serve` when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the HTTP server (default)
    Serve,
    /// Apply pending database migrations and exit
    Migrate,
    /// Print the OpenAPI spec as JSON and exit
    Openapi,
    /// Manage the secret key
    #[command(subcommand)]
    Keys(KeysCommand),
}

#[derive(Subcommand)]
pub enum KeysCommand {
    /// Print a new random NM_SECRET_KEY value
    Generate,
}
