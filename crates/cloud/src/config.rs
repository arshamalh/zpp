//! Twelve-factor config: every flag can also come from an env var (ConfigMap / Secret in k8s).

use std::net::SocketAddr;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(about = "peakpilot cloud EMS service")]
pub struct Config {
    #[arg(long, env = "HTTP_ADDR", default_value = "0.0.0.0:8080")]
    pub http_addr: SocketAddr,
    #[arg(long, env = "MQTT_HOST", default_value = "localhost")]
    pub mqtt_host: String,
    #[arg(long, env = "MQTT_PORT", default_value_t = 1883)]
    pub mqtt_port: u16,
    /// Postgres URL. Empty or unset = run without persistence (in-memory only).
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: Option<String>,
    /// Re-plan a site every N telemetry messages (N PTUs).
    #[arg(long, env = "REPLAN_EVERY_PTUS", default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..))]
    pub replan_every: u64,
    /// Planning horizon in PTUs (192 = 48 h). Only the first day is nominated.
    #[arg(long, env = "HORIZON_PTUS", default_value_t = 192)]
    pub horizon_ptus: usize,
    /// How long a schedule stays valid on the gateway, in PTUs after it starts.
    #[arg(long, env = "SCHEDULE_VALIDITY_PTUS", default_value_t = 8)]
    pub validity_ptus: i32,
    #[arg(long, env = "SAFETY_MARGIN_KW", default_value_t = 3.0)]
    pub safety_margin_kw: f64,
    /// First ack timeout; doubles on every retry.
    #[arg(long, env = "ACK_TIMEOUT_MS", default_value_t = 1500)]
    pub ack_timeout_ms: u64,
    #[arg(long, env = "MAX_ATTEMPTS", default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=10))]
    pub max_attempts: u32,
    /// A site is shown offline after this many seconds without telemetry.
    #[arg(long, env = "OFFLINE_AFTER_SECS", default_value_t = 5)]
    pub offline_after_secs: u64,
    #[arg(long, env = "LOG_JSON", default_value_t = false)]
    pub log_json: bool,
}

impl Config {
    pub fn database_url(&self) -> Option<&str> {
        self.database_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
    }
}
