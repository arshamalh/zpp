//! Logs (tracing) and metrics (Prometheus).

use std::time::Duration;

use metrics::gauge;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::state::Shared;

pub fn init_tracing(json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tower_http=info,sqlx=warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init(); // one JSON object per line, ready for Loki/ELK
    } else {
        builder.init();
    }
}

pub fn init_metrics() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Suffix("_seconds".into()),
            &[
                0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
        )?
        .install_recorder()?;
    Ok(handle)
}

/// Periodic gauges that aren't tied to a single event.
pub async fn monitor(state: Shared) {
    let mut every = tokio::time::interval(Duration::from_secs(2));
    let offline_after = Duration::from_secs(state.cfg.offline_after_secs);
    loop {
        every.tick().await;
        let (total, online) = {
            let sites = state.sites_read();
            let online = sites
                .values()
                .filter(|s| s.last_seen.is_some_and(|t| t.elapsed() < offline_after))
                .count();
            (sites.len(), online)
        };
        gauge!("peakpilot_sites").set(total as f64);
        gauge!("peakpilot_sites_online").set(online as f64);
        gauge!("peakpilot_pending_acks").set(state.pending().len() as f64);
    }
}
