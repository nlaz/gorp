//! Configuration loading: file, then environment overrides.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub listen_port: u16,
    pub worker_threads: usize,
    pub database_url: String,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_port: 8080,
            worker_threads: 4,
            database_url: "postgres://localhost/jobs".into(),
            log_level: "info".into(),
        }
    }
}

/// Parse a flat `key = value` file. Blank lines and `#` comments are skipped;
/// unknown keys are ignored rather than fatal, so a newer config file can be
/// read by an older binary.
pub fn parse_config_file(text: &str) -> Config {
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim(), value.trim());
        }
    }
    let mut cfg = Config::default();
    if let Some(v) = fields.get("listen_port").and_then(|v| v.parse().ok()) {
        cfg.listen_port = v;
    }
    if let Some(v) = fields.get("worker_threads").and_then(|v| v.parse().ok()) {
        cfg.worker_threads = v;
    }
    if let Some(v) = fields.get("database_url") {
        cfg.database_url = v.to_string();
    }
    if let Some(v) = fields.get("log_level") {
        cfg.log_level = v.to_string();
    }
    cfg
}

/// Environment wins over the file so a deploy can override without a rebuild.
pub fn apply_env_overrides(mut cfg: Config, env: &HashMap<String, String>) -> Config {
    if let Some(v) = env.get("JOBS_PORT").and_then(|v| v.parse().ok()) {
        cfg.listen_port = v;
    }
    if let Some(v) = env.get("JOBS_LOG_LEVEL") {
        cfg.log_level = v.clone();
    }
    cfg
}
