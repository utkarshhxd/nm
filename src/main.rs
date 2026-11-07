// SPDX-License-Identifier: AGPL-3.0-only

use clap::Parser;
use nm::{
    cli::{Cli, Command, KeysCommand},
    config::Config,
    db, telemetry,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Keys(KeysCommand::Generate) => {
            println!("{}", nm::config::generate_secret_key());
        }
        Command::Migrate => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            let pool = db::connect(&cfg).await?;
            db::MIGRATOR.run(&pool).await?;
            println!("migrations up to date");
        }
        Command::Serve => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            nm::app::serve(cfg).await?;
        }
    }
    Ok(())
}
