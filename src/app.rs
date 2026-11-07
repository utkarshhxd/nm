// SPDX-License-Identifier: AGPL-3.0-only

use std::{net::SocketAddr, sync::Arc};

use axum::{Router, middleware};
use metrics_exporter_prometheus::PrometheusBuilder;
use sqlx::PgPool;
use tokio::net::TcpListener;
use webauthn_rs::Webauthn;

use crate::{
    auth::passkeys, config::Config, crypto::secrets::Secrets, db, error::ApiError, http,
    http::csrf, http::security_headers::security_headers, telemetry,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub secrets: Arc<Secrets>,
    /// The WebAuthn relying party, derived from `public_url`. `None` when that host is not a domain
    /// name (an IP address), where passkeys cannot work; use `webauthn()`.
    pub webauthn: Option<Arc<Webauthn>>,
}

impl AppState {
    pub fn new(pool: PgPool, config: Config) -> anyhow::Result<AppState> {
        let secrets = Arc::new(Secrets::new(&config.secret_key));
        let webauthn = passkeys::relying_party(&config.public_url)?.map(Arc::new);
        Ok(AppState {
            pool,
            config: Arc::new(config),
            secrets,
            webauthn,
        })
    }

    /// The relying party, or `409 passkeys-unavailable` on an instance whose `public_url` has no
    /// domain name.
    pub fn webauthn(&self) -> Result<&Webauthn, ApiError> {
        self.webauthn
            .as_deref()
            .ok_or(ApiError::Conflict("passkeys-unavailable"))
    }
}

pub fn router(state: AppState) -> Router {
    http::routes()
        // Innermost, so a 403 still gets security headers and is counted by the metrics.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf::require_csrf,
        ))
        .with_state(state)
        .layer(telemetry::metrics_layer())
        // Outermost, so every response gets the headers: 404s, errors, metrics-free paths.
        .layer(middleware::from_fn(security_headers))
}

/// Connect, migrate (unless disabled), then serve until SIGINT/SIGTERM.
pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let pool = db::connect(&cfg).await?;
    if cfg.auto_migrate {
        db::MIGRATOR.run(&pool).await?;
    }
    if let Some(addr) = cfg.metrics_listen {
        PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()?;
        tracing::info!(%addr, "metrics exporter listening");
    }
    let listener = TcpListener::bind(cfg.listen).await?;
    tracing::info!(addr = %cfg.listen, "listening");
    let state = AppState::new(pool.clone(), cfg)?;
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    pool.close().await;
    tracing::info!("shut down");
    Ok(())
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what container runtimes send).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler")
    };
    #[cfg(unix)]
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received");
}
