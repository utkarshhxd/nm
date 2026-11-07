// SPDX-License-Identifier: AGPL-3.0-only

use clap::Parser;
use nm::{
    audit::{self, Actor, AuditEvent, actions},
    auth::sessions,
    cli::{AdminCommand, Cli, Command, KeysCommand},
    config::Config,
    db,
    error::ApiError,
    http::request_meta::RequestMeta,
    telemetry, users,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Keys(KeysCommand::Generate) => {
            println!("{}", nm::config::generate_secret_key());
        }
        Command::Openapi => {
            println!("{}", nm::openapi::spec().to_pretty_json()?);
        }
        Command::Migrate => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            let pool = db::connect(&cfg).await?;
            db::MIGRATOR.run(&pool).await?;
            println!("migrations up to date");
        }
        Command::Admin(cmd) => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            let pool = db::connect(&cfg).await?;
            admin(cmd, &pool).await?;
        }
        Command::Serve => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            nm::app::serve(cfg).await?;
        }
    }
    Ok(())
}

/// The first line of stdin without its line ending; spaces are part of a password.
fn read_password_from_stdin() -> anyhow::Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let password = line.trim_end_matches(['\n', '\r']);
    anyhow::ensure!(
        !password.is_empty(),
        "expected the password on the first line of stdin"
    );
    Ok(password.to_owned())
}

/// `ApiError` has no `Display`; its `Debug` carries a validation message or a problem code,
/// never the password.
fn api_error(e: ApiError) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}

async fn admin(cmd: AdminCommand, pool: &sqlx::PgPool) -> anyhow::Result<()> {
    match cmd {
        AdminCommand::CreateUser {
            email,
            instance_admin,
        } => {
            let password = read_password_from_stdin()?;
            let id = users::create_user(pool, &email, &password, instance_admin)
                .await
                .map_err(api_error)?;
            println!("{id}");
        }
        AdminCommand::ResetPassword { email } => {
            let password = read_password_from_stdin()?;
            let email = users::normalize_email(&email).map_err(|e| anyhow::anyhow!(e.message))?;
            let user = users::find_by_email(pool, &email)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no user with that email"))?;
            let mut tx = pool.begin().await?;
            users::set_password(&mut *tx, user.id, &password)
                .await
                .map_err(api_error)?;
            sessions::revoke_all_for_user(&mut *tx, user.id, None).await?;
            audit::record(
                &mut *tx,
                &RequestMeta::default(),
                AuditEvent {
                    workspace_id: None,
                    actor: Actor::System,
                    action: actions::AUTH_PASSWORD_CHANGED,
                    target_type: Some("user"),
                    target_id: Some(user.id.to_string()),
                    metadata: serde_json::json!({ "via": "cli" }),
                },
            )
            .await?;
            tx.commit().await?;
            println!("password reset; all sessions revoked");
        }
    }
    Ok(())
}
