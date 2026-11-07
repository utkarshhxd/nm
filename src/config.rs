// SPDX-License-Identifier: AGPL-3.0-only

use std::fmt;
use std::net::SocketAddr;

use base64::{Engine, engine::general_purpose::STANDARD};
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use rand::{TryRng, rngs::SysRng};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

/// 32-byte master key. `Debug` never prints the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(pub [u8; 32]);

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(***)")
    }
}

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    pub public_url: url::Url,
    pub secret_key: SecretKey,
    pub auto_migrate: bool,
    pub trust_proxy_headers: bool,
    pub metrics_listen: Option<SocketAddr>,
    pub log_format: LogFormat,
}

/// Hand-written so the database URL (it carries the password) never reaches logs.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No `..`: a new field must be named here, forcing a redaction decision.
        let Config {
            database_url: _,
            listen,
            public_url,
            secret_key,
            auto_migrate,
            trust_proxy_headers,
            metrics_listen,
            log_format,
        } = self;
        f.debug_struct("Config")
            .field("database_url", &"***")
            .field("listen", listen)
            .field("public_url", public_url)
            .field("secret_key", secret_key)
            .field("auto_migrate", auto_migrate)
            .field("trust_proxy_headers", trust_proxy_headers)
            .field("metrics_listen", metrics_listen)
            .field("log_format", log_format)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("NM_DATABASE_URL is required (for example postgres://user:pass@host:5432/nm)")]
    MissingDatabaseUrl,
    #[error(
        "no secret key: set NM_SECRET_KEY (base64, 32 bytes) or NM_SECRET_KEY_FILE; \
         generate one with `nm keys generate`"
    )]
    MissingSecretKey,
    #[error("set only one of NM_SECRET_KEY and NM_SECRET_KEY_FILE, not both")]
    SecretKeyConflict,
    #[error("cannot read NM_SECRET_KEY_FILE {path}: {source}")]
    SecretKeyFile {
        path: String,
        source: std::io::Error,
    },
    #[error("secret key is not valid base64; generate one with `nm keys generate`")]
    SecretKeyNotBase64,
    #[error("secret key must decode to exactly 32 bytes, got {0}")]
    SecretKeyLength(usize),
    #[error("invalid configuration: {0}")]
    Invalid(Box<figment::Error>),
}

/// Shape of the merged env/TOML values before validation.
#[derive(Deserialize)]
struct Raw {
    database_url: Option<String>,
    #[serde(default = "default_listen")]
    listen: SocketAddr,
    #[serde(default = "default_public_url")]
    public_url: url::Url,
    secret_key: Option<String>,
    secret_key_file: Option<String>,
    #[serde(default = "default_true")]
    auto_migrate: bool,
    #[serde(default)]
    trust_proxy_headers: bool,
    metrics_listen: Option<SocketAddr>,
    #[serde(default = "default_log_format")]
    log_format: LogFormat,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_public_url() -> url::Url {
    "http://localhost:8080".parse().expect("valid literal")
}

fn default_true() -> bool {
    true
}

fn default_log_format() -> LogFormat {
    LogFormat::Json
}

/// TOML layer for an explicitly requested config file; a missing or unreadable
/// file is an error, not a silent fallback to defaults.
fn figment_for(config_file: Option<&str>) -> Figment {
    match config_file {
        Some(path) => Figment::new().merge(Toml::file_exact(path)),
        None => Figment::new(),
    }
}

impl Config {
    /// Env `NM_*` over an optional TOML file named by `NM_CONFIG_FILE`.
    pub fn load() -> Result<Config, ConfigError> {
        let path = std::env::var("NM_CONFIG_FILE").ok();
        Config::from_figment(figment_for(path.as_deref()).merge(Env::prefixed("NM_")))
    }

    pub fn from_figment(f: Figment) -> Result<Config, ConfigError> {
        let raw: Raw = f.extract().map_err(|e| ConfigError::Invalid(Box::new(e)))?;
        let secret_key = match (raw.secret_key, raw.secret_key_file) {
            (Some(_), Some(_)) => return Err(ConfigError::SecretKeyConflict),
            (Some(k), None) => parse_secret_key(&k)?,
            (None, Some(path)) => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|source| ConfigError::SecretKeyFile { path, source })?;
                parse_secret_key(&text)?
            }
            (None, None) => return Err(ConfigError::MissingSecretKey),
        };
        Ok(Config {
            database_url: raw.database_url.ok_or(ConfigError::MissingDatabaseUrl)?,
            listen: raw.listen,
            public_url: raw.public_url,
            secret_key,
            auto_migrate: raw.auto_migrate,
            trust_proxy_headers: raw.trust_proxy_headers,
            metrics_listen: raw.metrics_listen,
            log_format: raw.log_format,
        })
    }
}

/// Decodes a standard-base64 32-byte key, ignoring surrounding whitespace.
pub fn parse_secret_key(s: &str) -> Result<SecretKey, ConfigError> {
    let bytes = STANDARD
        .decode(s.trim())
        .map_err(|_| ConfigError::SecretKeyNotBase64)?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|b: Vec<u8>| ConfigError::SecretKeyLength(b.len()))?;
    Ok(SecretKey(key))
}

/// Standard base64 of 32 bytes from the OS RNG.
pub fn generate_secret_key() -> String {
    let mut key = [0u8; 32];
    SysRng.try_fill_bytes(&mut key).expect("OS RNG unavailable");
    STANDARD.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use figment::{Figment, providers::Serialized};

    const KEY_B64: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

    /// Maps `NM_FOO_BAR` to key `foo_bar` without touching the process environment.
    fn figment_with(pairs: &[(&str, &str)]) -> Figment {
        // Parse values the way figment's `Env` provider does (bool/number/string).
        let map: figment::value::Dict = pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.strip_prefix("NM_").unwrap().to_lowercase(),
                    v.parse().unwrap(),
                )
            })
            .collect();
        Figment::from(Serialized::defaults(map))
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("NM_DATABASE_URL", "postgres://x"),
            ("NM_SECRET_KEY", KEY_B64),
        ]
    }

    #[test]
    fn defaults_apply_when_only_required_values_set() {
        let c = Config::from_figment(figment_with(&base())).unwrap();
        assert_eq!(c.listen, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.public_url.as_str(), "http://localhost:8080/");
        assert!(c.auto_migrate && !c.trust_proxy_headers && c.metrics_listen.is_none());
        assert_eq!(c.log_format, LogFormat::Json);
        assert_eq!(c.secret_key, SecretKey([1; 32]));
    }

    #[test]
    fn overrides_are_parsed() {
        let mut p = base();
        p.extend([
            ("NM_AUTO_MIGRATE", "false"),
            ("NM_TRUST_PROXY_HEADERS", "true"),
            ("NM_METRICS_LISTEN", "127.0.0.1:9090"),
            ("NM_LOG_FORMAT", "pretty"),
        ]);
        let c = Config::from_figment(figment_with(&p)).unwrap();
        assert!(!c.auto_migrate && c.trust_proxy_headers);
        assert_eq!(c.metrics_listen, Some("127.0.0.1:9090".parse().unwrap()));
        assert_eq!(c.log_format, LogFormat::Pretty);
    }

    #[test]
    fn missing_database_url_is_an_error() {
        let e = Config::from_figment(figment_with(&[("NM_SECRET_KEY", KEY_B64)])).unwrap_err();
        assert!(e.to_string().contains("NM_DATABASE_URL"));
    }

    #[test]
    fn missing_secret_key_error_is_actionable() {
        let e = Config::from_figment(figment_with(&[("NM_DATABASE_URL", "postgres://x")]))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("NM_SECRET_KEY")
                && e.contains("NM_SECRET_KEY_FILE")
                && e.contains("nm keys generate")
        );
    }

    #[test]
    fn secret_key_must_be_32_bytes() {
        let short = STANDARD.encode([0u8; 16]);
        let e = Config::from_figment(figment_with(&[
            ("NM_DATABASE_URL", "postgres://x"),
            ("NM_SECRET_KEY", &short),
        ]))
        .unwrap_err()
        .to_string();
        assert!(e.contains("32 bytes"), "{e}");
    }

    #[test]
    fn secret_key_must_be_base64() {
        let e = Config::from_figment(figment_with(&[
            ("NM_DATABASE_URL", "postgres://x"),
            ("NM_SECRET_KEY", "not base64!"),
        ]))
        .unwrap_err();
        assert!(matches!(e, ConfigError::SecretKeyNotBase64));
    }

    #[test]
    fn secret_key_file_is_read_and_trimmed() {
        let path = std::env::temp_dir().join(format!("nm-key-{}", std::process::id()));
        std::fs::write(&path, format!("  {KEY_B64}\n")).unwrap();
        let c = Config::from_figment(figment_with(&[
            ("NM_DATABASE_URL", "postgres://x"),
            ("NM_SECRET_KEY_FILE", path.to_str().unwrap()),
        ]));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(c.unwrap().secret_key, SecretKey([1; 32]));
    }

    #[test]
    fn key_and_key_file_together_is_an_error() {
        let mut p = base();
        p.push(("NM_SECRET_KEY_FILE", "/nonexistent"));
        let e = Config::from_figment(figment_with(&p)).unwrap_err();
        assert!(matches!(e, ConfigError::SecretKeyConflict));
    }

    #[test]
    fn missing_config_file_is_an_error() {
        let e = Config::from_figment(figment_for(Some("/nonexistent/nm.toml"))).unwrap_err();
        assert!(matches!(e, ConfigError::Invalid(_)), "{e}");
    }

    #[test]
    fn config_file_values_are_loaded() {
        let path = std::env::temp_dir().join(format!("nm-cfg-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            format!("database_url = \"postgres://x\"\nsecret_key = \"{KEY_B64}\"\n"),
        )
        .unwrap();
        let c = Config::from_figment(figment_for(path.to_str()));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(c.unwrap().database_url, "postgres://x");
    }

    #[test]
    fn config_debug_redacts_database_url() {
        let c = Config::from_figment(figment_with(&[
            ("NM_DATABASE_URL", "postgres://u:hunter2@h/db"),
            ("NM_SECRET_KEY", KEY_B64),
        ]))
        .unwrap();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("Config"), "{dbg}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
    }

    #[test]
    fn secret_key_debug_is_redacted() {
        assert_eq!(format!("{:?}", SecretKey([1; 32])), "SecretKey(***)");
    }

    #[test]
    fn generated_key_is_44_chars_and_valid() {
        let k = generate_secret_key();
        assert_eq!(k.len(), 44);
        assert!(parse_secret_key(&k).is_ok());
        assert_ne!(k, generate_secret_key());
    }
}
