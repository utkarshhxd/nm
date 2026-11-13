// SPDX-License-Identifier: AGPL-3.0-only

use std::{net::SocketAddr, sync::Arc};

use axum::{Router, middleware};
use metrics_exporter_prometheus::PrometheusBuilder;
use sqlx::PgPool;
use tokio::net::TcpListener;
use webauthn_rs::Webauthn;

use crate::{
    auth::passkeys,
    config::Config,
    crypto::secrets::Secrets,
    db,
    error::ApiError,
    http,
    http::csrf,
    http::rate_limit::{EXPORT_PER_MINUTE, PREVIEW_PER_MINUTE, RateLimiter, SELECTION_PER_MINUTE},
    http::security_headers::security_headers,
    telemetry,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub secrets: Arc<Secrets>,
    /// The WebAuthn relying party, derived from `public_url`. `None` when that host is not a domain
    /// name (an IP address), where passkeys cannot work; use `webauthn()`.
    pub webauthn: Option<Arc<Webauthn>>,
    /// Per-workspace cap on `POST /segments/preview`, which a viewer may call on a debounce and
    /// which may hold a pooled connection for 1.5 seconds.
    pub preview_limit: Arc<RateLimiter>,
    /// Per-workspace cap on `POST /contacts/selection`, the most expensive endpoint in the
    /// product: it materialises the match, measured at 4,887 ms and 158 MB for a million rows.
    pub selection_limit: Arc<RateLimiter>,
    /// Per-workspace cap on `POST /contacts/export`, which holds a pooled connection for as long
    /// as the client takes to read the book.
    pub export_limit: Arc<RateLimiter>,
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
            preview_limit: Arc::new(RateLimiter::per_minute(PREVIEW_PER_MINUTE)),
            selection_limit: Arc::new(RateLimiter::per_minute(SELECTION_PER_MINUTE)),
            export_limit: Arc::new(RateLimiter::per_minute(EXPORT_PER_MINUTE)),
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
        //
        // This layer is the CSRF gate AND the place the `Cookie` header is stripped from
        // bearer-authenticated requests, and a layer only covers the routes BELOW it. Every route
        // module therefore merges inside `http::routes()`. A `.merge(...)` added here, after this
        // `.layer()`, would silently get neither — no error, no failing test, just unprotected
        // unsafe methods. Add routes in `http::routes()`, not here.
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
    // The selection reaper: its own bounded sweep, because a snapshot is up to 158 MB and its
    // cascade is measured at 850 ms. Riding a user's request, as the session sweep does, would put
    // that on the hot path of a stranger's request and would stop sweeping the moment everyone
    // went home. Started here rather than in `AppState::new`, which every test builds.
    tokio::spawn(crate::contacts::selection::sweep_expired_snapshots(
        pool.clone(),
    ));
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
