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
    /// Run the HTTP server (default). Does NOT send: run `nm worker` or `nm all` for that
    Serve,
    /// Run the send worker alone
    Worker,
    /// Run the HTTP server and the send worker in one process
    All,
    /// Apply pending database migrations and exit
    Migrate,
    /// Print the OpenAPI spec as JSON and exit
    Openapi,
    /// Manage the secret key
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Account administration (reads the password from the first line of stdin)
    #[command(subcommand)]
    Admin(AdminCommand),
}

#[derive(Subcommand)]
pub enum KeysCommand {
    /// Print a new random NM_SECRET_KEY value
    Generate,
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Create a user and print its id; the password is the first line of stdin
    CreateUser {
        #[arg(long)]
        email: String,
        #[arg(long)]
        instance_admin: bool,
    },
    /// Set a user's password from the first line of stdin and sign them out everywhere
    ResetPassword {
        #[arg(long)]
        email: String,
    },
    /// Turn off a user's TOTP, delete their recovery codes and sign them out everywhere
    DisableMfa {
        #[arg(long)]
        email: String,
    },
}
