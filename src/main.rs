// SPDX-License-Identifier: AGPL-3.0-only

use clap::Parser;
use nm::cli::{Cli, Command, KeysCommand};

fn main() {
    match Cli::parse().command {
        Some(Command::Keys(KeysCommand::Generate)) => {
            println!("{}", nm::config::generate_secret_key());
        }
        None => println!("nm {}", env!("CARGO_PKG_VERSION")),
    }
}
