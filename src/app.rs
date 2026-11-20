// SPDX-License-Identifier: AGPL-3.0-only

use std::{net::SocketAddr, sync::Arc};

use axum::{Router, middleware};
use metrics_exporter_prometheus::PrometheusBuilder;
use sqlx::PgPool;
use tokio::{net::TcpListener, sync::Semaphore};
use webauthn_rs::Webauthn;

use crate::{
    auth::passkeys,
    config::Config,
    contacts::export::MAX_CONCURRENT_EXPORTS,
    crypto::secrets::Secrets,
    db,
    error::ApiError,
    http,
    http::csrf,
    http::rate_limit::{
        EXPORT_PER_MINUTE, IMPORT_PER_MINUTE, PREVIEW_PER_MINUTE, PUBLIC_PER_MINUTE, RateLimiter,
        SELECTION_PER_MINUTE,
    },
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
    /// Instance-wide cap on exports *streaming at once*, every workspace together. The limiter
    /// above is keyed by workspace and so cannot see an instance-wide resource: four workspaces
    /// each inside their own five-a-minute hold all twenty pooled connections between them, and
    /// `/readyz` waits on the same pool. See `contacts::export::MAX_CONCURRENT_EXPORTS`.
    pub export_slots: Arc<Semaphore>,
    /// Per-workspace cap on `POST /imports`, the heaviest write the product takes: up to 512 MB
    /// streamed to disk per call, kept there until the import is run or swept.
    pub import_limit: Arc<RateLimiter>,
    /// Per-**IP** cap on the public pages, which have no workspace to count against until their
    /// token is parsed. Its own limiter and so its own map: a flood of addresses fills this one
    /// and cannot reach the four above. See `http::rate_limit::MAX_KEYS`.
    pub public_limit: Arc<RateLimiter>,
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
            export_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_EXPORTS)),
            import_limit: Arc::new(RateLimiter::per_minute(IMPORT_PER_MINUTE)),
            public_limit: Arc::new(RateLimiter::per_minute(PUBLIC_PER_MINUTE)),
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
    // THE BOOT SWEEP. An import left `running` is failed with a reason, and then the uploads of
    // the imports that are over are removed. A row stuck in `running` for ever is worse than a
    // clear failure: nothing will finish it, `GET /imports` reports it as in progress, and the
    // workspace's guard in `start_import` holds against it.
    //
    // Awaited here rather than spawned: it is two statements and a directory walk, and a request
    // served before it has run would read the state it exists to clear. Best effort, like the
    // other sweeps — one that cannot run is not a reason to refuse to serve.
    match crate::contacts::import::routes::sweep_running_imports(&pool).await {
        Ok(0) => {}
        Ok(swept) => tracing::warn!(swept, "imports left running by a restart were failed"),
        Err(e) => tracing::warn!(error = ?e, "the running-import sweep failed"),
    }
    match crate::contacts::import::routes::sweep_orphaned_uploads(&pool).await {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "removed the uploads of imports that are over"),
        Err(e) => tracing::warn!(error = ?e, "the orphaned-upload sweep failed"),
    }
    // The public pages are limited per client IP, and `client_ip` falls back to the peer address
    // unless this is on. Behind a reverse proxy — which is how NM is meant to run — the peer is
    // the proxy, so every subscriber in the world shares one window of `PUBLIC_PER_MINUTE`. This
    // is a warning rather than a default because trusting `X-Forwarded-For` without a proxy in
    // front is worse: the header is client-controlled, and an attacker would then pick their own
    // rate-limit key.
    //
    // ONLY ON AN INSTANCE SOMEONE CAN REACH, and that is the whole point of the condition. A
    // warning a developer sees on every run is one nobody reads by the end of the week, and then
    // it is worse than no warning: it looks like a safety net while the misconfiguration it exists
    // to catch goes past someone trained to scroll over that line. `log_format` cannot gate it —
    // it defaults to `Json`, so a dev run is already the production format — and `public_url` asks
    // the same question the warning does, because a reverse proxy is how an instance gets a
    // hostname that is not localhost.
    //
    // THE GAP THIS TRADE ACCEPTS, so it is not rediscovered as a surprise: `public_url` defaults
    // to `http://localhost:8080`, so an operator who deploys behind a proxy and never sets it
    // gets no warning — on exactly the misconfiguration this exists for. That is deliberate and
    // not worth a second heuristic, because it is self-limiting: the same unset `public_url` puts
    // `localhost:8080` in every confirmation and unsubscribe link they send, which is a louder
    // failure than a shared rate-limit window and is found first. The warning is aimed at the
    // operator who configured their instance correctly and missed this one flag.
    if !cfg.trust_proxy_headers && !is_local_url(&cfg.public_url) {
        tracing::warn!(
            "trust_proxy_headers is off, so rate limiting keys on the peer address. If NM is \
             behind a reverse proxy, every public page request (/c/, /u/, /f/) counts against one \
             shared limit and subscribers will be refused. Set it only if the proxy is yours."
        );
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

/// Whether `public_url` names this machine — a developer's instance rather than one subscribers
/// reach. `.localhost` is included because RFC 6761 reserves the whole suffix for loopback, which
/// is how a local reverse-proxy setup is usually named. A URL with no host at all reads as local:
/// the consequence of being wrong here is only whether a warning is printed, and a warning nobody
/// can act on is the thing being avoided.
fn is_local_url(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => true,
    }
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

#[cfg(test)]
mod tests {
    use super::is_local_url;

    fn local(url: &str) -> bool {
        is_local_url(&url.parse().expect("a valid url"))
    }

    /// The proxy warning in `serve` is printed only when this is false, so a mistake here either
    /// nags every dev run — which is how a warning stops being read — or stays silent on the one
    /// deployment it exists for.
    #[test]
    fn only_an_instance_someone_else_can_reach_is_not_local() {
        // The config default, what `.env.example` leads to, and the loopback spellings.
        for url in [
            "http://localhost:8080",
            "http://localhost",
            "https://nm.localhost",
            "http://127.0.0.1:8080",
            "http://127.1.2.3",
            "http://[::1]:8080",
        ] {
            assert!(local(url), "{url}");
        }
        for url in [
            "https://mail.example.com",
            "https://example.com:8443",
            "http://203.0.113.10",
            "http://[2001:db8::1]",
            // A private address is still an instance other people reach: an office install
            // behind a proxy is exactly the deployment the warning is for.
            "http://10.0.0.5:8080",
            // Not loopback, however much it reads like it.
            "https://localhost.example.com",
        ] {
            assert!(!local(url), "{url}");
        }
    }
}
