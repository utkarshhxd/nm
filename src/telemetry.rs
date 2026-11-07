// SPDX-License-Identifier: AGPL-3.0-only

use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

/// Filter comes from `NM_LOG` (same syntax as `RUST_LOG`), default `info`.
pub fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_env("NM_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.pretty().init(),
    }
}
