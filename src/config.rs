//! Production Configuration Management for Omni MCP Platform.
//! Loads settings from environment variables with sensible production defaults.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Debug)]
pub struct OmniConfig {
    pub env: String,
    pub host: String,
    pub port: u16,
    pub log_level: String,
    pub data_dir: String,
    pub cache_refresh_interval_sec: f64,
    pub default_rate_limit_rpm: u32,
    pub max_body_size_bytes: usize,
    pub enable_pii_redaction: bool,
    pub cors_allowed_origins: String,
    pub auth_secret: String,
    pub worker_threads: usize,
    pub circuit_breaker_threshold: u32,
    pub circuit_breaker_reset_sec: f64,
    pub semantic_cache_ttl_sec: f64,
    pub stdio_allowlist: Vec<String>,
    pub secrets_dir: PathBuf,
    pub upstream_call_timeout_ms: u64,
    pub allow_loopback_upstream: bool,
    pub demo_seed: bool,
}

impl Default for OmniConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl OmniConfig {
    pub fn from_env() -> Self {
        let default_data_dir = env::var("USERPROFILE")
            .or_else(|_| env::var("HOME"))
            .map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".omni");
                p.push("data");
                p.to_string_lossy().into_owned()
            })
            .unwrap_or_else(|_| "./.omni/data".to_string());

        let enable_pii = env::var("OMNI_ENABLE_PII_REDACTION")
            .unwrap_or_else(|_| "true".to_string())
            .to_lowercase();

        Self {
            env: env::var("OMNI_ENV").unwrap_or_else(|_| "production".to_string()),
            host: env::var("OMNI_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: env::var("OMNI_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            log_level: env::var("OMNI_LOG_LEVEL")
                .unwrap_or_else(|_| "INFO".to_string())
                .to_uppercase(),
            data_dir: env::var("OMNI_DATA_DIR").unwrap_or(default_data_dir),
            cache_refresh_interval_sec: env::var("OMNI_CACHE_REFRESH_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.0),
            default_rate_limit_rpm: env::var("OMNI_DEFAULT_RATE_LIMIT_RPM")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1200),
            max_body_size_bytes: env::var("OMNI_MAX_BODY_SIZE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10 * 1024 * 1024),
            enable_pii_redaction: matches!(enable_pii.as_str(), "true" | "1" | "yes"),
            cors_allowed_origins: env::var("OMNI_CORS_ORIGINS").unwrap_or_default(),
            auth_secret: env::var("OMNI_AUTH_SECRET").unwrap_or_default(),
            worker_threads: env::var("OMNI_WORKER_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|n| (n.get() * 2).max(8))
                        .unwrap_or(16)
                }),
            circuit_breaker_threshold: env::var("OMNI_CB_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            circuit_breaker_reset_sec: env::var("OMNI_CB_RESET_SEC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30.0),
            semantic_cache_ttl_sec: env::var("OMNI_SEMANTIC_CACHE_TTL_SEC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(15.0),
            stdio_allowlist: env::var("OMNI_MCP_STDIO_ALLOWLIST")
                .map(|v| {
                    v.split([';', ':', ','])
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            secrets_dir: env::var("OMNI_MCP_SECRETS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./secrets")),
            upstream_call_timeout_ms: env::var("OMNI_MCP_UPSTREAM_CALL_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120_000),
            allow_loopback_upstream: env::var("OMNI_MCP_ALLOW_LOOPBACK_UPSTREAM")
                .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or_else(|_| {
                    let env_name =
                        env::var("OMNI_ENV").unwrap_or_else(|_| "production".to_string());
                    env_name == "development" || cfg!(test)
                }),
            demo_seed: env::var("OMNI_MCP_DEMO_SEED")
                .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or_else(|_| {
                    if let Ok(exe) = env::current_exe() {
                        let exe_str = exe.to_string_lossy();
                        if exe_str.contains("deps") || exe_str.contains("test") {
                            return true;
                        }
                    }
                    cfg!(test)
                }),
        }
    }
}

/// Returns `true` when `host` (without port) is a loopback address or name.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h == "127.0.0.1" || h == "::1"
}

impl OmniConfig {
    /// Refuses insecure network exposure: binding to a non-loopback address
    /// requires `OMNI_AUTH_SECRET` to be configured.
    pub fn validate_bind(&self, host: &str) -> Result<(), String> {
        if !is_loopback_host(host) && self.auth_secret.is_empty() {
            return Err(format!(
                "Refusing to bind to non-loopback address '{}' without OMNI_AUTH_SECRET configured",
                host
            ));
        }
        Ok(())
    }
}

static GLOBAL_CONFIG: OnceLock<RwLock<OmniConfig>> = OnceLock::new();

pub fn get_config() -> OmniConfig {
    let lock = GLOBAL_CONFIG.get_or_init(|| {
        let cfg = OmniConfig::from_env();
        let _ = fs::create_dir_all(&cfg.data_dir);
        RwLock::new(cfg)
    });
    lock.read().unwrap().clone()
}

pub fn set_config(cfg: OmniConfig) {
    let _ = fs::create_dir_all(&cfg.data_dir);
    let lock = GLOBAL_CONFIG.get_or_init(|| RwLock::new(cfg.clone()));
    if let Ok(mut guard) = lock.write() {
        *guard = cfg;
    }
}
