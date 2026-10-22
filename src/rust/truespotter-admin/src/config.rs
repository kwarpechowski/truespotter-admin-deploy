//! Environment-driven configuration (same pattern as truespotter-core/-edge).
//!
//! Knobs: METRICS_SECRET + JWT_SECRET (admin auth), DATABASE_URL or
//! POSTGRES_URL (required — every endpoint reads Postgres),
//! OPENSEARCH_URL / OPENSEARCH_INDEX / OPENSEARCH_USER + OPENSEARCH_PASSWORD
//! (products list + stats product count), PORT (default 8089).

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub metrics_secret: Option<String>,
    pub jwt_secret: Option<String>,
    pub database_url: Option<String>,
    pub opensearch_url: String,
    pub opensearch_index: String,
    pub opensearch_user: Option<String>,
    pub opensearch_password: Option<String>,
    pub port: u16,
}

fn non_empty(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    non_empty(key).unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            metrics_secret: non_empty("METRICS_SECRET"),
            jwt_secret: non_empty("JWT_SECRET"),
            database_url: non_empty("DATABASE_URL").or_else(|| non_empty("POSTGRES_URL")),
            opensearch_url: env_or("OPENSEARCH_URL", "http://localhost:9200"),
            opensearch_index: env_or("OPENSEARCH_INDEX", "products"),
            opensearch_user: non_empty("OPENSEARCH_USER"),
            opensearch_password: non_empty("OPENSEARCH_PASSWORD"),
            port: env::var("PORT")
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok())
                .unwrap_or(8089),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_parses() {
        assert_eq!("8089".trim().parse::<u16>().unwrap(), 8089);
    }

    #[test]
    fn env_or_falls_back_on_empty() {
        // SAFETY: single-threaded test process; this key is unused elsewhere.
        env::set_var("_TS_ADMIN_TEST_KEY", "  ");
        assert_eq!(env_or("_TS_ADMIN_TEST_KEY", "fallback"), "fallback");
        env::remove_var("_TS_ADMIN_TEST_KEY");
    }
}
