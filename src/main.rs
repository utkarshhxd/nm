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
        // THE THREE MODES, and the split is the point of spec M2a section 5.1: `serve` does not
        // send. An operator who scales the API to four replicas must not thereby quadruple their
        // send rate, and an API host's connection pool must not be drawn down by sending.
        Command::Serve => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            nm::app::serve(cfg).await?;
        }
        Command::Worker => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            nm::worker::run(cfg, nm::app::shutdown_signal()).await?;
        }
        Command::All => {
            let cfg = Config::load()?;
            telemetry::init_logging(cfg.log_format);
            // MIGRATIONS FIRST, BEFORE EITHER HALF STARTS. `serve` runs them too, and finds
            // them already applied — but it runs them *concurrently with* the worker's first
            // tick, which would then claim against a schema that does not exist yet and log an
            // error on a clean first boot. An operator reasonably reads that as "this is
            // broken". It is the only mode where the two race, because it is the only mode that
            // starts both halves.
            if cfg.auto_migrate {
                let pool = db::connect(&cfg).await?;
                db::MIGRATOR.run(&pool).await?;
                pool.close().await;
            }
            // One process, two pools (`db::connect` and `db::connect_worker`), and one signal
            // each: both halves stop on the same Ctrl-C or SIGTERM. `try_join` so that either
            // half failing — the worker's boot check refusing an `http` public URL, say — takes
            // the process down rather than leaving half of it serving.
            let worker_cfg = cfg.clone();
            tokio::try_join!(
                nm::app::serve(cfg),
                nm::worker::run(worker_cfg, nm::app::shutdown_signal()),
            )?;
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
            let id = users::create_user(pool, &email, &password, None, instance_admin)
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
        AdminCommand::DisableMfa { email } => {
            users::disable_mfa(pool, &email).await.map_err(api_error)?;
            println!("two-factor sign-in disabled; all sessions revoked");
        }
    }
    Ok(())
}
